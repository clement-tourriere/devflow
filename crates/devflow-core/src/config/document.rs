//! Format-aware project documents. Effective overlays are read separately and
//! must never be flattened back into the shared project file.
use super::Config;
use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize)]
pub struct ConfigDocument {
    pub path: PathBuf,
    pub format: String,
    pub content: String,
    pub config: Config,
    /// Read-only merged values, never used as the source for a project save.
    pub effective: Config,
    pub service_sources: Vec<super::ServiceWithSource>,
}

impl ConfigDocument {
    pub fn open(project: &Path) -> Result<Self> {
        let path = Config::resolve_config_path(project, false)
            .context("No devflow project configuration found")?;
        let content = std::fs::read_to_string(&path)?;
        let config = Config::from_file(&path)?;
        let format = Self::format(&path).into();
        let effective = Config::load_effective_for_dir(project)?;
        let service_sources = config.services_with_sources(&path);
        Ok(Self {
            path,
            format,
            content,
            config,
            effective,
            service_sources,
        })
    }

    pub fn format(path: &Path) -> &'static str {
        if path.extension().is_some_and(|e| e == "toml") {
            "toml"
        } else {
            "yaml"
        }
    }

    pub fn parse(format: &str, content: &str) -> Result<Config> {
        match format {
            "toml" => toml::from_str(content).context("Invalid TOML configuration"),
            "yaml" => serde_yaml_ng::from_str(content).context("Invalid YAML configuration"),
            _ => anyhow::bail!("Unsupported configuration format: {format}"),
        }
    }

    pub fn save_text(&self, content: &str) -> Result<()> {
        Self::parse(&self.format, content)?;
        // Refuse stale editor writes; raw mode preserves comments and formatting.
        anyhow::ensure!(
            std::fs::read_to_string(&self.path)? == self.content,
            "Configuration changed on disk; reload before saving"
        );
        Self::write_text(&self.path, content)
    }

    pub fn write_config(path: &Path, config: &Config) -> Result<()> {
        let content = if Self::format(path) == "toml" {
            toml::to_string_pretty(config).context("Cannot represent this configuration as TOML")?
        } else {
            serde_yaml_ng::to_string(config)?
        };
        Self::parse(Self::format(path), &content)?;
        Self::write_text(path, &content)
    }

    fn write_text(path: &Path, content: &str) -> Result<()> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let name = path
            .file_name()
            .context("Configuration file name is missing")?;
        crate::files::WorkspaceFiles::open(parent)?
            .write(
                Path::new(name),
                content.as_bytes(),
                crate::files::WritePolicy::Replace,
            )
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edits_use_the_existing_format_and_preserve_raw_comments() {
        for name in [
            ".devflow.yml",
            ".devflow.yaml",
            ".devflow.toml",
            "devflow.toml",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(name);
            ConfigDocument::write_config(&path, &Config::default()).unwrap();
            let doc = ConfigDocument::open(dir.path()).unwrap();
            assert_eq!(doc.path, path);
            doc.save_text(&format!("# a comment\n{}", doc.content))
                .unwrap();
            assert!(std::fs::read_to_string(&path)
                .unwrap()
                .starts_with("# a comment"));
            assert!(
                doc.save_text(&doc.content).is_err(),
                "stale document must not overwrite a newer edit"
            );
            assert!(Config::from_file(&path).is_ok());
        }
    }
}
