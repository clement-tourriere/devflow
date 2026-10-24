//! Classification, the inventory and the removal of instances Git forgot.
//!
//! Nothing here needs a database server or Docker: resources are daemons
//! (against a fake pitchfork) and, where a backend is wanted, an external
//! server at a closed port.
use super::*;
use crate::receipt::{DataBinding, ProcessRecord, ServiceBinding};
use crate::worktree::testutil::{TestRepo, repo};
use serde_json::{Value, json};

const FAKE: &str = include_str!("../../../../tests/support/fake-pitchfork.sh");

/// A pitchfork that reports whatever the test writes beside it.
struct Fake {
    dir: tempfile::TempDir,
}

impl Fake {
    fn new() -> Fake {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("pitchfork");
        std::fs::write(&script, FAKE).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Fake { dir }
    }

    fn pitchfork(&self) -> Pitchfork {
        Pitchfork::at(self.dir.path().join("pitchfork"))
    }

    fn set(&self, name: &str, content: &str) {
        std::fs::write(self.dir.path().join(name), content).unwrap();
    }

    fn json(&self, name: &str, value: &Value) {
        self.set(name, &value.to_string());
    }

    fn stops(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("calls.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.split('|').nth(1))
            .filter(|args| args.starts_with("stop"))
            .map(str::to_string)
            .collect()
    }
}

fn listed(namespace: &str, name: &str, status: &str) -> Value {
    json!({
        "id": format!("{namespace}/{name}"), "namespace": namespace, "name": name,
        "pid": if status == "running" { json!(4242) } else { Value::Null },
        "status": status, "oneshot": false, "disabled": false,
        "available": status == "available", "proxy_url": null, "url": null,
        "error": null, "port": [],
    })
}

fn quick() -> Options {
    Options {
        measure: false,
        evidence: false,
        backfill: false,
    }
}

/// A worktree with a receipt (so a record), optionally with daemons recorded.
fn workspace(t: &TestRepo, name: &str, daemons: bool) -> (Workspace, Receipt) {
    let ws = worktree::create(&t.project, name, None).unwrap();
    let mut receipt = Receipt::new(t.project.id(), name, None);
    if daemons {
        receipt.processes = Some(ProcessRecord {
            started_at: Utc::now(),
            ids: vec![format!("{}/web", receipt.slug())],
        });
    }
    receipt.save(&ws.git_dir).unwrap();
    (ws, receipt)
}

/// A workspace Git forgot: the worktree was removed with `git worktree remove`.
fn removed_elsewhere(t: &TestRepo, name: &str, daemons: bool) -> Receipt {
    let (ws, receipt) = workspace(t, name, daemons);
    worktree::force_remove(&t.project, &ws).unwrap();
    assert!(!ws.git_dir.exists(), "the receipt went with the worktree");
    receipt
}

fn class_of(inventory: &Inventory, receipt: &Receipt) -> Class {
    inventory
        .entry(receipt.instance_id)
        .unwrap_or_else(|| panic!("no entry for {}", receipt.workspace))
        .class
}

fn rewrite(t: &TestRepo, instance: Uuid, change: impl FnOnce(&mut Record)) {
    let mut record = ledger::read(&t.project.git_common_dir, instance)
        .unwrap()
        .unwrap();
    change(&mut record);
    std::fs::write(
        ledger::path(&t.project.git_common_dir, instance),
        serde_json::to_vec_pretty(&record).unwrap(),
    )
    .unwrap();
}

#[test]
fn every_class_is_told_apart_and_only_two_are_removable() {
    let t = repo(None);
    let (_active_ws, active) = workspace(&t, "active", true);
    let elsewhere = removed_elsewhere(&t, "elsewhere", true);
    let (missing_ws, missing) = workspace(&t, "missing", true);
    std::fs::remove_dir_all(&missing_ws.path).unwrap();
    let (_moved_ws, moved) = workspace(&t, "moved", true);
    rewrite(&t, moved.instance_id, |r| {
        r.worktree = t.root.join("somewhere-else")
    });

    let inventory = inventory(&t.project, &quick()).unwrap();
    assert_eq!(class_of(&inventory, &active), Class::Active);
    assert_eq!(class_of(&inventory, &elsewhere), Class::RemovedElsewhere);
    assert_eq!(class_of(&inventory, &missing), Class::Missing);
    assert_eq!(class_of(&inventory, &moved), Class::Moved);

    for (receipt, removable) in [
        (&active, false),
        (&elsewhere, true),
        (&missing, true),
        (&moved, false),
    ] {
        let entry = inventory.entry(receipt.instance_id).unwrap();
        assert_eq!(entry.removable, removable, "{}", receipt.workspace);
        assert_eq!(entry.not_removable.is_some(), !removable);
        assert!(entry.generation.is_some() && entry.target().is_some());
    }
    // The default workspace has no record of its own here, and is not listed
    // as something to clean up.
    let moved_entry = inventory.entry(moved.instance_id).unwrap();
    assert_eq!(
        moved_entry.recorded_path,
        Some(t.root.join("somewhere-else"))
    );
    assert_eq!(
        inventory.removable().count(),
        2,
        "{:?}",
        inventory
            .entries
            .iter()
            .map(|e| (&e.workspace, e.class))
            .collect::<Vec<_>>()
    );
    // Ordered by what needs a decision first.
    assert_eq!(inventory.entries[0].class, Class::RemovedElsewhere);
    assert_eq!(inventory.entries[1].class, Class::Missing);
}

#[test]
fn what_cannot_be_positively_identified_is_unknown_and_never_removable() {
    let t = repo(None);
    let unknown = |inventory: &Inventory, receipt: &Receipt| {
        let entry = inventory.entry(receipt.instance_id).unwrap();
        assert_eq!(entry.class, Class::Unknown, "{}", receipt.workspace);
        assert!(!entry.removable, "{}", receipt.workspace);
        assert!(entry.cause.is_some() && entry.reason.is_some());
        entry.cause.clone().unwrap()
    };

    // A record written for another repository that is still there: a copy.
    let copy_of = removed_elsewhere(&t, "copy", true);
    let other = tempfile::tempdir().unwrap();
    rewrite(&t, copy_of.instance_id, |r| {
        r.checkout.common_dir = other.path().canonicalize().unwrap()
    });
    // ...but one whose repository was moved (the old place is gone) is ours.
    let moved_repo = removed_elsewhere(&t, "relocated", true);
    rewrite(&t, moved_repo.instance_id, |r| {
        r.checkout.common_dir = t.root.join("no-longer-there/.git")
    });
    // The folder the worktree was in is missing too: a disconnected drive.
    let drive = removed_elsewhere(&t, "drive", true);
    rewrite(&t, drive.instance_id, |r| {
        r.worktree = t.root.join("unplugged/volume/drive")
    });
    // A directory is where the worktree was, and Git has no registration.
    let squatter = removed_elsewhere(&t, "squatter", true);
    let place = t.root.join("squatter-dir");
    std::fs::create_dir(&place).unwrap();
    rewrite(&t, squatter.instance_id, |r| {
        r.worktree = place.clone();
        r.fingerprint = None;
    });
    // ...unless the directory is evidently another one than the record saw.
    let replaced = removed_elsewhere(&t, "replaced", true);
    let other_place = t.root.join("replaced-dir");
    std::fs::create_dir(&other_place).unwrap();
    rewrite(&t, replaced.instance_id, |r| {
        r.worktree = other_place.clone();
        r.fingerprint = Some("1:2:3".into());
    });
    // Registered, and the receipt is gone.
    let (norecord_ws, noreceipt) = workspace(&t, "noreceipt", true);
    std::fs::remove_file(Receipt::path(&norecord_ws.git_dir)).unwrap();
    // Registered, and the receipt cannot be read.
    let (broken_ws, broken) = workspace(&t, "broken", true);
    std::fs::write(Receipt::path(&broken_ws.git_dir), "{ no").unwrap();
    // A record that cannot be read.
    let garbage = Uuid::new_v4();
    std::fs::write(ledger::path(&t.project.git_common_dir, garbage), "{ no").unwrap();
    // Registered, locked and gone: a portable device that may be unplugged.
    let (locked_ws, locked) = workspace(&t, "locked", true);
    t.project
        .repo()
        .unwrap()
        .find_worktree(locked_ws.git_dir.file_name().unwrap().to_str().unwrap())
        .unwrap()
        .lock(Some("on a portable drive"))
        .unwrap();
    std::fs::remove_dir_all(&locked_ws.path).unwrap();
    // The instance is live in another worktree than the record says.
    let (_live_ws, live) = workspace(&t, "live", true);
    rewrite(&t, live.instance_id, |r| {
        r.admin_id = Some("renamed".into())
    });
    // Another project's record.
    let foreign = removed_elsewhere(&t, "foreign", true);
    rewrite(&t, foreign.instance_id, |r| r.project_id = Uuid::new_v4());

    let inventory = inventory(&t.project, &quick()).unwrap();
    assert!(matches!(
        unknown(&inventory, &copy_of),
        Cause::OtherCheckout { .. }
    ));
    assert_eq!(class_of(&inventory, &moved_repo), Class::RemovedElsewhere);
    assert!(matches!(
        unknown(&inventory, &drive),
        Cause::ParentMissing { .. }
    ));
    assert!(matches!(
        unknown(&inventory, &squatter),
        Cause::Unregistered { .. }
    ));
    assert_eq!(class_of(&inventory, &replaced), Class::RemovedElsewhere);
    // (a receipt that is gone from a registered worktree has no instance to
    // match: the record is the one that says what the worktree was.)
    assert!(matches!(
        unknown(&inventory, &noreceipt),
        Cause::ReceiptMissing
    ));
    assert!(matches!(
        unknown(&inventory, &broken),
        Cause::ReceiptUnreadable
    ));
    assert!(matches!(unknown(&inventory, &foreign), Cause::OtherProject));
    assert!(matches!(unknown(&inventory, &locked), Cause::Locked));
    assert!(matches!(
        unknown(&inventory, &live),
        Cause::AnotherWorktree { .. }
    ));
    let garbled = inventory
        .entries
        .iter()
        .find(|e| e.instance == Some(garbage))
        .expect("an unreadable record is listed");
    assert_eq!(garbled.class, Class::Unknown);
    assert!(!garbled.removable);
    assert!(matches!(
        garbled.cause,
        Some(Cause::RecordUnreadable { .. })
    ));
    assert!(garbled.target().is_none(), "nothing to remove it by");

    // Asked to remove any of them, nothing happens.
    let mut targets = Vec::new();
    for entry in &inventory.entries {
        if let Some(target) = entry.target()
            && entry.class == Class::Unknown
        {
            targets.push(target);
        }
    }
    assert!(!targets.is_empty());
    let outcome = remove(
        &t.project,
        &RemoveRequest {
            targets,
            force: true,
            ..RemoveRequest::default()
        },
        &mut |_| {},
    )
    .unwrap();
    assert!(outcome.removed.is_empty(), "{:?}", outcome.removed);
    assert!(outcome.skipped.iter().all(|s| s.reason.contains("cannot")
        || s.reason.contains("record")
        || !s.reason.is_empty()));
    for instance in [copy_of.instance_id, drive.instance_id, squatter.instance_id] {
        assert!(
            ledger::read(&t.project.git_common_dir, instance)
                .unwrap()
                .is_some(),
            "still recorded"
        );
    }
}

#[test]
fn a_record_copied_from_elsewhere_never_acquires_the_right_to_delete_or_a_new_identity() {
    let t = repo(None);
    let gone = |name: &str| removed_elsewhere(&t, name, true);
    let path = |instance: Uuid| ledger::path(&t.project.git_common_dir, instance);
    // Copied from another machine: its repository is not here to inspect, and
    // the directory it was written in is another one than this.
    let copied = gone("copied");
    rewrite(&t, copied.instance_id, |r| {
        r.checkout.common_dir = "/nowhere/other-machine/shop/.git".into();
        r.checkout.identity = Some("12345:678".into());
    });
    // Written by a build that did not record which directory it was: a move
    // can never be proved from it.
    let legacy = gone("legacy");
    rewrite(&t, legacy.instance_id, |r| {
        r.checkout.common_dir = "/nowhere/before-the-move/shop/.git".into();
        r.checkout.identity = None;
    });
    // The very directory, moved: the path changed, the directory did not.
    let moved = gone("moved");
    rewrite(&t, moved.instance_id, |r| {
        r.checkout.common_dir = "/nowhere/before-the-move/shop/.git".into()
    });
    let (before_copied, before_legacy) = (
        std::fs::read(path(copied.instance_id)).unwrap(),
        std::fs::read(path(legacy.instance_id)).unwrap(),
    );

    // Even an inventory that is allowed to write records writes nothing for
    // what it cannot identify, and refreshes only what it can prove moved.
    let inventory = inventory(
        &t.project,
        &Options {
            backfill: true,
            ..quick()
        },
    )
    .unwrap();
    for receipt in [&copied, &legacy] {
        let entry = inventory.entry(receipt.instance_id).unwrap();
        assert_eq!(entry.class, Class::Unknown, "{}", receipt.workspace);
        assert!(matches!(entry.cause, Some(Cause::OtherCheckout { .. })));
        assert!(!entry.removable);
    }
    assert_eq!(class_of(&inventory, &moved), Class::RemovedElsewhere);
    assert_eq!(
        std::fs::read(path(copied.instance_id)).unwrap(),
        before_copied
    );
    assert_eq!(
        std::fs::read(path(legacy.instance_id)).unwrap(),
        before_legacy
    );
    let refreshed = ledger::read(&t.project.git_common_dir, moved.instance_id)
        .unwrap()
        .unwrap();
    assert_eq!(refreshed.checkout.common_dir, t.project.git_common_dir);

    // And asked to remove them, nothing happens to them.
    let outcome = remove(
        &t.project,
        &RemoveRequest {
            targets: [&copied, &legacy]
                .iter()
                .map(|r| Target {
                    backend: Vec::new(),
                    instance: r.instance_id,
                    generation: ledger::read(&t.project.git_common_dir, r.instance_id)
                        .unwrap()
                        .unwrap()
                        .generation(),
                })
                .collect(),
            force: true,
            ..RemoveRequest::default()
        },
        &mut |_| {},
    )
    .unwrap();
    assert!(outcome.removed.is_empty(), "{:?}", outcome.removed);
    assert_eq!(outcome.skipped.len(), 2);
}

#[test]
fn a_server_that_cannot_be_asked_makes_what_it_holds_unknown() {
    let id = Uuid::new_v4();
    let t = repo(Some(&format!(
        "[project]\nname = \"shop\"\nid = \"{id}\"\n\n[services.db]\nengine = \"postgres\"\n\
         mode = \"external\"\nadmin_url = \"postgres://prov:pw@127.0.0.1:1/postgres\"\n"
    )));
    let ws = worktree::create(&t.project, "feature", None).unwrap();
    let mut receipt = Receipt::new(t.project.id(), "feature", None);
    receipt.services.insert(
        "db".into(),
        ServiceBinding::postgres(
            DataBinding {
                branch_id: Some(Uuid::new_v4()),
                database: "d".into(),
                role: "r".into(),
                url: "postgres://r@127.0.0.1:1/d".into(),
                source: "empty".into(),
                owned: true,
            },
            None,
        ),
    );
    receipt.save(&ws.git_dir).unwrap();
    worktree::force_remove(&t.project, &ws).unwrap();

    let inventory = inventory(&t.project, &quick()).unwrap();
    let entry = inventory.entry(receipt.instance_id).unwrap();
    assert_eq!(entry.class, Class::Unknown);
    assert_eq!(entry.cause, Some(Cause::ServerUnreachable));
    assert!(!entry.removable);
    assert!(matches!(
        entry.branches[0].presence,
        Presence::Unreachable { .. }
    ));
    assert!(
        entry
            .remedies
            .iter()
            .any(|r| matches!(r, Remedy::StartDatabase { .. }))
    );
    // Not a word of a command in what is said.
    assert!(!entry.reason.as_deref().unwrap().contains("`devflow"));
}

#[test]
fn the_summary_comes_from_files_alone_and_counts_only_what_owns_something() {
    let t = repo(None);
    removed_elsewhere(&t, "owns-daemons", true);
    removed_elsewhere(&t, "owns-nothing", false);
    let (ws, _) = workspace(&t, "lost", true);
    std::fs::remove_dir_all(&ws.path).unwrap();
    workspace(&t, "alive", true);
    std::fs::write(
        ledger::path(&t.project.git_common_dir, Uuid::new_v4()),
        "{ no",
    )
    .unwrap();

    let summary = summary(&t.project).unwrap();
    assert_eq!(summary.removed_elsewhere, ["owns-daemons"]);
    assert_eq!(summary.missing, ["lost"]);
    assert_eq!(summary.unreadable, 1);
    assert_eq!(summary.count(), 2);
    assert_eq!(
        summary.sentence().as_deref(),
        Some(
            "1 workspace removed outside devflow and 1 with a missing directory still own data \
             or processes"
        )
    );
    assert!(!summary.sentence().unwrap().contains("devflow gc"));
    assert!(Summary::default().sentence().is_none());
}

#[test]
fn the_inventory_writes_the_records_of_older_workspaces_and_nothing_else() {
    let t = repo(None);
    let (ws, receipt) = workspace(&t, "older", true);
    let file = ledger::path(&t.project.git_common_dir, receipt.instance_id);
    std::fs::remove_file(&file).unwrap();

    let without = inventory(&t.project, &quick()).unwrap();
    assert!(!file.exists(), "no backfill unless asked");
    let entry = without.entry(receipt.instance_id).unwrap();
    assert_eq!(
        (entry.class, entry.has_record, entry.removable),
        (Class::Active, false, false)
    );

    let with = inventory(
        &t.project,
        &Options {
            backfill: true,
            ..quick()
        },
    )
    .unwrap();
    assert!(file.exists(), "recorded from the receipt");
    assert!(
        with.notes.iter().any(|n| n.contains("older")),
        "{:?}",
        with.notes
    );
    let entry = with.entry(receipt.instance_id).unwrap();
    assert!(entry.has_record && entry.generation.is_some());
    assert_eq!(entry.daemons.len(), 1);
    assert!(
        ws.git_dir.exists() && ws.path.exists(),
        "nothing was removed"
    );
}

#[test]
fn daemons_are_asked_about_never_stopped_and_the_record_goes_once_none_runs() {
    let t = repo(None);
    let receipt = removed_elsewhere(&t, "forgotten", true);
    let namespace = receipt.slug();
    let fake = Fake::new();
    fake.json(
        "list.json",
        &json!([
            listed(&namespace, "web", "running"),
            listed("df-someone-else", "web", "running"),
        ]),
    );
    let ask = |t: &TestRepo, fake: &Fake, force: bool| {
        let target = inventory(&t.project, &quick())
            .unwrap()
            .entry(receipt.instance_id)
            .unwrap()
            .target()
            .unwrap();
        let mut said = Vec::new();
        let outcome = remove(
            &t.project,
            &RemoveRequest {
                targets: vec![target],
                pitchfork: Some(fake.pitchfork()),
                force,
                ..RemoveRequest::default()
            },
            &mut |line| said.push(line.to_string()),
        )
        .unwrap();
        (outcome, said)
    };

    // The daemon runs: stopping it would make pitchfork run its lifecycle hooks
    // from a configuration nobody approved, so the removal is refused (forceable)
    // and nothing was stopped or dropped.
    let (refused, _) = ask(&t, &fake, false);
    assert!(refused.removed.is_empty());
    assert_eq!(refused.skipped.len(), 1, "{:?}", refused.skipped);
    assert!(refused.skipped[0].forceable);
    assert!(
        refused.skipped[0].reason.contains("lifecycle hooks")
            && refused.skipped[0]
                .reason
                .contains(&format!("{namespace}/web")),
        "{}",
        refused.skipped[0].reason
    );
    assert!(fake.stops().is_empty(), "never stopped: {:?}", fake.stops());
    let record = ledger::read(&t.project.git_common_dir, receipt.instance_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        record.state,
        ledger::State::Active,
        "a refusal changes nothing"
    );

    // Once the person stopped it (a listing that was read shows nothing of the
    // namespace running), the record stops listing it and the removal goes on.
    fake.json(
        "list.json",
        &json!([
            listed(&namespace, "web", "stopped"),
            listed("df-someone-else", "web", "running"),
        ]),
    );
    let (done, said) = ask(&t, &fake, false);
    assert!(done.skipped.is_empty(), "{:?}", done.skipped);
    assert_eq!(done.removed.len(), 1);
    assert!(done.removed[0].still_recorded.is_empty());
    assert!(done.removed[0].may_still_run.is_none());
    assert!(
        said.iter().any(|line| line.contains("forgotten")),
        "{said:?}"
    );
    assert!(fake.stops().is_empty(), "never stopped: {:?}", fake.stops());
    assert!(
        ledger::read(&t.project.git_common_dir, receipt.instance_id)
            .unwrap()
            .is_none(),
        "everything accounted for: the record is gone"
    );
    let leftover: Vec<_> = std::fs::read_dir(ledger::dir(&t.project.git_common_dir))
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert!(leftover.is_empty(), "{leftover:?}");
}

#[test]
fn a_forced_removal_leaves_running_daemons_running_and_on_the_record() {
    let t = repo(None);
    let receipt = removed_elsewhere(&t, "still-up", true);
    let namespace = receipt.slug();
    let fake = Fake::new();
    fake.json("list.json", &json!([listed(&namespace, "web", "running")]));
    let target = inventory(&t.project, &quick())
        .unwrap()
        .entry(receipt.instance_id)
        .unwrap()
        .target()
        .unwrap();
    let outcome = remove(
        &t.project,
        &RemoveRequest {
            targets: vec![target],
            pitchfork: Some(fake.pitchfork()),
            force: true,
            ..RemoveRequest::default()
        },
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(outcome.removed.len(), 1, "{:?}", outcome.skipped);
    let removed = &outcome.removed[0];
    let may = removed.may_still_run.as_ref().expect("said, not hidden");
    assert_eq!(may.namespace.as_deref(), Some(namespace.as_str()));
    assert_eq!(may.ids, [format!("{namespace}/web")]);
    assert_eq!(
        removed.still_recorded,
        [format!("the daemon {namespace}/web")]
    );
    assert!(fake.stops().is_empty());
    let record = ledger::read(&t.project.git_common_dir, receipt.instance_id)
        .unwrap()
        .expect("the daemon is still named");
    assert_eq!(record.state, ledger::State::Removing);
    assert_eq!(record.resources.daemons, [format!("{namespace}/web")]);
}

#[test]
fn daemons_that_cannot_be_checked_stop_the_removal_and_a_forced_one_keeps_them_recorded() {
    let t = repo(None);
    let receipt = removed_elsewhere(&t, "unsure", true);
    let fake = Fake::new();
    fake.set("list_fail", "");
    let target = inventory(&t.project, &quick())
        .unwrap()
        .entry(receipt.instance_id)
        .unwrap()
        .target()
        .unwrap();

    let refused = remove(
        &t.project,
        &RemoveRequest {
            targets: vec![target.clone()],
            pitchfork: Some(fake.pitchfork()),
            ..RemoveRequest::default()
        },
        &mut |_| {},
    )
    .unwrap();
    assert!(refused.removed.is_empty());
    assert_eq!(refused.skipped.len(), 1);
    assert!(refused.skipped[0].forceable, "{:?}", refused.skipped[0]);
    assert!(
        !refused.skipped[0].reason.contains("`devflow"),
        "{}",
        refused.skipped[0].reason
    );
    let record = ledger::read(&t.project.git_common_dir, receipt.instance_id)
        .unwrap()
        .expect("nothing is lost");
    assert_eq!(
        record.state,
        ledger::State::Active,
        "refused: nothing changed"
    );
    assert_eq!(record.resources.daemons.len(), 1);

    // Asked again, forced.
    let target = inventory(&t.project, &quick())
        .unwrap()
        .entry(receipt.instance_id)
        .unwrap()
        .target()
        .unwrap();
    let forced = remove(
        &t.project,
        &RemoveRequest {
            targets: vec![target],
            pitchfork: Some(fake.pitchfork()),
            force: true,
            ..RemoveRequest::default()
        },
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(forced.removed.len(), 1, "{:?}", forced.skipped);
    let removed = &forced.removed[0];
    let may = removed.may_still_run.as_ref().expect("said, not hidden");
    assert_eq!(may.namespace.as_deref(), Some(receipt.slug().as_str()));
    assert_eq!(removed.still_recorded.len(), 1, "{removed:?}");
    assert!(
        ledger::read(&t.project.git_common_dir, receipt.instance_id)
            .unwrap()
            .is_some(),
        "the daemons are still named"
    );
}

#[test]
fn a_namespace_nobody_could_look_into_stays_on_the_record_even_with_no_daemon_recorded() {
    let t = repo(None);
    // The project uses pitchfork, so a daemon may have been started by hand in
    // the namespace although devflow recorded none.
    std::fs::write(
        t.root.join("pitchfork.toml"),
        "[daemons.web]\nrun = \"x\"\n",
    )
    .unwrap();
    let receipt = removed_elsewhere(&t, "unread", false);
    let fake = Fake::new();
    fake.set("list_fail", "");
    let ask = |force: bool| {
        let target = inventory(&t.project, &quick())
            .unwrap()
            .entry(receipt.instance_id)
            .unwrap()
            .target()
            .unwrap();
        remove(
            &t.project,
            &RemoveRequest {
                targets: vec![target],
                pitchfork: Some(fake.pitchfork()),
                force,
                ..RemoveRequest::default()
            },
            &mut |_| {},
        )
        .unwrap()
    };
    // Not read: refused, nothing changed.
    let refused = ask(false);
    assert!(refused.removed.is_empty() && refused.skipped[0].forceable);
    // Forced: gone on without knowing. The namespace stays named, though no id
    // was ever recorded, so the record is not deleted.
    let forced = ask(true);
    assert_eq!(forced.removed.len(), 1, "{:?}", forced.skipped);
    assert!(forced.removed[0].may_still_run.is_some());
    assert_eq!(forced.removed[0].still_recorded.len(), 1);
    assert!(
        forced.removed[0].still_recorded[0].contains(&receipt.slug()),
        "{:?}",
        forced.removed[0].still_recorded
    );
    let record = ledger::read(&t.project.git_common_dir, receipt.instance_id)
        .unwrap()
        .expect("still named");
    assert!(record.resources.namespace_unverified);
    assert!(
        summary(&t.project)
            .unwrap()
            .removed_elsewhere
            .contains(&"unread".to_string())
    );

    // Once a listing that was read shows nothing running, it goes.
    std::fs::remove_file(fake.dir.path().join("list_fail")).unwrap();
    let done = ask(false);
    assert_eq!(done.removed.len(), 1, "{:?}", done.skipped);
    assert!(done.removed[0].still_recorded.is_empty());
    assert!(
        ledger::read(&t.project.git_common_dir, receipt.instance_id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_removal_is_refused_when_the_record_changed_since_it_was_shown() {
    let t = repo(None);
    let receipt = removed_elsewhere(&t, "shifting", true);
    let shown = inventory(&t.project, &quick())
        .unwrap()
        .entry(receipt.instance_id)
        .unwrap()
        .target()
        .unwrap();
    rewrite(&t, receipt.instance_id, |r| {
        r.resources.daemons.push("ns/other".into())
    });
    let fake = Fake::new();
    let outcome = remove(
        &t.project,
        &RemoveRequest {
            targets: vec![
                shown,
                Target {
                    instance: Uuid::new_v4(),
                    generation: "0".into(),
                    backend: Vec::new(),
                },
            ],
            pitchfork: Some(fake.pitchfork()),
            ..RemoveRequest::default()
        },
        &mut |_| {},
    )
    .unwrap();
    assert!(outcome.removed.is_empty());
    assert_eq!(outcome.skipped.len(), 2);
    assert!(
        outcome.skipped[0]
            .reason
            .contains("changed since it was shown")
    );
    assert!(outcome.skipped[1].reason.contains("no record"));
    assert!(fake.stops().is_empty());
}

#[test]
fn an_active_or_moved_workspace_is_never_removed_even_when_named() {
    let t = repo(None);
    let (ws, active) = workspace(&t, "alive", true);
    let (_moved_ws, moved) = workspace(&t, "moved", true);
    rewrite(&t, moved.instance_id, |r| {
        r.worktree = t.root.join("elsewhere")
    });
    let inventory = inventory(&t.project, &quick()).unwrap();
    let targets: Vec<Target> = [&active, &moved]
        .iter()
        .map(|r| inventory.entry(r.instance_id).unwrap().target().unwrap())
        .collect();
    let outcome = remove(
        &t.project,
        &RemoveRequest {
            targets,
            force: true,
            ..RemoveRequest::default()
        },
        &mut |_| {},
    )
    .unwrap();
    assert!(outcome.removed.is_empty());
    assert_eq!(outcome.skipped.len(), 2);
    assert!(ws.path.exists() && ws.git_dir.exists());
    assert!(
        ledger::read(&t.project.git_common_dir, active.instance_id)
            .unwrap()
            .unwrap()
            .state
            == ledger::State::Active
    );
}

#[test]
fn a_registered_workspace_whose_directory_is_gone_goes_through_the_ordinary_removal() {
    let t = repo(None);
    let (ws, receipt) = workspace(&t, "lost", false);
    std::fs::remove_dir_all(&ws.path).unwrap();
    let inventory = inventory(&t.project, &quick()).unwrap();
    let entry = inventory.entry(receipt.instance_id).unwrap();
    assert_eq!(entry.class, Class::Missing);
    let outcome = remove(
        &t.project,
        &RemoveRequest {
            targets: vec![entry.target().unwrap()],
            pitchfork: Some(Fake::new().pitchfork()),
            ..RemoveRequest::default()
        },
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(outcome.removed.len(), 1, "{:?}", outcome.skipped);
    assert!(
        worktree::list(&t.project)
            .unwrap()
            .iter()
            .all(|w| w.name != "lost"),
        "the registration went too"
    );
    assert!(
        ledger::read(&t.project.git_common_dir, receipt.instance_id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn two_clean_ups_of_one_workspace_do_not_run_at_once() {
    let t = repo(None);
    let receipt = removed_elsewhere(&t, "busy", false);
    let target = inventory(&t.project, &quick())
        .unwrap()
        .entry(receipt.instance_id)
        .unwrap()
        .target()
        .unwrap();
    let held = ledger::try_removal_lock(&t.project.git_common_dir, receipt.instance_id)
        .unwrap()
        .unwrap();
    let outcome = remove(
        &t.project,
        &RemoveRequest {
            targets: vec![target],
            ..RemoveRequest::default()
        },
        &mut |_| {},
    )
    .unwrap();
    assert!(outcome.removed.is_empty());
    assert!(outcome.skipped[0].reason.contains("another clean-up"));
    drop(held);
}

#[test]
fn the_snapshot_of_a_clean_up_is_named_by_workspace_and_date() {
    let name = snapshot_name("feature/login page", Uuid::new_v4());
    assert!(name.starts_with("gc-feature-login-page-"), "{name}");
    assert!(crate::db::validate_snapshot_name(&name).is_ok());
    // A name that cannot be made valid falls back to the instance.
    let instance = Uuid::parse_str("105a6201-aaaa-4bbb-8ccc-0123456789ab").unwrap();
    let long = snapshot_name(&"x".repeat(200), instance);
    assert!(long.starts_with("gc-105a6201-"), "{long}");
}

/// A workspace the person was shown as missing, whose checkout is put back while
/// the clean-up is working: the clean-up must find it active and leave it alone.
#[test]
fn a_checkout_restored_during_the_snapshot_is_not_removed() {
    let t = repo(None);
    let (ws, receipt) = workspace(&t, "restored", false);
    // The directory is "gone" (moved away, an unmounted volume).
    let away = t.root.join("away");
    std::fs::rename(&ws.path, &away).unwrap();
    let shown = inventory(&t.project, &quick()).unwrap();
    let entry = shown.entry(receipt.instance_id).unwrap();
    assert_eq!(entry.class, Class::Missing);
    let target = entry.target().unwrap();

    // The snapshot takes time; the checkout comes back meanwhile.
    let (from, to) = (away.clone(), ws.path.clone());
    AFTER_SNAPSHOT.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || std::fs::rename(&from, &to).unwrap()));
    });
    let outcome = remove(
        &t.project,
        &RemoveRequest {
            targets: vec![target],
            snapshot: true,
            force: true,
            pitchfork: Some(Fake::new().pitchfork()),
        },
        &mut |_| {},
    )
    .unwrap();
    AFTER_SNAPSHOT.with(|hook| *hook.borrow_mut() = None);

    assert!(outcome.removed.is_empty(), "{:?}", outcome.removed);
    assert_eq!(outcome.skipped.len(), 1);
    assert!(!outcome.skipped[0].forceable);
    assert!(
        ws.path.join(".git").exists() && ws.git_dir.exists(),
        "the checkout and its registration are untouched"
    );
    assert!(
        worktree::list(&t.project)
            .unwrap()
            .iter()
            .any(|w| w.path == ws.path && !w.missing)
    );
    assert!(
        Receipt::load(&ws.git_dir).unwrap().is_some(),
        "its receipt is untouched"
    );
    let record = ledger::read(&t.project.git_common_dir, receipt.instance_id)
        .unwrap()
        .unwrap();
    assert_eq!(record.state, ledger::State::Active);
}

/// The removal itself looks at the directory again each time it compares the
/// workspace with what was shown, under its locks, and `force` lifts nothing
/// of it.
#[test]
fn a_removal_that_was_shown_a_missing_directory_refuses_one_that_is_back() {
    let t = repo(None);
    let (ws, receipt) = workspace(&t, "back", false);
    let away = t.root.join("away");
    std::fs::rename(&ws.path, &away).unwrap();
    let listed = worktree::list(&t.project)
        .unwrap()
        .into_iter()
        .find(|w| w.name == "back")
        .unwrap();
    assert!(listed.missing);
    std::fs::rename(&away, &ws.path).unwrap();
    for force in [false, true] {
        let error = setup::remove_workspace(
            &t.project,
            listed.clone(),
            RemoveOptions {
                force,
                expect: Some(ExpectedWorkspace {
                    path: listed.path.clone(),
                    instance_id: Some(receipt.instance_id),
                    fingerprint: None,
                    missing: true,
                }),
                ..RemoveOptions::default()
            },
            &mut NoProgress,
        )
        .unwrap_err();
        let refusal = error.downcast_ref::<Protected>().unwrap();
        assert!(!refusal.forceable, "force={force}: {error:#}");
        assert!(
            refusal.reason.contains("its directory is back"),
            "{}",
            refusal.reason
        );
        assert!(ws.path.join(".git").exists() && ws.git_dir.exists());
    }
}

/// A workspace with no registration to take locks on: it is read again after the
/// snapshot, and an instance whose record changed meanwhile (it was set up
/// again, a removal resumed elsewhere) is left alone.
#[test]
fn a_workspace_that_changed_while_it_was_snapshotted_is_left_alone() {
    let t = repo(None);
    let receipt = removed_elsewhere(&t, "changing", true);
    let target = inventory(&t.project, &quick())
        .unwrap()
        .entry(receipt.instance_id)
        .unwrap()
        .target()
        .unwrap();
    let (common, instance) = (t.project.git_common_dir.clone(), receipt.instance_id);
    AFTER_SNAPSHOT.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            let mut record = ledger::read(&common, instance).unwrap().unwrap();
            record.resources.daemons.push("ns/new".into());
            std::fs::write(
                ledger::path(&common, instance),
                serde_json::to_vec_pretty(&record).unwrap(),
            )
            .unwrap();
        }));
    });
    let fake = Fake::new();
    let outcome = remove(
        &t.project,
        &RemoveRequest {
            targets: vec![target],
            snapshot: true,
            pitchfork: Some(fake.pitchfork()),
            ..RemoveRequest::default()
        },
        &mut |_| {},
    )
    .unwrap();
    AFTER_SNAPSHOT.with(|hook| *hook.borrow_mut() = None);
    assert!(outcome.removed.is_empty());
    assert!(
        outcome.skipped[0].reason.contains("changed since"),
        "{:?}",
        outcome.skipped
    );
    assert!(
        ledger::read(&t.project.git_common_dir, instance)
            .unwrap()
            .is_some(),
        "the record stays"
    );
    assert!(fake.stops().is_empty());
}

fn owned_branch(generation: Option<u32>, volume: Option<&str>) -> Branch {
    Branch {
        service: Some("cache".into()),
        engine: Some(Engine::Valkey),
        id: Uuid::new_v4(),
        pending: false,
        presence: Presence::Owned,
        size_bytes: None,
        container: None,
        memory_bytes: None,
        ready: Some(true),
        generation,
        volume: volume.map(str::to_string),
    }
}

/// A reset keeps the id and the owner and replaces the data: what a removal
/// drops must be what was shown.
#[test]
fn a_branch_reset_since_it_was_shown_is_not_the_data_that_was_agreed_to() {
    let t = repo(None);
    let receipt = removed_elsewhere(&t, "reset", false);
    let mut entry = inventory(&t.project, &quick())
        .unwrap()
        .entry(receipt.instance_id)
        .unwrap()
        .clone();
    let branch = owned_branch(Some(2), Some("vol-b"));
    entry.branches = vec![branch.clone()];
    let shown = entry.target().unwrap();
    assert_eq!(
        shown.backend.len(),
        1,
        "a container branch is shown by its volume"
    );
    assert!(reset_since_shown(&shown, &entry).is_none());

    // Reset: the same id and owner, another generation and volume.
    entry.branches[0].generation = Some(3);
    entry.branches[0].volume = Some("vol-c".into());
    let why = reset_since_shown(&shown, &entry).unwrap();
    assert!(why.contains("not the data that was shown"), "{why}");

    // A branch that has an identity the target never listed is new.
    let unlisted = Target {
        backend: Vec::new(),
        ..shown.clone()
    };
    entry.branches[0] = branch;
    assert!(reset_since_shown(&unlisted, &entry).is_some());

    // Another instance's branch, or one that is not there, is not this
    // removal's to check.
    entry.branches[0].presence = Presence::Foreign;
    assert!(reset_since_shown(&unlisted, &entry).is_none());
}

/// A requested snapshot is never silently skipped: whatever the removal would
/// drop and cannot snapshot stops it before anything is dropped.
#[test]
fn a_requested_snapshot_that_cannot_be_made_stops_the_removal() {
    let t = repo(None);
    let receipt = removed_elsewhere(&t, "snap", false);
    let record = ledger::read(&t.project.git_common_dir, receipt.instance_id)
        .unwrap()
        .unwrap();
    let handle = ledger::Handle::new(&t.project.git_common_dir, receipt.instance_id);
    let entry = inventory(&t.project, &quick())
        .unwrap()
        .entry(receipt.instance_id)
        .unwrap()
        .clone();
    let try_snapshot = |branches: Vec<Branch>| {
        let mut entry = entry.clone();
        entry.branches = branches;
        let mut taken = Vec::new();
        let result = snapshot_all(&t.project, &record, &entry, &handle, &mut taken);
        (result.map_err(|e| format!("{e:#}")), taken)
    };

    // Not there, or somebody else's: nothing of this workspace's to snapshot.
    for presence in [Presence::Absent, Presence::Foreign] {
        let branch = Branch {
            presence,
            ready: None,
            ..owned_branch(None, None)
        };
        assert!(try_snapshot(vec![branch]).0.is_ok());
    }
    // Cannot be asked, not asked, being made or failed: refused.
    for (presence, ready, words) in [
        (
            Presence::Unreachable {
                reason: "no server".into(),
            },
            None,
            "no server",
        ),
        (Presence::NotChecked, None, "not looked at"),
        (Presence::Owned, Some(false), "not ready"),
        (Presence::Owned, None, "not ready"),
    ] {
        let branch = Branch {
            presence,
            ready,
            ..owned_branch(None, None)
        };
        let (result, taken) = try_snapshot(vec![branch]);
        let error = result.unwrap_err();
        assert!(error.contains(words), "{words}: {error}");
        assert!(taken.is_empty());
    }
    // A branch recorded before its creation, which the backend has ready and as
    // the instance's, is snapshotted like any other: the attempt is made (and
    // fails here, for want of a server), it is not skipped.
    let pending = Branch {
        pending: true,
        ..owned_branch(None, None)
    };
    let error = try_snapshot(vec![pending]).0.unwrap_err();
    assert!(error.contains("cannot snapshot"), "{error}");
    let journal = ledger::read(&t.project.git_common_dir, receipt.instance_id)
        .unwrap()
        .unwrap()
        .journal;
    assert!(
        journal
            .iter()
            .any(|entry| matches!(entry.effect, Effect::Snapshot { .. })),
        "the attempt was journaled first: {journal:?}"
    );
}

/// The inventory looks for a branch where its record says it is, as removal
/// does, whatever the configuration says now.
#[test]
fn the_inventory_asks_the_backend_the_record_names_not_the_one_configured_now() {
    let id = Uuid::new_v4();
    let t = repo(Some(&format!(
        "[project]\nname = \"shop\"\nid = \"{id}\"\n\n[services.cache]\nengine = \"postgres\"\n\
         mode = \"external\"\nadmin_url = \"postgres://prov:pw@127.0.0.1:1/postgres\"\n"
    )));
    let instance = Uuid::new_v4();
    let mut backends = Backends::new(&t.project, false);
    backends.pg = Some(Err("the postgres server was asked".into()));
    backends.docker = Some(Err("the docker daemon was asked".into()));
    let mut asked = |service: Option<&str>, engine: Option<Engine>| {
        let branch = ledger::BranchRef {
            service: service.map(str::to_string),
            engine,
            id: Uuid::new_v4(),
            pending: false,
        };
        match backends.probe(&branch, instance).presence {
            Presence::Unreachable { reason } => reason,
            other => panic!("{other:?}"),
        }
    };
    // Recorded as a Valkey branch under a key that is configured as PostgreSQL
    // now: the container catalog is asked, not the server.
    assert!(asked(Some("cache"), Some(Engine::Valkey)).contains("docker daemon"));
    assert!(asked(Some("cache"), Some(Engine::Postgres)).contains("postgres server"));
    // A database an earlier build bound has no key: PostgreSQL.
    assert!(asked(None, None).contains("postgres server"));
    // No engine recorded: removal asks both and so does the inventory; the
    // configuration (which may have changed since) never places the branch.
    let unknown = asked(Some("cache"), None);
    assert!(
        unknown.contains("postgres server") || unknown.contains("docker daemon"),
        "{unknown}"
    );
}

#[test]
fn a_daemon_found_running_is_kept_on_the_record_even_when_the_record_never_listed_it() {
    let t = repo(None);
    // The project uses pitchfork, so a daemon may have been started by hand in
    // the namespace although devflow recorded none.
    std::fs::write(
        t.root.join("pitchfork.toml"),
        "[daemons.web]\nrun = \"x\"\n",
    )
    .unwrap();
    let receipt = removed_elsewhere(&t, "by-hand", false);
    let namespace = receipt.slug();
    let fake = Fake::new();
    fake.json(
        "list.json",
        &json!([listed(&namespace, "hand-made", "running")]),
    );
    let ask = |force: bool| {
        let target = inventory(&t.project, &quick())
            .unwrap()
            .entry(receipt.instance_id)
            .unwrap()
            .target()
            .unwrap();
        remove(
            &t.project,
            &RemoveRequest {
                targets: vec![target],
                pitchfork: Some(fake.pitchfork()),
                force,
                ..RemoveRequest::default()
            },
            &mut |_| {},
        )
        .unwrap()
    };
    let forced = ask(true);
    assert_eq!(forced.removed.len(), 1, "{:?}", forced.skipped);
    let seen = format!("{namespace}/hand-made");
    assert_eq!(
        forced.removed[0].still_recorded,
        [format!("the daemon {seen}")]
    );
    let record = ledger::read(&t.project.git_common_dir, receipt.instance_id)
        .unwrap()
        .expect("the daemon it found is named");
    assert!(record.resources.namespace_unverified);
    assert_eq!(record.resources.daemons, std::slice::from_ref(&seen));

    // It runs still: not forced, the removal is refused, with the id.
    let refused = ask(false);
    assert!(refused.removed.is_empty());
    assert!(
        refused.skipped[0].reason.contains(&seen),
        "{:?}",
        refused.skipped
    );

    // Stopped, as a listing that was read shows: the record lets go of it.
    fake.json(
        "list.json",
        &json!([listed(&namespace, "hand-made", "stopped")]),
    );
    let done = ask(false);
    assert_eq!(done.removed.len(), 1, "{:?}", done.skipped);
    assert!(
        ledger::read(&t.project.git_common_dir, receipt.instance_id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_namespace_in_doubt_is_cleared_only_by_a_listing_that_was_read_and_came_back_empty() {
    let t = repo(None);
    let pitchfork_toml = t.root.join("pitchfork.toml");
    std::fs::write(&pitchfork_toml, "[daemons.web]\nrun = \"x\"\n").unwrap();
    let receipt = removed_elsewhere(&t, "doubtful", false);
    let fake = Fake::new();
    fake.set("list_fail", "");
    let ask = |force: bool| {
        let target = inventory(&t.project, &quick())
            .unwrap()
            .entry(receipt.instance_id)
            .unwrap()
            .target()
            .unwrap();
        remove(
            &t.project,
            &RemoveRequest {
                targets: vec![target],
                pitchfork: Some(fake.pitchfork()),
                force,
                ..RemoveRequest::default()
            },
            &mut |_| {},
        )
        .unwrap()
    };
    // Gone on without knowing: the namespace stays named, with no id.
    let forced = ask(true);
    assert_eq!(forced.removed.len(), 1, "{:?}", forced.skipped);
    assert!(
        ledger::read(&t.project.git_common_dir, receipt.instance_id)
            .unwrap()
            .unwrap()
            .resources
            .namespace_unverified
    );

    // The project does not use pitchfork any more, and devflow recorded no
    // start: that used to be the end of the doubt, with no listing read.
    std::fs::remove_file(&pitchfork_toml).unwrap();
    let still_in_doubt = |why: &str| {
        let refused = ask(false);
        assert!(refused.removed.is_empty(), "{why}: {:?}", refused.removed);
        assert!(refused.skipped[0].forceable, "{why}");
        assert!(
            ledger::read(&t.project.git_common_dir, receipt.instance_id)
                .unwrap()
                .unwrap()
                .resources
                .namespace_unverified,
            "{why}: the doubt stays"
        );
    };
    // A listing that fails, a supervisor that is down: nothing was read.
    still_in_doubt("listing fails");
    std::fs::remove_file(fake.dir.path().join("list_fail")).unwrap();
    fake.set("supervisor_down", "");
    still_in_doubt("supervisor down");
    // No pitchfork to ask at all.
    let checked = processes::check_recorded(None, &receipt.slug(), &[], false, false, true);
    assert!(checked.is_err(), "no pitchfork: {checked:?}");
    assert!(
        processes::check_recorded(None, &receipt.slug(), &[], false, false, false)
            .unwrap()
            .verified,
        "while it was never in doubt, nothing recorded means nothing to look for"
    );

    // A listing that was read and shows nothing running clears it.
    std::fs::remove_file(fake.dir.path().join("supervisor_down")).unwrap();
    let done = ask(false);
    assert_eq!(done.removed.len(), 1, "{:?}", done.skipped);
    assert!(done.removed[0].still_recorded.is_empty());
    assert!(
        ledger::read(&t.project.git_common_dir, receipt.instance_id)
            .unwrap()
            .is_none()
    );
}
