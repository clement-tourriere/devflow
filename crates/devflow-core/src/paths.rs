//! Shared filesystem locations for devflow's user-level files.

use anyhow::{Context, Result};
use std::path::PathBuf;

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
