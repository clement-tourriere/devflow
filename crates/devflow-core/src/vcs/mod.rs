pub mod cow_worktree;
pub mod git;
pub mod jj;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// Re-export for convenience
pub use git::GitRepository;
pub use jj::JjRepository;

/// Information about a single workspace.
#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    /// Workspace name (e.g. "feature/auth")
    pub name: String,
    /// Whether this is the currently checked-out workspace
    pub is_current: bool,
    /// Whether this workspace is the default/main workspace
    pub is_default: bool,
}

/// Result of a worktree creation operation.
#[derive(Debug, Clone, Copy)]
pub struct WorktreeCreateResult {
    _private: (),
}

impl WorktreeCreateResult {
    pub(crate) fn new() -> Self {
        Self { _private: () }
    }
}

/// Information about a Git worktree.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct WorktreeInfo {
    /// Filesystem path to the worktree
    pub path: PathBuf,
    /// Workspace checked out in this worktree (None for detached HEAD)
    pub workspace: Option<String>,
    /// Whether this is the main (bare) worktree
    pub is_main: bool,
    /// Whether the worktree is locked
    pub is_locked: bool,
}

/// Abstraction over version control systems.
///
/// Git is the primary implementation. jj (Jujutsu) is also supported
/// via `JjRepository`.
pub trait VcsProvider: Send {
    // ── Workspace operations ──────────────────────────────────────────
    fn current_workspace(&self) -> Result<Option<String>>;
    fn default_workspace(&self) -> Result<Option<String>>;
    fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>>;
    fn create_workspace(&self, name: &str, base: Option<&str>) -> Result<()>;
    fn delete_workspace(&self, name: &str) -> Result<()>;
    fn workspace_exists(&self, name: &str) -> Result<bool>;

    /// Checkout/switch to an existing workspace (classic mode, no worktrees).
    fn checkout_workspace(&self, _name: &str) -> Result<()> {
        anyhow::bail!(
            "{} does not support checkout_workspace",
            self.provider_name()
        )
    }

    // ── Worktree operations ────────────────────────────────────────
    fn supports_worktrees(&self) -> bool;
    fn is_worktree(&self) -> bool;
    fn list_worktrees(&self) -> Result<Vec<WorktreeInfo>>;
    fn create_worktree(&self, workspace: &str, path: &Path) -> Result<WorktreeCreateResult>;
    /// Remove the worktree at `path`.
    ///
    /// Without `force`, implementations must refuse when the worktree has
    /// uncommitted work (tracked modifications, staged changes, or untracked
    /// non-ignored files) — mirroring `git worktree remove`.  With `force`,
    /// the worktree is removed regardless.
    fn remove_worktree(&self, path: &Path, force: bool) -> Result<()>;
    fn worktree_path(&self, workspace: &str) -> Result<Option<PathBuf>>;
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

    /// List ignored entries (files **and** directories) without recursing
    /// into ignored directories.
    ///
    /// Returns paths relative to the repo root.  An ignored directory like
    /// `front/node_modules` appears as a single entry rather than listing
    /// every file inside it.  Used by `respect_gitignore` to exclude heavy
    /// directories from worktree clones.
    fn list_ignored_entries(&self) -> Result<Vec<PathBuf>> {
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

    /// Ensure the repository has at least one commit so the default workspace
    /// is materialised and `list_workspaces` returns it.
    ///
    /// This is a no-op when the repo already has commits.  For git it
    /// creates an empty "Initial commit (devflow)" on an unborn HEAD.
    fn ensure_initial_commit(&self) -> Result<()> {
        Ok(())
    }

    /// Clean up stale worktree entries.
    fn prune_worktrees(&self) -> Result<()> {
        Ok(())
    }
}

/// Which VCS was detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VcsKind {
    Git,
    Jj,
}

impl std::fmt::Display for VcsKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VcsKind::Git => write!(f, "git"),
            VcsKind::Jj => write!(f, "jj"),
        }
    }
}

/// Auto-detect the VCS in use and return a boxed provider.
///
/// Detection order:
/// 1. **jj-only** — `.jj/` exists but `.git/` does not → `JjRepository`
/// 2. **jj colocated** — both `.jj/` and `.git/` exist → `JjRepository`
///    when the `jj` CLI is available; otherwise fall back to `GitRepository`
/// 3. **git-only** — `.git/` exists without `.jj/` → `GitRepository`
/// 4. Neither → error
///
/// This walks up the directory tree from `path` to find the repo root.
pub fn detect_vcs_provider<P: AsRef<Path>>(path: P) -> Result<Box<dyn VcsProvider>> {
    let start = path.as_ref();

    // Walk up to find .jj or .git
    let (has_jj, has_git) = find_vcs_markers(start);
    let jj_available = tool_available("jj");

    match effective_vcs_kind(has_jj, has_git, jj_available) {
        Some(VcsKind::Jj) => {
            let provider = JjRepository::new(start)?;
            log::debug!("Detected VCS: jj (colocated={})", has_git);
            Ok(Box::new(provider))
        }
        Some(VcsKind::Git) => {
            if has_jj && has_git && !jj_available {
                log::warn!(
                    "Detected colocated jj/git repository, but the 'jj' CLI is unavailable; falling back to git"
                );
            }
            let provider = GitRepository::new(start)?;
            log::debug!("Detected VCS: git");
            Ok(Box::new(provider))
        }
        None => {
            anyhow::bail!("No VCS repository found. Initialize with 'git init' or 'jj init'.");
        }
    }
}

fn effective_vcs_kind(has_jj: bool, has_git: bool, jj_available: bool) -> Option<VcsKind> {
    match (has_jj, has_git, jj_available) {
        (true, true, false) => Some(VcsKind::Git),
        (true, _, _) => Some(VcsKind::Jj),
        (false, true, _) => Some(VcsKind::Git),
        (false, false, _) => None,
    }
}

/// Resolve a path inside a repository to its **canonical main-repo root**.
///
/// This is the single source of project identity. When devflow runs from a
/// git worktree (the agent scenario), the naive "directory containing
/// `.devflow.yml`" is the *worktree* directory, which differs from the main
/// repo — fragmenting local state, hook approvals, and container identity
/// into per-worktree silos. Resolving every identity derivation through this
/// function unifies them: a worktree and its main repo share one identity.
///
/// For a normal repo this is the work tree root; for a worktree it is the
/// main work tree root (the parent of the shared `.git` common dir). Falls
/// back to the canonicalized input when `start` is not in a git repository
/// (e.g. a plain directory or a jj-only repo).
pub fn resolve_project_root(start: &Path) -> PathBuf {
    let canonical = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());

    // `discover` walks up from `start` to the nearest `.git`, so it works
    // from the repo root, a subdirectory, or a worktree root alike.
    if let Ok(repo) = git2::Repository::discover(&canonical) {
        // `commondir()` is the shared git dir: `<root>/.git` for a normal
        // repo and `<main-root>/.git` for a worktree. Its parent is the
        // main work tree root in both cases.
        let common = repo.commondir();
        let common = common
            .canonicalize()
            .unwrap_or_else(|_| common.to_path_buf());
        if let Some(parent) = common.parent() {
            // Guard against bare repos (no work tree): only trust the result
            // when it actually contains a work tree / our config could live.
            if parent.is_dir() {
                return parent
                    .canonicalize()
                    .unwrap_or_else(|_| parent.to_path_buf());
            }
        }
    }

    canonical
}

/// Detect which VCS kind is present without constructing a provider.
pub fn detect_vcs_kind<P: AsRef<Path>>(path: P) -> Option<VcsKind> {
    let (has_jj, has_git) = find_vcs_markers(path.as_ref());
    match (has_jj, has_git) {
        (true, _) => Some(VcsKind::Jj),
        (false, true) => Some(VcsKind::Git),
        (false, false) => None,
    }
}

/// Check whether a CLI tool is available on PATH.
fn tool_available(name: &str) -> bool {
    which::which(name).is_ok()
}

/// Return which VCS tools are available on the system.
///
/// Returns a vec of `VcsKind` values — always includes `Git` (since
/// git2 is embedded as a fallback even without the CLI).
pub fn available_vcs_tools() -> Vec<VcsKind> {
    let mut tools = Vec::new();
    // Git is always available via the embedded git2 library
    tools.push(VcsKind::Git);
    if tool_available("jj") {
        tools.push(VcsKind::Jj);
    }
    tools
}

/// Initialize a new VCS repository at `path`.
///
/// Selection logic (in priority order):
/// 1. If `preference` is `Some(kind)`, use that VCS.
/// 2. Auto-detect which tools are available (`jj`, `git`).
///    - If only one is available, use it.
///    - If both are available and `interactive` is true, prompt the user.
///    - If both are available and `interactive` is false, default to **git**.
/// 3. If **neither** CLI is available, use `git2::Repository::init()` as an
///    embedded fallback (no external binary required).
///
/// Returns the `VcsKind` that was initialized.
pub fn init_vcs_repository<P: AsRef<Path>>(
    path: P,
    preference: Option<VcsKind>,
    interactive: bool,
) -> Result<VcsKind> {
    let path = path.as_ref();

    // If a preference is set, honour it directly.
    if let Some(kind) = preference {
        return init_specific_vcs(path, kind);
    }

    let has_jj = tool_available("jj");
    let has_git_cli = tool_available("git");

    match (has_jj, has_git_cli) {
        (true, true) => {
            // Both available — ask user or default to git.
            let chosen = if interactive {
                prompt_vcs_choice()?
            } else {
                VcsKind::Git
            };
            init_specific_vcs(path, chosen)
        }
        (true, false) => init_specific_vcs(path, VcsKind::Jj),
        (false, true) => init_specific_vcs(path, VcsKind::Git),
        (false, false) => {
            // No CLI available — use git2 library as embedded fallback.
            log::info!("No git or jj CLI found; using embedded git2 library to initialize");
            GitRepository::init(path)?;
            Ok(VcsKind::Git)
        }
    }
}

/// Initialize a specific VCS at `path`.
fn init_specific_vcs(path: &Path, kind: VcsKind) -> Result<VcsKind> {
    match kind {
        VcsKind::Git => {
            GitRepository::init(path)?;
            Ok(VcsKind::Git)
        }
        VcsKind::Jj => {
            // Default to colocated mode so git tooling also works.
            JjRepository::init(path, true)?;
            Ok(VcsKind::Jj)
        }
    }
}

/// Interactive prompt: ask the user which VCS to initialize.
fn prompt_vcs_choice() -> Result<VcsKind> {
    let options = vec!["Git", "Jujutsu (jj)"];
    let choice = inquire::Select::new("Which VCS would you like to initialize?", options)
        .with_help_message("Both git and jj are available on your system")
        .prompt()
        .unwrap_or("Git");
    if choice.starts_with("Jujutsu") {
        Ok(VcsKind::Jj)
    } else {
        Ok(VcsKind::Git)
    }
}

/// Walk up from `start` looking for `.jj/` and `.git/` directories.
fn find_vcs_markers(start: &Path) -> (bool, bool) {
    // Resolve relative paths (e.g. ".") to absolute so that pop() can walk
    // up the directory tree.  Fall back to the raw path when canonicalize
    // fails (e.g. the path does not exist yet).
    let mut current = std::env::current_dir()
        .ok()
        .and_then(|cwd| {
            let abs = if start.is_relative() {
                cwd.join(start)
            } else {
                start.to_path_buf()
            };
            // Use dunce::canonicalize or std; the important thing is an
            // absolute path so pop() works.
            abs.canonicalize().ok()
        })
        .unwrap_or_else(|| start.to_path_buf());

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
    fn test_effective_vcs_kind_falls_back_to_git_for_colocated_without_jj() {
        assert_eq!(effective_vcs_kind(true, true, false), Some(VcsKind::Git));
        assert_eq!(effective_vcs_kind(true, true, true), Some(VcsKind::Jj));
        assert_eq!(effective_vcs_kind(true, false, false), Some(VcsKind::Jj));
        assert_eq!(effective_vcs_kind(false, true, false), Some(VcsKind::Git));
        assert_eq!(effective_vcs_kind(false, false, false), None);
    }

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
    fn test_resolve_project_root_plain_dir_falls_back() {
        // A non-git directory resolves to itself (canonicalized).
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        assert_eq!(resolve_project_root(&root), root);
    }

    #[test]
    fn test_resolve_project_root_unifies_worktree_and_main() {
        use crate::vcs::git::GitRepository;

        let tmp = tempfile::tempdir().unwrap();
        let main_root = tmp.path().canonicalize().unwrap();
        let repo = GitRepository::init(&main_root).unwrap();
        repo.ensure_initial_commit().unwrap();

        // The main repo resolves to itself.
        assert_eq!(resolve_project_root(&main_root), main_root);

        // A worktree resolves to the SAME main-repo root — this is what
        // unifies project identity across worktrees.
        let wt_path = main_root.join("wt-feature");
        repo.create_worktree("feature/x", &wt_path).unwrap();
        let wt_canonical = wt_path.canonicalize().unwrap();
        assert_ne!(wt_canonical, main_root, "worktree dir differs from main");
        assert_eq!(
            resolve_project_root(&wt_canonical),
            main_root,
            "worktree must resolve to the main repo root"
        );
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
