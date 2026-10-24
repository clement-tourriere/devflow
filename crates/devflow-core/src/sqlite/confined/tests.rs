//! Tests.
use super::*;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn a_path_that_is_not_below_the_worktree_is_refused_before_anything_is_looked_at() {
    let dir = tempfile::tempdir().unwrap();
    for bad in ["", "..", "../x.db", "a/../../x.db", "/etc/x.db", "./x.db"] {
        let error = Place::open(dir.path(), Path::new(bad), true).unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("not a name") || text.contains("empty"),
            "{bad:?}: {text}"
        );
    }
    assert!(names(dir.path()).is_empty(), "nothing was made");
    // A `.` inside the text is not a part of the path SQLite is given.
    let place = Place::open(dir.path(), Path::new("a/./x.db"), true)
        .unwrap()
        .unwrap();
    assert_eq!(place.path(), dir.path().join("a").join("x.db"));
}

#[test]
fn directories_are_made_on_request_and_their_absence_is_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let relative = Path::new("data/db/app.sqlite3");
    assert!(Place::open(dir.path(), relative, false).unwrap().is_none());
    assert!(names(dir.path()).is_empty(), "looking made nothing");
    let place = Place::open(dir.path(), relative, true).unwrap().unwrap();
    assert!(dir.path().join("data/db").is_dir());
    assert_eq!(place.identity().unwrap(), None, "no file yet");
    assert_eq!(place.path(), dir.path().join(relative));
    assert_eq!(place.name(), "app.sqlite3");
    // Opening what is there is the same.
    assert!(Place::open(dir.path(), relative, false).unwrap().is_some());
}

#[cfg(unix)]
#[test]
fn a_link_anywhere_on_the_way_or_in_the_name_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (root, elsewhere) = (dir.path().join("ws"), dir.path().join("elsewhere"));
    std::fs::create_dir_all(&root).unwrap();
    write(&elsewhere.join("app.sqlite3"), "theirs");

    // A directory that is a link, at each depth, to a place outside and to a
    // place inside the worktree alike.
    std::os::unix::fs::symlink(&elsewhere, root.join("db")).unwrap();
    for create in [false, true] {
        let error = Place::open(&root, Path::new("db/app.sqlite3"), create).unwrap_err();
        assert!(format!("{error:#}").contains("symbolic link"), "{error:#}");
    }
    std::fs::remove_file(root.join("db")).unwrap();
    std::fs::create_dir_all(root.join("real/inner")).unwrap();
    std::os::unix::fs::symlink(root.join("real"), root.join("db")).unwrap();
    let error = Place::open(&root, Path::new("db/inner/app.sqlite3"), true).unwrap_err();
    assert!(format!("{error:#}").contains("symbolic link"), "{error:#}");
    std::fs::remove_file(root.join("db")).unwrap();
    std::fs::create_dir_all(root.join("db")).unwrap();
    std::os::unix::fs::symlink(root.join("real"), root.join("db/inner")).unwrap();
    assert!(Place::open(&root, Path::new("db/inner/app.sqlite3"), true).is_err());

    // The file's own name as a link.
    std::fs::remove_file(root.join("db/inner")).unwrap();
    std::os::unix::fs::symlink(elsewhere.join("app.sqlite3"), root.join("db/app.sqlite3")).unwrap();
    let place = Place::open(&root, Path::new("db/app.sqlite3"), false)
        .unwrap()
        .unwrap();
    let error = place.identity().unwrap_err();
    assert!(format!("{error:#}").contains("symbolic link"), "{error:#}");
    // Removing the name removes the link and never what it names.
    place.remove_database().unwrap();
    assert!(root.join("db/app.sqlite3").symlink_metadata().is_err());
    assert_eq!(
        std::fs::read_to_string(elsewhere.join("app.sqlite3")).unwrap(),
        "theirs",
        "the file the link names is untouched"
    );

    // The worktree's own root may itself be reached through a link: it is
    // the place the caller named, not a part of the way below it.
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(&root, &alias).unwrap();
    assert!(
        Place::open(&alias, Path::new("db/app.sqlite3"), false)
            .unwrap()
            .is_some()
    );
}

#[test]
fn only_a_regular_file_is_a_database() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("db/app.sqlite3")).unwrap();
    let place = Place::open(dir.path(), Path::new("db/app.sqlite3"), false)
        .unwrap()
        .unwrap();
    let error = place.identity().unwrap_err();
    assert!(
        format!("{error:#}").contains("not a regular file"),
        "{error:#}"
    );
    assert!(place.remove_database().is_err());
    assert!(dir.path().join("db/app.sqlite3").is_dir(), "left alone");
}

#[cfg(unix)]
#[test]
fn what_is_done_goes_through_the_directory_that_was_opened_not_through_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let (root, elsewhere) = (dir.path().join("ws"), dir.path().join("elsewhere"));
    write(&root.join("db/app.sqlite3"), "mine");
    write(&root.join("db/app.sqlite3-wal"), "mine, log");
    write(&elsewhere.join("app.sqlite3"), "theirs");
    write(&elsewhere.join("app.sqlite3-wal"), "theirs, log");
    let place = Place::open(&root, Path::new("db/app.sqlite3"), false)
        .unwrap()
        .unwrap();
    let held = place.identity().unwrap().unwrap();
    place.confirm().unwrap();

    // After the walk, the directory is swapped for a link to somebody else's.
    std::fs::rename(root.join("db"), root.join("db.moved")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, root.join("db")).unwrap();

    // The path now reaches another file, which SQLite (opening by path) would
    // lock: confirming against the descriptor says so.
    let error = place.confirm().unwrap_err();
    assert!(format!("{error:#}").contains("changed"), "{error:#}");
    // The file held is still identified as the one that was looked at.
    assert!(place.identity().unwrap().unwrap().is_same_file(&held));

    // And what is removed is what was opened.
    place.remove_database().unwrap();
    assert_eq!(names(&root.join("db.moved")), Vec::<String>::new());
    assert_eq!(
        names(&elsewhere),
        ["app.sqlite3", "app.sqlite3-wal"],
        "nothing of theirs was touched"
    );
    assert_eq!(
        std::fs::read_to_string(elsewhere.join("app.sqlite3")).unwrap(),
        "theirs"
    );
}

#[test]
fn a_new_file_is_installed_without_replacing_and_a_replacement_is_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let place = Place::open(dir.path(), Path::new("db/app.sqlite3"), true)
        .unwrap()
        .unwrap();
    let staged = OsString::from(".app.sqlite3.devflow-1.partial");
    write(&place.sibling(&staged), "staged one");
    let staged_identity = place.confirm_staged(&staged).unwrap();
    place.install_new(&staged).unwrap();
    assert_eq!(std::fs::read_to_string(place.path()).unwrap(), "staged one");
    assert!(
        place.identity_of(&staged).unwrap().is_none(),
        "staged name goes"
    );
    assert!(
        place
            .identity()
            .unwrap()
            .unwrap()
            .is_same_file(&staged_identity),
        "the installed file is the staged inode"
    );

    // Something is there: the install fails and takes nothing.
    write(&place.sibling(&staged), "staged two");
    let error = place.install_new(&staged).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read_to_string(place.path()).unwrap(), "staged one");
    // A replacement does replace, and the staged name is consumed.
    place.replace_with(&staged).unwrap();
    assert_eq!(std::fs::read_to_string(place.path()).unwrap(), "staged two");
    assert_eq!(names(place.path().parent().unwrap()), ["app.sqlite3"]);
}

#[test]
fn the_files_sqlite_keeps_beside_a_database_go_with_it_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let place = Place::open(dir.path(), Path::new("db/app.sqlite3"), true)
        .unwrap()
        .unwrap();
    let beside = place.path().parent().unwrap().to_path_buf();
    for name in [
        "app.sqlite3",
        "app.sqlite3-wal",
        "app.sqlite3-shm",
        "app.sqlite3-journal",
        "app.sqlite3.bak",
        "other.sqlite3-wal",
    ] {
        write(&beside.join(name), name);
    }
    place.remove_database().unwrap();
    assert_eq!(names(&beside), ["app.sqlite3.bak", "other.sqlite3-wal"]);
    // Removing what is not there is fine.
    place.remove_database().unwrap();
    // A staged file and what SQLite kept beside it.
    let staged = OsString::from(".app.sqlite3.devflow-2.partial");
    write(&place.sibling(&staged), "x");
    let mut log = staged.clone();
    log.push("-wal");
    write(&place.sibling(&log), "x");
    place.remove_staged(&staged);
    assert_eq!(names(&beside), ["app.sqlite3.bak", "other.sqlite3-wal"]);
}

#[test]
fn an_identity_is_the_inode_and_not_the_size() {
    let a = Identity {
        dev: 1,
        ino: 2,
        len: 10,
    };
    assert!(a.is_same_file(&Identity { len: 4096, ..a }));
    assert!(!a.is_same_file(&Identity { ino: 3, ..a }));
    assert!(!a.is_same_file(&Identity { dev: 2, ..a }));
}

#[test]
fn a_sidecar_or_a_reserved_directory_is_never_a_database_whatever_its_case() {
    let dir = tempfile::tempdir().unwrap();
    for bad in [
        "db/app.sqlite3-wal",
        "db/app.sqlite3-WAL",
        "db/app.sqlite3-Shm",
        "db/app.sqlite3-JOURNAL",
        "db/app.sqlite3-\u{17f}hm",
        ".git/app.db",
        ".GIT/app.db",
        ".DevFlow/app.db",
    ] {
        let error = Place::open(dir.path(), Path::new(bad), true).unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("beside a database") || text.contains("own"),
            "{bad}: {text}"
        );
    }
    assert!(names(dir.path()).is_empty(), "nothing was made");
}

#[test]
fn on_a_disk_that_ignores_case_the_log_of_a_database_cannot_be_dropped_as_a_database() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("db/app.sqlite3-wal"), "an active log");
    // Does this disk see `APP.SQLITE3-WAL` as the same file?
    let insensitive = dir.path().join("db/APP.SQLITE3-WAL").exists();
    if !insensitive {
        eprintln!("case-sensitive disk: the alias is a different file here");
    }
    let error = Place::open(dir.path(), Path::new("db/APP.SQLITE3-WAL"), false).unwrap_err();
    assert!(
        format!("{error:#}").contains("beside a database"),
        "{error:#}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("db/app.sqlite3-wal")).unwrap(),
        "an active log"
    );
}

#[test]
fn a_platform_without_inodes_refuses_with_a_typed_problem() {
    // The path every non-Unix build takes through `Place::open`.
    let error = unsupported_platform();
    let problem = error.downcast_ref::<Problem>().expect("typed");
    assert!(problem.message.contains("not supported on this platform"));
    // And what it would otherwise rely on never says two files are the same.
    if cfg!(not(unix)) {
        let a = Identity {
            dev: 0,
            ino: 0,
            len: 1,
        };
        assert!(!a.is_same_file(&a));
    }
}

#[cfg(unix)]
#[test]
fn a_file_is_displaced_only_if_it_is_the_one_that_was_locked() {
    let dir = tempfile::tempdir().unwrap();
    let place = Place::open(dir.path(), Path::new("db/app.sqlite3"), true)
        .unwrap()
        .unwrap();
    let beside = place.path().parent().unwrap().to_path_buf();
    write(place.path(), "the one that was locked");
    let locked = place.identity().unwrap().unwrap();

    // The very file: moved aside under the private name, which then goes.
    let private = place.displace(&locked, "t1").unwrap();
    assert!(place.identity().unwrap().is_none(), "the name is free");
    assert_eq!(
        std::fs::read_to_string(place.sibling(&private)).unwrap(),
        "the one that was locked"
    );
    // It can be put back while the name is free, and not over something.
    place.restore(&private).unwrap();
    assert_eq!(
        std::fs::read_to_string(place.path()).unwrap(),
        "the one that was locked"
    );
    assert_eq!(names(&beside), ["app.sqlite3"]);

    // Another file was renamed over the name: it is put back as it was, the
    // operation is refused, and nothing of it is removed.
    write(&beside.join("other"), "somebody else's");
    std::fs::rename(beside.join("other"), place.path()).unwrap();
    let error = place.displace(&locked, "t2").unwrap_err();
    assert!(format!("{error:#}").contains("changed"), "{error:#}");
    assert_eq!(
        std::fs::read_to_string(place.path()).unwrap(),
        "somebody else's"
    );
    assert_eq!(names(&beside), ["app.sqlite3"], "nothing left aside");
}
