pub mod local_driver;
pub mod zfs_driver;
pub mod zfs_setup;

use std::path::Path;

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};

use super::model::{Project, StorageDriver, Workspace};

#[derive(Debug, Clone)]
pub struct StorageSelection {
    pub driver: StorageDriver,
    pub config: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageDoctorEntry {
    pub kind: String,
    pub available: bool,
    pub detail: String,
    pub selected: bool,
}

#[derive(Debug, Clone)]
pub struct StorageDoctorReport {
    pub entries: Vec<StorageDoctorEntry>,
    pub default_driver: StorageDriver,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoragePreferenceCheck {
    pub preference: Option<String>,
    pub available: bool,
    pub selected_driver: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZfsProjectConfig {
    pub root_dataset: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZfsBranchMetadata {
    pub dataset: String,
    pub origin_snapshot: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StorageCoordinator {
    projects_root: std::path::PathBuf,
    local: local_driver::LocalDriver,
    zfs: zfs_driver::ZfsDriver,
}

impl StorageCoordinator {
    pub fn new(projects_root: std::path::PathBuf) -> Self {
        Self {
            local: local_driver::LocalDriver::new(),
            zfs: zfs_driver::ZfsDriver::new(),
            projects_root,
        }
    }

    pub async fn doctor(&self) -> StorageDoctorReport {
        let zfs_report = self.zfs.detect(&self.projects_root).await;
        let apfs_report = self.local.detect_apfs(&self.projects_root).await;
        let reflink_report = self.local.detect_reflink(&self.projects_root).await;

        let default_driver = if zfs_report.available {
            StorageDriver::Zfs
        } else if apfs_report.available {
            StorageDriver::ApfsClone
        } else if reflink_report.available {
            StorageDriver::Reflink
        } else {
            StorageDriver::Copy
        };

        let entries = vec![
            StorageDoctorEntry {
                kind: StorageDriver::Zfs.as_str().to_string(),
                available: zfs_report.available,
                detail: zfs_report.detail,
                selected: default_driver == StorageDriver::Zfs,
            },
            StorageDoctorEntry {
                kind: StorageDriver::ApfsClone.as_str().to_string(),
                available: apfs_report.available,
                detail: apfs_report.detail,
                selected: default_driver == StorageDriver::ApfsClone,
            },
            StorageDoctorEntry {
                kind: StorageDriver::Reflink.as_str().to_string(),
                available: reflink_report.available,
                detail: reflink_report.detail,
                selected: default_driver == StorageDriver::Reflink,
            },
            StorageDoctorEntry {
                kind: StorageDriver::Copy.as_str().to_string(),
                available: true,
                detail: "portable full copy fallback".to_string(),
                selected: default_driver == StorageDriver::Copy,
            },
        ];

        StorageDoctorReport {
            entries,
            default_driver,
        }
    }

    pub async fn select_for_new_project(&self) -> StorageSelection {
        // Probe in preference order and short-circuit: unlike doctor(), which
        // reports on every driver, project creation only needs the first
        // available one, and each probe costs a process spawn.
        let zfs_report = self.zfs.detect(&self.projects_root).await;
        if zfs_report.available {
            if let Some(root_dataset) = zfs_report.root_dataset {
                let config = ZfsProjectConfig { root_dataset };
                return StorageSelection {
                    driver: StorageDriver::Zfs,
                    config: Some(
                        serde_json::to_string(&config).unwrap_or_else(|_| "{}".to_string()),
                    ),
                };
            }
        }

        if self.local.detect_apfs(&self.projects_root).await.available {
            return StorageSelection {
                driver: StorageDriver::ApfsClone,
                config: None,
            };
        }

        if self
            .local
            .detect_reflink(&self.projects_root)
            .await
            .available
        {
            return StorageSelection {
                driver: StorageDriver::Reflink,
                config: None,
            };
        }

        log::warn!(
            "No copy-on-write storage support detected at '{}' (ZFS, APFS clone, or reflink); \
             workspace databases will be cloned with full copies, which is slow for large data. \
             On Linux, Btrfs or XFS enables instant reflink clones.",
            self.projects_root.display()
        );

        StorageSelection {
            driver: StorageDriver::Copy,
            config: None,
        }
    }

    /// Return a diagnostic for an optional storage preference.
    pub async fn check_preference(&self, preferred: Option<&str>) -> StoragePreferenceCheck {
        let preference = preferred
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty());
        match self.select_for_new_project_with(preferred).await {
            Ok(selection) => StoragePreferenceCheck {
                preference,
                available: true,
                selected_driver: Some(selection.driver.as_str().to_string()),
                detail: format!(
                    "storage preference resolves to '{}'",
                    selection.driver.as_str()
                ),
            },
            Err(err) => StoragePreferenceCheck {
                preference,
                available: false,
                selected_driver: None,
                detail: err.to_string(),
            },
        }
    }

    /// Select a storage driver for a new project.
    ///
    /// `preferred` accepts `auto`, `zfs`, `apfs_clone`/`apfs`, `reflink`, or `copy`.
    /// Explicit CoW drivers fail with a clear error if the required filesystem/tooling
    /// is unavailable; `copy` always works as a portable fallback.
    pub async fn select_for_new_project_with(
        &self,
        preferred: Option<&str>,
    ) -> anyhow::Result<StorageSelection> {
        let preferred = preferred
            .map(|s| s.trim().to_ascii_lowercase().replace('-', "_"))
            .filter(|s| !s.is_empty() && s != "auto");

        if let Some(preferred) = preferred.as_deref() {
            let driver = match preferred {
                "apfs" => StorageDriver::ApfsClone,
                other => StorageDriver::from_str(other).ok_or_else(|| {
                    anyhow!(
                        "invalid local.storage value '{}'; expected auto, zfs, apfs_clone, reflink, or copy",
                        other
                    )
                })?,
            };

            if driver == StorageDriver::Copy {
                return Ok(StorageSelection {
                    driver,
                    config: None,
                });
            }

            return self.select_explicit_driver(driver).await;
        }

        Ok(self.select_for_new_project().await)
    }

    async fn select_explicit_driver(
        &self,
        driver: StorageDriver,
    ) -> anyhow::Result<StorageSelection> {
        let report = self.doctor().await;
        let entry = report
            .entries
            .iter()
            .find(|e| e.kind == driver.as_str())
            .ok_or_else(|| anyhow!("storage driver '{}' is not known", driver.as_str()))?;

        if !entry.available {
            anyhow::bail!(
                "local.storage is set to '{}' but it is unavailable: {}. Use 'auto' or 'copy' to fall back.",
                driver.as_str(),
                entry.detail
            );
        }

        self.selection_for_driver(driver).await
    }

    async fn selection_for_driver(
        &self,
        driver: StorageDriver,
    ) -> anyhow::Result<StorageSelection> {
        match driver {
            StorageDriver::Zfs => {
                let zfs_report = self.zfs.detect(&self.projects_root).await;
                if let Some(root_dataset) = zfs_report.root_dataset {
                    let config = ZfsProjectConfig { root_dataset };
                    Ok(StorageSelection {
                        driver: StorageDriver::Zfs,
                        config: Some(
                            serde_json::to_string(&config).unwrap_or_else(|_| "{}".to_string()),
                        ),
                    })
                } else {
                    anyhow::bail!(
                        "ZFS storage was selected but no devflow ZFS dataset was found: {}",
                        zfs_report.detail
                    )
                }
            }
            other => Ok(StorageSelection {
                driver: other,
                config: None,
            }),
        }
    }

    pub async fn create_empty_branch(
        &self,
        project: &Project,
        branch_id: &str,
        data_dir: &Path,
    ) -> anyhow::Result<Option<String>> {
        match project.storage_driver {
            StorageDriver::Zfs => {
                let config = parse_zfs_config(project)?;
                self.zfs
                    .create_empty(project, &config, branch_id, data_dir)
                    .await
            }
            StorageDriver::ApfsClone => {
                self.local
                    .prepare_empty(data_dir, local_driver::LocalMode::ApfsClone)
                    .await?;
                Ok(None)
            }
            StorageDriver::Reflink => {
                self.local
                    .prepare_empty(data_dir, local_driver::LocalMode::Reflink)
                    .await?;
                Ok(None)
            }
            StorageDriver::Copy => {
                self.local
                    .prepare_empty(data_dir, local_driver::LocalMode::Copy)
                    .await?;
                Ok(None)
            }
        }
    }

    pub async fn clone_branch_from_parent(
        &self,
        project: &Project,
        parent: &Workspace,
        child_branch_id: &str,
        child_data_dir: &Path,
    ) -> anyhow::Result<Option<String>> {
        match project.storage_driver {
            StorageDriver::Zfs => {
                let config = parse_zfs_config(project)?;
                self.zfs
                    .clone_from_parent(project, &config, parent, child_branch_id, child_data_dir)
                    .await
            }
            StorageDriver::ApfsClone => {
                self.local
                    .clone_dir(
                        std::path::PathBuf::from(&parent.data_dir).as_path(),
                        child_data_dir,
                        local_driver::LocalMode::ApfsClone,
                    )
                    .await?;
                Ok(None)
            }
            StorageDriver::Reflink => {
                self.local
                    .clone_dir(
                        std::path::PathBuf::from(&parent.data_dir).as_path(),
                        child_data_dir,
                        local_driver::LocalMode::Reflink,
                    )
                    .await?;
                Ok(None)
            }
            StorageDriver::Copy => {
                log::warn!(
                    "Project '{}' uses full-copy storage (no copy-on-write support was detected \
                     when the project was created); cloning workspace data from '{}' may be slow. \
                     Run 'devflow doctor' to inspect storage drivers.",
                    project.name,
                    parent.name
                );
                self.local
                    .clone_dir(
                        std::path::PathBuf::from(&parent.data_dir).as_path(),
                        child_data_dir,
                        local_driver::LocalMode::Copy,
                    )
                    .await?;
                Ok(None)
            }
        }
    }

    pub async fn delete_workspace_data(
        &self,
        project: &Project,
        workspace: &Workspace,
    ) -> anyhow::Result<()> {
        match project.storage_driver {
            StorageDriver::Zfs => {
                let config = parse_zfs_config(project)?;
                self.zfs.delete_workspace(project, &config, workspace).await
            }
            StorageDriver::ApfsClone | StorageDriver::Reflink | StorageDriver::Copy => {
                self.local
                    .remove_dir(std::path::PathBuf::from(&workspace.data_dir).as_path())
                    .await
            }
        }
    }

    pub async fn delete_project_data(&self, project: &Project) -> anyhow::Result<()> {
        match project.storage_driver {
            StorageDriver::Zfs => {
                let config = parse_zfs_config(project)?;
                let project_dataset = format!("{}/projects/{}", config.root_dataset, project.id);
                // Recursively destroy the entire project dataset and children
                let output = tokio::process::Command::new("zfs")
                    .args(["destroy", "-r", "-f", &project_dataset])
                    .output()
                    .await;
                if let Err(e) = output {
                    log::warn!("failed to destroy ZFS dataset '{}': {}", project_dataset, e);
                } else if let Ok(ref out) = output {
                    if !out.status.success() {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        log::warn!(
                            "zfs destroy failed for '{}': {}",
                            project_dataset,
                            stderr.trim()
                        );
                    }
                }
                // Also remove the local project directory if it exists
                let project_dir = self.projects_root.join(&project.id);
                if tokio::fs::metadata(&project_dir).await.is_ok() {
                    tokio::fs::remove_dir_all(&project_dir)
                        .await
                        .with_context(|| {
                            format!(
                                "failed to remove project directory '{}'",
                                project_dir.display()
                            )
                        })?;
                }
            }
            StorageDriver::ApfsClone | StorageDriver::Reflink | StorageDriver::Copy => {
                let project_dir = self.projects_root.join(&project.id);
                if tokio::fs::metadata(&project_dir).await.is_ok() {
                    tokio::fs::remove_dir_all(&project_dir)
                        .await
                        .with_context(|| {
                            format!(
                                "failed to remove project directory '{}'",
                                project_dir.display()
                            )
                        })?;
                }
            }
        }
        Ok(())
    }
}

fn parse_zfs_config(project: &Project) -> anyhow::Result<ZfsProjectConfig> {
    let raw = project
        .storage_config
        .as_ref()
        .ok_or_else(|| anyhow!("project '{}' missing ZFS storage config", project.id))?;

    serde_json::from_str::<ZfsProjectConfig>(raw).map_err(|err| {
        anyhow!(
            "invalid ZFS storage config for project '{}': {err}",
            project.id
        )
    })
}

#[cfg(test)]
mod tests {
    use super::super::model::WorkspaceState;
    use super::*;

    #[tokio::test]
    async fn explicit_copy_storage_is_always_selectable() {
        let tmp = tempfile::tempdir().unwrap();
        let coordinator = StorageCoordinator::new(tmp.path().to_path_buf());

        let selection = coordinator
            .select_for_new_project_with(Some("copy"))
            .await
            .expect("copy storage should not need platform support");

        assert_eq!(selection.driver, StorageDriver::Copy);
        assert!(selection.config.is_none());
    }

    #[tokio::test]
    async fn invalid_storage_preference_has_clear_error() {
        let tmp = tempfile::tempdir().unwrap();
        let coordinator = StorageCoordinator::new(tmp.path().to_path_buf());

        let err = coordinator
            .select_for_new_project_with(Some("mystery"))
            .await
            .expect_err("invalid storage should fail");

        assert!(err.to_string().contains("invalid local.storage value"));
    }

    #[tokio::test]
    async fn storage_preference_diagnostic_reports_copy_selection() {
        let tmp = tempfile::tempdir().unwrap();
        let coordinator = StorageCoordinator::new(tmp.path().to_path_buf());

        let check = coordinator.check_preference(Some("copy")).await;

        assert!(check.available);
        assert_eq!(check.preference.as_deref(), Some("copy"));
        assert_eq!(check.selected_driver.as_deref(), Some("copy"));
        assert!(check.detail.contains("resolves to 'copy'"));
    }

    #[tokio::test]
    async fn storage_preference_diagnostic_reports_invalid_values() {
        let tmp = tempfile::tempdir().unwrap();
        let coordinator = StorageCoordinator::new(tmp.path().to_path_buf());

        let check = coordinator.check_preference(Some("mystery")).await;

        assert!(!check.available);
        assert_eq!(check.preference.as_deref(), Some("mystery"));
        assert!(check.detail.contains("invalid local.storage value"));
    }

    #[tokio::test]
    async fn copy_storage_clones_postgres_data_independently() {
        let tmp = tempfile::tempdir().unwrap();
        let coordinator = StorageCoordinator::new(tmp.path().join("projects"));

        let parent_pgdata = tmp.path().join("parent").join("pgdata");
        tokio::fs::create_dir_all(parent_pgdata.join("base"))
            .await
            .unwrap();
        tokio::fs::write(parent_pgdata.join("PG_VERSION"), b"17\n")
            .await
            .unwrap();
        tokio::fs::write(parent_pgdata.join("base").join("fixture"), b"parent-data")
            .await
            .unwrap();

        let project = Project {
            id: "project-1".to_string(),
            name: "app-db".to_string(),
            image: "postgres:17".to_string(),
            storage_driver: StorageDriver::Copy,
            storage_config: None,
            project_path: None,
            created_at: 0,
        };
        let parent = Workspace {
            id: "main-id".to_string(),
            project_id: project.id.clone(),
            name: "main".to_string(),
            parent_workspace_id: None,
            state: WorkspaceState::Stopped,
            data_dir: parent_pgdata.display().to_string(),
            container_name: "devflow-test-parent".to_string(),
            port: 55432,
            storage_metadata: None,
            created_at: 0,
        };
        let child_pgdata = tmp
            .path()
            .join("project-1")
            .join("workspaces")
            .join("child-id")
            .join("pgdata");

        let metadata = coordinator
            .clone_branch_from_parent(&project, &parent, "child-id", &child_pgdata)
            .await
            .expect("copy storage should clone a PostgreSQL pgdata directory");

        assert!(metadata.is_none());
        assert_eq!(
            tokio::fs::read_to_string(child_pgdata.join("PG_VERSION"))
                .await
                .unwrap(),
            "17\n"
        );
        assert_eq!(
            tokio::fs::read_to_string(child_pgdata.join("base").join("fixture"))
                .await
                .unwrap(),
            "parent-data"
        );

        tokio::fs::write(child_pgdata.join("base").join("fixture"), b"child-data")
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(parent_pgdata.join("base").join("fixture"))
                .await
                .unwrap(),
            "parent-data"
        );
    }
}
