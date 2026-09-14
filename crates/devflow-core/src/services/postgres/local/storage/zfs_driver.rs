use std::path::{Path, PathBuf};

use super::{
    replacement::{DatasetMutation, DatasetOps},
    zfs_native::NativeZfs,
};
use anyhow::{anyhow, Context};
use uuid::Uuid;

use super::{ZfsBranchMetadata, ZfsProjectConfig};
use crate::services::postgres::local::model::{Project, Workspace};

#[derive(Debug, Clone)]
pub struct DriverDetection {
    pub available: bool,
    pub detail: String,
    pub root_dataset: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct ZfsDriver;

impl ZfsDriver {
    pub fn new() -> Self {
        Self
    }

    pub async fn detect(&self, projects_root: &Path) -> DriverDetection {
        if !cfg!(target_os = "linux") {
            return DriverDetection {
                available: false,
                detail: "ZFS driver is only supported on Linux".to_string(),
                root_dataset: None,
            };
        }

        let mountpoints = match NativeZfs.mountpoints().await {
            Ok(mountpoints) => mountpoints,
            Err(error) => {
                return DriverDetection {
                    available: false,
                    detail: format!("Native ZFS unavailable: {error:#}"),
                    root_dataset: None,
                }
            }
        };

        let dataset = if let Ok(explicit) = std::env::var("DEVFLOW_ZFS_DATASET") {
            if explicit.trim().is_empty() {
                None
            } else {
                Some(explicit)
            }
        } else {
            detect_dataset_from_mountpoints(projects_root, &mountpoints)
        };

        let Some(root_dataset) = dataset else {
            return DriverDetection {
                available: false,
                detail: format!(
                    "no ZFS dataset found for '{}' (set DEVFLOW_ZFS_DATASET to force one)",
                    projects_root.display()
                ),
                root_dataset: None,
            };
        };

        let id = Uuid::new_v4();
        let probe_name = format!("{root_dataset}/devflow_probe_{id}");
        let probe_path = projects_root.join(format!(".devflow-zfs-probe-{id}"));
        let result = async {
            anyhow::ensure!(
                !NativeZfs.exists(&probe_name).await?,
                "probe dataset already exists"
            );
            let created = NativeZfs.create(&probe_name, Some(&probe_path)).await;
            // Creation can succeed before mounting fails. Clean up that exact
            // owned probe and report cleanup failures, including its identity.
            let cleanup = NativeZfs
                .apply(DatasetMutation::Destroy(probe_name.clone()))
                .await;
            if let Err(error) = cleanup {
                return Err(error.context(format!("ZFS probe cleanup failed for {probe_name}")));
            }
            let _ = tokio::fs::remove_dir(&probe_path).await;
            created
        }
        .await;
        DriverDetection {
            available: result.is_ok(),
            detail: match result {
                Ok(()) => format!("Native ZFS available with root dataset '{root_dataset}'"),
                Err(error) => format!(
                    "ZFS dataset '{root_dataset}' cannot create and mount workspaces: {error:#}"
                ),
            },
            root_dataset: Some(root_dataset),
        }
    }

    pub async fn create_empty(
        &self,
        project: &Project,
        config: &ZfsProjectConfig,
        workspace_id: &str,
        data_dir: &Path,
    ) -> anyhow::Result<Option<String>> {
        let workspace_root = branch_root_from_data_dir(data_dir)?;
        let project_dataset = project_dataset_name(config, &project.id);
        let branch_dataset = branch_dataset_name(config, &project.id, workspace_id);

        ensure_dataset_exists(&project_dataset).await?;
        ensure_dataset_exists(&format!("{project_dataset}/workspaces")).await?;
        ensure_dataset_absent(&branch_dataset).await?;

        create_dataset_with_mountpoint(&branch_dataset, workspace_root).await?;
        tokio::fs::create_dir_all(data_dir)
            .await
            .with_context(|| format!("failed to create '{}'", data_dir.display()))?;

        let metadata = ZfsBranchMetadata {
            dataset: branch_dataset,
            origin_snapshot: None,
        };

        Ok(Some(
            serde_json::to_string(&metadata)
                .context("failed to serialize ZFS workspace metadata")?,
        ))
    }

    pub async fn clone_from_parent(
        &self,
        project: &Project,
        config: &ZfsProjectConfig,
        parent: &Workspace,
        child_branch_id: &str,
        child_data_dir: &Path,
    ) -> anyhow::Result<Option<String>> {
        let parent_metadata = parse_zfs_branch_metadata(parent)?;
        let child_branch_root = branch_root_from_data_dir(child_data_dir)?;
        anyhow::ensure!(
            Path::new(&parent.data_dir).is_dir(),
            "parent data directory is missing"
        );
        anyhow::ensure!(
            Path::new(&parent.data_dir).file_name() == child_data_dir.file_name(),
            "parent and child data layouts differ"
        );

        let child_dataset = branch_dataset_name(config, &project.id, child_branch_id);

        let journal = crate::paths::devflow_config_dir()?
            .join("storage-operations")
            .join(format!(
                "{}.json",
                crate::services::ownership::identity_digest(&[&child_dataset])
            ));
        super::replacement::recover(&NativeZfs, &journal, &child_dataset).await?;
        let operation = Uuid::new_v4().to_string();
        let snapshot_full = format!("{}@devflow_{}", parent_metadata.dataset, operation);
        let mut replacement = super::replacement::Replacement {
            target: child_dataset.clone(),
            staged: format!("{child_dataset}-staged-{operation}"),
            backup: format!("{child_dataset}-previous-{operation}"),
            snapshot: snapshot_full.clone(),
            mountpoint: child_branch_root.into(),
            had_original: dataset_exists(&child_dataset).await?,
            committed: false,
        };
        super::replacement::replace(&NativeZfs, &mut replacement, &journal).await?;
        anyhow::ensure!(
            child_data_dir.is_dir(),
            "cloned data directory is missing: {}",
            child_data_dir.display()
        );

        let metadata = ZfsBranchMetadata {
            dataset: child_dataset,
            origin_snapshot: Some(snapshot_full),
        };

        Ok(Some(
            serde_json::to_string(&metadata)
                .context("failed to serialize ZFS workspace metadata")?,
        ))
    }

    pub async fn delete_workspace(
        &self,
        _project: &Project,
        _config: &ZfsProjectConfig,
        workspace: &Workspace,
    ) -> anyhow::Result<()> {
        let metadata = parse_zfs_branch_metadata(workspace)?;

        // Never remove mounted files after an API failure or unknown state.
        NativeZfs
            .apply(DatasetMutation::Destroy(metadata.dataset.clone()))
            .await
            .with_context(|| {
                format!(
                    "failed to destroy ZFS dataset '{}'; workspace files retained",
                    metadata.dataset
                )
            })?;
        if let Some(snapshot) = metadata.origin_snapshot {
            if let Err(error) = NativeZfs
                .apply(DatasetMutation::Destroy(snapshot.clone()))
                .await
            {
                log::warn!("Retained ZFS origin snapshot {snapshot}: {error:#}");
            }
        }

        let workspace_root = branch_root_from_data_dir(Path::new(&workspace.data_dir))?;
        if tokio::fs::metadata(workspace_root).await.is_ok() {
            tokio::fs::remove_dir_all(workspace_root)
                .await
                .with_context(|| format!("failed to remove '{}'", workspace_root.display()))?;
        }

        Ok(())
    }
}

fn detect_dataset_from_mountpoints(
    projects_root: &Path,
    mountpoints: &[(String, PathBuf)],
) -> Option<String> {
    let root = projects_root
        .canonicalize()
        .unwrap_or_else(|_| projects_root.to_path_buf());
    mountpoints
        .iter()
        .filter(|(_, path)| root.starts_with(path))
        .max_by_key(|(_, path)| path.components().count())
        .map(|(name, _)| name.clone())
}

fn parse_zfs_branch_metadata(workspace: &Workspace) -> anyhow::Result<ZfsBranchMetadata> {
    let raw = workspace.storage_metadata.as_ref().ok_or_else(|| {
        anyhow!(
            "workspace '{}' is missing ZFS storage metadata",
            workspace.id
        )
    })?;

    serde_json::from_str(raw).with_context(|| {
        format!(
            "workspace '{}' has invalid ZFS storage metadata: {}",
            workspace.id, raw
        )
    })
}

fn branch_root_from_data_dir(data_dir: &Path) -> anyhow::Result<&Path> {
    data_dir.parent().ok_or_else(|| {
        anyhow!(
            "invalid workspace data dir '{}': no parent",
            data_dir.display()
        )
    })
}

fn project_dataset_name(config: &ZfsProjectConfig, project_id: &str) -> String {
    format!("{}/projects/{}", config.root_dataset, project_id)
}

fn branch_dataset_name(config: &ZfsProjectConfig, project_id: &str, workspace_id: &str) -> String {
    format!(
        "{}/projects/{}/workspaces/{}",
        config.root_dataset, project_id, workspace_id
    )
}

async fn ensure_dataset_exists(dataset: &str) -> anyhow::Result<()> {
    let mut parts = dataset.split('/');
    let mut parent = parts.next().context("empty ZFS dataset")?.to_owned();
    anyhow::ensure!(
        NativeZfs.exists(&parent).await?,
        "ZFS pool '{parent}' is missing"
    );
    for part in parts {
        parent.push('/');
        parent.push_str(part);
        if !NativeZfs.exists(&parent).await? {
            NativeZfs.create(&parent, None).await?;
        }
    }
    Ok(())
}

async fn create_dataset_with_mountpoint(dataset: &str, mountpoint: &Path) -> anyhow::Result<()> {
    NativeZfs.create(dataset, Some(mountpoint)).await
}

async fn ensure_dataset_absent(dataset: &str) -> anyhow::Result<()> {
    NativeZfs
        .apply(DatasetMutation::Destroy(dataset.to_owned()))
        .await
}

async fn dataset_exists(dataset: &str) -> anyhow::Result<bool> {
    NativeZfs.exists(dataset).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selects_deepest_mountpoint_by_path_components() {
        let mounts = vec![
            ("pool".into(), PathBuf::from("/data")),
            ("pool/projects".into(), PathBuf::from("/data/projects")),
        ];
        assert_eq!(
            detect_dataset_from_mountpoints(Path::new("/data/projects/new"), &mounts).as_deref(),
            Some("pool/projects")
        );
        assert_eq!(
            detect_dataset_from_mountpoints(Path::new("/data-other"), &mounts),
            None
        );
    }
}
