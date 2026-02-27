use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

/// Tracks which hook commands a user has approved for a given project.
///
/// When a hook originates from a project config file (`.devflow.yml`), the user
/// must approve the commands before they run. Approved commands are stored in
/// the user-level config directory so they persist across invocations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ApprovalStore {
    /// project_path → { command_hash → ApprovalRecord }
    pub projects: HashMap<String, HashMap<String, ApprovalRecord>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRecord {
    /// The original command text that was approved
    pub command: String,
    /// When the approval was granted
    pub approved_at: chrono::DateTime<chrono::Utc>,
}

impl ApprovalStore {
    fn refresh_from_disk(&mut self) -> Result<()> {
        *self = Self::load()?;
        Ok(())
    }

    /// Load the approval store from the user config directory.
    pub fn load() -> Result<Self> {
        let path = Self::store_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read approval store: {}", path.display()))?;

        serde_yaml_ng::from_str(&content)
            .with_context(|| format!("Failed to parse approval store: {}", path.display()))
    }

    /// Save the approval store.
    pub fn save(&self) -> Result<()> {
        let path = Self::store_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
        }

        let lock_path = PathBuf::from(format!("{}.lock", path.display()));
        let _lock = acquire_file_lock(&lock_path)?;

        let content =
            serde_yaml_ng::to_string(self).context("Failed to serialize approval store")?;

        let tmp_path = PathBuf::from(format!("{}.tmp.{}", path.display(), std::process::id()));
        fs::write(&tmp_path, content).with_context(|| {
            format!(
                "Failed to write temporary approval store: {}",
                tmp_path.display()
            )
        })?;

        fs::rename(&tmp_path, &path)
            .with_context(|| format!("Failed to write approval store: {}", path.display()))?;

        Ok(())
    }

    /// Check if a command is approved for the given project.
    pub fn is_approved(&self, project_key: &str, command: &str) -> bool {
        let hash = Self::hash_command(command);
        self.projects
            .get(project_key)
            .and_then(|cmds| cmds.get(&hash))
            .map(|record| record.command == command)
            .unwrap_or(false)
    }

    /// Approve a command for a project.
    pub fn approve(&mut self, project_key: &str, command: &str) -> Result<()> {
        self.refresh_from_disk()?;

        let hash = Self::hash_command(command);
        let record = ApprovalRecord {
            command: command.to_string(),
            approved_at: chrono::Utc::now(),
        };

        self.projects
            .entry(project_key.to_string())
            .or_default()
            .insert(hash, record);

        self.save()
    }

    /// Approve all commands for a project at once.
    #[allow(dead_code)]
    pub fn approve_all(&mut self, project_key: &str, commands: &[String]) -> Result<()> {
        self.refresh_from_disk()?;

        let project_approvals = self.projects.entry(project_key.to_string()).or_default();

        for command in commands {
            let hash = Self::hash_command(command);
            project_approvals.insert(
                hash,
                ApprovalRecord {
                    command: command.to_string(),
                    approved_at: chrono::Utc::now(),
                },
            );
        }

        self.save()
    }

    /// Clear all approvals for a project.
    pub fn clear_project(&mut self, project_key: &str) -> Result<()> {
        self.refresh_from_disk()?;

        self.projects.remove(project_key);
        self.save()
    }

    /// Clear all approvals globally.
    #[allow(dead_code)]
    pub fn clear_all(&mut self) -> Result<()> {
        self.refresh_from_disk()?;

        self.projects.clear();
        self.save()
    }

    /// List all approved commands for a project.
    pub fn list_approved(&self, project_key: &str) -> Vec<&ApprovalRecord> {
        self.projects
            .get(project_key)
            .map(|cmds| cmds.values().collect())
            .unwrap_or_default()
    }

    fn store_path() -> Result<PathBuf> {
        let config_dir = dirs::config_dir()
            .context("Failed to get user config directory")?
            .join("devflow");
        Ok(config_dir.join("hook_approvals.yml"))
    }

    fn hash_command(command: &str) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        command.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }
}

struct FileLockGuard {
    path: PathBuf,
}

impl Drop for FileLockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn acquire_file_lock(lock_path: &PathBuf) -> Result<FileLockGuard> {
    const MAX_ATTEMPTS: usize = 200;
    const SLEEP_MS: u64 = 25;
    const STALE_LOCK_SECS: u64 = 30;

    for _ in 0..MAX_ATTEMPTS {
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(lock_path)
        {
            Ok(_) => {
                return Ok(FileLockGuard {
                    path: lock_path.clone(),
                });
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                if let Ok(metadata) = fs::metadata(lock_path) {
                    if let Ok(modified) = metadata.modified() {
                        if modified.elapsed().unwrap_or_default().as_secs() > STALE_LOCK_SECS {
                            let _ = fs::remove_file(lock_path);
                            continue;
                        }
                    }
                }
                thread::sleep(Duration::from_millis(SLEEP_MS));
            }
            Err(e) => {
                let msg = format!("Failed to acquire lock '{}': {}", lock_path.display(), e);
                return Err(e).context(msg);
            }
        }
    }

    anyhow::bail!("Timed out waiting for lock file '{}'", lock_path.display())
}
