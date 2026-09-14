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
        .replace("{branch}", workspace_name)
}

/// Resolve the full worktree path for a workspace.
///
/// Applies the configured path template with a collision-resistant workspace
/// key and joins it relative to the project directory.
pub fn resolve_worktree_path(config: &Config, project_dir: &Path, workspace_name: &str) -> PathBuf {
    let main_dir = crate::vcs::detect_vcs_provider(project_dir)
        .ok()
        .and_then(|provider| provider.main_worktree_dir())
        .unwrap_or_else(|| project_dir.to_path_buf());
    let repo_name = resolve_repo_name(config, &main_dir);
    let service_key = crate::state::LocalStateManager::new()
        .and_then(|state| state.resolve_workspace_service_key_by_dir(&main_dir, workspace_name))
        .unwrap_or_else(|_| config.get_service_workspace_key(workspace_name));
    let wt_path_str =
        apply_worktree_path_template(&config.worktree.path_template, &repo_name, &service_key);
    main_dir.join(wt_path_str)
}

/// Resolve the path a workspace selection would use without materializing it.
///
/// Existing materialized workspaces (including Git's primary checkout) take
/// precedence over the configured path template. Missing workspaces retain the
/// deterministic planned path used during creation.
pub fn resolve_existing_or_planned_worktree_path(
    config: &Config,
    project_dir: &Path,
    workspace_name: &str,
) -> PathBuf {
    if let Ok(provider) = crate::vcs::detect_vcs_provider(project_dir) {
        if let Ok(Some(existing)) = provider.worktree_path(workspace_name) {
            return existing.canonicalize().unwrap_or(existing);
        }
    }

    resolve_worktree_path(config, project_dir, workspace_name)
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
    create_worktree_with_files_at(
        vcs,
        config,
        project_dir,
        workspace_name,
        copy_files_override,
        copy_ignored_override,
        &resolve_worktree_path(config, project_dir, workspace_name),
    )
}

pub(crate) fn create_worktree_with_files_at(
    vcs: &dyn VcsProvider,
    config: &Config,
    project_dir: &Path,
    workspace_name: &str,
    copy_files_override: Option<&[String]>,
    copy_ignored_override: Option<bool>,
    planned_path: &Path,
) -> Result<WorktreeSetupResult> {
    let (wt_path, created) = if let Some(path) = vcs.worktree_path(workspace_name)? {
        vcs.prepare_worktree(&path)?;
        (path, false)
    } else {
        let path = planned_path.to_path_buf();
        vcs.create_worktree(workspace_name, &path)
            .with_context(|| {
                format!("Failed to create worktree for workspace '{workspace_name}'")
            })?;
        (path, true)
    };

    // Copy configured files from main worktree
    let main_dir = vcs
        .main_worktree_dir()
        .unwrap_or_else(|| project_dir.to_path_buf());
    if main_dir.canonicalize()? != wt_path.canonicalize()? {
        copy_worktree_payload(
            Some(vcs),
            config,
            &main_dir,
            &wt_path,
            copy_files_override,
            copy_ignored_override,
        )?;
    }
    crate::cache::materialize(config, &main_dir, &wt_path)?;
    let resolved = std::fs::canonicalize(&wt_path).unwrap_or(wt_path);
    Ok(WorktreeSetupResult {
        path: resolved,
        created,
    })
}

/// Copy only explicitly configured payloads through confined directory handles.
#[derive(Debug, serde::Serialize)]
pub struct PayloadCopyEntry {
    pub path: PathBuf,
    pub disposition: crate::files::CopyDisposition,
}
#[derive(Debug, Default, serde::Serialize)]
pub struct PayloadCopyResult {
    pub entries: Vec<PayloadCopyEntry>,
}
impl PayloadCopyResult {
    pub fn copied(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.disposition == crate::files::CopyDisposition::Copied)
            .count()
    }
}

pub fn copy_worktree_payload(
    vcs: Option<&dyn VcsProvider>,
    config: &Config,
    main_dir: &Path,
    wt_path: &Path,
    copy_files_override: Option<&[String]>,
    copy_ignored_override: Option<bool>,
) -> Result<PayloadCopyResult> {
    let source = crate::files::WorkspaceFiles::open(main_dir)?;
    let target = crate::files::WorkspaceFiles::open(wt_path)?;
    let mut paths: Vec<PathBuf> = copy_files_override
        .unwrap_or(&config.worktree.copy_files)
        .iter()
        .map(PathBuf::from)
        .collect();
    if config.worktree.copy_ai_configs {
        log::warn!("worktree.copy_ai_configs is deprecated; list the required files in worktree.copy_files");
        paths.extend(crate::config::AI_TOOL_DIRS.iter().map(PathBuf::from));
        paths.extend(config.worktree.extra_ai_dirs.iter().map(PathBuf::from));
    }
    if copy_ignored_override.unwrap_or(config.worktree.copy_ignored) {
        log::warn!("worktree.copy_ignored is deprecated; declare input-keyed caches for reusable build artifacts");
        paths.extend(
            vcs.context("copy_ignored requires a VCS provider")?
                .list_ignored_entries()?,
        );
    }
    paths.sort();
    paths.dedup();
    let mut result = PayloadCopyResult::default();
    for path in paths {
        let disposition = target
            .copy_entry_from(&source, &path)
            .with_context(|| format!("Failed to copy worktree payload {}", path.display()))?;
        result.entries.push(PayloadCopyEntry { path, disposition });
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn payload_rejects_symlink_escapes_and_reports_special_file_failure() {
        use super::*;
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(source.join("payload")).unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(source.join("payload/secret"), "secret").unwrap();
        std::os::unix::fs::symlink(&outside, target.join("payload")).unwrap();
        let mut config = Config::default();
        config.worktree.copy_files = vec!["payload/secret".into()];
        assert!(copy_worktree_payload(None, &config, &source, &target, None, None).is_err());
        assert!(!outside.join("secret").exists());
        std::fs::write(source.join(".env"), "SECRET=fixture").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.env"), target.join(".env")).unwrap();
        config.worktree.copy_files = vec![".env".into()];
        assert!(copy_worktree_payload(None, &config, &source, &target, None, None).is_err());
        assert!(!outside.join("secret.env").exists());
        let _socket =
            std::os::unix::net::UnixListener::bind(source.join("payload/socket")).unwrap();
        std::fs::remove_file(target.join("payload")).unwrap();
        config.worktree.copy_files = vec!["payload".into()];
        assert!(copy_worktree_payload(None, &config, &source, &target, None, None).is_err());
        assert!(!target.join("payload").exists());
    }
    use super::*;
    use crate::vcs::{GitRepository, VcsProvider};

    #[test]
    fn test_apply_worktree_path_template() {
        assert_eq!(
            apply_worktree_path_template("../{repo}.{workspace}", "myapp", "feature-auth"),
            "../myapp.feature-auth"
        );
        assert_eq!(
            apply_worktree_path_template("../{repo}.{branch}", "myapp", "feature-auth"),
            "../myapp.feature-auth"
        );
    }

    #[test]
    fn resolved_paths_do_not_collide_for_lossy_legacy_names() {
        let project = tempfile::tempdir().unwrap();
        let config = Config::default();

        let slash = resolve_worktree_path(&config, project.path(), "feature/auth");
        let dash = resolve_worktree_path(&config, project.path(), "feature-auth");

        assert_ne!(slash, dash);
        assert!(slash
            .to_string_lossy()
            .contains(&config.get_service_workspace_key("feature/auth")));
    }

    #[test]
    fn selection_path_prefers_materialized_worktrees_and_primary_checkout() {
        let container = tempfile::tempdir().unwrap();
        let primary = container.path().join("project");
        std::fs::create_dir_all(&primary).unwrap();
        let repo = GitRepository::init(&primary).unwrap();
        let actual_feature = container.path().join("manually-placed-feature");
        repo.create_worktree("feature/existing", &actual_feature)
            .unwrap();
        let config = Config {
            name: Some("different-template-name".to_string()),
            ..Default::default()
        };

        assert_eq!(
            resolve_existing_or_planned_worktree_path(&config, &primary, "feature/existing"),
            actual_feature.canonicalize().unwrap()
        );
        assert_eq!(
            resolve_existing_or_planned_worktree_path(&config, &primary, "main"),
            primary.canonicalize().unwrap()
        );
        assert_eq!(
            resolve_existing_or_planned_worktree_path(&config, &primary, "feature/missing"),
            resolve_worktree_path(&config, &primary, "feature/missing")
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

        std::fs::create_dir_all(&dst).unwrap();
        crate::files::WorkspaceFiles::open(&dst)
            .unwrap()
            .copy_entry_from(
                &crate::files::WorkspaceFiles::open(&src).unwrap(),
                Path::new("bin"),
            )
            .unwrap();

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
