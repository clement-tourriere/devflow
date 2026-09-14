//! A separate durable record for every explicit execution.
use super::CommandSpec;
use crate::{
    process_identity::ProcessIdentity,
    services::ownership::{atomic_write, OperationLease},
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    Starting,
    Running,
    Exited,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionRecord {
    pub id: String,
    pub project: PathBuf,
    pub workspace: String,
    pub workdir: PathBuf,
    pub command: CommandSpec,
    pub state: ExecutionState,
    pub owner: Option<ProcessIdentity>,
    pub child: Option<ProcessIdentity>,
    pub session: Option<String>,
    pub exit_code: Option<i32>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}
impl ExecutionRecord {
    pub fn active(&self) -> bool {
        matches!(
            self.state,
            ExecutionState::Starting | ExecutionState::Running
        ) && self.owner.as_ref().is_some_and(ProcessIdentity::is_alive)
    }
}
pub(super) struct Guard {
    pub record: ExecutionRecord,
    pub path: PathBuf,
    lease: Option<OperationLease>,
    finished: bool,
}
fn directory(project: &Path) -> Result<PathBuf> {
    let project = crate::vcs::resolve_project_root(project);
    Ok(crate::paths::devflow_config_dir()?.join("executions").join(
        crate::state::LocalStateManager::new()?
            .control(&project)
            .id
            .0,
    ))
}
impl Guard {
    pub fn new(
        project: &Path,
        workspace: &str,
        workdir: &Path,
        command: &CommandSpec,
    ) -> Result<Self> {
        let id = uuid::Uuid::new_v4().to_string();
        let path = directory(project)?.join(format!("{id}.json"));
        let guard = Self {
            record: ExecutionRecord {
                id,
                project: crate::vcs::resolve_project_root(project),
                workspace: workspace.into(),
                workdir: workdir.into(),
                command: command.clone(),
                state: ExecutionState::Starting,
                owner: ProcessIdentity::read(std::process::id()),
                child: None,
                session: None,
                exit_code: None,
                updated_at: chrono::Utc::now(),
            },
            path,
            lease: None,
            finished: false,
        };
        guard.save()?;
        Ok(guard)
    }
    pub fn open(path: &Path) -> Result<Self> {
        let record: ExecutionRecord = serde_json::from_slice(&std::fs::read(path)?)?;
        anyhow::ensure!(
            path == directory(&record.project)?.join(format!("{}.json", record.id)),
            "Execution record identity mismatch"
        );
        Ok(Self {
            record,
            path: path.into(),
            lease: None,
            finished: true,
        })
    }
    pub fn claim(&mut self) -> Result<()> {
        self.lease = Some(
            OperationLease::try_acquire(&self.path.with_extension("lock"))?
                .context("Execution is already owned by another runner")?,
        );
        self.record = self.read()?;
        anyhow::ensure!(
            matches!(self.record.state, ExecutionState::Starting),
            "Execution has already been claimed or completed"
        );
        self.finished = false;
        self.record.owner = ProcessIdentity::read(std::process::id());
        self.record.state = ExecutionState::Running;
        self.save()
    }
    pub fn read(&self) -> Result<ExecutionRecord> {
        Ok(serde_json::from_slice(&std::fs::read(&self.path)?)?)
    }
    /// Cancel an unclaimed launch under the same lease used by the runner.
    /// If a runner won the race, return its actual record instead.
    pub fn cancel_start(&mut self, code: i32) -> Result<Option<ExecutionRecord>> {
        self.lease = OperationLease::try_acquire(&self.path.with_extension("lock"))?;
        let current = self.read()?;
        if self.lease.is_none() || !matches!(current.state, ExecutionState::Starting) {
            self.transfer();
            return Ok(Some(current));
        }
        self.finish(code)?;
        Ok(None)
    }
    pub fn save(&self) -> Result<()> {
        atomic_write(&self.path, &serde_json::to_vec_pretty(&self.record)?)
    }
    pub fn finish(&mut self, code: i32) -> Result<()> {
        self.record.state = if code == 0 {
            ExecutionState::Exited
        } else {
            ExecutionState::Failed
        };
        self.record.exit_code = Some(code);
        self.record.updated_at = chrono::Utc::now();
        self.save()?;
        self.finished = true;
        Ok(())
    }
    pub fn transfer(&mut self) {
        self.finished = true;
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // A detached runner may already have claimed the record.
        if self.lease.is_none() {
            let Ok(Some(lease)) = OperationLease::try_acquire(&self.path.with_extension("lock"))
            else {
                return;
            };
            self.lease = Some(lease);
        }
        if let Ok(bytes) = std::fs::read(&self.path) {
            if let Ok(current) = serde_json::from_slice::<ExecutionRecord>(&bytes) {
                if current.owner != self.record.owner {
                    return;
                }
            }
        }
        self.record.state = if matches!(self.record.state, ExecutionState::Starting) {
            ExecutionState::Failed
        } else {
            ExecutionState::Interrupted
        };
        self.record.updated_at = chrono::Utc::now();
        let _ = self.save();
    }
}
pub fn list(project: &Path) -> Result<Vec<ExecutionRecord>> {
    let dir = directory(project)?;
    if !dir.try_exists()? {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "json") {
            let mut record: ExecutionRecord = serde_json::from_slice(&std::fs::read(path)?)?;
            if matches!(
                record.state,
                ExecutionState::Starting | ExecutionState::Running
            ) && !record.active()
            {
                record.state = ExecutionState::Interrupted;
            }
            records.push(record);
        }
    }
    records.sort_by_key(|record| std::cmp::Reverse(record.updated_at));
    Ok(records)
}
