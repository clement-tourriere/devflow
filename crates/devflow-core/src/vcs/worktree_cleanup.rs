//! Durable ownership of a workspace directory while its removal is retried.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
pub(super) struct Cleanup {
    pub internal: String,
    pub original: PathBuf,
    pub saved_commit: String,
    pub bookmark: Option<String>,
    staged: PathBuf,
    #[serde(default)]
    staged_once: bool,
    identity: String,
    #[serde(default)]
    registration: Option<(PathBuf, String)>,
}

fn identity(path: &Path) -> Result<String> {
    let meta = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "Workspace cleanup path is no longer a directory"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(format!("{}:{}", meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    {
        Ok(format!("{:?}", meta.created()?))
    }
}

impl Cleanup {
    pub fn receipt(repo: &Path, path: &Path) -> PathBuf {
        let normalized = path
            .parent()
            .and_then(|parent| parent.canonicalize().ok())
            .and_then(|parent| path.file_name().map(|name| parent.join(name)))
            .unwrap_or_else(|| path.to_path_buf());
        repo.join("devflow-removals").join(format!(
            "{}.json",
            crate::services::ownership::identity_digest(&[&normalized.to_string_lossy()])
        ))
    }
    pub fn list(repo: &Path) -> Result<Vec<Self>> {
        let dir = repo.join("devflow-removals");
        if !dir.try_exists()? {
            return Ok(Vec::new());
        }
        std::fs::read_dir(dir)?
            .map(|entry| -> Result<Option<Self>> {
                let path = entry?.path();
                if path.extension().is_none_or(|ext| ext != "json") {
                    return Ok(None);
                }
                Self::read(&path)
            })
            .filter_map(|entry| entry.transpose())
            .collect()
    }
    pub fn read(receipt: &Path) -> Result<Option<Self>> {
        match std::fs::read(receipt) {
            Ok(data) => Ok(Some(serde_json::from_slice(&data)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    pub fn begin(
        receipt: &Path,
        internal: String,
        path: &Path,
        saved_commit: String,
        bookmark: Option<String>,
        registration: Option<&Path>,
    ) -> Result<Self> {
        let record = Self {
            internal,
            original: path.to_path_buf(),
            saved_commit,
            bookmark,
            staged: path
                .parent()
                .context("workspace directory has no parent")?
                .join(format!(".devflow-remove-{}", uuid::Uuid::new_v4())),
            staged_once: false,
            identity: identity(path)?,
            registration: registration
                .map(|path| Ok::<_, anyhow::Error>((path.to_path_buf(), identity(path)?)))
                .transpose()?,
        };
        crate::services::ownership::atomic_write(receipt, &serde_json::to_vec(&record)?)?;
        Ok(record)
    }
    pub fn verify_registration(&self) -> Result<()> {
        if let Some((path, expected)) = &self.registration {
            if path.try_exists()? {
                anyhow::ensure!(
                    identity(path)? == *expected,
                    "Worktree registration was replaced; refusing to remove another generation"
                );
            }
        }
        Ok(())
    }
    pub fn remove_directory(&mut self, receipt: &Path) -> Result<()> {
        if !self.staged_once && !self.staged.try_exists()? && self.original.try_exists()? {
            anyhow::ensure!(
                identity(&self.original)? == self.identity,
                "Workspace cleanup path was replaced; refusing to delete its new owner"
            );
            std::fs::rename(&self.original, &self.staged).context(
                "Cannot stage workspace directory for removal; cleanup receipt retained",
            )?;
        }
        if self.staged.try_exists()? {
            anyhow::ensure!(
                identity(&self.staged)? == self.identity,
                "Workspace cleanup directory ownership changed"
            );
            if !self.staged_once {
                self.staged_once = true;
                crate::services::ownership::atomic_write(receipt, &serde_json::to_vec(self)?)?;
            }
            std::fs::remove_dir_all(&self.staged)
                .context("Cannot finish workspace directory cleanup; retry removal")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn partial_traversal_retries_after_metadata_loss_and_preserves_replacement_path() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("child");
        std::fs::create_dir_all(original.join(".jj")).unwrap();
        std::fs::create_dir(original.join("blocked")).unwrap();
        std::fs::write(original.join("blocked/data"), "owned").unwrap();
        let receipt = root.path().join("receipt.json");
        let mut cleanup = Cleanup::begin(
            &receipt,
            "feature".into(),
            &original,
            "saved-commit".into(),
            Some("feature".into()),
            None,
        )
        .unwrap();
        std::fs::rename(&original, &cleanup.staged).unwrap();
        std::fs::remove_dir(cleanup.staged.join(".jj")).unwrap();
        std::fs::set_permissions(
            cleanup.staged.join("blocked"),
            std::fs::Permissions::from_mode(0o500),
        )
        .unwrap();
        let attempt = cleanup.remove_directory(&receipt);
        if attempt.is_err() {
            std::fs::set_permissions(
                cleanup.staged.join("blocked"),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        std::fs::create_dir(&original).unwrap();
        std::fs::write(original.join("replacement"), "leave alone").unwrap();
        if attempt.is_err() {
            Cleanup::read(&receipt)
                .unwrap()
                .unwrap()
                .remove_directory(&receipt)
                .unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(original.join("replacement")).unwrap(),
            "leave alone"
        );
        assert!(!cleanup.staged.exists());
        Cleanup::read(&receipt)
            .unwrap()
            .unwrap()
            .remove_directory(&receipt)
            .unwrap();
        assert!(original.join("replacement").exists());
    }
}
