//! Jujutsu (jj) VCS provider.
//!
//! Maps jj concepts to devflow's VcsProvider trait:
//! - jj **bookmarks** → workspaces
//! - jj **workspaces** → worktrees
//! - jj **colocated repos** are supported (`.jj` + `.git` side by side)
//!
//! This provider shells out to the `jj` CLI since there is no stable Rust
//! library equivalent to `git2`. Commands are run with `--no-pager` and
//! `--color=never` for machine-friendly output.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

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
struct JjWorkspaceEntry {
    internal_name: String,
    path: PathBuf,
}

const WORKSPACE_LIST_TEMPLATE: &str =
    r#"json(self.name()) ++ "\t" ++ json(stringify(self.root())) ++ "\n""#;
const BOOKMARK_LIST_TEMPLATE: &str =
    r#"if(!self.remote() && self.present(), json(self.name()) ++ "\n")"#;

impl JjRepository {
    /// Open the jj repository at `path` (or a parent containing `.jj/`).
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

    fn parse_workspace_entries(output: &str) -> Result<Vec<JjWorkspaceEntry>> {
        output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let (internal_name, path) = line.split_once('\t').with_context(|| {
                    format!("Unexpected `jj workspace list` template output: {line:?}")
                })?;
                let internal_name: String =
                    serde_json::from_str(internal_name).with_context(|| {
                        format!("Invalid workspace-name JSON from `jj workspace list`: {line:?}")
                    })?;
                let path: String =
                    serde_json::from_str(path.trim_end_matches('\r')).with_context(|| {
                        format!("Invalid workspace-path JSON from `jj workspace list`: {line:?}")
                    })?;
                if internal_name.is_empty() || path.is_empty() {
                    bail!("Unexpected empty field in `jj workspace list` output: {line:?}");
                }
                let path = PathBuf::from(path);
                let path = path.canonicalize().unwrap_or(path);
                Ok(JjWorkspaceEntry {
                    internal_name,
                    path,
                })
            })
            .collect()
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
        let output = self.jj(&["workspace", "list", "--template", WORKSPACE_LIST_TEMPLATE])?;
        Self::parse_workspace_entries(&output)
    }

    fn load_configured_default_workspace(primary_root: &Path) -> Result<Option<String>> {
        // Honor `git.main_workspace` only when the key is literally present
        // (committed file or local override); the serde default would
        // hard-fail every jj repo whose default bookmark is e.g. `master`.
        crate::config::explicit_main_workspace_for_dir(primary_root)
    }

    fn bookmark_names(&self) -> Result<Vec<String>> {
        let output = self.jj(&["bookmark", "list", "--template", BOOKMARK_LIST_TEMPLATE])?;
        output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str::<String>(line.trim_end_matches('\r')).with_context(|| {
                    format!("Invalid bookmark-name JSON from `jj bookmark list`: {line:?}")
                })
            })
            .collect()
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

    /// Whether `bookmark` points at an ancestor of (or exactly) `revision`,
    /// i.e. moving it to `revision` is a pure forward move.
    fn bookmark_is_ancestor_of(&self, bookmark: &str, revision: &str) -> Result<bool> {
        let revset = format!("{} & ::{revision}", Self::bookmark_revset(bookmark));
        let output = self.jj(&["log", "--no-graph", "-r", &revset, "--template", "\"x\""])?;
        Ok(!output.trim().is_empty())
    }

    fn local_bookmark_removal_args(name: &str) -> Vec<String> {
        vec![
            "bookmark".to_owned(),
            "forget".to_owned(),
            format!("exact:{name}"),
        ]
    }

    fn bookmark_revset(name: &str) -> String {
        let quoted = serde_json::to_string(name).expect("serializing a Rust string cannot fail");
        format!("bookmarks(exact:{quoted})")
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

    fn revision_for_workspace(&self, raw_workspace: &str) -> Result<String> {
        let entries = self.workspace_entries()?;
        let default = self.detect_default_bookmark()?;
        Ok(
            Self::internal_workspace_for_raw(&entries, raw_workspace, default.as_deref())
                .map(|internal_name| format!("{internal_name}@"))
                .unwrap_or_else(|| Self::bookmark_revset(raw_workspace)),
        )
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
        let bookmarks = self.bookmark_names()?;
        let default = self.detect_default_bookmark()?;
        Ok(self
            .workspace_entries()?
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
        let names = self.bookmark_names()?;
        let current = self.current_workspace()?;
        let default = self.detect_default_bookmark()?;

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
        if self.workspace_exists(name)? {
            log::info!("jj bookmark '{}' already exists, reusing", name);
            return Ok(());
        }

        // Create the bookmark at an explicit revision without changing the
        // source workspace's working-copy commit.
        let revision = match base {
            Some(base) => self.revision_for_workspace(base)?,
            None => "@".to_owned(),
        };
        self.jj(&["bookmark", "create", "--revision", &revision, name])?;
        Ok(())
    }

    fn delete_workspace(&self, name: &str) -> Result<()> {
        // `bookmark delete` records a remote deletion which a later push can
        // propagate. A devflow workspace removal, like deleting a local Git
        // branch, must be local-only.
        let args = Self::local_bookmark_removal_args(name);
        let args = args.iter().map(String::as_str).collect::<Vec<_>>();
        self.jj(&args)?;
        Ok(())
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
        let bookmarks = self.bookmark_names()?;
        let default = self.detect_default_bookmark()?;

        Ok(self
            .workspace_entries()?
            .into_iter()
            .map(|entry| WorktreeInfo {
                is_main: Self::paths_equal(&entry.path, &self.primary_root),
                workspace: Self::raw_workspace_for_internal(
                    &entry.internal_name,
                    &bookmarks,
                    default.as_deref(),
                ),
                path: entry.path,
                is_locked: false,
            })
            .collect())
    }

    fn create_worktree(&self, workspace: &str, path: &Path) -> Result<WorktreeCreateResult> {
        if !self.workspace_exists(workspace)? {
            bail!("jj bookmark '{workspace}' does not exist");
        }

        let internal_name = Self::workspace_name_for_branch(workspace);
        if self
            .workspace_entries()?
            .iter()
            .any(|entry| entry.internal_name == internal_name)
        {
            bail!("jj workspace '{internal_name}' already exists");
        }

        let path_str = path.to_str().context("Worktree path is not valid UTF-8")?;
        let revision = Self::bookmark_revset(workspace);

        // This creates a fresh working-copy commit on top of the bookmark. It
        // does not edit (and therefore cannot stale) the source workspace.
        // Any failure is returned to the caller.
        self.jj(&[
            "workspace",
            "add",
            "--name",
            &internal_name,
            "--revision",
            &revision,
            path_str,
        ])?;

        // jj has no active-bookmark concept. Attach the raw identity to the
        // initial working-copy commit. `devflow commit` refreshes it after
        // each commit, and removal refreshes it once more before forgetting
        // the native workspace.
        let working_copy_revision = format!("{internal_name}@");
        if let Err(error) = self.jj(&[
            "bookmark",
            "set",
            "--revision",
            &working_copy_revision,
            workspace,
        ]) {
            let _ = self.jj(&["workspace", "forget", &internal_name]);
            if path.exists() {
                let _ = std::fs::remove_dir_all(path);
            }
            return Err(error.context(format!(
                "Failed to attach jj bookmark '{workspace}' to its new workspace"
            )));
        }

        Ok(WorktreeCreateResult::new())
    }

    fn remove_worktree(&self, path: &Path, force: bool) -> Result<()> {
        let entry = self
            .workspace_entries()?
            .into_iter()
            .find(|entry| Self::paths_equal(&entry.path, path))
            .with_context(|| format!("No jj workspace is registered at '{}'", path.display()))?;

        if Self::paths_equal(&entry.path, &self.primary_root) {
            bail!("Cannot remove jj's primary workspace");
        }
        if Self::paths_equal(&entry.path, &self.root) {
            bail!(
                "Cannot remove the current jj workspace; run devflow from another workspace first"
            );
        }

        if !force && entry.path.exists() && self.worktree_is_dirty(&entry.path)? {
            bail!(
                "jj workspace at '{}' has uncommitted changes; commit them or use --force",
                entry.path.display()
            );
        }

        // Preserve the exact workspace head before its native identity is
        // forgotten. This also repairs a bookmark left behind by commits made
        // directly with `jj commit` instead of `devflow commit`. Best-effort
        // only: jj refuses backwards/sideways bookmark moves (e.g. after the
        // user ran `jj edit main` inside the workspace), and failing here
        // must never block removal — aborting before `workspace forget`
        // would leave the workspace registered while the force path deletes
        // its directory, permanently blocking re-creation of that branch.
        let bookmarks = self.bookmark_names()?;
        let default = self.detect_default_bookmark()?;
        if let Some(raw_workspace) =
            Self::raw_workspace_for_internal(&entry.internal_name, &bookmarks, default.as_deref())
        {
            let working_copy_revision = format!("{}@", entry.internal_name);
            if let Err(error) = self.jj(&[
                "bookmark",
                "set",
                "--revision",
                &working_copy_revision,
                &raw_workspace,
            ]) {
                log::warn!(
                    "Not preserving jj bookmark '{raw_workspace}' before removing its workspace: {error:#}"
                );
            }
        }

        self.jj(&["workspace", "forget", &entry.internal_name])?;
        if entry.path.exists() {
            std::fs::remove_dir_all(&entry.path).with_context(|| {
                format!(
                    "Failed to remove jj workspace directory '{}'",
                    entry.path.display()
                )
            })?;
        }

        Ok(())
    }

    fn worktree_is_dirty(&self, path: &Path) -> Result<bool> {
        let entry = self
            .workspace_entries()?
            .into_iter()
            .find(|entry| Self::paths_equal(&entry.path, path))
            .with_context(|| format!("No jj workspace is registered at '{}'", path.display()))?;
        if !entry.path.exists() {
            return Ok(false);
        }
        let path_str = entry
            .path
            .to_str()
            .context("Worktree path is not valid UTF-8")?;
        let output = Command::new("jj")
            .args([
                "--no-pager",
                "--color=never",
                "--repository",
                path_str,
                "diff",
                "--summary",
            ])
            .output()
            .context("Failed to inspect jj workspace changes")?;
        if !output.status.success() {
            bail!(
                "jj diff failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(!output.stdout.is_empty())
    }

    fn worktree_path(&self, workspace: &str) -> Result<Option<PathBuf>> {
        let entries = self.workspace_entries()?;
        let default = self.detect_default_bookmark()?;
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

    // ── Hooks ──────────────────────────────────────────────────────

    fn install_hooks(&self) -> Result<()> {
        // jj doesn't have native hook support (as of 0.24+).
        // For colocated repos, install into .git/hooks (same as Git).
        // For pure jj repos, we rely on devflow's own hook engine triggered
        // by `devflow git-hook` which the user runs manually or via shell integration.
        let git_hooks_dir = self.primary_root.join(".git").join("hooks");
        if git_hooks_dir.parent().map(|p| p.exists()).unwrap_or(false) {
            // Colocated repo — install into .git/hooks
            std::fs::create_dir_all(&git_hooks_dir).context("Failed to create hooks directory")?;

            let hook_script = self.generate_hook_script();

            let hook_name = "post-checkout";
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

            log::info!("Installed hooks into colocated .git/hooks");
        } else {
            // Pure jj repo — no hook directory to install into.
            // Keep guidance on stderr through the logging layer. Core library
            // code must never write to stdout because JSON-mode callers own
            // stdout's single-document protocol.
            log::warn!(
                "jj does not support native hooks. Use 'devflow git-hook' via shell integration."
            );
        }

        Ok(())
    }

    fn uninstall_hooks(&self) -> Result<()> {
        // Only relevant for colocated repos
        let git_hooks_dir = self.primary_root.join(".git").join("hooks");
        if git_hooks_dir.exists() {
            let hook_name = "post-checkout";
            let hook_path = git_hooks_dir.join(hook_name);
            if hook_path.exists() && self.is_devflow_hook(&hook_path)? {
                std::fs::remove_file(&hook_path)
                    .with_context(|| format!("Failed to remove {} hook", hook_name))?;
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
        &self.primary_root
    }

    fn list_ignored_files(&self) -> Result<Vec<PathBuf>> {
        // For colocated repos (`.jj` + `.git`), shell out to git which
        // understands .gitignore rules natively.
        let git_dir = self.primary_root.join(".git");
        if git_dir.exists() {
            let output = Command::new("git")
                .args(["ls-files", "--others", "--ignored", "--exclude-standard"])
                .current_dir(&self.primary_root)
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
        let entries = self.workspace_entries()?;
        let bookmarks = self.bookmark_names()?;
        let default = self.detect_default_bookmark()?;
        let raw_workspace = entries
            .iter()
            .find(|entry| Self::paths_equal(&entry.path, &self.root))
            .and_then(|entry| {
                Self::raw_workspace_for_internal(
                    &entry.internal_name,
                    &bookmarks,
                    default.as_deref(),
                )
            });

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

        // Bookmarks do not automatically advance to the new committed child
        // in jj. Keep devflow's raw workspace identity at the commit just
        // finalized (`@-`); the new `@` is the empty working-copy commit.
        // Advance only when the bookmark is an ancestor of that commit: in
        // the primary workspace the mapped bookmark is the DEFAULT bookmark,
        // and `@` may sit on an unrelated lineage (e.g. after `jj new
        // feature-x`) — advancing would silently pull foreign commits onto
        // the default bookmark, and a sideways refusal would fail the
        // command after the commit already landed.
        if let Some(raw_workspace) = raw_workspace {
            match self.bookmark_is_ancestor_of(&raw_workspace, "@-") {
                Ok(true) => {
                    if let Err(error) =
                        self.jj(&["bookmark", "set", "--revision", "@-", &raw_workspace])
                    {
                        log::warn!(
                            "Commit succeeded, but did not advance jj bookmark '{raw_workspace}': {error:#}"
                        );
                    }
                }
                Ok(false) => {
                    log::warn!(
                        "Commit succeeded; leaving jj bookmark '{raw_workspace}' in place: it is not an ancestor of the committed revision"
                    );
                }
                Err(error) => {
                    log::warn!(
                        "Commit succeeded; leaving jj bookmark '{raw_workspace}' in place: {error:#}"
                    );
                }
            }
        }

        Ok(())
    }
}

impl JjRepository {
    /// Initialize a new jj repository at `path` with `jj git init`.
    ///
    /// When `colocate` is true, passes `--colocate` so the repo also has a
    /// `.git/` directory (the most common setup for devflow).
    ///
    /// Requires the `jj` CLI to be installed.
    pub fn init<P: AsRef<Path>>(path: P, colocate: bool) -> Result<Self> {
        let path = path.as_ref();
        let colocation_flag = if colocate {
            "--colocate"
        } else {
            "--no-colocate"
        };

        let output = Command::new("jj")
            .args(["git", "init", colocation_flag, "."])
            .current_dir(path)
            .output()
            .context("Failed to run 'jj git init'. Is Jujutsu installed?")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("jj git init failed: {}", stderr.trim());
        }

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
    fn parses_explicit_workspace_ref_template_output() {
        let entries = JjRepository::parse_workspace_entries(
            "\"default\"\t\"/tmp/project\"\n\"jj-workspace-feature\"\t\"/tmp/project-feature\"\n",
        )
        .unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].internal_name, "default");
        assert_eq!(entries[0].path, PathBuf::from("/tmp/project"));
        assert_eq!(entries[1].internal_name, "jj-workspace-feature");
    }

    #[test]
    fn rejects_human_workspace_list_output() {
        let error = JjRepository::parse_workspace_entries("default: abc123 (empty)\n")
            .expect_err("human output must not be interpreted as a filesystem path");
        assert!(error.to_string().contains("template output"));
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
    fn bookmark_revset_quotes_raw_identity() {
        assert_eq!(
            JjRepository::bookmark_revset("feature/quoted name"),
            r#"bookmarks(exact:"feature/quoted name")"#
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

    #[test]
    fn workspace_removal_forgets_local_bookmark_without_scheduling_remote_delete() {
        assert_eq!(
            JjRepository::local_bookmark_removal_args("feature/auth"),
            vec!["bookmark", "forget", "exact:feature/auth"]
        );
    }
}
