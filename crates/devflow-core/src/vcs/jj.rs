//! Jujutsu (jj) VCS provider.
//!
//! Maps jj concepts to devflow's VcsProvider trait:
//! - jj **bookmarks** → workspaces
//! - jj **workspaces** → worktrees
//! - jj **colocated repos** are supported (`.jj` + `.git` side by side)
//!
//! Repository operations use jj-lib, including snapshots and native workspaces.
//! No jj executable is required.

use super::jj_native::{self as native, State};
use anyhow::{bail, Context, Result};
use jj_lib::{
    default_backend_factories::default_working_copy_factory,
    op_store::RefTarget,
    ref_name::{RefName, WorkspaceName},
    workspace::Workspace,
};
use std::path::{Path, PathBuf};

use super::{VcsProvider, WorkspaceInfo, WorktreeCreateResult, WorktreeInfo};

/// A Jujutsu repository.
pub struct JjRepository {
    /// Root of the workspace from which the provider was opened.
    root: PathBuf,
    /// Root of jj's primary (`default`) workspace. This is the stable project
    /// identity when devflow is invoked from another native jj workspace.
    primary_root: PathBuf,
    /// Raw default identity from devflow's project/local configuration. When
    /// present it is authoritative; guessing from conventional bookmark names
    /// would otherwise map jj's physical `default` workspace incorrectly.
    configured_default: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct JjWorkspaceEntry {
    pub internal_name: String,
    pub path: PathBuf,
}

impl JjRepository {
    /// Open the jj repository at `path` (or a parent containing `.jj/`).
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let root = Self::find_repo_root(path.as_ref())
            .context("Failed to find jj repository (no .jj/ directory)")?;

        let mut repository = Self {
            primary_root: root.clone(),
            root,
            configured_default: None,
        };
        let entries = repository.workspace_entries()?;
        repository.primary_root = Self::primary_workspace_entry(&entries)?.path.clone();
        repository.configured_default =
            Self::load_configured_default_workspace(&repository.primary_root)?;

        Ok(repository)
    }

    /// Walk up from `start` to find a directory containing `.jj/`.
    fn find_repo_root(start: &Path) -> Option<PathBuf> {
        let mut current = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
        if current.is_file() {
            current.pop();
        }
        loop {
            if current.join(".jj").is_dir() {
                return Some(current);
            }
            if !current.pop() {
                return None;
            }
        }
    }

    fn primary_workspace_entry(entries: &[JjWorkspaceEntry]) -> Result<&JjWorkspaceEntry> {
        entries
            .iter()
            .find(|workspace| workspace.internal_name == "default")
            .context(
                "Unsupported jj workspace layout: the primary workspace must retain jj's internal name 'default'",
            )
    }

    fn workspace_entries(&self) -> Result<Vec<JjWorkspaceEntry>> {
        pollster::block_on(async { State::inspect(&self.root).await?.entries() })
    }

    fn load_configured_default_workspace(primary_root: &Path) -> Result<Option<String>> {
        // Honor `git.main_workspace` only when the key is literally present
        // (committed file or local override); the serde default would
        // hard-fail every jj repo whose default bookmark is e.g. `master`.
        crate::config::explicit_main_workspace_for_dir(primary_root)
    }

    fn bookmark_names(&self) -> Result<Vec<String>> {
        pollster::block_on(async { Ok(State::inspect(&self.root).await?.bookmarks()) })
    }

    /// Resolve the default bookmark from devflow config when available. For an
    /// unmanaged jj repo, prefer conventional names and then deterministic
    /// bookmark order so `devflow init` can adopt it.
    fn detect_default_bookmark(&self) -> Result<Option<String>> {
        let names = self.bookmark_names()?;
        Self::select_default_bookmark(&names, self.configured_default.as_deref())
    }

    fn select_default_bookmark(
        names: &[String],
        configured_default: Option<&str>,
    ) -> Result<Option<String>> {
        if let Some(configured) = configured_default {
            if names.iter().any(|name| name == configured) {
                return Ok(Some(configured.to_owned()));
            }
            bail!(
                "Configured default workspace '{}' is not a local jj bookmark",
                configured
            );
        }

        Ok(names
            .iter()
            .find(|name| name.as_str() == "main")
            .or_else(|| names.iter().find(|name| name.as_str() == "master"))
            .cloned()
            .or_else(|| names.first().cloned()))
    }

    fn raw_workspace_for_internal(
        internal_name: &str,
        bookmarks: &[String],
        default: Option<&str>,
    ) -> Option<String> {
        // `default` is jj's reserved primary workspace identity. Keep it
        // mapped to the detected default bookmark even if a bookmark happens
        // to have that literal name.
        if internal_name == "default" {
            return default.map(ToOwned::to_owned);
        }

        bookmarks
            .iter()
            .find(|bookmark| Self::workspace_name_for_branch(bookmark) == internal_name)
            // Compatibility with devflow builds which used the bare service
            // key as jj's internal workspace name.
            .or_else(|| {
                bookmarks.iter().find(|bookmark| {
                    crate::config::workspace_service_key(bookmark) == internal_name
                })
            })
            // Older releases used the lossy normalized name directly. This
            // lookup is safe only as a discovery candidate; local-state
            // reconciliation rejects it when multiple raw bookmarks collide.
            .or_else(|| {
                let mut matches = bookmarks.iter().filter(|bookmark| {
                    crate::config::legacy_normalize_workspace_name(bookmark) == internal_name
                });
                let candidate = matches.next();
                candidate.filter(|_| matches.next().is_none())
            })
            // The release before hashed internal names used the bookmark with
            // '/' replaced by '-'. Same ambiguity rule as the legacy lookup.
            .or_else(|| {
                let mut matches = bookmarks
                    .iter()
                    .filter(|bookmark| bookmark.replace('/', "-") == internal_name);
                let candidate = matches.next();
                candidate.filter(|_| matches.next().is_none())
            })
            // Also expose manually-created jj workspaces named exactly like a
            // local bookmark.
            .or_else(|| {
                bookmarks
                    .iter()
                    .find(|bookmark| bookmark.as_str() == internal_name)
            })
            .cloned()
    }

    fn internal_workspace_for_raw(
        entries: &[JjWorkspaceEntry],
        raw_workspace: &str,
        default: Option<&str>,
    ) -> Option<String> {
        if default == Some(raw_workspace)
            && entries.iter().any(|entry| entry.internal_name == "default")
        {
            return Some("default".to_string());
        }

        let current_name = Self::workspace_name_for_branch(raw_workspace);
        let bare_service_key = crate::config::workspace_service_key(raw_workspace);
        let legacy_name = crate::config::legacy_normalize_workspace_name(raw_workspace);
        // The release immediately before hashed internal names materialized
        // jj workspaces as the bookmark with '/' replaced by '-'. Missing it
        // here would hide those workspaces and create duplicates for the
        // same bookmark on the next switch.
        let dashed_name = raw_workspace.replace('/', "-");
        entries
            .iter()
            .find(|entry| {
                entry.internal_name == current_name
                    // `default` is jj's reserved primary workspace. Never let
                    // a raw bookmark with that literal name claim it through
                    // a legacy/bare-name compatibility match.
                    || (entry.internal_name != "default"
                        && (entry.internal_name == bare_service_key
                            || entry.internal_name == legacy_name
                            || entry.internal_name == dashed_name
                            || entry.internal_name == raw_workspace))
            })
            .map(|entry| entry.internal_name.clone())
    }

    fn paths_equal(left: &Path, right: &Path) -> bool {
        super::paths_equal(left, right)
    }

    /// Hook script installed into a colocated repo's `.git/hooks`.
    ///
    /// Same worktree-only semantics as the git provider: plain `git checkout`
    /// in a colocated repo must not resurrect in-place switch-on-checkout.
    fn generate_hook_script(&self) -> String {
        super::git::worktree_only_post_checkout_script()
    }
}

impl VcsProvider for JjRepository {
    // ── Workspace operations (mapped to bookmarks) ────────────────────

    fn current_workspace(&self) -> Result<Option<String>> {
        let state = pollster::block_on(State::inspect(&self.root))?;
        let bookmarks = state.bookmarks();
        let default =
            Self::select_default_bookmark(&bookmarks, self.configured_default.as_deref())?;
        Ok(state
            .entries()?
            .into_iter()
            .find(|workspace| Self::paths_equal(&workspace.path, &self.root))
            .and_then(|workspace| {
                Self::raw_workspace_for_internal(
                    &workspace.internal_name,
                    &bookmarks,
                    default.as_deref(),
                )
            }))
    }

    fn default_workspace(&self) -> Result<Option<String>> {
        self.detect_default_bookmark()
    }

    fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let state = pollster::block_on(State::inspect(&self.root))?;
        let names = state.bookmarks();
        let default = Self::select_default_bookmark(&names, self.configured_default.as_deref())?;
        let current = state
            .entries()?
            .iter()
            .find(|entry| Self::paths_equal(&entry.path, &self.root))
            .and_then(|entry| {
                Self::raw_workspace_for_internal(&entry.internal_name, &names, default.as_deref())
            });

        Ok(names
            .into_iter()
            .map(|name| WorkspaceInfo {
                is_current: current.as_deref() == Some(name.as_str()),
                is_default: default.as_deref() == Some(name.as_str()),
                name,
            })
            .collect())
    }

    fn create_workspace(&self, name: &str, base: Option<&str>) -> Result<()> {
        pollster::block_on(async {
            if let Some(base) = base {
                if let Some(path) = self.worktree_path(base)? {
                    if !Self::paths_equal(&path, &self.root) {
                        drop(State::load(&path).await?);
                    }
                }
            }
            let mut state = State::load(&self.root).await?;
            if state.bookmarks().iter().any(|n| n == name) {
                return Ok(());
            }
            anyhow::ensure!(!name.is_empty(), "jj bookmark name cannot be empty");
            let commit = if let Some(base) = base {
                let entries = state.entries()?;
                let default = Self::select_default_bookmark(
                    &state.bookmarks(),
                    self.configured_default.as_deref(),
                )?;
                if let Some(internal) =
                    Self::internal_workspace_for_raw(&entries, base, default.as_deref())
                {
                    state
                        .commit_for_internal(WorkspaceName::new(&internal))
                        .await?
                } else {
                    state.bookmark_commit(base).await?
                }
            } else {
                state.working_commit().await?
            };
            state.set_bookmark(name, &commit).await
        })
    }

    fn workspace_revision(&self, name: &str) -> Result<Option<String>> {
        use jj_lib::object_id::ObjectId;
        pollster::block_on(async {
            let state = State::inspect(&self.root).await?;
            let names = state.bookmarks();
            if !names.iter().any(|n| n == name) {
                return Ok(None);
            }
            let default =
                Self::select_default_bookmark(&names, self.configured_default.as_deref())?;
            let commit = if let Some(internal) =
                Self::internal_workspace_for_raw(&state.entries()?, name, default.as_deref())
            {
                state
                    .commit_for_internal(WorkspaceName::new(&internal))
                    .await?
            } else {
                state.bookmark_commit(name).await?
            };
            Ok(Some(commit.id().hex()))
        })
    }
    fn create_workspace_at(&self, name: &str, revision: &str) -> Result<()> {
        use jj_lib::repo::Repo;
        pollster::block_on(async {
            let mut state = State::load(&self.root).await?;
            let id = jj_lib::backend::CommitId::try_from_hex(revision)
                .context("Invalid jj commit identity")?;
            let commit = state.repo.store().get_commit_async(&id).await?;
            state.set_bookmark(name, &commit).await
        })
    }

    fn delete_workspace(&self, name: &str) -> Result<()> {
        pollster::block_on(async {
            let mut state = State::load(&self.root).await?;
            let name = RefName::new(name);
            let mut tx = state.repo.start_transaction();
            // Forget locally without scheduling a remote deletion.
            let remotes = state
                .repo
                .view()
                .all_remote_bookmarks()
                .filter(|(symbol, _)| symbol.name == name && symbol.remote.as_str() != "git")
                .map(|(symbol, _)| symbol.to_owned())
                .collect::<Vec<_>>();
            for symbol in remotes {
                tx.repo_mut().untrack_remote_bookmark(symbol.as_ref());
            }
            tx.repo_mut()
                .set_local_bookmark_target(name, RefTarget::absent());
            state.finish(tx, "devflow: forget local bookmark").await
        })
    }

    fn workspace_exists(&self, name: &str) -> Result<bool> {
        Ok(self
            .bookmark_names()?
            .iter()
            .any(|bookmark| bookmark == name))
    }

    // ── Worktree operations (mapped to workspaces) ─────────────────

    fn supports_worktrees(&self) -> bool {
        true // jj has native workspace support
    }

    fn is_worktree(&self) -> bool {
        !Self::paths_equal(&self.root, &self.primary_root)
    }

    fn list_worktrees(&self) -> Result<Vec<WorktreeInfo>> {
        let state = pollster::block_on(State::inspect(&self.root))?;
        let bookmarks = state.bookmarks();
        let default =
            Self::select_default_bookmark(&bookmarks, self.configured_default.as_deref())?;

        let mut worktrees: Vec<_> = state
            .entries()?
            .into_iter()
            .map(|entry| WorktreeInfo {
                is_main: Self::paths_equal(&entry.path, &self.primary_root),
                workspace: Self::raw_workspace_for_internal(
                    &entry.internal_name,
                    &bookmarks,
                    default.as_deref(),
                ),
                path: entry.path,
            })
            .collect();
        for cleanup in super::worktree_cleanup::Cleanup::list(state.workspace.repo_path())? {
            if !worktrees
                .iter()
                .any(|wt| Self::paths_equal(&wt.path, &cleanup.original))
            {
                worktrees.push(WorktreeInfo {
                    is_main: false,
                    workspace: cleanup.bookmark,
                    path: cleanup.original,
                });
            }
        }
        Ok(worktrees)
    }

    fn create_worktree(&self, workspace: &str, path: &Path) -> Result<WorktreeCreateResult> {
        let state = pollster::block_on(State::inspect(&self.root))?;
        anyhow::ensure!(
            !super::worktree_cleanup::Cleanup::list(state.workspace.repo_path())?
                .iter()
                .any(|cleanup| cleanup.bookmark.as_deref() == Some(workspace)
                    || Self::paths_equal(&cleanup.original, path)),
            "jj workspace has pending cleanup; finish removal before recreating it"
        );
        drop(state);
        self.prune_worktrees()?;
        pollster::block_on(async {
            let state = State::load(&self.root).await?;
            let base = state.bookmark_commit(workspace).await?;
            let entries = state.entries()?;
            let default = Self::select_default_bookmark(
                &state.bookmarks(),
                self.configured_default.as_deref(),
            )?;
            anyhow::ensure!(
                Self::internal_workspace_for_raw(&entries, workspace, default.as_deref()).is_none(),
                "jj workspace already exists for '{workspace}'"
            );
            let parent = path.parent().context("jj workspace path has no parent")?;
            std::fs::create_dir_all(parent)?;
            std::fs::create_dir(path)
                .with_context(|| format!("Cannot create jj workspace at {}", path.display()))?;
            let internal = Self::workspace_name_for_branch(workspace);
            let result: Result<()> = async {
                let (mut new_workspace, repo) = Workspace::init_workspace_with_existing_repo(
                    path,
                    state.workspace.repo_path(),
                    &state.repo,
                    default_working_copy_factory().as_ref(),
                    internal.clone().into(),
                )
                .await?;
                let mut locked = new_workspace.start_working_copy_mutation().await?;
                locked
                    .locked_wc()
                    .set_sparse_patterns(state.workspace.working_copy().sparse_patterns()?.to_vec())
                    .await?;
                let operation = locked.locked_wc().old_operation_id().clone();
                locked.finish(operation).await?;
                let mut tx = repo.start_transaction();
                let commit = tx
                    .repo_mut()
                    .check_out(internal.clone().into(), &base)
                    .await?;
                tx.repo_mut().set_local_bookmark_target(
                    RefName::new(workspace),
                    RefTarget::normal(commit.id().clone()),
                );
                tx.repo_mut().rebase_descendants().await?;
                if state.workspace.workspace_root().join(".git").exists() {
                    jj_lib::git::export_refs(tx.repo_mut())?;
                }
                let repo = tx.commit("devflow: materialize workspace").await?;
                new_workspace
                    .check_out(repo.op_id().clone(), None, &commit)
                    .await?;
                Ok(())
            }
            .await;
            if let Err(error) = result {
                return Err(error.context(format!(
                    "jj workspace creation did not finish; inspect {} before retrying",
                    path.display()
                )));
            }
            Ok(WorktreeCreateResult::new())
        })
    }

    fn remove_worktree(&self, path: &Path, force: bool) -> Result<()> {
        use super::worktree_cleanup::Cleanup;
        anyhow::ensure!(
            !Self::paths_equal(path, &self.primary_root) && !Self::paths_equal(path, &self.root),
            "Cannot remove the current or primary jj workspace"
        );
        let repo_path = pollster::block_on(State::inspect(&self.root))?
            .workspace
            .repo_path()
            .to_path_buf();
        let receipt = Cleanup::receipt(&repo_path, path);
        let _lease = crate::services::ownership::OperationLease::try_acquire(
            &receipt.with_extension("lock"),
        )?
        .context("Another jj cleanup is active for this directory")?;
        if let Some(mut cleanup) = Cleanup::read(&receipt)? {
            anyhow::ensure!(
                Self::paths_equal(&cleanup.original, path),
                "jj cleanup receipt path mismatch"
            );
            pollster::block_on(async {
                let mut state = State::load(&self.root).await?;
                state
                    .verify_removal_generation(
                        &cleanup.internal,
                        &cleanup.saved_commit,
                        &cleanup.original,
                    )
                    .await?;
                cleanup.remove_directory(&receipt)?;
                state.forget(&cleanup.internal).await
            })?;
            std::fs::remove_file(receipt)?;
            return Ok(());
        }
        let entry = self
            .workspace_entries()?
            .into_iter()
            .find(|entry| Self::paths_equal(&entry.path, path))
            .with_context(|| format!("No jj workspace is registered at '{}'", path.display()))?;

        if !force && entry.path.exists() && self.worktree_is_dirty(&entry.path)? {
            bail!(
                "jj workspace at '{}' has uncommitted changes; commit them or use --force",
                entry.path.display()
            );
        }

        // Preserve changes in the workspace being removed before forgetting it.
        if entry.path.exists() {
            drop(pollster::block_on(State::load(&entry.path))?);
        }
        let (saved_commit, bookmark) = pollster::block_on(async {
            let mut state = State::load(&self.root).await?;
            let default = Self::select_default_bookmark(
                &state.bookmarks(),
                self.configured_default.as_deref(),
            )?;
            let raw = Self::raw_workspace_for_internal(
                &entry.internal_name,
                &state.bookmarks(),
                default.as_deref(),
            );
            let commit = state
                .commit_for_internal(WorkspaceName::new(&entry.internal_name))
                .await?;
            if let Some(raw) = &raw {
                state.set_bookmark(raw, &commit).await?;
            }
            use jj_lib::object_id::ObjectId;
            Ok::<_, anyhow::Error>((commit.id().hex(), raw))
        })?;
        if entry.path.try_exists()? {
            let mut cleanup = Cleanup::begin(
                &receipt,
                entry.internal_name.clone(),
                &entry.path,
                saved_commit.clone(),
                bookmark,
                None,
            )?;
            cleanup.remove_directory(&receipt)?;
        }
        pollster::block_on(async {
            let mut state = State::load(&self.root).await?;
            state
                .verify_removal_generation(&entry.internal_name, &saved_commit, &entry.path)
                .await?;
            state.forget(&entry.internal_name).await
        })?;
        if receipt.try_exists()? {
            std::fs::remove_file(receipt)?;
        }

        Ok(())
    }

    fn worktree_is_dirty(&self, path: &Path) -> Result<bool> {
        if !path.exists() {
            return Ok(false);
        }
        pollster::block_on(async { State::inspect(path).await?.is_dirty().await })
    }

    fn prepare_worktree(&self, path: &Path) -> Result<()> {
        pollster::block_on(State::load(path)).map(drop)
    }

    fn worktree_path(&self, workspace: &str) -> Result<Option<PathBuf>> {
        let state = pollster::block_on(State::inspect(&self.root))?;
        if let Some(cleanup) = super::worktree_cleanup::Cleanup::list(state.workspace.repo_path())?
            .into_iter()
            .find(|cleanup| cleanup.bookmark.as_deref() == Some(workspace))
        {
            return Ok(Some(cleanup.original));
        }
        let entries = state.entries()?;
        let default =
            Self::select_default_bookmark(&state.bookmarks(), self.configured_default.as_deref())?;
        let Some(internal_name) =
            Self::internal_workspace_for_raw(&entries, workspace, default.as_deref())
        else {
            return Ok(None);
        };
        Ok(entries
            .into_iter()
            .find(|entry| entry.internal_name == internal_name)
            .map(|entry| entry.path))
    }

    fn main_worktree_dir(&self) -> Option<PathBuf> {
        Some(self.primary_root.clone())
    }

    fn prune_worktrees(&self) -> Result<()> {
        // Mirror `git worktree prune`: forget workspace registrations whose
        // working directory no longer exists. Without this, a manually
        // deleted jj workspace directory stays registered forever —
        // `worktree_path` keeps returning the dangling path and re-creation
        // of the bookmark's workspace is permanently blocked. Only native
        // registration metadata is touched: no directories, no bookmarks.
        for entry in self.workspace_entries()? {
            let state = pollster::block_on(State::inspect(&self.root))?;
            if super::worktree_cleanup::Cleanup::receipt(state.workspace.repo_path(), &entry.path)
                .try_exists()?
            {
                continue;
            }
            drop(state);
            // `default` is jj's reserved primary workspace; never forget it,
            // even if its recorded path looks missing (e.g. an unmounted
            // volume) — losing the primary registration is unrecoverable.
            if entry.internal_name == "default" || entry.path.try_exists()? {
                continue;
            }
            if let Err(error) = pollster::block_on(async {
                State::load(&self.root)
                    .await?
                    .forget(&entry.internal_name)
                    .await
            }) {
                log::debug!(
                    "Failed to forget stale jj workspace '{}': {error:#}",
                    entry.internal_name
                );
            }
        }
        Ok(())
    }

    // ── Hooks ──────────────────────────────────────────────────────

    fn install_hooks(&self) -> Result<()> {
        if self.primary_root.join(".git").exists() {
            super::hooks::install(
                &git2::Repository::open(&self.primary_root)?,
                &[("post-checkout", self.generate_hook_script())],
            )?;
        } else {
            log::warn!("jj has no Git hooks; use devflow lifecycle hooks");
        }
        Ok(())
    }

    fn uninstall_hooks(&self) -> Result<()> {
        if self.primary_root.join(".git").exists() {
            super::hooks::uninstall(
                &git2::Repository::open(&self.primary_root)?,
                &["post-checkout"],
            )?;
        }
        Ok(())
    }

    fn is_devflow_hook(&self, hook_path: &Path) -> Result<bool> {
        super::hooks::is_owned(hook_path)
    }

    fn hook_paths(&self) -> Vec<PathBuf> {
        git2::Repository::open(&self.root)
            .ok()
            .and_then(|repo| super::hooks::directory(&repo).ok())
            .map(|dir| vec![dir.join("post-checkout")])
            .unwrap_or_default()
    }

    // ── Meta ───────────────────────────────────────────────────────

    fn provider_name(&self) -> &'static str {
        "jj"
    }
}

impl JjRepository {
    /// Initialize a jj repository at `path` using native library calls.
    ///
    /// When `colocate` is true, share its working copy with the `.git/`
    /// repository, adopting an existing Git repository when present.
    ///
    /// Uses the embedded jj-lib implementation.
    pub fn init<P: AsRef<Path>>(path: P, colocate: bool) -> Result<Self> {
        let path = path.as_ref();
        std::fs::create_dir_all(path)?;
        let canonical = path.canonicalize()?;
        let path = canonical.as_path();
        pollster::block_on(async {
            let settings = native::settings(path, None)?;
            if colocate && path.join(".git").exists() {
                let git = git2::Repository::open(path)?;
                Workspace::init_external_git(&settings, path, git.path()).await?;
            } else if colocate {
                Workspace::init_colocated_git(&settings, path, Default::default()).await?;
            } else {
                Workspace::init_internal_git(&settings, path, Default::default()).await?;
            }
            drop(State::load(path).await?);
            Ok::<_, anyhow::Error>(())
        })?;

        let repository = Self::new(path)?;
        if repository.bookmark_names()?.is_empty() {
            let initial_workspace = repository.configured_default.as_deref().unwrap_or("main");
            repository.create_workspace(initial_workspace, None)?;
        }
        Ok(repository)
    }

    /// Convert a raw bookmark to a collision-resistant jj workspace name.
    fn workspace_name_for_branch(workspace: &str) -> String {
        // Prefix the identity before normalizing so it cannot collide with
        // jj's reserved primary workspace name (`default`).
        crate::config::workspace_service_key(&format!("jj-workspace-{workspace}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_commit(
        git: &git2::Repository,
        reference: &str,
        parent: Option<git2::Oid>,
        content: &str,
    ) -> git2::Oid {
        let blob = git.blob(content.as_bytes()).unwrap();
        let mut builder = git.treebuilder(None).unwrap();
        builder.insert("file", blob, 0o100644).unwrap();
        let tree = git.find_tree(builder.write().unwrap()).unwrap();
        let sig = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        let parent = parent.map(|id| git.find_commit(id).unwrap());
        git.commit(
            Some(reference),
            &sig,
            &sig,
            "fixture",
            &tree,
            &parent.iter().collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[test]
    fn inspection_and_snapshot_preserve_git_staging_and_inspection_preserves_merge() {
        let root = tempfile::tempdir().unwrap();
        let git = git2::Repository::init_opts(
            root.path(),
            git2::RepositoryInitOptions::new().initial_head("main"),
        )
        .unwrap();
        let base = git_commit(&git, "HEAD", None, "base\n");
        git.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        let jj = JjRepository::init(root.path(), true).unwrap();
        std::fs::write(root.path().join("file"), "staged\n").unwrap();
        let mut index = git.index().unwrap();
        index.add_path(Path::new("file")).unwrap();
        index.write().unwrap();
        let staged = index.get_path(Path::new("file"), 0).unwrap().id;
        std::fs::write(root.path().join("file"), "unstaged\n").unwrap();
        jj.list_workspaces().unwrap();
        jj.list_worktrees().unwrap();
        jj.worktree_is_dirty(root.path()).unwrap();
        jj.prepare_worktree(root.path()).unwrap();
        jj.create_workspace("staging-preserved", Some("main"))
            .unwrap();
        index.read(true).unwrap();
        assert_eq!(index.get_path(Path::new("file"), 0).unwrap().id, staged);
        assert_eq!(
            std::fs::read_to_string(root.path().join("file")).unwrap(),
            "unstaged\n"
        );
        git_commit(&git, "HEAD", Some(base), "ours\n");
        git.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        let theirs = git_commit(&git, "refs/heads/theirs", Some(base), "theirs\n");
        jj.prepare_worktree(root.path()).unwrap();
        git.merge(&[&git.find_annotated_commit(theirs).unwrap()], None, None)
            .unwrap();
        let contents = std::fs::read(root.path().join("file")).unwrap();
        jj.list_workspaces().unwrap();
        jj.list_worktrees().unwrap();
        assert!(jj.prepare_worktree(root.path()).is_err());
        index.read(true).unwrap();
        assert!(index.has_conflicts());
        assert_eq!(git.state(), git2::RepositoryState::Merge);
        assert!(git.path().join("MERGE_HEAD").exists());
        assert_eq!(std::fs::read(root.path().join("file")).unwrap(), contents);
    }

    #[test]
    fn stale_workspace_recovers_natively_and_keeps_local_edits() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("primary");
        let child = temp.path().join("child");
        let jj = JjRepository::init(&root, false).unwrap();
        std::fs::write(root.join("file"), "base\n").unwrap();
        jj.create_workspace("feature", Some("main")).unwrap();
        jj.create_worktree("feature", &child).unwrap();
        std::fs::write(child.join("local"), "keep\n").unwrap();
        std::fs::write(root.join("file"), "new main\n").unwrap();
        jj.prepare_worktree(&root).unwrap();
        let child_jj = JjRepository::new(&child).unwrap();
        child_jj.list_workspaces().unwrap();
        assert_eq!(
            std::fs::read_to_string(child.join("file")).unwrap(),
            "base\n"
        );
        child_jj.prepare_worktree(&child).unwrap();
        assert_eq!(
            std::fs::read_to_string(child.join("file")).unwrap(),
            "new main\n"
        );
        assert_eq!(
            std::fs::read_to_string(child.join("local")).unwrap(),
            "keep\n"
        );
        child_jj.prepare_worktree(&child).unwrap();
    }

    #[test]
    fn stale_workspace_preserves_conflicting_parent_and_child_edits() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("primary");
        let child = temp.path().join("child");
        let jj = JjRepository::init(&root, false).unwrap();
        std::fs::write(root.join("file"), "base\n").unwrap();
        jj.create_workspace("feature", Some("main")).unwrap();
        jj.create_worktree("feature", &child).unwrap();
        std::fs::write(child.join("file"), "child edit\n").unwrap();
        std::fs::write(root.join("file"), "parent edit\n").unwrap();
        jj.prepare_worktree(&root).unwrap();
        jj.prepare_worktree(&child).unwrap();
        let conflict = std::fs::read_to_string(child.join("file")).unwrap();
        assert!(
            conflict.contains("parent edit") && conflict.contains("child edit"),
            "{conflict}"
        );
        assert!(conflict.contains("<<<<<<<"), "{conflict}");
        jj.prepare_worktree(&child).unwrap();
        assert_eq!(
            std::fs::read_to_string(child.join("file")).unwrap(),
            conflict
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_directory_removal_keeps_registration_and_can_retry() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("primary");
        let parent = temp.path().join("restricted");
        let child = parent.join("child");
        let jj = JjRepository::init(&root, false).unwrap();
        jj.create_workspace("feature", Some("main")).unwrap();
        jj.create_worktree("feature", &child).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = jj.remove_worktree(&child, true);
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
        if result.is_err() {
            assert!(jj.worktree_path("feature").unwrap().is_some());
        }
        if result.is_err() {
            jj.remove_worktree(&child, true).unwrap();
        }
        assert!(!child.exists());
        assert!(jj.worktree_path("feature").unwrap().is_none());
    }

    #[test]
    fn cleanup_receipt_survives_forgotten_registration_and_blocks_recreation_until_finished() {
        use super::super::worktree_cleanup::Cleanup;
        use jj_lib::object_id::ObjectId;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("primary");
        let child = temp.path().join("child");
        let jj = JjRepository::init(&root, false).unwrap();
        jj.create_workspace("feature", Some("main")).unwrap();
        jj.create_worktree("feature", &child).unwrap();
        let receipt = pollster::block_on(async {
            let mut state = State::load(&root).await.unwrap();
            let entry = state
                .entries()
                .unwrap()
                .into_iter()
                .find(|entry| super::super::paths_equal(&entry.path, &child))
                .unwrap();
            let commit = state
                .commit_for_internal(WorkspaceName::new(&entry.internal_name))
                .await
                .unwrap();
            let receipt = Cleanup::receipt(state.workspace.repo_path(), &child);
            let mut cleanup = Cleanup::begin(
                &receipt,
                entry.internal_name.clone(),
                &child,
                commit.id().hex(),
                Some("feature".into()),
                None,
            )
            .unwrap();
            cleanup.remove_directory(&receipt).unwrap();
            state.forget(&entry.internal_name).await.unwrap();
            receipt
        });
        assert!(jj.worktree_path("feature").unwrap().is_some());
        assert!(jj
            .list_worktrees()
            .unwrap()
            .iter()
            .any(|wt| wt.workspace.as_deref() == Some("feature")));
        assert!(jj.create_worktree("feature", &child).is_err());
        jj.remove_worktree(&child, true).unwrap();
        assert!(!receipt.exists());
        jj.create_worktree("feature", &child).unwrap();
        jj.remove_worktree(&child, true).unwrap();
    }

    #[test]
    fn native_forget_keeps_remote_bookmark_and_sparse_workspace_rules() {
        use jj_lib::{
            op_store::{RemoteRef, RemoteRefState},
            repo_path::RepoPathBuf,
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("primary");
        let jj = JjRepository::init(&root, false).unwrap();
        std::fs::write(root.join("included"), "include").unwrap();
        std::fs::write(root.join("excluded"), "exclude").unwrap();
        jj.create_workspace("feature", None).unwrap();
        pollster::block_on(async {
            let mut state = State::load(&root).await.unwrap();
            let id = state.bookmark_commit("feature").await.unwrap().id().clone();
            let mut tx = state.repo.start_transaction();
            let remote = RefName::new("feature").to_remote_symbol("origin".as_ref());
            tx.repo_mut().set_remote_bookmark(
                remote,
                RemoteRef {
                    target: RefTarget::normal(id),
                    state: RemoteRefState::Tracked,
                },
            );
            state.finish(tx, "test: remote tracking").await.unwrap();
            let mut locked = state.workspace.start_working_copy_mutation().await.unwrap();
            locked
                .locked_wc()
                .set_sparse_patterns(vec![RepoPathBuf::from_internal_string("included").unwrap()])
                .await
                .unwrap();
            let op = state.repo.op_id().clone();
            locked.finish(op).await.unwrap();
        });
        let child = temp.path().join("child");
        jj.create_worktree("feature", &child).unwrap();
        assert!(child.join("included").exists());
        assert!(!child.join("excluded").exists());
        jj.remove_worktree(&child, false).unwrap();
        jj.delete_workspace("feature").unwrap();
        pollster::block_on(async {
            let state = State::load(&root).await.unwrap();
            let remote = state
                .repo
                .view()
                .get_remote_bookmark(RefName::new("feature").to_remote_symbol("origin".as_ref()));
            assert!(remote.target.is_present());
            assert_eq!(remote.state, RemoteRefState::New);
        });
    }

    #[test]
    fn native_colocated_imports_existing_and_subsequent_git_commits() {
        let temp = tempfile::tempdir().unwrap();
        let git = git2::Repository::init(temp.path()).unwrap();
        fn commit(git: &git2::Repository, content: &str) {
            let blob = git.blob(content.as_bytes()).unwrap();
            let mut tree = git.treebuilder(None).unwrap();
            tree.insert("file", blob, 0o100644).unwrap();
            let tree = git.find_tree(tree.write().unwrap()).unwrap();
            let signature = git2::Signature::now("Native test", "test@example.com").unwrap();
            let parent = git.head().ok().and_then(|h| h.peel_to_commit().ok());
            let parents = parent.iter().collect::<Vec<_>>();
            git.commit(
                Some("HEAD"),
                &signature,
                &signature,
                "test",
                &tree,
                &parents,
            )
            .unwrap();
            git.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
                .unwrap();
        }
        commit(&git, "first");
        let jj = JjRepository::init(temp.path(), true).unwrap();
        commit(&git, "second");
        jj.create_workspace("git-import", None).unwrap();
        let child_root = tempfile::tempdir().unwrap();
        let child = child_root.path().join("child");
        jj.create_worktree("git-import", &child).unwrap();
        assert_eq!(
            std::fs::read_to_string(child.join("file")).unwrap(),
            "second"
        );
    }

    #[test]
    fn native_snapshot_respects_immutable_history_and_untracked_files() {
        let temp = tempfile::tempdir().unwrap();
        let jj = JjRepository::init(temp.path(), false).unwrap();
        let initial = pollster::block_on(async {
            State::load(temp.path())
                .await
                .unwrap()
                .bookmark_commit("main")
                .await
                .unwrap()
                .id()
                .clone()
        });
        std::fs::write(temp.path().join(".jj/repo/config.toml"), "[revset-aliases]\n'immutable_heads()' = 'bookmarks(exact:\"main\")'\n[snapshot]\nauto-track = 'glob:\"*.rs\"'\n[signing]\nbackend = 'ssh'\nbehavior = 'own'\nkey = '/does-not-exist'\n").unwrap();
        std::fs::write(temp.path().join("tracked.rs"), "fn main() {}\n").unwrap();
        std::fs::write(temp.path().join("untracked.txt"), "keep").unwrap();
        pollster::block_on(async {
            let state = State::load(temp.path()).await.unwrap();
            assert_eq!(state.bookmark_commit("main").await.unwrap().id(), &initial);
            assert_eq!(
                state.working_commit().await.unwrap().parent_ids(),
                &[initial]
            );
        });
        assert!(jj.worktree_is_dirty(temp.path()).unwrap());
        assert!(temp.path().join("untracked.txt").exists());
    }

    #[tokio::test]
    async fn native_workspace_lifecycle_preserves_data_and_identity() {
        for colocated in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("primary");
            let repo = JjRepository::init(&root, colocated).unwrap();
            std::fs::write(root.join("source.txt"), "source content").unwrap();
            std::fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
            std::fs::create_dir(root.join("ignored")).unwrap();
            std::fs::write(root.join("ignored/secret"), "excluded").unwrap();
            repo.create_workspace("feature/auth", Some("main")).unwrap();
            let child = temp.path().join("child with spaces");
            repo.create_worktree("feature/auth", &child).unwrap();
            assert_eq!(
                std::fs::read_to_string(child.join("source.txt")).unwrap(),
                "source content"
            );
            assert!(!child.join("ignored/secret").exists());
            let child_repo = JjRepository::new(&child).unwrap();
            assert_eq!(
                child_repo.current_workspace().unwrap().as_deref(),
                Some("feature/auth")
            );
            assert_eq!(
                child_repo.main_worktree_dir(),
                Some(root.canonicalize().unwrap())
            );
            assert!(!repo.worktree_is_dirty(&child).unwrap());
            std::fs::write(child.join("source.txt"), "child changes").unwrap();
            assert!(repo.worktree_is_dirty(&child).unwrap());
            assert!(repo.remove_worktree(&child, false).is_err());
            assert!(child.join("source.txt").exists());
            repo.remove_worktree(&child, true).unwrap();
            assert!(!child.exists());
            repo.create_worktree("feature/auth", &child).unwrap();
            assert_eq!(
                std::fs::read_to_string(child.join("source.txt")).unwrap(),
                "child changes"
            );
            assert_eq!(
                std::fs::read_to_string(root.join("source.txt")).unwrap(),
                "source content"
            );
            repo.remove_worktree(&child, false).unwrap();
            repo.delete_workspace("feature/auth").unwrap();
            assert!(!repo.workspace_exists("feature/auth").unwrap());
        }
    }

    #[test]
    fn native_workspaces_prune_missing_registrations_and_refuse_existing_paths() {
        let temp = tempfile::tempdir().unwrap();
        let repo = JjRepository::init(temp.path().join("primary"), false).unwrap();
        repo.create_workspace("feature/a", None).unwrap();
        let child = temp.path().join("child");
        std::fs::create_dir(&child).unwrap();
        std::fs::write(child.join("owned"), "keep").unwrap();
        assert!(repo.create_worktree("feature/a", &child).is_err());
        assert_eq!(
            std::fs::read_to_string(child.join("owned")).unwrap(),
            "keep"
        );
        std::fs::remove_dir_all(&child).unwrap();
        repo.create_worktree("feature/a", &child).unwrap();
        std::fs::remove_dir_all(&child).unwrap();
        repo.prune_worktrees().unwrap();
        assert!(repo.worktree_path("feature/a").unwrap().is_none());
        repo.create_worktree("feature/a", &child).unwrap();
    }

    #[test]
    fn test_workspace_name_for_branch() {
        let main = JjRepository::workspace_name_for_branch("main");
        assert!(main.starts_with("jj_workspace_main_"));
        assert!(main.len() <= 63);
        assert_ne!(
            JjRepository::workspace_name_for_branch("feature/auth"),
            JjRepository::workspace_name_for_branch("feature-auth")
        );
        assert_ne!(
            JjRepository::workspace_name_for_branch("default"),
            "default"
        );
    }

    #[test]
    fn rejects_layout_without_native_default_workspace() {
        let entries = vec![JjWorkspaceEntry {
            internal_name: "renamed-primary".to_owned(),
            path: PathBuf::from("/tmp/project"),
        }];
        let error = JjRepository::primary_workspace_entry(&entries).unwrap_err();
        assert!(error.to_string().contains("must retain"));
    }

    #[test]
    fn maps_raw_bookmarks_to_internal_workspace_names() {
        let bookmarks = vec![
            "main".to_owned(),
            "feature/auth".to_owned(),
            "feature-auth".to_owned(),
        ];
        let internal = JjRepository::workspace_name_for_branch("feature/auth");

        assert_eq!(
            JjRepository::raw_workspace_for_internal(&internal, &bookmarks, Some("main"))
                .as_deref(),
            Some("feature/auth")
        );
        assert_eq!(
            JjRepository::raw_workspace_for_internal("default", &bookmarks, Some("main"))
                .as_deref(),
            Some("main")
        );

        let entries = vec![
            JjWorkspaceEntry {
                internal_name: "default".to_string(),
                path: PathBuf::from("/tmp/main"),
            },
            JjWorkspaceEntry {
                internal_name: internal.clone(),
                path: PathBuf::from("/tmp/feature"),
            },
        ];
        assert_eq!(
            JjRepository::internal_workspace_for_raw(&entries, "main", Some("main")).as_deref(),
            Some("default")
        );
        assert_eq!(
            JjRepository::internal_workspace_for_raw(&entries, "feature/auth", Some("main"))
                .as_deref(),
            Some(internal.as_str())
        );

        let literal_default_internal = JjRepository::workspace_name_for_branch("default");
        let entries_with_literal_default = vec![
            JjWorkspaceEntry {
                internal_name: "default".to_owned(),
                path: PathBuf::from("/tmp/main"),
            },
            JjWorkspaceEntry {
                internal_name: literal_default_internal.clone(),
                path: PathBuf::from("/tmp/literal-default"),
            },
        ];
        assert_eq!(
            JjRepository::internal_workspace_for_raw(
                &entries_with_literal_default,
                "default",
                Some("main")
            )
            .as_deref(),
            Some(literal_default_internal.as_str())
        );
        assert_eq!(
            JjRepository::internal_workspace_for_raw(&entries[..1], "default", Some("main")),
            None,
            "the raw literal 'default' must not alias jj's reserved primary workspace"
        );
    }

    #[test]
    fn maps_dash_scheme_internal_names_from_previous_release() {
        // The release before hashed internal names materialized jj workspaces
        // as the bookmark with '/' replaced by '-'. Those workspaces must
        // stay attached to their bookmark, or the next switch would create a
        // duplicate workspace and orphan the old directory.
        let bookmarks = vec!["main".to_owned(), "feature/auth".to_owned()];
        assert_eq!(
            JjRepository::raw_workspace_for_internal("feature-auth", &bookmarks, Some("main"))
                .as_deref(),
            Some("feature/auth")
        );

        let entries = vec![
            JjWorkspaceEntry {
                internal_name: "default".to_owned(),
                path: PathBuf::from("/tmp/main"),
            },
            JjWorkspaceEntry {
                internal_name: "feature-auth".to_owned(),
                path: PathBuf::from("/tmp/feature"),
            },
        ];
        assert_eq!(
            JjRepository::internal_workspace_for_raw(&entries, "feature/auth", Some("main"))
                .as_deref(),
            Some("feature-auth")
        );

        // A literal `feature-auth` bookmark makes the dashed form ambiguous;
        // the exact-name fallback must then win deterministically.
        let ambiguous = vec!["feature/auth".to_owned(), "feature-auth".to_owned()];
        assert_eq!(
            JjRepository::raw_workspace_for_internal("feature-auth", &ambiguous, Some("main"))
                .as_deref(),
            Some("feature-auth")
        );
    }

    #[test]
    fn configured_default_bookmark_is_authoritative() {
        let bookmarks = vec!["main".to_owned(), "develop".to_owned()];
        assert_eq!(
            JjRepository::select_default_bookmark(&bookmarks, Some("develop"))
                .unwrap()
                .as_deref(),
            Some("develop")
        );
        assert!(JjRepository::select_default_bookmark(&bookmarks, Some("missing")).is_err());
    }
}
