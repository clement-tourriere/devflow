use crate::config::NamedServiceConfig;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

const CURRENT_STATE_SCHEMA_VERSION: u32 = 2;

fn legacy_state_schema_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalState {
    #[serde(default = "legacy_state_schema_version")]
    pub schema_version: u32,
    pub projects: HashMap<String, ProjectState>,
}

impl Default for LocalState {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_STATE_SCHEMA_VERSION,
            projects: HashMap::new(),
        }
    }
}

/// A devflow workspace — an abstraction above git workspaces.
/// Tracks parent-child relationships, worktree paths, and creation time
/// independently of the VCS provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevflowWorkspace {
    /// Exact VCS branch/bookmark name. This is the canonical user-facing
    /// identity and must never be replaced with a sanitized service name.
    pub name: String,
    /// Collision-resistant identifier used by services and worktree paths.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub service_key: String,
    /// Whether `name` has been verified against a live VCS workspace (or was
    /// recorded directly by a raw-name-aware devflow release). Legacy state
    /// keeps this false until the project becomes available for reconciliation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub raw_identity_verified: bool,
    /// Immutable creation/clone provenance, stored as a raw VCS name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Command executed in this workspace (e.g., "claude", "npm run migrate").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executed_command: Option<String>,
    /// Execution status (e.g., "running", "detached", "done", "failed").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_status: Option<String>,
    /// When the command was executed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executed_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectState {
    pub last_updated: chrono::DateTime<chrono::Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub services: Option<Vec<NamedServiceConfig>>,
    /// Registry of devflow workspaces tracked for this project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspaces: Option<Vec<DevflowWorkspace>>,
}

pub struct LocalStateManager {
    state_file_path: PathBuf,
    state: LocalState,
}

struct FileLockGuard {
    path: PathBuf,
}

impl Drop for FileLockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn lock_file_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.lock", path.display()))
}

fn acquire_file_lock(lock_path: &Path) -> Result<FileLockGuard> {
    const MAX_ATTEMPTS: usize = 200;
    const SLEEP_MS: u64 = 25;
    const STALE_LOCK_SECS: u64 = 30;

    for _ in 0..MAX_ATTEMPTS {
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(lock_path)
        {
            Ok(_) => {
                return Ok(FileLockGuard {
                    path: lock_path.to_path_buf(),
                });
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                if let Ok(metadata) = fs::metadata(lock_path) {
                    if let Ok(modified) = metadata.modified() {
                        if modified.elapsed().unwrap_or_default().as_secs() > STALE_LOCK_SECS {
                            let _ = fs::remove_file(lock_path);
                            continue;
                        }
                    }
                }
                thread::sleep(Duration::from_millis(SLEEP_MS));
            }
            Err(e) => {
                let msg = format!("Failed to acquire lock '{}': {}", lock_path.display(), e);
                return Err(e).context(msg);
            }
        }
    }

    anyhow::bail!("Timed out waiting for lock file '{}'", lock_path.display())
}

impl LocalStateManager {
    pub fn new() -> Result<Self> {
        let state_file_path = Self::get_state_file_path()?;
        let state = Self::load_state(&state_file_path)?;

        let mut mgr = Self {
            state_file_path,
            state,
        };

        if mgr.requires_state_migration() {
            // Migration is a mutation too: another agent may register a
            // workspace while this process starts. Reload and migrate while
            // holding the same transaction lock used by normal CRUD so an old
            // snapshot cannot overwrite that concurrent update. Fully current
            // state remains a lock-free read path.
            let lock_path = lock_file_path(&mgr.state_file_path);
            let _lock = acquire_file_lock(&lock_path)?;
            mgr.refresh_state()?;

            // One-time migration: older versions keyed state by the worktree
            // directory, so the same project fragmented across worktrees.
            let mut migrated = mgr.migrate_worktree_project_keys();
            migrated |= mgr.migrate_workspace_identities();
            if migrated {
                if let Err(e) = mgr.save_state_unlocked() {
                    log::warn!("Failed to persist local state migration: {}", e);
                }
            }
        }

        Ok(mgr)
    }

    fn requires_state_migration(&self) -> bool {
        if self.state.schema_version < CURRENT_STATE_SCHEMA_VERSION
            || self.state.projects.values().any(|project| {
                project.workspaces.as_ref().is_some_and(|workspaces| {
                    workspaces.iter().any(|workspace| {
                        workspace.service_key.is_empty() || !workspace.raw_identity_verified
                    })
                })
            })
        {
            return true;
        }

        self.state.projects.keys().any(|key| {
            let dir = Path::new(key);
            dir.join(".git").is_file()
                && crate::vcs::resolve_project_root(dir)
                    .to_string_lossy()
                    .as_ref()
                    != key
        })
    }

    /// Merge per-worktree project-state silos into their canonical main-repo
    /// key. Returns true if anything changed.
    fn migrate_worktree_project_keys(&mut self) -> bool {
        // Find keys that are git worktrees (a `.git` *file*, not dir) whose
        // canonical root differs from the key itself.
        let mut moves: Vec<(String, String)> = Vec::new();
        for key in self.state.projects.keys() {
            let dir = Path::new(key);
            if dir.join(".git").is_file() {
                let root = crate::vcs::resolve_project_root(dir)
                    .to_string_lossy()
                    .to_string();
                if root != *key {
                    moves.push((key.clone(), root));
                }
            }
        }
        if moves.is_empty() {
            return false;
        }

        for (from, to) in moves {
            let Some(src) = self.state.projects.remove(&from) else {
                continue;
            };
            match self.state.projects.get_mut(&to) {
                // Destination (main repo) wins on conflicts; the source only
                // contributes entries the main repo doesn't already have.
                Some(dst) => merge_project_state(dst, src),
                None => {
                    self.state.projects.insert(to, src);
                }
            }
        }
        log::info!("Migrated per-worktree devflow state into canonical project keys");
        true
    }

    /// Upgrade and reconcile the old lossy workspace registry to raw VCS names
    /// plus stable service keys.
    ///
    /// Reconciliation deliberately runs even after the schema marker has been
    /// upgraded. A project may be temporarily unavailable during the first
    /// migration (detached disk, deleted checkout, missing VCS executable), and
    /// a later run with live worktrees is the only authoritative opportunity to
    /// recover its raw names. Ambiguous legacy entries are retained rather than
    /// guessed.
    fn migrate_workspace_identities(&mut self) -> bool {
        if self.state.schema_version >= CURRENT_STATE_SCHEMA_VERSION
            && self.state.projects.values().all(|project| {
                project.workspaces.as_ref().is_none_or(|workspaces| {
                    workspaces.iter().all(|workspace| {
                        !workspace.service_key.is_empty() && workspace.raw_identity_verified
                    })
                })
            })
        {
            return false;
        }

        let mut changed = false;

        for (project_path, project) in &mut self.state.projects {
            let Some(workspaces) = project.workspaces.as_mut() else {
                continue;
            };
            if workspaces.iter().all(|workspace| {
                !workspace.service_key.is_empty() && workspace.raw_identity_verified
            }) {
                continue;
            }

            let live_worktrees = crate::vcs::detect_vcs_provider(Path::new(project_path))
                .ok()
                .and_then(|repo| repo.list_worktrees().ok())
                .unwrap_or_default();
            let live_names = live_worktrees
                .iter()
                .filter_map(|worktree| worktree.workspace.as_ref())
                .cloned()
                .collect::<std::collections::HashSet<_>>();

            let mut raw_by_legacy_key: HashMap<String, Vec<String>> = HashMap::new();
            for raw_name in live_worktrees
                .iter()
                .filter_map(|worktree| worktree.workspace.as_ref())
            {
                raw_by_legacy_key
                    .entry(crate::config::legacy_normalize_workspace_name(raw_name))
                    .or_default()
                    .push(raw_name.clone());
            }

            // Resolve names first so parent provenance can use the same map.
            let mut migrated_names = HashMap::new();
            for workspace in workspaces.iter() {
                if workspace.raw_identity_verified {
                    continue;
                }
                // `raw_by_legacy_key` is keyed by the legacy-normalized form;
                // a stored name that is already the raw live name (old
                // releases persisted raw names too) must be normalized before
                // the ambiguity lookup or it never verifies.
                if live_names.contains(&workspace.name)
                    && raw_by_legacy_key
                        .get(&crate::config::legacy_normalize_workspace_name(
                            &workspace.name,
                        ))
                        .is_some_and(|candidates| candidates.len() == 1)
                {
                    migrated_names.insert(workspace.name.clone(), workspace.name.clone());
                } else if let Some(candidates) = raw_by_legacy_key.get(&workspace.name) {
                    if candidates.len() == 1 {
                        migrated_names.insert(workspace.name.clone(), candidates[0].clone());
                    }
                }
            }

            for workspace in workspaces {
                let legacy_name = workspace.name.clone();
                // An entry that already carries a verified identity and a
                // persisted key was fully resolved by an earlier pass (its
                // key may be an intentionally adopted legacy key). Re-runs —
                // guaranteed while ANY sibling stays unverified, because
                // `requires_state_migration` re-triggers on every manager
                // construction — must never re-derive it: after the pass-1
                // rename `legacy_name` is the raw name, the adoption
                // condition below can no longer match, and the entry would
                // silently fall through to the hashed canonical key,
                // orphaning the resources the adoption preserved.
                let already_resolved =
                    workspace.raw_identity_verified && !workspace.service_key.is_empty();
                if let Some(raw_name) = migrated_names.get(&legacy_name) {
                    if workspace.name != *raw_name {
                        workspace.name = raw_name.clone();
                        changed = true;
                    }
                    if !workspace.raw_identity_verified {
                        workspace.raw_identity_verified = true;
                        changed = true;
                    }
                }
                let raw_parent = workspace.parent.as_ref().and_then(|parent| {
                    migrated_names.get(parent).cloned().or_else(|| {
                        raw_by_legacy_key
                            .get(parent)
                            .filter(|candidates| candidates.len() == 1)
                            .map(|candidates| candidates[0].clone())
                    })
                });
                if let Some(raw_parent) = raw_parent {
                    if workspace.parent.as_ref() != Some(&raw_parent) {
                        workspace.parent = Some(raw_parent);
                        changed = true;
                    }
                }
                // State written before collision-resistant keys existed used
                // the lossy normalized workspace name directly for database,
                // container, and process namespaces. Retain that key only
                // when the live VCS inventory proves the mapping is unique.
                // This adopts existing resources without renaming them. An
                // ambiguous entry deliberately remains unverified and is
                // rejected by `resolve_workspace_service_key_by_dir`.
                if !already_resolved {
                    let canonical_key = crate::config::workspace_service_key(&workspace.name);
                    let raw_legacy_key =
                        crate::config::legacy_normalize_workspace_name(&workspace.name);
                    // Adopt the legacy key both when the stored name was the
                    // normalized form and when it was already the raw name:
                    // pre-key releases always created database/container/
                    // process namespaces under the lossy normalized name,
                    // regardless of which form the registry persisted.
                    let service_key = if workspace.raw_identity_verified
                        && (legacy_name == raw_legacy_key || legacy_name == workspace.name)
                        && raw_by_legacy_key
                            .get(&raw_legacy_key)
                            .is_some_and(|candidates| candidates.len() == 1)
                    {
                        raw_legacy_key
                    } else if !workspace.raw_identity_verified {
                        // Preserve the only identity old state can authoritatively
                        // provide. Do not invent a hash while raw recovery is
                        // pending: that would make old resources disappear.
                        legacy_name
                    } else {
                        canonical_key
                    };
                    if workspace.service_key != service_key {
                        workspace.service_key = service_key;
                        changed = true;
                    }
                }

                if let Some(live) = live_worktrees
                    .iter()
                    .find(|worktree| worktree.workspace.as_deref() == Some(&workspace.name))
                {
                    let live_path = live.path.display().to_string();
                    if workspace.worktree_path.as_deref() != Some(live_path.as_str()) {
                        workspace.worktree_path = Some(live_path);
                        changed = true;
                    }
                }
            }
        }

        if self.state.schema_version < CURRENT_STATE_SCHEMA_VERSION {
            self.state.schema_version = CURRENT_STATE_SCHEMA_VERSION;
            changed = true;
        }
        if changed {
            log::info!("Reconciled devflow workspace state with raw VCS identities");
        }
        changed
    }

    fn refresh_state(&mut self) -> Result<()> {
        self.state = Self::load_state(&self.state_file_path)?;
        Ok(())
    }

    /// Apply one read-modify-write transaction to local state.
    ///
    /// The previous implementation refreshed before acquiring the write lock,
    /// so concurrent agents could both mutate the same snapshot and the later
    /// serialized write silently discarded the earlier update. Keep the lock
    /// across refresh, mutation, and atomic rename instead.
    fn mutate_state<T>(
        &mut self,
        mutation: impl FnOnce(&mut LocalState) -> Result<T>,
    ) -> Result<T> {
        let lock_path = lock_file_path(&self.state_file_path);
        let _lock = acquire_file_lock(&lock_path)?;
        self.refresh_state()?;
        let previous = self.state.clone();
        let result = match mutation(&mut self.state) {
            Ok(result) => result,
            Err(error) => {
                self.state = previous;
                return Err(error);
            }
        };
        if let Err(error) = self.save_state_unlocked() {
            self.state = previous;
            return Err(error);
        }
        Ok(result)
    }

    pub fn get_services(&self, project_path: &Path) -> Option<Vec<NamedServiceConfig>> {
        let project_key = self.get_project_key(project_path)?;
        self.state
            .projects
            .get(&project_key)
            .and_then(|project| project.services.clone())
            .map(|mut services| {
                normalize_service_defaults(&mut services);
                services
            })
    }

    pub fn add_service(
        &mut self,
        project_path: &Path,
        service: NamedServiceConfig,
        force: bool,
    ) -> Result<()> {
        let project_key = self.get_project_key(project_path).ok_or_else(|| {
            anyhow::anyhow!(
                "Failed to get project key for path: {}",
                project_path.display()
            )
        })?;

        self.mutate_state(move |state| {
            let existing = state.projects.get(&project_key);
            let existing_branches = existing.and_then(|p| p.workspaces.clone());
            let mut services = existing
                .and_then(|p| p.services.clone())
                .unwrap_or_default();

            if let Some(pos) = services.iter().position(|b| b.name == service.name) {
                if force {
                    services[pos] = service;
                } else {
                    anyhow::bail!(
                        "Service '{}' already exists. Use --force to overwrite.",
                        services[pos].name
                    );
                }
            } else {
                let mut service = service;
                if services.is_empty() {
                    service.default = true;
                }
                services.push(service);
            }

            normalize_service_defaults(&mut services);
            state.projects.insert(
                project_key,
                ProjectState {
                    last_updated: chrono::Utc::now(),
                    services: Some(services),
                    workspaces: existing_branches,
                },
            );
            Ok(())
        })
    }

    pub fn remove_service(&mut self, project_path: &Path, name: &str) -> Result<()> {
        let project_key = self.get_project_key(project_path).ok_or_else(|| {
            anyhow::anyhow!(
                "Failed to get project key for path: {}",
                project_path.display()
            )
        })?;

        self.mutate_state(|state| {
            if let Some(project) = state.projects.get_mut(&project_key) {
                if let Some(ref mut services) = project.services {
                    services.retain(|b| b.name != name);
                }
                project.last_updated = chrono::Utc::now();
            }
            Ok(())
        })
    }

    /// Remove an entire project from the local state (workspace registry + services).
    pub fn remove_project(&mut self, project_path: &Path) -> Result<()> {
        let project_key = self.get_project_key(project_path).ok_or_else(|| {
            anyhow::anyhow!(
                "Failed to get project key for path: {}",
                project_path.display()
            )
        })?;

        self.mutate_state(|state| {
            state.projects.remove(&project_key);
            Ok(())
        })
    }

    /// Remove a project by its raw key (canonical path string).
    ///
    /// Unlike [`remove_project`] this does **not** call `canonicalize()`, so it
    /// works even when the directory no longer exists on disk — which is exactly
    /// the situation during orphan cleanup.
    pub fn remove_project_by_key(&mut self, project_key: &str) -> Result<()> {
        self.mutate_state(|state| {
            state.projects.remove(project_key);
            Ok(())
        })
    }

    /// Return a snapshot of **all** projects in the local state, keyed by their
    /// canonical path.  Used by orphan detection to iterate over every known
    /// project.
    pub fn list_all_projects(&self) -> HashMap<String, ProjectState> {
        self.state.projects.clone()
    }

    // ── Workspace registry CRUD ────────────────────────────────────────

    /// Get all registered devflow workspaces for a project.
    pub fn get_workspaces(&self, project_path: &Path) -> Vec<DevflowWorkspace> {
        self.get_project_key(project_path)
            .and_then(|key| self.state.projects.get(&key))
            .and_then(|p| p.workspaces.clone())
            .unwrap_or_default()
    }

    /// Look up a single registered workspace by name.
    pub fn get_workspace(&self, project_path: &Path, name: &str) -> Option<DevflowWorkspace> {
        self.get_workspaces(project_path)
            .into_iter()
            .find(|b| b.name == name)
    }

    /// Register (upsert) a devflow workspace in the registry.
    /// If a workspace with the same name exists, it is updated.
    pub fn register_workspace(
        &mut self,
        project_path: &Path,
        mut workspace: DevflowWorkspace,
    ) -> Result<()> {
        let project_key = self.get_project_key(project_path).ok_or_else(|| {
            anyhow::anyhow!(
                "Failed to get project key for path: {}",
                project_path.display()
            )
        })?;

        self.mutate_state(move |state| {
            let project = state
            .projects
            .entry(project_key)
            .or_insert_with(|| ProjectState {
                last_updated: chrono::Utc::now(),
                services: None,
                workspaces: None,
            });

        let workspaces = project.workspaces.get_or_insert_with(Vec::new);

        if let Some(pos) = workspaces.iter().position(|b| b.name == workspace.name) {
            if !workspaces[pos].raw_identity_verified && workspace.raw_identity_verified {
                anyhow::bail!(
                    "workspace '{}' still has unresolved legacy identity; refusing to mark it verified through a generic registry update",
                    workspace.name
                );
            }
            // Known creation provenance is immutable. An auto-adopted
            // workspace has no provenance yet, however, so an explicit
            // `--from`/link operation may repair it exactly once.
            workspace.parent = workspaces[pos].parent.clone().or(workspace.parent);
            validate_workspace_parent(workspaces, &workspace.name, workspace.parent.as_deref())?;
            // A persisted key can be an intentionally adopted legacy key.
            // Never replace it merely because the raw identity is re-linked.
            if !workspaces[pos].service_key.is_empty() {
                workspace.service_key = workspaces[pos].service_key.clone();
            } else if workspace.service_key.is_empty() {
                workspace.service_key = crate::config::workspace_service_key(&workspace.name);
            }
            if let Some(owner) = workspaces.iter().enumerate().find_map(|(index, existing)| {
                (index != pos && existing.service_key == workspace.service_key).then_some(existing)
            }) {
                anyhow::bail!(
                    "service key '{}' for raw workspace '{}' is already owned by raw workspace '{}'; refusing to conflate their resources",
                    workspace.service_key,
                    workspace.name,
                    owner.name
                );
            }
            workspaces[pos] = workspace;
        } else {
            validate_workspace_parent(workspaces, &workspace.name, workspace.parent.as_deref())?;
            if workspace.service_key.is_empty() {
                workspace.service_key = crate::config::workspace_service_key(&workspace.name);
            }
            let legacy_key = crate::config::legacy_normalize_workspace_name(&workspace.name);
            if let Some(unresolved) = workspaces.iter().find(|existing| {
                !existing.raw_identity_verified
                    && (existing.name == legacy_key || existing.service_key == legacy_key)
            }) {
                anyhow::bail!(
                    "legacy service key '{}' has unresolved ownership in registry entry '{}'; refusing to register raw workspace '{}' with a parallel namespace",
                    legacy_key,
                    unresolved.name,
                    workspace.name
                );
            }
            if let Some(owner) = workspaces.iter().find(|existing| {
                existing.service_key == workspace.service_key && existing.name != workspace.name
            }) {
                anyhow::bail!(
                    "service key '{}' for raw workspace '{}' is already owned by raw workspace '{}'; refusing to conflate their resources",
                    workspace.service_key,
                    workspace.name,
                    owner.name
                );
            }
            workspaces.push(workspace);
        }

            project.last_updated = chrono::Utc::now();
            Ok(())
        })
    }

    // ── Convenience methods that accept a project directory ────────
    //
    // The standard CRUD methods above expect the `.devflow.yml` config file
    // path (because `get_project_key` strips the last component).  These
    // `_by_dir` variants accept the **project directory** and append
    // `.devflow.yml` internally, eliminating a common source of bugs.

    /// Get all registered devflow workspaces for a project directory.
    pub fn get_workspaces_by_dir(&self, project_dir: &Path) -> Vec<DevflowWorkspace> {
        self.get_workspaces(&project_dir.join(".devflow.yml"))
    }

    /// Look up a single registered workspace by name (project directory variant).
    pub fn get_workspace_by_dir(&self, project_dir: &Path, name: &str) -> Option<DevflowWorkspace> {
        self.get_workspace(&project_dir.join(".devflow.yml"), name)
    }

    /// Resolve the effective backend/process key for a raw VCS workspace.
    ///
    /// New workspaces use [`crate::config::workspace_service_key`]. A workspace
    /// migrated unambiguously from legacy state keeps its persisted legacy key
    /// so existing databases, containers, and process records remain visible.
    /// Ambiguous legacy identities and key collisions fail closed before a
    /// caller can create a parallel namespace or operate on another workspace.
    /// Resolve like [`Self::resolve_workspace_service_key_by_dir`], but also
    /// accept an ALREADY-RESOLVED service key: when the input is not a
    /// registered raw workspace name yet exactly matches the persisted
    /// `service_key` of exactly one verified workspace, it resolves to
    /// itself. Frontends that echo provider-side identities back into
    /// commands (GUI service-workspace rows, process records) need this —
    /// re-deriving a key from a key trips the owner guard below.
    pub fn resolve_workspace_or_key_by_dir(
        &self,
        project_dir: &Path,
        workspace: &str,
    ) -> Result<String> {
        let registered = self.get_workspaces_by_dir(project_dir);
        if !registered
            .iter()
            .any(|candidate| candidate.name == workspace)
        {
            let owners = registered
                .iter()
                .filter(|candidate| {
                    candidate.raw_identity_verified && candidate.service_key == workspace
                })
                .collect::<Vec<_>>();
            if owners.len() == 1 {
                return Ok(workspace.to_string());
            }
            if owners.len() > 1 {
                anyhow::bail!(
                    "service key '{}' has multiple raw owners; refusing an ambiguous operation",
                    workspace
                );
            }
        }
        self.resolve_workspace_service_key_by_dir(project_dir, workspace)
    }

    pub fn resolve_workspace_service_key_by_dir(
        &self,
        project_dir: &Path,
        workspace_name: &str,
    ) -> Result<String> {
        let workspaces = self.get_workspaces_by_dir(project_dir);
        let canonical_key = crate::config::workspace_service_key(workspace_name);
        let legacy_key = crate::config::legacy_normalize_workspace_name(workspace_name);

        let unresolved = workspaces
            .iter()
            .filter(|workspace| {
                !workspace.raw_identity_verified
                    && (workspace.name == legacy_key
                        || workspace.service_key == legacy_key
                        || workspace.service_key == canonical_key)
            })
            .collect::<Vec<_>>();
        if !unresolved.is_empty() {
            let mut candidates = crate::vcs::detect_vcs_provider(project_dir)
                .ok()
                .and_then(|repo| repo.list_worktrees().ok())
                .unwrap_or_default()
                .into_iter()
                .filter_map(|worktree| worktree.workspace)
                .filter(|raw| crate::config::legacy_normalize_workspace_name(raw) == legacy_key)
                .collect::<Vec<_>>();
            candidates.sort();
            candidates.dedup();
            let candidate_text = if candidates.is_empty() {
                "no live candidates are currently available".to_string()
            } else {
                format!("live candidates: {}", candidates.join(", "))
            };
            anyhow::bail!(
                "legacy workspace key '{}' has unresolved ownership ({}). Refusing to use or create namespace '{}' for raw workspace '{}'. Inspect `devflow --json list` warnings, then rename/remove the conflicting VCS workspace or edit the workspace registry (~/.config/devflow/local_state.yml) after identifying which existing resources own the legacy key.",
                legacy_key,
                candidate_text,
                canonical_key,
                workspace_name
            );
        }

        let effective_key = workspaces
            .iter()
            .find(|workspace| workspace.raw_identity_verified && workspace.name == workspace_name)
            .map(|workspace| workspace.service_key.as_str())
            .filter(|key| !key.is_empty())
            .unwrap_or(&canonical_key)
            .to_string();

        if let Some(owner) = workspaces.iter().find(|workspace| {
            workspace.raw_identity_verified
                && workspace.name != workspace_name
                && workspace.service_key == effective_key
        }) {
            anyhow::bail!(
                "service key '{}' for raw workspace '{}' is already owned by raw workspace '{}'. Refusing to conflate their service and process namespaces; rename one workspace before continuing.",
                effective_key,
                workspace_name,
                owner.name
            );
        }

        Ok(effective_key)
    }

    /// Register (upsert) a devflow workspace (project directory variant).
    pub fn register_workspace_by_dir(
        &mut self,
        project_dir: &Path,
        workspace: DevflowWorkspace,
    ) -> Result<()> {
        self.register_workspace(&project_dir.join(".devflow.yml"), workspace)
    }

    /// Remove a workspace from the registry by name (project directory variant).
    pub fn unregister_workspace_by_dir(&mut self, project_dir: &Path, name: &str) -> Result<()> {
        self.unregister_workspace(&project_dir.join(".devflow.yml"), name)
    }

    /// Get project workspaces, initializing the default workspace when empty.
    ///
    /// This is the common workspace-loading path used by CLI/TUI/GUI so all
    /// surfaces share the same bootstrap behavior.
    pub fn get_or_init_workspaces_by_dir(
        &mut self,
        project_dir: &Path,
        main_workspace: &str,
    ) -> Result<Vec<DevflowWorkspace>> {
        self.ensure_default_workspace(project_dir, main_workspace)?;
        Ok(self.get_workspaces_by_dir(project_dir))
    }

    /// Ensure a default devflow workspace exists for this project.
    ///
    /// Registers `main_workspace` with `created_at = now` and no parent when
    /// it is missing. This also repairs older partial registries that contain
    /// child workspaces but no default node.
    pub fn ensure_default_workspace(
        &mut self,
        project_dir: &Path,
        main_workspace: &str,
    ) -> Result<()> {
        let config_path = project_dir.join(".devflow.yml");
        let existing = self.get_workspaces(&config_path);
        if existing
            .iter()
            .any(|workspace| workspace.name == main_workspace)
        {
            return Ok(());
        }

        let workspace = DevflowWorkspace {
            name: main_workspace.to_string(),
            service_key: crate::config::workspace_service_key(main_workspace),
            raw_identity_verified: true,
            parent: None,
            worktree_path: None,
            created_at: chrono::Utc::now(),
            executed_command: None,
            execution_status: None,
            executed_at: None,
        };

        self.register_workspace(&config_path, workspace)
    }

    /// Remove a workspace from the registry by name.
    pub fn unregister_workspace(&mut self, project_path: &Path, name: &str) -> Result<()> {
        let project_key = self.get_project_key(project_path).ok_or_else(|| {
            anyhow::anyhow!(
                "Failed to get project key for path: {}",
                project_path.display()
            )
        })?;

        self.mutate_state(|state| {
            if let Some(project) = state.projects.get_mut(&project_key) {
                if let Some(ref mut workspaces) = project.workspaces {
                    workspaces.retain(|b| b.name != name);
                }
                project.last_updated = chrono::Utc::now();
            }
            Ok(())
        })
    }

    // ── Workspace relationship queries ──────────────────────────────

    /// Get child workspaces (workspaces whose parent is `workspace`).
    pub fn get_children(&self, project_dir: &Path, workspace: &str) -> Vec<DevflowWorkspace> {
        self.get_workspaces_by_dir(project_dir)
            .into_iter()
            .filter(|w| w.parent.as_deref() == Some(workspace))
            .collect()
    }

    /// Get sibling workspaces (workspaces with the same parent).
    pub fn get_siblings(&self, project_dir: &Path, workspace: &str) -> Vec<DevflowWorkspace> {
        let workspaces = self.get_workspaces_by_dir(project_dir);
        let parent = workspaces
            .iter()
            .find(|w| w.name == workspace)
            .and_then(|w| w.parent.clone());

        workspaces
            .into_iter()
            .filter(|w| w.parent == parent && w.name != workspace)
            .collect()
    }

    fn get_project_key(&self, project_path: &Path) -> Option<String> {
        // The project key is the canonical MAIN-repo root of the directory
        // containing `.devflow.yml`. Resolving worktrees to their main repo
        // (instead of using the worktree dir directly) keeps state unified
        // across the main repo and all of its worktrees.
        project_path.parent().map(|dir| {
            crate::vcs::resolve_project_root(dir)
                .to_string_lossy()
                .to_string()
        })
    }

    /// Public accessor for the project key, used by `devflow destroy` to clear hook approvals.
    pub fn get_project_key_for(&self, project_path: &Path) -> Option<String> {
        self.get_project_key(project_path)
    }

    fn get_state_file_path() -> Result<PathBuf> {
        let config_dir = crate::paths::devflow_config_dir()?;

        // Ensure the config directory exists
        fs::create_dir_all(&config_dir).with_context(|| {
            format!(
                "Failed to create config directory: {}",
                config_dir.display()
            )
        })?;

        Ok(config_dir.join("local_state.yml"))
    }

    fn load_state(state_file_path: &Path) -> Result<LocalState> {
        if !state_file_path.exists() {
            log::debug!("Local state file does not exist, creating new state");
            return Ok(LocalState::default());
        }

        let content = fs::read_to_string(state_file_path).with_context(|| {
            format!(
                "Failed to read local state file: {}",
                state_file_path.display()
            )
        })?;

        let state: LocalState = serde_yaml_ng::from_str(&content).with_context(|| {
            format!(
                "Failed to parse local state file: {}",
                state_file_path.display()
            )
        })?;

        log::debug!("Loaded local state with {} projects", state.projects.len());
        Ok(state)
    }

    #[cfg(test)]
    fn save_state(&self) -> Result<()> {
        let lock_path = lock_file_path(&self.state_file_path);
        let _lock = acquire_file_lock(&lock_path)?;
        self.save_state_unlocked()
    }

    /// Persist the current snapshot while the caller holds the state lock.
    /// Keeping lock acquisition out of this helper prevents mutation
    /// transactions from recursively locking themselves.
    fn save_state_unlocked(&self) -> Result<()> {
        let content = serde_yaml_ng::to_string(&self.state)
            .context("Failed to serialize local state to YAML")?;

        let tmp_path = PathBuf::from(format!(
            "{}.tmp.{}",
            self.state_file_path.display(),
            std::process::id()
        ));

        fs::write(&tmp_path, content).with_context(|| {
            format!(
                "Failed to write temporary local state file: {}",
                tmp_path.display()
            )
        })?;

        fs::rename(&tmp_path, &self.state_file_path).with_context(|| {
            format!(
                "Failed to write local state file: {}",
                self.state_file_path.display()
            )
        })?;

        log::debug!("Saved local state to: {}", self.state_file_path.display());
        Ok(())
    }
}

/// Reject lineage that would make a workspace its own ancestor. Keeping this
/// invariant in the registry protects every frontend and lifecycle caller,
/// including adoption/link paths that repair a previously unknown parent.
fn validate_workspace_parent(
    workspaces: &[DevflowWorkspace],
    workspace_name: &str,
    parent: Option<&str>,
) -> Result<()> {
    let mut cursor = parent;
    let mut visited = std::collections::HashSet::new();
    while let Some(name) = cursor {
        if name == workspace_name {
            anyhow::bail!(
                "workspace '{}' cannot use '{}' as its parent because that would create a lineage cycle",
                workspace_name,
                parent.unwrap_or(name)
            );
        }
        if !visited.insert(name) {
            anyhow::bail!(
                "workspace '{}' cannot use parent '{}' because the existing parent chain already contains a cycle at '{}'",
                workspace_name,
                parent.unwrap_or(name),
                name
            );
        }
        cursor = workspaces
            .iter()
            .find(|workspace| workspace.name == name)
            .and_then(|workspace| workspace.parent.as_deref());
    }
    Ok(())
}

fn normalize_service_defaults(services: &mut [NamedServiceConfig]) {
    let mut seen_default = false;
    for service in services {
        if service.default {
            if seen_default {
                service.default = false;
            } else {
                seen_default = true;
            }
        }
    }
}

/// Merge `src` project state into `dst`, preferring `dst` on conflicts.
///
/// Used by the worktree-key migration: the main repo's state is authoritative;
/// the worktree silo only contributes workspaces/services the main repo lacks.
fn merge_project_state(dst: &mut ProjectState, src: ProjectState) {
    // Workspaces: union by name, keeping the destination's entry on conflict.
    if let Some(src_ws) = src.workspaces {
        let dst_ws = dst.workspaces.get_or_insert_with(Vec::new);
        for ws in src_ws {
            if !dst_ws.iter().any(|b| b.name == ws.name) {
                dst_ws.push(ws);
            }
        }
    }
    // Services: fill in only if the destination has none.
    if dst.services.is_none() {
        dst.services = src.services;
    }
    dst.last_updated = dst.last_updated.max(src.last_updated);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::{GitRepository, VcsProvider};
    use tempfile::TempDir;

    fn service(name: &str, default: bool) -> NamedServiceConfig {
        NamedServiceConfig {
            name: name.to_string(),
            provider_type: "local".to_string(),
            service_type: "postgres".to_string(),
            auto_workspace: true,
            default,
            local: None,
            shared: None,
            neon: None,
            dblab: None,
            xata: None,
            clickhouse: None,
            mysql: None,
            generic: None,
            plugin: None,
            docker: None,
        }
    }

    fn ws(name: &str) -> DevflowWorkspace {
        DevflowWorkspace {
            name: name.to_string(),
            service_key: crate::config::workspace_service_key(name),
            raw_identity_verified: true,
            parent: None,
            worktree_path: None,
            created_at: chrono::Utc::now(),
            executed_command: None,
            execution_status: None,
            executed_at: None,
        }
    }

    #[test]
    fn workspace_parent_validation_rejects_self_and_ancestor_cycles() {
        let self_parent = validate_workspace_parent(&[], "feature/a", Some("feature/a"))
            .unwrap_err()
            .to_string();
        assert!(self_parent.contains("lineage cycle"));

        let mut parent = ws("feature/a");
        parent.parent = Some("main".to_string());
        let mut child = ws("feature/b");
        child.parent = Some("feature/a".to_string());
        let cycle = validate_workspace_parent(&[parent, child], "feature/a", Some("feature/b"))
            .unwrap_err()
            .to_string();
        assert!(cycle.contains("lineage cycle"));
    }

    #[test]
    fn test_merge_project_state_dst_wins_src_fills_gaps() {
        // Destination (main repo) has workspace "main"; source (worktree silo)
        // has "main" (duplicate) and "feature" (unique).
        let mut dst = ProjectState {
            last_updated: chrono::Utc::now() - chrono::Duration::hours(1),
            services: None,
            workspaces: Some(vec![ws("main")]),
        };
        let src = ProjectState {
            last_updated: chrono::Utc::now(),
            services: None,
            workspaces: Some(vec![ws("main"), ws("feature")]),
        };

        merge_project_state(&mut dst, src);

        let names: Vec<String> = dst
            .workspaces
            .as_ref()
            .unwrap()
            .iter()
            .map(|w| w.name.clone())
            .collect();
        // "main" not duplicated; "feature" merged in.
        assert_eq!(names, vec!["main".to_string(), "feature".to_string()]);
    }

    #[test]
    fn test_normalize_service_defaults_keeps_first_default_only() {
        let mut services = vec![service("db", true), service("cache", true)];

        normalize_service_defaults(&mut services);

        assert!(services[0].default);
        assert!(!services[1].default);
    }

    #[test]
    fn test_project_key_generation() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join(".devflow.yml");

        let manager = LocalStateManager {
            state_file_path: temp_dir.path().join("state.yml"),
            state: LocalState::default(),
        };
        let project_key = manager.get_project_key(&config_path);

        assert!(project_key.is_some());
        assert!(project_key
            .unwrap()
            .contains(temp_dir.path().to_str().unwrap()));
    }

    #[test]
    fn test_workspace_identity_migration_recovers_raw_live_name() {
        let project = TempDir::new().unwrap();
        let repo = GitRepository::init(project.path()).unwrap();
        repo.ensure_initial_commit().unwrap();
        repo.create_workspace("feature/auth", Some("main")).unwrap();
        let worktree = project.path().join("auth-worktree");
        repo.create_worktree("feature/auth", &worktree).unwrap();

        let project_key = crate::vcs::resolve_project_root(project.path())
            .to_string_lossy()
            .to_string();
        let old_workspace = DevflowWorkspace {
            name: crate::config::legacy_normalize_workspace_name("feature/auth"),
            service_key: String::new(),
            raw_identity_verified: false,
            parent: Some("main".to_string()),
            worktree_path: None,
            created_at: chrono::Utc::now(),
            executed_command: None,
            execution_status: None,
            executed_at: None,
        };
        let mut manager = LocalStateManager {
            state_file_path: project.path().join("state.yml"),
            state: LocalState {
                schema_version: 1,
                projects: HashMap::from([(
                    project_key,
                    ProjectState {
                        last_updated: chrono::Utc::now(),
                        services: None,
                        workspaces: Some(vec![old_workspace]),
                    },
                )]),
            },
        };

        assert!(manager.migrate_workspace_identities());
        let migrated = manager
            .state
            .projects
            .values()
            .next()
            .unwrap()
            .workspaces
            .as_ref()
            .unwrap()
            .first()
            .unwrap();
        assert_eq!(migrated.name, "feature/auth");
        assert_eq!(
            migrated.service_key,
            crate::config::legacy_normalize_workspace_name("feature/auth")
        );
        assert_eq!(
            migrated.worktree_path.as_deref(),
            Some(worktree.canonicalize().unwrap().to_string_lossy().as_ref())
        );
    }

    #[test]
    fn test_workspace_identity_migration_verifies_raw_stored_name() {
        // Some pre-key releases persisted the RAW branch name in the registry
        // while still creating service namespaces under the lossy normalized
        // name. Such an entry must verify against the live worktree (the
        // ambiguity map is keyed by the normalized form) and adopt the legacy
        // key so those namespaces stay attached.
        let project = TempDir::new().unwrap();
        let repo = GitRepository::init(project.path()).unwrap();
        repo.ensure_initial_commit().unwrap();
        repo.create_workspace("feature/auth", Some("main")).unwrap();
        let worktree = project.path().join("auth-worktree");
        repo.create_worktree("feature/auth", &worktree).unwrap();

        let project_key = crate::vcs::resolve_project_root(project.path())
            .to_string_lossy()
            .to_string();
        let old_workspace = DevflowWorkspace {
            name: "feature/auth".to_string(),
            service_key: String::new(),
            raw_identity_verified: false,
            parent: Some("main".to_string()),
            worktree_path: None,
            created_at: chrono::Utc::now(),
            executed_command: None,
            execution_status: None,
            executed_at: None,
        };
        let mut manager = LocalStateManager {
            state_file_path: project.path().join("state.yml"),
            state: LocalState {
                schema_version: 1,
                projects: HashMap::from([(
                    project_key,
                    ProjectState {
                        last_updated: chrono::Utc::now(),
                        services: None,
                        workspaces: Some(vec![old_workspace]),
                    },
                )]),
            },
        };

        assert!(manager.migrate_workspace_identities());
        let migrated = manager
            .state
            .projects
            .values()
            .next()
            .unwrap()
            .workspaces
            .as_ref()
            .unwrap()
            .first()
            .unwrap();
        assert_eq!(migrated.name, "feature/auth");
        assert!(migrated.raw_identity_verified);
        assert_eq!(
            migrated.service_key,
            crate::config::legacy_normalize_workspace_name("feature/auth")
        );
    }

    #[test]
    fn test_migration_rerun_preserves_adopted_legacy_service_key() {
        // Re-runs are guaranteed while ANY entry stays unverified (an
        // unresolved entry keeps `requires_state_migration` true on every
        // manager construction). A verified workspace whose service_key was
        // adopted from legacy state must keep that key on the second pass —
        // rewriting it to the canonical hashed key would orphan the
        // databases/containers the adoption deliberately preserved.
        let project = TempDir::new().unwrap();
        let repo = GitRepository::init(project.path()).unwrap();
        repo.ensure_initial_commit().unwrap();
        repo.create_workspace("feature/auth", Some("main")).unwrap();
        let worktree = project.path().join("auth-worktree");
        repo.create_worktree("feature/auth", &worktree).unwrap();

        let project_key = crate::vcs::resolve_project_root(project.path())
            .to_string_lossy()
            .to_string();
        let adoptable = DevflowWorkspace {
            name: crate::config::legacy_normalize_workspace_name("feature/auth"),
            service_key: String::new(),
            raw_identity_verified: false,
            parent: Some("main".to_string()),
            worktree_path: None,
            created_at: chrono::Utc::now(),
            executed_command: None,
            execution_status: None,
            executed_at: None,
        };
        // A legacy entry with no matching live worktree stays unverified
        // forever and keeps re-triggering migration.
        let unresolved = DevflowWorkspace {
            name: "stale_orphan".to_string(),
            service_key: String::new(),
            raw_identity_verified: false,
            parent: None,
            worktree_path: None,
            created_at: chrono::Utc::now(),
            executed_command: None,
            execution_status: None,
            executed_at: None,
        };
        let mut manager = LocalStateManager {
            state_file_path: project.path().join("state.yml"),
            state: LocalState {
                schema_version: 1,
                projects: HashMap::from([(
                    project_key,
                    ProjectState {
                        last_updated: chrono::Utc::now(),
                        services: None,
                        workspaces: Some(vec![adoptable, unresolved]),
                    },
                )]),
            },
        };

        assert!(manager.migrate_workspace_identities());
        let adopted_key = crate::config::legacy_normalize_workspace_name("feature/auth");
        let key_of = |manager: &LocalStateManager, name: &str| {
            manager
                .state
                .projects
                .values()
                .next()
                .unwrap()
                .workspaces
                .as_ref()
                .unwrap()
                .iter()
                .find(|workspace| workspace.name == name)
                .unwrap()
                .service_key
                .clone()
        };
        assert_eq!(key_of(&manager, "feature/auth"), adopted_key);

        // The unresolved sibling keeps migration re-triggering; the second
        // pass must not touch the adopted key.
        manager.migrate_workspace_identities();
        assert_eq!(key_of(&manager, "feature/auth"), adopted_key);
        assert_ne!(
            key_of(&manager, "feature/auth"),
            crate::config::workspace_service_key("feature/auth")
        );
    }

    #[test]
    fn test_workspace_identity_reconciles_after_project_becomes_available() {
        let container = TempDir::new().unwrap();
        let project_path = container.path().join("temporarily-unavailable");
        let project_key = project_path.to_string_lossy().to_string();
        let old_workspace = DevflowWorkspace {
            name: crate::config::legacy_normalize_workspace_name("feature/auth"),
            service_key: String::new(),
            raw_identity_verified: false,
            parent: Some("main".to_string()),
            worktree_path: None,
            created_at: chrono::Utc::now(),
            executed_command: None,
            execution_status: None,
            executed_at: None,
        };
        let mut manager = LocalStateManager {
            state_file_path: container.path().join("state.yml"),
            state: LocalState {
                schema_version: 1,
                projects: HashMap::from([(
                    project_key,
                    ProjectState {
                        last_updated: chrono::Utc::now(),
                        services: None,
                        workspaces: Some(vec![old_workspace]),
                    },
                )]),
            },
        };

        // The schema can advance while the project is offline, but raw-name
        // recovery must remain retryable on later startups.
        assert!(manager.migrate_workspace_identities());
        assert_eq!(manager.state.schema_version, CURRENT_STATE_SCHEMA_VERSION);
        assert!(
            !manager
                .state
                .projects
                .values()
                .next()
                .unwrap()
                .workspaces
                .as_ref()
                .unwrap()[0]
                .raw_identity_verified
        );

        std::fs::create_dir_all(&project_path).unwrap();
        let repo = GitRepository::init(&project_path).unwrap();
        repo.ensure_initial_commit().unwrap();
        repo.create_workspace("feature/auth", Some("main")).unwrap();
        let worktree = container.path().join("auth-worktree");
        repo.create_worktree("feature/auth", &worktree).unwrap();

        assert!(manager.migrate_workspace_identities());
        let migrated = manager
            .state
            .projects
            .values()
            .next()
            .unwrap()
            .workspaces
            .as_ref()
            .unwrap()
            .first()
            .unwrap();
        assert_eq!(migrated.name, "feature/auth");
        assert!(migrated.raw_identity_verified);
        assert_eq!(
            migrated.service_key,
            crate::config::legacy_normalize_workspace_name("feature/auth")
        );
        assert_eq!(
            migrated.worktree_path.as_deref(),
            Some(worktree.canonicalize().unwrap().to_string_lossy().as_ref())
        );
    }

    #[test]
    fn ambiguous_legacy_identity_fails_before_namespace_duplication() {
        let project = TempDir::new().unwrap();
        let repo = GitRepository::init(project.path()).unwrap();
        repo.ensure_initial_commit().unwrap();
        for (raw, path) in [
            ("feature/auth", project.path().join("slash-worktree")),
            ("feature_auth", project.path().join("underscore-worktree")),
        ] {
            repo.create_workspace(raw, Some("main")).unwrap();
            repo.create_worktree(raw, &path).unwrap();
        }

        let project_key = crate::vcs::resolve_project_root(project.path())
            .to_string_lossy()
            .to_string();
        let legacy_key = crate::config::legacy_normalize_workspace_name("feature/auth");
        let mut manager = LocalStateManager {
            state_file_path: project.path().join("state.yml"),
            state: LocalState {
                schema_version: 1,
                projects: HashMap::from([(
                    project_key,
                    ProjectState {
                        last_updated: chrono::Utc::now(),
                        services: None,
                        workspaces: Some(vec![DevflowWorkspace {
                            name: legacy_key.clone(),
                            service_key: String::new(),
                            raw_identity_verified: false,
                            parent: Some("main".to_string()),
                            worktree_path: None,
                            created_at: chrono::Utc::now(),
                            executed_command: None,
                            execution_status: None,
                            executed_at: None,
                        }]),
                    },
                )]),
            },
        };

        assert!(manager.migrate_workspace_identities());
        let unresolved = manager
            .state
            .projects
            .values()
            .next()
            .unwrap()
            .workspaces
            .as_ref()
            .unwrap()
            .first()
            .unwrap();
        assert!(!unresolved.raw_identity_verified);
        assert_eq!(unresolved.service_key, legacy_key);

        for raw in ["feature/auth", "feature_auth"] {
            let error = manager
                .resolve_workspace_service_key_by_dir(project.path(), raw)
                .unwrap_err()
                .to_string();
            assert!(error.contains("unresolved ownership"));
            assert!(error.contains("feature/auth"));
            assert!(error.contains("feature_auth"));
        }
    }

    #[test]
    fn adopted_legacy_key_blocks_a_later_raw_name_collision() {
        let project = TempDir::new().unwrap();
        let project_key = crate::vcs::resolve_project_root(project.path())
            .to_string_lossy()
            .to_string();
        let manager = LocalStateManager {
            state_file_path: project.path().join("state.yml"),
            state: LocalState {
                schema_version: CURRENT_STATE_SCHEMA_VERSION,
                projects: HashMap::from([(
                    project_key,
                    ProjectState {
                        last_updated: chrono::Utc::now(),
                        services: None,
                        workspaces: Some(vec![DevflowWorkspace {
                            name: "feature/auth".to_string(),
                            service_key: "feature_auth".to_string(),
                            raw_identity_verified: true,
                            parent: Some("main".to_string()),
                            worktree_path: None,
                            created_at: chrono::Utc::now(),
                            executed_command: None,
                            execution_status: None,
                            executed_at: None,
                        }]),
                    },
                )]),
            },
        };
        manager.save_state().unwrap();

        assert_eq!(
            manager
                .resolve_workspace_service_key_by_dir(project.path(), "feature/auth")
                .unwrap(),
            "feature_auth"
        );
        let error = manager
            .resolve_workspace_service_key_by_dir(project.path(), "feature_auth")
            .unwrap_err()
            .to_string();
        assert!(error.contains("already owned"));
        assert!(error.contains("feature/auth"));
    }

    #[test]
    fn workspace_parent_can_be_repaired_once_but_not_rewritten() {
        let project = TempDir::new().unwrap();
        let state_path = project.path().join("state.yml");
        let config_path = project.path().join(".devflow.yml");
        let project_key = crate::vcs::resolve_project_root(project.path())
            .to_string_lossy()
            .to_string();
        let mut manager = LocalStateManager {
            state_file_path: state_path,
            state: LocalState {
                schema_version: CURRENT_STATE_SCHEMA_VERSION,
                projects: HashMap::from([(
                    project_key,
                    ProjectState {
                        last_updated: chrono::Utc::now(),
                        services: None,
                        workspaces: Some(vec![ws("feature/adopted")]),
                    },
                )]),
            },
        };
        manager.save_state().unwrap();

        let mut repaired = ws("feature/adopted");
        repaired.parent = Some("main".to_string());
        manager.register_workspace(&config_path, repaired).unwrap();
        assert_eq!(
            manager
                .get_workspace(&config_path, "feature/adopted")
                .unwrap()
                .parent
                .as_deref(),
            Some("main")
        );

        let mut attempted_rewrite = ws("feature/adopted");
        attempted_rewrite.parent = Some("release".to_string());
        manager
            .register_workspace(&config_path, attempted_rewrite)
            .unwrap();
        assert_eq!(
            manager
                .get_workspace(&config_path, "feature/adopted")
                .unwrap()
                .parent
                .as_deref(),
            Some("main")
        );
    }

    #[test]
    fn concurrent_workspace_registrations_do_not_lose_updates() {
        const WRITERS: usize = 12;

        let project = TempDir::new().unwrap();
        let config_path = project.path().join(".devflow.yml");
        let state_path = project.path().join("concurrent-state.yml");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(WRITERS));

        let handles = (0..WRITERS)
            .map(|index| {
                let config_path = config_path.clone();
                let state_path = state_path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut manager = LocalStateManager {
                        state_file_path: state_path,
                        state: LocalState::default(),
                    };
                    barrier.wait();
                    manager
                        .register_workspace(&config_path, ws(&format!("agent/concurrent-{index}")))
                        .unwrap();
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            handle.join().unwrap();
        }

        let state = LocalStateManager::load_state(&state_path).unwrap();
        let project_key = crate::vcs::resolve_project_root(project.path())
            .to_string_lossy()
            .to_string();
        let mut names = state.projects[&project_key]
            .workspaces
            .as_ref()
            .unwrap()
            .iter()
            .map(|workspace| workspace.name.clone())
            .collect::<Vec<_>>();
        names.sort();

        assert_eq!(names.len(), WRITERS);
        for index in 0..WRITERS {
            assert!(names.contains(&format!("agent/concurrent-{index}")));
        }
    }
}
