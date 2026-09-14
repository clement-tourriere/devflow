//! Exact ownership of external resources. Display-name prefixes are never proof of ownership.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

pub fn identity_digest(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

/// OS locks are released on process exit. Never steal a lock based on its age.
pub struct OperationLease {
    _file: File,
}

impl OperationLease {
    pub async fn acquire(path: &Path) -> Result<Self> {
        Self::acquire_mode(path, false).await
    }
    pub async fn acquire_shared(path: &Path) -> Result<Self> {
        Self::acquire_mode(path, true).await
    }
    async fn acquire_mode(path: &Path, shared: bool) -> Result<Self> {
        fs::create_dir_all(path.parent().context("lock path has no parent")?)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            match if shared {
                file.try_lock_shared()
            } else {
                file.try_lock()
            } {
                Ok(()) => return Ok(Self { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) => {
                    anyhow::ensure!(
                        tokio::time::Instant::now() < deadline,
                        "another operation holds {}; retry after it finishes",
                        path.display()
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
    }

    pub fn try_acquire(path: &Path) -> Result<Option<Self>> {
        fs::create_dir_all(path.parent().context("lock path has no parent")?)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }

    pub fn acquire_sync(path: &Path) -> Result<Self> {
        fs::create_dir_all(path.parent().context("lock path has no parent")?)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("cannot open operation lock {}", path.display()))?;
        file.lock().context("cannot lock operation")?;
        Ok(Self { _file: file })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceRecord {
    pub version: u32,
    pub project: String,
    pub service: String,
    pub workspace: String,
    pub resource: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub parent: Option<String>,
    pub ready: bool,
    #[serde(default)]
    pub deleted: bool,
}

impl ResourceRecord {
    pub fn into_workspace_info(self, state: &str) -> super::WorkspaceInfo {
        super::WorkspaceInfo {
            name: self.workspace,
            created_at: Some(self.created_at),
            parent_workspace: self.parent,
            database_name: self.resource,
            state: Some(state.into()),
        }
    }
}

#[derive(Clone)]
pub struct ResourceRegistry {
    project: String,
    service: String,
    root: PathBuf,
    legacy: HashMap<String, String>,
}

impl ResourceRegistry {
    pub fn new(
        project: &str,
        service: &str,
        engine: &str,
        legacy: HashMap<String, String>,
    ) -> Result<Self> {
        let host = std::env::var("DOCKER_HOST").unwrap_or_default();
        let context = std::env::var("DOCKER_CONTEXT").unwrap_or_default();
        Ok(Self {
            project: project.into(),
            service: service.into(),
            root: crate::paths::devflow_config_dir()?
                .join("resources")
                .join(identity_digest(&[&host, &context, engine])),
            legacy,
        })
    }

    pub fn resource_name(&self, workspace: &str) -> String {
        self.legacy.get(workspace).cloned().unwrap_or_else(|| {
            // Bounded, valid for PostgreSQL, ClickHouse, S3 and Docker. Hash the
            // complete raw identity, including separators and Unicode.
            format!(
                "df-{}",
                &identity_digest(&[&self.project, &self.service, workspace])[..48]
            )
        })
    }

    pub fn explicitly_adopted(&self, workspace: &str) -> bool {
        self.legacy.contains_key(workspace)
    }

    pub async fn lease(&self, workspace: &str) -> Result<OperationLease> {
        OperationLease::acquire(&self.operation_path(workspace)).await
    }

    pub fn operation_path(&self, workspace: &str) -> PathBuf {
        self.root.join("operations").join(format!(
            "{}.lock",
            identity_digest(&[&self.resource_name(workspace)])
        ))
    }

    pub fn check_existing(&self, workspace: &str, exists: bool) -> Result<()> {
        anyhow::ensure!(!exists || self.record(workspace)?.is_some() || self.explicitly_adopted(workspace),
            "resource '{}' exists without an ownership record; explicitly map it in shared.legacy_resources before adoption",
            self.resource_name(workspace));
        Ok(())
    }

    pub fn check_legacy(&self, workspace: &str, legacy: &str, exists: bool) -> Result<()> {
        anyhow::ensure!(!exists || self.record(workspace)?.is_some() || self.explicitly_adopted(workspace),
            "legacy resource '{legacy}' may contain existing data; refusing to create a replacement. Verify ownership and set shared.legacy_resources['{workspace}'] to '{legacy}'");
        Ok(())
    }

    fn path(&self, resource: &str) -> PathBuf {
        self.root
            .join(format!("{}.json", identity_digest(&[resource])))
    }

    fn owns(&self, record: &ResourceRecord) -> bool {
        record.project == self.project && record.service == self.service
    }

    fn read(&self, path: &Path) -> Result<ResourceRecord> {
        let record: ResourceRecord =
            serde_json::from_slice(&fs::read(path)?).with_context(|| {
                format!(
                    "invalid ownership record {}; refusing resource operations",
                    path.display()
                )
            })?;
        anyhow::ensure!(
            record.version == 1 && path == self.path(&record.resource),
            "invalid resource ownership record {}",
            path.display()
        );
        Ok(record)
    }

    pub fn record(&self, workspace: &str) -> Result<Option<ResourceRecord>> {
        let path = self.path(&self.resource_name(workspace));
        if !path.exists() {
            return Ok(None);
        }
        let record = self.read(&path)?;
        anyhow::ensure!(self.owns(&record) && record.workspace == workspace,
            "resource '{}' is owned by another project, service, or workspace; refusing to reuse it", record.resource);
        Ok((!record.deleted).then_some(record))
    }

    pub fn list(&self) -> Result<Vec<ResourceRecord>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut records = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "json") {
                let record = self.read(&path)?;
                if self.owns(&record) && !record.deleted {
                    records.push(record);
                }
            }
        }
        records.sort_by(|a, b| a.workspace.cmp(&b.workspace));
        Ok(records)
    }

    /// Reserve before provisioning so interruption can be retried. Explicit
    /// legacy mappings are claims too: another owner can never claim the same resource.
    pub fn claim(&self, workspace: &str, parent: Option<&str>) -> Result<ResourceRecord> {
        let _lease = OperationLease::acquire_sync(&self.root.join("registry.lock"))?;
        if let Some(record) = self.record(workspace)? {
            return Ok(record);
        }
        anyhow::ensure!(!self.list()?.iter().any(|r| r.workspace == workspace),
            "resource mapping for '{workspace}' changed; restore its previous mapping before continuing");
        let record = ResourceRecord {
            version: 1,
            project: self.project.clone(),
            service: self.service.clone(),
            workspace: workspace.into(),
            resource: self.resource_name(workspace),
            created_at: chrono::Utc::now(),
            parent: parent.map(str::to_string),
            ready: false,
            deleted: false,
        };
        self.write(&record)?;
        Ok(record)
    }

    pub fn complete(&self, workspace: &str) -> Result<()> {
        let _lease = OperationLease::acquire_sync(&self.root.join("registry.lock"))?;
        let mut record = self
            .record(workspace)?
            .context("resource was not reserved")?;
        record.ready = true;
        self.write(&record)
    }

    fn write(&self, record: &ResourceRecord) -> Result<()> {
        atomic_write(
            &self.path(&record.resource),
            &serde_json::to_vec_pretty(record)?,
        )
    }

    pub fn was_deleted(&self, workspace: &str) -> Result<bool> {
        let path = self.path(&self.resource_name(workspace));
        if !path.exists() {
            return Ok(false);
        }
        let record = self.read(&path)?;
        Ok(self.owns(&record) && record.workspace == workspace && record.deleted)
    }

    pub fn release(&self, workspace: &str) -> Result<()> {
        let _lease = OperationLease::acquire_sync(&self.root.join("registry.lock"))?;
        if let Some(mut record) = self.record(workspace)? {
            record.deleted = true;
            record.ready = false;
            self.write(&record)?;
        }
        Ok(())
    }
}

pub fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().context("file has no parent directory")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".devflow-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(content)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_encodes_boundaries_and_raw_characters() {
        let names = [
            "main",
            "_main",
            "MAIN",
            "feature/auth",
            "feature-auth",
            "é",
            "e",
            "a__b",
            "a_b",
        ];
        let mut keys = std::collections::HashSet::new();
        for project in ["app", "app_child"] {
            for service in ["db", "analytics"] {
                for workspace in names {
                    assert!(keys.insert(identity_digest(&[project, service, workspace])));
                }
            }
        }
        assert_ne!(identity_digest(&["a", "bc"]), identity_digest(&["ab", "c"]));
    }

    #[test]
    fn exact_claims_preserve_neighbors_and_reject_conflicting_adoption() {
        let root = std::env::temp_dir().join(format!("devflow-ownership-{}", uuid::Uuid::new_v4()));
        let a = ResourceRegistry {
            project: "app".into(),
            service: "db".into(),
            root: root.clone(),
            legacy: HashMap::new(),
        };
        let b = ResourceRegistry {
            project: "app_child".into(),
            ..a.clone()
        };
        let ra = a.claim("main", None).unwrap();
        let rb = b.claim("main", None).unwrap();
        assert_ne!(ra.resource, rb.resource);
        assert_eq!(a.list().unwrap().len(), 1);
        let mut conflicting = b.clone();
        conflicting
            .legacy
            .insert("other".into(), ra.resource.clone());
        assert!(conflicting.claim("other", None).is_err());
        a.release("main").unwrap();
        assert!(b.record("main").unwrap().is_some());
        fs::remove_dir_all(root).unwrap();
    }
}
