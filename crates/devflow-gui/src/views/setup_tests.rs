//! Headless tests of the setup views: the Setup tab (hooks and environment,
//! written to the real file through core), the workspace detail pane, and the
//! setup wizard (scan, propose, review, apply, approve) against scratch
//! repositories. Core does the work; these check that the screens drive it
//! and show what it did.
use super::{
    hooks_editor::{HooksEditorView, hook_suggestions},
    onboarding::{self, OnboardingView, Step},
    ui_tests::{
        Harness, Scratch, frame, init_scratch_repo, install_loaded, open, open_scratch, press_key,
        scratch_project, update,
    },
};
use crate::model::{LoadPhase, Tab};
use devflow_core::{
    config::PostgresMode,
    propose,
    receipt::{Phase, PhaseState},
    registry::Entry,
    trust::{self, TrustState},
};
use gpui_kit::{
    AppContext as _, Entity, ScrollDelta, SharedString, TestAppContext,
    component::{ActiveTheme as _, WindowExt as _},
    point, px,
    test::TestWindowExt as _,
};
use std::{path::Path, path::PathBuf, time::Duration};
use uuid::Uuid;

/// Let the executors run until `done` holds.
///
/// Background work (scans, previews, applies) runs on real threads, so the
/// budget is real time: a loaded machine (parallel builds) can take far
/// longer than the few milliseconds these jobs need when idle.
pub(super) fn wait(
    cx: &mut TestAppContext,
    what: &str,
    mut done: impl FnMut(&mut TestAppContext) -> bool,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        cx.executor().advance_clock(Duration::from_millis(150));
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        if std::time::Instant::now() > deadline {
            panic!("never happened: {what}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub(super) fn id(text: String) -> SharedString {
    SharedString::from(text)
}

fn setup_view(cx: &mut TestAppContext, h: &Harness) -> Entity<HooksEditorView> {
    cx.update(|cx| h.shell.read(cx).project.read(cx).setup.clone())
}

pub(super) fn click(
    cx: &mut TestAppContext,
    h: &Harness,
    target: impl Into<gpui_kit::ElementId> + Clone,
) {
    cx.update_window(h.window.into(), |_, window, cx| {
        // A toast over the target would take the click.
        window.clear_notifications(cx);
        window.click(target.clone(), cx)
    })
    .unwrap();
    cx.run_until_parked();
}

pub(super) fn visible(
    cx: &mut TestAppContext,
    h: &Harness,
    target: impl Into<gpui_kit::ElementId> + Clone,
) -> bool {
    let mut found = false;
    frame(cx, h, |window| {
        found = window.try_find(target.clone()).is_some_and(|e| e.visible());
    });
    found
}

/// Click the radio at `index` of the group `scope`.
pub(super) fn click_in(cx: &mut TestAppContext, h: &Harness, scope: &'static str, index: usize) {
    cx.update_window(h.window.into(), |_, window, cx| {
        window.within(scope).click(index, cx)
    })
    .unwrap();
    cx.run_until_parked();
}

/// `target` is wholly inside `scroller`, where a click on its center lands.
pub(super) fn inside(
    cx: &mut TestAppContext,
    h: &Harness,
    scroller: &'static str,
    target: impl Into<gpui_kit::ElementId> + Clone,
) -> bool {
    let mut inside = false;
    frame(cx, h, |window| {
        if let (Some(t), Some(s)) = (window.try_find(target.clone()), window.try_find(scroller)) {
            inside = t.visible()
                && t.bounds().top() >= s.bounds().top()
                && t.bounds().bottom() <= s.bounds().bottom();
        }
    });
    inside
}

/// Scroll `scroller` until `target` is on screen.
pub(super) fn scroll_to(
    cx: &mut TestAppContext,
    h: &Harness,
    scroller: &'static str,
    target: impl Into<gpui_kit::ElementId> + Clone,
) {
    if inside(cx, h, scroller, target.clone()) {
        return;
    }
    // From the top, a screen at a time.
    scroll_up_in(cx, h, scroller);
    // Small steps: the pane can be shorter than a big step.
    for _ in 0..80 {
        if inside(cx, h, scroller, target.clone()) {
            return;
        }
        cx.update_window(h.window.into(), |_, window, cx| {
            window.scroll(scroller, ScrollDelta::Pixels(point(px(0.), px(-60.))), cx)
        })
        .unwrap();
        cx.run_until_parked();
    }
    let target: gpui_kit::ElementId = target.into();
    panic!("could not scroll {target:?} into view in {scroller}");
}

/// Scroll to `target` in `scroller`, then click it.
pub(super) fn click_scrolled(
    cx: &mut TestAppContext,
    h: &Harness,
    scroller: &'static str,
    target: impl Into<gpui_kit::ElementId> + Clone,
) {
    scroll_to(cx, h, scroller, target.clone());
    click(cx, h, target);
}

pub(super) fn scroll_up_in(cx: &mut TestAppContext, h: &Harness, scroller: &'static str) {
    for _ in 0..30 {
        cx.update_window(h.window.into(), |_, window, cx| {
            window.scroll(scroller, ScrollDelta::Pixels(point(px(0.), px(300.))), cx)
        })
        .unwrap();
    }
    cx.run_until_parked();
}

fn scroll_up(cx: &mut TestAppContext, h: &Harness) {
    scroll_up_in(cx, h, "hooks-editor");
}

pub(super) fn exists(
    cx: &mut TestAppContext,
    h: &Harness,
    target: impl Into<gpui_kit::ElementId> + Clone,
) -> bool {
    let mut found = false;
    frame(cx, h, |window| {
        found = window.try_find(target.clone()).is_some()
    });
    found
}

/// A scratch project with the Setup tab open and its data read.
fn setup_tab(cx: &mut TestAppContext) -> (Scratch, Harness, Entity<HooksEditorView>) {
    let scratch = scratch_project();
    let h = open_scratch(cx, &scratch);
    update(cx, &h, |s, cx| s.set_tab(Tab::Setup, cx));
    let setup = setup_view(cx, &h);
    wait(cx, "the Setup tab reads the files", |cx| {
        cx.read_entity(&setup, |v, _| v.loaded().is_some())
    });
    (scratch, h, setup)
}

fn config(scratch: &Scratch) -> String {
    std::fs::read_to_string(scratch.entry.root.join("devflow.toml")).unwrap()
}

fn fill_hook(
    cx: &mut TestAppContext,
    h: &Harness,
    setup: &Entity<HooksEditorView>,
    fields: [&str; 3],
) {
    cx.update_window(h.window.into(), |_, window, cx| {
        setup.update(cx, |v, cx| {
            v.fill_hook(fields[0], fields[1], fields[2], window, cx)
        })
    })
    .unwrap();
}

fn setup_runs(cx: &mut TestAppContext, setup: &Entity<HooksEditorView>) -> Vec<String> {
    cx.read_entity(setup, |v, _| {
        v.loaded()
            .map(|d| d.hooks.setup.hooks.iter().map(|h| h.run.clone()).collect())
            .unwrap_or_default()
    })
}

// ---- the Setup tab ---------------------------------------------------------------------

#[gpui_kit::test]
fn the_setup_tab_lists_the_phases_and_the_environment_with_their_actions(cx: &mut TestAppContext) {
    let (_scratch, h, _setup) = setup_tab(cx);
    frame(cx, &h, |window| {
        for id in ["hooks-setup", "add-hook-setup", "setup-reload"] {
            assert!(window.find(id).visible(), "{id} is laid out");
        }
    });
    // The rest of the page is below the fold, in the same scrolling column.
    for id in [
        "hooks-switch",
        "hooks-teardown",
        "add-hook-switch",
        "add-hook-teardown",
        "env-presets",
        "env-recipes",
        "add-var",
        "preset-prisma",
    ] {
        assert!(exists(cx, &h, id), "{id} is in the page");
    }
    scroll_to(cx, &h, "hooks-editor", "add-var");
    // The empty phases say so instead of leaving a blank card.
    assert!(!visible(cx, &h, "hook-row-setup-0"));
}

#[gpui_kit::test]
fn adding_a_hook_writes_the_file_in_place_and_keeps_its_comments(cx: &mut TestAppContext) {
    let (scratch, h, setup) = setup_tab(cx);
    let before = config(&scratch);
    assert!(before.contains("# Shell commands run in a new worktree"));

    click(cx, &h, "add-hook-setup");
    assert!(visible(cx, &h, "hook-save"), "the form opened");
    fill_hook(cx, &h, &setup, ["install", "bun install", ""]);
    click(cx, &h, "hook-save");
    wait(cx, "the hook reaches the file", |_| {
        config(&scratch).contains("bun install")
    });

    let after = config(&scratch);
    assert!(
        after.contains("# Shell commands run in a new worktree"),
        "{after}"
    );
    assert!(
        after.contains("{ name = \"install\", run = \"bun install\" }"),
        "{after}"
    );
    wait(cx, "the row appears", |cx| {
        setup_runs(cx, &setup) == ["bun install"]
    });
    assert!(visible(cx, &h, "hook-row-setup-0"));
    assert!(!visible(cx, &h, "hook-save"), "the form closed");

    // The new content needs approval, and the tab says so.
    wait(cx, "the trust banner", |cx| {
        cx.read_entity(&setup, |v, _| {
            v.loaded()
                .is_some_and(|d| matches!(d.trust, TrustState::Untrusted { .. }))
        })
    });
    assert!(visible(cx, &h, "review-approve"));
}

#[gpui_kit::test]
fn hooks_are_reordered_edited_and_removed_from_their_rows(cx: &mut TestAppContext) {
    let (scratch, h, setup) = setup_tab(cx);
    for (name, command) in [("install", "bun install"), ("gen", "bun run gen")] {
        click(cx, &h, "add-hook-setup");
        fill_hook(cx, &h, &setup, [name, command, ""]);
        click(cx, &h, "hook-save");
        wait(cx, "added", |_| config(&scratch).contains(command));
        wait(cx, "reloaded", |cx| {
            setup_runs(cx, &setup).iter().any(|r| r == command)
        });
    }
    assert_eq!(setup_runs(cx, &setup), ["bun install", "bun run gen"]);

    // Down on the first row.
    click(cx, &h, "hook-down-setup-0");
    wait(cx, "moved", |cx| {
        setup_runs(cx, &setup) == ["bun run gen", "bun install"]
    });
    let text = config(&scratch);
    assert!(
        text.find("bun run gen").unwrap() < text.find("bun install").unwrap(),
        "{text}"
    );
    // The first row cannot go up.
    assert!(visible(cx, &h, "hook-up-setup-0"));

    // Edit: the form opens on the entry, the save replaces it in place.
    click(cx, &h, "hook-edit-setup-0");
    fill_hook(cx, &h, &setup, ["codegen", "bun run generate", "2m"]);
    click(cx, &h, "hook-save");
    wait(cx, "edited", |cx| {
        setup_runs(cx, &setup) == ["bun run generate", "bun install"]
    });
    assert!(
        config(&scratch)
            .contains("{ name = \"codegen\", run = \"bun run generate\", timeout = \"2m\" }"),
        "{}",
        config(&scratch)
    );

    // Remove.
    click(cx, &h, "hook-delete-setup-1");
    wait(cx, "removed", |cx| {
        setup_runs(cx, &setup) == ["bun run generate"]
    });
    assert!(!config(&scratch).contains("bun install"));
}

#[gpui_kit::test]
fn a_bad_value_is_refused_inline_and_nothing_is_written(cx: &mut TestAppContext) {
    let (scratch, h, setup) = setup_tab(cx);
    let before = config(&scratch);
    click(cx, &h, "add-hook-setup");
    fill_hook(cx, &h, &setup, ["", "echo secret-sauce", "forever"]);
    click(cx, &h, "hook-save");
    wait(cx, "the refusal", |cx| {
        cx.read_entity(&setup, |v, _| v.inline_error().is_some())
    });
    let message = cx.read_entity(&setup, |v, _| v.inline_error().map(str::to_string).unwrap());
    assert!(message.contains(".timeout"), "{message}");
    assert!(
        !message.contains("forever") && !message.contains("secret-sauce"),
        "{message}"
    );
    assert!(
        visible(cx, &h, "form-error"),
        "the error shows under the form"
    );
    assert!(visible(cx, &h, "hook-save"), "the form stays open");
    assert_eq!(config(&scratch), before);

    // An empty command is refused before core is asked.
    fill_hook(cx, &h, &setup, ["", "  ", ""]);
    click(cx, &h, "hook-save");
    let message = cx.read_entity(&setup, |v, _| v.inline_error().map(str::to_string).unwrap());
    assert!(message.contains("Enter the command"), "{message}");

    // Cancel closes the form and forgets the error.
    click(cx, &h, "hook-cancel");
    assert!(!visible(cx, &h, "hook-save"));
    assert!(cx.read_entity(&setup, |v, _| v.inline_error().is_none()));
}

#[gpui_kit::test]
fn a_file_that_changed_on_disk_is_never_overwritten_and_reload_recovers(cx: &mut TestAppContext) {
    let (scratch, h, setup) = setup_tab(cx);
    // Somebody edits the file after the tab read it.
    let theirs = format!("{}\n# edited elsewhere\n", config(&scratch));
    std::fs::write(scratch.entry.root.join("devflow.toml"), &theirs).unwrap();

    click(cx, &h, "add-hook-setup");
    fill_hook(cx, &h, &setup, ["", "echo mine", ""]);
    click(cx, &h, "hook-save");
    wait(cx, "the conflict", |cx| {
        cx.read_entity(&setup, |v, _| v.conflict().is_some())
    });
    assert_eq!(
        cx.read_entity(&setup, |v, _| v.conflict().map(str::to_string)),
        Some("devflow.toml changed on disk — reload".to_string())
    );
    assert_eq!(config(&scratch), theirs, "their edit is still there");
    assert!(visible(cx, &h, "setup-conflict"));

    // Reload reads their file; saving again goes through.
    click(cx, &h, "setup-conflict-reload");
    wait(cx, "reloaded", |cx| {
        cx.read_entity(&setup, |v, _| v.conflict().is_none())
    });
    assert!(!visible(cx, &h, "setup-conflict"));
    click(cx, &h, "hook-save");
    wait(cx, "saved", |_| config(&scratch).contains("echo mine"));
    assert!(config(&scratch).contains("# edited elsewhere"));
}

#[gpui_kit::test]
fn variables_and_presets_are_edited_and_a_local_save_goes_to_the_local_file(
    cx: &mut TestAppContext,
) {
    let (scratch, h, setup) = setup_tab(cx);
    // A variable.
    click_scrolled(cx, &h, "hooks-editor", "add-var");
    cx.update_window(h.window.into(), |_, window, cx| {
        setup.update(cx, |v, cx| {
            v.fill_var("APP_DB", "${DATABASE_URL}", window, cx)
        })
    })
    .unwrap();
    click_scrolled(cx, &h, "hooks-editor", "var-save");
    wait(cx, "the recipe", |_| {
        config(&scratch).contains("APP_DB = \"${DATABASE_URL}\"")
    });
    wait(cx, "its row", |cx| exists(cx, &h, "var-row-APP_DB"));
    scroll_to(cx, &h, "hooks-editor", "var-resolved-APP_DB");

    // A preset, by its checkbox.
    click_scrolled(cx, &h, "hooks-editor", "preset-spring");
    wait(cx, "the preset", |_| {
        config(&scratch).contains("presets = [\"spring\"]")
    });

    // A credential is refused in the committed file.
    click_scrolled(cx, &h, "hooks-editor", "add-var");
    cx.update_window(h.window.into(), |_, window, cx| {
        setup.update(cx, |v, cx| {
            v.fill_var("LEGACY", "postgres://u:hunter2hunter2@h/db", window, cx) // kingfisher:ignore
        })
    })
    .unwrap();
    click_scrolled(cx, &h, "hooks-editor", "var-save");
    wait(cx, "the refusal", |cx| {
        cx.read_entity(&setup, |v, _| v.inline_error().is_some())
    });
    let message = cx.read_entity(&setup, |v, _| v.inline_error().map(str::to_string).unwrap());
    assert!(
        message.contains("--local") || message.contains("devflow.local.toml"),
        "{message}"
    );
    assert!(!message.contains("hunter2"), "{message}");
    assert!(!config(&scratch).contains("hunter2"));
    click_scrolled(cx, &h, "hooks-editor", "var-cancel");

    // Save to this machine instead: the local file gets the edit.
    scroll_up(cx, &h);
    click_in(cx, &h, "edit-target", 1);
    assert!(exists(cx, &h, "setup-local-note"));
    click_scrolled(cx, &h, "hooks-editor", "add-hook-teardown");
    fill_hook(cx, &h, &setup, ["", "docker compose down", ""]);
    click_scrolled(cx, &h, "hooks-editor", "hook-save");
    let local = scratch.entry.root.join("devflow.local.toml");
    wait(cx, "the local file", |_| local.exists());
    let local_text = std::fs::read_to_string(&local).unwrap();
    assert!(local_text.contains("docker compose down"), "{local_text}");
    assert!(!config(&scratch).contains("docker compose down"));
}

fn presets_shown(cx: &mut TestAppContext, setup: &Entity<HooksEditorView>) -> Vec<String> {
    cx.read_entity(setup, |v, _| {
        v.loaded()
            .map(|d| d.env.presets.iter().map(|p| p.preset.to_string()).collect())
            .unwrap_or_default()
    })
}

#[gpui_kit::test]
fn the_global_refresh_reloads_a_setup_tab_whose_file_changed_outside(cx: &mut TestAppContext) {
    let (scratch, h, setup) = setup_tab(cx);
    assert!(presets_shown(cx, &setup).is_empty());

    // `devflow env preset add prisma` from a terminal.
    let path = scratch.entry.root.join("devflow.toml");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("\n[env]\npresets = [\"prisma\"]\n");
    std::fs::write(&path, text).unwrap();

    // Nothing is polled: the tab still shows what it read.
    cx.run_until_parked();
    assert!(presets_shown(cx, &setup).is_empty());

    // The global refresh (⌘R) looks at the disk and reads the file again.
    update(cx, &h, |s, cx| s.refresh_all(cx));
    wait(cx, "the preset written outside", |cx| {
        presets_shown(cx, &setup) == ["prisma"]
    });
    assert!(cx.read_entity(&setup, |v, _| v.conflict().is_none()));

    // A refresh with nothing changed on disk reads nothing and says nothing.
    update(cx, &h, |s, cx| s.refresh_all(cx));
    cx.run_until_parked();
    assert_eq!(presets_shown(cx, &setup), ["prisma"]);
    assert!(cx.read_entity(&setup, |v, _| v.conflict().is_none()));
}

#[gpui_kit::test]
fn the_global_refresh_keeps_an_edit_in_progress_and_says_the_file_changed(cx: &mut TestAppContext) {
    let (scratch, h, setup) = setup_tab(cx);
    // An edit in progress: the hook form is open with text in it.
    click_scrolled(cx, &h, "hooks-editor", "add-hook-setup");
    fill_hook(cx, &h, &setup, ["", "echo mine", ""]);

    let path = scratch.entry.root.join("devflow.toml");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("\n[env]\npresets = [\"prisma\"]\n");
    std::fs::write(&path, text).unwrap();

    update(cx, &h, |s, cx| s.refresh_all(cx));
    wait(cx, "the notice", |cx| {
        cx.read_entity(&setup, |v, _| v.conflict().is_some())
    });
    // Neither the data nor the open form was replaced.
    assert!(presets_shown(cx, &setup).is_empty());
    assert!(exists(cx, &h, "setup-conflict"));
    let notice = cx.read_entity(&setup, |v, _| v.conflict().map(str::to_string).unwrap());
    assert!(notice.contains("devflow.toml"), "{notice}");
    assert!(exists(cx, &h, "hook-save"), "the form is still open");

    // Reload shows the new content and clears the notice.
    click(cx, &h, "setup-conflict-reload");
    wait(cx, "the reload", |cx| {
        presets_shown(cx, &setup) == ["prisma"]
    });
    assert!(cx.read_entity(&setup, |v, _| v.conflict().is_none()));
}

/// An edit made from outside the app (a terminal, an editor): applied to the
/// shared file as it is now.
fn edited_elsewhere(scratch: &Scratch, edit: devflow_core::config_edit::Edit) {
    use devflow_core::config_edit::{self, ConfigFiles, Target};
    let root = &scratch.entry.root;
    let hash = ConfigFiles::read(root).unwrap().project.hash;
    config_edit::apply(root, Target::Project, hash.as_deref(), &edit).unwrap();
}

fn add_elsewhere(scratch: &Scratch, run: &str, at: devflow_core::config_edit::Placement) {
    edited_elsewhere(
        scratch,
        devflow_core::config_edit::Edit::AddHook {
            phase: devflow_core::hooks::HookPhase::Setup,
            entry: devflow_core::config::HookEntry {
                name: None,
                run: run.to_string(),
                timeout: None,
            },
            at,
        },
    );
}

#[gpui_kit::test]
fn a_reload_that_finishes_under_an_open_form_cannot_make_its_save_overwrite_a_new_hook(
    cx: &mut TestAppContext,
) {
    use devflow_core::{config_edit::Placement, hooks::HookPhase};
    let (scratch, h, setup) = setup_tab(cx);
    add_elsewhere(&scratch, "echo old0", Placement::End);
    click(cx, &h, "setup-reload");
    wait(cx, "hook 0 is read", |cx| {
        setup_runs(cx, &setup) == ["echo old0"]
    });

    // Somebody inserts a hook before hook 0. An automatic read (the window was
    // activated) starts, and before it finishes the person opens the form on
    // the hook they see, hook 0 of the data on screen. Editing is allowed
    // while a read is running.
    add_elsewhere(&scratch, "echo inserted", Placement::Start);
    let theirs = config(&scratch);
    cx.update_window(h.window.into(), |_, window, cx| {
        setup.update(cx, |v, cx| {
            v.start_automatic_read(window, cx);
            assert!(v.is_loading());
            v.open_edit_form(HookPhase::Setup, 0, window, cx);
            v.fill_hook("", "echo mine", "", window, cx);
        })
    })
    .unwrap();
    wait(cx, "the read finishes", |cx| {
        !cx.read_entity(&setup, |v, _| v.is_loading())
    });

    // The read found the file changed under the open form: it says so and
    // leaves the data (and the hash) the form was made on.
    assert!(cx.read_entity(&setup, |v, _| v.conflict().is_some()));
    assert!(exists(cx, &h, "setup-conflict"));
    assert!(exists(cx, &h, "hook-save"), "the form is still open");
    assert_eq!(setup_runs(cx, &setup), ["echo old0"]);

    // Saving is a conflict, not an overwrite of the inserted hook.
    click(cx, &h, "hook-save");
    wait(cx, "the save is refused", |cx| {
        cx.read_entity(&setup, |v, _| {
            v.conflict() == Some("devflow.toml changed on disk — reload")
        })
    });
    assert_eq!(config(&scratch), theirs, "their hook is still there");

    // Reload follows the hook the form was opened on (same command, new
    // position); saving then edits that hook and keeps the inserted one.
    click(cx, &h, "setup-conflict-reload");
    wait(cx, "reloaded", |cx| {
        setup_runs(cx, &setup) == ["echo inserted", "echo old0"]
            && cx.read_entity(&setup, |v, _| v.conflict().is_none())
    });
    assert!(exists(cx, &h, "hook-save"), "the form is still open");
    click(cx, &h, "hook-save");
    wait(cx, "saved", |_| config(&scratch).contains("echo mine"));
    let saved = config(&scratch);
    assert!(saved.contains("echo inserted"), "{saved}");
    assert!(!saved.contains("echo old0"), "{saved}");
}

#[gpui_kit::test]
fn a_form_whose_hook_disappeared_is_closed_by_reload_not_aimed_at_another_hook(
    cx: &mut TestAppContext,
) {
    use devflow_core::{
        config_edit::{Edit, HookRef, Placement},
        hooks::HookPhase,
    };
    let (scratch, h, setup) = setup_tab(cx);
    add_elsewhere(&scratch, "echo old0", Placement::End);
    click(cx, &h, "setup-reload");
    wait(cx, "hook 0 is read", |cx| {
        setup_runs(cx, &setup) == ["echo old0"]
    });
    cx.update_window(h.window.into(), |_, window, cx| {
        setup.update(cx, |v, cx| {
            v.open_edit_form(HookPhase::Setup, 0, window, cx);
            v.fill_hook("", "echo mine", "", window, cx);
        })
    })
    .unwrap();

    // Somebody replaces it by another command.
    edited_elsewhere(
        &scratch,
        Edit::RemoveHook {
            phase: HookPhase::Setup,
            hook: HookRef::Index(0),
        },
    );
    add_elsewhere(&scratch, "echo other", Placement::End);
    let theirs = config(&scratch);
    click(cx, &h, "hook-save");
    wait(cx, "the conflict", |cx| {
        cx.read_entity(&setup, |v, _| v.conflict().is_some())
    });
    assert_eq!(config(&scratch), theirs);
    click(cx, &h, "setup-conflict-reload");
    wait(cx, "reloaded", |cx| {
        setup_runs(cx, &setup) == ["echo other"]
    });
    assert!(!exists(cx, &h, "hook-save"), "the form was closed");
    assert!(
        cx.read_entity(&setup, |v, _| v.inline_error().map(str::to_string))
            .is_some_and(|e| e.contains("changed on disk"))
    );
    assert_eq!(config(&scratch), theirs, "nothing was written");
}

#[gpui_kit::test]
fn a_recipe_and_a_hook_that_quote_a_credential_are_masked_until_reveal(cx: &mut TestAppContext) {
    let (scratch, h, setup) = setup_tab(cx);
    // Machine-local content may hold a credential; the tab must not show it.
    std::fs::write(
        scratch.entry.root.join("devflow.local.toml"),
        "[env]\nLEGACY = \"postgres://u:sentinel@h/db\"\nDB_PASSWORD = \"literal-value\"\n\
         DB_PASSWORD_REF = \"${DEVFLOW_WORKSPACE}\"\n\
         [hooks]\nsetup = [\"psql postgres://u:sentinel@h/db -c 'select 1'\"]\n", // kingfisher:ignore
    )
    .unwrap();
    click(cx, &h, "setup-reload");
    wait(cx, "the local file is read", |cx| {
        cx.read_entity(&setup, |v, _| {
            v.loaded()
                .is_some_and(|d| d.env.recipes.iter().any(|r| r.key == "LEGACY"))
        })
    });
    scroll_up(cx, &h);
    click_in(cx, &h, "edit-target", 1);

    let templates = |cx: &mut TestAppContext| {
        cx.read_entity(&setup, |v, _| v.shown_templates())
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let commands = |cx: &mut TestAppContext| {
        cx.read_entity(&setup, |v, _| {
            v.shown_commands(devflow_core::hooks::HookPhase::Setup)
        })
    };
    let shown = templates(cx);
    assert_eq!(shown["LEGACY"], "postgres://u:***@h/db");
    assert_eq!(shown["DB_PASSWORD"], "***");
    assert_eq!(
        shown["DB_PASSWORD_REF"], "${DEVFLOW_WORKSPACE}",
        "a reference is no secret"
    );
    assert_eq!(commands(cx), ["psql postgres://u:***@h/db -c 'select 1'"]);
    assert!(!format!("{shown:?}{:?}", commands(cx)).contains("sentinel"));

    // Reveal, in the hooks card and in the variables card, shows the text.
    assert!(
        visible(cx, &h, "reveal-hooks-setup"),
        "a masked hook offers it"
    );
    click(cx, &h, "reveal-hooks-setup");
    assert_eq!(
        commands(cx),
        ["psql postgres://u:sentinel@h/db -c 'select 1'"] // kingfisher:ignore
    );
    assert_eq!(templates(cx)["LEGACY"], "postgres://u:sentinel@h/db"); // kingfisher:ignore
    assert_eq!(templates(cx)["DB_PASSWORD"], "literal-value");
    // The same toggle, in the variables card, hides them again.
    click_scrolled(cx, &h, "hooks-editor", "reveal-values");
    assert_eq!(templates(cx)["DB_PASSWORD"], "***");
    assert!(commands(cx)[0].contains("***"));
}

/// Add `bun install` to the shared file, then remove it with the row's
/// button: the removal's notification offers Undo. Waits for the toast.
fn delete_a_shared_hook(
    cx: &mut TestAppContext,
    h: &Harness,
    scratch: &Scratch,
    setup: &Entity<HooksEditorView>,
) {
    click(cx, h, "add-hook-setup");
    fill_hook(cx, h, setup, ["install", "bun install", ""]);
    click(cx, h, "hook-save");
    wait(cx, "the hook is added", |cx| {
        config(scratch).contains("bun install") && setup_runs(cx, setup) == ["bun install"]
    });
    click(cx, h, "hook-delete-setup-0");
    wait(cx, "the hook is removed", |cx| {
        !config(scratch).contains("bun install") && setup_runs(cx, setup).is_empty()
    });
    // The toast slides in on the wall clock.
    std::thread::sleep(Duration::from_millis(450));
    frame(cx, h, |window| {
        assert!(
            window.find("undo-edit").visible(),
            "the removal offers Undo"
        );
    });
}

fn click_undo(cx: &mut TestAppContext, h: &Harness) {
    cx.update_window(h.window.into(), |_, window, cx| {
        window.hover("undo-edit", cx);
        window.click("undo-edit", cx);
    })
    .unwrap();
    cx.run_until_parked();
}

#[gpui_kit::test]
fn undo_puts_the_edit_back_in_the_file_it_changed_not_in_the_one_selected_now(
    cx: &mut TestAppContext,
) {
    let (scratch, h, setup) = setup_tab(cx);
    delete_a_shared_hook(cx, &h, &scratch, &setup);

    // "Save to" moves to this machine before Undo is clicked.
    scroll_up(cx, &h);
    click_in(cx, &h, "edit-target", 1);
    assert!(exists(cx, &h, "setup-local-note"));
    click_undo(cx, &h);

    wait(cx, "the shared file has its hook again", |_| {
        config(&scratch).contains("bun install")
    });
    let local = scratch.entry.root.join("devflow.local.toml");
    assert!(
        !local.exists()
            || !std::fs::read_to_string(&local)
                .unwrap()
                .contains("bun install"),
        "the hook did not land in the local file"
    );
}

#[gpui_kit::test]
fn undo_is_refused_when_the_file_changed_after_the_edit(cx: &mut TestAppContext) {
    let (scratch, h, setup) = setup_tab(cx);
    delete_a_shared_hook(cx, &h, &scratch, &setup);

    // Somebody edits the file between the edit and the click.
    let theirs = format!("{}\n# edited elsewhere\n", config(&scratch));
    std::fs::write(scratch.entry.root.join("devflow.toml"), &theirs).unwrap();
    click_undo(cx, &h);
    std::thread::sleep(Duration::from_millis(300));
    cx.run_until_parked();
    assert_eq!(config(&scratch), theirs, "their file is left alone");
    assert!(!config(&scratch).contains("bun install"));
}

#[gpui_kit::test]
fn the_trust_banner_approves_what_was_shown_and_goes_away(cx: &mut TestAppContext) {
    let (scratch, h, setup) = setup_tab(cx);
    click(cx, &h, "add-hook-setup");
    fill_hook(cx, &h, &setup, ["install", "bun install", ""]);
    click(cx, &h, "hook-save");
    wait(cx, "the banner", |cx| visible(cx, &h, "review-approve"));

    click(cx, &h, "review-approve");
    wait(cx, "the approval dialog", |cx| {
        exists(cx, &h, "approve-hooks")
    });
    assert!(
        exists(cx, &h, "approve-section-setup"),
        "the commands are listed"
    );
    click_scrolled(cx, &h, "review-scroll", "approve-hooks");
    wait(cx, "approved", |_| {
        let project = devflow_core::Project::open(&scratch.entry.root).unwrap();
        matches!(trust::state(&project).unwrap(), TrustState::Trusted)
    });
    wait(cx, "the banner leaves", |cx| {
        cx.read_entity(&setup, |v, _| {
            v.loaded()
                .is_some_and(|d| matches!(d.trust, TrustState::Trusted))
        })
    });
    assert!(!exists(cx, &h, "review-approve"));
    assert!(visible(cx, &h, "setup-approved"), "a quiet mark instead");
}

// ---- the workspace detail ---------------------------------------------------------------

#[gpui_kit::test]
fn selecting_a_workspace_shows_its_detail_below_the_table(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    frame(cx, &h, |window| {
        assert!(window.find("workspace-detail").visible(), "the pane");
        for id in [
            "detail-open-finder",
            "detail-open-terminal",
            "detail-open-editor",
            "detail-copy-path",
            "detail-timeline",
            "detail-connections",
            "phase-materialize",
        ] {
            assert!(window.find(id).visible(), "{id}");
        }
        // The pane scrolls: the rest is in it.
        for id in ["detail-disk", "detail-processes", "phase-hooks"] {
            assert!(window.try_find(id).is_some(), "{id}");
        }
        // The table keeps its row actions.
        assert!(window.find(("remove", 1usize)).visible());
    });
}

#[gpui_kit::test]
fn a_failed_phase_is_a_banner_with_a_retry_and_a_skip_points_to_the_setup_tab(
    cx: &mut TestAppContext,
) {
    let h = open(cx, install_loaded);
    update(cx, &h, |s, cx| {
        let model = s.selection.current_mut().unwrap();
        let receipt = model
            .workspaces
            .iter_mut()
            .find(|w| w.workspace.name == "feature-x")
            .and_then(|w| w.receipt.as_mut())
            .unwrap();
        receipt.set_phase(
            Phase::Services,
            PhaseState::Failed,
            Some("the server is down".into()),
        );
        receipt.skip_required(Phase::Hooks, "the hooks are not approved".into());
        model.selected_workspace = Some("feature-x".into());
        cx.notify();
    });
    frame(cx, &h, |window| {
        assert!(window.find("problem-services").visible(), "the failure");
        assert!(window.find("problem-hooks").visible(), "the skip");
        assert!(window.find("detail-retry-setup").visible(), "retry");
        assert!(
            window.find("problem-review-hooks").visible(),
            "review in Setup"
        );
    });
    click(cx, &h, "problem-review-hooks");
    assert_eq!(
        cx.read_entity(&h.state, |s, _| s.tab),
        Tab::Setup,
        "the skip leads to where approval lives"
    );
}

#[gpui_kit::test]
fn the_detail_follows_the_selection_and_hides_when_there_is_none(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    update(cx, &h, |s, cx| s.select_workspace("adopted".into(), cx));
    frame(cx, &h, |window| {
        assert!(window.find("workspace-detail").visible());
        // No receipt: nothing to time, and the pane says it was not set up.
        assert!(window.try_find("phase-materialize").is_none());
        assert!(window.find("detail-retry-setup").visible(), "Set up");
    });
    update(cx, &h, |s, cx| {
        s.selection.current_mut().unwrap().workspaces.clear();
        s.selection.current_mut().unwrap().selected_workspace = None;
        cx.notify();
    });
    frame(cx, &h, |window| {
        assert!(window.try_find("workspace-detail").is_none());
        assert!(
            window.find("empty-new-workspace").visible(),
            "one primary action"
        );
    });
}

// ---- shortcuts and menus -------------------------------------------------------------------

#[gpui_kit::test]
fn the_setup_tab_sits_between_daemons_and_config_on_the_shortcuts(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    frame(cx, &h, |_| {});
    let order: Vec<_> = Tab::ALL.iter().map(|t| t.title()).collect();
    assert_eq!(
        order,
        ["Workspaces", "Data", "Daemons", "Setup", "Config", "Logs"]
    );
    for (key, tab) in [
        ("secondary-1", Tab::Workspaces),
        ("secondary-2", Tab::Data),
        ("secondary-3", Tab::Daemons),
        ("secondary-4", Tab::Setup),
        ("secondary-5", Tab::Config),
        ("secondary-6", Tab::Logs),
    ] {
        press_key(cx, &h, key);
        assert_eq!(cx.read_entity(&h.state, |s, _| s.tab), tab, "{key}");
    }
}

// ---- the wizard ---------------------------------------------------------------------------------

const STACK: [(&str, &str); 7] = [
    (
        "package.json",
        r#"{"name":"blog","private":true,"scripts":{"dev":"next dev"},"dependencies":{"next":"15.1.0","@prisma/client":"6.1.0"},"devDependencies":{"prisma":"6.1.0"}}"#,
    ),
    ("bun.lock", "{}\n"),
    (
        "prisma/schema.prisma",
        "datasource db {\n  provider = \"postgresql\"\n  url      = env(\"DATABASE_URL\")\n}\n",
    ),
    (
        "prisma/migrations/20240101000000_init/migration.sql",
        "CREATE TABLE t (id int);\n",
    ),
    (
        "compose.yaml",
        "services:\n  db:\n    image: postgres:16\n    ports:\n      - \"5432:5432\"\n",
    ),
    (".env", "DATABASE_URL=SENTINEL_VALUE_THAT_MUST_NOT_LEAK\n"),
    (".gitignore", ".env\nnode_modules\n"),
];

/// The same stack with a Redis, a MySQL 8.4 and a MongoDB service next to the
/// PostgreSQL one: two devflow branches natively, one it does not.
const DATASTORES: [(&str, &str); 7] = [
    STACK[0],
    STACK[1],
    STACK[2],
    STACK[3],
    (
        "compose.yaml",
        "services:\n  db:\n    image: postgres:16\n  cache:\n    image: redis:7\n  sql:\n    image: mysql:8.4\n  mongo:\n    image: mongo:7\n",
    ),
    STACK[5],
    STACK[6],
];

/// A repository with a stack worth detecting and no devflow configuration.
fn unconfigured_repo(dir: &Path, files: &[(&str, &str)]) -> Entry {
    std::fs::create_dir(dir).unwrap();
    for (path, content) in files {
        let full = dir.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["commit", "-q", "--allow-empty", "-m", "init"][..],
    ] {
        let status = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success());
    }
    let root = dir.canonicalize().unwrap();
    Entry {
        id: Uuid::new_v4(),
        name: "blog".into(),
        root,
    }
}

pub(super) struct WizardRig {
    _scratch: Scratch,
    pub(super) h: Harness,
    pub(super) wizard: Entity<OnboardingView>,
    root: PathBuf,
}

pub(super) fn wizard(cx: &mut TestAppContext) -> WizardRig {
    wizard_over(cx, &STACK)
}

fn wizard_over(cx: &mut TestAppContext, files: &[(&str, &str)]) -> WizardRig {
    let scratch = scratch_project();
    let entry = unconfigured_repo(&scratch._outer.path().join("blog"), files);
    let root = entry.root.clone();
    let for_state = entry.clone();
    let h = open(cx, move |state| {
        state.inflight = 0;
        state.entries = vec![for_state.clone()];
        state.selection.select(for_state);
    });
    update(cx, &h, |s, cx| s.refresh(cx));
    wait(cx, "the project reads as not initialized", |cx| {
        cx.read_entity(&h.state, |s, _| {
            s.selection
                .current()
                .is_some_and(|m| m.phase == LoadPhase::NotInitialized)
        })
    });
    let state = h.state.clone();
    let wizard = cx
        .update_window(h.window.into(), |_, window, cx| {
            onboarding::open_view(&state, root.clone(), window, cx)
        })
        .unwrap();
    wait(cx, "the scan", |cx| {
        cx.read_entity(&wizard, |w, _| w.scanned().is_some())
    });
    WizardRig {
        _scratch: scratch,
        h,
        wizard,
        root,
    }
}

fn step(cx: &mut TestAppContext, rig: &WizardRig) -> Step {
    cx.read_entity(&rig.wizard, |w, _| w.step())
}

#[gpui_kit::test]
fn the_empty_project_offers_the_wizard_as_its_one_primary_action(cx: &mut TestAppContext) {
    let scratch = scratch_project();
    let entry = unconfigured_repo(&scratch._outer.path().join("blog"), &STACK);
    let h = open(cx, move |state| {
        state.inflight = 0;
        state.entries = vec![entry.clone()];
        state.selection.select(entry);
    });
    update(cx, &h, |s, cx| s.refresh(cx));
    wait(cx, "not initialized", |cx| {
        cx.read_entity(&h.state, |s, _| {
            s.selection
                .current()
                .is_some_and(|m| m.phase == LoadPhase::NotInitialized)
        })
    });
    assert!(visible(cx, &h, "setup-wizard"), "Set up devflow…");
    assert!(
        visible(cx, &h, "initialize"),
        "the minimal template stays a quiet option"
    );
    click(cx, &h, "setup-wizard");
    wait(cx, "the wizard opens", |cx| visible(cx, &h, "wizard-next"));
    assert!(exists(cx, &h, "wizard-detected"));
}

#[gpui_kit::test]
fn the_palette_offers_the_wizard_for_the_selected_project(cx: &mut TestAppContext) {
    let scratch = scratch_project();
    let h = open_scratch(cx, &scratch);
    press_key(cx, &h, "secondary-k");
    frame(cx, &h, |window| assert!(window.find("command").visible()));
    cx.update_window(h.window.into(), |_, window, cx| window.input("Set up", cx))
        .unwrap();
    cx.run_until_parked();
    press_key(cx, &h, "enter");
    wait(cx, "the wizard", |cx| visible(cx, &h, "wizard-next"));
}

#[gpui_kit::test]
fn the_wizard_walks_detect_propose_review_apply_approve_and_done(cx: &mut TestAppContext) {
    let rig = wizard(cx);
    let WizardRig {
        h, wizard, root, ..
    } = &rig;

    // a) Detected: facts, no values.
    assert_eq!(step(cx, &rig), Step::Detected);
    assert!(visible(cx, h, "wizard-detected"));
    cx.update_window(h.window.into(), |_, window, cx| window.press("enter", cx))
        .unwrap();
    cx.run_until_parked();

    // b) Proposal: items with checkboxes at their defaults.
    assert_eq!(
        step(cx, &rig),
        Step::Proposal,
        "Enter goes to the next step"
    );
    let (plan_ids, defaults) = cx.read_entity(wizard, |w, _| {
        let scanned = w.scanned().unwrap();
        (
            scanned
                .plan
                .items
                .iter()
                .map(|i| i.id.clone())
                .collect::<Vec<_>>(),
            w.selected().clone(),
        )
    });
    assert!(plan_ids.iter().any(|i| i == "project:init"), "{plan_ids:?}");
    assert!(
        plan_ids.iter().any(|i| i.starts_with("hook:setup:")),
        "{plan_ids:?}"
    );
    assert!(
        defaults.contains("project:init") && defaults.contains("service:db"),
        "{defaults:?}"
    );
    assert!(exists(cx, h, "wizard-proposal"));
    assert!(exists(cx, h, "wizard-pg-choice"), "the PostgreSQL choice");
    assert!(visible(cx, h, id("item-project:init".into())));

    // Untick an item that depends on nothing optional, tick it again.
    let optional = plan_ids
        .iter()
        .find(|i| i.starts_with("hook:setup:") && defaults.contains(*i))
        .cloned()
        .expect("a default hook");
    click_scrolled(cx, h, "wizard-body", id(format!("item-{optional}")));
    assert!(!cx.read_entity(wizard, |w, _| w.selected().contains(&optional)));
    click_scrolled(cx, h, "wizard-body", id(format!("item-{optional}")));
    assert!(cx.read_entity(wizard, |w, _| w.selected().contains(&optional)));
    // Unticking the service drops what depends on it (the migration).
    click_scrolled(cx, h, "wizard-body", id("item-service:db".into()));
    let after = cx.read_entity(wizard, |w, _| w.selected().clone());
    assert!(!after.contains("service:db"));
    assert!(
        after
            .iter()
            .all(|i| !i.contains("migrate") && !i.starts_with("env:preset")),
        "{after:?}"
    );
    click_scrolled(cx, h, "wizard-body", id("item-service:db".into()));
    // The detail opens.
    click_scrolled(cx, h, "wizard-body", id("expand-project:init".into()));
    assert!(exists(cx, h, id("item-detail-project:init".into())));

    // c) Review: the exact lines per file; nothing is written yet.
    click(cx, h, "wizard-next");
    assert_eq!(step(cx, &rig), Step::Review);
    wait(cx, "the review", |cx| {
        exists(cx, h, id("review-devflow.toml".into()))
    });
    assert!(
        !root.join("devflow.toml").exists(),
        "nothing is written by looking"
    );

    // d) Apply.
    click(cx, h, "wizard-next");
    wait(cx, "the apply", |cx| {
        cx.read_entity(wizard, |w, _| w.applied().is_some())
    });
    assert!(root.join("devflow.toml").exists());
    let written = std::fs::read_to_string(root.join("devflow.toml")).unwrap();
    assert!(
        written.contains("[project]") && written.contains("engine = \"postgres\""),
        "{written}"
    );
    assert!(
        !written.contains("SENTINEL_VALUE"),
        "no .env value ever reaches a file"
    );
    assert!(exists(cx, h, "wizard-applied"));
    assert!(exists(cx, h, id("applied-devflow.toml".into())));
    // The project is registered: the app reads it as configured.
    wait(cx, "the project loads", |cx| {
        cx.read_entity(&h.state, |s, _| {
            s.selection
                .current()
                .is_some_and(|m| m.phase == LoadPhase::Ready)
        })
    });

    // e) Approve: the exact commands, then the button.
    cx.update_window(h.window.into(), |_, window, cx| window.press("enter", cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(step(cx, &rig), Step::Approve);
    let approval = cx
        .read_entity(wizard, |w, _| w.approval())
        .expect("the approval view");
    wait(cx, "the scope", |cx| {
        cx.read_entity(&approval, |a, _| a.is_ready())
    });
    assert!(exists(cx, h, "approve-section-setup"));
    click_scrolled(cx, h, "wizard-body", "approve-hooks");
    wait(cx, "approved", |cx| {
        cx.read_entity(&approval, |a, _| a.is_approved())
    });
    let project = devflow_core::Project::open(root).unwrap();
    assert!(matches!(
        trust::state(&project).unwrap(),
        TrustState::Trusted
    ));

    // f) Done: next steps, and the new-workspace dialog is one click away.
    click(cx, h, "wizard-next");
    assert_eq!(step(cx, &rig), Step::Done);
    assert!(exists(cx, h, "wizard-first-workspace"));
    click_scrolled(cx, h, "wizard-body", "wizard-first-workspace");
    wait(cx, "the new workspace dialog", |cx| {
        visible(cx, h, "create")
    });
}

#[gpui_kit::test]
fn the_wizard_shows_cache_and_mysql_services_and_applies_the_ticked_ones(cx: &mut TestAppContext) {
    let rig = wizard_over(cx, &DATASTORES);
    let WizardRig {
        h, wizard, root, ..
    } = &rig;
    cx.update_window(h.window.into(), |_, window, cx| window.press("enter", cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(step(cx, &rig), Step::Proposal);

    // The plan has the two native services next to the database; the
    // compose hook is for what devflow does not branch.
    let (ids, selected) = cx.read_entity(wizard, |w, _| {
        (
            w.scanned()
                .unwrap()
                .plan
                .items
                .iter()
                .map(|i| i.id.clone())
                .collect::<Vec<_>>(),
            w.selected().clone(),
        )
    });
    for item_id in ["service:db", "service:cache", "service:mysql"] {
        assert!(ids.iter().any(|i| i == item_id), "{item_id} in {ids:?}");
    }
    // Recommended: ticked. MySQL is a preview: offered, not ticked.
    assert!(selected.contains("service:cache"), "{selected:?}");
    assert!(!selected.contains("service:mysql"), "{selected:?}");
    // The PostgreSQL choice is still the database's.
    assert!(exists(cx, h, "wizard-pg-choice"));
    for item_id in ["service:cache", "service:mysql"] {
        assert!(
            exists(cx, h, id(format!("item-row-{item_id}"))),
            "{item_id}"
        );
    }
    assert!(visible(cx, h, id("item-service:cache".into())));

    // Opening the detail shows what can go wrong and the content.
    click_scrolled(cx, h, "wizard-body", id("expand-service:mysql".into()));
    assert!(exists(cx, h, id("item-detail-service:mysql".into())));
    let risks = cx.read_entity(wizard, |w, _| {
        w.scanned()
            .unwrap()
            .plan
            .item("service:mysql")
            .unwrap()
            .risks
            .join("\n")
    });
    assert!(
        risks.contains("Preview") && risks.contains("can stay in compose.yaml"),
        "{risks}"
    );

    // Tick MySQL, review, apply.
    click_scrolled(cx, h, "wizard-body", id("item-service:mysql".into()));
    assert!(cx.read_entity(wizard, |w, _| w.selected().contains("service:mysql")));
    click(cx, h, "wizard-next");
    wait(cx, "the review", |cx| {
        exists(cx, h, id("review-devflow.toml".into()))
    });
    click(cx, h, "wizard-next");
    wait(cx, "the apply", |cx| {
        cx.read_entity(wizard, |w, _| w.applied().is_some())
    });
    let written = std::fs::read_to_string(root.join("devflow.toml")).unwrap();
    for expected in [
        "[services.cache]",
        "engine = \"redis\"",
        "[services.mysql]",
        "engine = \"mysql\"",
        "image = \"mysql:8.4\"",
    ] {
        assert!(written.contains(expected), "{expected}\n{written}");
    }
    // The file loads, services and all.
    let (config, _) = devflow_core::Config::load(root).unwrap();
    assert_eq!(config.containers().count(), 2);
}

/// The stack with a MySQL 8.4 service and an Elasticsearch one next to
/// PostgreSQL: devflow could branch MySQL (a preview, unticked); it does not
/// branch Elasticsearch.
const MYSQL_AND_SEARCH: [(&str, &str); 7] = [
    STACK[0],
    STACK[1],
    STACK[2],
    STACK[3],
    (
        "compose.yaml",
        "services:\n  db:\n    image: postgres:16\n  sql:\n    image: mysql:8.4\n  search:\n    image: elasticsearch:8.13.0\n",
    ),
    STACK[5],
    STACK[6],
];

#[gpui_kit::test]
fn an_unticked_mysql_is_still_started_by_the_compose_hook(cx: &mut TestAppContext) {
    let rig = wizard_over(cx, &MYSQL_AND_SEARCH);
    let WizardRig {
        h, wizard, root, ..
    } = &rig;
    cx.update_window(h.window.into(), |_, window, cx| window.press("enter", cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(step(cx, &rig), Step::Proposal);
    // MySQL is offered and unticked; the compose hook is offered too, for
    // what devflow does not branch (Elasticsearch) and, if MySQL stays
    // unticked, MySQL.
    let (selected, plan_run) = cx.read_entity(wizard, |w, _| {
        let plan = &w.scanned().unwrap().plan;
        (
            w.selected().clone(),
            plan.item("hook:setup:compose-up")
                .map(|i| i.value["run"].clone()),
        )
    });
    assert!(!selected.contains("service:mysql"), "{selected:?}");
    assert_eq!(
        plan_run,
        Some(serde_json::json!("docker compose up -d --wait search"))
    );

    // Tick the compose hook only, review and apply.
    click_scrolled(
        cx,
        h,
        "wizard-body",
        id("item-hook:setup:compose-up".into()),
    );
    assert!(cx.read_entity(wizard, |w, _| {
        w.selected().contains("hook:setup:compose-up")
    }));
    click(cx, h, "wizard-next");
    wait(cx, "the review", |cx| {
        exists(cx, h, id("review-devflow.toml".into()))
    });
    click(cx, h, "wizard-next");
    wait(cx, "the apply", |cx| {
        cx.read_entity(wizard, |w, _| w.applied().is_some())
    });
    // MySQL was not accepted, so the hook starts it with the rest.
    let (config, _) = devflow_core::Config::load(root).unwrap();
    assert_eq!(config.containers().count(), 0, "MySQL stays compose's");
    let runs: Vec<_> = config.hooks.setup.iter().map(|h| h.run.clone()).collect();
    assert!(
        runs.contains(&"docker compose up -d --wait search sql".to_string()),
        "{runs:?}"
    );
}

#[test]
fn a_suggested_compose_hook_starts_the_services_devflow_does_not_branch_an_unticked_mysql_included()
{
    let dir = tempfile::tempdir().unwrap();
    let entry = unconfigured_repo(&dir.path().join("blog"), &MYSQL_AND_SEARCH);
    // The Setup tab is for a project that is set up: no `project:init` in the
    // plan for a hook to wait for.
    devflow_core::project::init(
        &entry.root,
        devflow_core::project::InitOptions {
            name: None,
            postgres: None,
        },
    )
    .unwrap();
    let plan = propose::propose(&entry.root, &propose::PlanOptions::default()).unwrap();
    // As proposed, the hook names only what devflow never branches...
    assert_eq!(
        plan.item("hook:setup:compose-up").unwrap().value["run"],
        serde_json::json!("docker compose up -d --wait search")
    );
    // ...but a suggestion adds that hook alone, with MySQL not accepted as a
    // service of its own, so it must start MySQL too.
    let suggestions = hook_suggestions(&plan);
    let compose = suggestions
        .iter()
        .find(|s| s.id == "hook:setup:compose-up")
        .expect("the compose start is suggested");
    assert_eq!(compose.run, "docker compose up -d --wait search sql");
}

#[gpui_kit::test]
fn the_postgres_choice_plans_again_and_keeps_the_other_choices(cx: &mut TestAppContext) {
    let rig = wizard(cx);
    let WizardRig { h, wizard, .. } = &rig;
    cx.update_window(h.window.into(), |_, window, cx| window.press("enter", cx))
        .unwrap();
    cx.run_until_parked();
    // Untick a hook, then choose "No database".
    let hook = cx.read_entity(wizard, |w, _| {
        w.scanned()
            .unwrap()
            .plan
            .items
            .iter()
            .find(|i| i.id.starts_with("hook:setup:install"))
            .map(|i| i.id.clone())
    });
    let Some(hook) = hook else {
        return;
    };
    click_scrolled(cx, h, "wizard-body", id(format!("item-{hook}")));
    scroll_up_in(cx, h, "wizard-body");
    click_in(cx, h, "pg-choice", 2);
    wait(cx, "the new plan", |cx| {
        cx.read_entity(wizard, |w, _| {
            w.scanned()
                .is_some_and(|s| s.plan.items.iter().all(|i| i.id != "service:db"))
        })
    });
    let selected = cx.read_entity(wizard, |w, _| w.selected().clone());
    assert!(
        !selected.contains(&hook),
        "the person's choice is kept: {selected:?}"
    );
    assert!(!selected.contains("service:db"));
}

#[gpui_kit::test]
fn a_repository_that_changed_since_the_plan_is_not_written_and_can_be_planned_again(
    cx: &mut TestAppContext,
) {
    let rig = wizard(cx);
    let WizardRig {
        h, wizard, root, ..
    } = &rig;
    cx.update_window(h.window.into(), |_, window, cx| window.press("enter", cx))
        .unwrap();
    cx.run_until_parked();
    let defaults = cx.read_entity(wizard, |w, _| w.selected().clone());
    click(cx, h, "wizard-next");
    wait(cx, "the review", |cx| {
        exists(cx, h, id("review-devflow.toml".into()))
    });

    // The repository changes before Apply: somebody writes a devflow.toml.
    let theirs = devflow_core::Config::template("blog", Uuid::new_v4(), None);
    std::fs::write(root.join("devflow.toml"), &theirs).unwrap();
    click(cx, h, "wizard-next");
    wait(cx, "the stale plan", |cx| {
        cx.read_entity(wizard, |w, _| w.is_stale())
    });
    assert_eq!(
        std::fs::read_to_string(root.join("devflow.toml")).unwrap(),
        theirs,
        "nothing was written over it"
    );
    assert!(exists(cx, h, "wizard-stale"));
    assert!(visible(cx, h, "wizard-replan"));

    // Plan again: back on the proposal, with the new state of the files.
    click(cx, h, "wizard-replan");
    wait(cx, "the new plan", |cx| {
        cx.read_entity(wizard, |w, _| {
            w.scanned().is_some() && w.step() == Step::Proposal
        })
    });
    let selected = cx.read_entity(wizard, |w, _| w.selected().clone());
    assert!(
        selected.len() <= defaults.len(),
        "{selected:?} against {defaults:?}: items the file has now are no longer proposed"
    );
}

#[gpui_kit::test]
fn an_apply_that_was_interrupted_is_resumed_by_the_wizard_not_called_stale(
    cx: &mut TestAppContext,
) {
    let rig = wizard(cx);
    let WizardRig {
        h, wizard, root, ..
    } = &rig;
    press_key(cx, h, "enter");
    // Tick an item that writes another file than devflow.toml, so the apply
    // has more than one file to be interrupted between.
    let (other, scroller) = cx.read_entity(wizard, |w, _| {
        let plan = &w.scanned().unwrap().plan;
        let other = plan
            .items
            .iter()
            .find(|i| {
                onboarding::selectable(plan, i)
                    && i.patch.file != "devflow.toml"
                    && !w.selected().contains(&i.id)
            })
            .map(|i| i.id.clone())
            .expect("the plan offers an item for another file");
        (other, "wizard-body")
    });
    click_scrolled(cx, h, scroller, id(format!("item-{other}")));
    assert!(cx.read_entity(wizard, |w, _| w.selected().contains(&other)));
    click(cx, h, "wizard-next");
    wait(cx, "the review", |cx| {
        exists(cx, h, id("review-devflow.toml".into()))
    });

    // An earlier apply of this very plan wrote the first file and stopped.
    let (plan, accepted) = cx.read_entity(wizard, |w, _| {
        let plan = w.scanned().unwrap().plan.clone();
        let accepted = onboarding::accepted(&plan, w.selected());
        (plan, accepted)
    });
    propose::apply_interrupted_after(root, &plan, &accepted, 1)
        .expect_err("the plan writes more than one file");
    assert!(
        root.join("devflow.toml").exists(),
        "the first file is there"
    );

    // Apply continues from the journal; it is not a repository that changed.
    click(cx, h, "wizard-next");
    wait(cx, "the apply finishes or is refused", |cx| {
        cx.read_entity(wizard, |w, _| w.applied().is_some() || w.is_stale())
    });
    assert!(
        !cx.read_entity(wizard, |w, _| w.is_stale()),
        "the wizard asked core, which resumes its own journal"
    );
    let resumed = cx.read_entity(wizard, |w, _| w.applied().map(|r| r.resumed));
    assert_eq!(resumed, Some(true));
    assert!(root.join("mise.toml").exists() || root.join("pitchfork.toml").exists());
}

#[gpui_kit::test]
fn the_wizard_is_driven_from_the_keyboard(cx: &mut TestAppContext) {
    let rig = wizard(cx);
    let WizardRig { h, wizard, .. } = &rig;
    // Right and Enter go forward, Left goes back.
    press_key(cx, h, "right");
    assert_eq!(step(cx, &rig), Step::Proposal);
    press_key(cx, h, "left");
    assert_eq!(step(cx, &rig), Step::Detected);
    press_key(cx, h, "enter");
    assert_eq!(step(cx, &rig), Step::Proposal);
    // Down moves to the second item, space toggles it.
    let second = cx.read_entity(wizard, |w, _| {
        let plan = &w.scanned().unwrap().plan;
        let selectable: Vec<_> = plan
            .items
            .iter()
            .filter(|i| onboarding::selectable(plan, i))
            .map(|i| i.id.clone())
            .collect();
        selectable.get(1).cloned()
    });
    let Some(second) = second else {
        return;
    };
    let was = cx.read_entity(wizard, |w, _| w.selected().contains(&second));
    press_key(cx, h, "down");
    press_key(cx, h, "space");
    let now = cx.read_entity(wizard, |w, _| w.selected().contains(&second));
    assert_ne!(was, now, "space toggled {second}");
    // Escape closes the dialog.
    press_key(cx, h, "escape");
    assert!(!visible(cx, h, "wizard-next"));
}

#[gpui_kit::test]
fn views_follow_the_theme_through_its_tokens(cx: &mut TestAppContext) {
    // The setup views name no color: both themes render them.
    let h = open(cx, install_loaded);
    for dark in [true, false] {
        cx.update(|cx| {
            let mode = if dark {
                gpui_kit::component::ThemeMode::Dark
            } else {
                gpui_kit::component::ThemeMode::Light
            };
            gpui_kit::component::Theme::change(mode, None, cx);
            crate::theme::apply_accent(cx);
        });
        frame(cx, &h, |window| {
            assert!(window.find("workspace-detail").visible(), "dark={dark}");
        });
        assert_eq!(cx.update(|cx| cx.theme().is_dark()), dark);
    }
    let _ = (
        PostgresMode::Managed,
        init_scratch_repo as fn(&Path) -> Entry,
    );
}
