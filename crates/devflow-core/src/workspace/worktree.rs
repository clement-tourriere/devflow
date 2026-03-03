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
///
/// `copy_files_override` overrides `config.worktree.copy_files` when set.
/// `copy_ignored_override` overrides `config.worktree.copy_ignored` when set.
pub fn create_worktree_with_files(
    vcs: &dyn VcsProvider,
    config: &Config,
    project_dir: &Path,
    workspace_name: &str,
    copy_files_override: Option<&[String]>,
    copy_ignored_override: Option<bool>,
) -> Result<WorktreeSetupResult> {
    // Check for existing worktree first
    if let Some(existing_path) = vcs.worktree_path(workspace_name)? {
        let resolved = std::fs::canonicalize(&existing_path).unwrap_or(existing_path);
        return Ok(WorktreeSetupResult {
            path: resolved,
            created: false,
        });
    }

    // Resolve target path
    let wt_path = resolve_worktree_path(config, project_dir, workspace_name);

    // Create the worktree via git2 (instant checkout of tracked files only)
    vcs.create_worktree(workspace_name, &wt_path)
        .with_context(|| {
            format!(
                "Failed to create worktree for workspace '{}'",
                workspace_name
            )
        })?;

    // Copy configured files from main worktree
    if let Some(ref wt_config) = config.worktree {
        use rayon::prelude::*;

        let main_dir = vcs
            .main_worktree_dir()
            .unwrap_or_else(|| project_dir.to_path_buf());

        // Use overrides if provided, otherwise fall back to config values.
        let files_to_copy = copy_files_override.unwrap_or(&wt_config.copy_files);
        let copy_ignored = copy_ignored_override.unwrap_or(wt_config.copy_ignored);

        // Copy explicitly listed files/directories using parallel reflink.
        files_to_copy.par_iter().for_each(|entry| {
            let src = main_dir.join(entry);
            let dst = wt_path.join(entry);
            if src.is_dir() {
                reflink_copy_dir(&src, &dst);
            } else if src.is_file() {
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                if let Err(e) = reflink_copy::reflink_or_copy(&src, &dst) {
                    log::warn!("Failed to copy '{}' to worktree: {}", entry, e);
                }
            }
        });

        // Copy gitignored entries (node_modules, .venv, target, etc.) from the
        // main worktree using parallel reflink.
        //
        // Uses list_ignored_entries() which returns collapsed directory-level
        // entries (e.g. "node_modules" as one entry) instead of
        // list_ignored_files() which would recurse and enumerate every file
        // inside each ignored directory.
        if copy_ignored {
            if let Ok(ignored_entries) = vcs.list_ignored_entries() {
                ignored_entries.par_iter().for_each(|rel_path| {
                    let src = main_dir.join(rel_path);
                    let dst = wt_path.join(rel_path);
                    if !src.exists() || dst.exists() {
                        return;
                    }
                    if src.is_dir() {
                        reflink_copy_dir(&src, &dst);
                    } else if src.is_file() {
                        if let Some(parent) = dst.parent() {
                            std::fs::create_dir_all(parent).ok();
                        }
                        if let Err(e) = reflink_copy::reflink_or_copy(&src, &dst) {
                            log::warn!(
                                "Failed to copy ignored entry '{}': {}",
                                rel_path.display(),
                                e
                            );
                        }
                    }
                });
            }
        }
    }

    let resolved = std::fs::canonicalize(&wt_path).unwrap_or(wt_path);
    Ok(WorktreeSetupResult {
        path: resolved,
        created: true,
    })
}

/// Recursively copy a directory using parallel reflink (CoW) per file.
///
/// Uses rayon's work-stealing thread pool to copy files across all CPU
/// cores.  Directory creation is sequential (must happen before children),
/// but file copies within each directory level run in parallel.
///
/// Non-fatal on errors — logs warnings and continues.
pub fn reflink_copy_dir(src: &Path, dst: &Path) {
    use rayon::prelude::*;

    let entries: Vec<_> = match std::fs::read_dir(src) {
        Ok(iter) => iter.filter_map(|e| e.ok()).collect(),
        Err(e) => {
            log::warn!("Failed to read directory '{}': {}", src.display(), e);
            return;
        }
    };

    if let Err(e) = std::fs::create_dir_all(dst) {
        log::warn!("Failed to create directory '{}': {}", dst.display(), e);
        return;
    }

    entries.par_iter().for_each(|entry| {
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            reflink_copy_dir(&src_path, &dst_path);
        } else if src_path.is_file() {
            if let Err(e) = reflink_copy::reflink_or_copy(&src_path, &dst_path) {
                log::warn!(
                    "Failed to reflink copy '{}': {}",
                    src_path.display(),
                    e
                );
            }
        }
    });
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
