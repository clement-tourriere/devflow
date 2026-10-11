//! Remedies as buttons, and what each one does in the app.
//!
//! Core decides what can be done about a problem (`devflow_core::remedy`);
//! this module draws one button per remedy and runs the core-backed operation
//! the app already has for it (set up, approve, start, reconcile, forget, …).
//! It never shows or runs a command line.
//!
//! A remedy names its target (a workspace's path, a project's root, a service
//! key). Nothing here acts on "the current selection": the target must be the
//! open project's or the click says so and changes nothing.
use super::{
    dialogs::{self, open_confirm},
    service_dialogs,
};
use crate::{
    model::Tab,
    services::{OrphanBranch, ServiceTarget},
    state::{AppState, TrustPurpose},
};
use devflow_core::{config::EnvPreset, remedy::Remedy};
use gpui_kit::{
    component::{
        Sizable as _, WindowExt as _,
        button::{Button, ButtonVariants as _},
        h_flex,
    },
    prelude::*,
    *,
};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// The element id of the button for `remedy`, the `index`th of a row.
pub fn id(prefix: &str, index: usize, remedy: &Remedy) -> SharedString {
    format!("{prefix}-{index}-{}", remedy.kind()).into()
}

/// One remedy as a button. `close_dialog` closes the dialog the button is in
/// before the action starts (the doctor's dialog must not stay over a
/// confirmation or a running job).
pub fn button(
    state: &Entity<AppState>,
    prefix: &str,
    index: usize,
    remedy: &Remedy,
    close_dialog: bool,
) -> Button {
    button_with_id(state, id(prefix, index, remedy), remedy, close_dialog)
}

/// [`button`] with an element id of the caller's choosing.
pub fn button_with_id(
    state: &Entity<AppState>,
    id: SharedString,
    remedy: &Remedy,
    close_dialog: bool,
) -> Button {
    let (state, run) = (state.clone(), remedy.clone());
    let button = Button::new(id)
        .label(crate::redaction::shown(&remedy.label()))
        .on_click(move |_, window, cx| {
            if close_dialog {
                window.close_dialog(cx);
            }
            run_scoped(&state, &run, window, cx);
        });
    if remedy.destructive() {
        button.ghost().danger()
    } else {
        button.outline()
    }
}

/// A wrapping row of buttons, one per remedy.
pub fn row(state: &Entity<AppState>, prefix: &str, remedies: &[Remedy], close_dialog: bool) -> Div {
    h_flex().gap_2().flex_wrap().children(
        remedies
            .iter()
            .enumerate()
            .map(|(index, remedy)| button(state, prefix, index, remedy, close_dialog).small()),
    )
}

/// Say why nothing happened.
fn refuse(window: &mut Window, cx: &mut App, remedy: &Remedy, why: &str) {
    AppState::notify_error(window, cx, &remedy.label(), why);
}

/// The workspace of the open project checked out at `path`.
fn workspace_at(
    state: &Entity<AppState>,
    path: &Path,
    cx: &App,
) -> Option<devflow_core::worktree::Workspace> {
    state
        .read(cx)
        .selection
        .current()?
        .workspaces
        .iter()
        .find(|w| w.workspace.path == path)
        .map(|w| w.workspace.clone())
}

/// The workspace of the open project (the one at `root`) called `name`.
fn workspace_named(
    state: &Entity<AppState>,
    root: &Path,
    name: &str,
    cx: &App,
) -> Option<devflow_core::worktree::Workspace> {
    let model = state.read(cx).selection.current()?;
    if model.entry.root != root {
        return None;
    }
    model.workspace(name).map(|w| w.workspace.clone())
}

/// The service `key` of the open project, which must be the one at `root`.
fn service_target(
    state: &Entity<AppState>,
    root: &Path,
    key: &str,
    cx: &App,
) -> Option<ServiceTarget> {
    let project = state.read(cx).selected_project_ref()?;
    (project.root == root).then(|| ServiceTarget {
        project,
        key: key.to_string(),
    })
}

fn short(id: Uuid) -> String {
    id.to_string().chars().take(8).collect()
}

/// What the remedies that act on a workspace, a service or the open project do
/// in the app. The others (`Retry`, the project list's) are
/// `problems::run_remedy`'s.
pub fn run_scoped(state: &Entity<AppState>, remedy: &Remedy, window: &mut Window, cx: &mut App) {
    let not_open = "That is about a project that is not the open one. Open it, then try again.";
    let no_workspace = "That workspace is no longer part of the open project.";
    match remedy {
        Remedy::Setup { workspace } => {
            let Some(found) = workspace_at(state, workspace, cx) else {
                return refuse(window, cx, remedy, no_workspace);
            };
            let app = state.clone();
            let path = found.path.clone();
            dialogs::with_trust(
                state,
                &path,
                TrustPurpose::Setup,
                window,
                cx,
                move |trust, window, cx| {
                    let found = found.clone();
                    app.update(cx, |s, cx| s.rerun_setup(found, trust, window, cx));
                },
            );
        }
        Remedy::Trust { workspace } => {
            let Some(found) = workspace_at(state, workspace, cx) else {
                return refuse(window, cx, remedy, no_workspace);
            };
            // The review of what the workspace asks to run is the Setup tab's.
            state.update(cx, |s, cx| {
                s.select_workspace(found.name, cx);
                s.go_to_tab(Tab::Setup, cx);
            });
        }
        Remedy::StartDatabase { root } => {
            let root = root.clone();
            state.update(cx, |s, cx| {
                if s.open_project_at(&root, window, cx, "Start the database server") {
                    s.server_up(window, cx);
                }
            });
        }
        Remedy::StartService {
            root, key, branch, ..
        } => {
            let Some(target) = service_target(state, root, key, cx) else {
                return refuse(window, cx, remedy, not_open);
            };
            match branch {
                Some(branch) => {
                    let (branch, label) = (*branch, format!("branch {}", short(*branch)));
                    state.update(cx, |s, cx| {
                        s.service_start(target, branch, label, window, cx)
                    });
                }
                // Not about one branch: show the service, where each branch
                // has its Start.
                None => {
                    let key = key.clone();
                    state.update(cx, |s, cx| {
                        s.select_data_service(key, cx);
                        s.go_to_tab(Tab::Data, cx);
                    });
                }
            }
        }
        Remedy::MeasureServices { root, key } => {
            let Some(target) = service_target(state, root, key, cx) else {
                return refuse(window, cx, remedy, not_open);
            };
            state.update(cx, |s, cx| s.service_measure(target, window, cx));
        }
        Remedy::ReconcileService { root, key } => {
            let Some(target) = service_target(state, root, key, cx) else {
                return refuse(window, cx, remedy, not_open);
            };
            state.update(cx, |s, cx| s.service_reconcile(target, window, cx));
        }
        Remedy::ForgetService { root, key } => {
            let Some(target) = service_target(state, root, key, cx) else {
                return refuse(window, cx, remedy, not_open);
            };
            service_dialogs::open_forget(state, target, window, cx);
        }
        Remedy::DropServiceBranch { root, key, branch } => {
            let Some(target) = service_target(state, root, key, cx) else {
                return refuse(window, cx, remedy, not_open);
            };
            let orphan = OrphanBranch {
                service: key.clone(),
                id: *branch,
                name: format!("branch {}", short(*branch)),
                reason: String::new(),
            };
            service_dialogs::open_drop(state, target, orphan, window, cx);
        }
        Remedy::DropLostBranch { root, key, branch } => {
            let Some(target) = service_target(state, root, key, cx) else {
                return refuse(window, cx, remedy, not_open);
            };
            service_dialogs::open_drop_lost(state, target, *branch, window, cx);
        }
        Remedy::ReconcileDatabases { root } => {
            let root = root.clone();
            state.update(cx, |s, cx| s.reconcile_databases(root, false, window, cx));
        }
        Remedy::DropOrphanDatabases { root } => {
            let (app, root) = (state.clone(), root.clone());
            open_confirm(
                window,
                cx,
                "Drop unused databases",
                vec![
                    "Drop the scratch databases the configuration no longer lists?".to_string(),
                    "They are dropped by their recorded identity, and whatever they hold is lost. Adding their names back to extra_databases keeps them instead."
                        .to_string(),
                ],
                "Drop",
                move |window, cx| {
                    let root = root.clone();
                    app.update(cx, |s, cx| s.reconcile_databases(root, true, window, cx));
                },
            );
        }
        Remedy::InstallGitHook { root } => {
            let root = root.clone();
            state.update(cx, |s, cx| s.install_git_hook(root, window, cx));
        }
        Remedy::ChainGitHook { root } => chain_git_hook(state, root, window, cx),
        Remedy::InstallPitchfork => install_pitchfork(state, window, cx),
        Remedy::PruneWorktrees { root } => {
            let root = root.clone();
            state.update(cx, |s, cx| s.prune_worktrees(root, window, cx));
        }
        Remedy::RemoveWorkspace { root, name } => {
            let Some(found) = workspace_named(state, root, name, cx) else {
                return refuse(window, cx, remedy, no_workspace);
            };
            dialogs::open_remove(state, &found, window, cx);
        }
        Remedy::SwitchTo { root, name } => {
            let Some(found) = workspace_named(state, root, name, cx) else {
                return refuse(window, cx, remedy, no_workspace);
            };
            state.update(cx, |s, cx| {
                s.select_workspace(found.name, cx);
                s.go_to_tab(Tab::Workspaces, cx);
            });
        }
        Remedy::AddEnvPreset { root, preset } => {
            let Some(preset) = EnvPreset::parse(preset) else {
                return refuse(window, cx, remedy, "That preset is not one devflow knows.");
            };
            let root: PathBuf = root.clone();
            state.update(cx, |s, cx| s.add_env_preset(root, preset, window, cx));
        }
        // The project list's remedies are `problems::run_remedy`'s.
        Remedy::Retry
        | Remedy::ForgetProject { .. }
        | Remedy::LocateProject { .. }
        | Remedy::MigrateConfig { .. }
        | Remedy::Init { .. }
        | Remedy::OpenFile { .. }
        | Remedy::Reveal { .. }
        | Remedy::OpenGuide { .. } => super::problems::run_remedy(state, None, remedy, window, cx),
    }
}

/// Add devflow to the Git hook somebody else wrote: the person sees which file
/// is edited and what is added before anything is written.
fn chain_git_hook(state: &Entity<AppState>, root: &Path, window: &mut Window, cx: &mut App) {
    let hook = state
        .read(cx)
        .selection
        .current()
        .filter(|model| model.entry.root == root)
        .and_then(|model| model.project.as_ref())
        .and_then(|project| match devflow_core::githook::state(project) {
            Ok(devflow_core::githook::InstallState::Foreign { path }) => Some(path),
            _ => None,
        });
    let (app, root) = (state.clone(), root.to_path_buf());
    open_confirm(
        window,
        cx,
        "Add worktree setup to the existing hook",
        chain_lines(hook.as_deref()),
        "Add",
        move |window, cx| {
            let root = root.clone();
            app.update(cx, |s, cx| s.chain_git_hook(root, window, cx));
        },
    );
}

/// What the confirmation of [`chain_git_hook`] says.
pub(crate) fn chain_lines(hook: Option<&Path>) -> Vec<String> {
    let file = hook.map_or("The post-checkout hook".to_string(), |path| {
        crate::redaction::shown(&crate::model::shorten_home(path))
    });
    vec![
        format!("Add worktree setup to {file}?"),
        "One line is added at the end of the hook, so that worktrees other tools create are set up too. Nothing else in it changes. A hook that is a link, is not a shell script or ends before the added line is left alone."
            .to_string(),
    ]
}

/// Install pitchfork: the person sees what will run and confirms; the run is
/// the Daemons tab's. Without mise there is nothing devflow can run, so the
/// Daemons tab (which says what to do) opens instead.
fn install_pitchfork(state: &Entity<AppState>, window: &mut Window, cx: &mut App) {
    let plan = state.read(cx).pitchfork.install_plan();
    let Some(command) = plan.run.as_ref().map(|argv| argv.join(" ")) else {
        state.update(cx, |s, cx| s.go_to_tab(Tab::Daemons, cx));
        return;
    };
    let app = state.clone();
    open_confirm(
        window,
        cx,
        "Install pitchfork",
        vec![
            "devflow will run this command to install pitchfork:".to_string(),
            command,
        ],
        "Install",
        move |window, cx| {
            let plan = plan.clone();
            app.update(cx, |s, cx| s.install_pitchfork(plan, window, cx));
        },
    );
}
