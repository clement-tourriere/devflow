//! Shared detached-process management.
//!
//! The controller daemon, the proxy CLI, and the TUI all follow the same
//! pattern: re-exec the current binary in the background, record its pid in
//! a pidfile, probe liveness with signal 0, and stop with SIGTERM. This
//! module is that pattern, once.

use anyhow::{Context, Result};
use std::path::Path;

/// Spawn the current executable with `args`, detached from the terminal
/// (stdin/stdout/stderr → null), and record the child's pid at `pid_path`.
/// Returns the child pid.
pub fn spawn_self_detached<S: AsRef<std::ffi::OsStr>>(pid_path: &Path, args: &[S]) -> Result<u32> {
    let exe = std::env::current_exe().context("Failed to resolve current executable")?;
    let child = std::process::Command::new(exe)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("Failed to spawn detached process")?;

    if let Some(parent) = pid_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(pid_path, child.id().to_string())
        .with_context(|| format!("Failed to write pidfile {}", pid_path.display()))?;

    Ok(child.id())
}

/// Read the pid recorded at `pid_path`. Returns `None` when the file is
/// missing or unparseable.
pub fn read_pid(pid_path: &Path) -> Option<i32> {
    std::fs::read_to_string(pid_path).ok()?.trim().parse().ok()
}

/// Is a process with `pid` alive? (signal 0 probes without killing.)
#[cfg(unix)]
pub fn process_alive(pid: i32) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    kill(Pid::from_raw(pid), None).is_ok()
}
#[cfg(not(unix))]
pub fn process_alive(_pid: i32) -> bool {
    false
}

/// Whether the process recorded at `pid_path` is currently running.
pub fn pidfile_alive(pid_path: &Path) -> bool {
    read_pid(pid_path).map(process_alive).unwrap_or(false)
}

/// SIGTERM the process recorded at `pid_path` and remove the pidfile.
/// Returns `Ok(Some(pid))` when a recorded process was signalled,
/// `Ok(None)` when no pidfile existed, and an error when the pidfile
/// exists but is unreadable — in that case the file is PRESERVED, because
/// deleting it would destroy the only record of a possibly-live daemon.
pub fn stop(pid_path: &Path) -> Result<Option<i32>> {
    if !pid_path.exists() {
        return Ok(None);
    }
    let Some(pid) = read_pid(pid_path) else {
        anyhow::bail!(
            "pidfile {} exists but does not contain a valid pid; \
             stop the process manually, then delete the file",
            pid_path.display()
        );
    };
    #[cfg(unix)]
    {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
    }
    let _ = std::fs::remove_file(pid_path);
    Ok(Some(pid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_returns_none_when_no_pidfile_exists() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("missing.pid");
        assert!(stop(&pid_path).unwrap().is_none());
    }

    #[test]
    fn stop_preserves_an_unparseable_pidfile_and_errors() {
        // Deleting a corrupt pidfile would destroy the only record of a
        // possibly-live daemon and report it as "not running".
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("daemon.pid");
        std::fs::write(&pid_path, "not-a-pid").unwrap();

        let err = stop(&pid_path).expect_err("corrupt pidfile must error");
        assert!(err.to_string().contains("does not contain a valid pid"));
        assert!(pid_path.exists(), "pidfile must be preserved");
        assert_eq!(std::fs::read_to_string(&pid_path).unwrap(), "not-a-pid");
    }
}
