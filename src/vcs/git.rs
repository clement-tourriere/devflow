use anyhow::{Context, Result};
use git2::{Repository, WorktreeAddOptions, WorktreeLockStatus, WorktreePruneOptions};
use std::fs;
use std::path::{Path, PathBuf};

use super::{BranchInfo, VcsProvider, WorktreeInfo};

pub struct GitRepository {
    repo: Repository,
}

impl GitRepository {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let repo = Repository::open(path).context("Failed to open Git repository")?;

        Ok(GitRepository { repo })
    }

    pub fn get_current_branch(&self) -> Result<Option<String>> {
        let head = self.repo.head().context("Failed to get HEAD reference")?;

        if let Some(branch_name) = head.shorthand() {
            Ok(Some(branch_name.to_string()))
        } else {
            Ok(None)
        }
    }

    pub fn branch_exists(&self, branch_name: &str) -> Result<bool> {
        match self.repo.find_branch(branch_name, git2::BranchType::Local) {
            Ok(_) => Ok(true),
            Err(e) => {
                if e.code() == git2::ErrorCode::NotFound {
                    Ok(false)
                } else {
                    Err(anyhow::anyhow!("Error checking branch: {}", e))
                }
            }
        }
    }

    pub fn detect_main_branch(&self) -> Result<Option<String>> {
        // Strategy 1: Check for remote's default branch (most reliable)
        if let Some(main_branch) = self.get_remote_default_branch()? {
            log::debug!("Found remote default branch: {}", main_branch);
            return Ok(Some(main_branch));
        }

        // Strategy 2: Check common main branch names that exist locally
        let common_main_branches = vec!["main", "master", "develop", "development"];
        for branch_name in common_main_branches {
            if self.branch_exists(branch_name)? {
                log::debug!("Found local main branch: {}", branch_name);
                return Ok(Some(branch_name.to_string()));
            }
        }

        // Strategy 3: Find the local branch that tracks a remote main branch
        if let Some(main_branch) = self.find_local_tracking_main_branch()? {
            log::debug!("Found local branch tracking remote main: {}", main_branch);
            return Ok(Some(main_branch));
        }

        // Strategy 4: Use current branch as last resort (original behavior)
        if let Some(current_branch) = self.get_current_branch()? {
            log::debug!("Using current branch as fallback main: {}", current_branch);
            return Ok(Some(current_branch));
        }

        Ok(None)
    }

    fn get_remote_default_branch(&self) -> Result<Option<String>> {
        // Try to get the default branch from the remote
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
                        // Extract branch name from refs/remotes/origin/main -> main
                        let prefix = format!("refs/remotes/{}/", remote_name);
                        if target.starts_with(&prefix) {
                            let branch_name = target.strip_prefix(&prefix).unwrap();
                            found_default = Some(branch_name.to_string());
                            break;
                        }
                    }
                }
            }
        }

        Ok(found_default)
    }

    fn find_local_tracking_main_branch(&self) -> Result<Option<String>> {
        let branches = self.repo.branches(Some(git2::BranchType::Local))?;

        for branch_result in branches {
            let (branch, _) = branch_result?;
            if let Some(branch_name) = branch.name()? {
                // Check if this branch tracks a remote main/master branch
                if let Ok(upstream) = branch.upstream() {
                    if let Some(upstream_name) = upstream.name()? {
                        // Check if upstream is a main branch (contains main, master, etc.)
                        let upstream_lower = upstream_name.to_lowercase();
                        if upstream_lower.contains("main") || upstream_lower.contains("master") {
                            return Ok(Some(branch_name.to_string()));
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    #[allow(dead_code)]
    pub fn get_all_branches(&self) -> Result<Vec<String>> {
        let branches = self
            .repo
            .branches(Some(git2::BranchType::Local))
            .context("Failed to get branches")?;

        let mut branch_names = Vec::new();
        for branch in branches {
            let (branch, _) = branch.context("Failed to get branch")?;
            if let Some(name) = branch.name()? {
                branch_names.push(name.to_string());
            }
        }

        Ok(branch_names)
    }

    fn generate_hook_script(&self) -> String {
        r#"#!/bin/sh
# devflow auto-generated hook
# This hook automatically creates database branches when switching Git branches

# For post-checkout hook, check if this is a branch checkout (not file checkout)
# Parameters: $1=previous HEAD, $2=new HEAD, $3=checkout type (1=branch, 0=file)
if [ "$3" = "0" ]; then
    # This is a file checkout, not a branch checkout - skip devflow execution
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

# Regular checkout: skip if same branch
PREV_BRANCH=$(git reflog | awk 'NR==1{ print $6; exit }')
NEW_BRANCH=$(git reflog | awk 'NR==1{ print $8; exit }')

if [ "$PREV_BRANCH" = "$NEW_BRANCH" ]; then
    # This is the same branch checkout - skip devflow execution
    exit 0
fi

# Check if devflow is available
if command -v devflow >/dev/null 2>&1; then
    # Run devflow git-hook command to handle branch creation
    devflow git-hook
else
    echo "devflow not found in PATH, skipping database branch creation"
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

    /// Sanitize a branch name into a valid worktree name for git.
    /// Replaces `/` with `-` since worktree names are used as directory components.
    fn worktree_name_for_branch(branch: &str) -> String {
        branch.replace('/', "-")
    }
}

// ─── VcsProvider implementation ────────────────────────────────────────────

impl VcsProvider for GitRepository {
    fn current_branch(&self) -> Result<Option<String>> {
        self.get_current_branch()
    }

    fn default_branch(&self) -> Result<Option<String>> {
        self.detect_main_branch()
    }

    fn list_branches(&self) -> Result<Vec<BranchInfo>> {
        let current = self.get_current_branch()?;
        let default = self.detect_main_branch()?;

        let branches = self
            .repo
            .branches(Some(git2::BranchType::Local))
            .context("Failed to list branches")?;

        let mut result = Vec::new();
        for branch_result in branches {
            let (branch, _) = branch_result?;
            if let Some(name) = branch.name()? {
                result.push(BranchInfo {
                    name: name.to_string(),
                    is_current: current.as_deref() == Some(name),
                    is_default: default.as_deref() == Some(name),
                });
            }
        }

        Ok(result)
    }

    fn create_branch(&self, name: &str, base: Option<&str>) -> Result<()> {
        // Resolve the base commit
        let base_commit = if let Some(base_name) = base {
            let obj = self
                .repo
                .revparse_single(base_name)
                .with_context(|| format!("Failed to find base branch '{}'", base_name))?;
            obj.peel_to_commit()
                .context("Base reference is not a commit")?
        } else {
            let head = self.repo.head().context("Failed to get HEAD")?;
            head.peel_to_commit()
                .context("HEAD does not point to a commit")?
        };

        self.repo
            .branch(name, &base_commit, false)
            .with_context(|| format!("Failed to create branch '{}'", name))?;

        Ok(())
    }

    fn delete_branch(&self, name: &str) -> Result<()> {
        let mut branch = self
            .repo
            .find_branch(name, git2::BranchType::Local)
            .with_context(|| format!("Branch '{}' not found", name))?;
        branch
            .delete()
            .with_context(|| format!("Failed to delete branch '{}'", name))?;
        Ok(())
    }

    fn branch_exists(&self, name: &str) -> Result<bool> {
        self.branch_exists(name)
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
        let current_branch = self.get_current_branch()?;
        let repo_root = self
            .repo
            .workdir()
            .unwrap_or_else(|| self.repo.path())
            .to_path_buf();

        result.push(WorktreeInfo {
            path: repo_root,
            branch: current_branch,
            is_main: true,
            is_locked: false,
        });

        // List linked worktrees
        let worktree_names = self.repo.worktrees().context("Failed to list worktrees")?;

        for wt_name in worktree_names.iter() {
            let Some(name) = wt_name else { continue };

            if let Ok(wt) = self.repo.find_worktree(name) {
                let wt_path = wt.path().to_path_buf();

                // Get branch for this worktree by opening the repo at that path
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
                    branch: wt_branch,
                    is_main: false,
                    is_locked,
                });
            }
        }

        Ok(result)
    }

    fn create_worktree(&self, branch: &str, path: &Path) -> Result<()> {
        let wt_name = Self::worktree_name_for_branch(branch);

        // Check if the branch already exists
        let branch_exists = self.branch_exists(branch)?;

        if branch_exists {
            // Use the existing branch
            let branch_ref = self
                .repo
                .find_branch(branch, git2::BranchType::Local)
                .with_context(|| format!("Branch '{}' not found", branch))?;
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
        } else {
            // Let git create the branch automatically (named after the worktree)
            // First create the branch from HEAD, then create the worktree
            let head = self.repo.head().context("Failed to get HEAD")?;
            let head_commit = head
                .peel_to_commit()
                .context("HEAD does not point to a commit")?;

            let new_branch = self
                .repo
                .branch(branch, &head_commit, false)
                .with_context(|| format!("Failed to create branch '{}'", branch))?;

            let reference = new_branch.into_reference();
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
        }

        Ok(())
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

    fn worktree_path(&self, branch: &str) -> Result<Option<PathBuf>> {
        let worktree_names = self.repo.worktrees().context("Failed to list worktrees")?;

        for wt_name in worktree_names.iter() {
            let Some(name) = wt_name else { continue };

            if let Ok(wt) = self.repo.find_worktree(name) {
                let wt_path = wt.path().to_path_buf();
                // Check if this worktree has the target branch checked out
                if let Ok(wt_repo) = Repository::open(&wt_path) {
                    if let Ok(head) = wt_repo.head() {
                        if head.shorthand() == Some(branch) {
                            return Ok(Some(wt_path));
                        }
                    }
                }
            }
        }

        // Also check if the main worktree has this branch
        if let Some(current) = self.get_current_branch()? {
            if current == branch {
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
