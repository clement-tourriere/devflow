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

/// Clone each relative `entries` path from `source_root` into `dest_root`.
///
/// Paths must stay inside both roots: `..`, absolute paths and symlinked
/// parent directories that leave a root are refused. Each entry is staged
/// under a temporary name beside its destination and renamed into place, so
/// an interrupted copy never leaves a partial entry under the real name.
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
    let result = (|| -> Result<Method> {
        let stats = Stats::default();
        if metadata.is_dir() {
            copy_tree(&source, &staged, &stats)?;
        } else {
            copy_entry(&source, &staged, &metadata, &stats)?;
        }
        // rename(2) replaces an existing empty directory or file; check
        // again so a concurrently created destination is never overwritten.
        anyhow::ensure!(
            fs::symlink_metadata(&dest).is_err(),
            "destination appeared during the copy"
        );
        fs::rename(&staged, &dest)?;
        Ok(stats.method())
    })();
    match result {
        Ok(method) => Ok(EntryResult::Done { method }),
        Err(error) => {
            let _ = remove_any(&staged);
            if fs::symlink_metadata(&dest).is_ok() {
                return Ok(EntryResult::AlreadyPresent);
            }
            Err(error)
        }
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
                preserve_tree_times(source, target)?;
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

// clonefile preserves contents, modes and extended attributes, but timestamps
// need restoring after the hierarchy has been created.
#[cfg(target_os = "macos")]
fn preserve_tree_times(source: &Path, target: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.is_symlink() {
        return preserve_symlink_times(target, &metadata);
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            preserve_tree_times(&entry.path(), &target.join(entry.file_name()))?;
        }
    }
    preserve_metadata(target, &metadata)
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
}
