//! Read-only workspace resolution, also consumed by the lifecycle executor.
use super::switch::SwitchOptions;
use crate::{baseline::DataOrigin, config::Config, state::LocalStateManager, vcs};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspacePlan {
    pub schema_version: u32,
    pub workspace: String,
    pub service_key: String,
    pub creating: bool,
    pub code_parent: Option<String>,
    pub code_revision: Option<String>,
    /// jj snapshots edits during an explicit mutation, producing a new revision.
    pub snapshot_required: bool,
    pub data_parent: Option<String>,
    pub data_origins: BTreeMap<String, DataOrigin>,
    pub worktree_path: PathBuf,
    pub payloads: Vec<String>,
    pub caches: Vec<crate::cache::CacheSpec>,
    pub hooks: serde_json::Value,
    pub protections: Vec<String>,
}
pub fn resolve(
    config: &Config,
    project: &Path,
    workspace: &str,
    options: &SwitchOptions,
) -> Result<WorkspacePlan> {
    super::validate_workspace_name(workspace).map_err(anyhow::Error::msg)?;
    let repo = vcs::detect_vcs_provider(project)?;
    let state = LocalStateManager::new()?;
    let existing = state.get_workspace_by_dir(project, workspace);
    let path = repo.worktree_path(workspace)?;
    let creating = path.is_none() && workspace != config.git.main_workspace;
    let code_parent = existing
        .as_ref()
        .and_then(|w| w.parent.clone())
        .or_else(|| options.from_workspace.clone())
        .or(if creating {
            repo.current_workspace()?
        } else {
            None
        });
    let data_parent = options
        .data_from_workspace
        .clone()
        .or_else(|| {
            existing
                .as_ref()
                .and_then(|w| w.data_parent.clone().or(w.parent.clone()))
        })
        .or_else(|| code_parent.clone());
    let exists = repo.workspace_exists(workspace)?;
    anyhow::ensure!(
        exists || options.create_if_missing,
        "Workspace '{workspace}' does not exist. Use the create flag to create it."
    );
    if !exists {
        if let Some(parent) = &code_parent {
            anyhow::ensure!(
                repo.workspace_exists(parent)?,
                "Code parent workspace '{parent}' does not exist"
            );
        }
    }
    let code_source = if exists {
        Some(workspace)
    } else {
        code_parent.as_deref()
    };
    let code_revision = code_source
        .map(|name| repo.workspace_revision(name))
        .transpose()?
        .flatten();
    let snapshot_required = repo.provider_name() == "jj"
        && code_source
            .map(|name| repo.worktree_path(name))
            .transpose()?
            .flatten()
            .is_some_and(|path| repo.worktree_is_dirty(&path).unwrap_or(true));
    let control = state.control(project);
    let mut protections = Vec::new();
    if workspace == config.git.main_workspace {
        protections.push("primary workspace".into());
    }
    if control.workspaces.get(workspace).is_some_and(|w| w.pinned) {
        protections.push("pinned".into());
    }
    Ok(WorkspacePlan {
        schema_version: 1,
        workspace: workspace.into(),
        service_key: state.resolve_workspace_service_key_by_dir(project, workspace)?,
        creating,
        code_parent,
        code_revision,
        snapshot_required,
        data_origins: crate::baseline::resolve_origins(
            config,
            project,
            workspace,
            data_parent.as_deref(),
        )?,
        data_parent,
        worktree_path: crate::paths::resolve_expected_path(&path.unwrap_or_else(|| {
            super::worktree::resolve_worktree_path(config, project, workspace)
        }))?,
        payloads: options
            .copy_files
            .clone()
            .unwrap_or_else(|| config.worktree.copy_files.clone()),
        caches: config.worktree.caches.clone(),
        hooks: if options.lifecycle.skip_hooks {
            serde_json::Value::Null
        } else {
            serde_json::to_value(&config.hooks)?
        },
        protections,
    })
}
