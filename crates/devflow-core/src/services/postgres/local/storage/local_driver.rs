use std::path::Path;

use anyhow::{anyhow, Context};

use super::zfs_driver::DriverDetection;

#[derive(Debug, Clone, Copy)]
pub enum LocalMode {
    ApfsClone,
    Reflink,
    Copy,
}

#[derive(Debug, Default, Clone)]
pub struct LocalDriver;

impl LocalDriver {
    pub fn new() -> Self {
        Self
    }

    pub async fn detect_apfs(&self, projects_root: &Path) -> DriverDetection {
        if !cfg!(target_os = "macos") {
            return DriverDetection {
                available: false,
                detail: "APFS clone driver only applies on macOS".to_string(),
                root_dataset: None,
            };
        }

        let result = crate::fs_copy::probe_cow(projects_root, projects_root);

        match result {
            Ok(()) => DriverDetection {
                available: true,
                detail: "APFS clone probe succeeded".to_string(),
                root_dataset: None,
            },
            Err(err) => DriverDetection {
                available: false,
                detail: format!("APFS clone probe failed: {err}"),
                root_dataset: None,
            },
        }
    }

    pub async fn detect_reflink(&self, projects_root: &Path) -> DriverDetection {
        if !cfg!(target_os = "linux") {
            return DriverDetection {
                available: false,
                detail: "reflink driver probe only runs on Linux".to_string(),
                root_dataset: None,
            };
        }

        let result = crate::fs_copy::probe_cow(projects_root, projects_root);

        match result {
            Ok(()) => DriverDetection {
                available: true,
                detail: "reflink clone probe succeeded".to_string(),
                root_dataset: None,
            },
            Err(err) => DriverDetection {
                available: false,
                detail: format!("reflink clone probe failed: {err}"),
                root_dataset: None,
            },
        }
    }

    pub async fn prepare_empty(&self, data_dir: &Path, _mode: LocalMode) -> anyhow::Result<()> {
        recreate_dir(data_dir).await
    }

    pub async fn clone_dir(
        &self,
        source: &Path,
        target: &Path,
        mode: LocalMode,
    ) -> anyhow::Result<()> {
        crate::services::copy::replace_directory(target, |staged| async move {
            self.clone_dir_raw(source, &staged, mode).await
        })
        .await
    }

    async fn clone_dir_raw(
        &self,
        source: &Path,
        target: &Path,
        mode: LocalMode,
    ) -> anyhow::Result<()> {
        let source = source.to_path_buf();
        let target = target.to_path_buf();
        let mode = match mode {
            LocalMode::ApfsClone | LocalMode::Reflink => crate::fs_copy::CopyMode::Cow,
            LocalMode::Copy => crate::fs_copy::CopyMode::Copy,
        };
        // Only the new staging directory is populated. Recreating its parent
        // would delete the live data before the copy can be committed.
        tokio::task::spawn_blocking(move || crate::fs_copy::copy_directory(&source, &target, mode))
            .await?
    }

    pub async fn remove_dir(&self, data_dir: &Path) -> anyhow::Result<()> {
        let workspace_root = branch_root_from_data_dir(data_dir)?;
        if tokio::fs::metadata(workspace_root).await.is_ok() {
            tokio::fs::remove_dir_all(workspace_root)
                .await
                .with_context(|| {
                    format!("failed to delete directory '{}'", workspace_root.display())
                })?;
        }
        Ok(())
    }
}

fn branch_root_from_data_dir(data_dir: &Path) -> anyhow::Result<&Path> {
    data_dir
        .parent()
        .ok_or_else(|| anyhow!("invalid data dir '{}'", data_dir.display()))
}

async fn recreate_dir(path: &Path) -> anyhow::Result<()> {
    let workspace_root = branch_root_from_data_dir(path)?;
    if tokio::fs::metadata(workspace_root).await.is_ok() {
        tokio::fs::remove_dir_all(workspace_root)
            .await
            .with_context(|| {
                format!("failed to delete directory '{}'", workspace_root.display())
            })?;
    }

    tokio::fs::create_dir_all(path)
        .await
        .with_context(|| format!("failed to create directory '{}'", path.display()))?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failed_staged_clone_preserves_live_data_and_workspace_siblings() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("workspace/pgdata");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("marker"), "live data").unwrap();
        std::fs::write(target.parent().unwrap().join("receipt"), "keep").unwrap();
        std::fs::write(source.join("marker"), "replacement").unwrap();
        // An unsupported socket forces an actual filesystem copy error after
        // staging begins, even when this test runs with elevated privileges.
        let socket_path = source.join("socket");
        let socket = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        let driver = LocalDriver::new();
        assert!(driver
            .clone_dir(&source, &target, LocalMode::Copy)
            .await
            .is_err());
        assert_eq!(
            std::fs::read_to_string(target.join("marker")).unwrap(),
            "live data"
        );
        assert_eq!(
            std::fs::read_to_string(target.parent().unwrap().join("receipt")).unwrap(),
            "keep"
        );
        drop(socket);
        std::fs::remove_file(socket_path).unwrap();
        driver
            .clone_dir(&source, &target, LocalMode::Copy)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("marker")).unwrap(),
            "replacement"
        );
        assert_eq!(
            std::fs::read_to_string(target.parent().unwrap().join("receipt")).unwrap(),
            "keep"
        );
    }
}
