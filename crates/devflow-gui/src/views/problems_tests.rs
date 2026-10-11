//! Projects that cannot be opened, driven through the real window against the
//! real core in scratch repositories and an isolated registry: the page says
//! what is wrong, each button does what its label says, and the folder and
//! data of a project are never touched by taking it off the list.
use super::{
    setup_tests::{click, exists, id, visible, wait},
    sidebar::problem_badge_id,
    ui_tests::{Harness, Scratch, another_scratch_project, frame, open, scratch_project, update},
};
use crate::{jobs, model::LoadPhase, state::AppState};
use devflow_core::{
    health::{ProblemKind, ProjectProblem},
    registry::{self, Entry},
    remedy::Remedy,
};
use gpui_kit::{AppContext as _, TestAppContext};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const PRE_RELEASE: &str = "[postgres]\nmode = \"managed\"\nimage = \"postgres:18\"\n";

/// The app over the registry as it is now, with `selected` shown.
fn open_registry(cx: &mut TestAppContext, selected: &Entry) -> Harness {
    let entries = registry::list().unwrap();
    let problems = jobs::check_projects(&entries);
    let shown = selected.clone();
    let h = open(cx, move |state| {
        state.inflight = 0;
        state.entries = entries;
        state.problems = problems;
        state.selection.select(shown);
    });
    update(cx, &h, |s, cx| s.refresh(cx));
    h
}

fn phase(cx: &mut TestAppContext, h: &Harness) -> Option<LoadPhase> {
    cx.read_entity(&h.state, |s, _| {
        s.selection.current().map(|m| m.phase.clone())
    })
}

fn problem_kind(cx: &mut TestAppContext, h: &Harness) -> Option<ProblemKind> {
    match phase(cx, h) {
        Some(LoadPhase::Problem(problem)) => Some(problem.kind),
        _ => None,
    }
}

fn wait_for_problem(cx: &mut TestAppContext, h: &Harness, kind: ProblemKind) {
    wait(cx, "the project reads as a problem", |cx| {
        problem_kind(cx, h) == Some(kind)
    });
}

fn listed(cx: &mut TestAppContext, h: &Harness) -> Vec<Uuid> {
    cx.read_entity(&h.state, |s, _| s.entries.iter().map(|e| e.id).collect())
}

/// Replace `devflow.toml` of a scratch project with the pre-release layout
/// (same project id and name).
fn make_pre_release(entry: &Entry) {
    let text = format!(
        "[project]\nname = \"{}\"\nid = \"{}\"\n\n{PRE_RELEASE}",
        entry.name, entry.id
    );
    std::fs::write(entry.root.join("devflow.toml"), text).unwrap();
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

#[gpui_kit::test]
fn a_missing_folder_gets_a_page_a_badge_and_a_confirmed_removal(cx: &mut TestAppContext) {
    let scratch = scratch_project();
    let entry = scratch.entry.clone();
    let other = another_scratch_project(&scratch, "blog");
    std::fs::remove_dir_all(&entry.root).unwrap();

    let h = open_registry(cx, &entry);
    wait_for_problem(cx, &h, ProblemKind::FolderMissing);
    frame(cx, &h, |_| {});
    assert!(exists(cx, &h, "project-problem"));
    assert!(visible(cx, &h, "remedy-locate_project"));
    assert!(visible(cx, &h, "remedy-forget_project"));
    assert!(!exists(cx, &h, "remedy-migrate_config"));
    assert!(exists(cx, &h, problem_badge_id(entry.id)));
    assert!(
        !exists(cx, &h, problem_badge_id(other.id)),
        "a healthy project has none"
    );

    // Removing asks first; nothing happens until it is confirmed.
    click(cx, &h, "remedy-forget_project");
    assert!(exists(cx, &h, "confirm"));
    assert!(listed(cx, &h).contains(&entry.id));
    click(cx, &h, "confirm");
    wait(cx, "the project left the list", |cx| {
        !listed(cx, &h).contains(&entry.id)
    });
    assert!(listed(cx, &h).contains(&other.id));
    let in_registry = registry::list().unwrap();
    assert!(in_registry.iter().all(|e| e.id != entry.id));
    assert!(in_registry.iter().any(|e| e.id == other.id));
    let _ = scratch;
}

#[gpui_kit::test]
fn removing_a_project_from_the_list_leaves_its_folder_and_data_alone(cx: &mut TestAppContext) {
    let scratch = scratch_project();
    let entry = scratch.entry.clone();
    make_pre_release(&entry);
    let before = read(&entry.root.join("devflow.toml"));

    let h = open_registry(cx, &entry);
    wait_for_problem(cx, &h, ProblemKind::PreReleaseConfig);
    click(cx, &h, "remedy-forget_project");
    click(cx, &h, "confirm");
    wait(cx, "the project left the list", |cx| {
        !listed(cx, &h).contains(&entry.id)
    });
    assert!(entry.root.join(".git").is_dir());
    assert_eq!(read(&entry.root.join("devflow.toml")), before);
    let _ = scratch;
}

#[gpui_kit::test]
fn several_missing_folders_leave_the_list_in_one_confirmed_step(cx: &mut TestAppContext) {
    let scratch = scratch_project();
    let healthy = scratch.entry.clone();
    let gone = [
        another_scratch_project(&scratch, "blog"),
        another_scratch_project(&scratch, "wiki"),
    ];
    for entry in &gone {
        std::fs::remove_dir_all(&entry.root).unwrap();
    }
    let h = open_registry(cx, &healthy);
    wait(cx, "the project is ready", |cx| {
        matches!(phase(cx, &h), Some(LoadPhase::Ready))
    });

    frame(cx, &h, |_| {});
    assert!(visible(cx, &h, super::sidebar::FORGET_MISSING_ID));
    click(cx, &h, super::sidebar::FORGET_MISSING_ID);
    assert!(exists(cx, &h, "confirm"));
    // The dialog slides in: let it settle before clicking inside it.
    frame(cx, &h, |_| {});
    click(cx, &h, "confirm");
    wait(cx, "the missing projects left the list", |cx| {
        let ids = listed(cx, &h);
        ids == vec![healthy.id]
    });
    assert_eq!(registry::list().unwrap().len(), 1);
}

#[gpui_kit::test]
fn a_project_that_moved_is_found_again_and_a_wrong_folder_is_refused(cx: &mut TestAppContext) {
    let scratch = scratch_project();
    let entry = scratch.entry.clone();
    let moved = scratch._outer.path().join("moved");
    std::fs::rename(&entry.root, &moved).unwrap();
    let moved = moved.canonicalize().unwrap();

    let h = open_registry(cx, &entry);
    wait_for_problem(cx, &h, ProblemKind::FolderMissing);

    // Another project's folder is not this project: refused, nothing changes.
    let stranger = another_scratch_project(&scratch, "stranger");
    locate(cx, &h, entry.id, stranger.root.clone());
    for _ in 0..20 {
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(150));
        cx.run_until_parked();
    }
    assert_eq!(
        registry::find(&entry.id.to_string()).unwrap().root,
        entry.root
    );
    assert_eq!(problem_kind(cx, &h), Some(ProblemKind::FolderMissing));

    // The right folder is accepted and the project opens.
    locate(cx, &h, entry.id, moved.clone());
    wait(cx, "the project opens from its new place", |cx| {
        matches!(phase(cx, &h), Some(LoadPhase::Ready))
    });
    assert_eq!(registry::find(&entry.id.to_string()).unwrap().root, moved);
    cx.read_entity(&h.state, |s, _| {
        assert_eq!(s.selection.current().unwrap().entry.root, moved);
        assert!(s.problems.is_empty(), "{:?}", s.problems);
    });
}

fn locate(cx: &mut TestAppContext, h: &Harness, id: Uuid, root: PathBuf) {
    let state = h.state.clone();
    cx.update_window(h.window.into(), |_, window, cx| {
        state.update(cx, |s, cx| s.relocate_project(id, root, window, cx))
    })
    .unwrap();
}

#[gpui_kit::test]
fn an_earlier_builds_configuration_is_migrated_from_its_page(cx: &mut TestAppContext) {
    let scratch = scratch_project();
    let entry = scratch.entry.clone();
    make_pre_release(&entry);

    let h = open_registry(cx, &entry);
    wait_for_problem(cx, &h, ProblemKind::PreReleaseConfig);
    frame(cx, &h, |_| {});
    assert!(visible(cx, &h, "remedy-migrate_config"));
    assert!(exists(cx, &h, problem_badge_id(entry.id)));

    // The dialog shows the exact change before anything is written.
    click(cx, &h, "remedy-migrate_config");
    wait(cx, "the change is shown", |cx| {
        exists(cx, &h, "migration-review")
    });
    let path = entry.root.join("devflow.toml");
    assert!(read(&path).contains("[postgres]"), "nothing is written yet");

    click(cx, &h, "migration-apply");
    wait(cx, "the project opens", |cx| {
        matches!(phase(cx, &h), Some(LoadPhase::Ready))
    });
    let text = read(&path);
    assert!(
        text.contains("[services.db]\nengine = \"postgres\""),
        "{text}"
    );
    assert!(!text.contains("\n[postgres]"), "{text}");
    cx.read_entity(&h.state, |s, _| {
        assert!(s.problems.is_empty(), "{:?}", s.problems);
        let config = &s
            .selection
            .current()
            .unwrap()
            .project
            .as_ref()
            .unwrap()
            .config;
        assert_eq!(config.postgres().map(|(key, _)| key), Some("db"));
    });
    let _ = scratch;
}

#[gpui_kit::test]
fn a_migration_that_cannot_be_done_says_why_and_offers_no_write(cx: &mut TestAppContext) {
    let scratch = scratch_project();
    let entry = scratch.entry.clone();
    let both = format!(
        "[project]\nname = \"{}\"\nid = \"{}\"\n\n{PRE_RELEASE}\n[services.main]\nengine = \"postgres\"\n",
        entry.name, entry.id
    );
    std::fs::write(entry.root.join("devflow.toml"), &both).unwrap();

    let h = open_registry(cx, &entry);
    wait_for_problem(cx, &h, ProblemKind::PreReleaseConfig);
    // Core does not offer a migration it would refuse; the file is the way out.
    frame(cx, &h, |_| {});
    assert!(!exists(cx, &h, "remedy-migrate_config"));
    assert!(visible(cx, &h, "remedy-open_file"));
    assert_eq!(read(&entry.root.join("devflow.toml")), both);
    let _ = scratch;
}

#[gpui_kit::test]
fn a_project_without_a_devflow_toml_can_be_set_up_or_removed(cx: &mut TestAppContext) {
    let scratch = scratch_project();
    let entry = scratch.entry.clone();
    std::fs::remove_file(entry.root.join("devflow.toml")).unwrap();

    let h = open_registry(cx, &entry);
    wait(cx, "the project reads as not initialized", |cx| {
        matches!(phase(cx, &h), Some(LoadPhase::NotInitialized))
    });
    frame(cx, &h, |_| {});
    assert!(visible(cx, &h, "setup-wizard"));
    assert!(visible(cx, &h, "forget-project"));
    click(cx, &h, "forget-project");
    click(cx, &h, "confirm");
    wait(cx, "the project left the list", |cx| {
        !listed(cx, &h).contains(&entry.id)
    });
    let _ = scratch;
}

#[gpui_kit::test]
fn every_kind_of_problem_draws_one_button_per_remedy(cx: &mut TestAppContext) {
    let root = PathBuf::from("/tmp/devflow-problem");
    let all = |kind| {
        let remedies = vec![
            Remedy::Retry,
            Remedy::ForgetProject { root: root.clone() },
            Remedy::LocateProject {
                id: Uuid::nil(),
                root: root.clone(),
            },
            Remedy::MigrateConfig { root: root.clone() },
            Remedy::Init { root: root.clone() },
            Remedy::OpenFile {
                path: root.join("devflow.toml"),
            },
            Remedy::Reveal { path: root.clone() },
            Remedy::OpenGuide {
                url: "https://example.com/guide".into(),
            },
        ];
        ProjectProblem {
            kind,
            message: format!("{} (message)", kind.title()),
            root: root.clone(),
            remedies,
        }
    };
    for kind in [
        ProblemKind::FolderMissing,
        ProblemKind::NotGitCheckout,
        ProblemKind::NotInitialized,
        ProblemKind::PreReleaseConfig,
        ProblemKind::LegacyConfig,
        ProblemKind::InvalidConfig,
        ProblemKind::WrongProject,
        ProblemKind::Unreadable,
    ] {
        let problem = all(kind);
        let for_install = problem.clone();
        let h = open(cx, move |state| {
            let entry = Entry {
                id: Uuid::nil(),
                name: "shop".into(),
                root: PathBuf::from("/tmp/devflow-problem"),
            };
            state.entries = vec![entry.clone()];
            state.selection.select(entry);
            state.selection.current_mut().unwrap().phase = LoadPhase::Problem(for_install);
        });
        frame(cx, &h, |_| {});
        assert!(exists(cx, &h, "project-problem"), "{kind:?}");
        for remedy in &problem.remedies {
            assert!(
                visible(cx, &h, id(format!("remedy-{}", remedy.kind()))),
                "{kind:?}: {}",
                remedy.label()
            );
        }
    }
}

/// The text a person reads on the page and in the dialogs never carries a
/// command line: the labels come from core and none of them has one.
#[test]
fn no_remedy_label_is_a_command() {
    let root = PathBuf::from("/tmp/x");
    for remedy in [
        Remedy::Retry,
        Remedy::ForgetProject { root: root.clone() },
        Remedy::LocateProject {
            id: Uuid::nil(),
            root: root.clone(),
        },
        Remedy::MigrateConfig { root: root.clone() },
        Remedy::Init { root: root.clone() },
        Remedy::OpenFile {
            path: root.join("devflow.toml"),
        },
        Remedy::Reveal { path: root },
        Remedy::OpenGuide { url: "u".into() },
    ] {
        let label = remedy.label();
        assert!(
            !label.contains("devflow ") && !label.contains('`') && !label.contains("--"),
            "{label}"
        );
    }
}

#[allow(dead_code)]
fn _types(_: &Scratch, _: &AppState) {}
