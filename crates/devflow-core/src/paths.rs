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

/// The `devflow` executable: `DEVFLOW_BIN`, this process when it is
/// `devflow`, a `devflow` beside it (the desktop app is installed next to the
/// CLI; a test binary lives one directory below it), else the one on `PATH`.
pub fn devflow_program() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("DEVFLOW_BIN") {
        return Ok(path.into());
    }
    let exe = std::env::current_exe().context("cannot find the running executable")?;
    if exe.file_stem().is_some_and(|stem| stem == "devflow") {
        return Ok(exe);
    }
    for dir in exe.ancestors().skip(1).take(2) {
        let sibling = dir.join("devflow");
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("devflow");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    anyhow::bail!("cannot find the `devflow` executable; put it on PATH or set DEVFLOW_BIN")
}
