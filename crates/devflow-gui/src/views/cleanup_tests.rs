//! Headless tests of the Clean up page and the banner that leads to it.
//!
//! The workspaces here are real: worktrees created through core, with their
//! records in the repository's ledger, then removed the way an agent host
//! removes them (the worktree and its receipt go, the record stays). Daemons are
//! a fake `pitchfork`; the one backend is an external PostgreSQL server at a
//! closed port. Nothing needs Docker.
use super::{
    cleanup::{confirmation_lines, facts},
    dx_tests::Fake,
    palette::PaletteAction,
    remedies,
    setup_tests::{click, exists, id, visible, wait},
    ui::Tone,
    ui_tests::{
        Harness, Scratch, frame, init_scratch_repo, open_scratch, scratch_project, settle, update,
    },
};
use crate::model::Page;
use chrono::Utc;
use devflow_core::{
    gc::{self, Cause, Class},
    ledger,
    project::Project,
    receipt::{DataBinding, ProcessRecord, Receipt, ServiceBinding},
    remedy::Remedy,
    worktree,
};
use gpui_kit::{AppContext as _, TestAppContext, test::TestWindowExt as _};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use uuid::Uuid;

/// Whether text tells the person to run a command: `devflow` followed by one
/// of its subcommands, or a command in backticks. The product's name in a
/// sentence is not one.
fn names_a_command(text: &str) -> bool {
    const SUBCOMMANDS: [&str; 16] = [
        "gc", "remove", "setup", "switch", "list", "db", "service", "up", "down", "trust",
        "doctor", "prune", "hook", "hooks", "init", "exec",
    ];
    text.contains("`devflow")
        || SUBCOMMANDS
            .iter()
            .any(|sub| text.contains(&format!("devflow {sub}")))
}

fn project_of(scratch: &Scratch) -> Project {
    Project::open(&scratch.entry.root).unwrap()
}

/// A workspace Git forgot: its worktree was created and removed, and the record
/// of what it owns outlives it. With `daemon`, it owns a daemon devflow started.
fn forgotten(project: &Project, name: &str, daemon: bool) -> Receipt {
    let ws = worktree::create(project, name, None).unwrap();
    let mut receipt = Receipt::new(project.id(), name, None);
    if daemon {
        receipt.processes = Some(ProcessRecord {
            started_at: Utc::now(),
            ids: vec![format!("{}/web", receipt.slug())],
        });
    }
    receipt.save(&ws.git_dir).unwrap();
    worktree::force_remove(project, &ws).unwrap();
    receipt
}

fn short(receipt: &Receipt) -> String {
    devflow_core::ids::short(receipt.instance_id)
}

fn listed(namespace: &str, status: &str) -> Value {
    json!({
        "id": format!("{namespace}/web"), "namespace": namespace, "name": "web",
        "pid": if status == "running" { json!(4242) } else { Value::Null },
        "status": status, "oneshot": false, "disabled": false, "available": false,
        "proxy_url": null, "url": null, "error": null, "port": [],
    })
}

/// Open the page the way a remedy's button does, and wait for the inventory.
fn open_page(cx: &mut TestAppContext, h: &Harness, root: &Path) {
    let remedy = Remedy::CleanUp {
        root: root.to_path_buf(),
    };
    cx.update_window(h.window.into(), |_, window, cx| {
        remedies::run_scoped(&h.state, &remedy, window, cx)
    })
    .unwrap();
    wait(cx, "the inventory is read", |cx| {
        cx.read_entity(&h.state, |s, _| {
            s.page == Page::Cleanup && !s.cleanup.loading && s.cleanup.read().is_some()
        })
    });
}

/// Click "Remove selected…" until the question opens. A dialog that was just
/// closed leaves its layer over the page for a moment, and a click that lands
/// on it does nothing.
fn ask_to_remove(cx: &mut TestAppContext, h: &Harness) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        click(cx, h, "cleanup-remove");
        if exists(cx, h, "confirm") {
            return;
        }
        cx.executor().advance_clock(Duration::from_millis(150));
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the question never opened");
}

/// Click `button` of the question (`confirm` or `cancel`) until it takes: the
/// dialog's layer is not always in place the moment its buttons exist (a loaded
/// machine lays it out late), and a click that lands before does nothing. It
/// has taken once the question is gone or, for the answer, the job runs.
fn answer(cx: &mut TestAppContext, h: &Harness, button: &'static str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        click(cx, h, button);
        cx.executor().advance_clock(Duration::from_millis(150));
        cx.run_until_parked();
        if !exists(cx, h, "confirm") || cx.read_entity(&h.state, |s, _| s.busy().is_some()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the question never took the answer ({button})");
}

fn confirm_removal(cx: &mut TestAppContext, h: &Harness) {
    answer(cx, h, "confirm");
}

fn picked(cx: &mut TestAppContext, h: &Harness) -> usize {
    cx.read_entity(&h.state, |s, _| s.cleanup.selected.len())
}

fn instances(cx: &mut TestAppContext, h: &Harness) -> Vec<Uuid> {
    cx.read_entity(&h.state, |s, _| {
        s.cleanup
            .read()
            .map(|inventory| {
                inventory
                    .entries
                    .iter()
                    .filter_map(|e| e.instance)
                    .collect()
            })
            .unwrap_or_default()
    })
}

#[test]
fn an_entry_and_the_question_before_removing_it_are_plain_words() {
    let outer = tempfile::tempdir().unwrap();
    let entry = init_scratch_repo(&outer.path().join("shop"));
    let project = Project::open(&entry.root).unwrap();
    let receipt = forgotten(&project, "feature/login", true);
    let inventory = gc::inventory(
        &project,
        &gc::Options {
            measure: false,
            evidence: true,
            backfill: false,
        },
    )
    .unwrap();
    let found = inventory.entry(receipt.instance_id).unwrap();
    assert_eq!(found.class, Class::RemovedElsewhere);

    let facts = facts(found);
    assert_eq!(facts.name, "feature/login");
    assert_eq!(facts.owns, [format!("daemon {}/web", receipt.slug())]);
    assert_eq!(facts.activity.as_deref(), Some("last active just now"));
    // Its branch has no commits of its own: merged, said without a hint.
    assert_eq!(
        facts.merge,
        Some((Tone::Success, "merged into main".to_string()))
    );
    assert!(!facts.unfinished);

    let question = confirmation_lines(&[found], false);
    assert_eq!(question[0], "Remove the workspace feature/login?");
    assert!(
        question[1].contains(&format!(
            "checks that the daemon {}/web is not running",
            receipt.slug()
        )),
        "{question:?}"
    );
    assert!(question.contains(&"Dropped data cannot be recovered.".to_string()));
    let snapshotting = confirmation_lines(&[found], true);
    assert!(
        snapshotting
            .contains(&"A snapshot of each data branch is taken first and kept.".to_string())
    );
    assert!(!snapshotting.contains(&"Dropped data cannot be recovered.".to_string()));
    // A registered worktree whose directory is gone also loses its registration.
    let mut missing = found.clone();
    missing.class = Class::Missing;
    assert!(confirmation_lines(&[&missing], false)[1].contains("forgets its Git registration"));
    assert_eq!(
        confirmation_lines(&[found, &missing], false)[0],
        "Remove these 2 workspaces?"
    );
    // Nothing the page says is a command line.
    let all: Vec<String> = facts
        .lines()
        .into_iter()
        .chain(question)
        .chain(snapshotting)
        .collect();
    for line in &all {
        assert!(!names_a_command(line), "{line}");
    }
}

#[gpui_kit::test]
fn the_banner_leads_to_a_page_that_lists_the_forgotten_workspace_for_picking(
    cx: &mut TestAppContext,
) {
    let scratch = scratch_project();
    let project = project_of(&scratch);
    let receipt = forgotten(&project, "feature/login", true);
    let key = short(&receipt);
    let h = open_scratch(cx, &scratch);

    // The Workspaces tab says it, from files alone, with the one button.
    wait(cx, "the banner", |cx| exists(cx, &h, "cleanup-banner"));
    assert!(visible(cx, &h, "cleanup-banner-open"));
    cx.read_entity(&h.state, |s, _| {
        let summary = s.selection.current().unwrap().cleanup.clone().unwrap();
        assert_eq!(summary.removed_elsewhere, ["feature/login"]);
        let sentence = summary.sentence().unwrap();
        assert!(!names_a_command(&sentence), "{sentence}");
    });

    click(cx, &h, "cleanup-banner-open");
    wait(cx, "the page", |cx| {
        cx.read_entity(&h.state, |s, _| {
            s.page == Page::Cleanup && !s.cleanup.loading && s.cleanup.read().is_some()
        })
    });
    frame(cx, &h, |window| {
        assert!(window.find(id(format!("cleanup-row-{key}"))).visible());
        assert!(window.find("cleanup-group-removed elsewhere").visible());
        let checkbox = window.find(id(format!("cleanup-select-{key}")));
        assert!(checkbox.visible());
        assert_eq!(checkbox.checked(), Some(false));
        assert!(window.try_find("cleanup-nothing").is_none());
    });
    assert_eq!(picked(cx, &h), 0);
    // Nothing is picked: asking to remove asks nothing.
    click(cx, &h, "cleanup-remove");
    assert!(!exists(cx, &h, "confirm"));

    // Picking enables the removal; select all toggles.
    click(cx, &h, id(format!("cleanup-select-{key}")));
    assert_eq!(picked(cx, &h), 1);
    frame(cx, &h, |window| {
        assert_eq!(
            window.find(id(format!("cleanup-select-{key}"))).checked(),
            Some(true)
        );
    });
    ask_to_remove(cx, &h);
    answer(cx, &h, "cancel");
    wait(cx, "the question closed", |cx| !exists(cx, &h, "confirm"));
    assert_eq!(picked(cx, &h), 1, "cancelling keeps the pick");
    click(cx, &h, "cleanup-select-all");
    assert_eq!(picked(cx, &h), 0, "with something picked it unpicks");
    click(cx, &h, "cleanup-select-all");
    assert_eq!(picked(cx, &h), 1);

    // The snapshot choice is the person's and stays across a read.
    click(cx, &h, "cleanup-snapshot");
    assert!(cx.read_entity(&h.state, |s, _| s.cleanup.snapshot));
    click(cx, &h, "cleanup-refresh");
    wait(cx, "the second read", |cx| {
        cx.read_entity(&h.state, |s, _| !s.cleanup.loading)
    });
    assert!(cx.read_entity(&h.state, |s, _| s.cleanup.snapshot));
    assert_eq!(picked(cx, &h), 1, "its record did not change");
}

#[gpui_kit::test]
fn an_entry_core_cannot_identify_has_no_checkbox_and_its_remedies_are_buttons(
    cx: &mut TestAppContext,
) {
    let scratch = scratch_project();
    let root = scratch.entry.root.clone();
    // A database server nobody can reach, configured after the workspace exists.
    let config = root.join("devflow.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    // kingfisher:ignore
    let admin = "admin_url = \"postgres://prov:pw@127.0.0.1:1/postgres\"\n";
    std::fs::write(
        &config,
        format!("{text}\n[services.db]\nengine = \"postgres\"\nmode = \"external\"\n{admin}"),
    )
    .unwrap();
    let project = project_of(&scratch);
    let ws = worktree::create(&project, "feature/data", None).unwrap();
    let mut receipt = Receipt::new(project.id(), "feature/data", None);
    receipt.services.insert(
        "db".into(),
        ServiceBinding::postgres(
            DataBinding {
                branch_id: Some(Uuid::new_v4()),
                database: "df_x".into(),
                role: "role".into(),
                url: "postgres://role@127.0.0.1:1/df_x".into(),
                source: "empty".into(),
                owned: true,
            },
            None,
        ),
    );
    receipt.save(&ws.git_dir).unwrap();
    worktree::force_remove(&project, &ws).unwrap();
    let key = short(&receipt);

    let h = open_scratch(cx, &scratch);
    open_page(cx, &h, &root);
    let entry = cx.read_entity(&h.state, |s, _| {
        s.cleanup
            .read()
            .unwrap()
            .entry(receipt.instance_id)
            .unwrap()
            .clone()
    });
    assert_eq!(entry.class, Class::Unknown);
    assert_eq!(entry.cause, Some(Cause::ServerUnreachable));
    frame(cx, &h, |window| {
        assert!(window.find("cleanup-group-unknown").visible());
        assert!(window.find(id(format!("cleanup-row-{key}"))).visible());
        assert!(
            window
                .try_find(id(format!("cleanup-select-{key}")))
                .is_none(),
            "nothing to pick"
        );
        // Core's remedies, as buttons.
        assert!(
            window
                .find(id(format!("cleanup-remedy-{key}-0-start_database")))
                .visible()
        );
        assert!(
            window
                .find(id(format!("cleanup-remedy-{key}-1-retry")))
                .visible()
        );
    });
    // Nothing can be picked, by the button or by any other way.
    click(cx, &h, "cleanup-select-all");
    click(cx, &h, "cleanup-remove");
    assert!(!exists(cx, &h, "confirm"));
    assert_eq!(picked(cx, &h), 0);
    update(cx, &h, |s, cx| {
        s.pick_cleanup(receipt.instance_id, true, cx);
        s.pick_all_cleanup(true, cx);
    });
    assert_eq!(picked(cx, &h), 0);
    // What the entry and its buttons say is not a command line.
    let mut text = facts(&entry).lines();
    text.extend(entry.remedies.iter().map(Remedy::label));
    for line in &text {
        assert!(!names_a_command(line), "{line}");
    }
}

#[gpui_kit::test]
fn confirming_removes_exactly_the_picked_workspace_and_a_running_daemon_keeps_its_workspace(
    cx: &mut TestAppContext,
) {
    let scratch = scratch_project();
    let root = scratch.entry.root.clone();
    let project = project_of(&scratch);
    let (a, b) = (
        forgotten(&project, "feature/a", true),
        forgotten(&project, "feature/b", true),
    );
    // a's daemon is not running any more; b's is. devflow only asks pitchfork:
    // it never stops a daemon of a workspace that has no checkout to approve.
    let fake = Fake::new();
    fake.set(
        "list.json",
        &json!([listed(&a.slug(), "stopped"), listed(&b.slug(), "running")]).to_string(),
    );
    let h = open_scratch(cx, &scratch);
    update(cx, &h, |s, _| s.pitchfork = fake.source());
    open_page(cx, &h, &root);
    assert_eq!(instances(cx, &h).len(), 2);

    // Picking one and asking: the question names it and not the other.
    click(cx, &h, id(format!("cleanup-select-{}", short(&a))));
    assert_eq!(picked(cx, &h), 1);
    ask_to_remove(cx, &h);
    let question = cx.read_entity(&h.state, |s, _| {
        super::cleanup::confirmation_lines(&s.cleanup.picked(), s.cleanup.snapshot).join("\n")
    });
    assert!(
        question.contains("feature/a") && !question.contains("feature/b"),
        "{question}"
    );
    assert!(
        question.contains("checks that the daemon") && question.contains("is not stopped here"),
        "{question}"
    );
    // Nothing happens before the answer.
    assert!(fake.calls().is_empty() || fake.calls().iter().all(|call| !call.starts_with("stop")));
    assert!(
        ledger::read(&project.git_common_dir, a.instance_id)
            .unwrap()
            .is_some()
    );
    confirm_removal(cx, &h);
    settle(cx, &h, |s| {
        s.busy().is_none()
            && !s.cleanup.loading
            && s.activity
                .as_ref()
                .is_some_and(|a| a.title == "Clean up workspaces" && a.outcome.is_some())
    });

    // It dropped a's record and left b; it stopped nothing.
    assert!(
        fake.calls().iter().all(|call| !call.starts_with("stop")),
        "{:?}",
        fake.calls()
    );
    assert!(
        ledger::read(&project.git_common_dir, a.instance_id)
            .unwrap()
            .is_none()
    );
    assert!(
        ledger::read(&project.git_common_dir, b.instance_id)
            .unwrap()
            .is_some()
    );
    cx.read_entity(&h.state, |s, _| {
        let outcome = s.activity.as_ref().unwrap().outcome.as_ref().unwrap();
        let done = outcome.as_ref().expect("the clean-up worked");
        assert!(done.summary.contains("feature/a"), "{}", done.summary);
        assert!(!done.summary.contains("feature/b"), "{}", done.summary);
        assert!(s.cleanup.selected.is_empty());
    });
    // The page was read again: only b is left, and the banner counts one.
    assert_eq!(instances(cx, &h), [b.instance_id]);
    frame(cx, &h, |window| {
        assert!(
            window
                .try_find(id(format!("cleanup-row-{}", short(&a))))
                .is_none()
        );
        assert!(
            window
                .find(id(format!("cleanup-row-{}", short(&b))))
                .visible()
        );
    });
    settle(cx, &h, |s| {
        s.selection.current().is_some_and(|m| {
            m.cleanup
                .as_ref()
                .is_some_and(|c| c.removed_elsewhere == ["feature/b"])
        })
    });

    // b's daemon runs: asking to remove it is refused, with core's reason, and
    // nothing of b is lost or stopped.
    click(cx, &h, id(format!("cleanup-select-{}", short(&b))));
    assert_eq!(picked(cx, &h), 1);
    ask_to_remove(cx, &h);
    confirm_removal(cx, &h);
    // (The first job's outcome is the activity's until this one ends.)
    settle(cx, &h, |s| {
        s.busy().is_none()
            && !s.cleanup.loading
            && s.activity.as_ref().is_some_and(|a| {
                a.outcome
                    .as_ref()
                    .is_some_and(|o| o.as_ref().is_ok_and(|d| d.summary == "Nothing was removed"))
            })
    });
    cx.read_entity(&h.state, |s, _| {
        let outcome = s.activity.as_ref().unwrap().outcome.as_ref().unwrap();
        let done = outcome.as_ref().expect("the job itself worked");
        assert_eq!(done.summary, "Nothing was removed");
        assert!(done.needs_attention);
        let kept = done
            .warnings
            .iter()
            .find(|w| w.starts_with("kept feature/b"))
            .unwrap_or_else(|| panic!("{:?}", done.warnings));
        assert!(!names_a_command(kept), "{kept}");
    });
    assert!(
        fake.calls().iter().all(|call| !call.starts_with("stop")),
        "{:?}",
        fake.calls()
    );
    let kept = ledger::read(&project.git_common_dir, b.instance_id)
        .unwrap()
        .expect("b is still recorded");
    assert_eq!(kept.resources.daemons, [format!("{}/web", b.slug())]);
    assert_eq!(instances(cx, &h), [b.instance_id], "the page still lists b");
}

#[gpui_kit::test]
fn an_entry_whose_record_changed_after_it_was_shown_is_kept_and_said_so(cx: &mut TestAppContext) {
    let scratch = scratch_project();
    let root = scratch.entry.root.clone();
    let project = project_of(&scratch);
    let receipt = forgotten(&project, "feature/shifting", true);
    let fake = Fake::new();
    let h = open_scratch(cx, &scratch);
    update(cx, &h, |s, _| s.pitchfork = fake.source());
    open_page(cx, &h, &root);
    click(cx, &h, id(format!("cleanup-select-{}", short(&receipt))));
    assert_eq!(picked(cx, &h), 1);

    // Something else records another daemon for it before the answer.
    let common = project.git_common_dir.clone();
    let mut record = ledger::read(&common, receipt.instance_id).unwrap().unwrap();
    record.resources.daemons.push("df-other/web".into());
    std::fs::write(
        ledger::path(&common, receipt.instance_id),
        serde_json::to_vec_pretty(&record).unwrap(),
    )
    .unwrap();

    ask_to_remove(cx, &h);
    confirm_removal(cx, &h);
    settle(cx, &h, |s| {
        s.busy().is_none()
            && !s.cleanup.loading
            && s.activity
                .as_ref()
                .is_some_and(|a| a.title == "Clean up workspaces" && a.outcome.is_some())
    });

    cx.read_entity(&h.state, |s, _| {
        let outcome = s.activity.as_ref().unwrap().outcome.as_ref().unwrap();
        let done = outcome.as_ref().expect("the job itself worked");
        assert_eq!(done.summary, "Nothing was removed");
        assert!(done.needs_attention);
        assert!(
            done.warnings
                .iter()
                .any(|w| w.contains("kept feature/shifting")
                    && w.contains("changed since it was shown")),
            "{:?}",
            done.warnings
        );
        // The pick belonged to the record that was shown: it is gone with it.
        assert!(s.cleanup.selected.is_empty());
    });
    assert!(fake.calls().iter().all(|call| !call.starts_with("stop")));
    assert_eq!(
        ledger::read(&common, receipt.instance_id)
            .unwrap()
            .expect("nothing was removed")
            .resources
            .daemons
            .len(),
        2
    );
    // The page shows the record as it is now, and it can be picked again.
    assert_eq!(instances(cx, &h), [receipt.instance_id]);
}

#[gpui_kit::test]
fn the_banner_comes_with_a_forgotten_workspace_and_goes_with_its_removal(cx: &mut TestAppContext) {
    let scratch = scratch_project();
    let project = project_of(&scratch);
    let h = open_scratch(cx, &scratch);
    assert!(
        !exists(cx, &h, "cleanup-banner"),
        "nothing is forgotten yet"
    );

    let receipt = forgotten(&project, "feature/gone", true);
    update(cx, &h, |s, cx| s.refresh(cx));
    settle(cx, &h, |s| {
        s.selection
            .current()
            .is_some_and(|m| m.cleanup.as_ref().is_some_and(|c| c.count() == 1) && !m.refreshing)
    });
    assert!(visible(cx, &h, "cleanup-banner"));

    // Removed by anyone: the next refresh takes the banner away.
    let fake = Fake::new();
    let inventory = gc::inventory(
        &project,
        &gc::Options {
            measure: false,
            evidence: false,
            backfill: false,
        },
    )
    .unwrap();
    let target = inventory
        .entry(receipt.instance_id)
        .unwrap()
        .target()
        .unwrap();
    let outcome = gc::remove(
        &project,
        &gc::RemoveRequest {
            targets: vec![target],
            pitchfork: Some(fake.pitchfork()),
            ..gc::RemoveRequest::default()
        },
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(outcome.removed.len(), 1, "{:?}", outcome.skipped);
    update(cx, &h, |s, cx| s.refresh(cx));
    settle(cx, &h, |s| {
        s.selection
            .current()
            .is_some_and(|m| m.cleanup.as_ref().is_some_and(|c| c.count() == 0) && !m.refreshing)
    });
    assert!(!exists(cx, &h, "cleanup-banner"));
}

#[gpui_kit::test]
fn an_empty_inventory_says_nothing_was_removed_outside_devflow(cx: &mut TestAppContext) {
    let scratch = scratch_project();
    let root = scratch.entry.root.clone();
    let h = open_scratch(cx, &scratch);
    open_page(cx, &h, &root);
    frame(cx, &h, |window| {
        assert!(window.find("cleanup-nothing").visible());
    });
    click(cx, &h, "cleanup-select-all");
    click(cx, &h, "cleanup-remove");
    assert!(!exists(cx, &h, "confirm"));
    assert_eq!(picked(cx, &h), 0);
}

#[gpui_kit::test]
fn the_palette_opens_the_page_and_a_remedy_for_another_project_does_not(cx: &mut TestAppContext) {
    let scratch = scratch_project();
    let root = scratch.entry.root.clone();
    let h = open_scratch(cx, &scratch);
    cx.update_window(h.window.into(), |_, window, cx| {
        PaletteAction::CleanUp.run(&h.state, window, cx)
    })
    .unwrap();
    wait(cx, "the page", |cx| {
        cx.read_entity(&h.state, |s, _| {
            s.page == Page::Cleanup && s.cleanup.read().is_some() && !s.cleanup.loading
        })
    });
    assert_eq!(
        cx.read_entity(&h.state, |s, _| s.cleanup.project),
        Some(scratch.entry.id)
    );

    // Back on the project, a remedy that is about some other folder is refused.
    update(cx, &h, |s, cx| {
        s.go_to_tab(crate::model::Tab::Workspaces, cx)
    });
    let elsewhere = Remedy::CleanUp {
        root: root.join("not-the-open-project"),
    };
    cx.update_window(h.window.into(), |_, window, cx| {
        remedies::run_scoped(&h.state, &elsewhere, window, cx)
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        cx.read_entity(&h.state, |s, _| s.page),
        Page::Project,
        "the page stays closed"
    );
}

#[gpui_kit::test]
fn a_record_that_may_be_a_copys_is_bound_to_the_repository_by_a_button_the_person_confirms(
    cx: &mut TestAppContext,
) {
    let scratch = scratch_project();
    let root = scratch.entry.root.clone();
    let project = project_of(&scratch);
    let receipt = forgotten(&project, "feature/restored", false);
    // The repository was restored to this path: the record is another
    // directory's.
    let common = project.git_common_dir.clone();
    let mut record = ledger::read(&common, receipt.instance_id).unwrap().unwrap();
    record.checkout.identity = Some("12345:678".into());
    std::fs::write(
        ledger::path(&common, receipt.instance_id),
        serde_json::to_vec_pretty(&record).unwrap(),
    )
    .unwrap();

    let h = open_scratch(cx, &scratch);
    update(cx, &h, |s, _| s.pitchfork = Fake::new().source());
    open_page(cx, &h, &root);
    let key = short(&receipt);

    // It is listed with why, as unknown, and cannot be picked.
    let (reason, remedies) = cx.read_entity(&h.state, |s, _| {
        let entry = s
            .cleanup
            .read()
            .unwrap()
            .entry(receipt.instance_id)
            .unwrap()
            .clone();
        assert_eq!(entry.class, Class::Unknown);
        assert!(!entry.removable && entry.rebindable());
        (entry.reason.clone().unwrap_or_default(), entry.remedies)
    });
    assert!(!names_a_command(&reason), "{reason}");
    let index = remedies
        .iter()
        .position(|r| matches!(r, Remedy::RebindRecord { .. }))
        .expect("core offers to bind it");
    let button = super::remedies::id(&format!("cleanup-remedy-{key}"), index, &remedies[index]);
    assert!(!names_a_command(&remedies[index].label()));
    frame(cx, &h, |window| {
        assert!(window.find(button.clone()).visible());
        assert!(
            window
                .try_find(id(format!("cleanup-select-{key}")))
                .is_none(),
            "an unknown record is not offered for removal"
        );
    });

    // The button asks first, and nothing is bound before the answer.
    click(cx, &h, button.clone());
    wait(cx, "the question opens", |cx| exists(cx, &h, "confirm"));
    assert_eq!(
        ledger::read(&common, receipt.instance_id)
            .unwrap()
            .unwrap()
            .checkout
            .identity
            .as_deref(),
        Some("12345:678")
    );
    answer(cx, &h, "cancel");
    wait(cx, "the question closed", |cx| !exists(cx, &h, "confirm"));
    assert_eq!(
        ledger::read(&common, receipt.instance_id)
            .unwrap()
            .unwrap()
            .checkout
            .identity
            .as_deref(),
        Some("12345:678"),
        "cancelling binds nothing"
    );

    // Confirmed: bound, nothing removed, and the page shows it as any other
    // record now, with its checkbox.
    click(cx, &h, button);
    wait(cx, "the question opens", |cx| exists(cx, &h, "confirm"));
    confirm_removal(cx, &h);
    settle(cx, &h, |s| {
        s.busy().is_none()
            && !s.cleanup.loading
            && s.activity.as_ref().is_some_and(|a| {
                a.title == "Bind a record to this repository" && a.outcome.is_some()
            })
    });
    cx.read_entity(&h.state, |s, _| {
        let outcome = s.activity.as_ref().unwrap().outcome.as_ref().unwrap();
        let done = outcome.as_ref().expect("the job worked");
        assert!(
            done.summary.contains("feature/restored"),
            "{}",
            done.summary
        );
        let entry = s
            .cleanup
            .read()
            .unwrap()
            .entry(receipt.instance_id)
            .unwrap();
        assert_eq!(entry.class, Class::RemovedElsewhere);
        assert!(entry.removable);
    });
    assert!(
        ledger::read(&common, receipt.instance_id)
            .unwrap()
            .is_some(),
        "binding removes nothing"
    );
    frame(cx, &h, |window| {
        assert!(window.find(id(format!("cleanup-select-{key}"))).visible());
    });
}
