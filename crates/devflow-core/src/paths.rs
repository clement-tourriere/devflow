//! Machine-local directories. `DEVFLOW_CONFIG_DIR` and `DEVFLOW_DATA_DIR`
//! override the platform defaults (tests use them for isolation).
use anyhow::{Context, Result};
use std::path::PathBuf;

/// Projects registry and hook trust.
pub fn config_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("DEVFLOW_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }
    Ok(dirs::config_dir()
        .context("no configuration directory for this platform")?
        .join("devflow"))
}

/// Managed Postgres servers (compose files, admin credentials).
pub fn data_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("DEVFLOW_DATA_DIR") {
        return Ok(PathBuf::from(dir));
    }
    Ok(dirs::data_dir()
        .context("no data directory for this platform")?
        .join("devflow"))
}
