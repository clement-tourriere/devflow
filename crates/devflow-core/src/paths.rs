//! Shared filesystem locations for devflow's user-level files.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// The devflow user config directory (workspace registry, hook approvals,
/// global config, daemon pid/status files).
///
/// Honors `DEVFLOW_CONFIG_DIR` so tests and sandboxed environments can
/// redirect every user-level file at once; defaults to
/// `<platform config dir>/devflow` (e.g. `~/.config/devflow`).
pub fn devflow_config_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("DEVFLOW_CONFIG_DIR") {
        if !dir.trim().is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    Ok(dirs::config_dir()
        .context("Failed to get user config directory")?
        .join("devflow"))
}

// Canonicalize the existing prefix without materializing the planned directory.
pub(crate) fn resolve_expected_path(path: &Path) -> Result<PathBuf> {
    for ancestor in path.ancestors() {
        match ancestor.canonicalize() {
            Ok(mut prefix) => {
                for part in path.strip_prefix(ancestor)?.components() {
                    match part {
                        std::path::Component::ParentDir => {
                            prefix.pop();
                        }
                        std::path::Component::Normal(name) => prefix.push(name),
                        std::path::Component::CurDir => (),
                        _ => anyhow::bail!("Invalid workspace path"),
                    }
                }
                return Ok(prefix);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::bail!("Workspace path has no existing parent")
}
