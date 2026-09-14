//! Durable lifecycle intent shared by CLI and desktop callers.
use crate::services::ownership::{atomic_write, identity_digest, OperationLease};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationRecord {
    pub schema_version: u32,
    #[serde(default)]
    pub project_id: crate::state::ProjectId,
    #[serde(default)]
    pub attempt: u32,
    #[serde(default)]
    pub request: serde_json::Value,
    #[serde(default)]
    pub config_fingerprint: Option<String>,
    #[serde(default)]
    pub steps: Vec<OperationStep>,
    #[serde(default = "chrono::Utc::now")]
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub id: String,
    pub kind: String,
    pub project: String,
    pub workspace: String,
    pub code_parent: Option<String>,
    pub data_parent: Option<String>,
    pub worktree_path: Option<String>,
    pub status: String,
    pub phase: String,
    pub post_create_pending: bool,
    pub completed_phases: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Materialize,
    Service,
    Hook,
    Process,
    Seed,
    Reset,
    Cleanup,
    Verify,
    Other,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Running,
    Succeeded,
    Failed,
    NeedsAttention,
    Interrupted,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationStep {
    pub name: String,
    pub kind: StepKind,
    pub status: StepStatus,
    pub attempts: u32,
    pub detail: Option<String>,
}

#[derive(Debug)]
pub struct OperationError {
    pub operation: OperationRecord,
    pub message: String,
}
impl std::fmt::Display for OperationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} (operation {}, phase {}; retry the same command to resume)",
            self.message, self.operation.id, self.operation.phase
        )
    }
}
impl std::error::Error for OperationError {}

pub struct OperationJournal {
    pub record: OperationRecord,
    path: PathBuf,
    _lease: OperationLease,
    _project_lease: OperationLease,
    finished: bool,
}

/// Coordinate resource mutations across frontends, including legacy service commands.
pub async fn project_policy_lease(project: &Path, exclusive: bool) -> Result<OperationLease> {
    let id = crate::state::LocalStateManager::new()?.control(project).id;
    let path = crate::paths::devflow_config_dir()?
        .join("operations")
        .join(format!("{}.project.lock", id.0));
    if exclusive {
        OperationLease::acquire(&path).await
    } else {
        OperationLease::acquire_shared(&path).await
    }
}

impl OperationJournal {
    pub async fn begin(
        project: &Path,
        workspace: &str,
        kind: &str,
        code_parent: Option<String>,
        data_parent: Option<String>,
        creating: bool,
    ) -> Result<Self> {
        let project = crate::vcs::resolve_project_root(project)
            .display()
            .to_string();
        let mut state = crate::state::LocalStateManager::new()?;
        let project_id = state.update_control(Path::new(&project), |c| Ok(c.id.clone()))?;
        let key = identity_digest(&[&project_id.0, workspace]);
        let root = crate::paths::devflow_config_dir()?.join("operations");
        // Workspace operations share a policy lease. Retention/removal excludes
        // concurrent creation of dependencies while applying a deletion plan.
        let project_lease = project_policy_lease(
            Path::new(&project),
            kind.starts_with("cleanup:") || kind == "baseline-remove" || kind == "remove",
        )
        .await?;
        let lease = OperationLease::acquire(&root.join(format!("{key}.lock"))).await?;
        let safe_kind = identity_digest(&[kind]);
        let project_root = root.join(&project_id.0);
        std::fs::create_dir_all(&project_root)?;
        let path = project_root.join(format!("{key}-{safe_kind}.json"));
        let previous_flat = root.join(format!("{key}-{safe_kind}.json"));
        if !path.exists() && previous_flat.exists() {
            std::fs::rename(previous_flat, &path)?;
        }
        let legacy_path = root.join(format!(
            "{}-{kind}.json",
            identity_digest(&[&project, workspace])
        ));
        if !path.exists() && legacy_path.exists() {
            std::fs::rename(legacy_path, &path)?;
        }
        let previous: Option<OperationRecord> = if path.exists() {
            Some(
                serde_json::from_slice(&std::fs::read(&path)?)
                    .context("invalid operation journal; refusing to discard recovery intent")?,
            )
        } else {
            None
        };
        if let Some(previous) = previous
            .as_ref()
            .filter(|r| matches!(r.status.as_str(), "ready" | "cancelled"))
        {
            Self::archive(previous, &project_id)?;
        }
        let mut record = if let Some(previous) =
            previous.filter(|r| !matches!(r.status.as_str(), "ready" | "cancelled"))
        {
            anyhow::ensure!(
                (previous.schema_version == 1 || previous.schema_version == 2)
                    && (previous.project == project || previous.project_id == project_id)
                    && previous.workspace == workspace
                    && previous.kind == kind,
                "operation journal identity mismatch"
            );
            previous
        } else {
            OperationRecord {
                schema_version: 2,
                project_id: project_id.clone(),
                attempt: 0,
                request: serde_json::Value::Null,
                config_fingerprint: None,
                steps: Vec::new(),
                updated_at: chrono::Utc::now(),
                id: uuid::Uuid::new_v4().to_string(),
                kind: kind.into(),
                project: project.clone(),
                workspace: workspace.into(),
                code_parent,
                data_parent,
                worktree_path: None,
                status: "running".into(),
                phase: "preflight".into(),
                post_create_pending: creating,
                completed_phases: Vec::new(),
                error: None,
            }
        };
        record.project_id = project_id;
        record.project = project.clone();
        record.schema_version = 2;
        record.attempt += 1;
        state.update_control(Path::new(&project), |control| {
            let workspace = control.workspace(workspace);
            workspace.operation_id = Some(record.id.clone());
            workspace.desired = if kind == "remove" {
                crate::state::control::DesiredState::Absent
            } else {
                crate::state::control::DesiredState::Present
            };
            Ok(())
        })?;
        let mut journal = Self {
            record,
            path,
            _lease: lease,
            _project_lease: project_lease,
            finished: false,
        };
        journal.phase("preflight")?;
        Ok(journal)
    }

    pub fn records(project: &Path, include_history: bool) -> Result<Vec<OperationRecord>> {
        let canonical = crate::vcs::resolve_project_root(project)
            .display()
            .to_string();
        let project_id = crate::state::LocalStateManager::new()?.control(project).id;
        let root = crate::paths::devflow_config_dir()?.join("operations");
        let mut records = Vec::new();
        let mut dirs = vec![root.join(&project_id.0), root.clone()];
        if include_history {
            dirs.push(root.join("history").join(&project_id.0));
        }
        for dir in dirs {
            if !dir.exists() {
                continue;
            }
            for entry in std::fs::read_dir(dir)? {
                let path = entry?.path();
                if path.extension().is_none_or(|e| e != "json") {
                    continue;
                }
                let bytes = std::fs::read(&path)?;
                let record: OperationRecord = match serde_json::from_slice(&bytes) {
                    Ok(record) => record,
                    Err(error) if path.parent() == Some(root.as_path()) => {
                        log::warn!(
                            "Unassigned legacy journal {} is invalid: {error}",
                            path.display()
                        );
                        continue;
                    }
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("invalid operation journal {}", path.display())
                        })
                    }
                };
                if (record.project == canonical || record.project_id == project_id)
                    && !records.iter().any(|r: &OperationRecord| r.id == record.id)
                {
                    records.push(record);
                }
            }
        }
        records.sort_by_key(|record| std::cmp::Reverse(record.updated_at));
        Ok(records)
    }
    pub fn pending(project: &Path) -> Result<Vec<OperationRecord>> {
        Ok(Self::records(project, false)?
            .into_iter()
            .filter(|r| !matches!(r.status.as_str(), "ready" | "cancelled"))
            .collect())
    }
    pub fn inspect(project: &Path, id: &str) -> Result<OperationRecord> {
        Self::records(project, true)?
            .into_iter()
            .find(|r| r.id == id)
            .context("operation not found in this project")
    }
    pub fn set_request(
        &mut self,
        mut request: serde_json::Value,
        config: &crate::config::Config,
    ) -> Result<()> {
        if let Some(options) = request.get_mut("options") {
            if let Some(saved) = self.record.request.get("options") {
                // A materialized retry need not repeat -c or omitted parent flags.
                for field in ["create_if_missing", "from_workspace", "data_from_workspace"] {
                    let omitted = options
                        .get(field)
                        .is_none_or(|v| v.is_null() || v == &serde_json::json!(false));
                    if omitted {
                        if let Some(value) = saved.get(field) {
                            options[field] = value.clone();
                        }
                    }
                }
            }
        }
        // Presentation and approval UI modes are not part of resource intent.
        if let Some(lifecycle) = request
            .get_mut("options")
            .and_then(|v| v.get_mut("lifecycle"))
        {
            lifecycle["verbose_hooks"] = serde_json::json!(false);
            lifecycle["hook_approval"] = serde_json::json!("NonInteractive");
        }
        if !self.record.request.is_null() {
            let mut saved = self.record.request.clone();
            let mut proposed = request.clone();
            for value in [&mut saved, &mut proposed] {
                if let Some(options) = value.get_mut("options").and_then(|v| v.as_object_mut()) {
                    options.remove("lifecycle");
                }
            }
            anyhow::ensure!(
                saved == proposed,
                "operation {} has different saved resource inputs; inspect it before retrying",
                self.record.id
            );
        }
        self.record.request = request;
        let config = serde_json::to_string(config)?;
        let fingerprint = identity_digest(&[&config]);
        if self
            .record
            .config_fingerprint
            .as_ref()
            .is_some_and(|old| old != &fingerprint)
        {
            log::warn!("Configuration changed while retrying operation {}; completed external actions are inspected before reuse", self.record.id);
        }
        self.record.config_fingerprint = Some(fingerprint);
        self.save()
    }
    fn archive(record: &OperationRecord, project_id: &crate::state::ProjectId) -> Result<()> {
        let root = crate::paths::devflow_config_dir()?
            .join("operations/history")
            .join(&project_id.0);
        atomic_write(
            &root.join(format!("{}.json", record.id)),
            &serde_json::to_vec_pretty(record)?,
        )?;
        let mut files = std::fs::read_dir(&root)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect::<Vec<_>>();
        files.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
        let remove_count = files.len().saturating_sub(100);
        for path in files.into_iter().take(remove_count) {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }
    pub fn checkpoint(
        &mut self,
        name: &str,
        kind: StepKind,
        success: bool,
        detail: Option<String>,
    ) -> Result<()> {
        let status = if success {
            StepStatus::Succeeded
        } else {
            StepStatus::Failed
        };
        if let Some(step) = self.record.steps.iter_mut().find(|s| s.name == name) {
            step.status = status;
            step.detail = detail;
        } else {
            self.record.steps.push(OperationStep {
                name: name.into(),
                kind,
                status,
                attempts: self.record.attempt,
                detail,
            });
        }
        self.save()
    }

    pub fn phase(&mut self, phase: &str) -> Result<()> {
        self.record.updated_at = chrono::Utc::now();
        if let Some(step) = self.record.steps.iter_mut().find(|s| s.name == phase) {
            step.attempts += 1;
            step.status = StepStatus::Running;
        } else {
            self.record.steps.push(OperationStep {
                name: phase.into(),
                kind: match phase {
                    "materialize" => StepKind::Materialize,
                    "services" | "provider" => StepKind::Service,
                    "processes" => StepKind::Process,
                    "seed" => StepKind::Seed,
                    "verify" => StepKind::Verify,
                    p if p.starts_with("pre-") || p.starts_with("post-") => StepKind::Hook,
                    _ => StepKind::Other,
                },
                status: StepStatus::Running,
                attempts: 1,
                detail: None,
            });
        }
        self.record.phase = phase.into();
        self.record.status = "running".into();
        self.record.error = None;
        self.save()
    }

    pub fn completed(&mut self, phase: &str) -> Result<()> {
        if let Some(step) = self.record.steps.iter_mut().find(|s| s.name == phase) {
            step.status = StepStatus::Succeeded;
        }
        if !self.record.completed_phases.iter().any(|p| p == phase) {
            self.record.completed_phases.push(phase.into());
        }
        if phase == "post-create" {
            self.record.post_create_pending = false;
        }
        self.save()
    }

    pub fn save(&self) -> Result<()> {
        atomic_write(&self.path, &serde_json::to_vec_pretty(&self.record)?)
    }

    pub fn finish(&mut self, ready: bool) -> Result<()> {
        self.record.status = if ready { "ready" } else { "needs_attention" }.into();
        for step in &mut self.record.steps {
            if step.status == StepStatus::Running {
                step.status = if ready {
                    StepStatus::Succeeded
                } else {
                    StepStatus::NeedsAttention
                };
            }
        }
        self.record.updated_at = chrono::Utc::now();
        self.save()?;
        crate::state::LocalStateManager::new()?.update_control(
            Path::new(&self.record.project),
            |control| {
                let workspace = control.workspace(&self.record.workspace);
                if ready && self.record.kind == "switch" {
                    workspace.desired = crate::state::control::DesiredState::Ready;
                }
                if ready && self.record.kind == "remove" {
                    workspace.desired = crate::state::control::DesiredState::Absent;
                }
                Ok(())
            },
        )?;
        if ready && matches!(self.record.kind.as_str(), "remove" | "baseline-remove") {
            // Explicit resource removal resolves superseded setup intent as cancelled.
            for entry in
                std::fs::read_dir(self.path.parent().context("operation directory missing")?)?
            {
                let path = entry?.path();
                if path == self.path || path.extension().is_none_or(|e| e != "json") {
                    continue;
                }
                let mut old: OperationRecord = serde_json::from_slice(&std::fs::read(&path)?)?;
                if old.workspace == self.record.workspace && old.status != "ready" {
                    old.status = "cancelled".into();
                    old.error = Some(format!("resource removed by operation {}", self.record.id));
                    atomic_write(&path, &serde_json::to_vec_pretty(&old)?)?;
                    Self::archive(&old, &self.record.project_id)?;
                }
            }
        }
        Self::archive(&self.record, &self.record.project_id)?;
        self.finished = true;
        Ok(())
    }

    pub fn fail(&mut self, error: anyhow::Error) -> anyhow::Error {
        self.record.status = "failed".into();
        self.record.error = Some(format!("{error:#}"));
        self.record.updated_at = chrono::Utc::now();
        for step in &mut self.record.steps {
            if step.status == StepStatus::Running {
                step.status = StepStatus::Failed;
                step.detail = self.record.error.clone();
            }
        }
        let _ = self.save();
        self.finished = true;
        OperationError {
            operation: self.record.clone(),
            message: format!("{error:#}"),
        }
        .into()
    }
}

impl Drop for OperationJournal {
    fn drop(&mut self) {
        if !self.finished {
            self.record.status = "interrupted".into();
            self.record.updated_at = chrono::Utc::now();
            for step in &mut self.record.steps {
                if step.status == StepStatus::Running {
                    step.status = StepStatus::Interrupted;
                }
            }
            let _ = self.save();
        }
    }
}
