use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

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

        let content =
            serde_yaml_ng::to_string(self).context("Failed to serialize approval store")?;
        fs::write(&path, content)
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
        self.projects.remove(project_key);
        self.save()
    }

    /// Clear all approvals globally.
    #[allow(dead_code)]
    pub fn clear_all(&mut self) -> Result<()> {
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
