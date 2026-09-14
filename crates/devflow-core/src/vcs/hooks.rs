//! Shared native Git hook discovery and ownership checks.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub(super) fn directory(repo: &git2::Repository) -> Result<PathBuf> {
    match repo.config()?.get_path("core.hooksPath") {
        Ok(path) if path.is_absolute() => Ok(path),
        Ok(path) => Ok(repo.workdir().unwrap_or(repo.path()).join(path)),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(repo.commondir().join("hooks")),
        Err(e) => Err(e.into()),
    }
}

pub(super) fn is_owned(path: &Path) -> Result<bool> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Ok(false);
    }
    let content = std::fs::read_to_string(path)?;
    let legacy = crate::services::ownership::identity_digest(&[&content])
        == "34ee235d66878b934d0548660cb32dd73229d0c2ab1f7a9b65789089f9c236cc";
    Ok(legacy
        || content == super::git::worktree_only_post_checkout_script()
        || content == super::git::pre_commit_script())
}

pub(super) fn install(repo: &git2::Repository, hooks: &[(&str, String)]) -> Result<()> {
    let dir = directory(repo)?;
    std::fs::create_dir_all(&dir)?;
    let _lease =
        crate::services::ownership::OperationLease::try_acquire(&dir.join("devflow-install.lock"))?
            .context("Hook installation is already active")?;
    // Preflight the whole set before replacing any hook.
    for (name, _) in hooks {
        let path = dir.join(name);
        if std::fs::symlink_metadata(&path).is_ok() {
            anyhow::ensure!(is_owned(&path)?, "Preserving existing hook {}; integrate devflow there manually or move it before installation", path.display());
        }
    }
    let files = crate::files::WorkspaceFiles::open(&dir)?;
    for (name, content) in hooks {
        files.write(name, content.as_bytes(), crate::files::WritePolicy::Replace)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(())
}

pub(super) fn uninstall(repo: &git2::Repository, names: &[&str]) -> Result<()> {
    let dir = directory(repo)?;
    if !dir.try_exists()? {
        return Ok(());
    }
    let _lease =
        crate::services::ownership::OperationLease::try_acquire(&dir.join("devflow-install.lock"))?
            .context("Hook installation is already active")?;
    for name in names {
        let path = dir.join(name);
        if is_owned(&path)? {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::vcs::{GitRepository, VcsProvider};
    #[test]
    fn custom_hooks_path_preserves_user_hooks_and_updates_only_exact_owned_scripts() {
        let root = tempfile::tempdir().unwrap();
        let provider = GitRepository::init(root.path()).unwrap();
        let git = git2::Repository::open(root.path()).unwrap();
        git.config()
            .unwrap()
            .set_str("core.hooksPath", ".githooks")
            .unwrap();
        let dir = root.path().join(".githooks");
        std::fs::create_dir(&dir).unwrap();
        let custom = "#!/bin/sh\n# mentions devflow, but belongs to the project\nexit 1\n";
        std::fs::write(dir.join("pre-commit"), custom).unwrap();
        assert!(provider.install_hooks().is_err());
        assert!(!dir.join("post-checkout").exists());
        provider.uninstall_hooks().unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("pre-commit")).unwrap(),
            custom
        );
        std::fs::remove_file(dir.join("pre-commit")).unwrap();
        provider.install_hooks().unwrap();
        provider.install_hooks().unwrap();
        let hook = std::fs::read_to_string(dir.join("post-checkout")).unwrap();
        assert!(!hook.contains("git rev-parse"));
        let paths = provider.hook_paths();
        assert_eq!(paths.len(), 2);
        assert!(
            paths
                .iter()
                .all(|path| path.parent().unwrap().canonicalize().unwrap()
                    == dir.canonicalize().unwrap()),
            "{paths:?} vs {dir:?}"
        );
        provider.uninstall_hooks().unwrap();
        assert!(!dir.join("post-checkout").exists());
    }
}
