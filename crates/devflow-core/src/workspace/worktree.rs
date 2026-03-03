use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::vcs::VcsProvider;

use super::WorktreeSetupResult;

/// Derive the repository name for worktree path templates.
///
/// Uses `config.name` if set, falls back to the project directory name, and
/// ultimately defaults to `"repo"`.
pub fn resolve_repo_name(config: &Config, project_dir: &Path) -> String {
    config
        .name
        .as_ref()
        .filter(|n| !n.trim().is_empty())
        .cloned()
        .or_else(|| {
            project_dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .filter(|n| !n.trim().is_empty())
        })
        .unwrap_or_else(|| "repo".to_string())
}

/// Expand `{repo}` and `{workspace}` (plus legacy `{branch}`) placeholders in
/// the worktree path template.
pub fn apply_worktree_path_template(
    path_template: &str,
    repo_name: &str,
    workspace_name: &str,
) -> String {
    path_template
        .replace("{repo}", repo_name)
        .replace("{workspace}", workspace_name)
        // Backward compatibility with legacy templates.
        .replace("{branch}", workspace_name)
}

/// Resolve the full worktree path for a workspace.
///
/// Applies the config path template (or the default `../{repo}.{workspace}`)
/// and joins it relative to the project directory.
pub fn resolve_worktree_path(
    config: &Config,
    project_dir: &Path,
    workspace_name: &str,
) -> PathBuf {
    let repo_name = resolve_repo_name(config, project_dir);
    let normalized = config.get_normalized_workspace_name(workspace_name);
    let path_template = config
        .worktree
        .as_ref()
        .map(|wt| wt.path_template.as_str())
        .unwrap_or("../{repo}.{workspace}");
    let wt_path_str = apply_worktree_path_template(path_template, &repo_name, &normalized);
    project_dir.join(wt_path_str)
}

/// Create a worktree for the given workspace and copy configured files.
///
/// Returns `Ok(result)` with details about the created worktree, or reuses
/// an existing worktree if one is already present for the workspace.
pub fn create_worktree_with_files(
    vcs: &dyn VcsProvider,
    config: &Config,
    project_dir: &Path,
    workspace_name: &str,
) -> Result<WorktreeSetupResult> {
    // Check for existing worktree first
    if let Some(existing_path) = vcs.worktree_path(workspace_name)? {
        let resolved = std::fs::canonicalize(&existing_path).unwrap_or(existing_path);
        return Ok(WorktreeSetupResult {
            path: resolved,
            cow_used: false,
            created: false,
        });
    }

    // Resolve target path
    let wt_path = resolve_worktree_path(config, project_dir, workspace_name);

    // Create the worktree
    let wt_result = vcs
        .create_worktree(workspace_name, &wt_path)
        .with_context(|| {
            format!(
                "Failed to create worktree for workspace '{}'",
                workspace_name
            )
        })?;

    // Copy configured files from main worktree
    if let Some(ref wt_config) = config.worktree {
        let main_dir = vcs
            .main_worktree_dir()
            .unwrap_or_else(|| project_dir.to_path_buf());

        // Copy explicitly listed files.
        // When CoW was used, these already exist as clones — overwrite with
        // independent copies so they can diverge between workspaces.
        for file in &wt_config.copy_files {
            let src = main_dir.join(file);
            let dst = wt_path.join(file);
            if src.exists() {
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                // Remove existing clone first so the new copy is independent
                if wt_result.cow_used && dst.exists() {
                    let _ = std::fs::remove_file(&dst);
                }
                if let Err(e) = std::fs::copy(&src, &dst) {
                    log::warn!("Failed to copy '{}' to worktree: {}", file, e);
                }
            }
        }

        // Copy gitignored files — skip when CoW already cloned everything
        if !wt_result.cow_used && wt_config.copy_ignored {
            if let Ok(ignored_files) = vcs.list_ignored_files() {
                for rel_path in &ignored_files {
                    let src = main_dir.join(rel_path);
                    let dst = wt_path.join(rel_path);
                    if src.exists() && !dst.exists() {
                        if let Some(parent) = dst.parent() {
                            std::fs::create_dir_all(parent).ok();
                        }
                        if let Err(e) = std::fs::copy(&src, &dst) {
                            log::warn!(
                                "Failed to copy ignored file '{}': {}",
                                rel_path.display(),
                                e
                            );
                        }
                    }
                }
            }
        }
    }

    let resolved = std::fs::canonicalize(&wt_path).unwrap_or(wt_path);
    Ok(WorktreeSetupResult {
        path: resolved,
        cow_used: wt_result.cow_used,
        created: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_worktree_path_template() {
        assert_eq!(
            apply_worktree_path_template("../{repo}.{workspace}", "myapp", "feature-auth"),
            "../myapp.feature-auth"
        );
    }

    #[test]
    fn test_apply_worktree_path_template_legacy_branch() {
        assert_eq!(
            apply_worktree_path_template("../{repo}.{branch}", "myapp", "feature-auth"),
            "../myapp.feature-auth"
        );
    }

    #[test]
    fn test_resolve_repo_name_from_config() {
        let mut config = Config::default();
        config.name = Some("my-project".to_string());
        assert_eq!(resolve_repo_name(&config, Path::new("/tmp/foo")), "my-project");
    }

    #[test]
    fn test_resolve_repo_name_from_dir() {
        let config = Config::default();
        assert_eq!(resolve_repo_name(&config, Path::new("/tmp/foo")), "foo");
    }

    #[test]
    fn test_resolve_repo_name_fallback() {
        let config = Config::default();
        assert_eq!(resolve_repo_name(&config, Path::new("/")), "repo");
    }
}
