use super::*;
use rusqlite::Connection;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(10);

fn options(share: Share) -> Options {
    Options { wait: WAIT, share }
}

/// A database with two tables a writer keeps in step: every transaction adds
/// the same number to both, so a copy in which they differ is torn.
fn database(dir: &Path, mode: &str) -> PathBuf {
    let path = dir.join("app.sqlite3");
    let conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "journal_mode", mode).unwrap();
    conn.execute_batch(
        "CREATE TABLE a (n INTEGER PRIMARY KEY, pad TEXT);
         CREATE TABLE b (n INTEGER PRIMARY KEY, pad TEXT);",
    )
    .unwrap();
    path
}

fn pad() -> String {
    "x".repeat(900)
}

fn insert(conn: &Connection, n: i64) -> rusqlite::Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    conn.execute("INSERT INTO a (n, pad) VALUES (?1, ?2)", (n, pad()))?;
    conn.execute("INSERT INTO b (n, pad) VALUES (?1, ?2)", (n, pad()))?;
    conn.execute_batch("COMMIT")
}

/// Rows of both tables in a copy, which must agree, and be a prefix 1..=n.
fn rows(path: &Path) -> i64 {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let check: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(check, "ok", "the copy is corrupt");
    let a: i64 = conn
        .query_row("SELECT count(*) FROM a", [], |r| r.get(0))
        .unwrap();
    let b: i64 = conn
        .query_row("SELECT count(*) FROM b", [], |r| r.get(0))
        .unwrap();
    let max_a: i64 = conn
        .query_row("SELECT coalesce(max(n), 0) FROM a", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        a, b,
        "a copy of two tables that are always written together is torn"
    );
    assert_eq!(a, max_a, "rows are missing from the middle of the copy");
    a
}

fn reflinks(dir: &Path) -> bool {
    crate::cow::probe(dir)
}

#[test]
fn a_quiet_database_is_cloned_where_blocks_are_shared_and_backed_up_elsewhere() {
    for mode in ["delete", "wal"] {
        let dir = tempfile::tempdir().unwrap();
        let source = database(dir.path(), mode);
        let conn = Connection::open(&source).unwrap();
        for n in 1..=50 {
            insert(&conn, n).unwrap();
        }
        // Another connection is open (as an application would have it), with
        // frames still in the log in WAL mode.
        let dest = dir.path().join("copy.sqlite3");
        let made = capture(&source, &dest, &options(Share::Try)).unwrap();
        let expected = if reflinks(dir.path()) {
            How::Reflink
        } else {
            How::Backup
        };
        assert_eq!(made.how, expected, "{mode}");
        assert_eq!(made.bytes, fs::metadata(&dest).unwrap().len());
        // Nothing but the file itself (reading it below makes its own).
        for suffix in SIDECARS {
            assert!(!sidecar(&dest, suffix).exists(), "{mode}: {suffix}");
        }
        assert_eq!(
            rows(&dest),
            50,
            "{mode}: committed rows are all in the copy"
        );
        // The copy is its own database: writing to it leaves the source alone.
        let copy = Connection::open(&dest).unwrap();
        insert(&copy, 51).unwrap();
        drop(copy);
        assert_eq!(rows(&dest), 51);
        assert_eq!(rows(&source), 50);
    }
}

#[test]
fn the_backup_path_is_a_consistent_copy_of_a_write_ahead_log_database() {
    let dir = tempfile::tempdir().unwrap();
    let source = database(dir.path(), "wal");
    let conn = Connection::open(&source).unwrap();
    conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    for n in 1..=200 {
        insert(&conn, n).unwrap();
    }
    // Everything is in the log: the main file alone is nearly empty.
    assert!(sidecar(&source, "-wal").metadata().unwrap().len() > 0);
    let dest = dir.path().join("backup.sqlite3");
    let made = capture(&source, &dest, &options(Share::Never)).unwrap();
    assert_eq!(made.how, How::Backup);
    assert_eq!(rows(&dest), 200);
    let mode: String = Connection::open(&dest)
        .unwrap()
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal", "a copy of a WAL database is one");
}

#[test]
fn a_log_with_frames_is_folded_in_before_the_file_is_cloned() {
    if !reflinks(&std::env::temp_dir()) {
        eprintln!("skipped: the temporary directory does not share blocks");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let source = database(dir.path(), "wal");
    let conn = Connection::open(&source).unwrap();
    conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    for n in 1..=200 {
        insert(&conn, n).unwrap();
    }
    assert!(sidecar(&source, "-wal").metadata().unwrap().len() > 0);
    let dest = dir.path().join("clone.sqlite3");
    let made = capture(&source, &dest, &options(Share::Try)).unwrap();
    assert_eq!(made.how, How::Reflink);
    // The cloned file alone holds all 200 rows: the log was folded in first.
    assert_eq!(rows(&dest), 200);
}

/// Takes the test's hook out when the test ends, however it ends.
struct HookGuard;

impl Drop for HookGuard {
    fn drop(&mut self) {
        seams::BETWEEN_MODE_AND_LOCK.with(|hook| *hook.borrow_mut() = None);
    }
}

#[test]
fn a_database_that_enters_the_log_before_the_lock_is_held_is_copied_with_its_log() {
    // Only a clone trusts the file alone: a backup reads through the log.
    if !reflinks(&std::env::temp_dir()) {
        eprintln!("skipped: the temporary directory does not share blocks");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let source = database(dir.path(), "delete");
    let conn = Connection::open(&source).unwrap();
    for n in 1..=20 {
        insert(&conn, n).unwrap();
    }
    drop(conn);

    // Between the moment the mode was read (a rollback journal) and the moment
    // the lock is asked for, an application switches to the write-ahead log
    // and writes rows that stay in it.
    let application: Arc<std::sync::Mutex<Option<Connection>>> = Arc::default();
    let (slot, path) = (application.clone(), source.clone());
    let mut once = true;
    seams::BETWEEN_MODE_AND_LOCK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            if !std::mem::take(&mut once) {
                return;
            }
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "journal_mode", "wal").unwrap();
            conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
            for n in 21..=30 {
                insert(&conn, n).unwrap();
            }
            assert!(
                path.with_file_name("app.sqlite3-wal")
                    .metadata()
                    .unwrap()
                    .len()
                    > 0
            );
            *slot.lock().unwrap() = Some(conn);
        }));
    });
    let _guard = HookGuard;
    let dest = dir.path().join("clone.sqlite3");
    let made = capture(&source, &dest, &options(Share::Try)).unwrap();
    assert!(application.lock().unwrap().is_some(), "the application ran");
    assert_eq!(made.how, How::Reflink);
    // The 10 rows that were only in the log are in the clone.
    assert_eq!(rows(&dest), 30);
}

#[test]
fn copies_made_while_the_journal_mode_is_switched_back_and_forth_hold_every_committed_row() {
    let dir = tempfile::tempdir().unwrap();
    let source = database(dir.path(), "delete");
    let committed = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let switcher = {
        let (path, committed, stop) = (source.clone(), committed.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut next = 1i64;
            let mut round = 0;
            while !stop.load(Ordering::Relaxed) {
                round += 1;
                let Ok(conn) = Connection::open(&path) else {
                    continue;
                };
                let _ = conn.busy_timeout(Duration::from_millis(200));
                // Refused while another connection holds the database; the
                // mode is whatever it is, which is the point.
                let mode = if round % 2 == 0 { "delete" } else { "wal" };
                let _ = conn.pragma_update(None, "wal_autocheckpoint", 0);
                let _ = conn.pragma_update(None, "journal_mode", mode);
                for _ in 0..3 {
                    if insert(&conn, next).is_ok() {
                        committed.store(next as u64, Ordering::SeqCst);
                        next += 1;
                    }
                }
            }
        })
    };
    let mut copies = 0;
    let mut reflinked = 0;
    for round in 0..40 {
        let floor = committed.load(Ordering::SeqCst) as i64;
        let dest = dir.path().join(format!("copy-{round}.sqlite3"));
        match capture(&source, &dest, &options(Share::Try)) {
            Ok(made) => {
                let found = rows(&dest);
                assert!(
                    found >= floor,
                    "a copy has {found} rows, but {floor} were committed before it was asked for \
                     ({:?})",
                    made.how
                );
                copies += 1;
                reflinked += usize::from(made.how == How::Reflink);
            }
            Err(Error::Locked { .. }) => {}
            Err(other) => panic!("{other}"),
        }
    }
    stop.store(true, Ordering::Relaxed);
    switcher.join().unwrap();
    assert!(copies > 10, "only {copies} copies were made");
    eprintln!("{copies} copies, {reflinked} of them reflinks");
}

/// Run `writers` threads that add paired rows until told to stop.
struct Writers {
    stop: Arc<AtomicBool>,
    next: Arc<AtomicU64>,
    handles: Vec<std::thread::JoinHandle<()>>,
}

impl Writers {
    fn start(path: &Path, threads: usize, pause: Duration) -> Writers {
        let stop = Arc::new(AtomicBool::new(false));
        let next = Arc::new(AtomicU64::new(1));
        let handles = (0..threads)
            .map(|_| {
                let (path, stop, next) = (path.to_path_buf(), stop.clone(), next.clone());
                std::thread::spawn(move || {
                    let conn = Connection::open(&path).unwrap();
                    conn.busy_timeout(Duration::from_secs(30)).unwrap();
                    while !stop.load(Ordering::Relaxed) {
                        // A busy application, not a loop that never lets go.
                        std::thread::sleep(pause);
                        // Ids are handed out under the write lock, so rows
                        // 1..=n are always all there once nothing is in flight.
                        if conn.execute_batch("BEGIN IMMEDIATE").is_err() {
                            continue;
                        }
                        let n = next.fetch_add(1, Ordering::SeqCst) as i64;
                        let ok = conn
                            .execute("INSERT INTO a (n, pad) VALUES (?1, ?2)", (n, pad()))
                            .and_then(|_| {
                                conn.execute("INSERT INTO b (n, pad) VALUES (?1, ?2)", (n, pad()))
                            })
                            .is_ok();
                        if ok {
                            let _ = conn.execute_batch("COMMIT");
                        } else {
                            let _ = conn.execute_batch("ROLLBACK");
                        }
                    }
                })
            })
            .collect();
        Writers {
            stop,
            next,
            handles,
        }
    }

    fn stop(self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        for handle in self.handles {
            handle.join().unwrap();
        }
        self.next.load(Ordering::SeqCst)
    }
}

/// Copy `rounds` times beside writers that never stop; every copy must be a
/// state the database was in.
fn copies_beside_writers(threads: usize, pause: Duration, rounds: usize) {
    for mode in ["delete", "wal"] {
        for share in [Share::Try, Share::Never] {
            let dir = tempfile::tempdir().unwrap();
            let source = database(dir.path(), mode);
            let writers = Writers::start(&source, threads, pause);
            let mut cloned = 0;
            for round in 0..rounds {
                let dest = dir.path().join(format!("copy-{round}.sqlite3"));
                let made = capture(&source, &dest, &options(share)).unwrap_or_else(|e| {
                    panic!("{mode} {share:?} round {round}: {e}");
                });
                cloned += usize::from(made.how == How::Reflink);
                rows(&dest);
                std::thread::sleep(Duration::from_millis(10));
            }
            let total = writers.stop();
            assert!(
                total > rounds as u64,
                "{mode} {share:?}: writers made progress"
            );
            if share == Share::Try && reflinks(dir.path()) && pause >= Duration::from_millis(1) {
                // Beside writers that do pause, the quiesced file is cloned:
                // the invariant held for the reflinks too, not only for backups.
                assert!(cloned > 0, "{mode}: no copy was a reflink");
            }
            if share == Share::Never {
                assert_eq!(cloned, 0, "{mode}: a backup was asked for");
            }
        }
    }
}

#[test]
fn no_copy_made_beside_busy_writers_is_torn() {
    // The invariant: `a` and `b` always have the same rows, 1..=n. Writers
    // run the whole time; every copy, by either way and in either journal
    // mode, must hold a state the database was in.
    copies_beside_writers(2, Duration::from_millis(4), 6);
}

#[test]
#[ignore = "stress: writers that hardly pause; takes a minute"]
fn no_copy_made_beside_saturating_writers_is_torn() {
    copies_beside_writers(4, Duration::from_micros(100), 25);
}

#[test]
fn a_rollback_journal_source_locked_for_writing_is_waited_for_then_refused() {
    let dir = tempfile::tempdir().unwrap();
    let source = database(dir.path(), "delete");
    let holder = Connection::open(&source).unwrap();
    insert(&holder, 1).unwrap();
    // A transaction in flight: the file itself is mid-change, and nothing can
    // read it until the writer commits.
    holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
    holder
        .execute("INSERT INTO a (n, pad) VALUES (2, ?1)", (pad(),))
        .unwrap();
    let dest = dir.path().join("copy.sqlite3");
    let started = Instant::now();
    let error = capture(
        &source,
        &dest,
        &Options {
            wait: Duration::from_millis(600),
            share: Share::Try,
        },
    )
    .map(|made| panic!("copied beside a writer in flight: {made:?}"))
    .unwrap_err();
    let waited = started.elapsed();
    assert!(matches!(error, Error::Locked { .. }), "{error}");
    assert!(
        waited >= Duration::from_millis(550),
        "gave up after {waited:?}"
    );
    assert!(waited < Duration::from_secs(5), "{waited:?}");
    assert!(!dest.exists(), "nothing is left at the destination");
    let leftovers: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("copy"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // The writer finishes within the bound: the copy waits for it, then holds
    // the state it committed (rows 1 and 2 of both tables).
    holder
        .execute("INSERT INTO b (n, pad) VALUES (2, ?1)", (pad(),))
        .unwrap();
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        holder.execute_batch("COMMIT").unwrap();
        holder
    });
    let made = capture(&source, &dest, &options(Share::Try)).unwrap();
    let _holder = releaser.join().unwrap();
    assert!(
        made.waited >= Duration::from_millis(200),
        "{:?}",
        made.waited
    );
    assert_eq!(rows(&dest), 2);
}

#[test]
fn a_write_ahead_log_source_mid_transaction_is_copied_as_last_committed() {
    // In WAL mode a writer in flight holds nobody up from reading: the copy
    // is the last committed state (never a piece of the transaction), taken by
    // the backup once cloning the quiesced file had to give way.
    let dir = tempfile::tempdir().unwrap();
    let source = database(dir.path(), "wal");
    let holder = Connection::open(&source).unwrap();
    insert(&holder, 1).unwrap();
    holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
    holder
        .execute("INSERT INTO a (n, pad) VALUES (2, ?1)", (pad(),))
        .unwrap();
    let dest = dir.path().join("copy.sqlite3");
    let made = capture(
        &source,
        &dest,
        &Options {
            wait: Duration::from_secs(5),
            share: Share::Try,
        },
    )
    .unwrap();
    assert_eq!(made.how, How::Backup);
    assert_eq!(rows(&dest), 1, "only what was committed");
    // A reflink was required: that has to wait for the writer, and is refused
    // when it does not finish in time.
    let other = dir.path().join("required.sqlite3");
    let error = capture(
        &source,
        &other,
        &Options {
            wait: Duration::from_millis(500),
            share: Share::Require,
        },
    )
    .unwrap_err();
    assert!(
        matches!(error, Error::Locked { .. } | Error::CannotShare { .. }),
        "{error}"
    );
    assert!(!other.exists());
    holder.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn what_is_not_a_database_or_not_there_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("copy.sqlite3");
    assert!(matches!(
        capture(
            &dir.path().join("none.sqlite3"),
            &dest,
            &options(Share::Try)
        ),
        Err(Error::Missing)
    ));
    let text = dir.path().join("text.sqlite3");
    fs::write(
        &text,
        "this is not a database, it only says so in its name\n",
    )
    .unwrap();
    assert!(matches!(
        capture(&text, &dest, &options(Share::Try)),
        Err(Error::NotADatabase)
    ));
    assert!(!dest.exists());
    // A directory, and a link (which is never followed).
    assert!(matches!(
        capture(dir.path(), &dest, &options(Share::Try)),
        Err(Error::NotADatabase)
    ));
    #[cfg(unix)]
    {
        let real = database(dir.path(), "delete");
        let link = dir.path().join("link.sqlite3");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(matches!(
            capture(&link, &dest, &options(Share::Try)),
            Err(Error::NotADatabase)
        ));
    }
    // An existing destination is never touched.
    let real = dir.path().join("app.sqlite3");
    fs::write(&dest, "precious").unwrap();
    assert!(matches!(
        capture(&real, &dest, &options(Share::Try)),
        Err(Error::Exists)
    ));
    assert_eq!(fs::read_to_string(&dest).unwrap(), "precious");
}

#[test]
fn an_empty_file_is_an_empty_database() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("fresh.sqlite3");
    fs::write(&source, "").unwrap();
    let dest = dir.path().join("copy.sqlite3");
    let made = capture(&source, &dest, &options(Share::Try)).unwrap();
    assert_eq!(made.bytes, 0);
    assert_eq!(fs::metadata(&dest).unwrap().len(), 0);
}

#[cfg(unix)]
#[test]
fn a_reflink_that_cannot_be_had_is_refused_when_required() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let source = database(dir.path(), "delete");
    insert(&Connection::open(&source).unwrap(), 1).unwrap();
    // A read-only file cannot be locked for writing, so it cannot be cloned
    // in place: it is backed up, or refused when a reflink is required.
    fs::set_permissions(&source, fs::Permissions::from_mode(0o444)).unwrap();
    if Connection::open_with_flags(&source, OpenFlags::SQLITE_OPEN_READ_WRITE).is_ok() {
        eprintln!("skipped: the file can still be opened for writing (running as root)");
        return;
    }
    let dest = dir.path().join("copy.sqlite3");
    let error = capture(&source, &dest, &options(Share::Require)).unwrap_err();
    assert!(matches!(error, Error::CannotShare { .. }), "{error}");
    assert!(!dest.exists());
    let made = capture(&source, &dest, &options(Share::Try)).unwrap();
    assert_eq!(made.how, How::Backup);
    assert_eq!(rows(&dest), 1);
}

#[test]
fn what_makes_the_database_itself_survives_either_way_of_copying() {
    // The header's settings are the database's own: page size, the
    // application's version numbers, its journal mode and encoding.
    for mode in ["delete", "wal"] {
        for share in [Share::Try, Share::Never] {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("app.sqlite3");
            let conn = Connection::open(&source).unwrap();
            conn.pragma_update(None, "page_size", 8192).unwrap();
            conn.pragma_update(None, "journal_mode", mode).unwrap();
            conn.pragma_update(None, "user_version", 42).unwrap();
            conn.pragma_update(None, "application_id", 0x0DEF_1055_i64)
                .unwrap();
            conn.execute_batch(
                "CREATE TABLE big (n INTEGER PRIMARY KEY, pad BLOB);
                 CREATE INDEX big_pad ON big (length(pad));
                 CREATE VIEW v AS SELECT n FROM big;
                 CREATE TRIGGER t AFTER INSERT ON big BEGIN SELECT 1; END;",
            )
            .unwrap();
            conn.execute_batch("BEGIN").unwrap();
            for n in 1..=3000 {
                conn.execute("INSERT INTO big VALUES (?1, zeroblob(2000))", [n])
                    .unwrap();
            }
            conn.execute_batch("COMMIT").unwrap();
            let dest = dir.path().join("copy.sqlite3");
            capture(&source, &dest, &options(share)).unwrap();
            let copy = Connection::open(&dest).unwrap();
            let pragma = |name: &str| -> i64 {
                copy.query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))
                    .unwrap()
            };
            assert_eq!(pragma("page_size"), 8192, "{mode} {share:?}");
            assert_eq!(pragma("user_version"), 42);
            assert_eq!(pragma("application_id"), 0x0DEF_1055);
            let journal: String = copy
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .unwrap();
            assert_eq!(journal, mode, "{share:?}");
            let check: String = copy
                .query_row("PRAGMA integrity_check", [], |r| r.get(0))
                .unwrap();
            assert_eq!(check, "ok");
            let count: i64 = copy
                .query_row("SELECT count(*) FROM v", [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 3000);
            let objects: i64 = copy
                .query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))
                .unwrap();
            assert_eq!(objects, 4, "table, index, view and trigger");
        }
    }
}
