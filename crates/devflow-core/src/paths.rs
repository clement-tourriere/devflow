//! Machine-local directories. `DEVFLOW_CONFIG_DIR` and `DEVFLOW_DATA_DIR`
//! override the platform defaults (tests use them for isolation).
//!
//! # Tests never reach the real directories
//!
//! A debug build refuses to resolve the platform default of either directory
//! when the running executable is a cargo test binary (it lives in a `deps`
//! folder), so a test that forgot to point `DEVFLOW_CONFIG_DIR` and
//! `DEVFLOW_DATA_DIR` at a scratch folder fails instead of writing to the
//! user's project list or Postgres data. (Once, tests left their scratch
//! projects in the real registry.) A test that spawns the `devflow` binary
//! sets both variables on the child. Core's own unit tests need nothing: with
//! no variable set they get a scratch folder shared by the test process.
use anyhow::{Context, Result};
use std::path::PathBuf;

/// What stands in for the platform directory `kind` when its variable is not
/// set and this is a test: core's unit tests get a scratch folder (one per
/// test process); any other test binary in a debug build gets an error. A real
/// program gets `None` and uses the platform default.
fn test_stand_in(variable: &str, kind: &str) -> Result<Option<PathBuf>> {
    #[cfg(test)]
    {
        static ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        let _ = variable;
        let root = ROOT.get_or_init(|| tempfile::tempdir().expect("a scratch directory"));
        Ok(Some(root.path().join(kind)))
    }
    #[cfg(not(test))]
    {
        let _ = kind;
        if cfg!(debug_assertions)
            && std::env::current_exe().is_ok_and(|exe| {
                exe.parent()
                    .and_then(|dir| dir.file_name())
                    .is_some_and(|dir| dir == "deps")
            })
        {
            anyhow::bail!(
                "a test is using the real devflow directories; set {variable} (and the other of \
                 DEVFLOW_CONFIG_DIR / DEVFLOW_DATA_DIR) to a scratch folder first"
            );
        }
        Ok(None)
    }
}

/// Projects registry and hook trust.
pub fn config_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("DEVFLOW_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }
    if let Some(dir) = test_stand_in("DEVFLOW_CONFIG_DIR", "config")? {
        return Ok(dir);
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
    if let Some(dir) = test_stand_in("DEVFLOW_DATA_DIR", "data")? {
        return Ok(dir);
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
