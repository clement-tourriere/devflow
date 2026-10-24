use super::*;
use rusqlite::Connection;
use uuid::Uuid;

fn database(path: &Path, rows: i64) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let conn = Connection::open(path).unwrap();
    conn.execute_batch("CREATE TABLE t (n INTEGER PRIMARY KEY, v TEXT)")
        .unwrap();
    for n in 1..=rows {
        conn.execute("INSERT INTO t (n, v) VALUES (?1, 'row')", [n])
            .unwrap();
    }
}

fn count(path: &Path) -> i64 {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap()
        .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
        .unwrap()
}

fn limits() -> Limits {
    Limits {
        wait: Duration::from_millis(500),
        require_cow: false,
    }
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn a_file_is_cloned_into_a_new_directory_and_reported() {
    let dir = tempfile::tempdir().unwrap();
    let (source, root) = (dir.path().join("src/app.sqlite3"), dir.path().join("ws"));
    std::fs::create_dir_all(&root).unwrap();
    database(&source, 20);
    let dest = root.join("db/nested/app.sqlite3");
    let id = Uuid::new_v4();
    let made = provision(
        &root,
        &dest,
        &Source::File {
            path: source.clone(),
        },
        limits(),
        id,
    )
    .unwrap();
    assert_eq!(count(&dest), 20);
    assert_eq!(made.bytes, std::fs::metadata(&dest).unwrap().len());
    let expected = if crate::cow::probe(dir.path()) {
        CloneMethod::Reflink
    } else {
        CloneMethod::Copy
    };
    assert_eq!(made.method, Some(expected));
    // No staging file is left, and the source is untouched.
    assert_eq!(names(dest.parent().unwrap()), ["app.sqlite3"]);
    assert_eq!(count(&source), 20);
    // The clone is the workspace's own database.
    Connection::open(&dest)
        .unwrap()
        .execute("INSERT INTO t (n, v) VALUES (100, 'mine')", [])
        .unwrap();
    assert_eq!(count(&dest), 21);
    assert_eq!(count(&source), 20);
}

#[test]
fn an_existing_destination_is_never_replaced_by_provisioning() {
    let dir = tempfile::tempdir().unwrap();
    let (source, root) = (dir.path().join("src.sqlite3"), dir.path().join("ws"));
    std::fs::create_dir_all(&root).unwrap();
    database(&source, 3);
    let dest = root.join("app.sqlite3");
    std::fs::write(&dest, "mine").unwrap();
    let error = provision(
        &root,
        &dest,
        &Source::File { path: source },
        limits(),
        Uuid::new_v4(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("already exists"), "{error:#}");
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "mine");
    assert_eq!(names(&root), ["app.sqlite3"]);
}

#[cfg(unix)]
#[test]
fn a_symlinked_directory_in_the_path_is_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let (source, root) = (dir.path().join("src.sqlite3"), dir.path().join("ws"));
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    database(&source, 1);
    std::os::unix::fs::symlink(&outside, root.join("db")).unwrap();
    let dest = root.join("db/app.sqlite3");
    let error = provision(
        &root,
        &dest,
        &Source::File { path: source },
        limits(),
        Uuid::new_v4(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("symbolic link"), "{error:#}");
    assert!(names(&outside).is_empty());
}

#[test]
fn nothing_cloned_starts_without_a_file_but_with_its_directory() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();
    let dest = root.join("db/app.sqlite3");
    let made = provision(&root, &dest, &Source::Empty, limits(), Uuid::new_v4()).unwrap();
    assert_eq!(made.method, None);
    assert_eq!(made.bytes, 0);
    assert!(!dest.exists());
    assert!(dest.parent().unwrap().is_dir());
}

#[test]
fn a_snapshot_is_cloned_as_a_writable_database() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();
    let snapshot = dir.path().join("snapshot.sqlite");
    database(&snapshot, 7);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&snapshot, std::fs::Permissions::from_mode(0o400)).unwrap();
    }
    let dest = root.join("app.sqlite3");
    let made = provision(
        &root,
        &dest,
        &Source::Snapshot {
            file: snapshot.clone(),
        },
        limits(),
        Uuid::new_v4(),
    )
    .unwrap();
    assert!(made.method.is_some());
    assert!(made.how.is_none());
    assert_eq!(count(&dest), 7);
    Connection::open(&dest)
        .unwrap()
        .execute("INSERT INTO t (n, v) VALUES (8, 'new')", [])
        .unwrap();
    assert_eq!(count(&snapshot), 7, "the snapshot is untouched");
    // A full copy is refused when blocks must be shared and cannot be.
    if !crate::cow::probe(dir.path()) {
        let other = root.join("other.sqlite3");
        let strict = Limits {
            require_cow: true,
            ..limits()
        };
        let error = provision(
            &root,
            &other,
            &Source::Snapshot { file: snapshot },
            strict,
            Uuid::new_v4(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("copy-on-write"), "{error:#}");
        assert!(!other.exists());
    }
}

#[test]
fn replacing_swaps_the_data_and_removes_the_old_sidecars() {
    let dir = tempfile::tempdir().unwrap();
    let (source, root) = (dir.path().join("src.sqlite3"), dir.path().join("ws"));
    std::fs::create_dir_all(&root).unwrap();
    database(&source, 5);
    let dest = root.join("app.sqlite3");
    database(&dest, 99);
    // What a crashed WAL-mode process of the old database left: replayed
    // beside the new file it would corrupt it.
    std::fs::write(sidecar(&dest, "-wal"), b"stale log of the old database").unwrap();
    std::fs::write(sidecar(&dest, "-shm"), b"stale index").unwrap();
    let id = Uuid::new_v4();
    replace(&root, &dest, &Source::File { path: source }, limits(), id).unwrap();
    assert_eq!(count(&dest), 5);
    assert_eq!(names(&root), ["app.sqlite3"]);
    // Replacing with nothing removes the file.
    replace(&root, &dest, &Source::Empty, limits(), Uuid::new_v4()).unwrap();
    assert!(!dest.exists());
}

#[test]
fn a_database_another_connection_has_open_is_neither_replaced_nor_removed() {
    for mode in ["delete", "wal"] {
        let dir = tempfile::tempdir().unwrap();
        let (source, root) = (dir.path().join("src.sqlite3"), dir.path().join("ws"));
        std::fs::create_dir_all(&root).unwrap();
        database(&source, 5);
        let dest = root.join("app.sqlite3");
        database(&dest, 99);
        let open = Connection::open(&dest).unwrap();
        open.pragma_update(None, "journal_mode", mode).unwrap();
        // An application with the database open and a read in flight.
        open.execute_batch("BEGIN").unwrap();
        let rows: i64 = open
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 99);
        let error = replace(
            &root,
            &dest,
            &Source::File {
                path: source.clone(),
            },
            limits(),
            Uuid::new_v4(),
        )
        .unwrap_err();
        assert!(error.downcast_ref::<InUse>().is_some(), "{mode}: {error:#}");
        assert!(
            remove(&root, &dest, limits().wait)
                .unwrap_err()
                .downcast_ref::<InUse>()
                .is_some()
        );
        assert_eq!(count(&dest), 99, "{mode}: untouched");
        assert!(
            !names(&root).iter().any(|n| n.contains("partial")),
            "{mode}: {:?}",
            names(&root)
        );
        // Closed, it goes.
        open.execute_batch("ROLLBACK").unwrap();
        drop(open);
        replace(
            &root,
            &dest,
            &Source::File { path: source },
            limits(),
            Uuid::new_v4(),
        )
        .unwrap();
        assert_eq!(count(&dest), 5, "{mode}");
        assert!(remove(&root, &dest, limits().wait).unwrap());
        assert_eq!(names(&root), Vec::<String>::new());
    }
}

/// A database in `mode`, idle (nothing has it open).
fn database_in(path: &Path, rows: i64, mode: &str) {
    database(path, rows);
    let conn = Connection::open(path).unwrap();
    conn.pragma_update(None, "journal_mode", mode).unwrap();
}

/// Whether another connection is kept from writing to `path` right now.
fn writing_is_refused(path: &Path) -> bool {
    let attempt = Connection::open(path).and_then(|conn| {
        conn.busy_timeout(Duration::ZERO)?;
        conn.execute_batch("BEGIN IMMEDIATE")
    });
    matches!(attempt, Err(ref e) if is_busy(e))
}

#[test]
fn sqlites_lock_is_held_until_the_file_is_gone() {
    for mode in ["delete", "wal"] {
        let dir = tempfile::tempdir().unwrap();
        let (source, root) = (dir.path().join("src.sqlite3"), dir.path().join("ws"));
        std::fs::create_dir_all(&root).unwrap();
        database(&source, 5);
        let dest = root.join("app.sqlite3");
        database_in(&dest, 99, mode);

        // A writer that starts the moment before the file is replaced finds
        // the database locked, not an unlocked file about to be swapped.
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
        let (probe, flag) = (dest.clone(), seen.clone());
        seams::set_while_held(dest.clone(), move || {
            let refused = writing_is_refused(&probe);
            flag.store(
                if refused { 1 } else { 2 },
                std::sync::atomic::Ordering::SeqCst,
            );
        });
        replace(
            &root,
            &dest,
            &Source::File {
                path: source.clone(),
            },
            limits(),
            Uuid::new_v4(),
        )
        .unwrap();
        assert_eq!(
            seen.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "{mode}: replace did not hold the lock while it swapped the file"
        );
        assert_eq!(count(&dest), 5, "{mode}");
        assert_eq!(names(&root), ["app.sqlite3"], "{mode}: no stale sidecars");

        // The same for a removal.
        Connection::open(&dest)
            .unwrap()
            .pragma_update(None, "journal_mode", mode)
            .unwrap();
        let (probe, flag) = (dest.clone(), seen.clone());
        flag.store(0, std::sync::atomic::Ordering::SeqCst);
        seams::set_while_held(dest.clone(), move || {
            let refused = writing_is_refused(&probe);
            flag.store(
                if refused { 1 } else { 2 },
                std::sync::atomic::Ordering::SeqCst,
            );
        });
        assert!(remove(&root, &dest, limits().wait).unwrap());
        assert_eq!(
            seen.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "{mode}: remove did not hold the lock while it removed the file"
        );
        assert_eq!(names(&root), Vec::<String>::new(), "{mode}");
    }
}

#[cfg(unix)]
#[test]
fn a_file_put_over_the_name_after_the_lock_is_put_back_and_nothing_is_done_to_it() {
    for what in ["remove", "replace", "replace with nothing"] {
        let dir = tempfile::tempdir().unwrap();
        let (source, root) = (dir.path().join("src.sqlite3"), dir.path().join("ws"));
        std::fs::create_dir_all(&root).unwrap();
        database(&source, 5);
        let dest = root.join("app.sqlite3");
        database(&dest, 99);
        // Somebody else's database, in the directory, ready to be renamed over
        // the name the lock was taken on.
        let other = root.join("other.sqlite3");
        database(&other, 3);
        // What sits beside the name: after the swap it is the other file's.
        std::fs::write(sidecar(&dest, "-wal"), b"its own log").unwrap();
        let (from, to) = (other.clone(), dest.clone());
        seams::set_while_held(dest.clone(), move || {
            std::fs::rename(&from, &to).unwrap();
        });
        let error = match what {
            "remove" => remove(&root, &dest, limits().wait).unwrap_err(),
            "replace" => replace(
                &root,
                &dest,
                &Source::File { path: source },
                limits(),
                Uuid::new_v4(),
            )
            .unwrap_err(),
            _ => replace(&root, &dest, &Source::Empty, limits(), Uuid::new_v4()).unwrap_err(),
        };
        assert!(
            format!("{error:#}").contains("changed"),
            "{what}: {error:#}"
        );
        // The other database is where it was put, with its data and its log.
        assert_eq!(count(&dest), 3, "{what}");
        assert_eq!(
            std::fs::read(sidecar(&dest, "-wal")).unwrap(),
            b"its own log",
            "{what}: its log is not removed"
        );
        assert!(
            !names(&root).iter().any(|n| n.contains("devflow")),
            "{what}: nothing is left aside: {:?}",
            names(&root)
        );
    }
}

#[test]
fn a_file_that_replaces_another_goes_through_a_name_of_the_operations_own() {
    // The old file is moved aside under the operation's name and removed;
    // a retry or a recovery clears what a dead run left of it.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();
    let dest = root.join("app.sqlite3");
    let id = Uuid::new_v4();
    let displaced = root.join(format!(".app.sqlite3.devflow-{}.displaced", id.simple()));
    std::fs::write(&displaced, b"the old database, moved aside").unwrap();
    clear_staged(&root, &dest, id);
    assert!(!displaced.exists());
}

/// Swap `root/db` for a link to `outside`, as a branch could between the walk
/// and the write.
#[cfg(unix)]
fn swap_directory_for_link(root: &Path, outside: &Path) -> impl Fn() + Send + Sync + 'static {
    let (dir, moved, outside) = (
        root.join("db"),
        root.join("db.moved"),
        outside.to_path_buf(),
    );
    move || {
        std::fs::rename(&dir, &moved).unwrap();
        std::os::unix::fs::symlink(&outside, &dir).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn staging_through_a_directory_swapped_for_a_link_writes_nothing_outside() {
    for from in ["a snapshot", "a live file"] {
        let dir = tempfile::tempdir().unwrap();
        let (root, outside) = (dir.path().join("ws"), dir.path().join("outside"));
        std::fs::create_dir_all(&outside).unwrap();
        let source = dir.path().join("src.sqlite3");
        database(&source, 5);
        let dest = root.join("db/app.sqlite3");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        // The directory becomes a link to `outside` after the walk and before
        // the first byte of the staged file is written.
        seams::set_before_write(dest.clone(), swap_directory_for_link(&root, &outside));
        let source = if from == "a snapshot" {
            Source::Snapshot { file: source }
        } else {
            Source::File { path: source }
        };
        let error = provision(&root, &dest, &source, limits(), Uuid::new_v4()).unwrap_err();
        assert!(
            format!("{error:#}").contains("changed"),
            "{from}: {error:#}"
        );
        assert!(
            names(&outside).is_empty(),
            "{from}: something was written outside the worktree: {:?}",
            names(&outside)
        );
        // And nothing is left in the directory that was held.
        assert!(names(&root.join("db.moved")).is_empty(), "{from}");
    }
}

#[cfg(unix)]
#[test]
fn a_directory_swapped_before_staging_begins_is_refused_before_any_write() {
    let dir = tempfile::tempdir().unwrap();
    let (root, outside) = (dir.path().join("ws"), dir.path().join("outside"));
    std::fs::create_dir_all(&outside).unwrap();
    let source = dir.path().join("src.sqlite3");
    database(&source, 5);
    let dest = root.join("db/app.sqlite3");
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    // The walk is done by `prepare`; swapping right after it is the window the
    // directory check closes for a live file's copy.
    let place = place_of(&root, &dest, true).unwrap().unwrap();
    swap_directory_for_link(&root, &outside)();
    let error = place.confirm_dir().unwrap_err();
    assert!(format!("{error:#}").contains("changed"), "{error:#}");
    // A copy made through the held descriptor lands in the held directory.
    let shared = place
        .clone_new(&source, OsStr::new(".app.sqlite3.devflow-x.partial"))
        .unwrap();
    let _ = shared;
    assert_eq!(
        names(&root.join("db.moved")),
        [".app.sqlite3.devflow-x.partial"]
    );
    assert!(names(&outside).is_empty());
    assert_eq!(
        count(&root.join("db.moved/.app.sqlite3.devflow-x.partial")),
        5
    );
}

#[cfg(unix)]
#[test]
fn a_database_that_cannot_be_locked_for_another_reason_is_left_alone() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: `geteuid` has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: root can write to a file whatever its mode");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (source, root) = (dir.path().join("src.sqlite3"), dir.path().join("ws"));
    std::fs::create_dir_all(&root).unwrap();
    database(&source, 5);
    let dest = root.join("app.sqlite3");
    database(&dest, 99);
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o444)).unwrap();

    // The lock cannot be taken (the file cannot be written), so nothing can
    // be said about who uses it: not removed, not replaced, no leftover.
    let error = remove(&root, &dest, limits().wait).unwrap_err();
    assert!(error.downcast_ref::<CannotLock>().is_some(), "{error:#}");
    let error = replace(
        &root,
        &dest,
        &Source::File { path: source },
        limits(),
        Uuid::new_v4(),
    )
    .unwrap_err();
    assert!(error.downcast_ref::<CannotLock>().is_some(), "{error:#}");
    assert_eq!(names(&root), ["app.sqlite3"]);
    assert_eq!(count(&dest), 99);
}

#[test]
fn a_damaged_database_is_still_locked_and_can_be_removed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();
    let dest = root.join("app.sqlite3");
    database(&dest, 50);
    // The header is intact; the first b-tree page is not.
    let mut bytes = std::fs::read(&dest).unwrap();
    for byte in &mut bytes[100..200] {
        *byte = 0xff;
    }
    std::fs::write(&dest, bytes).unwrap();
    assert!(remove(&root, &dest, limits().wait).unwrap());
    assert_eq!(names(&root), Vec::<String>::new());
}

#[test]
fn removing_leaves_what_is_not_a_regular_file_and_what_is_not_there() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    assert!(!remove(root, &root.join("none.sqlite3"), limits().wait).unwrap());
    assert!(!remove(root, &root.join("no/dir/none.sqlite3"), limits().wait).unwrap());
    let sub = root.join("dir.sqlite3");
    std::fs::create_dir(&sub).unwrap();
    assert!(remove(root, &sub, limits().wait).is_err());
    assert!(sub.is_dir());
    // A text file at the name is no database, and is removable as a file.
    let text = root.join("text.sqlite3");
    std::fs::write(&text, "not sqlite").unwrap();
    assert!(remove(root, &text, limits().wait).unwrap());
}

#[test]
fn urls_are_the_absolute_path_percent_encoded() {
    assert_eq!(
        url_of(Path::new("/work/shop/db/dev.sqlite3")),
        "sqlite:///work/shop/db/dev.sqlite3"
    );
    assert_eq!(
        url_of(Path::new("/work/my shop/a#b?c%d/db.sqlite3")),
        "sqlite:///work/my%20shop/a%23b%3Fc%25d/db.sqlite3"
    );
    assert_eq!(
        url_of(Path::new("/w/été.db")),
        "sqlite:///w/%C3%A9t%C3%A9.db"
    );
}

#[test]
fn status_reads_without_changing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.sqlite3");
    assert!(!status(&path).exists);
    database(&path, 3);
    {
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "journal_mode", "wal").unwrap();
        conn.execute("INSERT INTO t (n, v) VALUES (9, 'x')", [])
            .unwrap();
        let seen = status(&path);
        assert!(seen.exists);
        assert_eq!(seen.journal_mode.as_deref(), Some("wal"));
        assert!(seen.sidecars.contains(&"-wal".to_string()), "{seen:?}");
    }
    let before = names(dir.path());
    let seen = status(&path);
    assert_eq!(seen.bytes, Some(std::fs::metadata(&path).unwrap().len()));
    assert_eq!(seen.journal_mode.as_deref(), Some("wal"));
    assert_eq!(names(dir.path()), before, "looking creates nothing");
    let plain = dir.path().join("plain.sqlite3");
    database(&plain, 1);
    assert_eq!(status(&plain).journal_mode.as_deref(), Some("rollback"));
    let text = dir.path().join("text.sqlite3");
    std::fs::write(&text, "not sqlite").unwrap();
    assert_eq!(status(&text).journal_mode, None);
}

#[cfg(unix)]
#[test]
fn a_clone_is_as_private_as_its_source_and_always_writable() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let (source, root) = (dir.path().join("src.sqlite3"), dir.path().join("ws"));
    std::fs::create_dir_all(&root).unwrap();
    database(&source, 3);
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    // A source the owner cannot write is cloned by backup (it cannot be locked
    // for writing); the others by reflink where blocks are shared.
    for (mine, expected) in [
        (0o600, 0o600),
        (0o640, 0o640),
        (0o444, 0o644),
        (0o400, 0o600),
    ] {
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(mine)).unwrap();
        let dest = root.join(format!("app-{mine:o}.sqlite3"));
        provision(
            &root,
            &dest,
            &Source::File {
                path: source.clone(),
            },
            limits(),
            Uuid::new_v4(),
        )
        .unwrap();
        assert_eq!(mode(&dest), expected, "source {mine:o}");
    }
}

#[cfg(unix)]
#[test]
fn a_file_reached_through_a_link_is_not_read() {
    let dir = tempfile::tempdir().unwrap();
    let (root, elsewhere) = (dir.path().join("ws"), dir.path().join("elsewhere"));
    std::fs::create_dir_all(&root).unwrap();
    database(&elsewhere.join("app.sqlite3"), 2);
    let relative = Path::new("db/app.sqlite3");
    // Not there yet: nothing, and not an error.
    assert_eq!(source_file(&root, relative).unwrap(), None);
    std::fs::create_dir_all(root.join("db")).unwrap();
    assert_eq!(source_file(&root, relative).unwrap(), None);
    database(&root.join("db/app.sqlite3"), 1);
    assert_eq!(
        source_file(&root, relative).unwrap(),
        Some(root.join("db/app.sqlite3"))
    );
    // A directory that is a link out of the worktree, and one that stays in it.
    std::fs::remove_dir_all(root.join("db")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, root.join("db")).unwrap();
    let error = source_file(&root, relative).unwrap_err();
    assert!(error.to_string().contains("symbolic link"), "{error:#}");
    std::fs::remove_file(root.join("db")).unwrap();
    std::fs::create_dir_all(root.join("real")).unwrap();
    database(&root.join("real/app.sqlite3"), 1);
    std::os::unix::fs::symlink(root.join("real"), root.join("db")).unwrap();
    assert!(source_file(&root, relative).is_err());
    // The file itself a link.
    std::fs::remove_file(root.join("db")).unwrap();
    std::fs::create_dir_all(root.join("db")).unwrap();
    std::os::unix::fs::symlink(elsewhere.join("app.sqlite3"), root.join("db/app.sqlite3")).unwrap();
    assert!(source_file(&root, relative).is_err());
}
