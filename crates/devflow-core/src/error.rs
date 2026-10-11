//! Typed errors that frontends act on (offer `--force`, show an
//! "initialize" button, report `in_progress`). They travel inside
//! `anyhow::Error`; match them with `error.downcast_ref::<T>()` instead of
//! inspecting message text.
use std::path::PathBuf;

/// The checkout has no `devflow.toml`.
#[derive(Debug, Clone)]
pub struct NotInitialized {
    pub root: PathBuf,
}

impl std::fmt::Display for NotInitialized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no devflow.toml in {}; run `devflow init` first",
            self.root.display()
        )
    }
}

impl std::error::Error for NotInitialized {}

/// Another setup run holds this workspace's lock.
#[derive(Debug, Clone)]
pub struct Busy {
    pub workspace: String,
}

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "setup is already running for '{}'; wait for it to finish",
            self.workspace
        )
    }
}

impl std::error::Error for Busy {}

/// A removal protection refused the operation before any effect.
#[derive(Debug, Clone)]
pub struct Protected {
    pub workspace: String,
    pub reason: String,
    /// `--force` would override this protection (dirty, locked, teardown
    /// failure, untrusted teardown); `false` for the default workspace and
    /// the caller's own workspace.
    pub forceable: bool,
}

impl std::fmt::Display for Protected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.forceable {
            write!(f, "{}, or pass --force", self.reason)
        } else {
            f.write_str(&self.reason)
        }
    }
}

impl std::error::Error for Protected {}

/// The configuration file changed on disk since the editor opened it.
#[derive(Debug, Clone)]
pub struct ConfigChanged {
    pub path: PathBuf,
}

impl std::fmt::Display for ConfigChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} changed on disk; reload before saving",
            self.path.display()
        )
    }
}

impl std::error::Error for ConfigChanged {}
