use anyhow::{Context, Result};
use git2::{ErrorCode, Repository, WorktreeAddOptions, WorktreeLockStatus, WorktreePruneOptions};
use std::fs;
use std::path::{Path, PathBuf};

use super::{VcsProvider, WorkspaceInfo, WorktreeCreateResult, WorktreeInfo};

pub struct GitRepository {
    repo: Repository,
}

impl GitRepository {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let repo = Repository::open(path).context("Failed to open Git repository")?;

        Ok(GitRepository { repo })
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

        let git_repo = GitRepository { repo };

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
            Ok(head) => {
                if let Some(workspace_name) = head.shorthand() {
                    Ok(Some(workspace_name.to_string()))
                } else {
                    Ok(None)
                }
            }
            Err(e) if e.code() == ErrorCode::UnbornBranch => {
                // HEAD exists but points to a workspace with no commits.
                // Read the symbolic target of HEAD to get the workspace name.
                match self.repo.find_reference("HEAD") {
                    Ok(head_ref) => {
                        if let Some(target) = head_ref.symbolic_target() {
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
        // Strategy 1: Check for remote's default workspace (most reliable)
        if let Some(main_workspace) = self.get_remote_default_workspace()? {
            log::debug!("Found remote default workspace: {}", main_workspace);
            return Ok(Some(main_workspace));
        }

        // Strategy 2: Check common main workspace names that exist locally
        let common_main_workspacees = vec!["main", "master", "develop", "development"];
        for workspace_name in common_main_workspacees {
            if self.workspace_exists(workspace_name)? {
                log::debug!("Found local main workspace: {}", workspace_name);
                return Ok(Some(workspace_name.to_string()));
            }
        }

        // Strategy 3: Find the local workspace that tracks a remote main workspace
        if let Some(main_workspace) = self.find_local_tracking_main_workspace()? {
            log::debug!(
                "Found local workspace tracking remote main: {}",
                main_workspace
            );
            return Ok(Some(main_workspace));
        }

        // Strategy 4: Use current workspace as last resort (original behavior)
        if let Some(current_workspace) = self.get_current_workspace()? {
            log::debug!(
                "Using current workspace as fallback main: {}",
                current_workspace
            );
            return Ok(Some(current_workspace));
        }

        Ok(None)
    }

    fn get_remote_default_workspace(&self) -> Result<Option<String>> {
        // Try to get the default workspace from the remote
        let mut found_default = None;

        // Get all remotes
        let remotes = self.repo.remotes()?;

        // Check origin first, then others
        let remote_names: Vec<&str> = if remotes.iter().any(|r| r == Some("origin")) {
            let mut names = vec!["origin"];
            names.extend(remotes.iter().flatten().filter(|&r| r != "origin"));
            names
        } else {
            remotes.iter().flatten().collect()
        };

        for remote_name in remote_names {
            if let Ok(_remote) = self.repo.find_remote(remote_name) {
                // Look for HEAD reference in remote
                let head_ref = format!("refs/remotes/{}/HEAD", remote_name);
                if let Ok(reference) = self.repo.find_reference(&head_ref) {
                    if let Some(target) = reference.symbolic_target() {
                        // Extract workspace name from refs/remotes/origin/main -> main
                        let prefix = format!("refs/remotes/{}/", remote_name);
                        if target.starts_with(&prefix) {
                            let workspace_name = target.strip_prefix(&prefix).unwrap();
                            found_default = Some(workspace_name.to_string());
                            break;
                        }
                    }
                }
            }
        }

        Ok(found_default)
    }

    fn find_local_tracking_main_workspace(&self) -> Result<Option<String>> {
        let workspaces = self.repo.branches(Some(git2::BranchType::Local))?;

        for branch_result in workspaces {
            let (workspace, _) = branch_result?;
            if let Some(workspace_name) = workspace.name()? {
                // Check if this workspace tracks a remote main/master workspace
                if let Ok(upstream) = workspace.upstream() {
                    if let Some(upstream_name) = upstream.name()? {
                        // Check if upstream is a main workspace (contains main, master, etc.)
                        let upstream_lower = upstream_name.to_lowercase();
                        if upstream_lower.contains("main") || upstream_lower.contains("master") {
                            return Ok(Some(workspace_name.to_string()));
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    #[allow(dead_code)]
    pub fn get_all_branches(&self) -> Result<Vec<String>> {
        let workspaces = self
            .repo
            .branches(Some(git2::BranchType::Local))
            .context("Failed to get workspaces")?;

        let mut workspace_names = Vec::new();
        for workspace in workspaces {
            let (workspace, _) = workspace.context("Failed to get workspace")?;
            if let Some(name) = workspace.name()? {
                workspace_names.push(name.to_string());
            }
        }

        Ok(workspace_names)
    }

    fn generate_hook_script(&self) -> String {
        r#"#!/bin/sh
# devflow auto-generated hook
# This hook automatically creates service workspaces when switching Git workspaces

# For post-checkout hook, check if this is a workspace checkout (not file checkout)
# Parameters: $1=previous HEAD, $2=new HEAD, $3=checkout type (1=workspace, 0=file)
if [ "$3" = "0" ]; then
    # This is a file checkout, not a workspace checkout - skip devflow execution
    exit 0
fi

# Detect if we're in a worktree (git-dir differs from common-dir)
GIT_DIR=$(git rev-parse --git-dir 2>/dev/null)
GIT_COMMON_DIR=$(git rev-parse --git-common-dir 2>/dev/null)

if [ "$GIT_DIR" != "$GIT_COMMON_DIR" ]; then
    # Worktree: resolve main worktree root from common dir
    MAIN_WORKTREE=$(cd "$GIT_COMMON_DIR/.." && pwd)
    if command -v devflow >/dev/null 2>&1; then
        devflow git-hook --worktree --main-worktree-dir "$MAIN_WORKTREE"
    fi
    exit 0
fi

# Regular checkout: skip if same workspace
PREV_BRANCH=$(git reflog | awk 'NR==1{ print $6; exit }')
NEW_BRANCH=$(git reflog | awk 'NR==1{ print $8; exit }')

if [ "$PREV_BRANCH" = "$NEW_BRANCH" ]; then
    # This is the same workspace checkout - skip devflow execution
    exit 0
fi

# Check if devflow is available
if command -v devflow >/dev/null 2>&1; then
    # Run devflow git-hook command to handle workspace creation
    devflow git-hook
else
    echo "devflow not found in PATH, skipping service workspace creation"
fi
"#
        .to_string()
    }

    fn generate_pre_commit_script(&self) -> String {
        r#"#!/bin/sh
# devflow auto-generated hook
# This hook runs devflow pre-commit lifecycle hooks before each commit.

if command -v devflow >/dev/null 2>&1; then
    devflow hook run pre-commit
    exit $?
fi
"#
        .to_string()
    }

    fn generate_post_rewrite_script(&self) -> String {
        r#"#!/bin/sh
# devflow auto-generated hook
# This hook runs after git rebase or git commit --amend.
# $1 is the cause: "rebase" or "amend"

CAUSE="$1"

if command -v devflow >/dev/null 2>&1; then
    devflow hook run "post-rewrite"
fi
"#
        .to_string()
    }

    #[allow(dead_code)]
    pub fn get_repo_root(&self) -> &Path {
        self.repo.workdir().unwrap_or_else(|| self.repo.path())
    }

    #[allow(dead_code)]
    pub fn is_worktree(&self) -> bool {
        self.repo.is_worktree()
    }

    #[allow(dead_code)]
    pub fn get_main_worktree_dir(&self) -> Option<PathBuf> {
        if !self.repo.is_worktree() {
            return None;
        }
        self.repo.commondir().parent().map(|p| p.to_path_buf())
    }

    /// Sanitize a workspace name into a valid worktree name for git.
    /// Replaces `/` with `-` since worktree names are used as directory components.
    fn worktree_name_for_branch(workspace: &str) -> String {
        workspace.replace('/', "-")
    }
}

// ─── VcsProvider implementation ────────────────────────────────────────────

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
            let obj = self
                .repo
                .revparse_single(base_name)
                .with_context(|| format!("Failed to find base workspace '{}'", base_name))?;
            obj.peel_to_commit()
                .context("Base reference is not a commit")?
        } else {
            // On unborn repos this auto-creates an initial empty commit.
            self.head_commit_or_init()?
        };

        self.repo
            .branch(name, &base_commit, false)
            .with_context(|| format!("Failed to create workspace '{}'", name))?;

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

    fn checkout_workspace(&self, name: &str) -> Result<()> {
        let workspace = self
            .repo
            .find_branch(name, git2::BranchType::Local)
            .with_context(|| {
                format!(
                    "Workspace '{}' not found. Run 'devflow list' to see available workspaces.",
                    name
                )
            })?;
        let reference = workspace.into_reference();
        let commit = reference
            .peel_to_commit()
            .context("Workspace does not point to a commit")?;
        let tree = commit.tree().context("Failed to get tree from commit")?;

        self.repo
            .checkout_tree(
                tree.as_object(),
                Some(git2::build::CheckoutBuilder::new().safe()),
            )
            .with_context(|| format!("Failed to checkout tree for workspace '{}'", name))?;

        let refname = reference
            .name()
            .ok_or_else(|| anyhow::anyhow!("Workspace reference has invalid UTF-8 name"))?;
        self.repo
            .set_head(refname)
            .with_context(|| format!("Failed to set HEAD to workspace '{}'", name))?;

        Ok(())
    }

    fn supports_worktrees(&self) -> bool {
        true
    }

    fn is_worktree(&self) -> bool {
        self.repo.is_worktree()
    }

    fn list_worktrees(&self) -> Result<Vec<WorktreeInfo>> {
        let mut result = Vec::new();

        // Add the main worktree
        let current_workspace = self.get_current_workspace()?;
        let repo_root = self
            .repo
            .workdir()
            .unwrap_or_else(|| self.repo.path())
            .to_path_buf();

        result.push(WorktreeInfo {
            path: repo_root,
            workspace: current_workspace,
            is_main: true,
            is_locked: false,
        });

        // List linked worktrees
        let worktree_names = self.repo.worktrees().context("Failed to list worktrees")?;

        for wt_name in worktree_names.iter() {
            let Some(name) = wt_name else { continue };

            if let Ok(wt) = self.repo.find_worktree(name) {
                let wt_path = wt.path().to_path_buf();

                // Get workspace for this worktree by opening the repo at that path
                let wt_branch = if let Ok(wt_repo) = Repository::open(&wt_path) {
                    if let Ok(head) = wt_repo.head() {
                        head.shorthand().map(|s| s.to_string())
                    } else {
                        None
                    }
                } else {
                    None
                };

                let is_locked = matches!(wt.is_locked(), Ok(WorktreeLockStatus::Locked(_)));

                result.push(WorktreeInfo {
                    path: wt_path,
                    workspace: wt_branch,
                    is_main: false,
                    is_locked,
                });
            }
        }

        Ok(result)
    }

    fn create_worktree(&self, workspace: &str, path: &Path) -> Result<WorktreeCreateResult> {
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

        self.repo
            .worktree(&wt_name, path, Some(&opts))
            .with_context(|| {
                format!(
                    "Failed to create worktree '{}' at '{}'",
                    wt_name,
                    path.display()
                )
            })?;

        Ok(WorktreeCreateResult::new())
    }

    fn remove_worktree(&self, path: &Path) -> Result<()> {
        // Find the worktree by path
        let worktree_names = self.repo.worktrees().context("Failed to list worktrees")?;

        for wt_name in worktree_names.iter() {
            let Some(name) = wt_name else { continue };

            if let Ok(wt) = self.repo.find_worktree(name) {
                if wt.path() == path {
                    // Prune the worktree (removes git metadata + working tree)
                    let mut prune_opts = WorktreePruneOptions::new();
                    prune_opts.valid(true);
                    prune_opts.working_tree(true);
                    wt.prune(Some(&mut prune_opts)).with_context(|| {
                        format!("Failed to prune worktree at '{}'", path.display())
                    })?;
                    return Ok(());
                }
            }
        }

        anyhow::bail!("No worktree found at path '{}'", path.display());
    }

    fn worktree_path(&self, workspace: &str) -> Result<Option<PathBuf>> {
        let worktree_names = self.repo.worktrees().context("Failed to list worktrees")?;

        for wt_name in worktree_names.iter() {
            let Some(name) = wt_name else { continue };

            if let Ok(wt) = self.repo.find_worktree(name) {
                let wt_path = wt.path().to_path_buf();
                // Check if this worktree has the target workspace checked out
                if let Ok(wt_repo) = Repository::open(&wt_path) {
                    if let Ok(head) = wt_repo.head() {
                        if head.shorthand() == Some(workspace) {
                            return Ok(Some(wt_path));
                        }
                    }
                }
            }
        }

        // Also check if the main worktree has this workspace
        if let Some(current) = self.get_current_workspace()? {
            if current == workspace {
                let main_path = self
                    .repo
                    .workdir()
                    .unwrap_or_else(|| self.repo.path())
                    .to_path_buf();
                return Ok(Some(main_path));
            }
        }

        Ok(None)
    }

    fn main_worktree_dir(&self) -> Option<PathBuf> {
        if self.repo.is_worktree() {
            self.repo.commondir().parent().map(|p| p.to_path_buf())
        } else {
            self.repo.workdir().map(|p| p.to_path_buf())
        }
    }

    fn install_hooks(&self) -> Result<()> {
        let hooks_dir = self.repo.path().join("hooks");
        fs::create_dir_all(&hooks_dir).context("Failed to create hooks directory")?;

        let hook_script = self.generate_hook_script();
        let pre_commit_script = self.generate_pre_commit_script();
        let post_rewrite_script = self.generate_post_rewrite_script();

        // Install post-checkout hook
        let post_checkout_hook = hooks_dir.join("post-checkout");
        fs::write(&post_checkout_hook, &hook_script)
            .context("Failed to write post-checkout hook")?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&post_checkout_hook)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&post_checkout_hook, perms)
                .context("Failed to set hook permissions")?;
        }

        // Install post-merge hook
        let post_merge_hook = hooks_dir.join("post-merge");
        fs::write(&post_merge_hook, &hook_script).context("Failed to write post-merge hook")?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&post_merge_hook)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&post_merge_hook, perms)
                .context("Failed to set hook permissions")?;
        }

        // Install pre-commit hook
        let pre_commit_hook = hooks_dir.join("pre-commit");
        fs::write(&pre_commit_hook, &pre_commit_script)
            .context("Failed to write pre-commit hook")?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&pre_commit_hook)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&pre_commit_hook, perms)
                .context("Failed to set hook permissions")?;
        }

        // Install post-rewrite hook (runs after rebase/amend)
        let post_rewrite_hook = hooks_dir.join("post-rewrite");
        fs::write(&post_rewrite_hook, &post_rewrite_script)
            .context("Failed to write post-rewrite hook")?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&post_rewrite_hook)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&post_rewrite_hook, perms)
                .context("Failed to set hook permissions")?;
        }

        Ok(())
    }

    fn uninstall_hooks(&self) -> Result<()> {
        let hooks_dir = self.repo.path().join("hooks");

        let post_checkout_hook = hooks_dir.join("post-checkout");
        if post_checkout_hook.exists() && self.is_devflow_hook(&post_checkout_hook)? {
            fs::remove_file(&post_checkout_hook).context("Failed to remove post-checkout hook")?;
        }

        let post_merge_hook = hooks_dir.join("post-merge");
        if post_merge_hook.exists() && self.is_devflow_hook(&post_merge_hook)? {
            fs::remove_file(&post_merge_hook).context("Failed to remove post-merge hook")?;
        }

        let pre_commit_hook = hooks_dir.join("pre-commit");
        if pre_commit_hook.exists() && self.is_devflow_hook(&pre_commit_hook)? {
            fs::remove_file(&pre_commit_hook).context("Failed to remove pre-commit hook")?;
        }

        let post_rewrite_hook = hooks_dir.join("post-rewrite");
        if post_rewrite_hook.exists() && self.is_devflow_hook(&post_rewrite_hook)? {
            fs::remove_file(&post_rewrite_hook).context("Failed to remove post-rewrite hook")?;
        }

        Ok(())
    }

    fn is_devflow_hook(&self, hook_path: &Path) -> Result<bool> {
        if !hook_path.exists() {
            return Ok(false);
        }

        let content = fs::read_to_string(hook_path).context("Failed to read hook file")?;

        Ok(content.contains("devflow auto-generated hook"))
    }

    fn provider_name(&self) -> &'static str {
        "git"
    }

    fn repo_root(&self) -> &Path {
        self.get_repo_root()
    }

    fn list_ignored_files(&self) -> Result<Vec<PathBuf>> {
        let mut opts = git2::StatusOptions::new();
        opts.include_ignored(true)
            .include_untracked(false)
            .exclude_submodules(true)
            // Only show files, not directories
            .recurse_ignored_dirs(true);

        let statuses = self
            .repo
            .statuses(Some(&mut opts))
            .context("Failed to enumerate git statuses")?;

        let root = self.get_repo_root().to_path_buf();
        let mut ignored = Vec::new();

        for entry in statuses.iter() {
            if entry.status().contains(git2::Status::IGNORED) {
                if let Some(path_str) = entry.path() {
                    let full_path = root.join(path_str);
                    // Only include actual files (not directories)
                    if full_path.is_file() {
                        ignored.push(PathBuf::from(path_str));
                    }
                }
            }
        }

        Ok(ignored)
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
                if let Some(path_str) = entry.path() {
                    // git2 may append '/' for directories
                    let cleaned = path_str.trim_end_matches('/');
                    ignored.push(PathBuf::from(cleaned));
                }
            }
        }

        Ok(ignored)
    }

    fn staged_diff(&self) -> Result<String> {
        // Use git CLI for diff output — git2's diff API is verbose to format.
        let root = self.get_repo_root();
        let output = std::process::Command::new("git")
            .args(["diff", "--cached"])
            .current_dir(root)
            .output()
            .context("Failed to run 'git diff --cached'")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git diff --cached failed: {}", stderr);
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    fn staged_summary(&self) -> Result<String> {
        let root = self.get_repo_root();
        let output = std::process::Command::new("git")
            .args(["diff", "--cached", "--stat"])
            .current_dir(root)
            .output()
            .context("Failed to run 'git diff --cached --stat'")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git diff --cached --stat failed: {}", stderr);
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    fn has_staged_changes(&self) -> Result<bool> {
        let statuses = self
            .repo
            .statuses(None)
            .context("Failed to get git status")?;

        for entry in statuses.iter() {
            let s = entry.status();
            if s.intersects(
                git2::Status::INDEX_NEW
                    | git2::Status::INDEX_MODIFIED
                    | git2::Status::INDEX_DELETED
                    | git2::Status::INDEX_RENAMED
                    | git2::Status::INDEX_TYPECHANGE,
            ) {
                return Ok(true);
            }
        }

        Ok(false)
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

    fn commit(&self, message: &str) -> Result<()> {
        // Use git CLI for commit — handles hooks, GPG signing, etc.
        let root = self.get_repo_root();
        let output = std::process::Command::new("git")
            .args(["commit", "-m", message])
            .current_dir(root)
            .output()
            .context("Failed to run 'git commit'")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            anyhow::bail!("git commit failed: {}{}", stdout, stderr);
        }

        Ok(())
    }

    fn merge_branch(&self, source: &str) -> Result<()> {
        let annotated = self
            .repo
            .find_branch(source, git2::BranchType::Local)
            .with_context(|| format!("Workspace '{}' not found", source))?
            .into_reference()
            .peel_to_commit()
            .context("Source workspace does not point to a commit")?;
        let annotated_commit = self
            .repo
            .find_annotated_commit(annotated.id())
            .context("Failed to create annotated commit for merge")?;

        let (analysis, _) = self
            .repo
            .merge_analysis(&[&annotated_commit])
            .context("Failed to perform merge analysis")?;

        if analysis.is_up_to_date() {
            return Ok(());
        }

        if analysis.is_fast_forward() {
            // Fast-forward: just move HEAD to the target commit
            let refname = format!(
                "refs/heads/{}",
                self.current_workspace()?.unwrap_or_default()
            );
            self.repo
                .find_reference(&refname)
                .and_then(|mut r| r.set_target(annotated.id(), "devflow: fast-forward merge"))
                .with_context(|| format!("Failed to fast-forward to '{}'", source))?;
            self.repo
                .checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
                .context("Failed to checkout after fast-forward")?;
            return Ok(());
        }

        if analysis.is_normal() {
            // Normal merge
            self.repo
                .merge(&[&annotated_commit], None, None)
                .with_context(|| format!("Failed to merge '{}'", source))?;

            // Check for conflicts
            let mut index = self.repo.index().context("Failed to get index")?;
            if index.has_conflicts() {
                anyhow::bail!("Merge conflicts detected. Resolve conflicts and commit manually.");
            }

            // Create merge commit
            let sig = self
                .repo
                .signature()
                .context("Failed to get default signature")?;
            let tree_oid = index.write_tree().context("Failed to write tree")?;
            let tree = self.repo.find_tree(tree_oid)?;
            let head_commit = self
                .repo
                .head()?
                .peel_to_commit()
                .context("HEAD is not a commit")?;
            self.repo
                .commit(
                    Some("HEAD"),
                    &sig,
                    &sig,
                    &format!("Merge workspace '{}' into HEAD", source),
                    &tree,
                    &[&head_commit, &annotated],
                )
                .context("Failed to create merge commit")?;

            self.repo.cleanup_state()?;
            return Ok(());
        }

        anyhow::bail!(
            "Cannot merge '{}': merge analysis returned unexpected result",
            source
        );
    }

    fn detach_head(&self) -> Result<()> {
        let head = self.repo.head().context("Failed to get HEAD")?;
        let commit = head
            .peel_to_commit()
            .context("HEAD does not point to a commit")?;
        self.repo
            .set_head_detached(commit.id())
            .context("Failed to detach HEAD")?;
        Ok(())
    }

    fn merge_base(&self, a: &str, b: &str) -> Result<String> {
        let a_oid = self
            .repo
            .find_branch(a, git2::BranchType::Local)
            .with_context(|| format!("Workspace '{}' not found", a))?
            .into_reference()
            .peel_to_commit()
            .context("Failed to resolve commit for workspace")?
            .id();

        let b_oid = self
            .repo
            .find_branch(b, git2::BranchType::Local)
            .with_context(|| format!("Workspace '{}' not found", b))?
            .into_reference()
            .peel_to_commit()
            .context("Failed to resolve commit for workspace")?
            .id();

        let base = self
            .repo
            .merge_base(a_oid, b_oid)
            .context("Failed to find merge base")?;

        Ok(base.to_string())
    }

    fn changed_files_since(
        &self,
        base_commit: &str,
        workspace: &str,
    ) -> Result<Vec<std::path::PathBuf>> {
        let base_oid = git2::Oid::from_str(base_commit)
            .with_context(|| format!("Invalid commit SHA: {}", base_commit))?;
        let base_tree = self
            .repo
            .find_commit(base_oid)
            .context("Failed to find base commit")?
            .tree()
            .context("Failed to get tree for base commit")?;

        let head_tree = self
            .repo
            .find_branch(workspace, git2::BranchType::Local)
            .with_context(|| format!("Workspace '{}' not found", workspace))?
            .into_reference()
            .peel_to_commit()
            .context("Failed to resolve commit")?
            .tree()
            .context("Failed to get tree")?;

        let diff = self
            .repo
            .diff_tree_to_tree(Some(&base_tree), Some(&head_tree), None)
            .context("Failed to diff trees")?;

        let mut files = Vec::new();
        for delta in diff.deltas() {
            if let Some(path) = delta.new_file().path() {
                files.push(path.to_path_buf());
            }
        }

        Ok(files)
    }

    fn rebase(&self, onto: &str) -> Result<crate::merge::RebaseResult> {
        let onto_branch = self
            .repo
            .find_branch(onto, git2::BranchType::Local)
            .with_context(|| format!("Target workspace '{}' not found", onto))?;
        let onto_annotated = self
            .repo
            .find_annotated_commit(
                onto_branch
                    .into_reference()
                    .peel_to_commit()
                    .context("Failed to resolve onto commit")?
                    .id(),
            )
            .context("Failed to create annotated commit for rebase target")?;

        let head = self
            .repo
            .head()
            .context("Failed to get HEAD")?
            .peel_to_commit()
            .context("HEAD does not point to a commit")?;
        let head_annotated = self
            .repo
            .find_annotated_commit(head.id())
            .context("Failed to create annotated commit for HEAD")?;

        let mut rebase = self
            .repo
            .rebase(
                Some(&head_annotated),
                None,
                Some(&onto_annotated),
                None,
            )
            .context("Failed to start rebase")?;

        let sig = self
            .repo
            .signature()
            .or_else(|_| git2::Signature::now("devflow", "devflow@localhost"))
            .context("Failed to create signature")?;

        let mut commits_replayed = 0;

        loop {
            match rebase.next() {
                Some(Ok(_op)) => {
                    // Check for conflicts in the index
                    let index = self.repo.index().context("Failed to get index")?;
                    if index.has_conflicts() {
                        let conflict_files: Vec<String> = index
                            .conflicts()
                            .into_iter()
                            .flatten()
                            .filter_map(|c| {
                                c.ok().and_then(|entry| {
                                    entry.our.map(|e| {
                                        String::from_utf8_lossy(&e.path).to_string()
                                    })
                                })
                            })
                            .collect();

                        rebase.abort().ok();
                        return Ok(crate::merge::RebaseResult {
                            success: false,
                            commits_replayed,
                            conflicts: true,
                            conflict_files,
                        });
                    }

                    rebase
                        .commit(None, &sig, None)
                        .context("Failed to commit rebased changes")?;
                    commits_replayed += 1;
                }
                Some(Err(e)) => {
                    rebase.abort().ok();
                    anyhow::bail!("Rebase operation failed: {}", e);
                }
                None => break,
            }
        }

        rebase.finish(None).context("Failed to finish rebase")?;

        Ok(crate::merge::RebaseResult {
            success: true,
            commits_replayed,
            conflicts: false,
            conflict_files: vec![],
        })
    }

    fn prune_worktrees(&self) -> Result<()> {
        // git2 doesn't expose worktree pruning directly, use git CLI
        let root = self.get_repo_root();
        let output = std::process::Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(root)
            .output()
            .context("Failed to prune worktrees")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git worktree prune failed: {}", stderr);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_worktree_name_for_branch() {
        assert_eq!(
            GitRepository::worktree_name_for_branch("feature/auth"),
            "feature-auth"
        );
        assert_eq!(GitRepository::worktree_name_for_branch("main"), "main");
        assert_eq!(
            GitRepository::worktree_name_for_branch("fix/bug/123"),
            "fix-bug-123"
        );
    }
}
