use anyhow::{Context, Result};
use git2::{ErrorCode, Repository, WorktreeAddOptions, WorktreePruneOptions};
use std::path::{Path, PathBuf};

use super::worktree_cleanup::Cleanup;
use super::{VcsProvider, WorkspaceInfo, WorktreeCreateResult, WorktreeInfo};

/// Linked worktree discovery happens in native devflow code.
pub(crate) fn worktree_only_post_checkout_script() -> String {
    r#"#!/bin/sh
# devflow auto-generated hook
[ "$3" = "0" ] && exit 0
if command -v devflow >/dev/null 2>&1; then
    devflow git-hook
fi
"#
    .to_owned()
}

pub(super) fn pre_commit_script() -> String {
    r#"#!/bin/sh
# devflow auto-generated hook
# This hook runs devflow pre-commit lifecycle hooks before each commit.

if command -v devflow >/dev/null 2>&1; then
    devflow hook run pre-commit
    exit $?
fi
"#
    .to_owned()
}

pub struct GitRepository {
    repo: Repository,
    /// Canonical path of the primary checkout. `Repository::workdir()` points
    /// at the linked checkout when this provider is opened from a worktree.
    primary_root: PathBuf,
}

impl GitRepository {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let repo = Repository::open(path).context("Failed to open Git repository")?;
        let primary_root = Self::resolve_primary_root(&repo)?;

        Ok(GitRepository { repo, primary_root })
    }

    /// Initialize a new Git repository at `path` using `git2::Repository::init()`.
    ///
    /// This is a pure library call — no external `git` binary needed.
    pub fn init<P: AsRef<Path>>(path: P) -> Result<Self> {
        let repo =
            Repository::init(path.as_ref()).context("Failed to initialize Git repository")?;

        // Point HEAD at refs/heads/main so the default workspace is always "main",
        // regardless of the user's `init.defaultBranch` git setting.
        repo.set_head("refs/heads/main")
            .or_else(|_| {
                // set_head can fail on a truly empty repo; fall back to
                // rewriting the symbolic reference directly.
                repo.reference_symbolic(
                    "HEAD",
                    "refs/heads/main",
                    true,
                    "devflow: set default workspace to main",
                )
                .map(|_| ())
            })
            .context("Failed to set default workspace to main")?;

        let primary_root = Self::resolve_primary_root(&repo)?;
        let git_repo = GitRepository { repo, primary_root };

        // Create an initial empty commit so that the "main" workspace actually
        // exists.  Without this the repo stays in "unborn HEAD" state and
        // git reports zero workspaces, which breaks list/tui/switch.
        git_repo.create_initial_commit()?;

        Ok(git_repo)
    }

    /// Return the HEAD commit, or create an initial empty commit if the
    /// repository has no commits yet (unborn HEAD).
    ///
    /// The auto-created commit uses an empty tree and the message
    /// `"Initial commit (devflow)"`.  The author/committer signature is
    /// resolved from the git configuration, falling back to a generic
    /// `"devflow" <devflow@localhost>` identity.
    fn head_commit_or_init(&self) -> Result<git2::Commit<'_>> {
        match self.repo.head() {
            Ok(head) => head
                .peel_to_commit()
                .context("HEAD does not point to a commit"),
            Err(e) if e.code() == ErrorCode::UnbornBranch => {
                log::info!("Unborn workspace detected — creating initial empty commit");
                self.create_initial_commit()
            }
            Err(e) => Err(e).context("Failed to get HEAD"),
        }
    }

    /// Create an initial empty commit on the current unborn workspace.
    fn create_initial_commit(&self) -> Result<git2::Commit<'_>> {
        let sig = self
            .repo
            .signature()
            .or_else(|_| git2::Signature::now("devflow", "devflow@localhost"))
            .context("Failed to create commit signature")?;

        let empty_tree_oid = self
            .repo
            .treebuilder(None)
            .context("Failed to create tree builder")?
            .write()
            .context("Failed to write empty tree")?;
        let tree = self
            .repo
            .find_tree(empty_tree_oid)
            .context("Failed to find empty tree")?;

        let oid = self
            .repo
            .commit(
                Some("HEAD"),
                &sig,
                &sig,
                "Initial commit (devflow)",
                &tree,
                &[],
            )
            .context("Failed to create initial commit")?;

        self.repo
            .find_commit(oid)
            .context("Failed to find newly created commit")
    }

    pub fn get_current_workspace(&self) -> Result<Option<String>> {
        match self.repo.head() {
            Ok(head) if head.is_branch() => Ok(head.shorthand().ok().map(ToOwned::to_owned)),
            // A detached HEAD has no raw workspace identity. In particular,
            // do not expose the literal pseudo-name `HEAD` to callers.
            Ok(_) => Ok(None),
            Err(e) if e.code() == ErrorCode::UnbornBranch => {
                // HEAD exists but points to a workspace with no commits.
                // Read the symbolic target of HEAD to get the workspace name.
                match self.repo.find_reference("HEAD") {
                    Ok(head_ref) => {
                        if let Ok(Some(target)) = head_ref.symbolic_target() {
                            // target is e.g. "refs/heads/main"
                            let workspace_name =
                                target.strip_prefix("refs/heads/").unwrap_or(target);
                            Ok(Some(workspace_name.to_string()))
                        } else {
                            Ok(None)
                        }
                    }
                    Err(_) => Ok(None),
                }
            }
            Err(e) => Err(e).context("Failed to get HEAD reference"),
        }
    }

    pub fn workspace_exists(&self, workspace_name: &str) -> Result<bool> {
        match self
            .repo
            .find_branch(workspace_name, git2::BranchType::Local)
        {
            Ok(_) => Ok(true),
            Err(e) => {
                if e.code() == git2::ErrorCode::NotFound {
                    Ok(false)
                } else {
                    Err(anyhow::anyhow!("Error checking workspace: {}", e))
                }
            }
        }
    }

    pub fn detect_main_workspace(&self) -> Result<Option<String>> {
        // The primary checkout is the stable root of devflow's workspace
        // graph. Its checked-out branch therefore defines Git's default
        // workspace, even when a remote advertises another default or a
        // conventional `main` branch also exists. This is deliberately
        // independent of the linked checkout from which devflow was invoked.
        Self::workspace_at(&self.primary_root)
    }

    fn generate_hook_script(&self) -> String {
        worktree_only_post_checkout_script()
    }

    fn generate_pre_commit_script(&self) -> String {
        pre_commit_script()
    }

    pub fn get_repo_root(&self) -> &Path {
        &self.primary_root
    }

    /// Resolve the primary checkout independently of the checkout from which
    /// the repository was opened.
    fn resolve_primary_root(repo: &Repository) -> Result<PathBuf> {
        let root = if repo.is_worktree() {
            repo.commondir().parent().with_context(|| {
                format!(
                    "Git common directory '{}' has no parent",
                    repo.commondir().display()
                )
            })?
        } else {
            repo.workdir().unwrap_or_else(|| repo.path())
        };

        Ok(root.canonicalize().unwrap_or_else(|_| root.to_path_buf()))
    }

    fn workspace_at(path: &Path) -> Result<Option<String>> {
        let repo = Repository::open(path)
            .with_context(|| format!("Failed to open Git worktree at '{}'", path.display()))?;
        let result = match repo.head() {
            Ok(head) if head.is_branch() => Ok(head.shorthand().ok().map(ToOwned::to_owned)),
            Ok(_) => Ok(None),
            Err(error) if error.code() == ErrorCode::UnbornBranch => {
                let head = repo.find_reference("HEAD")?;
                Ok(head.symbolic_target()?.map(|target| {
                    target
                        .strip_prefix("refs/heads/")
                        .unwrap_or(target)
                        .to_owned()
                }))
            }
            Err(error) => Err(error).context("Failed to get worktree HEAD"),
        };
        result
    }

    /// Build a collision-resistant internal worktree name for Git metadata.
    fn worktree_name_for_branch(workspace: &str) -> String {
        crate::config::workspace_service_key(workspace)
    }
}

// ─── VcsProvider implementation ────────────────────────────────────────────

/// Refuse to touch a worktree that still holds uncommitted work.
///
/// Counts tracked modifications, staged changes, and untracked non-ignored
/// files — the same set `git worktree remove` refuses on.  Gitignored files
/// (e.g. `.env.local` copied in by devflow) never block removal.
fn ensure_worktree_clean(path: &Path) -> Result<()> {
    let dirty = worktree_changes(path)?;

    if !dirty.is_empty() {
        let preview = dirty
            .iter()
            .take(3)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        let suffix = if dirty.len() > 3 { ", …" } else { "" };
        anyhow::bail!(
            "Worktree at '{}' has {} uncommitted change(s) ({}{}). Commit or stash them, or use --force to discard.",
            path.display(),
            dirty.len(),
            preview,
            suffix
        );
    }
    Ok(())
}

fn worktree_changes(path: &Path) -> Result<Vec<String>> {
    let wt_repo = Repository::open(path)
        .with_context(|| format!("Failed to open worktree at '{}'", path.display()))?;
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true)
        .include_ignored(false)
        .exclude_submodules(true);
    let statuses = wt_repo
        .statuses(Some(&mut opts))
        .with_context(|| format!("Failed to read status of worktree '{}'", path.display()))?;

    Ok(statuses
        .iter()
        .filter_map(|e| e.path().ok().map(String::from))
        .collect())
}

impl VcsProvider for GitRepository {
    fn current_workspace(&self) -> Result<Option<String>> {
        self.get_current_workspace()
    }

    fn default_workspace(&self) -> Result<Option<String>> {
        self.detect_main_workspace()
    }

    fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let current = self.get_current_workspace()?;
        let default = self.detect_main_workspace()?;

        let workspaces = self
            .repo
            .branches(Some(git2::BranchType::Local))
            .context("Failed to list workspaces")?;

        let mut result = Vec::new();
        for branch_result in workspaces {
            let (workspace, _) = branch_result?;
            if let Some(name) = workspace.name()? {
                result.push(WorkspaceInfo {
                    name: name.to_string(),
                    is_current: current.as_deref() == Some(name),
                    is_default: default.as_deref() == Some(name),
                });
            }
        }

        Ok(result)
    }

    fn create_workspace(&self, name: &str, base: Option<&str>) -> Result<()> {
        if self.workspace_exists(name)? {
            log::info!("VCS branch '{}' already exists, reusing", name);
            return Ok(());
        }

        // Resolve the base commit
        let base_commit = if let Some(base_name) = base {
            // Workspace identities are exact local branch names. Avoid Git's
            // DWIM revision lookup here: a tag with the same name as the
            // requested parent must never silently select a different commit.
            let base_branch = self
                .repo
                .find_branch(base_name, git2::BranchType::Local)
                .with_context(|| format!("Failed to find base workspace '{}'", base_name))?;
            base_branch
                .get()
                .peel_to_commit()
                .context("Base workspace reference is not a commit")?
        } else {
            // On unborn repos this auto-creates an initial empty commit.
            self.head_commit_or_init()?
        };

        self.repo
            .branch(name, &base_commit, false)
            .with_context(|| format!("Failed to create workspace '{}'", name))?;

        Ok(())
    }

    fn workspace_revision(&self, name: &str) -> Result<Option<String>> {
        match self.repo.find_branch(name, git2::BranchType::Local) {
            Ok(branch) => Ok(Some(branch.get().peel_to_commit()?.id().to_string())),
            Err(error) if error.code() == ErrorCode::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
    fn create_workspace_at(&self, name: &str, revision: &str) -> Result<()> {
        let commit = self.repo.find_commit(git2::Oid::from_str(revision)?)?;
        self.repo.branch(name, &commit, false)?;
        Ok(())
    }

    fn delete_workspace(&self, name: &str) -> Result<()> {
        let mut workspace = self
            .repo
            .find_branch(name, git2::BranchType::Local)
            .with_context(|| {
                format!(
                    "Workspace '{}' not found. Run 'devflow list' to see available workspaces.",
                    name
                )
            })?;
        workspace
            .delete()
            .with_context(|| format!("Failed to delete workspace '{}'", name))?;
        Ok(())
    }

    fn workspace_exists(&self, name: &str) -> Result<bool> {
        self.workspace_exists(name)
    }

    fn supports_worktrees(&self) -> bool {
        true
    }

    fn is_worktree(&self) -> bool {
        self.repo.is_worktree()
    }

    fn list_worktrees(&self) -> Result<Vec<WorktreeInfo>> {
        let mut result = Vec::new();

        // The repository may have been opened from a linked worktree. Always
        // inspect the primary checkout explicitly instead of labelling the
        // context checkout as main.
        result.push(WorktreeInfo {
            path: self.primary_root.clone(),
            workspace: Self::workspace_at(&self.primary_root)?,
            is_main: true,
        });

        // List linked worktrees
        let worktree_names = self.repo.worktrees().context("Failed to list worktrees")?;

        for wt_name in worktree_names.iter() {
            let name = wt_name?.context("Worktree metadata name is not UTF-8")?;

            {
                let wt = self.repo.find_worktree(name)?;
                let wt_path = wt
                    .path()
                    .canonicalize()
                    .unwrap_or_else(|_| wt.path().to_path_buf());

                // Resolve the worktree's workspace with the same guarded
                // HEAD parsing as the primary entry: a detached HEAD yields
                // None, never the literal pseudo-name "HEAD" — inventory
                // adoption would otherwise persist a phantom "HEAD"
                // workspace whenever a worktree is mid-rebase/bisect.
                let wt_branch = Self::workspace_at(&wt_path).ok().flatten();

                result.push(WorktreeInfo {
                    path: wt_path,
                    workspace: wt_branch,
                    is_main: false,
                });
            }
        }

        for cleanup in Cleanup::list(self.repo.commondir())? {
            if let Some(entry) = result
                .iter_mut()
                .find(|wt| super::paths_equal(&wt.path, &cleanup.original))
            {
                entry.workspace = cleanup.bookmark;
            } else {
                result.push(WorktreeInfo {
                    path: cleanup.original,
                    workspace: cleanup.bookmark,
                    is_main: false,
                });
            }
        }
        Ok(result)
    }

    fn create_worktree(&self, workspace: &str, path: &Path) -> Result<WorktreeCreateResult> {
        anyhow::ensure!(
            !Cleanup::list(self.repo.commondir())?
                .iter()
                .any(|cleanup| cleanup.bookmark.as_deref() == Some(workspace)
                    || super::paths_equal(&cleanup.original, path)),
            "Worktree has pending cleanup; finish removal before recreating it"
        );
        let parent = path.parent().context("worktree path has no parent")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create worktree parent '{}'", parent.display()))?;
        anyhow::ensure!(
            !path.exists(),
            "worktree destination '{}' already exists; inspect it before retrying",
            path.display()
        );
        // Old releases used a lossy worktree metadata name. Prune all stale
        // entries before selecting the collision-resistant name below.
        let _ = <Self as VcsProvider>::prune_worktrees(self);
        let wt_name = Self::worktree_name_for_branch(workspace);

        // If stale worktree metadata exists for this name (path removed on disk),
        // prune it first so creation can proceed.
        if let Ok(existing_wt) = self.repo.find_worktree(&wt_name) {
            let existing_path = existing_wt.path().to_path_buf();
            if !existing_path.exists() {
                log::warn!(
                    "Pruning stale worktree metadata '{}' at '{}'",
                    wt_name,
                    existing_path.display()
                );
                let mut prune_opts = WorktreePruneOptions::new();
                prune_opts.valid(true);
                prune_opts.working_tree(true);
                existing_wt.prune(Some(&mut prune_opts)).with_context(|| {
                    format!(
                        "Failed to prune stale worktree '{}' at '{}'",
                        wt_name,
                        existing_path.display()
                    )
                })?;
            }
        }

        // If the workspace doesn't exist yet, create it from HEAD so the
        // git2 worktree creation can reference it.
        // On unborn repos this auto-creates an initial empty commit.
        if !self.workspace_exists(workspace)? {
            let head_commit = self.head_commit_or_init()?;
            self.repo
                .branch(workspace, &head_commit, false)
                .with_context(|| format!("Failed to create workspace '{}'", workspace))?;
        }

        let branch_ref = self
            .repo
            .find_branch(workspace, git2::BranchType::Local)
            .with_context(|| format!("Workspace '{}' not found", workspace))?;
        let reference = branch_ref.into_reference();

        let mut opts = WorktreeAddOptions::new();
        opts.reference(Some(&reference));

        let metadata = self.repo.commondir().join("worktrees").join(&wt_name);
        let metadata_existed = metadata.exists();
        if let Err(error) = self.repo.worktree(&wt_name, path, Some(&opts)) {
            // libgit2 can leave an incomplete metadata directory. Remove only
            // metadata created by this attempt; retain checkout files for inspection.
            if !metadata_existed && metadata.exists() {
                std::fs::remove_dir_all(&metadata).with_context(|| {
                    format!("failed to clean incomplete metadata {}", metadata.display())
                })?;
            }
            return Err(error).with_context(|| {
                format!(
                    "Failed to create worktree '{}' at '{}'",
                    wt_name,
                    path.display()
                )
            });
        }

        Ok(WorktreeCreateResult::new())
    }

    fn remove_worktree(&self, path: &Path, force: bool) -> Result<()> {
        anyhow::ensure!(
            !super::paths_equal(path, &self.primary_root),
            "Cannot remove the primary worktree"
        );
        anyhow::ensure!(
            self.repo
                .workdir()
                .is_none_or(|current| !super::paths_equal(path, current)),
            "Cannot remove the current worktree"
        );
        let receipt = Cleanup::receipt(self.repo.commondir(), path);
        let _lease = crate::services::ownership::OperationLease::try_acquire(
            &receipt.with_extension("lock"),
        )?
        .context("Another worktree cleanup is active for this directory")?;
        let mut cleanup = if let Some(cleanup) = Cleanup::read(&receipt)? {
            anyhow::ensure!(
                super::paths_equal(path, &cleanup.original),
                "Cleanup receipt path mismatch"
            );
            cleanup
        } else {
            let names = self.repo.worktrees()?;
            let mut found = None;
            for name in names.iter().flatten().flatten() {
                let wt = self.repo.find_worktree(name)?;
                if super::paths_equal(wt.path(), path) {
                    found = Some((name.to_owned(), wt));
                    break;
                }
            }
            let (name, wt) =
                found.with_context(|| format!("No worktree found at path '{}'", path.display()))?;
            anyhow::ensure!(
                matches!(wt.is_locked()?, git2::WorktreeLockStatus::Unlocked),
                "Worktree is locked; unlock it before removal"
            );
            if !force {
                ensure_worktree_clean(path)?;
            }
            if !path.try_exists()? {
                let mut opts = WorktreePruneOptions::new();
                opts.valid(true).working_tree(false);
                wt.prune(Some(&mut opts))?;
                return Ok(());
            }
            let repo = Repository::open(path)?;
            let head = repo.head()?;
            let bookmark = head
                .is_branch()
                .then(|| head.shorthand().ok().map(str::to_owned))
                .flatten();
            let revision = head.peel_to_commit()?.id().to_string();
            let registration = self.repo.commondir().join("worktrees").join(&name);
            Cleanup::begin(
                &receipt,
                name,
                path,
                revision,
                bookmark,
                Some(&registration),
            )?
        };
        // Metadata remains discoverable until traversal has succeeded; the
        // receipt still identifies the staged directory if .git is gone.
        cleanup.verify_registration()?;
        cleanup.remove_directory(&receipt)?;
        cleanup.verify_registration()?;
        match self.repo.find_worktree(&cleanup.internal) {
            Ok(wt) => {
                let mut opts = WorktreePruneOptions::new();
                opts.valid(true).working_tree(false);
                wt.prune(Some(&mut opts))
                    .context("Cannot remove worktree registration; cleanup receipt retained")?;
            }
            Err(error)
                if error.code() == ErrorCode::NotFound
                    && !self
                        .repo
                        .commondir()
                        .join("worktrees")
                        .join(&cleanup.internal)
                        .try_exists()? => {}
            Err(error) => return Err(error.into()),
        }
        std::fs::remove_file(receipt)?;
        Ok(())
    }

    fn worktree_is_locked(&self, path: &Path) -> Result<bool> {
        for name in self.repo.worktrees()?.iter().flatten().flatten() {
            let wt = self.repo.find_worktree(name)?;
            if super::paths_equal(wt.path(), path) {
                return Ok(!matches!(
                    wt.is_locked()?,
                    git2::WorktreeLockStatus::Unlocked
                ));
            }
        }
        Ok(false)
    }

    fn worktree_is_dirty(&self, path: &Path) -> Result<bool> {
        if Cleanup::read(&Cleanup::receipt(self.repo.commondir(), path))?.is_some()
            || !path.try_exists()?
        {
            return Ok(false);
        }
        Ok(!worktree_changes(path)?.is_empty())
    }

    fn worktree_path(&self, workspace: &str) -> Result<Option<PathBuf>> {
        if let Some(cleanup) = Cleanup::list(self.repo.commondir())?
            .into_iter()
            .find(|record| record.bookmark.as_deref() == Some(workspace))
        {
            return Ok(Some(cleanup.original));
        }
        let worktree_names = self.repo.worktrees().context("Failed to list worktrees")?;

        for wt_name in worktree_names.iter() {
            let Ok(Some(name)) = wt_name else { continue };

            if let Ok(wt) = self.repo.find_worktree(name) {
                let wt_path = wt.path().to_path_buf();
                // Check if this worktree has the target workspace checked out
                if let Ok(wt_repo) = Repository::open(&wt_path) {
                    if let Ok(head) = wt_repo.head() {
                        if head.shorthand().ok() == Some(workspace) {
                            return Ok(Some(wt_path));
                        }
                    }
                }
            }
        }

        // Opening from a linked worktree means `get_current_workspace()` is
        // the linked branch, not the primary checkout's branch.
        if Self::workspace_at(&self.primary_root)?.as_deref() == Some(workspace) {
            return Ok(Some(self.primary_root.clone()));
        }

        Ok(None)
    }

    fn main_worktree_dir(&self) -> Option<PathBuf> {
        Some(self.primary_root.clone())
    }

    fn install_hooks(&self) -> Result<()> {
        super::hooks::install(
            &self.repo,
            &[
                ("post-checkout", self.generate_hook_script()),
                ("pre-commit", self.generate_pre_commit_script()),
            ],
        )
    }

    fn uninstall_hooks(&self) -> Result<()> {
        super::hooks::uninstall(&self.repo, &["post-checkout", "pre-commit"])
    }

    fn is_devflow_hook(&self, hook_path: &Path) -> Result<bool> {
        super::hooks::is_owned(hook_path)
    }

    fn hook_paths(&self) -> Vec<PathBuf> {
        super::hooks::directory(&self.repo)
            .map(|dir| {
                ["post-checkout", "pre-commit"]
                    .iter()
                    .map(|name| dir.join(name))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn provider_name(&self) -> &'static str {
        "git"
    }

    fn list_ignored_entries(&self) -> Result<Vec<PathBuf>> {
        let mut opts = git2::StatusOptions::new();
        opts.include_ignored(true)
            .include_untracked(false)
            .exclude_submodules(true)
            // Don't recurse into ignored dirs — we want the directory itself,
            // not every file inside node_modules/ or .venv/.
            .recurse_ignored_dirs(false);

        let statuses = self
            .repo
            .statuses(Some(&mut opts))
            .context("Failed to enumerate git statuses for ignored entries")?;

        let mut ignored = Vec::new();

        for entry in statuses.iter() {
            if entry.status().contains(git2::Status::IGNORED) {
                if let Ok(path_str) = entry.path() {
                    // git2 may append '/' for directories
                    let cleaned = path_str.trim_end_matches('/');
                    ignored.push(PathBuf::from(cleaned));
                }
            }
        }

        Ok(ignored)
    }

    fn ensure_initial_commit(&self) -> Result<()> {
        // If we can resolve HEAD, the repo already has commits.
        if self.repo.head().is_ok() {
            return Ok(());
        }
        // Unborn HEAD — create the initial empty commit.
        self.create_initial_commit()?;
        Ok(())
    }

    fn prune_worktrees(&self) -> Result<()> {
        // Mirror `git worktree prune`: drop admin entries whose working
        // directory no longer exists (or that are otherwise invalid).
        let names = self
            .repo
            .worktrees()
            .context("Failed to list worktrees for pruning")?;
        for name in names.iter().flatten().flatten() {
            let Ok(wt) = self.repo.find_worktree(name) else {
                continue;
            };
            if Cleanup::read(&Cleanup::receipt(self.repo.commondir(), wt.path()))?.is_some() {
                continue;
            }
            if wt.validate().is_ok() && wt.path().exists() {
                continue;
            }
            let mut opts = git2::WorktreePruneOptions::new();
            opts.working_tree(true);
            if let Err(e) = wt.prune(Some(&mut opts)) {
                log::debug!("Failed to prune worktree '{}': {}", name, e);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_worktree_name_for_branch() {
        assert_eq!(GitRepository::worktree_name_for_branch("main"), "main");
        assert_ne!(
            GitRepository::worktree_name_for_branch("feature/auth"),
            GitRepository::worktree_name_for_branch("feature-auth")
        );
    }

    #[test]
    fn colliding_legacy_names_can_have_distinct_worktrees() {
        let (_tmp, root, repo) = repo_with_commit();
        let slash_path = root.join("slash-worktree");
        let dash_path = root.join("dash-worktree");

        repo.create_worktree("feature/auth", &slash_path).unwrap();
        repo.create_worktree("feature-auth", &dash_path).unwrap();

        assert_eq!(
            repo.worktree_path("feature/auth").unwrap().as_deref(),
            Some(slash_path.as_path())
        );
        assert_eq!(
            repo.worktree_path("feature-auth").unwrap().as_deref(),
            Some(dash_path.as_path())
        );
    }

    #[test]
    fn provider_opened_from_linked_worktree_keeps_primary_checkout_identity() {
        let (_tmp, root, repo) = repo_with_commit();
        let wt_path = root.join("linked-feature");
        repo.create_worktree("feature/linked", &wt_path).unwrap();

        let linked = GitRepository::new(&wt_path).unwrap();

        assert_eq!(
            linked.current_workspace().unwrap().as_deref(),
            Some("feature/linked"),
            "the command context remains the linked checkout"
        );
        assert_eq!(linked.get_repo_root(), root.as_path());
        assert_eq!(linked.main_worktree_dir().as_deref(), Some(root.as_path()));
        assert_eq!(
            linked.worktree_path("main").unwrap().as_deref(),
            Some(root.as_path()),
            "switching to the default workspace must resolve the primary checkout"
        );

        let worktrees = linked.list_worktrees().unwrap();
        let primary = worktrees.iter().find(|worktree| worktree.is_main).unwrap();
        assert_eq!(primary.path, root);
        assert_eq!(primary.workspace.as_deref(), Some("main"));

        let feature = worktrees
            .iter()
            .find(|worktree| worktree.workspace.as_deref() == Some("feature/linked"))
            .unwrap();
        assert_eq!(feature.path, wt_path.canonicalize().unwrap());
        assert!(!feature.is_main);

        let sibling_path = root.join("linked-sibling");
        linked
            .create_worktree("feature/sibling", &sibling_path)
            .expect("a provider opened from a linked checkout can create sibling worktrees");
        assert_eq!(
            linked.worktree_path("feature/sibling").unwrap().as_deref(),
            Some(sibling_path.as_path())
        );
    }

    #[test]
    fn primary_checkout_branch_is_the_default_workspace() {
        let (_tmp, _root, repo) = repo_with_commit();
        repo.create_workspace("feature/primary", Some("main"))
            .unwrap();
        repo.repo.set_head("refs/heads/feature/primary").unwrap();

        // `main` still exists, but the primary checkout is intentionally on a
        // different raw branch. Devflow must not create a second linked
        // `main` and pretend that it is the root workspace.
        assert!(repo.workspace_exists("main").unwrap());
        assert_eq!(
            repo.default_workspace().unwrap().as_deref(),
            Some("feature/primary")
        );
    }

    #[test]
    fn workspace_parent_resolution_uses_exact_local_branch_when_tag_collides() {
        let (_tmp, root, repo) = repo_with_commit();
        repo.create_workspace("release", Some("main")).unwrap();
        let release_target = repo
            .repo
            .find_branch("release", git2::BranchType::Local)
            .unwrap()
            .get()
            .target()
            .unwrap();

        // Advance main and create a tag with the same raw name as the parent
        // workspace. DWIM revision parsing may prefer or reject this tag;
        // devflow must always use refs/heads/release.
        std::fs::write(root.join("later.txt"), "later").unwrap();
        let mut index = repo.repo.index().unwrap();
        index.add_path(Path::new("later.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("t", "t@t").unwrap();
        let parent = repo.repo.head().unwrap().peel_to_commit().unwrap();
        let later = repo
            .repo
            .commit(Some("HEAD"), &sig, &sig, "later", &tree, &[&parent])
            .unwrap();
        let later = repo.repo.find_object(later, None).unwrap();
        repo.repo.tag_lightweight("release", &later, false).unwrap();

        repo.create_workspace("child", Some("release")).unwrap();
        let child_target = repo
            .repo
            .find_branch("child", git2::BranchType::Local)
            .unwrap()
            .get()
            .target()
            .unwrap();
        assert_eq!(child_target, release_target);
        assert_ne!(child_target, later.id());
    }

    #[test]
    fn unborn_primary_checkout_retains_its_symbolic_workspace_name() {
        let tmp = tempfile::tempdir().unwrap();
        let raw = Repository::init(tmp.path()).unwrap();
        raw.set_head("refs/heads/feature/unborn").unwrap();
        drop(raw);

        let repo = GitRepository::new(tmp.path()).unwrap();
        assert_eq!(
            repo.default_workspace().unwrap().as_deref(),
            Some("feature/unborn")
        );
    }

    #[test]
    fn detached_primary_checkout_has_no_workspace_identity() {
        let (_tmp, _root, repo) = repo_with_commit();
        let head = repo.repo.head().unwrap().target().unwrap();
        repo.repo.set_head_detached(head).unwrap();

        assert_eq!(repo.current_workspace().unwrap(), None);
        assert_eq!(repo.default_workspace().unwrap(), None);
    }

    /// Build a repo with one commit so worktrees can be created.
    /// Returns the canonicalized root (macOS tempdirs are symlinked) so
    /// paths compare equal with what git2 reports.
    fn repo_with_commit() -> (tempfile::TempDir, PathBuf, GitRepository) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let repo = GitRepository::init(&root).unwrap();
        repo.ensure_initial_commit().unwrap();
        (tmp, root, repo)
    }

    #[test]
    fn test_remove_worktree_refuses_dirty_without_force() {
        let (_tmp, root, repo) = repo_with_commit();
        let wt_path = root.join("wt-dirty");
        repo.create_worktree("feature/dirty", &wt_path).unwrap();

        // An untracked (non-ignored) file must block removal
        std::fs::write(wt_path.join("work-in-progress.txt"), "precious").unwrap();

        let err = repo.remove_worktree(&wt_path, false).unwrap_err();
        assert!(
            err.to_string().contains("uncommitted change"),
            "unexpected error: {err}"
        );
        assert!(wt_path.exists(), "worktree must not be deleted");

        // Force discards it
        repo.remove_worktree(&wt_path, true).unwrap();
        assert!(!wt_path.exists());
    }

    #[test]
    fn test_remove_worktree_allows_clean_and_ignored() {
        let (_tmp, root, repo) = repo_with_commit();

        // Commit a .gitignore so ignored files exist in the worktree
        std::fs::write(root.join(".gitignore"), ".env.local\n").unwrap();
        let r = git2::Repository::open(&root).unwrap();
        let mut index = r.index().unwrap();
        index.add_path(Path::new(".gitignore")).unwrap();
        index.write().unwrap();
        let tree = r.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("t", "t@t").unwrap();
        let parent = r.head().unwrap().peel_to_commit().unwrap();
        r.commit(Some("HEAD"), &sig, &sig, "ignore", &tree, &[&parent])
            .unwrap();

        let wt_path = root.join("wt-clean");
        repo.create_worktree("feature/clean", &wt_path).unwrap();

        // A gitignored file (e.g. devflow-copied .env.local) must NOT block
        std::fs::write(wt_path.join(".env.local"), "DATABASE_URL=x").unwrap();

        repo.remove_worktree(&wt_path, false).unwrap();
        assert!(!wt_path.exists());
    }
}
