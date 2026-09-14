//! Authoritative identities, desired state, data bindings and retention policy.
//! Provider stores contain observations; this registry preserves user intent.
use super::{LocalStateManager, ProjectState};
use crate::services::ownership::identity_digest;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

macro_rules! identity {
    ($name:ident) => {
        #[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);
    };
}
identity!(ProjectId);
identity!(WorkspaceId);
identity!(ServiceId);
identity!(ResourceId);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceControl {
    pub id: WorkspaceId,
    pub desired: DesiredState,
    pub operation_id: Option<String>,
    pub pinned: bool,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub data: BTreeMap<String, crate::baseline::DataOrigin>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DesiredState {
    #[default]
    Present,
    Ready,
    Absent,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectControl {
    pub id: ProjectId,
    pub name: Option<String>,
    /// Preserve the old provider namespace through path relocation.
    pub resource_identity: String,
    pub visible: bool,
    pub workspaces: BTreeMap<String, WorkspaceControl>,
    pub services: BTreeMap<String, ServiceId>,
    pub baselines: BTreeMap<String, Vec<crate::baseline::Baseline>>,
    pub baseline_versions: BTreeMap<String, u64>,
}

impl ProjectControl {
    pub(super) fn initialize(&mut self, path: &str) {
        if self.id.0.is_empty() {
            self.id = ProjectId(identity_digest(&["project", path]));
            self.resource_identity = format!("path:{path}");
            self.visible = true;
        }
    }
    pub fn workspace(&mut self, name: &str) -> &mut WorkspaceControl {
        let id = WorkspaceId(identity_digest(&[&self.id.0, "workspace", name]));
        self.workspaces
            .entry(name.into())
            .or_insert_with(|| WorkspaceControl {
                id,
                ..Default::default()
            })
    }
    pub fn service(&mut self, name: &str) -> ServiceId {
        self.services
            .entry(name.into())
            .or_insert_with(|| ServiceId(identity_digest(&[&self.id.0, "service", name])))
            .clone()
    }
}

impl LocalStateManager {
    pub fn control(&self, project: &Path) -> ProjectControl {
        let key = crate::vcs::resolve_project_root(project)
            .display()
            .to_string();
        let mut control = self
            .state
            .projects
            .get(&key)
            .map(|p| p.control.clone())
            .unwrap_or_default();
        control.initialize(&key);
        control
    }

    pub fn update_control<T>(
        &mut self,
        project: &Path,
        update: impl FnOnce(&mut ProjectControl) -> Result<T>,
    ) -> Result<T> {
        let key = crate::vcs::resolve_project_root(project)
            .display()
            .to_string();
        self.mutate_state(|state| {
            let project = state
                .projects
                .entry(key.clone())
                .or_insert_with(|| ProjectState {
                    last_updated: chrono::Utc::now(),
                    services: None,
                    workspaces: None,
                    control: ProjectControl::default(),
                });
            project.control.initialize(&key);
            project.last_updated = chrono::Utc::now();
            update(&mut project.control)
        })
    }

    pub fn register_project(&mut self, path: &Path, name: Option<&str>) -> Result<ProjectId> {
        self.update_control(path, |control| {
            if let Some(name) = name {
                control.name = Some(name.into());
            }
            control.visible = true;
            Ok(control.id.clone())
        })
    }

    /// Explicit relocation changes the path attribute without renaming resources.
    pub fn relocate_project(&mut self, old: &Path, new: &Path) -> Result<ProjectId> {
        let old = if self.state.projects.contains_key(&old.display().to_string()) {
            old.to_path_buf()
        } else {
            old.canonicalize().or_else(|_| {
                old.parent()
                    .context("old project has no parent")
                    .and_then(|p| {
                        Ok(p.canonicalize()?
                            .join(old.file_name().context("old project has no name")?))
                    })
                    .map_err(std::io::Error::other)
            })?
        }
        .display()
        .to_string();
        let new = new
            .canonicalize()
            .context("new project directory does not exist")?
            .display()
            .to_string();
        self.mutate_state(|state| {
            anyhow::ensure!(
                !state.projects.contains_key(&new),
                "destination is already registered"
            );
            let mut project = state
                .projects
                .remove(&old)
                .context("old project path is not registered")?;
            project.control.initialize(&old);
            for workspace in project.workspaces.iter_mut().flatten() {
                if let Some(path) = workspace.worktree_path.as_mut() {
                    if let Ok(suffix) = Path::new(path).strip_prefix(&old) {
                        *path = Path::new(&new).join(suffix).display().to_string();
                    }
                }
            }
            let id = project.control.id.clone();
            state.projects.insert(new, project);
            Ok(id)
        })
    }
}
