//! Non-mutating checks for workspace-model invariants.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::Config;
use crate::vcs::{VcsProvider, WorktreeInfo};

/// The raw workspace physically occupying Git's primary checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitPrimaryWorkspace {
    pub path: PathBuf,
    /// `None` when the primary checkout has a detached HEAD.
    pub workspace: Option<String>,
}

/// A committed configuration whose default disagrees with Git's primary
/// checkout. This report is diagnostic only and never rewrites the config or
/// changes HEAD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitPrimaryWorkspaceMismatch {
    pub configured_workspace: String,
    pub primary: GitPrimaryWorkspace,
}

impl GitPrimaryWorkspaceMismatch {
    /// Actionable, user-facing remediation shared by every frontend.
    pub fn diagnostic(&self) -> String {
        match self.primary.workspace.as_deref() {
            Some(physical) => format!(
                "Git primary workspace mismatch: .devflow.yml configures '{}' as the default, but the primary checkout at '{}' contains '{}'. Either check out '{}' in the primary checkout or update git.main_workspace in .devflow.yml to '{}'.",
                self.configured_workspace,
                self.primary.path.display(),
                physical,
                self.configured_workspace,
                physical,
            ),
            None => format!(
                "Git primary workspace mismatch: .devflow.yml configures '{}' as the default, but the primary checkout at '{}' has a detached HEAD. Check out '{}' in the primary checkout, or attach the primary checkout to a branch and update git.main_workspace in .devflow.yml to that raw branch name.",
                self.configured_workspace,
                self.primary.path.display(),
                self.configured_workspace,
            ),
        }
    }
}

/// Inspect Git's physical primary checkout without changing VCS or config.
/// Returns `None` for non-Git providers.
pub fn inspect_git_primary_workspace(
    provider: &dyn VcsProvider,
) -> Result<Option<GitPrimaryWorkspace>> {
    if provider.provider_name() != "git" {
        return Ok(None);
    }

    let worktrees = provider
        .list_worktrees()
        .context("Failed to inspect Git worktrees")?;
    inspect_git_primary_workspace_from(provider, &worktrees)
        .map(Some)
        .context("Git did not report its physical primary checkout")
}

/// Like [`inspect_git_primary_workspace`], but reuses an already-fetched
/// worktree listing so callers that just enumerated worktrees don't pay for
/// a second full enumeration (one `Repository::open` per linked worktree).
pub fn inspect_git_primary_workspace_from(
    provider: &dyn VcsProvider,
    worktrees: &[WorktreeInfo],
) -> Option<GitPrimaryWorkspace> {
    if provider.provider_name() != "git" {
        return None;
    }

    worktrees
        .iter()
        .find(|worktree| worktree.is_main)
        .map(|primary| GitPrimaryWorkspace {
            path: primary.path.clone(),
            workspace: primary.workspace.clone(),
        })
}

/// True when the checkout at `path` is mid-operation (rebase, bisect, merge,
/// cherry-pick, …). Its HEAD is expected to be transiently detached then and
/// must not trip the primary/default invariant: the state is not devflow's
/// doing and resolves by itself when the operation finishes.
fn git_operation_in_progress(path: &Path) -> bool {
    git2::Repository::open(path)
        .map(|repo| repo.state() != git2::RepositoryState::Clean)
        .unwrap_or(false)
}

/// Return a mismatch when the configured default does not occupy Git's
/// physical primary checkout. Explicit config remains authoritative; this
/// check reports the disagreement rather than silently rewriting it.
pub fn git_primary_workspace_mismatch(
    config: &Config,
    provider: &dyn VcsProvider,
) -> Result<Option<GitPrimaryWorkspaceMismatch>> {
    if provider.provider_name() != "git" {
        return Ok(None);
    }
    let worktrees = provider
        .list_worktrees()
        .context("Failed to inspect Git worktrees")?;
    Ok(git_primary_workspace_mismatch_from(
        config, provider, &worktrees,
    ))
}

/// Like [`git_primary_workspace_mismatch`], but reuses an already-fetched
/// worktree listing.
pub fn git_primary_workspace_mismatch_from(
    config: &Config,
    provider: &dyn VcsProvider,
    worktrees: &[WorktreeInfo],
) -> Option<GitPrimaryWorkspaceMismatch> {
    let primary = inspect_git_primary_workspace_from(provider, worktrees)?;

    if primary.workspace.as_deref() == Some(config.git.main_workspace.as_str()) {
        return None;
    }

    // A detached primary HEAD during an in-progress git operation (rebase,
    // bisect, …) is transient: blocking every switch/link across the whole
    // project for its duration would fail operations that never touch the
    // primary checkout.
    if primary.workspace.is_none() && git_operation_in_progress(&primary.path) {
        log::debug!(
            "Primary checkout at '{}' is mid-operation (rebase/bisect/merge); skipping the primary/default invariant",
            primary.path.display()
        );
        return None;
    }

    Some(GitPrimaryWorkspaceMismatch {
        configured_workspace: config.git.main_workspace.clone(),
        primary,
    })
}

/// Enforce the Git primary/default invariant before a workspace lifecycle
/// operation can materialize resources using a contradictory default.
///
/// Enforcement requires `git.main_workspace` to be literally configured
/// (committed file or local override). The serde default is "main", so
/// hard-failing on it would block every switch/link in repos whose primary
/// branch is e.g. `master` the moment any config file exists — the same
/// hazard the jj provider guards against. A defaulted mismatch is only
/// logged here; the workspace inventory still surfaces it as a warning.
pub fn ensure_git_primary_workspace_matches_config(
    config: &Config,
    provider: &dyn VcsProvider,
) -> Result<()> {
    if let Some(mismatch) = git_primary_workspace_mismatch(config, provider)? {
        let explicitly_configured = config
            .project_root
            .as_deref()
            .and_then(|root| {
                crate::config::explicit_main_workspace_for_dir(root)
                    .ok()
                    .flatten()
            })
            .is_some();
        if !explicitly_configured {
            log::warn!("{}", mismatch.diagnostic());
            return Ok(());
        }
        anyhow::bail!(mismatch.diagnostic());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::{GitRepository, VcsProvider};

    #[test]
    fn reports_primary_mismatch_without_overriding_explicit_config() {
        let project = tempfile::tempdir().unwrap();
        let repo = GitRepository::init(project.path()).unwrap();
        repo.create_workspace("feature/primary", Some("main"))
            .unwrap();
        let raw = git2::Repository::open(project.path()).unwrap();
        raw.set_head("refs/heads/feature/primary").unwrap();

        let config = Config::default();
        let mismatch = git_primary_workspace_mismatch(&config, &repo)
            .unwrap()
            .unwrap();
        assert_eq!(mismatch.configured_workspace, "main");
        assert_eq!(
            mismatch.primary.workspace.as_deref(),
            Some("feature/primary")
        );
        assert!(mismatch.diagnostic().contains("update git.main_workspace"));

        let explicitly_aligned = Config {
            git: crate::config::GitConfig {
                main_workspace: "feature/primary".to_string(),
                ..Config::default().git
            },
            ..Config::default()
        };
        assert!(git_primary_workspace_mismatch(&explicitly_aligned, &repo)
            .unwrap()
            .is_none());
    }

    fn config_rooted_at(project_root: &Path) -> Config {
        Config {
            project_root: Some(project_root.to_path_buf()),
            ..Config::default()
        }
    }

    #[test]
    fn detached_primary_is_an_actionable_mismatch() {
        let project = tempfile::tempdir().unwrap();
        let repo = GitRepository::init(project.path()).unwrap();
        std::fs::write(
            project.path().join(".devflow.yml"),
            "git:\n  main_workspace: main\n",
        )
        .unwrap();
        let raw = git2::Repository::open(project.path()).unwrap();
        let head = raw.head().unwrap().target().unwrap();
        raw.set_head_detached(head).unwrap();

        let config = config_rooted_at(project.path());
        let mismatch = git_primary_workspace_mismatch(&config, &repo)
            .unwrap()
            .unwrap();
        assert_eq!(mismatch.primary.workspace, None);
        assert!(mismatch.diagnostic().contains("detached HEAD"));
        assert!(ensure_git_primary_workspace_matches_config(&config, &repo).is_err());
    }

    #[test]
    fn defaulted_main_workspace_is_never_enforced() {
        // A repo whose primary branch is not "main" and whose config never
        // sets git.main_workspace must not have every switch/link blocked by
        // the serde default; the mismatch stays a diagnostic.
        let project = tempfile::tempdir().unwrap();
        let repo = GitRepository::init(project.path()).unwrap();
        repo.create_workspace("master-like", Some("main")).unwrap();
        let raw = git2::Repository::open(project.path()).unwrap();
        raw.set_head("refs/heads/master-like").unwrap();
        std::fs::write(project.path().join(".devflow.yml"), "behavior: {}\n").unwrap();

        let config = config_rooted_at(project.path());
        assert!(git_primary_workspace_mismatch(&config, &repo)
            .unwrap()
            .is_some());
        assert!(ensure_git_primary_workspace_matches_config(&config, &repo).is_ok());

        std::fs::write(
            project.path().join(".devflow.yml"),
            "git:\n  main_workspace: main\n",
        )
        .unwrap();
        assert!(ensure_git_primary_workspace_matches_config(&config, &repo).is_err());
    }

    #[test]
    fn detached_primary_mid_rebase_is_tolerated_as_transient() {
        // A rebase/bisect in the primary checkout detaches HEAD transiently;
        // that must not block switch/link operations elsewhere in the
        // project for the duration of the operation.
        let project = tempfile::tempdir().unwrap();
        let repo = GitRepository::init(project.path()).unwrap();
        let raw = git2::Repository::open(project.path()).unwrap();
        let head = raw.head().unwrap().target().unwrap();
        raw.set_head_detached(head).unwrap();
        std::fs::create_dir_all(project.path().join(".git").join("rebase-merge")).unwrap();

        assert!(git_primary_workspace_mismatch(&Config::default(), &repo)
            .unwrap()
            .is_none());
        assert!(ensure_git_primary_workspace_matches_config(&Config::default(), &repo).is_ok());
    }
}
