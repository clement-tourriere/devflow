use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::{build_checks_from_config, run_checks, MergeReadinessReport};

/// Status of a merge train.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MergeTrainStatus {
    Active,
    Paused,
    Completed,
}

/// Status of an individual entry in the merge train.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MergeTrainEntryStatus {
    Queued,
    Checking,
    Merging,
    Succeeded,
    Failed,
    NeedsRebase,
    Cancelled,
}

/// A merge train — an ordered queue of workspaces to merge into a target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeTrain {
    pub id: String,
    pub target: String,
    pub created_at: DateTime<Utc>,
    pub entries: Vec<MergeTrainEntry>,
    pub status: MergeTrainStatus,
}

/// An entry in the merge train queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeTrainEntry {
    pub workspace: String,
    pub position: usize,
    pub status: MergeTrainEntryStatus,
    pub added_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_results: Option<MergeReadinessReport>,
}

/// Engine that manages merge train operations.
pub struct MergeTrainEngine {
    project_dir: PathBuf,
    config: crate::config::Config,
}

impl MergeTrainEngine {
    pub fn new(project_dir: &Path, config: &crate::config::Config) -> Self {
        Self {
            project_dir: project_dir.to_path_buf(),
            config: config.clone(),
        }
    }

    /// Load the merge train for a target from local state, or create a new one.
    fn load_train(&self, target: &str) -> Result<MergeTrain> {
        let state_mgr = crate::state::LocalStateManager::new()?;
        let trains = state_mgr.get_merge_trains(&self.project_dir);
        if let Some(train) = trains.into_iter().find(|t| t.target == target) {
            Ok(train)
        } else {
            Ok(MergeTrain {
                id: uuid::Uuid::new_v4().to_string(),
                target: target.to_string(),
                created_at: Utc::now(),
                entries: Vec::new(),
                status: MergeTrainStatus::Active,
            })
        }
    }

    fn save_train(&self, train: &MergeTrain) -> Result<()> {
        let mut state_mgr = crate::state::LocalStateManager::new()?;
        state_mgr.save_merge_train(&self.project_dir, train)
    }

    /// Add a workspace to the merge train queue.
    pub fn enqueue(&self, workspace: &str, target: &str) -> Result<()> {
        let mut train = self.load_train(target)?;

        if train.entries.iter().any(|e| e.workspace == workspace) {
            anyhow::bail!("Workspace '{}' is already in the merge train", workspace);
        }

        let position = train.entries.len();
        train.entries.push(MergeTrainEntry {
            workspace: workspace.to_string(),
            position,
            status: MergeTrainEntryStatus::Queued,
            added_at: Utc::now(),
            started_at: None,
            completed_at: None,
            error: None,
            check_results: None,
        });

        self.save_train(&train)
    }

    /// Remove a workspace from the merge train queue.
    pub fn dequeue(&self, workspace: &str) -> Result<()> {
        let target = self.config.git.main_workspace.clone();
        let mut train = self.load_train(&target)?;

        let before = train.entries.len();
        train.entries.retain(|e| e.workspace != workspace);
        if train.entries.len() == before {
            anyhow::bail!("Workspace '{}' not found in merge train", workspace);
        }

        // Reindex positions
        for (i, entry) in train.entries.iter_mut().enumerate() {
            entry.position = i;
        }

        self.save_train(&train)
    }

    /// Reorder a workspace to a new position.
    pub fn reorder(&self, workspace: &str, new_position: usize, target: &str) -> Result<()> {
        let mut train = self.load_train(target)?;

        let current_pos = train
            .entries
            .iter()
            .position(|e| e.workspace == workspace)
            .ok_or_else(|| anyhow::anyhow!("Workspace '{}' not found in merge train", workspace))?;

        let entry = train.entries.remove(current_pos);
        let insert_pos = new_position.min(train.entries.len());
        train.entries.insert(insert_pos, entry);

        for (i, e) in train.entries.iter_mut().enumerate() {
            e.position = i;
        }

        self.save_train(&train)
    }

    /// Get the current status of a merge train.
    pub fn status(&self, target: &str) -> Result<Option<MergeTrain>> {
        let state_mgr = crate::state::LocalStateManager::new()?;
        let trains = state_mgr.get_merge_trains(&self.project_dir);
        Ok(trains.into_iter().find(|t| t.target == target))
    }

    /// Pause a merge train.
    pub fn pause(&self, target: &str) -> Result<()> {
        let mut train = self.load_train(target)?;
        train.status = MergeTrainStatus::Paused;
        self.save_train(&train)
    }

    /// Resume a paused merge train.
    pub fn resume(&self, target: &str) -> Result<()> {
        let mut train = self.load_train(target)?;
        train.status = MergeTrainStatus::Active;
        self.save_train(&train)
    }

    /// Process the next queued entry in the merge train.
    ///
    /// Returns the processed entry, or None if there are no queued entries.
    /// When `cleanup_override` is `None`, falls back to the config default.
    pub fn process_next(&self, target: &str, cleanup: bool) -> Result<Option<MergeTrainEntry>> {
        let merge_defaults = self.config.merge.clone().unwrap_or_default();
        let cleanup = merge_defaults.effective_cleanup(cleanup);
        let strategy = merge_defaults.effective_strategy();
        let mut train = self.load_train(target)?;

        if train.status == MergeTrainStatus::Paused {
            return Ok(None);
        }

        let next_idx = train
            .entries
            .iter()
            .position(|e| e.status == MergeTrainEntryStatus::Queued);

        let idx = match next_idx {
            Some(i) => i,
            None => {
                train.status = MergeTrainStatus::Completed;
                self.save_train(&train)?;
                return Ok(None);
            }
        };

        // Run merge readiness checks
        train.entries[idx].status = MergeTrainEntryStatus::Checking;
        train.entries[idx].started_at = Some(Utc::now());
        self.save_train(&train)?;

        let workspace = train.entries[idx].workspace.clone();

        let repo = crate::vcs::detect_vcs_provider(&self.project_dir)?;

        if let Some(ref merge_config) = self.config.merge {
            let checks = build_checks_from_config(merge_config);
            let report = run_checks(&checks, repo.as_ref(), &workspace, target);
            train.entries[idx].check_results = Some(report.clone());

            if !report.ready {
                train.entries[idx].status = MergeTrainEntryStatus::NeedsRebase;
                train.entries[idx].completed_at = Some(Utc::now());
                train.entries[idx].error = Some("Merge readiness checks failed".to_string());
                self.save_train(&train)?;
                return Ok(Some(train.entries[idx].clone()));
            }
        }

        // Perform the merge
        train.entries[idx].status = MergeTrainEntryStatus::Merging;
        self.save_train(&train)?;

        // Switch to target, then merge
        let merge_dir = repo
            .worktree_path(target)?
            .unwrap_or_else(|| self.project_dir.clone());

        let merge_vcs = crate::vcs::detect_vcs_provider(&merge_dir)?;

        // If rebase strategy, rebase source onto target first
        if strategy == crate::config::MergeStrategy::Rebase {
            if merge_dir == self.project_dir {
                if let Err(e) = repo.checkout_workspace(&workspace) {
                    train.entries[idx].status = MergeTrainEntryStatus::Failed;
                    train.entries[idx].completed_at = Some(Utc::now());
                    train.entries[idx].error =
                        Some(format!("Failed to checkout source for rebase: {}", e));
                    self.save_train(&train)?;
                    return Ok(Some(train.entries[idx].clone()));
                }
            }
            match repo.rebase(target) {
                Ok(result) if result.success => {
                    log::info!(
                        "Rebased {} commits for '{}'",
                        result.commits_replayed,
                        workspace
                    );
                }
                Ok(result) => {
                    train.entries[idx].status = MergeTrainEntryStatus::Failed;
                    train.entries[idx].completed_at = Some(Utc::now());
                    train.entries[idx].error = Some(format!(
                        "Rebase conflicts in: {}",
                        result.conflict_files.join(", ")
                    ));
                    self.save_train(&train)?;
                    return Ok(Some(train.entries[idx].clone()));
                }
                Err(e) => {
                    train.entries[idx].status = MergeTrainEntryStatus::Failed;
                    train.entries[idx].completed_at = Some(Utc::now());
                    train.entries[idx].error = Some(format!("Rebase failed: {}", e));
                    self.save_train(&train)?;
                    return Ok(Some(train.entries[idx].clone()));
                }
            }
        }

        // If not in a worktree for target, checkout target first
        if merge_dir == self.project_dir {
            if let Err(e) = merge_vcs.checkout_workspace(target) {
                train.entries[idx].status = MergeTrainEntryStatus::Failed;
                train.entries[idx].completed_at = Some(Utc::now());
                train.entries[idx].error = Some(format!("Failed to checkout target: {}", e));
                self.save_train(&train)?;
                return Ok(Some(train.entries[idx].clone()));
            }
        }

        match merge_vcs.merge_branch(&workspace) {
            Ok(()) => {
                train.entries[idx].status = MergeTrainEntryStatus::Succeeded;
                train.entries[idx].completed_at = Some(Utc::now());

                if cleanup {
                    // Best-effort cleanup of source workspace
                    if let Err(e) = repo.delete_workspace(&workspace) {
                        log::warn!(
                            "Failed to delete workspace '{}' during train cleanup: {}",
                            workspace,
                            e
                        );
                    }
                }
            }
            Err(e) => {
                train.entries[idx].status = MergeTrainEntryStatus::Failed;
                train.entries[idx].completed_at = Some(Utc::now());
                train.entries[idx].error = Some(format!("Merge failed: {}", e));
            }
        }

        self.save_train(&train)?;
        Ok(Some(train.entries[idx].clone()))
    }

    /// Run the entire merge train until completion or failure.
    pub fn run(
        &self,
        target: &str,
        stop_on_failure: bool,
        cleanup: bool,
    ) -> Result<Vec<MergeTrainEntry>> {
        let mut results = Vec::new();

        loop {
            match self.process_next(target, cleanup)? {
                Some(entry) => {
                    let failed = matches!(
                        entry.status,
                        MergeTrainEntryStatus::Failed | MergeTrainEntryStatus::NeedsRebase
                    );
                    results.push(entry);
                    if failed && stop_on_failure {
                        break;
                    }
                }
                None => break,
            }
        }

        Ok(results)
    }
}
