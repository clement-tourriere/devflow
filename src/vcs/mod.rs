pub mod git;
pub mod jj;

use anyhow::Result;
use std::path::{Path, PathBuf};

// Re-export for backward compatibility during transition
pub use git::GitRepository;
pub use jj::JjRepository;

/// Information about a single branch.
#[derive(Debug, Clone)]
pub struct BranchInfo {
    /// Branch name (e.g. "feature/auth")
    pub name: String,
    /// Whether this is the currently checked-out branch
    pub is_current: bool,
    /// Whether this branch is the default/main branch
    pub is_default: bool,
}

/// Information about a Git worktree.
#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    /// Filesystem path to the worktree
    pub path: PathBuf,
    /// Branch checked out in this worktree (None for detached HEAD)
    pub branch: Option<String>,
    /// Whether this is the main (bare) worktree
    #[allow(dead_code)]
    pub is_main: bool,
    /// Whether the worktree is locked
    #[allow(dead_code)]
    pub is_locked: bool,
}

/// Abstraction over version control systems.
///
/// Git is the primary implementation. jj (Jujutsu) is also supported
/// via `JjRepository`.
#[allow(dead_code)]
pub trait VcsProvider: Send {
    // ── Branch operations ──────────────────────────────────────────
    fn current_branch(&self) -> Result<Option<String>>;
    fn default_branch(&self) -> Result<Option<String>>;
    fn list_branches(&self) -> Result<Vec<BranchInfo>>;
    fn create_branch(&self, name: &str, base: Option<&str>) -> Result<()>;
    fn delete_branch(&self, name: &str) -> Result<()>;
    fn branch_exists(&self, name: &str) -> Result<bool>;

    // ── Worktree operations ────────────────────────────────────────
    fn supports_worktrees(&self) -> bool;
    fn is_worktree(&self) -> bool;
    fn list_worktrees(&self) -> Result<Vec<WorktreeInfo>>;
    fn create_worktree(&self, branch: &str, path: &Path) -> Result<()>;
    fn remove_worktree(&self, path: &Path) -> Result<()>;
    fn worktree_path(&self, branch: &str) -> Result<Option<PathBuf>>;
    fn main_worktree_dir(&self) -> Option<PathBuf>;

    // ── Hooks ──────────────────────────────────────────────────────
    fn install_hooks(&self) -> Result<()>;
    fn uninstall_hooks(&self) -> Result<()>;
    /// Check if a hook file at the given path was written by devflow.
    fn is_devflow_hook(&self, hook_path: &Path) -> Result<bool>;

    // ── Meta ───────────────────────────────────────────────────────
    fn provider_name(&self) -> &'static str;
    fn repo_root(&self) -> &Path;

    // ── File queries ───────────────────────────────────────────────
    /// List files that are present on disk but ignored by VCS (e.g. `.env.local`).
    ///
    /// Returns paths relative to the repo root.  Used by `copy_ignored`
    /// to replicate gitignored files into new worktrees.
    fn list_ignored_files(&self) -> Result<Vec<PathBuf>> {
        Ok(Vec::new())
    }

    // ── Commit operations ──────────────────────────────────────────
    /// Return a unified diff of all staged (index) changes.
    ///
    /// For git this is `git diff --cached`.  For jj this is `jj diff`.
    fn staged_diff(&self) -> Result<String> {
        anyhow::bail!("{} does not support staged_diff", self.provider_name())
    }

    /// Return a summary of staged changes (short stat-like output).
    fn staged_summary(&self) -> Result<String> {
        anyhow::bail!("{} does not support staged_summary", self.provider_name())
    }

    /// Create a commit with the given message.
    fn commit(&self, _message: &str) -> Result<()> {
        anyhow::bail!("{} does not support commit", self.provider_name())
    }

    /// Check whether there are staged changes ready to commit.
    fn has_staged_changes(&self) -> Result<bool> {
        anyhow::bail!(
            "{} does not support has_staged_changes",
            self.provider_name()
        )
    }
}

/// Which VCS was detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VcsKind {
    Git,
    Jj,
}

/// Auto-detect the VCS in use and return a boxed provider.
///
/// Detection order:
/// 1. **jj-only** — `.jj/` exists but `.git/` does not → `JjRepository`
/// 2. **jj colocated** — both `.jj/` and `.git/` exist → `JjRepository`
///    (jj is the "primary" VCS in colocated mode; it manages `.git/` for you)
/// 3. **git-only** — `.git/` exists without `.jj/` → `GitRepository`
/// 4. Neither → error
///
/// This walks up the directory tree from `path` to find the repo root.
pub fn detect_vcs_provider<P: AsRef<Path>>(path: P) -> Result<Box<dyn VcsProvider>> {
    let start = path.as_ref();

    // Walk up to find .jj or .git
    let (has_jj, has_git) = find_vcs_markers(start);

    match (has_jj, has_git) {
        (true, _) => {
            // jj (with or without colocated .git/) — prefer jj
            let provider = JjRepository::new(start)?;
            log::debug!("Detected VCS: jj (colocated={})", has_git);
            Ok(Box::new(provider))
        }
        (false, true) => {
            let provider = GitRepository::new(start)?;
            log::debug!("Detected VCS: git");
            Ok(Box::new(provider))
        }
        (false, false) => {
            anyhow::bail!("No VCS repository found. Initialize with 'git init' or 'jj init'.");
        }
    }
}

/// Detect which VCS kind is present without constructing a provider.
#[allow(dead_code)]
pub fn detect_vcs_kind<P: AsRef<Path>>(path: P) -> Option<VcsKind> {
    let (has_jj, has_git) = find_vcs_markers(path.as_ref());
    match (has_jj, has_git) {
        (true, _) => Some(VcsKind::Jj),
        (false, true) => Some(VcsKind::Git),
        (false, false) => None,
    }
}

/// Walk up from `start` looking for `.jj/` and `.git/` directories.
fn find_vcs_markers(start: &Path) -> (bool, bool) {
    let mut current = start.to_path_buf();
    if current.is_file() {
        current.pop();
    }

    let mut has_jj = false;
    let mut has_git = false;

    loop {
        if !has_jj && current.join(".jj").is_dir() {
            has_jj = true;
        }
        if !has_git && (current.join(".git").is_dir() || current.join(".git").is_file()) {
            has_git = true;
        }
        // Found both or reached filesystem root
        if (has_jj && has_git) || !current.pop() {
            break;
        }
    }

    (has_jj, has_git)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_find_vcs_markers_git_only() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();

        let (has_jj, has_git) = find_vcs_markers(tmp.path());
        assert!(!has_jj);
        assert!(has_git);
        assert_eq!(detect_vcs_kind(tmp.path()), Some(VcsKind::Git));
    }

    #[test]
    fn test_find_vcs_markers_jj_only() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".jj")).unwrap();

        let (has_jj, has_git) = find_vcs_markers(tmp.path());
        assert!(has_jj);
        assert!(!has_git);
        assert_eq!(detect_vcs_kind(tmp.path()), Some(VcsKind::Jj));
    }

    #[test]
    fn test_find_vcs_markers_colocated() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        fs::create_dir(tmp.path().join(".jj")).unwrap();

        let (has_jj, has_git) = find_vcs_markers(tmp.path());
        assert!(has_jj);
        assert!(has_git);
        // jj takes priority in colocated mode
        assert_eq!(detect_vcs_kind(tmp.path()), Some(VcsKind::Jj));
    }

    #[test]
    fn test_find_vcs_markers_none() {
        let tmp = tempfile::tempdir().unwrap();

        let (has_jj, has_git) = find_vcs_markers(tmp.path());
        assert!(!has_jj);
        assert!(!has_git);
        assert_eq!(detect_vcs_kind(tmp.path()), None);
    }

    #[test]
    fn test_find_vcs_markers_from_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        let subdir = tmp.path().join("src").join("deep");
        fs::create_dir_all(&subdir).unwrap();

        let (has_jj, has_git) = find_vcs_markers(&subdir);
        assert!(!has_jj);
        assert!(has_git);
    }

    #[test]
    fn test_find_vcs_markers_git_file_worktree() {
        // Git worktrees use a `.git` *file* (not directory) that points to the main repo
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join(".git"),
            "gitdir: /some/other/path/.git/worktrees/foo",
        )
        .unwrap();

        let (has_jj, has_git) = find_vcs_markers(tmp.path());
        assert!(!has_jj);
        assert!(has_git);
    }
}
