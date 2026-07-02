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
pub fn resolve_worktree_path(config: &Config, project_dir: &Path, workspace_name: &str) -> PathBuf {
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

    // Create the worktree via git2 (checkout of tracked files only; libgit2's
    // single-threaded checkout can still take a while on large repos)
    let checkout_started = std::time::Instant::now();
    vcs.create_worktree(workspace_name, &wt_path)
        .with_context(|| {
            format!(
                "Failed to create worktree for workspace '{}'",
                workspace_name
            )
        })?;
    log::debug!(
        "Created worktree checkout at '{}' in {:.2?}",
        wt_path.display(),
        checkout_started.elapsed()
    );

    // Copy configured files from main worktree
    if let Some(ref wt_config) = config.worktree {
        use rayon::prelude::*;

        let main_dir = vcs
            .main_worktree_dir()
            .unwrap_or_else(|| project_dir.to_path_buf());

        // Use overrides if provided, otherwise fall back to config values.
        let files_to_copy = copy_files_override.unwrap_or(&wt_config.copy_files);
        let copy_ignored = copy_ignored_override.unwrap_or(wt_config.copy_ignored);

        let will_copy = !files_to_copy.is_empty() || copy_ignored || wt_config.copy_ai_configs;
        if will_copy {
            check_cow_support(&main_dir, &wt_path, copy_ignored);
        }

        // Copy explicitly listed files/directories using parallel reflink.
        let copy_started = std::time::Instant::now();
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
        if !files_to_copy.is_empty() {
            log::debug!(
                "Copied {} configured file(s) into worktree in {:.2?}",
                files_to_copy.len(),
                copy_started.elapsed()
            );
        }

        // Copy AI tool config directories (.claude, .cursor, etc.) if enabled.
        if wt_config.copy_ai_configs {
            let ai_copy_started = std::time::Instant::now();
            let ai_dirs: Vec<&str> = crate::config::AI_TOOL_DIRS.to_vec();
            let extra: Vec<&str> = wt_config.extra_ai_dirs.iter().map(|s| s.as_str()).collect();
            let all_ai_dirs: Vec<&str> = ai_dirs.into_iter().chain(extra).collect();

            all_ai_dirs.par_iter().for_each(|dir_name| {
                let src = main_dir.join(dir_name);
                let dst = wt_path.join(dir_name);
                if src.is_dir() && !dst.exists() {
                    reflink_copy_dir(&src, &dst);
                }
            });
            log::debug!(
                "Copied AI tool config dirs into worktree in {:.2?}",
                ai_copy_started.elapsed()
            );
        }

        // Copy gitignored entries (node_modules, .venv, target, etc.) from the
        // main worktree using parallel reflink.
        //
        // Uses list_ignored_entries() which returns collapsed directory-level
        // entries (e.g. "node_modules" as one entry) instead of
        // list_ignored_files() which would recurse and enumerate every file
        // inside each ignored directory.
        if copy_ignored {
            let ignored_copy_started = std::time::Instant::now();
            if let Ok(ignored_entries) = vcs.list_ignored_entries() {
                let entry_count = ignored_entries.len();
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
                log::debug!(
                    "Copied {} gitignored entrie(s) into worktree in {:.2?}",
                    entry_count,
                    ignored_copy_started.elapsed()
                );
            }
        }
    }

    trust_mise_configs(&wt_path);

    let resolved = std::fs::canonicalize(&wt_path).unwrap_or(wt_path);
    Ok(WorktreeSetupResult {
        path: resolved,
        created: true,
    })
}

/// Probe copy-on-write support between the main worktree and the new worktree
/// and warn when file copies will degrade to full byte copies.
///
/// The copy helpers below use `cp --reflink=auto` (Linux) which silently falls
/// back to a full copy on filesystems without reflink support (e.g. ext4), so
/// without this probe the user never learns why worktree creation is slow.
fn check_cow_support(main_dir: &Path, wt_path: &Path, copy_ignored: bool) {
    use crate::vcs::cow_worktree::{detect_cow_capability_cross, CowCapability};

    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        return;
    }

    match detect_cow_capability_cross(main_dir, wt_path) {
        CowCapability::None => {
            let hint = if copy_ignored {
                " Large gitignored directories (node_modules, target, .venv) are copied in full; \
                 consider setting 'worktree.copy_ignored: false' or using a CoW filesystem \
                 (Btrfs/XFS on Linux, APFS on macOS)."
            } else {
                " Consider a CoW filesystem (Btrfs/XFS on Linux, APFS on macOS)."
            };
            log::warn!(
                "No copy-on-write support between '{}' and '{}': worktree file copies will be \
                 full copies.{}",
                main_dir.display(),
                wt_path.display(),
                hint
            );
        }
        capability => {
            log::debug!("Worktree file copies will use CoW ({:?})", capability);
        }
    }
}

/// Copy a directory using the platform's bulk CoW-capable copy first.
///
/// A previous implementation reflinked every file one-by-one. That was correct
/// but surprisingly slow for large ignored dependency directories like
/// `.venv/` or `node_modules/`, and it dereferenced symlinks. Bulk copy keeps
/// workspace creation close to the underlying filesystem's clone speed
/// (`cp -cR` on APFS, `cp -a --reflink=auto` on Linux) and preserves symlinks.
///
/// Non-fatal on errors — logs warnings and continues.
pub fn reflink_copy_dir(src: &Path, dst: &Path) {
    if let Err(e) = copy_dir_bulk(src, dst).or_else(|bulk_err| {
        log::debug!(
            "Bulk CoW copy from '{}' to '{}' failed: {:#}; falling back to recursive copy",
            src.display(),
            dst.display(),
            bulk_err
        );
        copy_dir_recursive(src, dst)
    }) {
        log::warn!(
            "Failed to copy directory '{}' to '{}': {:#}",
            src.display(),
            dst.display(),
            e
        );
    }
}

fn trust_mise_configs(worktree_path: &Path) {
    for file_name in ["mise.toml", ".mise.toml"] {
        let path = worktree_path.join(file_name);
        if !path.is_file() {
            continue;
        }
        let output = std::process::Command::new("mise")
            .args(["trust", "-y"])
            .arg(&path)
            .output();
        match output {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                log::warn!(
                    "Failed to trust mise config '{}': {}",
                    path.display(),
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::debug!("mise not found; skipping trust for '{}'", path.display());
            }
            Err(e) => {
                log::warn!("Failed to run mise trust for '{}': {}", path.display(), e);
            }
        }
    }
}

fn copy_dir_bulk(src: &Path, dst: &Path) -> Result<()> {
    if !src.is_dir() {
        anyhow::bail!("source directory '{}' not found", src.display());
    }
    std::fs::create_dir_all(dst)
        .with_context(|| format!("failed to create directory '{}'", dst.display()))?;

    let source_dot = src.join(".");

    #[cfg(target_os = "macos")]
    {
        if run_cp(&["-cR"], &source_dot, dst).is_ok() {
            return Ok(());
        }
        run_cp(&["-R"], &source_dot, dst)
            .with_context(|| format!("failed to copy '{}'", src.display()))?;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    {
        if run_cp(&["-a", "--reflink=auto"], &source_dot, dst).is_ok() {
            return Ok(());
        }
        run_cp(&["-a"], &source_dot, dst)
            .with_context(|| format!("failed to copy '{}'", src.display()))?;
        return Ok(());
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        copy_dir_recursive(src, dst)
    }
}

fn run_cp(flags: &[&str], src: &Path, dst: &Path) -> Result<()> {
    let output = std::process::Command::new("cp")
        .args(flags)
        .arg(src)
        .arg(dst)
        .output()
        .context("failed to execute cp")?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    anyhow::bail!("cp failed: {stderr}")
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    use rayon::prelude::*;

    let entries: Vec<_> = std::fs::read_dir(src)
        .with_context(|| format!("failed to read directory '{}'", src.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;

    std::fs::create_dir_all(dst)
        .with_context(|| format!("failed to create directory '{}'", dst.display()))?;

    entries.par_iter().try_for_each(|entry| -> Result<()> {
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if file_type.is_symlink() {
            copy_symlink(&src_path, &dst_path)?;
        } else if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if file_type.is_file() {
            reflink_copy::reflink_or_copy(&src_path, &dst_path)
                .with_context(|| format!("failed to copy '{}'", src_path.display()))?;
        }
        Ok(())
    })?;

    Ok(())
}

#[cfg(unix)]
fn copy_symlink(src: &Path, dst: &Path) -> Result<()> {
    let target = std::fs::read_link(src)
        .with_context(|| format!("failed to read symlink '{}'", src.display()))?;
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(dst);
    std::os::unix::fs::symlink(&target, dst)
        .with_context(|| format!("failed to create symlink '{}'", dst.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn copy_symlink(src: &Path, dst: &Path) -> Result<()> {
    // Conservative fallback for platforms where creating a symlink may require
    // privileges: copy the target contents if it resolves to a file/directory.
    let target = std::fs::canonicalize(src)
        .with_context(|| format!("failed to resolve symlink '{}'", src.display()))?;
    if target.is_dir() {
        copy_dir_recursive(&target, dst)
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        reflink_copy::reflink_or_copy(&target, dst)
            .with_context(|| format!("failed to copy symlink target '{}'", target.display()))
    }
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
        let config = Config {
            name: Some("my-project".to_string()),
            ..Default::default()
        };
        assert_eq!(
            resolve_repo_name(&config, Path::new("/tmp/foo")),
            "my-project"
        );
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

    #[test]
    #[cfg(unix)]
    fn test_reflink_copy_dir_preserves_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(src.join("bin")).unwrap();
        std::fs::write(src.join("bin/python-real"), "python").unwrap();
        std::os::unix::fs::symlink("python-real", src.join("bin/python")).unwrap();

        reflink_copy_dir(&src, &dst);

        let copied_link = dst.join("bin/python");
        assert!(std::fs::symlink_metadata(&copied_link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_link(copied_link).unwrap(),
            PathBuf::from("python-real")
        );
    }
}
