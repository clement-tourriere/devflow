/// Linux Landlock filesystem sandbox.
///
/// This module provides filesystem isolation using the Linux Landlock LSM.
/// It restricts file access to a set of allowed paths, with the workspace
/// directory being the only writable location by default.
///
/// Requires the `sandbox-landlock` feature and Linux kernel 5.13+.
#[cfg(all(target_os = "linux", feature = "sandbox-landlock"))]
use std::path::{Path, PathBuf};

#[cfg(all(target_os = "linux", feature = "sandbox-landlock"))]
use landlock::{
    Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, ABI,
};

/// Apply Landlock filesystem restrictions to the current process.
///
/// This should be called in a child process (e.g., via `pre_exec`) before
/// executing the sandboxed command.
///
/// Default allowed paths:
/// - Read: /usr, /bin, /sbin, /lib, /lib64, /etc, /proc, /dev, /nix,
///   /opt/homebrew, ~/.cargo, ~/.rustup, ~/.nvm, ~/.local
/// - Read+Write: workspace_dir, $TMPDIR or /tmp
/// - Extra from config: extra_read_paths, extra_write_paths
#[cfg(all(target_os = "linux", feature = "sandbox-landlock"))]
pub fn apply_landlock(
    workspace_dir: &Path,
    extra_read_paths: &[PathBuf],
    extra_write_paths: &[PathBuf],
) -> anyhow::Result<()> {
    let abi = ABI::V3;

    let read_access = AccessFs::from_read(abi);
    let write_access = AccessFs::from_all(abi);

    let mut ruleset = Ruleset::default()
        .handle_access(write_access)?
        .create()?;

    // System read-only paths
    let system_read_paths = [
        "/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc", "/proc", "/dev", "/nix",
        "/opt/homebrew",
    ];

    for path in &system_read_paths {
        if Path::new(path).exists() {
            if let Ok(fd) = PathFd::new(path) {
                let _ = ruleset.add_rule(PathBeneath::new(fd, read_access));
            }
        }
    }

    // User tool directories (read-only)
    if let Some(home) = dirs::home_dir() {
        let user_read_dirs = [".cargo", ".rustup", ".nvm", ".local", ".bun", ".npm", ".config"];
        for dir in &user_read_dirs {
            let full_path = home.join(dir);
            if full_path.exists() {
                if let Ok(fd) = PathFd::new(&full_path) {
                    let _ = ruleset.add_rule(PathBeneath::new(fd, read_access));
                }
            }
        }
    }

    // Extra read paths from config
    for path in extra_read_paths {
        if path.exists() {
            if let Ok(fd) = PathFd::new(path) {
                let _ = ruleset.add_rule(PathBeneath::new(fd, read_access));
            }
        }
    }

    // Workspace directory (read+write)
    if workspace_dir.exists() {
        if let Ok(fd) = PathFd::new(workspace_dir) {
            ruleset.add_rule(PathBeneath::new(fd, write_access))?;
        }
    }

    // Temp directory (read+write)
    let tmpdir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
    let tmpdir_path = Path::new(&tmpdir);
    if tmpdir_path.exists() {
        if let Ok(fd) = PathFd::new(tmpdir_path) {
            let _ = ruleset.add_rule(PathBeneath::new(fd, write_access));
        }
    }

    // Extra write paths from config
    for path in extra_write_paths {
        if path.exists() {
            if let Ok(fd) = PathFd::new(path) {
                let _ = ruleset.add_rule(PathBeneath::new(fd, write_access));
            }
        }
    }

    ruleset.restrict_self()?;
    Ok(())
}

/// Stub for non-Linux or non-landlock builds.
#[cfg(not(all(target_os = "linux", feature = "sandbox-landlock")))]
pub fn apply_landlock(
    _workspace_dir: &std::path::Path,
    _extra_read_paths: &[std::path::PathBuf],
    _extra_write_paths: &[std::path::PathBuf],
) -> anyhow::Result<()> {
    // Landlock is not available on this platform/build
    Ok(())
}
