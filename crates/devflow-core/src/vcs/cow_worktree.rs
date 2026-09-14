use std::path::Path;

/// Filesystem Copy-on-Write capability
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CowCapability {
    /// macOS APFS clonefile
    Apfs,
    /// Linux FICLONE — Btrfs, XFS
    #[allow(dead_code)]
    Reflink,
    /// No CoW support detected
    None,
}

/// Probe native block cloning within a filesystem.
pub fn detect_cow_capability(path: &Path) -> CowCapability {
    detect_cow_capability_cross(path, path)
}

/// Probe native block cloning across the source and target volumes.
pub fn detect_cow_capability_cross(source_dir: &Path, target_parent: &Path) -> CowCapability {
    if crate::fs_copy::probe_cow(source_dir, target_parent).is_ok() {
        #[cfg(target_os = "macos")]
        return CowCapability::Apfs;
        #[cfg(target_os = "linux")]
        return CowCapability::Reflink;
    }
    CowCapability::None
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
}
