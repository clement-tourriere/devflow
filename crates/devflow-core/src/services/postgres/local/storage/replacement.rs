//! Recoverable ZFS cutover. The previous dataset survives until publication.
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[async_trait]
pub(super) trait DatasetCommands: Sync {
    async fn run(&self, args: &[String]) -> Result<String>;
    async fn exists(&self, dataset: &str) -> Result<bool> {
        let names = self
            .run(&[
                "list".into(),
                "-H".into(),
                "-o".into(),
                "name".into(),
                "-t".into(),
                "all".into(),
            ])
            .await?;
        Ok(names.lines().any(|name| name == dataset))
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Replacement {
    pub target: String,
    pub staged: String,
    pub backup: String,
    pub snapshot: String,
    pub mountpoint: PathBuf,
    pub had_original: bool,
    pub committed: bool,
}

impl Replacement {
    fn save(&self, path: &Path) -> Result<()> {
        crate::services::ownership::atomic_write(path, &serde_json::to_vec_pretty(self)?)
    }
}

async fn run(ops: &dyn DatasetCommands, args: &[&str]) -> Result<()> {
    ops.run(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        .await?;
    Ok(())
}

async fn mount(ops: &dyn DatasetCommands, dataset: &str, path: &Path) -> Result<()> {
    let property = format!("mountpoint={}", path.display());
    run(ops, &["set", &property, dataset]).await?;
    let mounted = ops
        .run(&[
            "get".into(),
            "-H".into(),
            "-o".into(),
            "value".into(),
            "mounted".into(),
            dataset.into(),
        ])
        .await?;
    if mounted.trim() != "yes" {
        run(ops, &["mount", dataset]).await?;
    }
    Ok(())
}

/// Recover only names recorded by this operation; never infer ownership from a
/// prefix or recursively delete another workspace's dataset.
pub(super) async fn recover(ops: &dyn DatasetCommands, path: &Path, target: &str) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let record: Replacement = serde_json::from_slice(&std::fs::read(path)?)?;
    anyhow::ensure!(
        record.target == target,
        "dataset replacement identity mismatch"
    );
    if record.committed {
        if ops.exists(&record.backup).await? {
            // Dependent snapshots may prevent reclamation. Keep their explicit
            // recovery record for later GC; they do not block the new dataset.
            if let Err(error) = run(ops, &["destroy", "-r", &record.backup]).await {
                let retired = path.with_file_name(format!(
                    "{}.retired.json",
                    crate::services::ownership::identity_digest(&[&record.backup])
                ));
                record.save(&retired)?;
                log::warn!(
                    "Retained previous ZFS generation {}: {error:#}; record {}",
                    record.backup,
                    retired.display()
                );
            }
        }
    } else {
        let backup_exists = ops.exists(&record.backup).await?;
        if backup_exists || !record.had_original {
            if ops.exists(&record.target).await? {
                let origin = ops
                    .run(&[
                        "get".into(),
                        "-H".into(),
                        "-o".into(),
                        "value".into(),
                        "origin".into(),
                        record.target.clone(),
                    ])
                    .await?;
                anyhow::ensure!(
                    origin.trim() == record.snapshot,
                    "refusing to remove a dataset with an unexpected origin"
                );
                run(ops, &["destroy", "-r", &record.target]).await?;
            }
            if backup_exists {
                run(ops, &["rename", &record.backup, &record.target]).await?;
            }
        }
        if record.had_original {
            anyhow::ensure!(
                ops.exists(&record.target).await?,
                "previous dataset is missing; recovery intent retained"
            );
            mount(ops, &record.target, &record.mountpoint).await?;
        }
        if ops.exists(&record.staged).await? {
            run(ops, &["destroy", "-r", &record.staged]).await?;
        }
        // Snapshot creation can itself have failed, so query before deleting.
        let snapshots = ops
            .run(&[
                "list".into(),
                "-H".into(),
                "-t".into(),
                "snapshot".into(),
                "-o".into(),
                "name".into(),
            ])
            .await?;
        if snapshots.lines().any(|s| s == record.snapshot) {
            run(ops, &["destroy", &record.snapshot]).await?;
        }
    }
    std::fs::remove_file(path)?;
    Ok(())
}

pub(super) async fn replace(
    ops: &dyn DatasetCommands,
    record: &mut Replacement,
    journal: &Path,
) -> Result<()> {
    record.save(journal)?;
    let result: Result<()> = async {
        run(ops, &["snapshot", &record.snapshot]).await?;
        // The clone is prepared without touching or mounting over the target.
        run(
            ops,
            &[
                "clone",
                "-o",
                "mountpoint=none",
                &record.snapshot,
                &record.staged,
            ],
        )
        .await?;
        anyhow::ensure!(
            ops.exists(&record.staged).await?,
            "staged dataset was not created"
        );
        let origin = ops
            .run(&[
                "get".into(),
                "-H".into(),
                "-o".into(),
                "value".into(),
                "origin".into(),
                record.staged.clone(),
            ])
            .await?;
        anyhow::ensure!(
            origin.trim() == record.snapshot,
            "staged dataset has an unexpected origin"
        );
        if record.had_original {
            run(ops, &["set", "mountpoint=none", &record.target]).await?;
            run(ops, &["rename", &record.target, &record.backup]).await?;
        }
        run(ops, &["rename", &record.staged, &record.target]).await?;
        mount(ops, &record.target, &record.mountpoint).await?;
        record.committed = true;
        record.save(journal)?;
        Ok(())
    }
    .await;
    if let Err(error) = result {
        // A failed checkpoint is still uncommitted on disk; recovery uses the
        // persisted record rather than the in-memory committed flag.
        recover(ops, journal, &record.target)
            .await
            .with_context(|| {
                format!(
                    "ZFS replacement failed ({error:#}); recovery remains at {}",
                    journal.display()
                )
            })?;
        return Err(error);
    }
    recover(ops, journal, &record.target).await
}

/// Read-only inspection of replacement intents owned by this data root.
pub(super) fn diagnostics(data_root: &Path) -> Result<Vec<crate::services::DoctorCheck>> {
    let root = crate::paths::devflow_config_dir()?.join("storage-operations");
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut checks = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let record: Replacement = serde_json::from_slice(&std::fs::read(&path)?)?;
        if !record.mountpoint.starts_with(data_root) {
            continue;
        }
        let retired = path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(".retired.json"));
        checks.push(crate::services::DoctorCheck {
            name: "ZFS replacement recovery".into(),
            available: false,
            detail: if retired {
                format!("Previous generation {} retained because dependent snapshots prevented reclamation; inspect dependencies and recovery record {}", record.backup, path.display())
            } else {
                format!("Interrupted replacement of {}; retry the original operation to recover from {}", record.target, path.display())
            },
        });
    }
    Ok(checks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    struct Model {
        data: Mutex<BTreeMap<String, String>>,
        mutation: Mutex<usize>,
        fail_at: usize,
    }
    #[async_trait]
    impl DatasetCommands for Model {
        async fn run(&self, args: &[String]) -> Result<String> {
            let mut data = self.data.lock().unwrap();
            if args[0] == "list" {
                return Ok(data.keys().cloned().collect::<Vec<_>>().join("\n"));
            }
            if args[0] == "get" {
                return Ok(if args[4] == "mounted" {
                    "yes".into()
                } else {
                    data.get(args.last().unwrap()).cloned().unwrap()
                });
            }
            let mut count = self.mutation.lock().unwrap();
            *count += 1;
            anyhow::ensure!(*count != self.fail_at, "injected mutation failure");
            match args[0].as_str() {
                "snapshot" => {
                    data.insert(args[1].clone(), "snapshot".into());
                }
                "clone" => {
                    data.insert(args[4].clone(), args[3].clone());
                }
                "rename" => {
                    let value = data.remove(&args[1]).context("source missing")?;
                    data.insert(args[2].clone(), value);
                }
                "destroy" => {
                    data.remove(args.last().unwrap());
                }
                "set" | "mount" => {}
                _ => anyhow::bail!("unexpected command: {args:?}"),
            }
            Ok(String::new())
        }
    }

    #[tokio::test]
    async fn every_precommit_failure_preserves_the_old_generation() {
        // Snapshot, clone, unmount, both renames, and final mountpoint change.
        for fail_at in 1..=6 {
            let temp = tempfile::tempdir().unwrap();
            let model = Model {
                data: Mutex::new(BTreeMap::from([("pool/child".into(), "old-data".into())])),
                mutation: Mutex::new(0),
                fail_at,
            };
            let mut record = Replacement {
                target: "pool/child".into(),
                staged: "pool/staged".into(),
                backup: "pool/backup".into(),
                snapshot: "pool/parent@new".into(),
                mountpoint: temp.path().join("data"),
                had_original: true,
                committed: false,
            };
            let result = replace(&model, &mut record, &temp.path().join("operation.json")).await;
            assert!(result.is_err(), "failure {fail_at}");
            assert_eq!(
                model
                    .data
                    .lock()
                    .unwrap()
                    .get("pool/child")
                    .map(String::as_str),
                Some("old-data"),
                "failure {fail_at}: {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn interrupted_publication_restores_the_recorded_backup() {
        let temp = tempfile::tempdir().unwrap();
        let model = Model {
            data: Mutex::new(BTreeMap::from([
                ("pool/backup".into(), "old-data".into()),
                ("pool/child".into(), "pool/parent@new".into()),
                ("pool/parent@new".into(), "snapshot".into()),
            ])),
            mutation: Mutex::new(0),
            fail_at: usize::MAX,
        };
        let record = Replacement {
            target: "pool/child".into(),
            staged: "pool/staged".into(),
            backup: "pool/backup".into(),
            snapshot: "pool/parent@new".into(),
            mountpoint: temp.path().join("data"),
            had_original: true,
            committed: false,
        };
        let journal = temp.path().join("operation.json");
        record.save(&journal).unwrap();
        recover(&model, &journal, &record.target).await.unwrap();
        assert_eq!(model.data.lock().unwrap()["pool/child"], "old-data");
        assert!(!journal.exists());
    }
}

// Run against a dedicated disposable pool in the Linux CI job. The fixture only
// destroys its own UUID-named subtree, never the supplied pool.
#[cfg(all(test, unix))]
mod linux_integration {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Zfs {
        count: AtomicUsize,
        fail_at: usize,
        after: bool,
    }
    async fn command(args: &[String]) -> Result<String> {
        let output = tokio::process::Command::new("zfs")
            .args(args)
            .output()
            .await?;
        anyhow::ensure!(
            output.status.success(),
            "zfs {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?)
    }
    #[async_trait]
    impl DatasetCommands for Zfs {
        async fn run(&self, args: &[String]) -> Result<String> {
            let mutation = !matches!(args[0].as_str(), "get" | "list");
            let fail = mutation && self.count.fetch_add(1, Ordering::SeqCst) + 1 == self.fail_at;
            anyhow::ensure!(!fail || self.after, "injected failure before mutation");
            let result = command(args).await?;
            anyhow::ensure!(!fail || !self.after, "injected failure after mutation");
            Ok(result)
        }
    }
    struct OwnedDataset(String);
    impl Drop for OwnedDataset {
        fn drop(&mut self) {
            let _ = std::process::Command::new("zfs")
                .args(["destroy", "-r", &self.0])
                .status();
        }
    }
    #[tokio::test]
    #[ignore = "requires Linux, ZFS, privileges and DEVFLOW_TEST_ZFS_POOL"]
    async fn linux_zfs_failures_preserve_mounted_data() {
        if !cfg!(target_os = "linux") {
            panic!("this test requires Linux ZFS");
        }
        let pool = std::env::var("DEVFLOW_TEST_ZFS_POOL").expect("set a disposable ZFS pool");
        for after in [false, true] {
            // Both sides of snapshot, clone, unmount, both renames and remount.
            for fail_at in 1..=6 {
                let temp = tempfile::tempdir().unwrap();
                let dataset = format!("{pool}/devflow_test_{}", uuid::Uuid::new_v4().simple());
                command(&[
                    "create".into(),
                    "-o".into(),
                    "mountpoint=none".into(),
                    dataset.clone(),
                ])
                .await
                .unwrap();
                let _owned = OwnedDataset(dataset.clone());
                let parent = format!("{dataset}/parent");
                let target = format!("{dataset}/child");
                for (name, path, data) in [
                    (&parent, temp.path().join("parent"), "new"),
                    (&target, temp.path().join("child"), "old"),
                ] {
                    command(&[
                        "create".into(),
                        "-o".into(),
                        format!("mountpoint={}", path.display()),
                        name.clone(),
                    ])
                    .await
                    .unwrap();
                    std::fs::write(path.join("generation"), data).unwrap();
                }
                let mut record = Replacement {
                    target: target.clone(),
                    staged: format!("{dataset}/stage"),
                    backup: format!("{dataset}/backup"),
                    snapshot: format!("{parent}@new"),
                    mountpoint: temp.path().join("child"),
                    had_original: true,
                    committed: false,
                };
                let ops = Zfs {
                    count: AtomicUsize::new(0),
                    fail_at,
                    after,
                };
                let journal = temp.path().join("replacement.json");
                assert!(
                    replace(&ops, &mut record, &journal).await.is_err(),
                    "failure {fail_at}, after={after}"
                );
                assert_eq!(
                    std::fs::read_to_string(temp.path().join("child/generation")).unwrap(),
                    "old",
                    "failure {fail_at}, after={after}"
                );
                assert!(!journal.exists());
                assert!(!ops.exists(&record.staged).await.unwrap());
                assert!(!ops.exists(&record.backup).await.unwrap());
                assert!(!ops.exists(&record.snapshot).await.unwrap());
                // The same data paths can subsequently complete a replacement.
                let healthy = Zfs {
                    count: AtomicUsize::new(0),
                    fail_at: usize::MAX,
                    after: false,
                };
                replace(&healthy, &mut record, &journal).await.unwrap();
                assert_eq!(
                    std::fs::read_to_string(temp.path().join("child/generation")).unwrap(),
                    "new"
                );
            }
        }
    }
}
