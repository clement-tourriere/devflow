//! Per-worktree setup receipt, stored in the worktree's own Git directory
//! (`git rev-parse --git-dir`/devflow/receipt.json) so it is unique per
//! worktree and disappears with `git worktree remove`/`prune`.
//!
//! Every pipeline phase is recorded as `started` before its effect and
//! `done`/`skipped`/`failed` after it; re-running setup resumes at the first
//! phase that is not done.
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Materialize,
    Payload,
    Database,
    Env,
    Hooks,
}

impl Phase {
    pub const ALL: [Phase; 5] = [
        Phase::Materialize,
        Phase::Payload,
        Phase::Database,
        Phase::Env,
        Phase::Hooks,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Materialize => "materialize",
            Phase::Payload => "payload",
            Phase::Database => "database",
            Phase::Env => "env",
            Phase::Hooks => "hooks",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseState {
    #[default]
    Pending,
    Started,
    Done,
    /// Deliberately not run (no database configured, hooks untrusted, …).
    /// `required_skip` in the record says whether readiness is affected.
    Skipped,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseRecord {
    pub state: PhaseState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// A skip that leaves the workspace `needs_attention` (e.g. untrusted hooks).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required_skip: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    InProgress,
    Ready,
    NeedsAttention,
}

/// The database this worktree is attached to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataBinding {
    /// Server-side branch id; `None` when attached with only a URL
    /// (`devflow setup --database-url`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<uuid::Uuid>,
    pub database: String,
    pub role: String,
    /// Full application URL including the branch password.
    pub url: String,
    /// What the branch was created from (`empty`, `workspace:NAME`, `snapshot:NAME@vN`, `url`).
    pub source: String,
    /// This instance created the branch, so it may reset or drop it.
    /// `false` for URL-only attachments and copied (forked) receipts.
    #[serde(default)]
    pub owned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub version: u32,
    pub instance_id: uuid::Uuid,
    pub project_id: uuid::Uuid,
    /// Workspace (branch) name at creation.
    pub workspace: String,
    /// Workspace the code (and payloads) came from; `None` for adopted checkouts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_source: Option<String>,
    /// Requested data source, recorded before provisioning so a retry uses the same one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_source: Option<String>,
    /// The identity `data_source` resolved to (`empty`, `branch:<id>`,
    /// `snapshot:<id>`), recorded together with it: a retry clones exactly this
    /// source even if the workspace or snapshot name now means something else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_source_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<DataBinding>,
    #[serde(default)]
    pub phases: std::collections::BTreeMap<String, PhaseRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks_digest: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Receipt {
    pub fn new(project_id: uuid::Uuid, workspace: &str, code_source: Option<String>) -> Self {
        let now = Utc::now();
        Self {
            version: VERSION,
            // Always a fresh identity: `DEVFLOW_INSTANCE_ID` is applied by an
            // explicit `setup::setup` alone, never by whatever creates a receipt.
            instance_id: uuid::Uuid::new_v4(),
            project_id,
            workspace: workspace.to_string(),
            code_source,
            data_source: None,
            data_source_id: None,
            data: None,
            phases: Default::default(),
            hooks_digest: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn phase(&self, phase: Phase) -> PhaseRecord {
        self.phases.get(phase.as_str()).cloned().unwrap_or_default()
    }

    pub fn set_phase(&mut self, phase: Phase, state: PhaseState, detail: Option<String>) {
        let now = Utc::now();
        self.phases.insert(
            phase.as_str().to_string(),
            PhaseRecord {
                state,
                detail,
                required_skip: false,
                updated_at: Some(now),
            },
        );
        self.updated_at = now;
    }

    pub fn skip_required(&mut self, phase: Phase, detail: String) {
        self.set_phase(phase, PhaseState::Skipped, Some(detail));
        if let Some(record) = self.phases.get_mut(phase.as_str()) {
            record.required_skip = true;
        }
    }

    pub fn status(&self) -> Status {
        let records: Vec<_> = Phase::ALL.iter().map(|p| self.phase(*p)).collect();
        if records.iter().any(|r| {
            r.state == PhaseState::Failed || (r.state == PhaseState::Skipped && r.required_skip)
        }) {
            Status::NeedsAttention
        } else if records
            .iter()
            .all(|r| matches!(r.state, PhaseState::Done | PhaseState::Skipped))
        {
            Status::Ready
        } else {
            Status::InProgress
        }
    }

    pub fn path(git_dir: &Path) -> PathBuf {
        git_dir.join("devflow").join("receipt.json")
    }

    pub fn load(git_dir: &Path) -> Result<Option<Receipt>> {
        let path = Self::path(git_dir);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let receipt: Receipt = serde_json::from_str(&text)
                    .with_context(|| format!("invalid receipt {}", path.display()))?;
                anyhow::ensure!(
                    receipt.version <= VERSION,
                    "{} was written by a newer devflow",
                    path.display()
                );
                Ok(Some(receipt))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    pub fn save(&self, git_dir: &Path) -> Result<()> {
        let text = serde_json::to_vec_pretty(self)?;
        crate::fsutil::write_atomic(&Self::path(git_dir), &text, true)
    }

    /// Exclusive lock for a setup run on this worktree. Returns `None` when
    /// another process holds it.
    pub fn try_lock(git_dir: &Path) -> Result<Option<ReceiptLock>> {
        let dir = git_dir.join("devflow");
        std::fs::create_dir_all(&dir)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("setup.lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(ReceiptLock { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
        }
    }

    /// Whether a setup run currently holds the lock (for status display).
    pub fn is_locked(git_dir: &Path) -> bool {
        matches!(Self::try_lock(git_dir), Ok(None))
    }

    pub fn logs_dir(git_dir: &Path) -> PathBuf {
        git_dir.join("devflow").join("logs")
    }
}

/// Held for the duration of a setup run; released on drop.
pub struct ReceiptLock {
    _file: std::fs::File,
}

/// `DEVFLOW_INSTANCE_ID` lets an orchestrator give a forked VM a known
/// identity. Only [`crate::setup::setup`] (the explicit `devflow setup`) reads
/// it; the generated `.devflow/env` exports the informational
/// `DEVFLOW_WORKSPACE_ID` instead, so sourcing it cannot set this.
pub(crate) fn instance_id_override() -> Option<uuid::Uuid> {
    std::env::var("DEVFLOW_INSTANCE_ID")
        .ok()
        .and_then(|v| v.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_follows_phase_states() {
        let mut receipt = Receipt::new(uuid::Uuid::new_v4(), "feature", None);
        assert_eq!(receipt.status(), Status::InProgress);
        for phase in [Phase::Materialize, Phase::Payload, Phase::Env] {
            receipt.set_phase(phase, PhaseState::Done, None);
        }
        receipt.set_phase(
            Phase::Database,
            PhaseState::Skipped,
            Some("no [postgres]".into()),
        );
        receipt.skip_required(Phase::Hooks, "hooks are not trusted".into());
        assert_eq!(receipt.status(), Status::NeedsAttention);
        receipt.set_phase(Phase::Hooks, PhaseState::Done, None);
        assert_eq!(receipt.status(), Status::Ready);
    }

    #[test]
    fn saves_privately_and_locks_exclusively() {
        let dir = tempfile::tempdir().unwrap();
        let receipt = Receipt::new(uuid::Uuid::new_v4(), "feature", Some("main".into()));
        receipt.save(dir.path()).unwrap();
        assert_eq!(Receipt::load(dir.path()).unwrap(), Some(receipt));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(Receipt::path(dir.path()))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let lock = Receipt::try_lock(dir.path()).unwrap();
        assert!(lock.is_some());
        assert!(Receipt::try_lock(dir.path()).unwrap().is_none());
        drop(lock);
        assert!(Receipt::try_lock(dir.path()).unwrap().is_some());
    }
}
