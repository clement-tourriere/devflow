//! Copy-on-write payloads: clone explicitly listed paths from one worktree
//! into another. APFS clones whole directory trees with one `clonefile`
//! call; Linux reflinks each file (`FICLONE`); anything else is copied.
use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{fs, io, path::Path};

/// How an entry reached the destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    /// Every file shares blocks with the source.
    Cloned,
    /// Some files were cloned, some copied.
    Mixed,
    /// Byte copies only (no CoW support between these locations).
    Copied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum EntryResult {
    Done {
        method: Method,
    },
    /// The destination already has this path; it is left untouched.
    AlreadyPresent,
    /// The source worktree does not have this path.
    Missing,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntryReport {
    pub path: String,
    #[serde(flatten)]
    pub result: EntryResult,
}

/// One line for a payload phase: what was cloned, copied, found in place or
/// missing, from `source`. "Cloned" is only said of entries whose blocks are
/// shared with the source; "copied" only of byte copies.
pub fn summary(reports: &[EntryReport], source: &Path) -> String {
    let count =
        |wanted: fn(&EntryResult) -> bool| reports.iter().filter(|r| wanted(&r.result)).count();
    let parts: Vec<String> = [
        (
            count(|r| {
                matches!(
                    r,
                    EntryResult::Done {
                        method: Method::Cloned
                    }
                )
            }),
            "cloned",
        ),
        (
            count(|r| {
                matches!(
                    r,
                    EntryResult::Done {
                        method: Method::Copied
                    }
                )
            }),
            "copied",
        ),
        (
            count(|r| {
                matches!(
                    r,
                    EntryResult::Done {
                        method: Method::Mixed
                    }
                )
            }),
            "partly cloned, partly copied",
        ),
        (
            count(|r| matches!(r, EntryResult::AlreadyPresent)),
            "already there",
        ),
        (
            count(|r| matches!(r, EntryResult::Missing)),
            "not in the source",
        ),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, what)| format!("{n} {what}"))
    .collect();
    if parts.is_empty() {
        return "nothing to copy".to_string();
    }
    format!("{} (from {})", parts.join(", "), source.display())
}

/// Clone each relative `entries` path from `source_root` into `dest_root`.
///
/// Paths must stay inside both roots: `..`, absolute paths and symlinked
/// parent directories that leave a root are refused. Each entry is staged
/// under a temporary name beside its destination and renamed into place
/// without replacing anything ([`rename_noreplace`]), so an interrupted copy
/// never leaves a partial entry under the real name and a destination that
/// appears meanwhile is never overwritten.
pub fn copy_paths(
    source_root: &Path,
    dest_root: &Path,
    entries: &[String],
) -> Result<Vec<EntryReport>> {
    let source_root = source_root.canonicalize()?;
    let dest_root = dest_root.canonicalize()?;
    entries
        .iter()
        .map(|entry| {
            let result = copy_one(&source_root, &dest_root, entry)
                .with_context(|| format!("cannot copy '{entry}'"))?;
            Ok(EntryReport {
                path: entry.clone(),
                result,
            })
        })
        .collect()
}

fn copy_one(source_root: &Path, dest_root: &Path, entry: &str) -> Result<EntryResult> {
    crate::config::validate_relative(entry)?;
    let relative = Path::new(entry);
    let source = source_root.join(relative);
    let Ok(metadata) = fs::symlink_metadata(&source) else {
        return Ok(EntryResult::Missing);
    };
    let source_parent = source.parent().context("entry has no parent")?;
    anyhow::ensure!(
        source_parent.canonicalize()?.starts_with(source_root),
        "source path leaves the source worktree through a symlink"
    );
    let dest = dest_root.join(relative);
    if fs::symlink_metadata(&dest).is_ok() {
        return Ok(EntryResult::AlreadyPresent);
    }
    let dest_parent = dest.parent().context("entry has no parent")?;
    create_dirs_inside(dest_root, dest_parent)?;
    let name = dest.file_name().context("entry has no file name")?;
    let staged = dest_parent.join(format!(
        ".devflow-stage-{}-{}",
        uuid::Uuid::new_v4(),
        name.to_string_lossy()
    ));
    let result = (|| -> Result<Option<Method>> {
        let stats = Stats::default();
        if metadata.is_dir() {
            copy_tree(&source, &staged, &stats)?;
        } else {
            copy_entry(&source, &staged, &metadata, &stats)?;
        }
        Ok(match rename_noreplace(&staged, &dest) {
            Ok(()) => Some(stats.method()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => None,
            Err(e) => return Err(e.into()),
        })
    })();
    match result {
        Ok(Some(method)) => Ok(EntryResult::Done { method }),
        Ok(None) => {
            let _ = remove_any(&staged);
            Ok(EntryResult::AlreadyPresent)
        }
        Err(error) => {
            let _ = remove_any(&staged);
            if fs::symlink_metadata(&dest).is_ok() {
                return Ok(EntryResult::AlreadyPresent);
            }
            Err(error)
        }
    }
}

/// Move `from` to `to`, failing with `AlreadyExists` instead of replacing
/// whatever is at `to` (`rename(2)` silently replaces a file or an empty
/// directory). The check and the rename are one atomic step where the kernel
/// offers it (`renamex_np` with `RENAME_EXCL` on macOS, `renameat2` with
/// `RENAME_NOREPLACE` on Linux). Where the file system or kernel does not,
/// a file is installed with `link(2)`, which fails with `EEXIST` rather than
/// replace, and anything else is an `Unsupported` error: never a plain rename
/// that a concurrent writer to `to` could lose a file or an empty directory
/// to. (Platforms other than macOS and Linux have only the check and the
/// rename, which is not atomic.)
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = |path: &Path| {
            std::ffi::CString::new(path.as_os_str().as_bytes())
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
        };
        let (from_c, to_c) = (c(from)?, c(to)?);
        #[cfg(target_os = "macos")]
        // SAFETY: both arguments are NUL-terminated paths that outlive the call.
        let result = unsafe { libc::renamex_np(from_c.as_ptr(), to_c.as_ptr(), libc::RENAME_EXCL) };
        #[cfg(target_os = "linux")]
        // SAFETY: both arguments are NUL-terminated paths that outlive the call.
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                from_c.as_ptr(),
                libc::AT_FDCWD,
                to_c.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            if result == 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            // The flag is not offered: by the kernel (ENOSYS, EINVAL), or by
            // this file system (ENOTSUP).
            let unsupported = matches!(
                error.raw_os_error(),
                Some(libc::EINVAL | libc::ENOSYS | libc::ENOTSUP)
            );
            if unsupported {
                install_without_flag(from, to)
            } else {
                Err(error)
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let _ = (from_c, to_c);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        if fs::symlink_metadata(to).is_ok() {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        fs::rename(from, to)
    }
}

/// Install `from` at `to` without `rename`'s flag: a regular file is linked
/// into place (`EEXIST` if `to` appeared) and the staged name dropped; a
/// directory or a symbolic link cannot be installed without the risk of
/// replacing something, so it is not installed.
#[cfg(any(unix, test))]
fn install_without_flag(from: &Path, to: &Path) -> io::Result<()> {
    if !fs::symlink_metadata(from)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "this file system cannot install {} without the risk of replacing something \
                 that appears at {} meanwhile (it has no atomic no-replace rename), so it was \
                 not installed",
                from.display(),
                to.display()
            ),
        ));
    }
    match fs::hard_link(from, to) {
        Ok(()) => fs::remove_file(from),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
        Err(e) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "this file system has neither an atomic no-replace rename nor hard links, so \
                 {} was not installed ({e})",
                to.display()
            ),
        )),
    }
}

/// Create `dir` (inside `root`) without following symlinks out of `root`.
fn create_dirs_inside(root: &Path, dir: &Path) -> Result<()> {
    let relative = dir
        .strip_prefix(root)
        .context("path is outside the worktree")?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => anyhow::bail!(
                "{} is not a directory (symlinked parents are not followed)",
                current.display()
            ),
            Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(&current)?,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn remove_any(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[derive(Default)]
struct Stats {
    cloned: AtomicU64,
    copied: AtomicU64,
}

impl Stats {
    fn method(&self) -> Method {
        match (
            self.cloned.load(Ordering::Relaxed),
            self.copied.load(Ordering::Relaxed),
        ) {
            (_, 0) => Method::Cloned,
            (0, _) => Method::Copied,
            _ => Method::Mixed,
        }
    }
}

/// Copy a directory tree to a path that does not exist yet.
fn copy_tree(source: &Path, target: &Path, stats: &Stats) -> Result<()> {
    // clonefile clones an entire hierarchy in one native call on APFS.
    #[cfg(target_os = "macos")]
    {
        match reflink_copy::reflink(source, target) {
            Ok(()) => {
                restore_directory_times(source, target)?;
                stats.cloned.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
            Err(error) if fs::symlink_metadata(target).is_ok() => {
                return Err(error).context("native clone failed with a destination present");
            }
            Err(_) => {} // Not APFS (or across volumes): copy entry by entry.
        }
    }
    let metadata = fs::symlink_metadata(source)?;
    fs::create_dir(target)?;
    let entries = fs::read_dir(source)?.collect::<io::Result<Vec<_>>>()?;
    entries.par_iter().try_for_each(|entry| -> Result<()> {
        let src = entry.path();
        let dst = target.join(entry.file_name());
        let metadata = fs::symlink_metadata(&src)?;
        if metadata.is_dir() {
            copy_tree(&src, &dst, stats)
        } else {
            copy_entry(&src, &dst, &metadata, stats)
        }
    })?;
    preserve_metadata(target, &metadata)
}

fn copy_entry(source: &Path, target: &Path, metadata: &fs::Metadata, stats: &Stats) -> Result<()> {
    if metadata.is_symlink() {
        copy_symlink(source, target)?;
        return preserve_symlink_times(target, metadata);
    }
    anyhow::ensure!(
        metadata.is_file(),
        "unsupported file type: {}",
        source.display()
    );
    match reflink_copy::reflink_or_copy(source, target)? {
        None => stats.cloned.fetch_add(1, Ordering::Relaxed),
        Some(_) => stats.copied.fetch_add(1, Ordering::Relaxed),
    };
    preserve_metadata(target, metadata)
}

// clonefile preserves contents, modes, extended attributes and the times of
// files and symbolic links (measured on APFS, see the `clone_keeps_times`
// test), but every directory it creates is stamped with the time of the call.
// Only directories need their times back, and setting one does not touch its
// parent's, so the tree is walked in parallel and in any order, reading only
// the directories (a file's type comes with its directory entry).
#[cfg(target_os = "macos")]
fn restore_directory_times(source: &Path, target: &Path) -> Result<()> {
    let children = fs::read_dir(source)?
        .filter_map(|entry| match entry {
            Ok(entry) => match entry.file_type() {
                Ok(kind) if kind.is_dir() => Some(Ok(entry.file_name())),
                Ok(_) => None,
                Err(e) => Some(Err(e)),
            },
            Err(e) => Some(Err(e)),
        })
        .collect::<io::Result<Vec<_>>>()?;
    children
        .par_iter()
        .try_for_each(|name| restore_directory_times(&source.join(name), &target.join(name)))?;
    let metadata = fs::symlink_metadata(source)?;
    filetime::set_file_times(
        target,
        filetime::FileTime::from_last_access_time(&metadata),
        filetime::FileTime::from_last_modification_time(&metadata),
    )?;
    Ok(())
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
    use std::os::windows::fs::{FileTypeExt, symlink_dir, symlink_file};
    if fs::symlink_metadata(source)?.file_type().is_symlink_dir() {
        symlink_dir(fs::read_link(source)?, target)
    } else {
        symlink_file(fs::read_link(source)?, target)
    }
}

/// What a path holds, as the file system reports it.
///
/// Neither number is "the disk this path costs": a file that was cloned
/// shares blocks with its source, and a file system does not say which. So
/// the sizes are an upper bound for a cloned path (what it would take if
/// every block were its own), and callers must say so; the cost of a cloned
/// tree is never zero, since whatever is changed in it is written anew.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Measure {
    /// The sum of the files' lengths: what a plain copy would hold.
    pub apparent_bytes: u64,
    /// The blocks the files occupy as the file system reports them
    /// (`st_blocks`), shared blocks counted in full. `None` where the
    /// platform does not report it.
    pub allocated_bytes: Option<u64>,
    /// Files, links and directories visited.
    pub entries: u64,
    /// Some entries could not be read; the numbers are lower than the truth.
    pub partial: bool,
}

/// Measure `path` (a file or a tree) without following symbolic links.
pub fn measure(path: &Path) -> Result<Measure> {
    let mut total = Measure::default();
    let mut allocated: Option<u64> = Some(0);
    let mut pending = vec![path.to_path_buf()];
    let root =
        fs::symlink_metadata(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut first = Some(root);
    while let Some(next) = pending.pop() {
        let metadata = match first.take() {
            Some(metadata) => metadata,
            None => match fs::symlink_metadata(&next) {
                Ok(metadata) => metadata,
                Err(_) => {
                    total.partial = true;
                    continue;
                }
            },
        };
        total.entries += 1;
        if metadata.is_dir() {
            match fs::read_dir(&next) {
                Ok(children) => {
                    for child in children {
                        match child {
                            Ok(child) => pending.push(child.path()),
                            Err(_) => total.partial = true,
                        }
                    }
                }
                Err(_) => total.partial = true,
            }
            continue;
        }
        total.apparent_bytes += metadata.len();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if let Some(sum) = allocated.as_mut() {
                *sum += metadata.blocks() * 512;
            }
        }
        #[cfg(not(unix))]
        {
            allocated = None;
        }
    }
    total.allocated_bytes = allocated;
    Ok(total)
}

/// Whether files in `dir` can be block-cloned (probe file created and removed).
pub fn probe(dir: &Path) -> bool {
    let name = format!(".devflow-cow-probe-{}", uuid::Uuid::new_v4());
    let source = dir.join(format!("{name}-a"));
    let target = dir.join(format!("{name}-b"));
    let ok =
        fs::write(&source, b"devflow").is_ok() && reflink_copy::reflink(&source, &target).is_ok();
    let _ = fs::remove_file(source);
    let _ = fs::remove_file(target);
    ok
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn entries(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn copies_trees_files_and_links_independently() {
        let root = tempfile::tempdir().unwrap();
        let (src, dst) = (root.path().join("src"), root.path().join("dst"));
        fs::create_dir_all(src.join("node_modules/pkg/bin")).unwrap();
        fs::create_dir(&dst).unwrap();
        fs::write(src.join("node_modules/pkg/bin/tool"), "original").unwrap();
        fs::set_permissions(
            src.join("node_modules/pkg/bin/tool"),
            fs::Permissions::from_mode(0o751),
        )
        .unwrap();
        symlink("pkg/bin/tool", src.join("node_modules/tool")).unwrap();
        fs::write(src.join(".env"), "A=1").unwrap();

        let report =
            copy_paths(&src, &dst, &entries(&["node_modules", ".env", "missing"])).unwrap();
        assert!(matches!(report[0].result, EntryResult::Done { .. }));
        assert!(matches!(report[1].result, EntryResult::Done { .. }));
        assert_eq!(report[2].result, EntryResult::Missing);
        assert_eq!(
            fs::read_link(dst.join("node_modules/tool")).unwrap(),
            Path::new("pkg/bin/tool")
        );
        let tool = dst.join("node_modules/pkg/bin/tool");
        assert_eq!(
            fs::metadata(&tool).unwrap().permissions().mode() & 0o777,
            0o751
        );
        fs::write(&tool, "changed").unwrap();
        assert_eq!(
            fs::read_to_string(src.join("node_modules/pkg/bin/tool")).unwrap(),
            "original"
        );
        // Second run leaves existing entries alone.
        fs::write(dst.join(".env"), "A=2").unwrap();
        let report = copy_paths(&src, &dst, &entries(&[".env"])).unwrap();
        assert_eq!(report[0].result, EntryResult::AlreadyPresent);
        assert_eq!(fs::read_to_string(dst.join(".env")).unwrap(), "A=2");
        // No staging leftovers.
        assert!(fs::read_dir(&dst).unwrap().all(|e| {
            !e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".devflow-stage")
        }));
    }

    #[test]
    fn measure_adds_up_a_tree_without_following_links() {
        let root = tempfile::tempdir().unwrap();
        let tree = root.path().join("tree");
        fs::create_dir_all(tree.join("a/b")).unwrap();
        fs::write(tree.join("a/one"), vec![0u8; 1000]).unwrap();
        fs::write(tree.join("a/b/two"), vec![1u8; 24]).unwrap();
        // A link back up the tree would loop forever if it were followed.
        symlink(&tree, tree.join("a/loop")).unwrap();
        let measured = measure(&tree).unwrap();
        assert!(measured.apparent_bytes >= 1024, "{measured:?}");
        assert_eq!(measured.entries, 6, "tree, a, b, one, two, loop");
        assert!(!measured.partial);
        assert!(measured.allocated_bytes.is_some_and(|b| b >= 1000));
        // A single file, and a path that is not there.
        let one = measure(&tree.join("a/one")).unwrap();
        assert_eq!((one.apparent_bytes, one.entries), (1000, 1));
        assert!(measure(&tree.join("nope")).is_err());
    }

    #[test]
    fn refuses_paths_that_escape_either_worktree() {
        let root = tempfile::tempdir().unwrap();
        let (src, dst, outside) = (
            root.path().join("src"),
            root.path().join("dst"),
            root.path().join("outside"),
        );
        for dir in [&src, &dst, &outside] {
            fs::create_dir(dir).unwrap();
        }
        fs::write(outside.join("secret"), "s").unwrap();
        symlink(&outside, src.join("linked")).unwrap();
        assert!(copy_paths(&src, &dst, &entries(&["linked/secret"])).is_err());
        assert!(copy_paths(&src, &dst, &entries(&["../outside/secret"])).is_err());

        fs::create_dir(src.join("conf")).unwrap();
        fs::write(src.join("conf/app"), "x").unwrap();
        symlink(&outside, dst.join("conf")).unwrap();
        assert!(copy_paths(&src, &dst, &entries(&["conf/app"])).is_err());
        assert!(!outside.join("app").exists());
    }

    fn mtime(path: &Path) -> filetime::FileTime {
        filetime::FileTime::from_last_modification_time(&fs::symlink_metadata(path).unwrap())
    }

    fn walk(root: &Path, out: &mut Vec<std::path::PathBuf>) {
        out.push(root.to_path_buf());
        if fs::symlink_metadata(root).unwrap().is_dir() {
            for entry in fs::read_dir(root).unwrap() {
                walk(&entry.unwrap().path(), out);
            }
        }
    }

    #[test]
    fn a_cloned_tree_keeps_the_times_and_modes_of_the_source() {
        let root = tempfile::tempdir().unwrap();
        let (src, dst) = (root.path().join("src"), root.path().join("dst"));
        fs::create_dir(&dst).unwrap();
        let tree = src.join("node_modules");
        fs::create_dir_all(tree.join("a/deep/er")).unwrap();
        fs::create_dir_all(tree.join("b")).unwrap();
        fs::write(tree.join("a/deep/er/file.js"), "x").unwrap();
        fs::write(tree.join("a/run.sh"), "#!/bin/sh").unwrap();
        fs::set_permissions(tree.join("a/run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(tree.join("b"), fs::Permissions::from_mode(0o700)).unwrap();
        symlink("deep/er/file.js", tree.join("a/link")).unwrap();
        symlink("missing", tree.join("b/dangling")).unwrap();
        // Distinct old times everywhere; directories last, since filling a
        // directory changes its time.
        let mut all = Vec::new();
        walk(&tree, &mut all);
        for (n, path) in all.iter().rev().enumerate() {
            let time = filetime::FileTime::from_unix_time(1_500_000_000 + 1000 * n as i64, 0);
            if fs::symlink_metadata(path).unwrap().is_symlink() {
                filetime::set_symlink_file_times(path, time, time).unwrap();
            } else {
                filetime::set_file_times(path, time, time).unwrap();
            }
        }

        copy_paths(&src, &dst, &entries(&["node_modules"])).unwrap();

        let mut cloned = Vec::new();
        walk(&dst.join("node_modules"), &mut cloned);
        assert_eq!(all.len(), cloned.len());
        for path in &all {
            let twin = dst.join(path.strip_prefix(&src).unwrap());
            let (a, b) = (
                fs::symlink_metadata(path).unwrap(),
                fs::symlink_metadata(&twin).unwrap(),
            );
            assert_eq!(mtime(path), mtime(&twin), "mtime of {}", twin.display());
            assert_eq!(a.file_type(), b.file_type(), "{}", twin.display());
            if !a.is_symlink() {
                assert_eq!(
                    a.permissions().mode() & 0o7777,
                    b.permissions().mode() & 0o7777,
                    "mode of {}",
                    twin.display()
                );
            }
        }
    }

    #[test]
    fn an_existing_destination_is_never_replaced() {
        let root = tempfile::tempdir().unwrap();
        let (a, b) = (root.path().join("a"), root.path().join("b"));
        // A file, then an empty directory: rename(2) would replace both.
        fs::write(&a, "new").unwrap();
        fs::write(&b, "mine").unwrap();
        let error = rename_noreplace(&a, &b).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&b).unwrap(), "mine");
        assert_eq!(fs::read_to_string(&a).unwrap(), "new");
        fs::remove_file(&b).unwrap();
        fs::create_dir(&b).unwrap();
        fs::create_dir(root.path().join("c")).unwrap();
        let error = rename_noreplace(&root.path().join("c"), &b).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(root.path().join("c").is_dir());
        // A free destination works, and a missing source is an ordinary error.
        rename_noreplace(&a, &root.path().join("free")).unwrap();
        assert!(!a.exists());
        let error = rename_noreplace(&a, &root.path().join("free2")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn without_the_rename_flag_a_file_is_linked_and_nothing_else_is_installed() {
        let root = tempfile::tempdir().unwrap();
        let p = |n: &str| root.path().join(n);
        // A file: installed, the staged name gone.
        fs::write(p("stage"), "new").unwrap();
        install_without_flag(&p("stage"), &p("dest")).unwrap();
        assert_eq!(fs::read_to_string(p("dest")).unwrap(), "new");
        assert!(!p("stage").exists());
        // A file that appeared meanwhile is never replaced.
        fs::write(p("stage2"), "new").unwrap();
        fs::write(p("taken"), "mine").unwrap();
        let error = install_without_flag(&p("stage2"), &p("taken")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(p("taken")).unwrap(), "mine");
        // A directory (empty destination directory included) or a link is
        // reported as not installed, and nothing moves.
        fs::create_dir(p("tree")).unwrap();
        fs::create_dir(p("empty")).unwrap();
        let error = install_without_flag(&p("tree"), &p("empty")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(error.to_string().contains("not installed"), "{error}");
        assert!(p("tree").is_dir() && p("empty").is_dir());
        symlink("dest", p("link")).unwrap();
        let error = install_without_flag(&p("link"), &p("linked")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(p("link").symlink_metadata().is_ok() && !p("linked").exists());
    }

    #[test]
    fn a_destination_that_appears_during_the_copy_is_reported_and_kept() {
        let root = tempfile::tempdir().unwrap();
        let (src, dst) = (root.path().join("src"), root.path().join("dst"));
        fs::create_dir(&dst).unwrap();
        // The staging name is unknown, so another thread makes the destination
        // appear while the copy runs, many times over.
        for round in 0..30 {
            let entry = format!("data{round}");
            fs::create_dir_all(src.join(&entry)).unwrap();
            fs::write(src.join(&entry).join("x"), "x").unwrap();
            let stop = std::sync::atomic::AtomicBool::new(false);
            let list = entries(&[&entry]);
            let report = std::thread::scope(|scope| {
                scope.spawn(|| {
                    while !stop.load(Ordering::Relaxed) {
                        if fs::create_dir(dst.join(&entry)).is_ok() {
                            break;
                        }
                    }
                });
                let report = copy_paths(&src, &dst, &list).unwrap();
                stop.store(true, Ordering::Relaxed);
                report
            });
            // Whoever won, the destination is whole, and a copy that reports
            // itself done is the one that is there.
            let installed = dst.join(&entry).join("x").is_file();
            match report[0].result {
                EntryResult::Done { .. } => assert!(installed, "round {round}"),
                EntryResult::AlreadyPresent => assert!(!installed, "round {round}"),
                ref other => panic!("{other:?}"),
            }
            assert!(fs::read_dir(&dst).unwrap().all(|e| {
                !e.unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".devflow-stage")
            }));
        }
    }

    #[test]
    fn the_summary_says_cloned_only_for_shared_blocks() {
        let report = |result| EntryReport {
            path: "p".into(),
            result,
        };
        let done = |method| report(EntryResult::Done { method });
        let from = Path::new("/w/shop");
        assert_eq!(
            summary(&[done(Method::Cloned), done(Method::Cloned)], from),
            "2 cloned (from /w/shop)"
        );
        assert_eq!(
            summary(&[done(Method::Copied)], from),
            "1 copied (from /w/shop)"
        );
        assert_eq!(
            summary(
                &[
                    done(Method::Cloned),
                    done(Method::Mixed),
                    report(EntryResult::AlreadyPresent),
                    report(EntryResult::Missing),
                ],
                from
            ),
            "1 cloned, 1 partly cloned, partly copied, 1 already there, 1 not in the source (from /w/shop)"
        );
        assert_eq!(summary(&[], from), "nothing to copy");
    }

    /// Measurement, not a check: `cargo test -p devflow-core --release cow::tests::timing
    /// -- --ignored --nocapture` clones a 20,000-file tree of 300 MB.
    #[test]
    #[ignore = "timing"]
    fn timing() {
        let root = tempfile::tempdir().unwrap();
        let (src, dst) = (root.path().join("src"), root.path().join("dst"));
        fs::create_dir(&dst).unwrap();
        let block = vec![7u8; 15 * 1024];
        for pkg in 0..200 {
            let lib = src.join(format!("node_modules/pkg{pkg}/lib"));
            fs::create_dir_all(&lib).unwrap();
            for file in 0..100 {
                fs::write(lib.join(format!("f{file}.js")), &block).unwrap();
            }
        }
        let started = std::time::Instant::now();
        let report = copy_paths(&src, &dst, &entries(&["node_modules"])).unwrap();
        eprintln!(
            "payload phase: {:?} {:?}",
            started.elapsed(),
            report[0].result
        );
        let started = std::time::Instant::now();
        let _ = reflink_copy::reflink(src.join("node_modules"), dst.join("bare"));
        eprintln!("bare clonefile: {:?}", started.elapsed());
    }
}
