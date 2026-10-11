//! Small filesystem helpers shared by state writers.
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

/// SHA-256 of `bytes`, lowercase hex.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Write `content` to `path` atomically: a sibling temporary file is fully
/// written and synced, then renamed over the target.
///
/// The permissions of what is replaced carry over: when `path` is a regular
/// file, the temporary file is created with that file's mode, so it is never
/// wider than the file it replaces (a 0600 file holding a token stays 0600).
/// A file that does not exist yet gets the caller's mode: `private` restricts
/// it to the current user (credentials, receipts), otherwise the process
/// umask decides. `private` also narrows an existing file to its owner (a
/// credential file that was left world-readable is tightened, never widened).
///
/// This replaces whatever is at `path` when the rename happens. A caller that
/// validated the file earlier and must not overwrite a save that landed since
/// uses [`replace_if_unchanged`].
pub(crate) fn write_atomic(path: &Path, content: &[u8], private: bool) -> Result<()> {
    let tmp = stage(path, content, private)?;
    let result =
        std::fs::rename(&tmp, path).with_context(|| format!("cannot write {}", path.display()));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Create the sibling temporary file for `path`, holding `content`, synced and
/// with the mode [`write_atomic`] documents. Returns its path; the file is
/// removed again when staging fails.
fn stage(path: &Path, content: &[u8], private: bool) -> Result<PathBuf> {
    let parent = parent_of(path);
    std::fs::create_dir_all(parent)
        .with_context(|| format!("cannot create {}", parent.display()))?;
    let name = path
        .file_name()
        .context("path has no file name")?
        .to_string_lossy();
    let tmp = parent.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        let replaced_mode = {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            // `symlink_metadata`: a link is replaced by a regular file, so
            // the mode of what it points to says nothing about the new file.
            let replaced = std::fs::symlink_metadata(path)
                .ok()
                .filter(|metadata| metadata.is_file())
                .map(|metadata| {
                    let mode = metadata.mode() & 0o777;
                    if private { mode & 0o600 } else { mode }
                });
            if let Some(mode) = replaced {
                options.mode(mode);
            } else if private {
                options.mode(0o600);
            }
            replaced
        };
        #[cfg(not(unix))]
        let _ = private;
        let mut file = options.open(&tmp)?;
        // The umask may have cleared bits the replaced file had; they are
        // restored (never more than the target had) so the mode carries over.
        #[cfg(unix)]
        if let Some(mode) = replaced_mode {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        file.write_all(content)?;
        file.sync_all()?;
        Ok(())
    })();
    match result {
        Ok(()) => Ok(tmp),
        Err(error) => {
            let _ = std::fs::remove_file(&tmp);
            Err(error).with_context(|| format!("cannot write {}", path.display()))
        }
    }
}

fn parent_of(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

// -- replace if unchanged ---------------------------------------------------

/// Why [`replace_if_unchanged`] did not replace the file.
#[derive(Debug)]
pub(crate) enum ReplaceError {
    /// The file is not what the caller validated. With an atomic exchange
    /// (see [`replace_if_unchanged`]) nothing of the other party's is
    /// overwritten or lost; see [`ReplaceConflict`] for where each version of
    /// the file is. Without one, a narrow window remains.
    Conflict(ReplaceConflict),
    /// An ordinary failure (permissions, a full disk, ...).
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for ReplaceError {
    fn from(error: anyhow::Error) -> Self {
        Self::Failed(error)
    }
}

impl std::fmt::Display for ReplaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict(conflict) => conflict.fmt(f),
            Self::Failed(error) => write!(f, "{error:#}"),
        }
    }
}

impl std::error::Error for ReplaceError {}

/// What happened to the file between the caller's validation and the
/// replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConflictKind {
    /// It was edited and is back as the other party left it; nothing was
    /// written.
    Edited,
    /// It was deleted; nothing was written.
    Deleted,
    /// It was created (the caller expected no file); nothing was written.
    Created,
    /// It was edited, and that version could not be put back: the caller's
    /// content is at `path` and the edited version is kept next to it.
    NotRestored,
    /// It was edited, put back, and then edited again while it was: the first
    /// edit is at `path` and the later one is kept next to it. Nothing was
    /// written.
    EditedAgain,
}

/// A refused replacement, with where every version of the file is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplaceConflict {
    pub path: PathBuf,
    pub kind: ConflictKind,
    /// A file next to `path` (`<name>.devflow-conflict-<timestamp>`, with the
    /// mode of the version it holds) with content that would otherwise be
    /// lost.
    pub kept: Option<PathBuf>,
}

impl std::fmt::Display for ReplaceConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let path = self.path.display();
        let kept = self
            .kept
            .as_ref()
            .map(|kept| kept.display().to_string())
            .unwrap_or_default();
        match self.kind {
            ConflictKind::Edited => write!(
                f,
                "{path} was edited while devflow was writing it; it was not overwritten"
            ),
            ConflictKind::Deleted => write!(
                f,
                "{path} was deleted while devflow was writing it; nothing was written"
            ),
            ConflictKind::Created => write!(
                f,
                "{path} was created while devflow was writing it; it was not overwritten"
            ),
            ConflictKind::NotRestored => write!(
                f,
                "{path} was edited while devflow was writing it, and that edit could not be put \
                 back: devflow's version is at {path}, the edited version is kept at {kept}"
            ),
            ConflictKind::EditedAgain => write!(
                f,
                "{path} was edited twice while devflow was writing it: the first edit is at \
                 {path}, the later one is kept at {kept}; devflow's version was not written"
            ),
        }
    }
}

/// Seams for tests: acting at the moments a concurrent save would land, and
/// taking the platforms without an atomic exchange.
#[derive(Default)]
pub(crate) struct ReplaceSeams<'a> {
    /// Runs after the temporary file is staged and before anything is
    /// exchanged: the moment between a caller's validation and the swap.
    pub after_staging: Option<&'a dyn Fn()>,
    /// Runs right after the exchange, while the caller's content is at the
    /// target and the displaced content is still at the temporary path.
    pub after_exchange: Option<&'a dyn Fn()>,
    /// Runs in the fallback right after the target was linked to its backup.
    pub after_backup: Option<&'a dyn Fn()>,
    /// Behave as on a platform or filesystem without an atomic exchange.
    pub no_exchange: bool,
}

/// Replace `path` with `content` only if it still holds what the caller
/// validated. Where the platform has an atomic exchange, a save that lands in
/// between is never lost; elsewhere a narrow window remains (see below).
///
/// `expected` is the SHA-256 (lowercase hex, see [`sha256_hex`]) of the
/// content the caller read and validated, or `None` when the file did not
/// exist. A check followed by a rename is not enough: an editor saving after
/// the check and before the rename would be overwritten silently. So:
///
/// 1. The new content is staged in a synced sibling file first.
/// 2. The staged file and the target are atomically exchanged
///    (`renamex_np(RENAME_SWAP)` on macOS, `renameat2(RENAME_EXCHANGE)` on
///    Linux). The target is never absent.
/// 3. What was displaced, now at the temporary path, is hashed. Equal to
///    `expected`: it is deleted and the replacement stands. Different: the
///    files are exchanged back. If that fails, or the target changed once
///    more in between, the displaced version is kept next to the target as
///    `<name>.devflow-conflict-<timestamp>` (its own mode) and the conflict
///    names both paths. With an exchange, the displaced content is never
///    silently lost. The exchange exists on APFS and on the Linux filesystems
///    that support `RENAME_EXCHANGE`.
///
/// **The guarantee needs the exchange.** Where none exists, the target is
/// hard-linked to a backup name first, then replaced by rename, and the backup
/// is compared with `expected`; a mismatch keeps the backup as a conflict
/// file. That catches edits made in place and saves by rename before the link,
/// and the hard-link backup is kept for them; but a save by rename in the few
/// instructions between the inode check and the rename lands in a window the
/// check cannot close, and is overwritten without a trace. Where hard links
/// are not available either, only compare-then-rename is left: the same
/// window, and no backup. A missing file (`expected` is `None`) is installed
/// with a hard link, which fails when the name was taken meanwhile.
///
/// The mode of the replaced file carries over as in [`write_atomic`].
// `apply` goes through `replace_if_unchanged_with` to reach the test seams;
// this is the entry point for every other writer.
#[allow(dead_code)]
pub(crate) fn replace_if_unchanged(
    path: &Path,
    content: &[u8],
    private: bool,
    expected: Option<&str>,
) -> Result<(), ReplaceError> {
    replace_if_unchanged_with(path, content, private, expected, &ReplaceSeams::default())
}

/// [`replace_if_unchanged`] with test seams.
pub(crate) fn replace_if_unchanged_with(
    path: &Path,
    content: &[u8],
    private: bool,
    expected: Option<&str>,
    seams: &ReplaceSeams,
) -> Result<(), ReplaceError> {
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(ReplaceError::Failed(anyhow::anyhow!(
            "{} is a directory",
            path.display()
        )));
    }
    let tmp = stage(path, content, private)?;
    if let Some(seam) = seams.after_staging {
        seam();
    }
    let replacement = Replacement {
        path,
        tmp: &tmp,
        replacement_hash: sha256_hex(content),
        seams,
    };
    let result = match expected {
        Some(expected) => replacement.over(expected),
        None => replacement.install_new(),
    };
    // The temporary path is left alone only when it is the one place a
    // version of the file survives.
    let holds_content = matches!(
        &result,
        Err(ReplaceError::Conflict(conflict)) if conflict.kept.as_deref() == Some(tmp.as_path())
    );
    if !holds_content {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

struct Replacement<'a> {
    path: &'a Path,
    tmp: &'a Path,
    /// SHA-256 of the staged content.
    replacement_hash: String,
    seams: &'a ReplaceSeams<'a>,
}

impl Replacement<'_> {
    fn conflict(&self, kind: ConflictKind, kept: Option<PathBuf>) -> ReplaceError {
        ReplaceError::Conflict(ReplaceConflict {
            path: self.path.to_path_buf(),
            kind,
            kept,
        })
    }

    fn failed(&self, error: std::io::Error) -> ReplaceError {
        ReplaceError::Failed(
            anyhow::Error::new(error).context(format!("cannot write {}", self.path.display())),
        )
    }

    /// Install the staged file where nothing existed.
    fn install_new(&self) -> Result<(), ReplaceError> {
        match std::fs::hard_link(self.tmp, self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(self.conflict(ConflictKind::Created, None))
            }
            // No hard links on this filesystem: check, then rename.
            Err(_) if std::fs::symlink_metadata(self.path).is_err() => {
                std::fs::rename(self.tmp, self.path).map_err(|e| self.failed(e))
            }
            Err(_) => Err(self.conflict(ConflictKind::Created, None)),
        }
    }

    /// Replace the file that held `expected`.
    fn over(&self, expected: &str) -> Result<(), ReplaceError> {
        if self.seams.no_exchange {
            return self.over_by_link(expected);
        }
        match exchange(self.tmp, self.path) {
            Ok(()) => {}
            Err(ExchangeError::Missing) => return Err(self.conflict(ConflictKind::Deleted, None)),
            Err(ExchangeError::Unsupported) => return self.over_by_link(expected),
            Err(ExchangeError::Other(error)) => return Err(self.failed(error)),
        }
        if let Some(seam) = self.seams.after_exchange {
            seam();
        }
        // The temporary path now holds what was at the target.
        if hash_file(self.tmp).as_deref() == Some(expected) {
            return Ok(());
        }
        // Edited since the caller read it: put it back.
        match exchange(self.tmp, self.path) {
            Ok(()) => {
                // Now the temporary path holds what the target held since the
                // exchange: the replacement, unless it was edited once more.
                if hash_file(self.tmp).as_deref() == Some(self.replacement_hash.as_str()) {
                    Err(self.conflict(ConflictKind::Edited, None))
                } else {
                    let kept = keep_conflict(self.tmp, self.path);
                    Err(self.conflict(ConflictKind::EditedAgain, Some(kept)))
                }
            }
            Err(_) => {
                let kept = keep_conflict(self.tmp, self.path);
                Err(self.conflict(ConflictKind::NotRestored, Some(kept)))
            }
        }
    }

    /// The fallback where no exchange exists.
    fn over_by_link(&self, expected: &str) -> Result<(), ReplaceError> {
        let parent = parent_of(self.path);
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let backup = parent.join(format!(".{name}.{}.bak", uuid::Uuid::new_v4()));
        match std::fs::hard_link(self.path, &backup) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(self.conflict(ConflictKind::Deleted, None));
            }
            Err(_) => {
                // No hard links either: compare, then rename. Not airtight.
                return if hash_file(self.path).as_deref() == Some(expected) {
                    std::fs::rename(self.tmp, self.path).map_err(|e| self.failed(e))
                } else {
                    Err(self.conflict(ConflictKind::Edited, None))
                };
            }
        }
        if let Some(seam) = self.seams.after_backup {
            seam();
        }
        // A save by rename since the link made a new file at the target.
        // Replacing it would lose it.
        if !same_file(self.path, &backup) {
            let _ = std::fs::remove_file(&backup);
            return Err(self.conflict(ConflictKind::Edited, None));
        }
        if let Err(error) = std::fs::rename(self.tmp, self.path) {
            let _ = std::fs::remove_file(&backup);
            return Err(self.failed(error));
        }
        // The backup is the file that was replaced; an edit in place since
        // the caller read it shows in it.
        if hash_file(&backup).as_deref() == Some(expected) {
            let _ = std::fs::remove_file(&backup);
            Ok(())
        } else {
            let kept = keep_conflict(&backup, self.path);
            Err(self.conflict(ConflictKind::NotRestored, Some(kept)))
        }
    }
}

/// SHA-256 of a file's content, `None` when it cannot be read.
fn hash_file(path: &Path) -> Option<String> {
    std::fs::read(path).ok().map(|bytes| sha256_hex(&bytes))
}

#[cfg(unix)]
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::symlink_metadata(a), std::fs::symlink_metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_file(a: &Path, b: &Path) -> bool {
    // No inode to compare: the content check after the rename is what is left.
    a.exists() && b.exists()
}

/// Give `file` the name `<name>.devflow-conflict-<timestamp>` next to
/// `target`, without replacing anything, and return where it is now. The
/// content and the mode are those of `file`. If it cannot be renamed it stays
/// where it is and that path is returned: the content is not lost.
fn keep_conflict(file: &Path, target: &Path) -> PathBuf {
    let parent = parent_of(target);
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    for attempt in 0..100 {
        let suffix = if attempt == 0 {
            String::new()
        } else {
            format!("-{attempt}")
        };
        let candidate = parent.join(format!("{name}.devflow-conflict-{stamp}{suffix}"));
        match std::fs::hard_link(file, &candidate) {
            Ok(()) => {
                let _ = std::fs::remove_file(file);
                return candidate;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => {
                // No hard links: a rename, unless the name appeared meanwhile.
                if std::fs::symlink_metadata(&candidate).is_err()
                    && std::fs::rename(file, &candidate).is_ok()
                {
                    return candidate;
                }
                break;
            }
        }
    }
    file.to_path_buf()
}

enum ExchangeError {
    /// One of the two names does not exist.
    Missing,
    /// This platform or filesystem has no atomic exchange.
    Unsupported,
    Other(std::io::Error),
}

/// Atomically swap the files at `a` and `b`.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn exchange(a: &Path, b: &Path) -> Result<(), ExchangeError> {
    use std::os::unix::ffi::OsStrExt;
    let to_c = |path: &Path| {
        std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| ExchangeError::Other(std::io::ErrorKind::InvalidInput.into()))
    };
    let (a, b) = (to_c(a)?, to_c(b)?);
    // SAFETY: both are valid NUL-terminated strings that outlive the call.
    #[cfg(target_os = "macos")]
    let rc = unsafe { libc::renamex_np(a.as_ptr(), b.as_ptr(), libc::RENAME_SWAP) };
    // `renameat2` through `syscall`: glibc wraps it only since 2.28, musl not
    // at all. SAFETY: as above; the flags are those of renameat2(2).
    #[cfg(target_os = "linux")]
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            a.as_ptr(),
            libc::AT_FDCWD,
            b.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if rc == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    Err(match error.raw_os_error() {
        Some(libc::ENOENT) => ExchangeError::Missing,
        // The filesystem (or kernel) does not know the flag.
        Some(code)
            if code == libc::ENOSYS
                || code == libc::EINVAL
                || code == libc::ENOTSUP
                || code == libc::EOPNOTSUPP =>
        {
            ExchangeError::Unsupported
        }
        _ => ExchangeError::Other(error),
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn exchange(_a: &Path, _b: &Path) -> Result<(), ExchangeError> {
    Err(ExchangeError::Unsupported)
}

/// Run a read-modify-write of a shared state file under an exclusive lock on
/// `lock_path`, so concurrent writers (two terminals, the GUI) don't lose each
/// other's updates. Readers need no lock: writes replace files atomically.
pub(crate) fn with_lock<T>(lock_path: &Path, change: impl FnOnce() -> Result<T>) -> Result<T> {
    if let Some(dir) = lock_path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path)
        .with_context(|| format!("cannot open {}", lock_path.display()))?;
    lock.lock()
        .with_context(|| format!("cannot lock {}", lock_path.display()))?;
    change()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn set_mode(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_replaced_file_keeps_its_mode() {
        let dir = tempfile::tempdir().unwrap();
        for (existing, private, expected) in [
            // The reported case: a token file stays private after a rewrite.
            (0o600, false, 0o600),
            (0o640, false, 0o640),
            (0o644, false, 0o644),
            (0o400, false, 0o400),
            // `private` narrows an existing file and never widens one.
            (0o644, true, 0o600),
            (0o640, true, 0o600),
            (0o400, true, 0o400),
            (0o600, true, 0o600),
        ] {
            let path = dir.path().join(format!("f-{existing:o}-{private}"));
            std::fs::write(&path, "old").unwrap();
            set_mode(&path, existing);
            write_atomic(&path, b"new", private).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), b"new");
            assert_eq!(
                mode(&path),
                expected,
                "existing {existing:o}, private {private}"
            );
        }
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn a_new_file_gets_the_callers_mode() {
        let dir = tempfile::tempdir().unwrap();
        let private = dir.path().join("private");
        write_atomic(&private, b"x", true).unwrap();
        assert_eq!(mode(&private), 0o600);
        // Not private: whatever the process umask leaves, never owner-only
        // by force and never executable.
        let shared = dir.path().join("shared");
        write_atomic(&shared, b"x", false).unwrap();
        assert_eq!(mode(&shared) & 0o111, 0);
        assert_eq!(mode(&shared) & 0o600, 0o600);
    }

    #[test]
    fn a_link_is_replaced_by_a_file_with_the_callers_mode() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, "kept").unwrap();
        set_mode(&target, 0o444);
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        write_atomic(&link, b"new", true).unwrap();
        assert_eq!(mode(&link), 0o600);
        assert!(!std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(std::fs::read(&target).unwrap(), b"kept");
        assert_eq!(mode(&target), 0o444);
    }

    // -- replace_if_unchanged ------------------------------------------------

    /// A file holding "old", mode 0640, in a directory of its own.
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("devflow.toml");
        std::fs::write(&path, "old").unwrap();
        set_mode(&path, 0o640);
        (dir, path)
    }

    fn old() -> String {
        sha256_hex(b"old")
    }

    fn conflict(result: Result<(), ReplaceError>) -> ReplaceConflict {
        match result {
            Err(ReplaceError::Conflict(conflict)) => conflict,
            other => panic!("expected a conflict, got {other:?}"),
        }
    }

    /// Both ways of replacing: with the atomic exchange and without.
    const MODES: [bool; 2] = [false, true];

    #[test]
    fn an_unchanged_file_is_replaced_and_keeps_its_mode() {
        for no_exchange in MODES {
            let (dir, path) = fixture();
            let seams = ReplaceSeams {
                no_exchange,
                ..ReplaceSeams::default()
            };
            replace_if_unchanged_with(&path, b"new", false, Some(&old()), &seams).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), b"new", "{no_exchange}");
            assert_eq!(mode(&path), 0o640, "{no_exchange}");
            assert_eq!(names(dir.path()), vec!["devflow.toml"], "{no_exchange}");
        }
    }

    #[test]
    fn a_save_between_staging_and_the_exchange_is_not_overwritten() {
        // Both kinds of save: in place, and an editor's write-then-rename.
        for by_rename in [false, true] {
            let (dir, path) = fixture();
            let save = {
                let path = path.clone();
                move || {
                    if by_rename {
                        let scratch = path.with_extension("save");
                        std::fs::write(&scratch, "edited").unwrap();
                        set_mode(&scratch, 0o600);
                        std::fs::rename(&scratch, &path).unwrap();
                    } else {
                        std::fs::write(&path, "edited").unwrap();
                    }
                }
            };
            let seams = ReplaceSeams {
                after_staging: Some(&save),
                ..ReplaceSeams::default()
            };
            let result = replace_if_unchanged_with(&path, b"new", false, Some(&old()), &seams);
            let conflict = conflict(result);
            assert_eq!(conflict.kind, ConflictKind::Edited, "{by_rename}");
            assert_eq!(conflict.kept, None);
            assert_eq!(std::fs::read(&path).unwrap(), b"edited", "{by_rename}");
            assert_eq!(
                mode(&path),
                if by_rename { 0o600 } else { 0o640 },
                "the edited file is exactly as the editor left it"
            );
            assert_eq!(names(dir.path()), vec!["devflow.toml"], "no leftovers");
        }
    }

    #[test]
    fn a_second_edit_during_the_exchange_is_kept_next_to_the_target() {
        let (dir, path) = fixture();
        // First edit: an editor's save by rename, with a mode of its own.
        let first = {
            let path = path.clone();
            move || {
                let scratch = path.with_extension("save");
                std::fs::write(&scratch, "first").unwrap();
                set_mode(&scratch, 0o600);
                std::fs::rename(&scratch, &path).unwrap();
            }
        };
        // Second edit: lands while devflow's version is at the target.
        let second = {
            let path = path.clone();
            move || std::fs::write(&path, "second").unwrap()
        };
        let seams = ReplaceSeams {
            after_staging: Some(&first),
            after_exchange: Some(&second),
            ..ReplaceSeams::default()
        };
        let conflict = conflict(replace_if_unchanged_with(
            &path,
            b"new",
            false,
            Some(&old()),
            &seams,
        ));
        assert_eq!(conflict.kind, ConflictKind::EditedAgain);
        let kept = conflict.kept.clone().expect("kept");
        assert!(
            kept.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("devflow.toml.devflow-conflict-")
        );
        assert_eq!(kept.parent(), path.parent());
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        assert_eq!(mode(&path), 0o600, "the first edit is back untouched");
        assert_eq!(std::fs::read(&kept).unwrap(), b"second");
        let message = conflict.to_string();
        assert!(
            message.contains(&path.display().to_string())
                && message.contains(&kept.display().to_string()),
            "{message}"
        );
        assert_eq!(names(dir.path()).len(), 2, "{:?}", names(dir.path()));
    }

    #[test]
    fn an_edit_that_cannot_be_put_back_is_kept_with_its_mode() {
        let (dir, path) = fixture();
        let first = {
            let path = path.clone();
            move || {
                let scratch = path.with_extension("save");
                std::fs::write(&scratch, "edited").unwrap();
                set_mode(&scratch, 0o600);
                std::fs::rename(&scratch, &path).unwrap();
            }
        };
        // The target disappears while devflow's version is there, so the
        // exchange back has nothing to exchange with.
        let delete = {
            let path = path.clone();
            move || std::fs::remove_file(&path).unwrap()
        };
        let seams = ReplaceSeams {
            after_staging: Some(&first),
            after_exchange: Some(&delete),
            ..ReplaceSeams::default()
        };
        let conflict = conflict(replace_if_unchanged_with(
            &path,
            b"new",
            false,
            Some(&old()),
            &seams,
        ));
        assert_eq!(conflict.kind, ConflictKind::NotRestored);
        let kept = conflict.kept.clone().expect("kept");
        assert!(kept.to_string_lossy().contains(".devflow-conflict-"));
        assert_eq!(std::fs::read(&kept).unwrap(), b"edited");
        assert_eq!(mode(&kept), 0o600, "the mode of the displaced version");
        assert_eq!(names(dir.path()).len(), 1, "{:?}", names(dir.path()));
    }

    #[test]
    fn a_deleted_file_is_not_recreated() {
        for no_exchange in MODES {
            let (dir, path) = fixture();
            let delete = {
                let path = path.clone();
                move || std::fs::remove_file(&path).unwrap()
            };
            let seams = ReplaceSeams {
                after_staging: Some(&delete),
                no_exchange,
                ..ReplaceSeams::default()
            };
            let conflict = conflict(replace_if_unchanged_with(
                &path,
                b"new",
                false,
                Some(&old()),
                &seams,
            ));
            assert_eq!(conflict.kind, ConflictKind::Deleted, "{no_exchange}");
            assert!(!path.exists());
            assert!(names(dir.path()).is_empty(), "{:?}", names(dir.path()));
        }
    }

    #[test]
    fn a_file_created_meanwhile_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("devflow.toml");
        let create = {
            let path = path.clone();
            move || std::fs::write(&path, "theirs").unwrap()
        };
        let seams = ReplaceSeams {
            after_staging: Some(&create),
            ..ReplaceSeams::default()
        };
        let conflict = conflict(replace_if_unchanged_with(
            &path, b"new", false, None, &seams,
        ));
        assert_eq!(conflict.kind, ConflictKind::Created);
        assert_eq!(std::fs::read(&path).unwrap(), b"theirs");
        assert_eq!(names(dir.path()), vec!["devflow.toml"]);

        // Nothing there: the file is created, private when asked.
        let fresh = dir.path().join("fresh.toml");
        replace_if_unchanged(&fresh, b"new", true, None).unwrap();
        assert_eq!(std::fs::read(&fresh).unwrap(), b"new");
        assert_eq!(mode(&fresh), 0o600);
        assert_eq!(names(dir.path()), vec!["devflow.toml", "fresh.toml"]);
    }

    #[test]
    fn without_an_exchange_an_edit_in_place_is_kept_in_a_conflict_file() {
        let (dir, path) = fixture();
        let edit = {
            let path = path.clone();
            move || std::fs::write(&path, "edited").unwrap()
        };
        let seams = ReplaceSeams {
            after_staging: Some(&edit),
            no_exchange: true,
            ..ReplaceSeams::default()
        };
        let conflict = conflict(replace_if_unchanged_with(
            &path,
            b"new",
            false,
            Some(&old()),
            &seams,
        ));
        assert_eq!(conflict.kind, ConflictKind::NotRestored);
        let kept = conflict.kept.expect("kept");
        assert_eq!(std::fs::read(&kept).unwrap(), b"edited");
        assert_eq!(mode(&kept), 0o640);
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(names(dir.path()).len(), 2, "{:?}", names(dir.path()));
    }

    #[test]
    fn without_an_exchange_a_save_by_rename_after_the_backup_is_not_overwritten() {
        let (dir, path) = fixture();
        let save = {
            let path = path.clone();
            move || {
                let scratch = path.with_extension("save");
                std::fs::write(&scratch, "edited").unwrap();
                std::fs::rename(&scratch, &path).unwrap();
            }
        };
        let seams = ReplaceSeams {
            after_backup: Some(&save),
            no_exchange: true,
            ..ReplaceSeams::default()
        };
        let conflict = conflict(replace_if_unchanged_with(
            &path,
            b"new",
            false,
            Some(&old()),
            &seams,
        ));
        assert_eq!(conflict.kind, ConflictKind::Edited);
        assert_eq!(std::fs::read(&path).unwrap(), b"edited");
        assert_eq!(names(dir.path()), vec!["devflow.toml"], "no leftovers");
    }

    #[test]
    fn without_an_exchange_an_edit_in_place_after_the_backup_is_kept() {
        let (_dir, path) = fixture();
        let edit = {
            let path = path.clone();
            move || std::fs::write(&path, "edited").unwrap()
        };
        let seams = ReplaceSeams {
            after_backup: Some(&edit),
            no_exchange: true,
            ..ReplaceSeams::default()
        };
        let conflict = conflict(replace_if_unchanged_with(
            &path,
            b"new",
            false,
            Some(&old()),
            &seams,
        ));
        assert_eq!(conflict.kind, ConflictKind::NotRestored);
        let kept = conflict.kept.expect("kept");
        assert_eq!(std::fs::read(&kept).unwrap(), b"edited");
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn a_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("devflow.toml");
        std::fs::create_dir(&path).unwrap();
        match replace_if_unchanged(&path, b"new", false, Some(&old())) {
            Err(ReplaceError::Failed(error)) => {
                assert!(error.to_string().contains("directory"), "{error}")
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(names(dir.path()), vec!["devflow.toml"]);
    }
}
