//! Putting a database file in a workspace, replacing it and removing it.
//!
//! Everything here works on files and on SQLite's locks; which workspace owns
//! the file, where its data comes from and what the receipt records is the
//! caller's ([`crate::setup`]). The rules:
//!
//! - **Staged, then installed.** The new file is written beside its
//!   destination under a name that carries the operation's identity and
//!   installed with an atomic rename. An interrupted run leaves a partial file
//!   under that name, never one under the real name, and the partial file is
//!   removed by identity. The staged file's identity (device and inode) is
//!   known before it is installed ([`Prepared::identity`]), so a recovery can
//!   tell the file it installed from one somebody else put there.
//! - **Confined to the worktree.** Every file below is reached from the
//!   worktree's directory descriptor without following a link
//!   ([`super::confined`]), and what is done to it is done through that
//!   descriptor: a `db` directory that is a link to another checkout's is
//!   refused, never followed.
//! - **Never over a file that is in use.** Replacing or removing a database
//!   takes SQLite's exclusive lock on it ([`hold`]) within a bound and keeps
//!   it until the effect is done: the old file's `-wal`, `-shm` and `-journal`
//!   go, and the file goes or is replaced, only while that lock is held, and a
//!   failure to take it for any reason but "another process has it" is
//!   refused rather than guessed at. What no lock reveals is a process that
//!   has the file open between transactions: SQLite keeps no lock for an idle
//!   connection, and devflow does not enumerate other processes' descriptors,
//!   so such a handle is not detected; it goes on using the old inode (gone or
//!   replaced) and its next statement does not reach the new file.
//! - **Only what the receipt says is ours.** Nothing here decides ownership.
use super::capture::{self, Error as CaptureError, How, Options, Share, sidecar};
use super::confined::{self, Identity, Place, SIDECARS};
use crate::config::SqliteSection;
use crate::services::catalog::CloneMethod;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The absolute path of a workspace's database: `section.path` inside the
/// worktree `root`.
pub fn file_in(root: &Path, section: &SqliteSection) -> PathBuf {
    root.join(&section.path)
}

/// The `sqlite:` URL of `file`: `sqlite://` and the absolute path, percent
/// encoded (`sqlite:///work/shop/db/dev.sqlite3`). Frameworks differ on how
/// they read the slashes; `[env]` recipes build their own form from the path
/// variable.
pub fn url_of(file: &Path) -> String {
    const PATH: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'/')
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    format!(
        "sqlite://{}",
        percent_encoding::utf8_percent_encode(&file.to_string_lossy(), PATH)
    )
}

/// Where a database's data comes from.
#[derive(Debug, Clone)]
pub enum Source {
    /// Nothing: the workspace starts without a file, and the application
    /// creates one when it first opens it.
    Empty,
    /// The file another workspace has.
    File { path: PathBuf },
    /// A snapshot's file.
    Snapshot { file: PathBuf },
}

/// What provisioning a file did.
#[derive(Debug, Clone, Serialize)]
pub struct Provisioned {
    /// `reflink` when the blocks are shared with something, `copy` when
    /// they are not; `None` when nothing was cloned (an empty start).
    pub method: Option<CloneMethod>,
    /// How the consistent copy was taken, for a file source.
    pub how: Option<How>,
    pub bytes: u64,
    /// How long the source's writer was waited for.
    pub waited_ms: u64,
}

/// A new file written next to its destination.
struct Staged {
    bytes: u64,
    method: Option<CloneMethod>,
    how: Option<How>,
    waited: Duration,
}

impl Staged {
    fn provisioned(&self) -> Provisioned {
        Provisioned {
            method: self.method,
            how: self.how,
            bytes: self.bytes,
            waited_ms: self.waited.as_millis() as u64,
        }
    }
}

impl Provisioned {
    fn nothing() -> Provisioned {
        Provisioned {
            method: None,
            how: None,
            bytes: 0,
            waited_ms: 0,
        }
    }
}

/// What a clone needs to refuse: the bound for a writer and whether a full
/// copy is allowed.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub wait: Duration,
    pub require_cow: bool,
}

impl Limits {
    pub fn of(section: &SqliteSection) -> Limits {
        Limits {
            wait: section.busy_wait(),
            require_cow: section.require_cow,
        }
    }

    fn share(self) -> Share {
        if self.require_cow {
            Share::Require
        } else {
            Share::Try
        }
    }
}

/// The name a file being written next to `dest` has: it carries the identity
/// of the operation, so a leftover is recognized as this operation's.
pub fn staged_name(dest: &Path, identity: uuid::Uuid) -> PathBuf {
    let name = dest.file_name().unwrap_or_default();
    dest.with_file_name(staged_file_name(name, identity))
}

fn staged_file_name(name: &OsStr, identity: uuid::Uuid) -> OsString {
    let mut staged = OsString::from(".");
    staged.push(name);
    staged.push(format!(".devflow-{}.partial", identity.simple()));
    staged
}

/// The place of `dest` (a path below the worktree `root`).
fn place_of(root: &Path, dest: &Path, create: bool) -> Result<Option<Place>> {
    let relative = dest.strip_prefix(root).with_context(|| {
        format!(
            "{} is not inside the worktree {}",
            dest.display(),
            root.display()
        )
    })?;
    Place::open(root, relative, create)
}

/// The database `relative` below the worktree `root`, for reading: `None`
/// when there is none. A link on the way (a tracked `db -> /elsewhere`) is an
/// error, so a snapshot or a clone is never of somebody else's file.
pub fn source_file(root: &Path, relative: &Path) -> Result<Option<PathBuf>> {
    let Some(place) = Place::open(root, relative, false)? else {
        return Ok(None);
    };
    Ok(place.identity()?.map(|_| place.path().to_path_buf()))
}

/// Remove what an interrupted run of operation `identity` left beside `dest`.
/// Best effort: a leftover under a name that carries the identity is the
/// caller's own, and a directory that cannot be reached safely holds none.
pub fn clear_staged(root: &Path, dest: &Path, identity: uuid::Uuid) {
    if let Ok(Some(place)) = place_of(root, dest, false) {
        place.remove_staged(&staged_file_name(place.name(), identity));
        // A file this operation took out of the way before it died.
        let mut displaced = OsString::from(".");
        displaced.push(place.name());
        displaced.push(format!(".devflow-{}.displaced", identity.simple()));
        let _ = place.remove(&displaced);
    }
}

/// Something is at the destination that was not put there by this operation.
#[derive(Debug)]
pub struct Collision {
    pub path: PathBuf,
}

impl std::fmt::Display for Collision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} appeared while it was being cloned; it was left alone",
            self.path.display()
        )
    }
}

impl std::error::Error for Collision {}

/// Moments a test acts at, as an application that creates the file would.
#[doc(hidden)]
pub mod seams {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};

    type Action = Box<dyn Fn() + Send + Sync>;

    fn before_install() -> &'static Mutex<HashMap<PathBuf, Action>> {
        static HOOKS: OnceLock<Mutex<HashMap<PathBuf, Action>>> = OnceLock::new();
        HOOKS.get_or_init(Default::default)
    }

    fn before_write() -> &'static Mutex<HashMap<PathBuf, Action>> {
        static HOOKS: OnceLock<Mutex<HashMap<PathBuf, Action>>> = OnceLock::new();
        HOOKS.get_or_init(Default::default)
    }

    /// Run `action` once, right before the staged file for `destination` is
    /// written (the directory has just been looked at).
    pub fn set_before_write(destination: PathBuf, action: impl Fn() + Send + Sync + 'static) {
        before_write()
            .lock()
            .unwrap()
            .insert(destination, Box::new(action));
    }

    pub(super) fn run_before_write(destination: &Path) {
        let action = before_write().lock().unwrap().remove(destination);
        if let Some(action) = action {
            action();
        }
    }

    fn while_held() -> &'static Mutex<HashMap<PathBuf, Action>> {
        static HOOKS: OnceLock<Mutex<HashMap<PathBuf, Action>>> = OnceLock::new();
        HOOKS.get_or_init(Default::default)
    }

    /// Run `action` once, with SQLite's exclusive lock on the database at
    /// `destination` held, right before the file is removed or replaced.
    pub fn set_while_held(destination: PathBuf, action: impl Fn() + Send + Sync + 'static) {
        while_held()
            .lock()
            .unwrap()
            .insert(destination, Box::new(action));
    }

    pub(super) fn run_while_held(destination: &Path) {
        let action = while_held().lock().unwrap().remove(destination);
        if let Some(action) = action {
            action();
        }
    }

    /// Run `action` once, right before the file staged for `destination` is
    /// installed (after its identity was recorded).
    pub fn set_before_install(destination: PathBuf, action: impl Fn() + Send + Sync + 'static) {
        before_install()
            .lock()
            .unwrap()
            .insert(destination, Box::new(action));
    }

    pub(super) fn run_before_install(destination: &Path) {
        let action = before_install().lock().unwrap().remove(destination);
        if let Some(action) = action {
            action();
        }
    }
}

/// A new database written next to its destination, not installed yet.
pub struct Prepared {
    place: Place,
    staged: Option<(OsString, Staged, Identity)>,
}

impl Prepared {
    /// The identity of the staged file; `None` for an empty start (nothing was
    /// staged). Record it before [`Prepared::install`]: it is what a recovery
    /// compares the destination with.
    pub fn identity(&self) -> Option<Identity> {
        self.staged.as_ref().map(|(_, _, identity)| *identity)
    }

    /// Put the staged file in place without replacing anything. A destination
    /// that exists is a [`Collision`] and the staged file is removed.
    pub fn install(self) -> Result<Provisioned> {
        let Some((name, staged, identity)) = self.staged else {
            return Ok(Provisioned::nothing());
        };
        seams::run_before_install(self.place.path());
        // The file that was staged is still the one in the directory.
        match self.place.identity_of(&name)? {
            Some(now) if now.is_same_file(&identity) => {}
            _ => {
                self.place.remove_staged(&name);
                anyhow::bail!(
                    "the file staged for {} changed before it was installed; nothing was installed",
                    self.place.path().display()
                );
            }
        }
        match self.place.install_new(&name) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                self.place.remove_staged(&name);
                return Err(anyhow::Error::new(Collision {
                    path: self.place.path().to_path_buf(),
                }));
            }
            Err(e) => {
                self.place.remove_staged(&name);
                return Err(e)
                    .with_context(|| format!("cannot install {}", self.place.path().display()));
            }
        }
        self.place.sync();
        Ok(staged.provisioned())
    }

    /// The new database will not be installed: remove what was staged.
    pub fn discard(self) {
        if let Some((name, _, _)) = &self.staged {
            self.place.remove_staged(name);
        }
    }
}

/// Write the database of `source` next to `dest` (which must not exist),
/// staged under `identity`, without installing it.
pub fn prepare(
    root: &Path,
    dest: &Path,
    source: &Source,
    limits: Limits,
    identity: uuid::Uuid,
) -> Result<Prepared> {
    let place = place_of(root, dest, true)?.context("the directory could not be made")?;
    ensure!(
        place.identity()?.is_none(),
        "{} already exists",
        dest.display()
    );
    let staged = stage(&place, source, limits, identity)?;
    Ok(Prepared { place, staged })
}

/// Write the database of `source` at `dest` (which must not exist), staged
/// under `identity` and installed without replacing anything.
pub fn provision(
    root: &Path,
    dest: &Path,
    source: &Source,
    limits: Limits,
    identity: uuid::Uuid,
) -> Result<Provisioned> {
    prepare(root, dest, source, limits, identity)?.install()
}

/// Replace the database at `dest` with the data of `source`. The new file is
/// staged first; then SQLite's exclusive lock is taken on the old one and kept
/// while its sidecars go and the staged file takes its place, so nothing can
/// start writing to the old file in between.
pub fn replace(
    root: &Path,
    dest: &Path,
    source: &Source,
    limits: Limits,
    identity: uuid::Uuid,
) -> Result<Provisioned> {
    let place = place_of(root, dest, true)?.context("the directory could not be made")?;
    let name = staged_file_name(place.name(), identity);
    // The name the old file is moved aside under: this operation's, so a
    // retry or a recovery finds it ([`clear_staged`]).
    let identity_tag = identity.simple().to_string();
    let staged = stage(&place, source, limits, identity)?;
    let held = match hold(&place, limits.wait) {
        Ok(held) => held,
        Err(error) => {
            place.remove_staged(&name);
            return Err(error);
        }
    };
    seams::run_while_held(place.path());
    let Some((name, staged, identity)) = staged else {
        // An empty start: the old file goes and nothing takes its place.
        match &held {
            Some(held) => {
                let displaced = place.displace(&held.inode, &identity_tag)?;
                place.remove(&displaced)?;
                place.remove_sidecars()?;
            }
            None => place.remove_database()?,
        }
        place.sync();
        drop(held);
        return Ok(Provisioned::nothing());
    };
    let outcome = (|| -> Result<()> {
        match place.identity_of(&name)? {
            Some(now) if now.is_same_file(&identity) => {}
            _ => anyhow::bail!(
                "the file staged for {} changed before it was installed; nothing was replaced",
                place.path().display()
            ),
        }
        if let Some(old) = held.as_ref().map(|held| held.inode) {
            // The file that was locked is moved aside and checked (a different
            // file at the name is put back and the reset refused); then its
            // sidecars go, with its lock held, since beside the new file they
            // would be replayed into it; then the staged file takes the name
            // and the old one goes.
            let displaced = place.displace(&old, &identity_tag)?;
            place.remove_sidecars()?;
            match place.install_new(&name) {
                Ok(()) => {
                    place.remove(&displaced)?;
                    Ok(())
                }
                Err(e) => {
                    // Nothing took its place: the old file goes back if it can.
                    let _ = place.restore(&displaced);
                    Err(install_error(&place, e))
                }
            }
        } else if place.identity()?.is_some() {
            // A file that is no database: nothing locks it and it has no
            // sidecars; the staged file replaces it.
            place
                .replace_with(&name)
                .with_context(|| format!("cannot replace {}", place.path().display()))
        } else {
            // Nothing there to replace: a plain install, which refuses a
            // file that appeared meanwhile.
            place
                .install_new(&name)
                .map_err(|e| install_error(&place, e))
        }
    })();
    drop(held);
    if let Err(error) = outcome {
        place.remove_staged(&name);
        return Err(error);
    }
    place.sync();
    Ok(staged.provisioned())
}

/// An install that did not happen: a file at the name is a [`Collision`].
fn install_error(place: &Place, error: io::Error) -> anyhow::Error {
    if error.kind() == io::ErrorKind::AlreadyExists {
        anyhow::Error::new(Collision {
            path: place.path().to_path_buf(),
        })
    } else {
        anyhow::Error::new(error).context(format!("cannot install {}", place.path().display()))
    }
}

/// Write the new file next to the destination; `None` for an empty start.
fn stage(
    place: &Place,
    source: &Source,
    limits: Limits,
    identity: uuid::Uuid,
) -> Result<Option<(OsString, Staged, Identity)>> {
    let name = staged_file_name(place.name(), identity);
    let path = place.sibling(&name);
    place.remove_staged(&name);
    let made = match source {
        Source::Empty => return Ok(None),
        Source::File { path: from } => {
            let options = Options {
                wait: limits.wait,
                share: limits.share(),
            };
            // A consistent copy is made by SQLite, which opens its destination
            // by path: the directory that path leads to is the held one when
            // the copy begins, and the file it wrote is the held directory's
            // when it ends (below), or it is removed again.
            place.confirm_dir()?;
            seams::run_before_write(place.path());
            let made = match capture::capture(from, &path, &options) {
                Ok(made) => made,
                Err(error) => {
                    place.remove_stray(&name);
                    return Err(anyhow::Error::new(error));
                }
            };
            // As private as the source (a backup creates its file with the
            // default mode), and never read-only: it is the workspace's own.
            // Through the descriptor, on the file the directory holds.
            if place.identity_of(&name)?.is_none() {
                place.remove_stray(&name);
                return Err(confined::changed(place.path()));
            }
            let _ = finish_mode(place, from, &name);
            Staged {
                bytes: made.bytes,
                // An empty file in, an empty database out: nothing was cloned.
                method: (made.bytes > 0).then_some(match made.how {
                    How::Reflink => CloneMethod::Reflink,
                    How::Backup => CloneMethod::Copy,
                }),
                how: Some(made.how),
                waited: made.waited,
            }
        }
        Source::Snapshot { file } => {
            let started = Instant::now();
            seams::run_before_write(place.path());
            // Created and filled through the held directory's descriptor.
            let shared = match place.clone_new(file, &name) {
                Ok(shared) => shared,
                Err(e) => {
                    place.remove_staged(&name);
                    return Err(e).with_context(|| {
                        format!("cannot clone the snapshot at {}", file.display())
                    });
                }
            };
            if !shared && limits.require_cow {
                place.remove_staged(&name);
                return Err(anyhow::Error::new(CaptureError::CannotShare {
                    why: "the snapshot and the workspace are on file systems that do not share \
                          blocks"
                        .into(),
                }));
            }
            // A snapshot is read-only; the database in a workspace is not.
            let bytes = place.finish_file(&name, 0o600)?;
            Staged {
                bytes,
                method: Some(if shared {
                    CloneMethod::Reflink
                } else {
                    CloneMethod::Copy
                }),
                how: None,
                waited: started.elapsed(),
            }
        }
    };
    // The file that was written is the one the directory holds under that
    // name (the directory was not swapped for a link meanwhile).
    match place.confirm_staged(&name) {
        Ok(identity) => Ok(Some((name, made, identity))),
        Err(error) => {
            place.remove_staged(&name);
            Err(error)
        }
    }
}

/// Give the staged file the permissions of `source` (the owner can always
/// write), through the held directory.
#[cfg(unix)]
fn finish_mode(place: &Place, source: &Path, name: &OsStr) -> io::Result<u64> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(source)?.permissions().mode();
    place.finish_file(name, (mode & 0o777) | 0o600)
}

#[cfg(not(unix))]
fn finish_mode(place: &Place, _source: &Path, name: &OsStr) -> io::Result<u64> {
    place.finish_file(name, 0o600)
}

/// Another process keeps the database open.
#[derive(Debug)]
pub struct InUse {
    pub waited: Duration,
}

impl std::fmt::Display for InUse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "another process has the database open (waited {:.1} s)",
            self.waited.as_secs_f64()
        )
    }
}

impl std::error::Error for InUse {}

/// SQLite's lock could not be taken, and not because another process holds
/// it: the file is not replaced or removed on a guess.
#[derive(Debug)]
pub struct CannotLock {
    pub reason: String,
}

impl std::fmt::Display for CannotLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "devflow cannot take SQLite's lock on the database ({}), so it cannot tell whether \
             anything is using it",
            self.reason
        )
    }
}

impl std::error::Error for CannotLock {}

/// SQLite's exclusive lock on a database, held until it is dropped.
pub struct Held {
    conn: Option<Connection>,
    /// The file the directory holds under the database's name, seen before
    /// SQLite opened it and again once its lock was taken: the inode the lock
    /// is on, as far as devflow can establish without SQLite's internals.
    inode: Identity,
}

impl Drop for Held {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            let _ = conn.execute_batch("ROLLBACK");
            persist_wal(&conn);
            let _ = conn.close();
        }
    }
}

/// Closing the last connection of a WAL database makes SQLite checkpoint it
/// and delete `<name>-wal` and `<name>-shm` *by name*. Whoever replaced or
/// removed the database may have a new one at that name by then, so the files
/// are kept: what is deleted is only what devflow itself removed, under the
/// lock.
fn persist_wal(conn: &Connection) {
    // SAFETY: `handle` is the connection's live database handle, the call is
    // a documented file-control opcode taking a pointer to an int, and the
    // connection is not used from another thread.
    unsafe {
        let mut on: std::os::raw::c_int = 1;
        rusqlite::ffi::sqlite3_file_control(
            conn.handle(),
            c"main".as_ptr(),
            rusqlite::ffi::SQLITE_FCNTL_PERSIST_WAL,
            (&mut on as *mut std::os::raw::c_int).cast(),
        );
    }
}

fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(f, _)
            if matches!(f.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

fn is_not_a_database(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(f, _)
            if matches!(f.code, rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt)
    )
}

/// Take SQLite's exclusive lock on the database at `place` within `wait` and
/// keep it. `None` for a file that is not there, or is not a SQLite database
/// (nothing else can be using a text file as one). Another process holding the
/// database is [`InUse`]; any other failure to lock it is [`CannotLock`].
///
/// What this cannot see is a process that has the file open between
/// transactions: SQLite keeps no lock for such a connection.
pub fn hold(place: &Place, wait: Duration) -> Result<Option<Held>> {
    // What the directory holds under the name before SQLite opens it by path.
    let Some(before) = place.identity()? else {
        return Ok(None);
    };
    let started = Instant::now();
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let cannot = |e: &rusqlite::Error| {
        anyhow::Error::new(CannotLock {
            reason: match e {
                rusqlite::Error::SqliteFailure(f, _) => f.to_string(),
                other => other.to_string(),
            },
        })
    };
    let conn = Connection::open_with_flags(place.path(), flags).map_err(|e| cannot(&e))?;
    // A file that cannot be written is opened read-only all the same, and a
    // read-only connection takes a shared lock where it was asked for an
    // exclusive one: it would keep nobody out.
    if conn.is_readonly("main").map_err(|e| cannot(&e))? {
        return Err(anyhow::Error::new(CannotLock {
            reason: "the file is read-only for this user, so SQLite takes no exclusive lock on it"
                .to_string(),
        }));
    }
    conn.busy_timeout(wait).map_err(|e| cannot(&e))?;
    let locked = (|| -> rusqlite::Result<()> {
        // Exclusive locking mode also takes the whole write-ahead-log index
        // in WAL mode, which fails while another connection is reading it.
        conn.query_row("PRAGMA locking_mode=EXCLUSIVE", [], |_| Ok(()))?;
        conn.execute_batch("BEGIN EXCLUSIVE")
    })();
    match locked {
        Ok(()) => {}
        Err(e) if is_busy(&e) => {
            return Err(anyhow::Error::new(InUse {
                waited: started.elapsed(),
            }));
        }
        // The header says it is no database: nothing else can be using it as one.
        Err(e) if is_not_a_database(&e) => return Ok(None),
        Err(e) => return Err(cannot(&e)),
    }
    // The lock is held from here on, whatever the file turns out to be.
    let mut held = Held {
        conn: Some(conn),
        inode: before,
    };
    let schema = held
        .conn
        .as_ref()
        .map(|conn| conn.query_row("SELECT count(*) FROM sqlite_schema", [], |_| Ok(())));
    match schema {
        Some(Ok(())) | None => {}
        Some(Err(e)) if is_busy(&e) => {
            return Err(anyhow::Error::new(InUse {
                waited: started.elapsed(),
            }));
        }
        // A damaged database is exactly what a reset or a drop is for, and it
        // is locked all the same.
        Some(Err(e)) if is_not_a_database(&e) => {}
        Some(Err(e)) => return Err(cannot(&e)),
    }
    // The file the directory holds under the name is the one it held before
    // SQLite opened it: a file renamed over the name in between (so that the
    // lock is on one inode and the name leads to another) is refused, and so
    // is a path that now leads somewhere else.
    let after = place
        .identity()?
        .ok_or_else(|| confined::changed(place.path()))?;
    if !after.is_same_file(&before) {
        return Err(confined::changed(place.path()));
    }
    place.confirm()?;
    held.inode = after;
    Ok(Some(held))
}

/// Remove the database at `dest` and the files SQLite keeps beside it, with
/// SQLite's exclusive lock held while they go. `false` when there was none.
pub fn remove(root: &Path, dest: &Path, wait: Duration) -> Result<bool> {
    let Some(place) = place_of(root, dest, false)? else {
        return Ok(false);
    };
    if place.identity()?.is_none() {
        return Ok(false);
    }
    let held = hold(&place, wait)?;
    seams::run_while_held(place.path());
    match &held {
        // The file that was locked, and only it, goes: it is moved aside and
        // checked before it is removed, then its sidecars.
        Some(held) => {
            let tag = uuid::Uuid::new_v4().simple().to_string();
            let displaced = place.displace(&held.inode, &tag)?;
            place.remove(&displaced)?;
            place.remove_sidecars()?;
        }
        // No database (and so no lock): a file to remove like any other.
        None => place.remove_database()?,
    }
    place.sync();
    drop(held);
    Ok(true)
}

/// What a database file is like now, for listings.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FileStatus {
    pub exists: bool,
    pub bytes: Option<u64>,
    /// `wal` or `rollback` (a rollback journal: delete, truncate or persist),
    /// as the database file's header says; `None` when it is empty or not a
    /// database.
    pub journal_mode: Option<String>,
    /// SQLite's files beside the database that exist now.
    pub sidecars: Vec<String>,
}

/// Look at the database at `path` without changing it. SQLite is not opened:
/// opening a database in WAL mode creates its `-wal` and `-shm` files even
/// for a read, and a listing must leave nothing behind. The mode is read from
/// the file's header (bytes 18 and 19 are 2 in a WAL database).
pub fn status(path: &Path) -> FileStatus {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return FileStatus {
            exists: false,
            bytes: None,
            journal_mode: None,
            sidecars: Vec::new(),
        };
    };
    let journal_mode = if metadata.is_file() {
        header_mode(path)
    } else {
        None
    };
    FileStatus {
        exists: true,
        bytes: Some(metadata.len()),
        journal_mode,
        sidecars: SIDECARS
            .iter()
            .filter(|suffix| sidecar(path, suffix).exists())
            .map(|suffix| suffix.to_string())
            .collect(),
    }
}

fn header_mode(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut header = [0u8; 20];
    fs::File::open(path).ok()?.read_exact(&mut header).ok()?;
    if &header[..16] != capture::HEADER {
        return None;
    }
    Some(if header[18] == 2 && header[19] == 2 {
        "wal".to_string()
    } else {
        "rollback".to_string()
    })
}

#[cfg(test)]
mod tests;
