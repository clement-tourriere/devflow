//! Jujutsu (jj) VCS provider.
//!
//! Maps jj concepts to devflow's VcsProvider trait:
//! - jj **bookmarks** → branches
//! - jj **workspaces** → worktrees
//! - jj **colocated repos** are supported (`.jj` + `.git` side by side)
//!
//! This provider shells out to the `jj` CLI since there is no stable Rust
//! library equivalent to `git2`. Commands are run with `--no-pager` and
//! `--color=never` for machine-friendly output.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{BranchInfo, VcsProvider, WorktreeCreateResult, WorktreeInfo};

/// A Jujutsu repository.
pub struct JjRepository {
    /// Root of the repository (directory containing `.jj/`).
    root: PathBuf,
}

impl JjRepository {
    /// Open the jj repository at `path` (or a parent containing `.jj/`).
    #[allow(dead_code)]
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let root = Self::find_repo_root(path.as_ref())
            .context("Failed to find jj repository (no .jj/ directory)")?;

        // Verify `jj` is available
        let output = Command::new("jj")
            .args(["--version"])
            .output()
            .context("Failed to execute 'jj'. Is Jujutsu installed?")?;

        if !output.status.success() {
            bail!(
                "jj --version failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(Self { root })
    }

    /// Walk up from `start` to find a directory containing `.jj/`.
    fn find_repo_root(start: &Path) -> Option<PathBuf> {
        let mut current = start.to_path_buf();
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

    /// Run a jj command and return its stdout.
    fn jj(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("jj")
            .args(["--no-pager", "--color=never"])
            .args(args)
            .current_dir(&self.root)
            .output()
            .with_context(|| format!("Failed to run jj {}", args.join(" ")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("jj {} failed: {}", args.join(" "), stderr.trim());
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Run a jj command, returning Ok(stdout) on success or Err on failure.
    /// Like `jj()` but doesn't bail on non-zero exit — returns the error
    /// so the caller can handle it gracefully.
    fn jj_try(&self, args: &[&str]) -> Result<String> {
        self.jj(args)
    }

    /// Check whether this repository is colocated (has both .jj/ and .git/).
    #[allow(dead_code)]
    pub fn is_colocated(&self) -> bool {
        self.root.join(".git").exists()
    }

    /// Get the current bookmark (if the working-copy commit has exactly one).
    ///
    /// Uses `jj log -r @` with a template that outputs bookmark names.
    fn current_bookmark(&self) -> Result<Option<String>> {
        // Template: for each bookmark on @, output its name separated by newlines
        let output = self.jj(&[
            "log",
            "-r",
            "@",
            "--no-graph",
            "-T",
            r#"separate("\n", bookmarks)"#,
        ])?;

        let bookmarks: Vec<&str> = output.lines().filter(|l| !l.is_empty()).collect();

        match bookmarks.len() {
            0 => Ok(None),
            1 => Ok(Some(bookmarks[0].to_string())),
            _ => {
                // Multiple bookmarks point at @. Return the first one but log a warning.
                log::debug!(
                    "Multiple bookmarks at @: {:?}. Using first: {}",
                    bookmarks,
                    bookmarks[0]
                );
                Ok(Some(bookmarks[0].to_string()))
            }
        }
    }

    /// Detect the default/main bookmark. Tries "main", then "master",
    /// then falls back to the first bookmark that tracks a remote.
    fn detect_default_bookmark(&self) -> Result<Option<String>> {
        let output = self.jj(&["bookmark", "list", "--all"])?;
        let mut first_tracked: Option<String> = None;

        for line in output.lines() {
            let name = line.split(':').next().unwrap_or("").trim();
            if name.is_empty() {
                continue;
            }

            // Prefer "main" or "master"
            if name == "main" || name == "master" {
                return Ok(Some(name.to_string()));
            }

            // Track the first bookmark that has an @origin marker
            if first_tracked.is_none() && line.contains("@origin") {
                first_tracked = Some(name.to_string());
            }
        }

        Ok(first_tracked)
    }

    /// Write a hook script for devflow into the jj hooks directory.
    fn generate_hook_script(&self) -> String {
        "#!/bin/sh\n\
         # devflow hook — managed automatically, do not edit\n\
         if command -v devflow >/dev/null 2>&1; then\n\
         \tdevflow git-hook\n\
         fi\n"
            .to_string()
    }
}

impl VcsProvider for JjRepository {
    // ── Branch operations (mapped to bookmarks) ────────────────────

    fn current_branch(&self) -> Result<Option<String>> {
        self.current_bookmark()
    }

    fn default_branch(&self) -> Result<Option<String>> {
        self.detect_default_bookmark()
    }

    fn list_branches(&self) -> Result<Vec<BranchInfo>> {
        let output = self.jj(&["bookmark", "list"])?;
        let current = self.current_bookmark()?;
        let default = self.detect_default_bookmark()?;
        let mut branches = Vec::new();

        for line in output.lines() {
            let name = line.split(':').next().unwrap_or("").trim();
            if name.is_empty() {
                continue;
            }
            branches.push(BranchInfo {
                name: name.to_string(),
                is_current: current.as_deref() == Some(name),
                is_default: default.as_deref() == Some(name),
            });
        }

        Ok(branches)
    }

    fn create_branch(&self, name: &str, base: Option<&str>) -> Result<()> {
        if let Some(base_rev) = base {
            // Create a new commit on top of the base, then set the bookmark
            self.jj(&["new", base_rev])?;
        }
        self.jj(&["bookmark", "create", name])?;
        Ok(())
    }

    fn delete_branch(&self, name: &str) -> Result<()> {
        self.jj(&["bookmark", "delete", name])?;
        Ok(())
    }

    fn branch_exists(&self, name: &str) -> Result<bool> {
        // `jj bookmark list --bookmark <name>` returns empty if it doesn't exist
        let output = self.jj_try(&["bookmark", "list", "--bookmark", name])?;
        Ok(!output.trim().is_empty())
    }

    // ── Worktree operations (mapped to workspaces) ─────────────────

    fn supports_worktrees(&self) -> bool {
        true // jj has native workspace support
    }

    fn is_worktree(&self) -> bool {
        // Check if we're in a non-default workspace
        if let Ok(output) = self.jj_try(&["workspace", "list"]) {
            let lines: Vec<&str> = output.lines().collect();
            // If there's more than one workspace and we're not the default,
            // we're in a "worktree"-like workspace
            if lines.len() > 1 {
                // The default workspace is usually the first one
                return true;
            }
        }
        false
    }

    fn list_worktrees(&self) -> Result<Vec<WorktreeInfo>> {
        let output = self.jj(&["workspace", "list"])?;
        let mut worktrees = Vec::new();

        for line in output.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            // jj workspace list output format: "workspace_name: path"
            // The default workspace is named "default"
            let (name, path_str) = if let Some(colon_pos) = line.find(':') {
                (
                    line[..colon_pos].trim(),
                    line[colon_pos + 1..].trim().to_string(),
                )
            } else {
                (line, String::new())
            };

            let path = if path_str.is_empty() {
                self.root.clone()
            } else {
                PathBuf::from(&path_str)
            };

            let is_main = name == "default";

            worktrees.push(WorktreeInfo {
                path,
                branch: if is_main {
                    self.current_bookmark().ok().flatten()
                } else {
                    None // Would need per-workspace bookmark resolution
                },
                is_main,
                is_locked: false, // jj workspaces don't have a lock concept
            });
        }

        Ok(worktrees)
    }

    fn create_worktree(&self, branch: &str, path: &Path) -> Result<WorktreeCreateResult> {
        let workspace_name = Self::workspace_name_for_branch(branch);
        let path_str = path.to_str().context("Worktree path is not valid UTF-8")?;

        // Create a new workspace at the given path
        self.jj(&["workspace", "add", "--name", &workspace_name, path_str])?;

        // Set the bookmark in the new workspace
        // We need to move to the bookmark's commit in the new workspace
        if self.branch_exists(branch)? {
            // Run jj edit in the new workspace to point it at the bookmark
            let jj_result = Command::new("jj")
                .args([
                    "--no-pager",
                    "--color=never",
                    "--repository",
                    path_str,
                    "edit",
                    branch,
                ])
                .current_dir(&self.root)
                .output()
                .context("Failed to set workspace to bookmark")?;

            if !jj_result.status.success() {
                log::debug!(
                    "Could not set workspace to bookmark {}: {}",
                    branch,
                    String::from_utf8_lossy(&jj_result.stderr)
                );
            }
        }

        Ok(WorktreeCreateResult { cow_used: false })
    }

    fn remove_worktree(&self, path: &Path) -> Result<()> {
        // Find workspace name by path
        let worktrees = self.list_worktrees()?;
        let workspace = worktrees.iter().find(|w| w.path == path);

        if let Some(_ws) = workspace {
            // jj workspace forget <name>
            // We need to figure out the workspace name from the path.
            // For now, use path-based removal — jj supports forgetting by name
            let path_str = path.to_str().context("Worktree path is not valid UTF-8")?;

            // Try to forget the workspace using the path
            self.jj(&["workspace", "forget", "--repository", path_str])
                .or_else(|_| -> Result<String> {
                    // Fallback: remove the directory directly
                    log::debug!(
                        "jj workspace forget failed, removing directory: {}",
                        path_str
                    );
                    std::fs::remove_dir_all(path)
                        .context("Failed to remove workspace directory")?;
                    Ok(String::new())
                })?;
        } else {
            log::debug!(
                "No jj workspace found at {}; skipping removal",
                path.display()
            );
        }

        Ok(())
    }

    fn worktree_path(&self, branch: &str) -> Result<Option<PathBuf>> {
        let workspace_name = Self::workspace_name_for_branch(branch);
        let worktrees = self.list_worktrees()?;

        Ok(worktrees
            .iter()
            .find(|w| {
                // Match by branch name or workspace name derived from the branch
                w.branch.as_deref() == Some(branch)
                    || w.path
                        .file_name()
                        .map(|n| n.to_string_lossy().contains(&workspace_name))
                        .unwrap_or(false)
            })
            .map(|w| w.path.clone()))
    }

    fn main_worktree_dir(&self) -> Option<PathBuf> {
        Some(self.root.clone())
    }

    // ── Hooks ──────────────────────────────────────────────────────

    fn install_hooks(&self) -> Result<()> {
        // jj doesn't have native hook support (as of 0.24+).
        // For colocated repos, install into .git/hooks (same as Git).
        // For pure jj repos, we rely on devflow's own hook engine triggered
        // by `devflow git-hook` which the user runs manually or via shell integration.
        let git_hooks_dir = self.root.join(".git").join("hooks");
        if git_hooks_dir.parent().map(|p| p.exists()).unwrap_or(false) {
            // Colocated repo — install into .git/hooks
            std::fs::create_dir_all(&git_hooks_dir).context("Failed to create hooks directory")?;

            let hook_script = self.generate_hook_script();

            for hook_name in &["post-checkout", "post-merge"] {
                let hook_path = git_hooks_dir.join(hook_name);
                std::fs::write(&hook_path, &hook_script)
                    .with_context(|| format!("Failed to write {} hook", hook_name))?;

                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perms = std::fs::metadata(&hook_path)?.permissions();
                    perms.set_mode(0o755);
                    std::fs::set_permissions(&hook_path, perms)
                        .context("Failed to set hook permissions")?;
                }
            }

            log::info!("Installed hooks into colocated .git/hooks");
        } else {
            // Pure jj repo — no hook directory to install into.
            // Print guidance for the user.
            log::info!(
                "jj does not support native hooks. Use 'devflow git-hook' via shell integration."
            );
            println!(
                "Note: jj does not have native hook support.\n\
                 Add 'eval \"$(devflow shell-init bash)\"' to your shell RC file\n\
                 for automatic branch switching via shell integration."
            );
        }

        Ok(())
    }

    fn uninstall_hooks(&self) -> Result<()> {
        // Only relevant for colocated repos
        let git_hooks_dir = self.root.join(".git").join("hooks");
        if git_hooks_dir.exists() {
            for hook_name in &["post-checkout", "post-merge"] {
                let hook_path = git_hooks_dir.join(hook_name);
                if hook_path.exists() && self.is_devflow_hook(&hook_path)? {
                    std::fs::remove_file(&hook_path)
                        .with_context(|| format!("Failed to remove {} hook", hook_name))?;
                }
            }
        }
        Ok(())
    }

    fn is_devflow_hook(&self, hook_path: &Path) -> Result<bool> {
        if !hook_path.exists() {
            return Ok(false);
        }
        let content = std::fs::read_to_string(hook_path)
            .with_context(|| format!("Failed to read hook: {}", hook_path.display()))?;
        Ok(content.contains("devflow"))
    }

    // ── Meta ───────────────────────────────────────────────────────

    fn provider_name(&self) -> &'static str {
        "jj"
    }

    fn repo_root(&self) -> &Path {
        &self.root
    }

    fn list_ignored_files(&self) -> Result<Vec<PathBuf>> {
        // For colocated repos (`.jj` + `.git`), shell out to git which
        // understands .gitignore rules natively.
        let git_dir = self.root.join(".git");
        if git_dir.exists() {
            let output = Command::new("git")
                .args(["ls-files", "--others", "--ignored", "--exclude-standard"])
                .current_dir(&self.root)
                .output()
                .context("Failed to run 'git ls-files' for ignored file enumeration")?;

            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let files: Vec<PathBuf> = stdout
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(PathBuf::from)
                    .collect();
                return Ok(files);
            }
        }

        // Pure jj repos: no reliable way to enumerate ignored files yet.
        // jj uses .gitignore patterns but doesn't expose a "list ignored" command.
        Ok(Vec::new())
    }

    fn staged_diff(&self) -> Result<String> {
        // In jj, the working copy *is* the staging area. `jj diff` shows
        // changes in the current working-copy commit.
        let output = Command::new("jj")
            .args(["diff", "--no-pager", "--color=never"])
            .current_dir(&self.root)
            .output()
            .context("Failed to run 'jj diff'")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("jj diff failed: {}", stderr);
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    fn staged_summary(&self) -> Result<String> {
        let output = Command::new("jj")
            .args(["diff", "--stat", "--no-pager", "--color=never"])
            .current_dir(&self.root)
            .output()
            .context("Failed to run 'jj diff --stat'")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("jj diff --stat failed: {}", stderr);
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    fn has_staged_changes(&self) -> Result<bool> {
        // In jj, check if the working copy has any modifications
        let output = Command::new("jj")
            .args(["diff", "--stat", "--no-pager", "--color=never"])
            .current_dir(&self.root)
            .output()
            .context("Failed to run 'jj diff --stat'")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("jj diff --stat failed: {}", stderr);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(!stdout.trim().is_empty())
    }

    fn commit(&self, message: &str) -> Result<()> {
        // In jj, `jj commit -m "msg"` finalizes the working-copy commit
        // and starts a new empty one.
        let output = Command::new("jj")
            .args(["commit", "-m", message, "--no-pager"])
            .current_dir(&self.root)
            .output()
            .context("Failed to run 'jj commit'")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("jj commit failed: {}", stderr);
        }

        Ok(())
    }
}

impl JjRepository {
    /// Initialize a new jj repository at `path` by shelling out to `jj init`.
    ///
    /// When `colocate` is true, passes `--colocate` so the repo also has a
    /// `.git/` directory (the most common setup for devflow).
    ///
    /// Requires the `jj` CLI to be installed.
    pub fn init<P: AsRef<Path>>(path: P, colocate: bool) -> Result<Self> {
        let path = path.as_ref();
        let mut args = vec!["init"];
        if colocate {
            args.push("--colocate");
        }

        let output = Command::new("jj")
            .args(&args)
            .current_dir(path)
            .output()
            .context("Failed to run 'jj init'. Is Jujutsu installed?")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("jj init failed: {}", stderr.trim());
        }

        Self::new(path)
    }

    /// Convert a branch name to a workspace-safe name.
    /// Replaces `/` with `-` (same convention as Git worktrees).
    fn workspace_name_for_branch(branch: &str) -> String {
        branch.replace('/', "-")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workspace_name_for_branch() {
        assert_eq!(
            JjRepository::workspace_name_for_branch("feature/auth"),
            "feature-auth"
        );
        assert_eq!(JjRepository::workspace_name_for_branch("main"), "main");
        assert_eq!(
            JjRepository::workspace_name_for_branch("fix/deep/nested/branch"),
            "fix-deep-nested-branch"
        );
    }
}
