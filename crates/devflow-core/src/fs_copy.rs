//! Native directory copies shared by workspace payloads and data providers.
use std::{fs, io, path::Path};

use anyhow::{Context, Result};
use rayon::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyMode {
    /// Use native block cloning where supported, otherwise copy bytes.
    Cow,
    Copy,
}

/// Copy directory contents, preserving child symlinks. Existing directories
/// may be merged; directory entries at the destination cannot be symlinks.
/// The target directory and its parent are never removed. Data providers
/// publish a successfully copied staging directory separately.
pub fn copy_directory(source: &Path, target: &Path, mode: CopyMode) -> Result<()> {
    let source = source.canonicalize()?;
    let absolute_target = std::path::absolute(target)?;
    let target = absolute_target.as_path();
    let mut ancestor = target.to_path_buf();
    while !ancestor.try_exists()? {
        anyhow::ensure!(ancestor.pop(), "copy target has no existing ancestor");
    }
    anyhow::ensure!(
        !ancestor.canonicalize()?.starts_with(&source),
        "cannot copy a directory into itself: {}",
        target.display()
    );
    copy_directory_inner(&source, target, mode).with_context(|| {
        format!(
            "failed to copy '{}' to '{}'",
            source.display(),
            target.display()
        )
    })
}

fn copy_directory_inner(source: &Path, target: &Path, mode: CopyMode) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    anyhow::ensure!(
        metadata.is_dir(),
        "source is not a directory: {}",
        source.display()
    );
    let exists = match fs::symlink_metadata(target) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.is_dir(),
                "destination is not a directory: {}",
                target.display()
            );
            true
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    if !exists {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        // clonefile clones an entire hierarchy in one native call on APFS.
        #[cfg(target_os = "macos")]
        if mode == CopyMode::Cow {
            match reflink_copy::reflink(source, target) {
                Ok(()) => return preserve_tree_metadata(source, target),
                Err(error) if fs::symlink_metadata(target).is_ok() => {
                    // A partial clone or concurrent creation may now occupy
                    // the path. Only the owner of a staging directory can
                    // decide to remove it; this copier must not guess.
                    return Err(error).context("native clone failed with a destination present");
                }
                Err(_) => (), // Unsupported filesystem: use ordinary copies.
            }
        }
        fs::create_dir(target)?;
    }
    let entries = fs::read_dir(source)?.collect::<io::Result<Vec<_>>>()?;
    entries.par_iter().try_for_each(|entry| -> Result<()> {
        let src = entry.path();
        let dst = target.join(entry.file_name());
        let metadata = fs::symlink_metadata(&src)?;
        if metadata.is_dir() {
            copy_directory_inner(&src, &dst, mode)
        } else {
            copy_entry(&src, &dst, &metadata, mode)
        }
    })?;
    preserve_metadata(target, &metadata)
}

// clonefile preserves contents, modes and extended attributes, but timestamps
// need restoring after the hierarchy has been created.
#[cfg(target_os = "macos")]
fn preserve_tree_metadata(source: &Path, target: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.is_symlink() {
        return preserve_symlink_times(target, &metadata);
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            preserve_tree_metadata(&entry.path(), &target.join(entry.file_name()))?;
        }
    }
    preserve_metadata(target, &metadata)
}

fn copy_entry(source: &Path, target: &Path, metadata: &fs::Metadata, mode: CopyMode) -> Result<()> {
    anyhow::ensure!(
        metadata.is_file() || metadata.is_symlink(),
        "unsupported file type in copy: {}",
        source.display()
    );
    // Rename over an existing file/link only after the replacement is ready.
    // This also avoids writing through existing hardlinks or symlinks.
    let staged = target.with_file_name(format!(".devflow-file-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        if metadata.is_symlink() {
            copy_symlink(source, &staged)?;
            preserve_symlink_times(&staged, metadata)?;
        } else {
            match mode {
                CopyMode::Cow => {
                    reflink_copy::reflink_or_copy(source, &staged)?;
                }
                CopyMode::Copy => {
                    fs::copy(source, &staged)?;
                }
            }
            preserve_metadata(&staged, metadata)?;
        }
        fs::rename(&staged, target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result.with_context(|| format!("failed to copy '{}'", source.display()))
}

fn preserve_metadata(target: &Path, metadata: &fs::Metadata) -> Result<()> {
    filetime::set_file_times(
        target,
        filetime::FileTime::from_last_access_time(metadata),
        filetime::FileTime::from_last_modification_time(metadata),
    )?;
    fs::set_permissions(target, metadata.permissions())?;
    Ok(())
}

fn preserve_symlink_times(target: &Path, metadata: &fs::Metadata) -> Result<()> {
    filetime::set_symlink_file_times(
        target,
        filetime::FileTime::from_last_access_time(metadata),
        filetime::FileTime::from_last_modification_time(metadata),
    )?;
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(source: &Path, target: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(fs::read_link(source)?, target)
}

#[cfg(windows)]
fn copy_symlink(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::fs::{symlink_dir, symlink_file, FileTypeExt};
    if fs::symlink_metadata(source)?.file_type().is_symlink_dir() {
        symlink_dir(fs::read_link(source)?, target)
    } else {
        symlink_file(fs::read_link(source)?, target)
    }
}

/// Probe actual block cloning across the supplied volumes, without a byte
/// copy fallback. Temporary paths are unique and always cleaned up.
pub fn probe_cow(source_dir: &Path, target_dir: &Path) -> Result<()> {
    let name = format!(".devflow-cow-probe-{}", uuid::Uuid::new_v4());
    let source = source_dir.join(format!("{name}-source"));
    let target = target_dir.join(format!("{name}-target"));
    let result = (|| -> Result<()> {
        fs::create_dir_all(source_dir)?;
        fs::create_dir_all(target_dir)?;
        use std::io::Write;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&source)?
            .write_all(b"devflow")?;
        reflink_copy::reflink(&source, &target)?;
        Ok(())
    })();
    let _ = fs::remove_file(source);
    let _ = fs::remove_file(target);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn copies_preserve_links_modes_times_and_independence() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        for mode in [CopyMode::Copy, CopyMode::Cow] {
            let root = tempfile::tempdir().unwrap();
            let src = root.path().join("source");
            let dst = root.path().join("destination");
            fs::create_dir_all(src.join("bin")).unwrap();
            fs::write(src.join("bin/tool"), "original").unwrap();
            fs::set_permissions(src.join("bin/tool"), fs::Permissions::from_mode(0o751)).unwrap();
            symlink("bin/tool", src.join("tool")).unwrap();
            symlink("missing", src.join("dangling")).unwrap();
            let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
            for path in [&src, &src.join("bin"), &src.join("bin/tool")] {
                fs::File::open(path).unwrap().set_modified(time).unwrap();
            }
            copy_directory(&src, &dst, mode).unwrap();
            assert_eq!(
                fs::read_link(dst.join("tool")).unwrap(),
                Path::new("bin/tool")
            );
            assert_eq!(
                fs::read_link(dst.join("dangling")).unwrap(),
                Path::new("missing")
            );
            assert_eq!(
                fs::metadata(dst.join("bin/tool"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o751
            );
            assert_eq!(fs::metadata(&dst).unwrap().modified().unwrap(), time);
            assert_eq!(
                fs::metadata(dst.join("bin/tool"))
                    .unwrap()
                    .modified()
                    .unwrap(),
                time
            );
            fs::write(dst.join("bin/tool"), "changed").unwrap();
            assert_eq!(
                fs::read_to_string(src.join("bin/tool")).unwrap(),
                "original"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn merging_never_writes_through_destination_links() {
        let root = tempfile::tempdir().unwrap();
        let src = root.path().join("source");
        let dst = root.path().join("destination");
        fs::create_dir(&src).unwrap();
        fs::create_dir(&dst).unwrap();
        fs::write(src.join("file"), "new").unwrap();
        let external = root.path().join("external");
        fs::write(&external, "keep").unwrap();
        std::os::unix::fs::symlink(&external, dst.join("file")).unwrap();
        copy_directory(&src, &dst, CopyMode::Copy).unwrap();
        assert_eq!(fs::read_to_string(&external).unwrap(), "keep");
        assert_eq!(fs::read_to_string(dst.join("file")).unwrap(), "new");
        fs::create_dir(src.join("directory")).unwrap();
        std::os::unix::fs::symlink(root.path(), dst.join("directory")).unwrap();
        assert!(copy_directory(&src, &dst, CopyMode::Copy).is_err());
    }

    #[test]
    fn refuses_copy_inside_source() {
        let root = tempfile::tempdir().unwrap();
        assert!(copy_directory(root.path(), &root.path().join("child"), CopyMode::Cow).is_err());
    }
}
