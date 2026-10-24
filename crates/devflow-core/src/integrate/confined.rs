//! Reading and writing a host's file confined to the checkout.
//!
//! A host file lives under a directory somebody else can replace between the
//! moment devflow plans an edit and the moment it writes (a confirmation prompt
//! can stay open for minutes), and the file itself can be saved by an editor in
//! between. So:
//!
//! - **The checkout is a capability.** [`Checkout::open`] opens the directory
//!   once, when the edit is planned, and the plan keeps the descriptor. A write
//!   refuses when the checkout's path no longer opens that same directory (it
//!   was moved, or something was put at its place), and writes into the
//!   directory it holds either way, never into whatever the path names now.
//! - **Nothing resolves a path.** Every directory on the way is opened from its
//!   parent (cap-std, relative to the parent's descriptor), looked at with
//!   `lstat` before and after the open and confirmed to be the very object that
//!   was opened. A symbolic link anywhere on the way, the file itself included,
//!   is refused.
//! - **Files are opened without blocking and without following links**
//!   (`O_NONBLOCK | O_NOFOLLOW`) and checked with `fstat` on the opened
//!   descriptor (a regular file, the object `lstat` showed), so a FIFO put in
//!   place of the file cannot stall the write.
//! - **A save that lands meanwhile is not lost.** The new content is staged in
//!   the verified directory and exchanged with the file atomically
//!   (`renameat2(RENAME_EXCHANGE)` on Linux, `renameatx_np(RENAME_SWAP)` on
//!   macOS); what was displaced is compared with what the plan read, and when
//!   it differs the files are exchanged back and a conflict is reported. Where
//!   there is no exchange the old content is hard-linked to a backup first and
//!   the backup is compared afterwards; a difference keeps the backup as
//!   `<name>.devflow-conflict-<time>`. A new file is installed with a hard link
//!   that fails when the name was taken.
use crate::remedy::{Problem, Remedy};
use anyhow::{Context, Result};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, Metadata, OpenOptions};
#[cfg(unix)]
use cap_std::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Largest host file read.
pub(super) const MAX_FILE_BYTES: u64 = 1024 * 1024;

fn link_refused(path: &Path) -> anyhow::Error {
    Problem::new(format!(
        "{} is a symbolic link; devflow does not read or write through it",
        path.display()
    ))
    .with(Remedy::OpenFile {
        path: path.to_path_buf(),
    })
    .into_error()
}

fn changed(path: &Path) -> anyhow::Error {
    Problem::new(format!(
        "{} changed while devflow was preparing the edit; nothing was written",
        path.display()
    ))
    .with(Remedy::Retry)
    .into_error()
}

/// The file was edited meanwhile and devflow's version was not installed; the
/// edit is where it was, or at `kept`.
fn conflict(path: &Path, kept: Option<&Path>) -> anyhow::Error {
    match kept {
        None => changed(path),
        Some(kept) => Problem::new(format!(
            "{} was edited while devflow was writing it; the edit is kept at {} and the file holds devflow's version",
            path.display(),
            kept.display()
        ))
        .with(Remedy::OpenFile {
            path: kept.to_path_buf(),
        })
        .into_error(),
    }
}

#[cfg(unix)]
fn same(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(not(unix))]
fn same(_: &Metadata, _: &Metadata) -> bool {
    true
}

/// The checkout, held open from the plan to the write.
#[derive(Clone)]
pub(super) struct Checkout {
    dir: Arc<Dir>,
    root: PathBuf,
}

impl std::fmt::Debug for Checkout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Checkout({})", self.root.display())
    }
}

impl Checkout {
    pub(super) fn open(root: &Path) -> Result<Checkout> {
        let dir = Dir::open_ambient_dir(root, ambient_authority())
            .with_context(|| format!("cannot open {}", root.display()))?;
        Ok(Checkout {
            dir: Arc::new(dir),
            root: root.to_path_buf(),
        })
    }

    /// The path still opens the directory that was planned against.
    fn ensure_current(&self) -> Result<()> {
        let moved = || {
            Problem::new(format!(
                "{} is not the directory the edit was planned in any more (it was moved or replaced); nothing was written",
                self.root.display()
            ))
            .with(Remedy::Retry)
            .into_error()
        };
        let now = Dir::open_ambient_dir(&self.root, ambient_authority()).map_err(|_| moved())?;
        let (now, held) = (
            now.dir_metadata().map_err(|_| moved())?,
            self.dir.dir_metadata().map_err(|_| moved())?,
        );
        if same(&now, &held) {
            Ok(())
        } else {
            Err(moved())
        }
    }

    /// The directory `parents` leads to, every component opened from its
    /// parent and confirmed. `None` when one does not exist and `create` is
    /// off.
    fn walk(&self, parents: &[&str], create: bool) -> Result<Option<Dir>> {
        let mut current = self
            .dir
            .try_clone()
            .context("cannot keep the checkout open")?;
        let mut shown = self.root.clone();
        for name in parents {
            shown.push(name);
            let entry = match current.symlink_metadata(name) {
                Ok(entry) => entry,
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    if !create {
                        return Ok(None);
                    }
                    match current.create_dir(name) {
                        Ok(()) => {}
                        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                        Err(error) => {
                            return Err(error)
                                .with_context(|| format!("cannot create {}", shown.display()));
                        }
                    }
                    current
                        .symlink_metadata(name)
                        .with_context(|| format!("cannot look at {}", shown.display()))?
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("cannot look at {}", shown.display()));
                }
            };
            if entry.file_type().is_symlink() {
                return Err(link_refused(&shown));
            }
            anyhow::ensure!(entry.is_dir(), "{} is not a directory", shown.display());
            let child = current
                .open_dir(name)
                .with_context(|| format!("cannot open {}", shown.display()))?;
            let opened = child
                .dir_metadata()
                .with_context(|| format!("cannot look at {}", shown.display()))?;
            // Looked at again after the open: still the object that was
            // opened, and still not a link.
            let now = current
                .symlink_metadata(name)
                .map_err(|_| changed(&shown))?;
            if now.file_type().is_symlink() || !same(&now, &opened) {
                return Err(changed(&shown));
            }
            current = child;
        }
        Ok(Some(current))
    }
}

fn split(relative: &str) -> (Vec<&str>, &str) {
    let mut parts: Vec<&str> = relative.split('/').collect();
    let name = parts.pop().unwrap_or(relative);
    (parts, name)
}

/// Open `name` in `dir` for reading without blocking and without following a
/// link, and read at most [`MAX_FILE_BYTES`] of it. The opened descriptor must
/// be a regular file and the object `lstat` showed for the name: a FIFO or a
/// device put in its place, or a link, is refused instead of opened for good.
fn read_regular(dir: &Dir, name: &str, shown: &Path, seen: &Metadata) -> Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    let mut file = match dir.open_with(name, &options) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Err(changed(shown)),
        // A link (`O_NOFOLLOW`) or a type that cannot be opened this way.
        Err(_) => return Err(changed(shown)),
    };
    let opened = file
        .metadata()
        .with_context(|| format!("cannot read {}", shown.display()))?;
    if !opened.is_file() || !same(seen, &opened) {
        return Err(changed(shown));
    }
    anyhow::ensure!(
        opened.len() <= MAX_FILE_BYTES,
        "{} is larger than {} KiB; devflow does not edit it",
        shown.display(),
        MAX_FILE_BYTES / 1024
    );
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read {}", shown.display()))?;
    Ok(bytes)
}

/// The text of `relative` under the checkout; `None` when it does not exist.
pub(super) fn read(checkout: &Checkout, relative: &str) -> Result<Option<String>> {
    let shown = checkout.root.join(relative);
    let (parents, name) = split(relative);
    let Some(dir) = checkout.walk(&parents, false)? else {
        return Ok(None);
    };
    let entry = match dir.symlink_metadata(name) {
        Ok(entry) => entry,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot read {}", shown.display()));
        }
    };
    if entry.file_type().is_symlink() {
        return Err(link_refused(&shown));
    }
    anyhow::ensure!(entry.is_file(), "{} is not a regular file", shown.display());
    let bytes = read_regular(&dir, name, &shown, &entry)?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| super::unreadable(&shown, "it is not UTF-8 text"))
}

/// Moments a test acts at, as a concurrent writer would, and a way to behave as
/// a system without an atomic exchange.
#[derive(Default)]
pub(super) struct Seams<'a> {
    /// After the destination was looked at with `lstat` and before it is
    /// opened to be compared with what the plan read.
    pub before_read: Option<&'a dyn Fn()>,
    /// After the destination was checked and the new content staged, before
    /// anything is exchanged or renamed.
    pub after_staging: Option<&'a dyn Fn()>,
    /// Right after the exchange: devflow's content is at the destination and
    /// what was displaced is at the staged name.
    pub after_exchange: Option<&'a dyn Fn()>,
    /// In the fallback, right after the destination was linked to its backup.
    pub after_backup: Option<&'a dyn Fn()>,
    pub no_exchange: bool,
    /// The time stamp of a conflict file's name (default: now).
    pub stamp: Option<&'a str>,
}

/// Write `content` as `relative` in the checkout. `expected` is the SHA-256 of
/// the file the plan read (`None`: the plan found no file); the write is
/// refused, and a save that landed meanwhile kept, when the file is not that
/// any more.
pub(super) fn write(
    checkout: &Checkout,
    relative: &str,
    expected: Option<&str>,
    content: &[u8],
) -> Result<()> {
    write_with(checkout, relative, expected, content, &Seams::default())
}

pub(super) fn write_with(
    checkout: &Checkout,
    relative: &str,
    expected: Option<&str>,
    content: &[u8],
    seams: &Seams<'_>,
) -> Result<()> {
    checkout.ensure_current()?;
    let shown = checkout.root.join(relative);
    let (parents, name) = split(relative);
    let dir = checkout
        .walk(&parents, true)?
        .context("the directory could not be created")?;
    // What is there now.
    let current = match dir.symlink_metadata(name) {
        Ok(entry) => Some(entry),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("cannot look at {}", shown.display()));
        }
    };
    if let Some(entry) = &current {
        if entry.file_type().is_symlink() {
            return Err(link_refused(&shown));
        }
        anyhow::ensure!(entry.is_file(), "{} is not a regular file", shown.display());
    }
    match (expected, &current) {
        (None, None) => {}
        (Some(expected), Some(entry)) => {
            if let Some(seam) = seams.before_read {
                seam();
            }
            let bytes = read_regular(&dir, name, &shown, entry)?;
            if crate::fsutil::sha256_hex(&bytes) != expected {
                return Err(changed(&shown));
            }
        }
        _ => return Err(changed(&shown)),
    }
    // Stage beside the destination, in the verified directory.
    let staged = format!(".{name}.{}.tmp", uuid::Uuid::new_v4());
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(
        current
            .as_ref()
            .map_or(0o644, |entry| entry.permissions().mode() & 0o777),
    );
    // Set when the staged file holds somebody's content that could not be
    // moved to a name of its own: it is then the only place that content is,
    // and cleanup must not delete it.
    let leave_staged = std::cell::Cell::new(false);
    let result = (|| -> Result<()> {
        let mut file = dir
            .open_with(&staged, &options)
            .with_context(|| format!("cannot write beside {}", shown.display()))?;
        file.write_all(content)?;
        file.sync_all()?;
        drop(file);
        if let Some(seam) = seams.after_staging {
            seam();
        }
        match (current.as_ref(), expected) {
            (None, _) => install_new(&dir, &staged, name, &shown),
            (Some(_), Some(expected)) => replace(
                &dir,
                &staged,
                name,
                &shown,
                (expected, &crate::fsutil::sha256_hex(content)),
                seams,
                &leave_staged,
            ),
            (Some(_), None) => Err(changed(&shown)),
        }
    })();
    // (Whatever of the staged file is left, unless it is all that is left of
    // an edit.)
    if !leave_staged.get() {
        let _ = dir.remove_file(&staged);
    }
    result
}

/// A new file: a hard link, which fails when the name was taken meanwhile.
fn install_new(dir: &Dir, staged: &str, name: &str, shown: &Path) -> Result<()> {
    match dir.hard_link(staged, dir, name) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Err(changed(shown)),
        // No hard links on this filesystem: look, then rename. A name that
        // appears in between is the narrow window left.
        Err(_) if dir.symlink_metadata(name).is_err() => dir
            .rename(staged, dir, name)
            .with_context(|| format!("cannot write {}", shown.display())),
        Err(_) => Err(changed(shown)),
    }
}

enum Exchange {
    /// One of the two names does not exist.
    Missing,
    /// No atomic exchange here (platform, filesystem or kernel).
    Unsupported,
    Failed(std::io::Error),
}

/// Atomically swap the entries `a` and `b` of `dir`.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn exchange(dir: &Dir, a: &str, b: &str) -> Result<(), Exchange> {
    use std::os::fd::{AsFd, AsRawFd};
    let to_c = |name: &str| {
        std::ffi::CString::new(name)
            .map_err(|_| Exchange::Failed(std::io::ErrorKind::InvalidInput.into()))
    };
    let (a, b) = (to_c(a)?, to_c(b)?);
    let fd = dir.as_fd().as_raw_fd();
    // SAFETY: `fd` is an open directory descriptor owned by `dir`, both names
    // are NUL-terminated strings that outlive the call.
    #[cfg(target_os = "macos")]
    let rc = unsafe { libc::renameatx_np(fd, a.as_ptr(), fd, b.as_ptr(), libc::RENAME_SWAP) };
    // `renameat2` through `syscall`: glibc wraps it only since 2.28, musl not
    // at all. SAFETY: as above; the flags are those of renameat2(2).
    #[cfg(target_os = "linux")]
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            fd,
            a.as_ptr(),
            fd,
            b.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if rc == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    Err(match error.raw_os_error() {
        Some(libc::ENOENT) => Exchange::Missing,
        Some(code)
            if code == libc::ENOSYS
                || code == libc::EINVAL
                || code == libc::ENOTSUP
                || code == libc::EOPNOTSUPP =>
        {
            Exchange::Unsupported
        }
        _ => Exchange::Failed(error),
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn exchange(_: &Dir, _: &str, _: &str) -> Result<(), Exchange> {
    Err(Exchange::Unsupported)
}

/// SHA-256 of the regular file `name` in `dir`, read without blocking or
/// following links; `None` for anything else (gone, a link, a FIFO, too big).
fn hash_regular(dir: &Dir, name: &str, shown: &Path) -> Option<String> {
    let entry = dir.symlink_metadata(name).ok()?;
    if entry.file_type().is_symlink() || !entry.is_file() {
        return None;
    }
    read_regular(dir, name, shown, &entry)
        .ok()
        .map(|bytes| crate::fsutil::sha256_hex(&bytes))
}

/// Give `file` the name `<name>.devflow-conflict-<time>` in `dir`, without
/// replacing anything. When that cannot be done (every candidate name is taken,
/// the rename fails) the content stays where it is and `file` itself is
/// returned: the caller must then keep that file and report its path.
fn keep(dir: &Dir, file: &str, name: &str, stamp: Option<&str>) -> PathBuf {
    let stamp = stamp
        .map(str::to_string)
        .unwrap_or_else(|| chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string());
    for attempt in 0..100 {
        let suffix = if attempt == 0 {
            String::new()
        } else {
            format!("-{attempt}")
        };
        let candidate = format!("{name}.devflow-conflict-{stamp}{suffix}");
        match dir.hard_link(file, dir, &candidate) {
            Ok(()) => {
                let _ = dir.remove_file(file);
                return PathBuf::from(candidate);
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(_) => {
                if dir.symlink_metadata(&candidate).is_err()
                    && dir.rename(file, dir, &candidate).is_ok()
                {
                    return PathBuf::from(candidate);
                }
                break;
            }
        }
    }
    PathBuf::from(file)
}

/// Keep the content of `file` (somebody's edit) and report where it is. If it
/// cannot get a name of its own it stays at `file`, which is then not deleted
/// and is the path the conflict names.
fn kept_conflict(
    dir: &Dir,
    file: &str,
    name: &str,
    shown: &Path,
    seams: &Seams<'_>,
    leave_staged: &std::cell::Cell<bool>,
) -> anyhow::Error {
    let kept = keep(dir, file, name, seams.stamp);
    if kept == Path::new(file) {
        leave_staged.set(true);
    }
    conflict(shown, Some(&kept_at(shown, &kept)))
}

/// Where a kept file is, for a message.
fn kept_at(shown: &Path, kept: &Path) -> PathBuf {
    shown.parent().unwrap_or(shown).join(kept)
}

/// Replace the file that held `expected` with the staged one, never losing a
/// save that landed since the plan.
fn replace(
    dir: &Dir,
    staged: &str,
    name: &str,
    shown: &Path,
    (expected, ours): (&str, &str),
    seams: &Seams<'_>,
    leave_staged: &std::cell::Cell<bool>,
) -> Result<()> {
    if seams.no_exchange {
        return replace_by_link(dir, staged, name, shown, expected, seams);
    }
    match exchange(dir, staged, name) {
        Ok(()) => {}
        Err(Exchange::Missing) => return Err(changed(shown)),
        Err(Exchange::Unsupported) => {
            return replace_by_link(dir, staged, name, shown, expected, seams);
        }
        Err(Exchange::Failed(error)) => {
            return Err(error).with_context(|| format!("cannot write {}", shown.display()));
        }
    }
    if let Some(seam) = seams.after_exchange {
        seam();
    }
    // The staged name now holds what was at the destination.
    if hash_regular(dir, staged, shown).as_deref() == Some(expected) {
        return Ok(());
    }
    // Edited since the plan read it (or not a file we can vouch for): put it back.
    match exchange(dir, staged, name) {
        Ok(()) => {
            // Now the staged name holds what the destination held since the
            // first exchange: devflow's content, unless it was edited once more.
            if hash_regular(dir, staged, shown).as_deref() == Some(ours) {
                Err(changed(shown))
            } else {
                Err(kept_conflict(dir, staged, name, shown, seams, leave_staged))
            }
        }
        Err(_) => Err(kept_conflict(dir, staged, name, shown, seams, leave_staged)),
    }
}

/// Without an exchange: the destination is hard-linked to a backup, replaced
/// by rename, and the backup is compared with what the plan read. That catches
/// an edit in place and a save by rename before the link; a save by rename in
/// the instructions between the identity check and the rename is the window
/// left, and is overwritten.
fn replace_by_link(
    dir: &Dir,
    staged: &str,
    name: &str,
    shown: &Path,
    expected: &str,
    seams: &Seams<'_>,
) -> Result<()> {
    let backup = format!(".{name}.{}.bak", uuid::Uuid::new_v4());
    match dir.hard_link(name, dir, &backup) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Err(changed(shown)),
        Err(_) => {
            // No hard links either: compare, then rename. Not airtight.
            return if hash_regular(dir, name, shown).as_deref() == Some(expected) {
                dir.rename(staged, dir, name)
                    .with_context(|| format!("cannot write {}", shown.display()))
            } else {
                Err(changed(shown))
            };
        }
    }
    if let Some(seam) = seams.after_backup {
        seam();
    }
    let linked = |a: &str, b: &str| match (dir.symlink_metadata(a), dir.symlink_metadata(b)) {
        (Ok(a), Ok(b)) => same(&a, &b),
        _ => false,
    };
    // A save by rename since the link made a new file at the name.
    if !linked(name, &backup) {
        let _ = dir.remove_file(&backup);
        return Err(changed(shown));
    }
    if let Err(error) = dir.rename(staged, dir, name) {
        let _ = dir.remove_file(&backup);
        return Err(error).with_context(|| format!("cannot write {}", shown.display()));
    }
    // The backup is the file that was replaced; an edit in place shows in it.
    if hash_regular(dir, &backup, shown).as_deref() == Some(expected) {
        let _ = dir.remove_file(&backup);
        Ok(())
    } else {
        // (The backup is its own name: it survives whether or not it is moved.)
        let kept = keep(dir, &backup, name, seams.stamp);
        Err(conflict(shown, Some(&kept_at(shown, &kept))))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::os::unix::fs::MetadataExt as _;

    const PLANNED: &str = "{\"planned\": true}\n";
    const OURS: &str = "{\"ours\": true}\n";
    const THEIRS: &str = "{\"edited by someone\": true}\n";

    fn hash(text: &str) -> String {
        crate::fsutil::sha256_hex(text.as_bytes())
    }

    struct Rig {
        dir: tempfile::TempDir,
        checkout: Checkout,
    }

    impl Rig {
        fn new() -> Rig {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            std::fs::write(root.join("host.json"), PLANNED).unwrap();
            let checkout = Checkout::open(&root).unwrap();
            Rig { dir, checkout }
        }

        fn file(&self) -> PathBuf {
            self.dir.path().canonicalize().unwrap().join("host.json")
        }

        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(self.dir.path())
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }

        fn write(&self, seams: &Seams<'_>) -> Result<()> {
            write_with(
                &self.checkout,
                "host.json",
                Some(&hash(PLANNED)),
                OURS.as_bytes(),
                seams,
            )
        }
    }

    const MODES: [bool; 2] = [false, true];

    fn replace_by_rename(path: &Path, text: &str) {
        let other = path.with_extension("save");
        std::fs::write(&other, text).unwrap();
        std::fs::rename(other, path).unwrap();
    }

    #[test]
    fn the_file_is_replaced_by_exchange_and_by_the_fallback_and_nothing_is_left_beside_it() {
        for no_exchange in MODES {
            let rig = Rig::new();
            rig.write(&Seams {
                no_exchange,
                ..Seams::default()
            })
            .unwrap();
            assert_eq!(
                std::fs::read_to_string(rig.file()).unwrap(),
                OURS,
                "{no_exchange}"
            );
            assert_eq!(rig.names(), ["host.json"], "{no_exchange}");
        }
    }

    #[test]
    fn a_save_in_place_between_staging_and_the_replacement_is_kept() {
        for no_exchange in MODES {
            let rig = Rig::new();
            let file = rig.file();
            let edit = || std::fs::write(&file, THEIRS).unwrap();
            let error = rig
                .write(&Seams {
                    after_staging: Some(&edit),
                    no_exchange,
                    ..Seams::default()
                })
                .unwrap_err();
            let message = format!("{error:#}");
            // Exchange mode puts the edit back; the fallback replaced the file
            // and keeps the displaced edit beside it. Either way it exists.
            let kept: Vec<String> = rig
                .names()
                .into_iter()
                .filter(|n| n.contains("devflow-conflict"))
                .collect();
            let current = std::fs::read_to_string(&file).unwrap();
            if kept.is_empty() {
                assert_eq!(current, THEIRS, "{no_exchange}: {message}");
            } else {
                assert_eq!(kept.len(), 1, "{kept:?}");
                let saved = rig.dir.path().canonicalize().unwrap().join(&kept[0]);
                assert_eq!(std::fs::read_to_string(saved).unwrap(), THEIRS);
                assert!(message.contains("devflow-conflict"), "{message}");
            }
            assert!(
                !rig.names()
                    .iter()
                    .any(|n| n.ends_with(".tmp") || n.ends_with(".bak")),
                "{:?}",
                rig.names()
            );
        }
    }

    /// Every regular file in the directory, by name, with its content.
    fn contents(rig: &Rig) -> Vec<(String, String)> {
        rig.names()
            .into_iter()
            .filter_map(|name| {
                let text = std::fs::read_to_string(rig.dir.path().join(&name)).ok()?;
                Some((name, text))
            })
            .collect()
    }

    #[test]
    fn a_save_by_rename_between_staging_and_the_replacement_is_kept() {
        for no_exchange in MODES {
            let rig = Rig::new();
            let file = rig.file();
            let save = || replace_by_rename(&file, THEIRS);
            let seam = Seams {
                after_staging: Some(&save),
                no_exchange,
                ..Seams::default()
            };
            let error = rig.write(&seam).unwrap_err();
            let all = contents(&rig);
            // The save is somewhere: at the file (exchange mode puts it back)
            // or next to it (the fallback replaced the file and keeps it).
            assert!(
                all.iter().any(|(_, text)| text == THEIRS),
                "{no_exchange}: {all:?}: {error:#}"
            );
            assert!(
                !all.iter()
                    .any(|(n, _)| n.ends_with(".tmp") || n.ends_with(".bak")),
                "{all:?}"
            );
            if !no_exchange {
                assert_eq!(std::fs::read_to_string(&file).unwrap(), THEIRS);
                assert_eq!(rig.names(), ["host.json"]);
            }
        }
    }

    #[test]
    fn a_save_by_rename_right_after_the_link_in_the_fallback_is_kept() {
        let rig = Rig::new();
        let file = rig.file();
        let save = || replace_by_rename(&file, THEIRS);
        let error = rig
            .write(&Seams {
                after_backup: Some(&save),
                no_exchange: true,
                ..Seams::default()
            })
            .unwrap_err();
        // The save replaced the file after the backup link: devflow does not
        // replace it.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), THEIRS, "{error:#}");
        assert_eq!(rig.names(), ["host.json"]);
    }

    #[test]
    fn an_edit_right_after_the_exchange_is_kept_next_to_the_file() {
        let rig = Rig::new();
        let file = rig.file();
        // Someone saves over devflow's content the moment it is in place, and
        // the displaced file was an edit as well.
        let first = Cell::new(true);
        let edit = || {
            if first.replace(false) {
                std::fs::write(&file, "{\"second\": true}\n").unwrap();
            }
        };
        let before = || std::fs::write(&file, THEIRS).unwrap();
        let error = rig
            .write(&Seams {
                after_staging: Some(&before),
                after_exchange: Some(&edit),
                ..Seams::default()
            })
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("devflow-conflict"), "{message}");
        let kept: Vec<String> = rig
            .names()
            .into_iter()
            .filter(|n| n.contains("devflow-conflict"))
            .collect();
        assert_eq!(kept.len(), 1, "{:?}", rig.names());
        // Nothing anyone wrote is lost: the first edit is back at the file,
        // the second is kept.
        let all: Vec<String> = std::iter::once(std::fs::read_to_string(&file).unwrap())
            .chain(std::iter::once(
                std::fs::read_to_string(rig.dir.path().canonicalize().unwrap().join(&kept[0]))
                    .unwrap(),
            ))
            .collect();
        assert!(all.iter().any(|t| t == THEIRS), "{all:?}");
        assert!(all.iter().any(|t| t.contains("second")), "{all:?}");
    }

    #[test]
    fn an_edit_that_cannot_get_a_conflict_name_is_never_deleted() {
        let rig = Rig::new();
        let file = rig.file();
        let root = rig.dir.path().canonicalize().unwrap();
        // Every name the edit could be kept under is taken.
        std::fs::write(root.join("host.json.devflow-conflict-T"), "taken").unwrap();
        for attempt in 1..100 {
            std::fs::write(
                root.join(format!("host.json.devflow-conflict-T-{attempt}")),
                "taken",
            )
            .unwrap();
        }
        // The file was edited (so devflow's exchange displaces an edit), and
        // devflow's content is edited again the moment it is in place.
        let first = || std::fs::write(&file, THEIRS).unwrap();
        let second = || std::fs::write(&file, "{\"second\": true}\n").unwrap();
        let error = rig
            .write(&Seams {
                after_staging: Some(&first),
                after_exchange: Some(&second),
                stamp: Some("T"),
                ..Seams::default()
            })
            .unwrap_err();
        let message = format!("{error:#}");
        // The first edit is back at the file; the second survives, under the
        // only name it has left, and the report names that path.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), THEIRS);
        let survivors: Vec<String> = rig
            .names()
            .into_iter()
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert_eq!(survivors.len(), 1, "{:?}", rig.names());
        assert_eq!(
            std::fs::read_to_string(root.join(&survivors[0])).unwrap(),
            "{\"second\": true}\n"
        );
        assert!(message.contains(&survivors[0]), "{message}");
        // The names that were taken are untouched.
        assert_eq!(
            std::fs::read_to_string(root.join("host.json.devflow-conflict-T")).unwrap(),
            "taken"
        );
    }

    #[test]
    fn an_edit_that_gets_a_name_is_kept_under_it_and_the_staged_file_is_gone() {
        let rig = Rig::new();
        let file = rig.file();
        let first = || std::fs::write(&file, THEIRS).unwrap();
        let second = || std::fs::write(&file, "{\"second\": true}\n").unwrap();
        let error = rig
            .write(&Seams {
                after_staging: Some(&first),
                after_exchange: Some(&second),
                stamp: Some("T"),
                ..Seams::default()
            })
            .unwrap_err();
        assert!(format!("{error:#}").contains("host.json.devflow-conflict-T"));
        assert_eq!(rig.names(), ["host.json", "host.json.devflow-conflict-T"]);
    }

    #[test]
    fn a_deleted_file_is_not_recreated_over_the_users_deletion() {
        for no_exchange in MODES {
            let rig = Rig::new();
            let file = rig.file();
            let remove = || std::fs::remove_file(&file).unwrap();
            assert!(
                rig.write(&Seams {
                    after_staging: Some(&remove),
                    no_exchange,
                    ..Seams::default()
                })
                .is_err()
            );
            assert!(!file.exists(), "{no_exchange}");
            assert_eq!(rig.names(), Vec::<String>::new(), "{no_exchange}");
        }
    }

    fn mkfifo(path: &Path) {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    }

    /// Run `work` and fail the test when it does not return: a FIFO with no
    /// writer would block an ordinary `open` for good.
    fn promptly<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(work());
        });
        rx.recv_timeout(std::time::Duration::from_secs(10))
            .expect("the write blocked on a FIFO")
    }

    #[test]
    fn a_fifo_put_in_place_of_the_file_never_blocks_the_write() {
        // Between the check and the open of the comparison.
        let rig = Rig::new();
        let file = rig.file();
        let checkout = rig.checkout.clone();
        let outcome = promptly(move || {
            let swap = || {
                std::fs::remove_file(&file).unwrap();
                mkfifo(&file);
            };
            write_with(
                &checkout,
                "host.json",
                Some(&hash(PLANNED)),
                OURS.as_bytes(),
                &Seams {
                    before_read: Some(&swap),
                    ..Seams::default()
                },
            )
            .is_err()
        });
        assert!(outcome);

        // Between staging and the replacement: the exchange puts the FIFO at
        // the staged name, which is judged without opening it blocking.
        for no_exchange in MODES {
            let rig = Rig::new();
            let file = rig.file();
            let checkout = rig.checkout.clone();
            let outcome = promptly(move || {
                let swap = || {
                    std::fs::remove_file(&file).unwrap();
                    mkfifo(&file);
                };
                write_with(
                    &checkout,
                    "host.json",
                    Some(&hash(PLANNED)),
                    OURS.as_bytes(),
                    &Seams {
                        after_staging: Some(&swap),
                        no_exchange,
                        ..Seams::default()
                    },
                )
                .is_err()
            });
            assert!(outcome, "{no_exchange}");
            let fifos: Vec<String> = rig
                .names()
                .into_iter()
                .filter(|n| {
                    std::fs::symlink_metadata(rig.dir.path().join(n))
                        .unwrap()
                        .file_type()
                        .is_fifo_like()
                })
                .collect();
            // Never read, never lost: where it was (exchange), or kept beside
            // the file as a conflict (the fallback replaced the name).
            assert_eq!(fifos.len(), 1, "{no_exchange}: {:?}", rig.names());
            if !no_exchange {
                assert_eq!(fifos[0], "host.json");
            }
        }
    }

    trait FifoLike {
        fn is_fifo_like(&self) -> bool;
    }

    impl FifoLike for std::fs::FileType {
        fn is_fifo_like(&self) -> bool {
            use std::os::unix::fs::FileTypeExt;
            self.is_fifo()
        }
    }

    #[test]
    fn reading_a_fifo_or_a_link_is_refused_without_opening_it() {
        let rig = Rig::new();
        std::fs::remove_file(rig.file()).unwrap();
        mkfifo(&rig.file());
        let checkout = rig.checkout.clone();
        let error = promptly(move || read(&checkout, "host.json").unwrap_err());
        assert!(format!("{error:#}").contains("not a regular file"));
    }

    #[test]
    fn the_checkout_must_still_be_the_directory_the_edit_was_planned_in() {
        let rig = Rig::new();
        let root = rig.dir.path().canonicalize().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        // Moved, with a link to somewhere else put at its place.
        let moved = elsewhere.path().canonicalize().unwrap().join("moved");
        std::fs::rename(&root, &moved).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), &root).unwrap();
        let error = rig.write(&Seams::default()).unwrap_err();
        assert!(
            format!("{error:#}").contains("not the directory"),
            "{error:#}"
        );
        assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
        assert_eq!(
            std::fs::read_to_string(moved.join("host.json")).unwrap(),
            PLANNED
        );
        // Replaced by another real directory.
        std::fs::remove_file(&root).unwrap();
        std::fs::create_dir(&root).unwrap();
        let error = rig.write(&Seams::default()).unwrap_err();
        assert!(
            format!("{error:#}").contains("not the directory"),
            "{error:#}"
        );
        assert!(std::fs::read_dir(&root).unwrap().next().is_none());
        let _ = (moved.metadata().unwrap().ino(), 0);
        // (Put back, so the temporary directory can be removed.)
        std::fs::remove_dir(&root).unwrap();
        std::fs::rename(&moved, &root).unwrap();
    }
}
