//! Projects that cannot be opened as they are: the page that says what is
//! wrong in plain words, one button per remedy core offers, and the dialogs
//! behind the buttons (confirm a removal, review a configuration migration).
//!
//! Core decides what the remedies are (`devflow_core::remedy`); this module
//! only draws them and calls the core function behind each. It never shows or
//! runs a command line.
use super::{
    dialogs::{cancel_button, footer, host, open_confirm},
    onboarding::OnboardingView,
    ui::{self, Tone},
    widgets::{banner, caption},
};
use crate::{
    jobs,
    model::shorten_home,
    state::{AppEvent, AppState},
};
use devflow_core::{
    config_edit::migrate::Migration, health::ProjectProblem, redact, registry::Entry,
    remedy::Remedy,
};
use gpui_kit::{
    assets::IconName as Lucide,
    component::{
        Disableable as _, Icon, WindowExt as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        notification::Notification,
        v_flex,
    },
    prelude::*,
    *,
};
use std::path::PathBuf;
use uuid::Uuid;

/// The element ids of the buttons of `remedies`, in order: `remedy-<kind>`
/// (`remedy-forget_project`), and `remedy-<kind>-2`, `-3`... for the second and
/// later remedies of a kind (a problem can offer two files to open), so that
/// no two buttons of a page share an element id.
pub fn remedy_ids(remedies: &[Remedy]) -> Vec<SharedString> {
    let mut seen: Vec<&'static str> = Vec::new();
    remedies
        .iter()
        .map(|remedy| {
            let kind = remedy.kind();
            seen.push(kind);
            match seen.iter().filter(|k| **k == kind).count() {
                1 => format!("remedy-{kind}").into(),
                n => format!("remedy-{kind}-{n}").into(),
            }
        })
        .collect()
}

/// The project page for a project that cannot be opened.
pub fn page(
    state: &Entity<AppState>,
    entry: &Entry,
    problem: &ProjectProblem,
    cx: &App,
) -> AnyElement {
    let ids = remedy_ids(&problem.remedies);
    let buttons = problem.remedies.iter().enumerate().map(|(index, remedy)| {
        let (state, entry, run) = (state.clone(), entry.clone(), remedy.clone());
        let button = Button::new(ids[index].clone())
            .label(remedy.label())
            .on_click(move |_, window, cx| run_remedy(&state, Some(&entry), &run, window, cx));
        if index == 0 {
            button.primary()
        } else if remedy.destructive() {
            button.ghost().danger()
        } else {
            button.outline()
        }
    });
    let forgettable = problem
        .remedies
        .iter()
        .any(|r| matches!(r, Remedy::ForgetProject { .. }));
    v_flex()
        .id("project-problem")
        .size_full()
        .items_center()
        .justify_center()
        .p_8()
        .child(
            v_flex()
                .max_w(px(560.))
                .gap_3()
                .items_center()
                .text_center()
                .child(
                    Icon::new(Lucide::TriangleAlert)
                        .size_8()
                        .text_color(ui::tone_color(Tone::Warning, cx)),
                )
                .child(
                    div()
                        .text_xl()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(problem.kind.title()),
                )
                .child(
                    div()
                        .id("project-problem-message")
                        .text_sm()
                        .text_color(ui::muted(cx))
                        .child(redact::text(&problem.message)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(ui::muted(cx))
                        .child(shorten_home(&problem.root)),
                )
                .child(
                    h_flex()
                        .pt_2()
                        .gap_2()
                        .flex_wrap()
                        .justify_center()
                        .children(buttons),
                )
                .when(forgettable, |this| {
                    this.child(div().text_xs().text_color(ui::muted(cx)).child(
                        "Removing a project from the list never touches its folder or its data.",
                    ))
                }),
        )
        .test_support()
        .into_any_element()
}

/// What each remedy does in the app. `entry` is the listed project the remedy
/// is about, when the caller has it.
pub fn run_remedy(
    state: &Entity<AppState>,
    entry: Option<&Entry>,
    remedy: &Remedy,
    window: &mut Window,
    cx: &mut App,
) {
    match remedy {
        // Look again at the project the button is about, not at whichever
        // project is selected.
        Remedy::Retry => match entry {
            Some(entry) => state.update(cx, |s, cx| s.retry_project(entry.clone(), cx)),
            None => state.update(cx, |s, cx| s.refresh(cx)),
        },
        Remedy::ForgetProject { root } => {
            let listed = entry.filter(|e| e.root == *root).cloned().or_else(|| {
                state
                    .read(cx)
                    .entries
                    .iter()
                    .find(|e| e.root == *root)
                    .cloned()
            });
            if let Some(listed) = listed {
                confirm_forget(state, listed, window, cx);
            }
        }
        Remedy::LocateProject { id, .. } => {
            let id = *id;
            state.update(cx, |s, cx| s.locate_project(id, window, cx));
        }
        Remedy::MigrateConfig { root } => {
            let id = entry
                .map(|e| e.id)
                .or_else(|| {
                    state
                        .read(cx)
                        .entries
                        .iter()
                        .find(|e| e.root == *root)
                        .map(|e| e.id)
                })
                .unwrap_or_default();
            open_migration(state, id, root.clone(), window, cx);
        }
        Remedy::Init { root } => {
            let root = root.clone();
            state.update(cx, |_, cx| cx.emit(AppEvent::OfferInitialize(root)));
        }
        Remedy::OpenFile { path } => {
            let path = path.clone();
            state.update(cx, |s, cx| s.open_in_editor(&path, window, cx));
        }
        Remedy::Reveal { path } => cx.reveal_path(path),
        Remedy::OpenGuide { url } => cx.open_url(url),
    }
}

/// Ask before taking a project off the list.
pub fn confirm_forget(state: &Entity<AppState>, entry: Entry, window: &mut Window, cx: &mut App) {
    let state = state.clone();
    let lines = vec![
        format!("Remove '{}' from the list of projects?", entry.name),
        format!(
            "Its folder ({}) and its data are not touched. You can add the project again later.",
            shorten_home(&entry.root)
        ),
    ];
    open_confirm(
        window,
        cx,
        "Remove from list",
        lines,
        "Remove",
        move |window, cx| {
            let entry = entry.clone();
            state.update(cx, |s, cx| s.forget_project(entry, window, cx));
        },
    );
}

/// Ask before taking the projects whose folder was not found off the list.
/// Only the `entries` shown here are removed, and only if their folder is still
/// not found when the person confirms.
pub fn confirm_forget_missing(
    state: &Entity<AppState>,
    entries: Vec<Entry>,
    window: &mut Window,
    cx: &mut App,
) {
    let state = state.clone();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    let mut lines = vec![format!(
        "These projects' folders were not found (moved, deleted, or on a drive that is not \
         connected): {}.",
        names.join(", ")
    )];
    lines.push(
        "They are removed from the list; one whose folder is back by then is kept. Nothing on \
         disk is touched."
            .to_string(),
    );
    open_confirm(
        window,
        cx,
        "Remove unavailable projects",
        lines,
        "Remove",
        move |window, cx| {
            let confirmed = entries.clone();
            state.update(cx, |s, cx| s.forget_missing_projects(confirmed, window, cx));
        },
    );
}

// ---- configuration migration -----------------------------------------------------

/// Review, then write, the rewrite of an earlier build's configuration.
pub fn open_migration(
    state: &Entity<AppState>,
    id: Uuid,
    root: PathBuf,
    window: &mut Window,
    cx: &mut App,
) {
    let view = cx.new(|cx| MigrationDialog::new(state.clone(), id, root, cx));
    host(window, cx, "Migrate configuration", 640., view);
}

enum Plan {
    Loading,
    Ready(Migration),
    Writing(Migration),
    /// The plan was refused, or the write failed: why.
    Failed(String),
}

struct MigrationDialog {
    state: Entity<AppState>,
    id: Uuid,
    plan: Plan,
}

impl MigrationDialog {
    fn new(state: Entity<AppState>, id: Uuid, root: PathBuf, cx: &mut Context<Self>) -> Self {
        cx.spawn(async move |this, cx| {
            let planned = cx
                .background_spawn(async move { jobs::plan_migration(root) })
                .await;
            this.update(cx, |this, cx| {
                this.plan = match planned {
                    Ok(migration) => Plan::Ready(migration),
                    Err(message) => Plan::Failed(message),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
        Self {
            state,
            id,
            plan: Plan::Loading,
        }
    }

    fn write(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Plan::Ready(migration) = std::mem::replace(&mut self.plan, Plan::Loading) else {
            return;
        };
        let for_job = migration.clone();
        self.plan = Plan::Writing(migration);
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let written = cx
                .background_spawn(async move { jobs::apply_migration(for_job) })
                .await;
            this.update_in(cx, |this, window, cx| match written {
                Ok(()) => {
                    window.close_dialog(cx);
                    window.push_notification(
                        Notification::success(
                            "The configuration is in the current format. Your database, its \
                             branches and your workspaces were not touched.",
                        )
                        .title("Migrate configuration"),
                        cx,
                    );
                    let id = this.id;
                    this.state.update(cx, |s, cx| s.project_changed(id, cx));
                }
                Err(message) => {
                    this.plan = Plan::Failed(message);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }
}

impl Render for MigrationDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.plan {
            Plan::Loading => ui::loading("Working out the changes…", cx)
                .h(px(120.))
                .into_any_element(),
            Plan::Ready(migration) | Plan::Writing(migration) => v_flex()
                .id("migration-review")
                .test_support()
                .gap_3()
                .child(div().text_sm().child(
                    "devflow will rewrite these files in the current format. Comments and \
                         formatting are kept.",
                ))
                .children(
                    migration
                        .files
                        .iter()
                        .map(|file| OnboardingView::diff_card(&file.preview, cx)),
                )
                .children(migration.notes.iter().map(|note| {
                    div()
                        .text_xs()
                        .text_color(ui::muted(cx))
                        .child(note.clone())
                }))
                .into_any_element(),
            Plan::Failed(message) => v_flex()
                .gap_2()
                .child(caption("Cannot migrate", cx))
                .child(
                    banner(
                        Tone::Danger,
                        Lucide::CircleAlert,
                        redact::text(message),
                        None,
                        cx,
                    )
                    .id("migration-error")
                    .test_support(),
                )
                .into_any_element(),
        };
        let writing = matches!(self.plan, Plan::Writing(_));
        let ready = matches!(self.plan, Plan::Ready(_));
        v_flex().gap_3().child(body).child(footer([
            cancel_button(),
            Button::new("migration-apply")
                .primary()
                .label(if writing { "Migrating…" } else { "Migrate" })
                .disabled(!ready)
                .loading(writing)
                .on_click(cx.listener(|this, _, window, cx| this.write(window, cx))),
        ]))
    }
}
