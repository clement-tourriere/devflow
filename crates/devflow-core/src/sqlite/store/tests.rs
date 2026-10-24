use super::*;

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::at(dir.path().to_path_buf(), Uuid::new_v4(), "app");
    (dir, store)
}

/// Take a snapshot whose file holds `content`.
fn take(store: &Store, name: &str, content: &[u8]) -> Snapshot {
    let begun = store.begin(name, "workspace:main").unwrap();
    let partial = store.partial_path(&begun).unwrap();
    std::fs::write(&partial, content).unwrap();
    store
        .finish(&begun, content.len() as u64, How::Backup)
        .unwrap()
}

#[test]
fn versions_count_per_name_and_are_never_reused() {
    let (_dir, store) = store();
    assert_eq!(take(&store, "seed", b"one").version, 1);
    assert_eq!(take(&store, "seed", b"two").version, 2);
    assert_eq!(take(&store, "other", b"x").version, 1);
    let latest = store.find("seed", None).unwrap().unwrap();
    assert_eq!(latest.version, 2);
    assert_eq!(store.find("seed", Some(1)).unwrap().unwrap().version, 1);
    assert!(store.find("seed", Some(3)).unwrap().is_none());
    assert_eq!(store.count(), 3);

    // A deleted version is not given to other data.
    store.delete("seed", 2).unwrap();
    assert_eq!(store.find("seed", None).unwrap().unwrap().version, 1);
    assert_eq!(take(&store, "seed", b"three").version, 3);
    // Deleting what is not there is an error that says so.
    assert!(store.delete("seed", 2).is_err());
}

#[test]
fn a_snapshot_is_read_only_and_verified_before_it_is_read() {
    let (_dir, store) = store();
    let snapshot = take(&store, "seed", b"content");
    let file = store.verified_file(&snapshot).unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), b"content");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o400);
        let catalog = std::fs::metadata(store.catalog_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(catalog, 0o600);
    }
    // Changed behind devflow's back: refused, with the snapshot named.
    let mut perms = std::fs::metadata(&file).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&file, perms).unwrap();
    std::fs::write(&file, b"tampered with").unwrap();
    let error = store.verified_file(&snapshot).unwrap_err().to_string();
    assert!(error.contains("seed@v1"), "{error}");
    assert!(error.contains("changed"), "{error}");
    // And gone.
    std::fs::remove_file(&file).unwrap();
    let error = store.verified_file(&snapshot).unwrap_err().to_string();
    assert!(error.contains("gone"), "{error}");
    let damaged = store.reconcile().unwrap().damaged;
    assert_eq!(damaged, ["seed@v1"]);
}

#[test]
fn an_abandoned_snapshot_is_removed_by_the_next_change_and_its_version_stays_used() {
    let (_dir, store) = store();
    // A run that died: a record `creating` for a process that is gone, and a
    // partial file.
    let begun = store.begin("seed", "workspace:main").unwrap();
    let partial = store.partial_path(&begun).unwrap();
    std::fs::write(&partial, b"half").unwrap();
    store
        .change(|records| {
            records.snapshots[0].pid = Some(u32::MAX - 1);
            Ok(())
        })
        .unwrap();
    assert_eq!(store.unfinished().unwrap().len(), 1);
    assert_eq!(store.count(), 0, "a snapshot being made is not listed");
    assert!(store.find("seed", None).unwrap().is_none());

    let next = take(&store, "seed", b"whole");
    assert_eq!(
        next.version, 2,
        "version 1 was given out and is never reused"
    );
    assert!(!partial.exists());
    assert!(store.unfinished().unwrap().is_empty());
    assert_eq!(store.count(), 1);

    // A run that is still going is left alone.
    let live = store.begin("live", "workspace:main").unwrap();
    let live_file = store.partial_path(&live).unwrap();
    std::fs::write(&live_file, b"in progress").unwrap();
    let settled = store.reconcile().unwrap();
    assert!(settled.abandoned.is_empty());
    assert!(live_file.exists());
    store.abort(&live);
    assert!(!live_file.exists());
    assert!(store.unfinished().unwrap().is_empty());
}

#[test]
fn a_delete_that_was_started_is_finished() {
    let (_dir, store) = store();
    let snapshot = take(&store, "seed", b"content");
    let file = store.file_of(&snapshot);
    // Recorded, then the process died before the file went.
    store
        .change(|records| {
            records.snapshots[0].state = State::Deleting;
            Ok(())
        })
        .unwrap();
    assert!(file.exists());
    assert_eq!(store.count(), 0);
    let settled = store.reconcile().unwrap();
    assert_eq!(settled.removed, ["seed@v1"]);
    assert!(!file.exists());
    assert!(store.unfinished().unwrap().is_empty());
    assert!(store.find("seed", None).unwrap().is_none());
}

#[test]
fn names_follow_the_rule_the_other_engines_use() {
    for good in ["seed", "nightly-2026-10-24", "with.dot", "ünï"] {
        assert!(validate_name(good).is_ok(), "{good}");
    }
    for bad in ["", "a@b", "has space", "tab\tin", &"x".repeat(101)] {
        assert!(validate_name(bad).is_err(), "{bad:?}");
    }
    let (_dir, store) = store();
    assert!(store.begin("a@b", "workspace:main").is_err());
    assert!(store.unfinished().unwrap().is_empty());
}

#[test]
fn a_catalog_of_a_newer_devflow_or_another_project_is_refused() {
    let (dir, store) = store();
    take(&store, "seed", b"x");
    let path = store.catalog_path();
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["version"] = serde_json::json!(VERSION + 1);
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let error = store.list().unwrap_err().to_string();
    assert!(error.contains("newer devflow"), "{error}");
    value["version"] = serde_json::json!(VERSION);
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let other = Store::at(dir.path().to_path_buf(), Uuid::new_v4(), "app");
    assert!(
        other
            .list()
            .unwrap_err()
            .to_string()
            .contains("another project")
    );
}

#[test]
fn services_do_not_see_each_others_snapshots_and_forget_removes_only_its_own() {
    let (dir, app) = store();
    let project = app.project_id;
    let reports = Store::at(dir.path().to_path_buf(), project, "reports");
    take(&app, "seed", b"app");
    take(&reports, "seed", b"reports");
    assert_eq!(app.count(), 1);
    assert_eq!(reports.count(), 1);
    assert_eq!(
        std::fs::read(reports.file_of(&reports.find("seed", None).unwrap().unwrap())).unwrap(),
        b"reports"
    );
    assert_eq!(app.forget().unwrap(), 1);
    assert_eq!(app.count(), 0);
    assert!(!app.files_dir().exists());
    assert_eq!(reports.count(), 1);
}

#[test]
fn forget_removes_the_files_the_catalog_records_and_nothing_else() {
    let (_dir, app) = store();
    let first = take(&app, "seed", b"one");
    let second = take(&app, "seed", b"two");
    let stranger = app.files_dir().join("notes.txt");
    std::fs::write(&stranger, "somebody's").unwrap();
    let nested = app.files_dir().join("nested");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(nested.join("deep.txt"), "also somebody's").unwrap();
    assert_eq!(app.forget().unwrap(), 2);
    assert!(!app.file_of(&first).exists() && !app.file_of(&second).exists());
    assert!(!app.is_recorded().unwrap());
    // Not a directory it removed wholesale: what the catalog does not record
    // stays, and so does the directory that holds it.
    assert_eq!(std::fs::read_to_string(&stranger).unwrap(), "somebody's");
    assert!(nested.join("deep.txt").exists());
}

#[test]
fn forget_of_a_service_the_catalog_does_not_record_removes_nothing() {
    let (_dir, app) = store();
    take(&app, "seed", b"one");
    let other = Store::at(root_of(&app).to_path_buf(), app.project_id, "other");
    std::fs::create_dir_all(other.files_dir()).unwrap();
    std::fs::write(other.files_dir().join("keep.txt"), "not recorded").unwrap();
    let error = other.forget().unwrap_err();
    assert!(
        error.to_string().contains("records no snapshot"),
        "{error:#}"
    );
    assert_eq!(
        std::fs::read_to_string(other.files_dir().join("keep.txt")).unwrap(),
        "not recorded"
    );
    assert_eq!(app.count(), 1);

    // A store whose key is a path is not made, and one made without going
    // through `open` does not forget.
    let wild = Store::at(root_of(&app).to_path_buf(), app.project_id, "../escape");
    assert!(wild.forget().is_err());
    assert!(Store::open(app.project_id, "/tmp").is_err());
}

#[test]
fn forget_finishes_after_an_interruption() {
    let (_dir, app) = store();
    let snapshot = take(&app, "seed", b"one");
    // A forget that marked its records and was killed before removing: the
    // records say what was going on, and the next reconcile finishes it.
    app.change(|records| {
        records.snapshots[0].state = State::Deleting;
        Ok(())
    })
    .unwrap();
    assert!(app.file_of(&snapshot).exists());
    let done = app.reconcile().unwrap();
    assert_eq!(done.removed, ["seed@v1"]);
    assert!(!app.file_of(&snapshot).exists());
}

#[test]
fn concurrent_snapshots_get_distinct_versions() {
    let (_dir, store) = store();
    let store = std::sync::Arc::new(store);
    let handles: Vec<_> = (0..6)
        .map(|n| {
            let store = store.clone();
            std::thread::spawn(move || take(&store, "seed", format!("run {n}").as_bytes()).version)
        })
        .collect();
    let mut versions: Vec<u32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    versions.sort_unstable();
    assert_eq!(versions, [1, 2, 3, 4, 5, 6]);
    assert_eq!(store.count(), 6);
}
