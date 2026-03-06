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
