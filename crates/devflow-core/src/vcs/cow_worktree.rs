use anyhow::{Context, Result};
use std::path::Path;

/// Filesystem Copy-on-Write capability
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CowCapability {
    /// macOS APFS clonefile (cp -c)
    Apfs,
    /// Linux reflink (cp --reflink=always) — Btrfs, XFS
    #[allow(dead_code)]
    Reflink,
    /// No CoW support detected
    None,
}

/// Detect CoW capability of the filesystem at the given path.
/// Creates a temporary probe file to test actual clonefile/reflink support.
///
/// Uses blocking `std::process::Command` since the VcsProvider trait is sync.
pub fn detect_cow_capability(path: &Path) -> CowCapability {
    #[cfg(target_os = "macos")]
    {
        if test_apfs_clone(path) {
            return CowCapability::Apfs;
        }
    }

    #[cfg(target_os = "linux")]
    {
        if test_reflink(path) {
            return CowCapability::Reflink;
        }
    }

    // Suppress unused-variable warning on platforms where neither workspace fires
    let _ = path;

    CowCapability::None
}

/// Detect CoW capability across two directories (source → target).
///
/// Unlike `detect_cow_capability` which tests within a single directory,
/// this creates a probe file in `source_dir` and attempts to clone it into
/// `target_parent`. This catches cross-volume failures where source and target
/// reside on different filesystems (e.g. different APFS volumes).
pub fn detect_cow_capability_cross(source_dir: &Path, target_parent: &Path) -> CowCapability {
    #[cfg(target_os = "macos")]
    {
        if test_apfs_clone_cross(source_dir, target_parent) {
            return CowCapability::Apfs;
        }
    }

    #[cfg(target_os = "linux")]
    {
        if test_reflink_cross(source_dir, target_parent) {
            return CowCapability::Reflink;
        }
    }

    let _ = source_dir;
    let _ = target_parent;

    CowCapability::None
}

/// Generate a unique probe directory name using PID + timestamp (avoids
/// depending on the optional `uuid` crate).
fn probe_dir_name() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(".devflow-cow-probe-{}-{}", std::process::id(), ts)
}

#[cfg(target_os = "macos")]
fn test_apfs_clone(path: &Path) -> bool {
    use std::process::Command;

    let probe_dir = path.join(probe_dir_name());
    let _ = std::fs::create_dir_all(&probe_dir);

    let src = probe_dir.join("src");
    let dst = probe_dir.join("dst");

    let result = (|| -> bool {
        if std::fs::write(&src, b"devflow").is_err() {
            return false;
        }
        let status = Command::new("cp").args(["-c"]).arg(&src).arg(&dst).output();
        matches!(status, Ok(output) if output.status.success())
    })();

    let _ = std::fs::remove_dir_all(&probe_dir);
    result
}

#[cfg(target_os = "macos")]
fn test_apfs_clone_cross(source_dir: &Path, target_parent: &Path) -> bool {
    use std::process::Command;

    let src_probe = source_dir.join(probe_dir_name());
    let dst_probe = target_parent.join(probe_dir_name());
    let _ = std::fs::create_dir_all(&src_probe);
    let _ = std::fs::create_dir_all(&dst_probe);

    let src_file = src_probe.join("src");
    let dst_file = dst_probe.join("dst");

    let result = (|| -> bool {
        if std::fs::write(&src_file, b"devflow").is_err() {
            return false;
        }
        let status = Command::new("cp")
            .args(["-c"])
            .arg(&src_file)
            .arg(&dst_file)
            .output();
        matches!(status, Ok(output) if output.status.success())
    })();

    let _ = std::fs::remove_dir_all(&src_probe);
    let _ = std::fs::remove_dir_all(&dst_probe);
    result
}

#[cfg(target_os = "linux")]
fn test_reflink(path: &Path) -> bool {
    use std::process::Command;

    let probe_dir = path.join(probe_dir_name());
    let _ = std::fs::create_dir_all(&probe_dir);

    let src = probe_dir.join("src");
    let dst = probe_dir.join("dst");

    let result = (|| -> bool {
        if std::fs::write(&src, b"devflow").is_err() {
            return false;
        }
        let status = Command::new("cp")
            .args(["--reflink=always"])
            .arg(&src)
            .arg(&dst)
            .output();
        matches!(status, Ok(output) if output.status.success())
    })();

    let _ = std::fs::remove_dir_all(&probe_dir);
    result
}

#[cfg(target_os = "linux")]
fn test_reflink_cross(source_dir: &Path, target_parent: &Path) -> bool {
    use std::process::Command;

    let src_probe = source_dir.join(probe_dir_name());
    let dst_probe = target_parent.join(probe_dir_name());
    let _ = std::fs::create_dir_all(&src_probe);
    let _ = std::fs::create_dir_all(&dst_probe);

    let src_file = src_probe.join("src");
    let dst_file = dst_probe.join("dst");

    let result = (|| -> bool {
        if std::fs::write(&src_file, b"devflow").is_err() {
            return false;
        }
        let status = Command::new("cp")
            .args(["--reflink=always"])
            .arg(&src_file)
            .arg(&dst_file)
            .output();
        matches!(status, Ok(output) if output.status.success())
    })();

    let _ = std::fs::remove_dir_all(&src_probe);
    let _ = std::fs::remove_dir_all(&dst_probe);
    result
}

/// Create a worktree using Copy-on-Write if the filesystem supports it.
///
/// The CoW fast path:
/// 1. `git worktree add --no-checkout <path> <workspace>` — registers worktree
///    without performing a full checkout.
/// 2. For each top-level entry in the source (excluding `.git`):
///    `cp -cR` (macOS) or `cp -a --reflink=always` (Linux).
/// 3. `git -C <path> reset --no-refresh` — rebuilds the index to match the
///    copied working tree.
///
/// Returns `Ok(true)` if CoW was used, `Ok(false)` if the caller should fall
/// back to the standard git2 worktree creation (CoW not available).
/// Returns `Err` if CoW was attempted but failed mid-way (worktree is cleaned up).
pub fn create_cow_worktree(source_dir: &Path, target_path: &Path, workspace: &str) -> Result<bool> {
    // Resolve the target's parent directory for the cross-directory probe.
    // Ensure it exists so the probe can write into it.
    let target_parent = target_path.parent().unwrap_or(target_path);
    if !target_parent.exists() {
        std::fs::create_dir_all(target_parent)
            .context("Failed to create target parent directory for CoW probe")?;
    }

    let cow = detect_cow_capability_cross(source_dir, target_parent);

    if cow == CowCapability::None {
        return Ok(false); // Caller should use standard worktree creation
    }

    log::debug!(
        "CoW capability detected: {:?} — using fast worktree path",
        cow
    );

    // Step 1: git worktree add --no-checkout
    let output = std::process::Command::new("git")
        .args([
            "worktree",
            "add",
            "--no-checkout",
            target_path
                .to_str()
                .context("Target path is not valid UTF-8")?,
            workspace,
        ])
        .current_dir(source_dir)
        .output()
        .context("Failed to run git worktree add --no-checkout")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git worktree add --no-checkout failed: {}", stderr.trim());
    }

    // Step 2: Copy working tree files using CoW
    if let Err(e) = cow_copy_working_tree(source_dir, target_path, cow) {
        // If copy fails, try to clean up the registered worktree
        log::warn!("CoW copy failed: {e:#}. Cleaning up worktree registration.");
        let _ = std::process::Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(target_path)
            .current_dir(source_dir)
            .output();
        return Err(e.context("Failed to CoW-copy working tree files"));
    }

    // Step 3: Reset the index in the new worktree so it matches the copied files
    let output = std::process::Command::new("git")
        .args(["reset", "--no-refresh"])
        .current_dir(target_path)
        .output()
        .context("Failed to run git reset --no-refresh")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Non-fatal: the worktree is usable, the index just may need a refresh
        log::warn!(
            "git reset --no-refresh warning (non-fatal): {}",
            stderr.trim()
        );
    }

    Ok(true)
}

/// Copy all top-level entries from source to target using CoW, skipping `.git`.
///
/// Uses a single `cp` invocation with all entries to avoid per-entry process
/// spawning overhead. Fails fast if the copy fails — no silent fallback.
/// The caller is responsible for cleaning up and falling back to git2.
fn cow_copy_working_tree(source: &Path, target: &Path, cow: CowCapability) -> Result<()> {
    let src_paths: Vec<_> = std::fs::read_dir(source)
        .context("Failed to read source directory")?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            // Skip .git and anything that already exists in the target
            // (e.g. the .git file created by worktree add)
            name != ".git" && !target.join(&name).exists()
        })
        .map(|e| e.path())
        .collect();

    if src_paths.is_empty() {
        return Ok(());
    }

    log::debug!(
        "CoW copying {} top-level entries in a single invocation",
        src_paths.len()
    );

    // Single cp invocation: cp -cR src1 src2 ... target/
    let mut cmd = std::process::Command::new("cp");
    match cow {
        CowCapability::Apfs => {
            cmd.arg("-cR");
        }
        CowCapability::Reflink => {
            cmd.args(["-a", "--reflink=always"]);
        }
        CowCapability::None => unreachable!(),
    }
    for path in &src_paths {
        cmd.arg(path);
    }
    cmd.arg(target);

    let output = cmd.output().context("Failed to run cp for CoW copy")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("CoW copy failed: {}", stderr.trim());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_cow_capability_returns_value() {
        let tmp = tempfile::tempdir().unwrap();
        let cap = detect_cow_capability(tmp.path());
        // On macOS with APFS this should be Apfs; on Linux with Btrfs/XFS
        // it should be Reflink; otherwise None. We just check it doesn't panic.
        assert!(matches!(
            cap,
            CowCapability::Apfs | CowCapability::Reflink | CowCapability::None
        ));
    }

    #[test]
    fn test_detect_cow_capability_cross_same_volume() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src_dir");
        let dst = tmp.path().join("dst_dir");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();

        let cap = detect_cow_capability_cross(&src, &dst);
        // Same temp volume — should match single-dir detection
        let single = detect_cow_capability(tmp.path());
        assert_eq!(cap, single);
    }

    #[test]
    fn test_probe_dir_name_is_unique() {
        let a = probe_dir_name();
        // Small sleep to ensure timestamp changes
        std::thread::sleep(std::time::Duration::from_millis(1));
        let b = probe_dir_name();
        assert_ne!(a, b);
    }
}
