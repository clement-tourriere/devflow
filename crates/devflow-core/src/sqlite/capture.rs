//! A consistent copy of a SQLite database that other processes may be using.
//!
//! Copying a database file while somebody writes to it can produce a file that
//! never existed: pages of two states, or a main file without the log that
//! completes it. A torn copy is never acceptable, so a copy is made in one of
//! two ways, both under SQLite's own locks, and the second is the way out
//! whenever the first is not provably safe:
//!
//! 1. **Quiesce and clone.** The database is opened through SQLite and its
//!    write lock is taken (`BEGIN EXCLUSIVE`, which in a rollback-journal
//!    database also keeps readers out). In WAL mode the log is first folded
//!    into the main file and truncated, and the lock is only trusted once the
//!    log is seen to be empty while it is held: from then on no process can
//!    append to the log or move pages into the file, so the file *is* the
//!    database. It is cloned with a reflink (one metadata operation, no blocks
//!    written, shared until either side changes) and the lock is released.
//!    Only a reflink is accepted here: a byte copy under the lock would hold
//!    writers up for as long as it takes to read the file.
//! 2. **Back up.** SQLite's online backup API (`sqlite3_backup_step` with all
//!    pages at once) reads the database inside one read transaction and writes
//!    a new file. It is consistent whatever the other processes do and needs no
//!    write lock, but every page is written again, so the result shares no
//!    blocks with the source.
//!
//! A source that a writer holds locked is waited for, up to the bound the
//! caller gives, and refused after that ([`Error::Locked`]); neither way ever
//! copies while the file can change.
use rusqlite::{Connection, ErrorCode, OpenFlags};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The first 16 bytes of every SQLite database.
pub const HEADER: &[u8; 16] = b"SQLite format 3\0";

/// How long to sleep between looks at a log a writer keeps appending to.
const POLL: Duration = Duration::from_millis(25);
/// How long cloning the quiesced file waits for the write lock before giving
/// way to the backup, which a writer in the middle of a transaction does not
/// hold up in WAL mode. (A reflink that is required, or a rollback-journal
/// database, in which a backup waits for the writer too, wait the whole bound.)
const PATIENCE: Duration = Duration::from_millis(500);

/// What the caller wants of the blocks of the copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Share {
    /// A reflink when it is provably safe, else a backup.
    Try,
    /// A reflink, or no copy: the caller refuses full copies (`require_cow`).
    Require,
    /// Always back up (the way out of [`Share::Try`], exercised on its own).
    Never,
}

/// The bounds of one capture.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// How long a writer may keep the source locked before the capture is
    /// refused.
    pub wait: Duration,
    pub share: Share,
}

/// How the bytes reached the copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum How {
    /// The quiesced file was cloned: the blocks are shared.
    Reflink,
    /// SQLite's backup API wrote a new file.
    Backup,
}

/// What a capture made.
#[derive(Debug, Clone, Copy)]
pub struct Captured {
    pub how: How,
    /// Size of the copy.
    pub bytes: u64,
    /// How long the capture waited for a writer.
    pub waited: Duration,
}

#[derive(Debug)]
pub enum Error {
    /// There is no file at the source.
    Missing,
    /// The source is not a regular file (a link is never followed) or does not
    /// start like a SQLite database.
    NotADatabase,
    /// A writer held the source locked for the whole bound.
    Locked {
        waited: Duration,
    },
    /// A reflink was required and the copy could only have been a full one;
    /// `why` says what ruled the reflink out.
    CannotShare {
        why: String,
    },
    /// The destination exists already.
    Exists,
    Io(io::Error),
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Missing => f.write_str("the database file does not exist"),
            Error::NotADatabase => f.write_str(
                "the file is not a SQLite database (or is a link, which is not followed)",
            ),
            Error::Locked { waited } => write!(
                f,
                "another process holds the database locked for writing; waited {:.1} s",
                waited.as_secs_f64()
            ),
            Error::CannotShare { why } => write!(
                f,
                "the database cannot be cloned copy-on-write ({why}), and a full copy is not \
                 allowed"
            ),
            Error::Exists => f.write_str("the destination already exists"),
            Error::Io(e) => write!(f, "{e}"),
            Error::Sqlite(e) => write!(f, "SQLite: {}", reason(e)),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Sqlite(e)
    }
}

/// What SQLite says went wrong, without anything the statement held.
fn reason(error: &rusqlite::Error) -> String {
    match error {
        rusqlite::Error::SqliteFailure(failure, _) => failure.to_string(),
        other => other.to_string(),
    }
}

fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if matches!(failure.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

fn is_not_a_database(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if matches!(failure.code, ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt)
    )
}

/// Wrap a SQLite error: busy is [`Error::Locked`], a file that is not a
/// database says so.
fn classify(error: rusqlite::Error, since: Instant) -> Error {
    if is_busy(&error) {
        Error::Locked {
            waited: since.elapsed(),
        }
    } else if is_not_a_database(&error) {
        Error::NotADatabase
    } else {
        Error::Sqlite(error)
    }
}

/// `path` with `suffix` appended to its file name (`db.sqlite` → `db.sqlite-wal`).
pub fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// What SQLite keeps beside a database file.
pub use super::confined::SIDECARS;

/// Write a consistent copy of the database at `source` to `dest`, which must
/// not exist (the caller names it uniquely: an operation's staging name).
/// On an error after the copy was begun nothing is left at `dest`; a
/// destination that exists is refused and never touched.
pub fn capture(source: &Path, dest: &Path, options: &Options) -> Result<Captured, Error> {
    let started = Instant::now();
    let deadline = started + options.wait;
    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(Error::Missing),
        Err(e) => return Err(e.into()),
    };
    if !metadata.is_file() {
        return Err(Error::NotADatabase);
    }
    if fs::symlink_metadata(dest).is_ok() {
        return Err(Error::Exists);
    }
    let result = capture_within(source, dest, metadata.len(), options, started, deadline);
    if result.is_err() {
        // `dest` did not exist when this began, and is this call's to remove.
        remove_with_sidecars(dest);
    }
    result
}

fn capture_within(
    source: &Path,
    dest: &Path,
    len: u64,
    options: &Options,
    started: Instant,
    deadline: Instant,
) -> Result<Captured, Error> {
    if len == 0 {
        // An empty file is an empty database: SQLite starts one in it.
        create_empty(dest)?;
        return Ok(Captured {
            how: How::Backup,
            bytes: 0,
            waited: started.elapsed(),
        });
    }
    check_header(source)?;

    let mut why_not_shared = None;
    if options.share != Share::Never {
        // Required, the reflink is all there is to wait for; otherwise a
        // backup is the way out when the write lock is not to be had soon.
        let fast_deadline = match options.share {
            Share::Require => deadline,
            _ => deadline.min(started + PATIENCE),
        };
        match quiesced_reflink(source, dest, started, fast_deadline, deadline)? {
            Fast::Done(bytes) => {
                return Ok(Captured {
                    how: How::Reflink,
                    bytes,
                    waited: started.elapsed(),
                });
            }
            Fast::Skipped(why) => why_not_shared = Some(why),
        }
    }
    if options.share == Share::Require {
        return Err(Error::CannotShare {
            why: why_not_shared.unwrap_or_else(|| "a backup was asked for".into()),
        });
    }
    let bytes = backup(source, dest, started, deadline)?;
    Ok(Captured {
        how: How::Backup,
        bytes,
        waited: started.elapsed(),
    })
}

fn create_empty(dest: &Path) -> Result<(), Error> {
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .map_err(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                Error::Exists
            } else {
                e.into()
            }
        })?;
    file.sync_all()?;
    Ok(())
}

fn check_header(source: &Path) -> Result<(), Error> {
    let mut header = [0u8; 16];
    let mut file = fs::File::open(source)?;
    match file.read_exact(&mut header) {
        Ok(()) if &header == HEADER => Ok(()),
        Ok(()) => Err(Error::NotADatabase),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Err(Error::NotADatabase),
        Err(e) => Err(e.into()),
    }
}

fn left(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

enum Fast {
    Done(u64),
    /// The file cannot be cloned in place under a lock; why.
    Skipped(String),
}

/// Quiesce the database and clone its file with a reflink; see the module
/// documentation. `Skipped` says why it cannot be done here (and leaves
/// nothing at `dest`), `Err` that the source stayed locked for the whole wait.
fn quiesced_reflink(
    source: &Path,
    dest: &Path,
    started: Instant,
    deadline: Instant,
    last: Instant,
) -> Result<Fast, Error> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = match Connection::open_with_flags(source, flags) {
        Ok(conn) => conn,
        Err(e) if is_busy(&e) => return busy_open(e, started, last),
        Err(e) if is_not_a_database(&e) => return Err(Error::NotADatabase),
        Err(e) => {
            return Ok(Fast::Skipped(format!(
                "the file cannot be opened for writing: {}",
                reason(&e)
            )));
        }
    };
    // Busy within `deadline` gives way to the backup unless this is already
    // the last of the bound (`last`), when the source is refused.
    let busy = |e: rusqlite::Error| -> Result<Fast, Error> {
        if is_busy(&e) && Instant::now() < last {
            Ok(Fast::Skipped("a writer holds the database".into()))
        } else {
            Err(classify(e, started))
        }
    };
    let timeout = |conn: &Connection| conn.busy_timeout(left(deadline));
    timeout(&conn)?;
    // What the database is in *now*, without a lock: another connection can
    // switch it (to or from the write-ahead log) before this one has the lock,
    // so it is established again once the lock is held, below.
    let journal_is_wal = |conn: &Connection| -> rusqlite::Result<bool> {
        let mode: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        Ok(mode.eq_ignore_ascii_case("wal"))
    };
    let mut wal = match journal_is_wal(&conn) {
        Ok(wal) => wal,
        Err(e) => return busy(e),
    };
    let log = sidecar(source, "-wal");

    loop {
        timeout(&conn)?;
        if wal {
            // Fold the log into the file and cut it to nothing. A reader that
            // keeps it in use makes this report busy; wait for it like for a
            // writer.
            let blocked: i64 =
                match conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0)) {
                    Ok(blocked) => blocked,
                    Err(e) => return busy(e),
                };
            if blocked != 0 {
                if Instant::now() >= deadline {
                    return Ok(Fast::Skipped(
                        "other connections keep the write-ahead log in use".into(),
                    ));
                }
                std::thread::sleep(POLL);
                continue;
            }
        }
        timeout(&conn)?;
        seams::between_mode_and_lock();
        if let Err(e) = conn.execute_batch("BEGIN EXCLUSIVE") {
            return busy(e);
        }
        // The mode the database is in is only settled with the lock held. A
        // database that went into the write-ahead log since it was looked at
        // has frames the file does not: they were not folded in, and the file
        // alone is not the database. One that left it has no log to fold.
        // Either way the lock is let go and the steps for the mode it is in
        // now are taken.
        let locked_wal = match journal_is_wal(&conn) {
            Ok(wal) => wal,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return busy(e);
            }
        };
        if locked_wal != wal {
            let _ = conn.execute_batch("ROLLBACK");
            wal = locked_wal;
            if Instant::now() >= deadline {
                return Ok(Fast::Skipped(
                    "the journal mode of the database keeps changing".into(),
                ));
            }
            continue;
        }
        // In a rollback-journal database this lock has already kept readers
        // out and rolled back a journal a crash left behind. In WAL mode it
        // keeps writers out; the log must be empty *now*, with the lock held,
        // for the file to be the whole database.
        if wal && log_has_frames(&log)? {
            let _ = conn.execute_batch("ROLLBACK");
            if Instant::now() >= deadline {
                return Ok(Fast::Skipped("a writer keeps appending to the log".into()));
            }
            std::thread::sleep(POLL);
            continue;
        }
        break;
    }

    // The file cannot change until the lock is released.
    let outcome = reflink_copy::reflink(source, dest);
    let _ = conn.execute_batch("ROLLBACK");
    drop(conn);
    match outcome {
        Ok(()) => {
            let (from, to) = (fs::metadata(source)?.len(), fs::metadata(dest)?.len());
            if from != to {
                remove_with_sidecars(dest);
                return Ok(Fast::Skipped(
                    "the clone did not have the size of the file".into(),
                ));
            }
            fs::File::open(dest)?.sync_all()?;
            Ok(Fast::Done(to))
        }
        // Not ours to remove: somebody made it meanwhile.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(Error::Exists),
        Err(e) => {
            // The destination is ours: whatever a failed clone left goes.
            remove_with_sidecars(dest);
            Ok(Fast::Skipped(format!(
                "the file system does not share blocks between these files ({})",
                e.kind()
            )))
        }
    }
}

/// Moments a test acts at, as another process would.
mod seams {
    #[cfg(test)]
    thread_local! {
        pub(super) static BETWEEN_MODE_AND_LOCK: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Right before the exclusive lock is asked for, with the journal mode of
    /// the database as it was last looked at.
    pub(super) fn between_mode_and_lock() {
        #[cfg(test)]
        BETWEEN_MODE_AND_LOCK.with(|hook| {
            if let Some(hook) = hook.borrow_mut().as_mut() {
                hook();
            }
        });
    }
}

/// Opening was refused as busy.
fn busy_open(e: rusqlite::Error, started: Instant, last: Instant) -> Result<Fast, Error> {
    if Instant::now() < last {
        Ok(Fast::Skipped("a writer holds the database".into()))
    } else {
        Err(classify(e, started))
    }
}

fn log_has_frames(log: &Path) -> Result<bool, Error> {
    match fs::metadata(log) {
        Ok(metadata) => Ok(metadata.len() > 0),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// SQLite's online backup of the whole database in one step.
fn backup(source: &Path, dest: &Path, started: Instant, deadline: Instant) -> Result<u64, Error> {
    let src = open_for_backup(source, started)?;
    src.busy_timeout(left(deadline))?;
    let mut dst = Connection::open_with_flags(
        dest,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    dst.busy_timeout(Duration::from_secs(5))?;
    {
        let backup = rusqlite::backup::Backup::new(&src, &mut dst)?;
        loop {
            // All pages in one step: one read transaction, so what is copied
            // is one state of the database. (Stepping a few pages at a time
            // restarts whenever another connection writes, and never finishes
            // beside a busy writer.)
            match backup.step(-1) {
                Ok(rusqlite::backup::StepResult::Done) => break,
                Ok(rusqlite::backup::StepResult::More) => {}
                Ok(_) => {
                    // Busy or locked: a writer holds the source.
                    if Instant::now() >= deadline {
                        return Err(Error::Locked {
                            waited: started.elapsed(),
                        });
                    }
                    std::thread::sleep(POLL);
                }
                Err(e) => return Err(classify(e, started)),
            }
        }
    }
    dst.close().map_err(|(_, e)| classify(e, started))?;
    src.close().map_err(|(_, e)| classify(e, started))?;
    // Nothing of the copy may live anywhere but in the one file.
    for suffix in SIDECARS {
        if sidecar(dest, suffix).exists() {
            return Err(Error::Io(io::Error::other(format!(
                "the backup left {suffix} beside the copy"
            ))));
        }
    }
    fs::File::open(dest)?.sync_all()?;
    Ok(fs::metadata(dest)?.len())
}

/// Open the source for the backup: read-only when SQLite allows it (the
/// source is not changed), else for writing (a journal a crash left behind
/// can only be rolled back by a connection that may write).
fn open_for_backup(source: &Path, started: Instant) -> Result<Connection, Error> {
    let read_only = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    match Connection::open_with_flags(source, read_only) {
        Ok(conn) => Ok(conn),
        Err(e) if is_busy(&e) || is_not_a_database(&e) => Err(classify(e, started)),
        Err(_) => Connection::open_with_flags(
            source,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| classify(e, started)),
    }
}

/// Remove a database file and the files SQLite keeps beside it.
pub fn remove_with_sidecars(path: &Path) {
    let _ = fs::remove_file(path);
    for suffix in SIDECARS {
        let _ = fs::remove_file(sidecar(path, suffix));
    }
}

#[cfg(test)]
mod tests;
