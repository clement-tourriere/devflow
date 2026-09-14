//! Commit directory copies only after every file has been copied successfully.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub async fn replace_directory<F, Fut>(target: &Path, copy: F) -> Result<()>
where
    F: FnOnce(PathBuf) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let parent = target.parent().context("copy target has no parent")?;
    tokio::fs::create_dir_all(parent).await?;
    let journal = target.with_extension("devflow-copy.json");
    let backup = target.with_extension("devflow-previous");
    if journal.exists() {
        let recorded: PathBuf = serde_json::from_slice(&std::fs::read(&journal)?)?;
        anyhow::ensure!(recorded == target, "copy recovery target does not match");
        if backup.exists() && !target.exists() {
            std::fs::rename(&backup, target)?;
        }
        if backup.exists() {
            std::fs::remove_dir_all(&backup)?;
        }
        std::fs::remove_file(&journal)?;
    }
    anyhow::ensure!(
        !backup.exists(),
        "unclaimed copy backup exists: {}",
        backup.display()
    );
    let staged = parent.join(format!(".devflow-copy-{}", uuid::Uuid::new_v4()));
    let result = copy(staged.clone()).await;
    if let Err(error) = result {
        let _ = tokio::fs::remove_dir_all(&staged).await;
        return Err(error);
    }
    super::ownership::atomic_write(&journal, &serde_json::to_vec(target)?)?;
    if target.exists() {
        std::fs::rename(target, &backup)?;
    }
    if let Err(error) = std::fs::rename(&staged, target) {
        if backup.exists() {
            std::fs::rename(&backup, target)?;
        }
        return Err(error.into());
    }
    if backup.exists() {
        std::fs::remove_dir_all(&backup)?;
    }
    std::fs::remove_file(&journal)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn failed_copy_preserves_existing_data_and_retry_replaces_it() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("pgdata");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("marker"), "original").unwrap();
        let error = replace_directory(&target, |stage| async move {
            std::fs::create_dir(&stage)?;
            std::fs::write(stage.join("marker"), "partial")?;
            anyhow::bail!("injected copy failure")
        })
        .await;
        assert!(error.is_err());
        assert_eq!(
            std::fs::read_to_string(target.join("marker")).unwrap(),
            "original"
        );
        replace_directory(&target, |stage| async move {
            std::fs::create_dir(&stage)?;
            std::fs::write(stage.join("marker"), "complete")?;
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("marker")).unwrap(),
            "complete"
        );
    }
}
