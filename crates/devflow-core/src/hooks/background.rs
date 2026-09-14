//! Durable completion receipts for optional background hooks.
use anyhow::Result;
use serde::Serialize;
use std::path::{Path, PathBuf};
#[derive(Serialize)]
struct Record {
    id: String,
    hook: String,
    project: String,
    workspace: String,
    pid: u32,
    status: String,
    error: Option<String>,
}
pub(super) struct Receipt {
    record: Record,
    path: PathBuf,
    finished: bool,
}
impl Receipt {
    pub fn begin(root: &Path, workspace: &str, hook: &str) -> Result<Self> {
        let id = uuid::Uuid::new_v4().to_string();
        let receipt = Self {
            path: crate::paths::devflow_config_dir()?
                .join("hook-runs")
                .join(format!("{id}.json")),
            record: Record {
                id,
                hook: hook.into(),
                project: crate::vcs::resolve_project_root(root).display().to_string(),
                workspace: workspace.into(),
                pid: std::process::id(),
                status: "running".into(),
                error: None,
            },
            finished: false,
        };
        receipt.save()?;
        Ok(receipt)
    }
    pub fn id(&self) -> String {
        self.record.id.clone()
    }
    pub fn complete(&mut self, result: Result<()>) {
        self.record.status = if result.is_ok() {
            "succeeded"
        } else {
            "failed"
        }
        .into();
        self.record.error = result.err().map(|e| format!("{e:#}"));
        self.finished = true;
        if let Err(error) = self.save() {
            log::warn!("cannot save background hook completion: {error:#}");
        }
    }
    fn save(&self) -> Result<()> {
        crate::services::ownership::atomic_write(
            &self.path,
            &serde_json::to_vec_pretty(&self.record)?,
        )
    }
}
impl Drop for Receipt {
    fn drop(&mut self) {
        if !self.finished {
            self.record.status = "interrupted".into();
            let _ = self.save();
        }
    }
}
