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

    // Suppress unused-variable warning on platforms where neither branch fires
    let _ = path;

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

/// Create a worktree using Copy-on-Write if the filesystem supports it.
/// Falls back gracefully — returns `Ok(false)` when CoW is unavailable so the
/// caller can use the standard git2 worktree path.
///
/// The CoW fast path:
/// 1. `git worktree add --no-checkout <path> <branch>` — registers worktree
///    without performing a full checkout.
/// 2. For each top-level entry in the source (excluding `.git`):
///    `cp -cR` (macOS) or `cp -a --reflink=auto` (Linux).
/// 3. `git -C <path> reset --no-refresh` — rebuilds the index to match the
///    copied working tree.
///
/// Returns `Ok(true)` if CoW was used, `Ok(false)` if the caller should fall
/// back to the standard git2 worktree creation.
pub fn create_cow_worktree(source_dir: &Path, target_path: &Path, branch: &str) -> Result<bool> {
    let cow = detect_cow_capability(source_dir);

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
            branch,
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
fn cow_copy_working_tree(source: &Path, target: &Path, cow: CowCapability) -> Result<()> {
    let entries: Vec<_> = std::fs::read_dir(source)
        .context("Failed to read source directory")?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            name != ".git"
        })
        .collect();

    for entry in &entries {
        let src_path = entry.path();
        let file_name = entry.file_name();
        let dst_path = target.join(&file_name);

        // Skip if destination already exists (e.g. .git file created by worktree add)
        if dst_path.exists() {
            continue;
        }

        let result = match cow {
            CowCapability::Apfs => std::process::Command::new("cp")
                .arg("-cR")
                .arg(&src_path)
                .arg(&dst_path)
                .output(),
            CowCapability::Reflink => std::process::Command::new("cp")
                .args(["-a", "--reflink=auto"])
                .arg(&src_path)
                .arg(&dst_path)
                .output(),
            CowCapability::None => unreachable!(),
        };

        match result {
            Ok(output) if output.status.success() => {
                log::debug!("CoW copied: {}", file_name.to_string_lossy());
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                log::warn!(
                    "CoW copy failed for '{}': {}; falling back to regular copy",
                    file_name.to_string_lossy(),
                    stderr.trim()
                );
                copy_recursive(&src_path, &dst_path)?;
            }
            Err(e) => {
                log::warn!(
                    "CoW copy failed for '{}': {}; falling back to regular copy",
                    file_name.to_string_lossy(),
                    e
                );
                copy_recursive(&src_path, &dst_path)?;
            }
        }
    }

    Ok(())
}

/// Recursive file copy fallback when CoW fails for an individual entry.
fn copy_recursive(src: &Path, dst: &Path) -> Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let src_child = entry.path();
            let dst_child = dst.join(entry.file_name());
            copy_recursive(&src_child, &dst_child)?;
        }
    } else if src.is_symlink() {
        let link_target = std::fs::read_link(src)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&link_target, dst)?;
        #[cfg(windows)]
        {
            if link_target.is_dir() {
                std::os::windows::fs::symlink_dir(&link_target, dst)?;
            } else {
                std::os::windows::fs::symlink_file(&link_target, dst)?;
            }
        }
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst)?;
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
    fn test_probe_dir_name_is_unique() {
        let a = probe_dir_name();
        // Small sleep to ensure timestamp changes
        std::thread::sleep(std::time::Duration::from_millis(1));
        let b = probe_dir_name();
        assert_ne!(a, b);
    }

    #[test]
    fn test_copy_recursive_file() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.txt");
        let dst = tmp.path().join("dst.txt");
        std::fs::write(&src, b"hello").unwrap();
        copy_recursive(&src, &dst).unwrap();
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "hello");
    }

    #[test]
    fn test_copy_recursive_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let src_dir = tmp.path().join("srcdir");
        std::fs::create_dir_all(src_dir.join("sub")).unwrap();
        std::fs::write(src_dir.join("a.txt"), b"aaa").unwrap();
        std::fs::write(src_dir.join("sub").join("b.txt"), b"bbb").unwrap();

        let dst_dir = tmp.path().join("dstdir");
        copy_recursive(&src_dir, &dst_dir).unwrap();

        assert_eq!(
            std::fs::read_to_string(dst_dir.join("a.txt")).unwrap(),
            "aaa"
        );
        assert_eq!(
            std::fs::read_to_string(dst_dir.join("sub").join("b.txt")).unwrap(),
            "bbb"
        );
    }
}
