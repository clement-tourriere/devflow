//! The dialogs of a `sqlite` service: take a snapshot of a workspace's
//! database, delete a snapshot, reset a database from a source, drop it. Each
//! says what is lost before it asks; the work is `AppState`'s sqlite jobs, and
//! core refuses what it must (the default workspace's file, a file devflow did
//! not make, a file another process has open).
//!
//! A dialog is opened for a workspace by its checkout (a name is only a label)
//! and holds core to the database it showed: the job hands the receipt's
//! instance and database identity back, and core refuses if the workspace
//! holds another by then.
use super::{
    dialogs::{cancel_button, footer, open_confirm},
    service_dialogs::{project_is_open, selected},
};
use crate::{
    model::DataOption,
    redaction::shown,
    services::{BoundWorkspace, ServiceTarget, SqliteDb, SqliteService, sqlite_source_text},
    state::AppState,
};
use devflow_core::{
    db::DataSource,
    receipt::ServiceBinding,
    setup::{BranchExpect, WorkspaceStatus},
    worktree::Workspace,
};
use gpui_kit::{
    component::{
        ActiveTheme as _, IndexPath, WindowExt as _,
        button::{Button, ButtonVariants as _},
        input::{Input, InputEvent, InputState},
        notification::Notification,
        select::{Select, SelectState},
        v_flex,
    },
    prelude::*,
    *,
};
use std::path::{Path, PathBuf};

fn label(text: &'static str) -> Div {
    div().text_sm().font_weight(FontWeight::MEDIUM).child(text)
}

// ---- what a dialog is opened for ---------------------------------------------------------

/// The sqlite binding of `key` in a workspace's receipt, if it holds one the
/// configuration still has.
fn binding<'a>(status: &'a WorkspaceStatus, key: &str) -> Option<&'a ServiceBinding> {
    status
        .receipt
        .as_ref()?
        .services
        .get(key)
        .filter(|b| b.engine == devflow_core::config::Engine::Sqlite && !b.orphaned)
}

/// What the workspace's receipt binds for `key` now: the instance and the
/// database identity a job holds core to. `None` for a file no binding records.
pub(crate) fn expect_of(status: &WorkspaceStatus, key: &str) -> Option<BranchExpect> {
    let receipt = status.receipt.as_ref()?;
    Some(BranchExpect {
        instance: receipt.instance_id,
        branch: binding(status, key)?.branch_id?,
        source: None,
    })
}

/// A workspace whose database a snapshot can be taken of.
#[derive(Clone, Debug)]
pub(crate) struct SqliteChoice {
    /// The workspace, masked: what the list shows.
    pub label: String,
    pub workspace: Workspace,
    pub expect: Option<BranchExpect>,
}

/// The workspaces of the open project whose database file exists.
pub(crate) fn snapshot_choices(app: &AppState, key: &str) -> Vec<SqliteChoice> {
    let Some(model) = app.selection.current() else {
        return Vec::new();
    };
    let Some(Ok(databases)) = model
        .services
        .sqlite_service(key)
        .map(|service| service.databases.as_ref())
    else {
        return Vec::new();
    };
    databases
        .iter()
        .filter(|d| d.exists)
        .filter_map(|d| {
            let status = model.workspace_at(&d.checkout)?;
            Some(SqliteChoice {
                label: d.workspace.shown(),
                workspace: status.workspace.clone(),
                expect: expect_of(status, key),
            })
        })
        .collect()
}

/// The database a button was drawn for, the workspace that holds it and what
/// its receipt binds, or why the row is stale or the action is not offered.
pub(crate) fn bound_database(
    app: &AppState,
    key: &str,
    checkout: &Path,
) -> Result<(BoundWorkspace, SqliteDb, SqliteService), String> {
    let model = app
        .selection
        .current()
        .ok_or("No project is open.".to_string())?;
    let service = model.services.sqlite_service(key).ok_or_else(|| {
        format!(
            "The configuration no longer has the service {}.",
            shown(key)
        )
    })?;
    let db = service
        .database_at(checkout)
        .ok_or("This workspace is not in the list any more; refresh and look again.".to_string())?;
    let name = db.workspace.shown();
    if let Some(why) = db.owner().refusal() {
        return Err(why.to_string());
    }
    let status = model
        .workspace_at(checkout)
        .ok_or_else(|| format!("{name} is no longer a workspace of this project."))?;
    let expect = expect_of(status, key)
        .ok_or_else(|| format!("{name} has no database of {} bound.", shown(key)))?;
    // The row and the workspace's receipt must show the same database: if not,
    // the window is between two reads, and nothing should change from it.
    if db.id != Some(expect.branch) {
        return Err(format!(
            "{name} has another database of {} than the list shows; refresh and look again.",
            shown(key)
        ));
    }
    Ok((
        BoundWorkspace {
            workspace: status.workspace.clone(),
            expect,
        },
        db.clone(),
        service.clone(),
    ))
}

// ---- what they say -----------------------------------------------------------------------

/// What choosing "reset to" can mean for a sqlite database: the source it was
/// made from, nothing, another workspace's file or a snapshot. A choice is its
/// position in the list, so the labels can be masked.
pub fn reset_options(service: &SqliteService, current: &SqliteDb) -> Vec<DataOption> {
    let recorded = current
        .source
        .as_deref()
        .map(|source| format!("Recorded source ({})", shown(&sqlite_source_text(source))))
        .unwrap_or_else(|| "Recorded source".to_string());
    let mut options = vec![
        DataOption {
            label: recorded,
            source: None,
        },
        DataOption {
            label: "Empty database".into(),
            source: Some(DataSource::Empty),
        },
    ];
    if let Ok(databases) = &service.databases {
        for other in databases
            .iter()
            .filter(|d| d.exists && d.checkout != current.checkout)
        {
            options.push(DataOption {
                label: format!("Workspace {}", other.workspace.shown()),
                // Named by its checkout, which cannot be ambiguous.
                source: Some(DataSource::Workspace {
                    name: other.checkout.display().to_string(),
                }),
            });
        }
    }
    if let Ok(snapshots) = &service.snapshots {
        // A snapshot whose file is gone cannot be copied from.
        for snapshot in snapshots.ready.iter().filter(|s| !s.damaged) {
            options.push(DataOption {
                label: format!("Snapshot {}", snapshot.label()),
                source: Some(DataSource::Snapshot {
                    name: snapshot.name.as_str().to_string(),
                    version: i32::try_from(snapshot.version).ok(),
                }),
            });
        }
    }
    options
}

/// The sentences of the reset's confirmation: what is lost, what stays.
pub fn reset_lines(service: &str, workspace: &str, path: &str) -> Vec<String> {
    let (service, workspace, path) = (shown(service), shown(workspace), shown(path));
    vec![
        format!("Reset the {service} database of {workspace}?"),
        format!(
            "Everything {workspace} wrote to {path} since it was copied is lost. The file is replaced by a fresh copy of the source you choose, at the same path."
        ),
        "Snapshots are not touched: take one first to keep this state. A file another process has open is refused, so close the application that uses it first.".to_string(),
    ]
}

/// The sentences of the drop's confirmation.
pub fn drop_lines(service: &str, workspace: &str, path: &str) -> Vec<String> {
    let (service, workspace, path) = (shown(service), shown(workspace), shown(path));
    vec![
        format!("Drop the {service} database of {workspace}?"),
        format!(
            "{path} is removed with the files SQLite keeps beside it, and so is the workspace's record of it: whatever {workspace} wrote to it is lost, and setting the workspace up again makes a fresh database from the source it was created from."
        ),
        "A file another process has open is refused, so close the application that uses it first."
            .to_string(),
    ]
}

/// The sentences of a snapshot delete's confirmation.
pub fn delete_lines(service: &str, snapshot: &str) -> Vec<String> {
    vec![
        format!("Delete {snapshot} of {}?", shown(service)),
        "Workspaces already copied from it keep their data. Version numbers are never reused."
            .to_string(),
    ]
}

// ---- snapshot -------------------------------------------------------------------------------

/// Take a snapshot of one workspace's database. `checkout` is the row that was
/// clicked (its workspace is preselected); without it the workspace selected
/// in the app is, else the first that has a file.
pub fn open_snapshot(
    state: &Entity<AppState>,
    target: ServiceTarget,
    checkout: Option<PathBuf>,
    window: &mut Window,
    cx: &mut App,
) {
    if !project_is_open(state, &target, window, cx) {
        return;
    }
    let key = target.key.clone();
    let (choices, wanted) = {
        let app = state.read(cx);
        (
            snapshot_choices(app, &key),
            checkout.or_else(|| super::sqlite_view::preselected(app)),
        )
    };
    if choices.is_empty() {
        window.push_notification(
            Notification::info(format!(
                "No workspace has a database file of {} yet, so there is nothing to snapshot.",
                shown(&key)
            )),
            cx,
        );
        return;
    }
    let first = wanted
        .and_then(|path| choices.iter().position(|c| c.workspace.path == path))
        .unwrap_or(0);
    let state = state.clone();
    let view = cx.new(|cx| {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("baseline"));
        let subscription = cx.subscribe_in(
            &name,
            window,
            |this: &mut SnapshotDialog, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.submit(window, cx);
                }
            },
        );
        let items: Vec<SharedString> = choices.iter().map(|c| c.label.clone().into()).collect();
        let select = cx.new(|cx| SelectState::new(items, Some(IndexPath::new(first)), window, cx));
        SnapshotDialog {
            state,
            target,
            name,
            select,
            choices,
            error: None,
            _subscription: subscription,
        }
    });
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title("Take snapshot")
            .w(px(480.))
            .child(view.clone())
    });
}

struct SnapshotDialog {
    state: Entity<AppState>,
    target: ServiceTarget,
    name: Entity<InputState>,
    select: Entity<SelectState<Vec<SharedString>>>,
    choices: Vec<SqliteChoice>,
    error: Option<SharedString>,
    _subscription: Subscription,
}

impl SnapshotDialog {
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name.read(cx).value().trim().to_string();
        let Some(choice) = selected(&self.select, cx)
            .and_then(|ix| self.choices.get(ix))
            .cloned()
        else {
            return;
        };
        if name.is_empty() {
            self.error = Some("Enter a snapshot name.".into());
            cx.notify();
            return;
        }
        window.close_dialog(cx);
        let target = self.target.clone();
        self.state.update(cx, |s, cx| {
            s.sqlite_snapshot(target, choice.workspace, choice.expect, name, window, cx)
        });
    }
}

impl Render for SnapshotDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("sqlite-snapshot-dialog")
            .gap_3()
            .child(
                v_flex()
                    .gap_1()
                    .child(label("Snapshot name"))
                    .child(Input::new(&self.name).id("sqlite-snapshot-name")),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(label("From the database of workspace"))
                    .child(Select::new(&self.select).w_full()),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        "A consistent copy is taken under SQLite's own locks, so a database that is being written to is waited for, never copied half-written. The snapshot is the next version of this name (NAME@vN) and can seed new workspaces.",
                    ),
            )
            .children(
                self.error
                    .clone()
                    .map(|e| div().text_sm().text_color(cx.theme().danger).child(e)),
            )
            .child(footer([
                cancel_button(),
                Button::new("sqlite-snapshot-save")
                    .primary()
                    .label("Take snapshot")
                    .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
            ]))
    }
}

// ---- reset ------------------------------------------------------------------------------------

/// Ask to reset the database of the workspace checked out at `checkout`: it
/// says what is lost and offers where the fresh data comes from.
pub fn open_reset(
    state: &Entity<AppState>,
    target: ServiceTarget,
    checkout: PathBuf,
    window: &mut Window,
    cx: &mut App,
) {
    if !project_is_open(state, &target, window, cx) {
        return;
    }
    let found = bound_database(state.read(cx), &target.key, &checkout);
    let (workspace, db, service) = match found {
        Ok(found) => found,
        Err(why) => {
            window.push_notification(Notification::info(why), cx);
            return;
        }
    };
    let options = reset_options(&service, &db);
    let lines = reset_lines(
        &target.key,
        &workspace.workspace.name,
        &crate::model::shorten_home(&db.file),
    );
    let state = state.clone();
    let view = cx.new(|cx| {
        let items: Vec<SharedString> = options.iter().map(|o| o.label.clone().into()).collect();
        let source = cx.new(|cx| SelectState::new(items, Some(IndexPath::new(0)), window, cx));
        ResetDialog {
            state,
            target,
            workspace,
            source,
            options,
            lines,
        }
    });
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title("Reset database")
            .w(px(520.))
            .child(view.clone())
    });
}

struct ResetDialog {
    state: Entity<AppState>,
    target: ServiceTarget,
    workspace: BoundWorkspace,
    source: Entity<SelectState<Vec<SharedString>>>,
    options: Vec<DataOption>,
    lines: Vec<String>,
}

impl Render for ResetDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("sqlite-reset-dialog")
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.lines[0].clone()),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(label("Reset to"))
                    .child(Select::new(&self.source).w_full()),
            )
            .children(self.lines[1..].iter().map(|line| {
                div()
                    .text_sm()
                    .text_color(cx.theme().warning)
                    .child(line.clone())
            }))
            .child(footer([
                cancel_button(),
                Button::new("sqlite-reset-confirm")
                    .danger()
                    .label("Reset database")
                    .on_click(cx.listener(|this, _, window, cx| {
                        let Some(choice) = selected(&this.source, cx)
                            .and_then(|ix| this.options.get(ix))
                            .cloned()
                        else {
                            return;
                        };
                        window.close_dialog(cx);
                        let target = this.target.clone();
                        let workspace = this.workspace.clone();
                        this.state.update(cx, |s, cx| {
                            s.sqlite_reset(
                                target,
                                workspace,
                                choice.source,
                                choice.label.to_lowercase(),
                                window,
                                cx,
                            )
                        });
                    })),
            ]))
            .test_support()
    }
}

// ---- drop and delete -------------------------------------------------------------------------

/// Ask before removing the database file of the workspace checked out at
/// `checkout`.
pub fn open_drop(
    state: &Entity<AppState>,
    target: ServiceTarget,
    checkout: PathBuf,
    window: &mut Window,
    cx: &mut App,
) {
    if !project_is_open(state, &target, window, cx) {
        return;
    }
    let found = bound_database(state.read(cx), &target.key, &checkout);
    let (workspace, db, _) = match found {
        Ok(found) => found,
        Err(why) => {
            window.push_notification(Notification::info(why), cx);
            return;
        }
    };
    let lines = drop_lines(
        &target.key,
        &workspace.workspace.name,
        &crate::model::shorten_home(&db.file),
    );
    let state = state.clone();
    open_confirm(
        window,
        cx,
        "Drop database",
        lines,
        "Drop",
        move |window, cx| {
            let (target, workspace) = (target.clone(), workspace.clone());
            state.update(cx, |s, cx| s.sqlite_drop(target, workspace, window, cx));
        },
    );
}

/// Ask before deleting snapshot `name`@v`version` (`label` is how it is drawn).
pub fn open_delete_snapshot(
    state: &Entity<AppState>,
    target: ServiceTarget,
    name: String,
    version: u32,
    label: String,
    window: &mut Window,
    cx: &mut App,
) {
    let state = state.clone();
    let lines = delete_lines(&target.key, &label);
    open_confirm(
        window,
        cx,
        "Delete snapshot",
        lines,
        "Delete",
        move |window, cx| {
            let (target, name, label) = (target.clone(), name.clone(), label.clone());
            state.update(cx, |s, cx| {
                s.sqlite_delete_snapshot(target, name, version, label, window, cx)
            });
        },
    );
}
