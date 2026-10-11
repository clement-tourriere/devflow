//! The container services' dialogs: create a snapshot, reset a branch, drop
//! an orphan, forget a service. Each says what is lost before it asks; the
//! work is `AppState`'s service jobs, and core refuses what it must.
use super::dialogs::{cancel_button, footer, open_confirm};
use crate::{
    model::DataOption,
    redaction::{RawName, shown},
    services::{
        BoundWorkspace, BranchRow, BranchState, OrphanBranch, ServiceBackend, ServiceDetail,
        ServiceTarget,
    },
    state::AppState,
};
use devflow_core::{
    db::DataSource,
    project::Project,
    redact,
    services::{Effects, Operation},
    setup::{BranchExpect, ResetSource},
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

fn label(text: &'static str) -> Div {
    div().text_sm().font_weight(FontWeight::MEDIUM).child(text)
}

/// Is the project `target` names the open one? A dialog is opened for a
/// project, and a palette entry or a stale button may name another: say so
/// instead of drawing that project's workspaces under this one's name.
fn project_is_open(
    state: &Entity<AppState>,
    target: &ServiceTarget,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    match state.read(cx).open_project_for(&target.project) {
        Ok(_) => true,
        Err(message) => {
            window.push_notification(Notification::info(message), cx);
            false
        }
    }
}

fn selected(select: &Entity<SelectState<Vec<SharedString>>>, cx: &App) -> Option<usize> {
    select.read(cx).selected_index(cx).map(|ix| ix.row)
}

/// The workspaces whose receipt binds a branch of service `key`.
pub fn bound_workspaces(state: &AppState, key: &str) -> Vec<BoundWorkspace> {
    state
        .selection
        .current()
        .map(|model| {
            model
                .workspaces
                .iter()
                .filter_map(|w| {
                    let receipt = w.receipt.as_ref()?;
                    let binding = receipt.services.get(key)?;
                    if binding.orphaned {
                        return None;
                    }
                    Some(BoundWorkspace {
                        workspace: w.workspace.clone(),
                        expect: BranchExpect {
                            instance: receipt.instance_id,
                            branch: binding.branch_id?,
                            source: None,
                        },
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// What choosing "reset to" can mean for a container service: its recorded
/// source, nothing, another workspace's data or a snapshot.
pub fn reset_options(
    detail: Option<&ServiceDetail>,
    others: &[Workspace],
    target: &str,
) -> Vec<DataOption> {
    let mut options = vec![
        DataOption {
            label: "Recorded source".into(),
            source: None,
        },
        DataOption {
            label: "Empty data".into(),
            source: Some(DataSource::Empty),
        },
    ];
    for workspace in others.iter().filter(|w| w.name != target) {
        options.push(DataOption {
            label: devflow_core::redact::text(&format!("Workspace {}", workspace.name)),
            source: Some(DataSource::Workspace {
                name: workspace.name.clone(),
            }),
        });
    }
    for snapshot in detail.map(|d| d.snapshots.as_slice()).unwrap_or_default() {
        options.push(DataOption {
            // The label is drawn (masked); the source names the snapshot as
            // recorded.
            label: format!("Snapshot {}", snapshot.label()),
            source: Some(DataSource::Snapshot {
                name: snapshot.name.as_str().to_string(),
                version: i32::try_from(snapshot.version).ok(),
            }),
        });
    }
    options
}

/// The line a confirmation adds when copying stops the source's server
/// (MySQL): whose, for how long it is usually down, and that its connections
/// are interrupted. `source_state` is what the page last read of that branch.
/// `None` when the copy leaves its source serving.
pub fn interruption(
    engine: &str,
    effects: &Effects,
    source_state: Option<BranchState>,
) -> Option<String> {
    let stopped = effects.stops_source.as_ref()?;
    let whose = match &stopped.workspace {
        Some(workspace) => format!("workspace {}", redact::text(workspace)),
        None => format!("branch {}", redact::text(&stopped.name)),
    };
    let last = effects
        .last_downtime_ms
        .map(|ms| format!(" The last stop took {}.", crate::model::format_millis(ms)))
        .unwrap_or_default();
    Some(match source_state {
        Some(BranchState::Stopped | BranchState::NotStarted | BranchState::Missing) => format!(
            "{engine} copies by stopping the source's server. The source {whose} is not running now, so nothing is interrupted.{last}"
        ),
        Some(BranchState::Running) => format!(
            "{engine} stops the source {whose}'s server for a few seconds; its connections are interrupted.{last}"
        ),
        _ => format!(
            "{engine} stops the source {whose}'s server for a few seconds if it is running; its connections are interrupted.{last}"
        ),
    })
}

/// What a confirmation says about the source of `op`, from core: the
/// interruption, or why it could not be checked.
fn source_note(
    backend: &dyn ServiceBackend,
    project: &Project,
    key: &str,
    op: Operation<'_>,
    detail: Option<&ServiceDetail>,
) -> Option<String> {
    let engine = detail.map(|d| d.engine.clone()).or_else(|| {
        project
            .config
            .containers()
            .find(|(k, _, _)| *k == key)
            .map(|(_, engine, _)| engine.as_str().to_string())
    })?;
    match backend.effects(project, key, op) {
        Ok(effects) => {
            let state = effects.stops_source.as_ref().and_then(|stopped| {
                detail?
                    .branches
                    .iter()
                    .find(|b| b.id == stopped.branch)
                    .map(|b| b.state)
            });
            interruption(&engine, &effects, state)
        }
        Err(error) => Some(redact::text(&format!(
            "devflow could not check whether this stops a running server: {error:#}"
        ))),
    }
}

/// What each of the reset's choices resolves to now, as a catalog identity
/// (or why it cannot be used). The confirmation holds core to the one chosen.
pub(crate) fn pin_options(
    app: &AppState,
    target: &ServiceTarget,
    workspace: &Workspace,
    options: &[DataOption],
) -> Vec<Result<ResetSource, String>> {
    match app.open_project_for(&target.project) {
        Ok(project) => options
            .iter()
            .map(|o| {
                app.services
                    .reset_source(&project, workspace, &target.key, o.source.as_ref())
                    .map_err(|error| redact::text(&format!("{error:#}")))
            })
            .collect(),
        Err(message) => vec![Err(message); options.len()],
    }
}

/// The note under the dialog's fields, for the entry chosen in `select`.
fn note_line(
    notes: &[Option<String>],
    select: &Entity<SelectState<Vec<SharedString>>>,
    cx: &App,
) -> Option<AnyElement> {
    let note = notes.get(selected(select, cx)?)?.clone()?;
    Some(
        div()
            .id("service-source-stopped")
            .text_sm()
            .text_color(cx.theme().warning)
            .child(note)
            .test_support()
            .into_any_element(),
    )
}

/// The sentences of the reset's confirmation: what is lost, what stays.
pub fn reset_warning(service: &str, workspace: &str) -> Vec<String> {
    // The workspace's name is a request's identity elsewhere; here it is drawn.
    let workspace = &redact::text(workspace);
    vec![
        format!("Reset the {service} data of {workspace}?"),
        format!(
            "Everything {workspace} wrote to its {service} since it was cloned is lost. The branch is replaced by a fresh clone of the source you choose."
        ),
        "Its connection URL (port and password) stays the same, and snapshots are not touched: take one first to keep this state.".to_string(),
    ]
}

/// One note per operation: what it does to its source (see `source_note`).
fn notes_for(
    state: &Entity<AppState>,
    target: &ServiceTarget,
    ops: Vec<Option<Operation<'_>>>,
    cx: &App,
) -> Vec<Option<String>> {
    let app = state.read(cx);
    let Ok(project) = app.open_project_for(&target.project) else {
        return vec![None; ops.len()];
    };
    let detail = app
        .selection
        .current()
        .and_then(|m| m.services.detail(&target.key));
    let backend = app.services.clone();
    ops.into_iter()
        .map(|op| {
            op.and_then(|op| source_note(backend.as_ref(), &project, &target.key, op, detail))
        })
        .collect()
}

// ---- snapshot -------------------------------------------------------------------------

/// The snapshot dialog's workspace choices as they are drawn (a choice is its
/// position in the list, so the name can be masked).
pub(crate) fn snapshot_choices(workspaces: &[BoundWorkspace]) -> Vec<String> {
    workspaces
        .iter()
        .map(|w| redact::text(&w.workspace.name))
        .collect()
}

/// The workspace a clicked branch row belongs to, by the branch id its receipt
/// binds (never by position or by name alone), or why the row is stale.
pub(crate) fn snapshot_row_workspace(
    workspaces: &[BoundWorkspace],
    row: &BranchRow,
) -> Result<usize, String> {
    let named = row.workspace.as_ref().map(RawName::as_str).ok_or_else(|| {
        "No workspace owns this branch, so there is no workspace data to snapshot.".to_string()
    })?;
    workspaces
        .iter()
        .position(|w| w.expect.branch == row.id && w.workspace.name == named)
        .ok_or_else(|| {
            format!(
                "{} no longer has the branch the list shows; refresh and look again.",
                redact::text(named)
            )
        })
}

/// Create a snapshot of one of `key`'s branches; `branch` is the row that was
/// clicked (the workspace it belongs to is preselected), `None` for the
/// general action.
pub fn open_snapshot(
    state: &Entity<AppState>,
    target: ServiceTarget,
    branch: Option<BranchRow>,
    window: &mut Window,
    cx: &mut App,
) {
    if !project_is_open(state, &target, window, cx) {
        return;
    }
    let key = target.key.clone();
    let workspaces = bound_workspaces(state.read(cx), &key);
    if workspaces.is_empty() {
        window.push_notification(
            Notification::info(format!("No workspace has a {key} branch to snapshot yet.")),
            cx,
        );
        return;
    }
    // A clicked row names the branch it was drawn for: it must still be the one
    // a workspace's receipt binds, or the window is between two reads and the
    // snapshot would be of another workspace's data. Only the general "Create
    // snapshot" action has a default (the first workspace).
    let first = match &branch {
        None => 0,
        Some(row) => match snapshot_row_workspace(&workspaces, row) {
            Ok(ix) => ix,
            Err(message) => {
                window.push_notification(Notification::info(message), cx);
                return;
            }
        },
    };
    let notes = notes_for(
        state,
        &target,
        workspaces
            .iter()
            .map(|w| {
                Some(Operation::Snapshot {
                    workspace: &w.workspace,
                })
            })
            .collect(),
        cx,
    );
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
        let items: Vec<SharedString> = snapshot_choices(&workspaces)
            .into_iter()
            .map(Into::into)
            .collect();
        let select = cx.new(|cx| SelectState::new(items, Some(IndexPath::new(first)), window, cx));
        let follow = cx.observe(&select, |_, _, cx| cx.notify());
        SnapshotDialog {
            state,
            target,
            name,
            select,
            workspaces,
            notes,
            error: None,
            _subscription: subscription,
            _follow: follow,
        }
    });
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title("Create snapshot")
            .w(px(480.))
            .child(view.clone())
    });
}

struct SnapshotDialog {
    state: Entity<AppState>,
    target: ServiceTarget,
    name: Entity<InputState>,
    select: Entity<SelectState<Vec<SharedString>>>,
    workspaces: Vec<BoundWorkspace>,
    /// What each workspace's snapshot does to its source, if anything.
    notes: Vec<Option<String>>,
    error: Option<SharedString>,
    _subscription: Subscription,
    _follow: Subscription,
}

impl SnapshotDialog {
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name.read(cx).value().trim().to_string();
        let Some(bound) = selected(&self.select, cx)
            .and_then(|ix| self.workspaces.get(ix))
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
            s.service_snapshot(target, bound, name, window, cx)
        });
    }
}

impl Render for SnapshotDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_3()
            .child(
                v_flex()
                    .gap_1()
                    .child(label("Snapshot name"))
                    .child(Input::new(&self.name).id("service-snapshot-name")),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(label("From the data of workspace"))
                    .child(Select::new(&self.select).w_full()),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!(
                        "The {} data is saved and cloned into a sealed snapshot, the next version of this name (NAME@vN), which can seed new workspaces.",
                        self.target.key
                    )),
            )
            .children(note_line(&self.notes, &self.select, cx))
            .children(
                self.error
                    .clone()
                    .map(|e| div().text_sm().text_color(cx.theme().danger).child(e)),
            )
            .child(footer([
                cancel_button(),
                Button::new("service-snapshot-save")
                    .primary()
                    .label("Create snapshot")
                    .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
            ]))
    }
}

// ---- reset ---------------------------------------------------------------------------

/// Ask to reset the branch `row` of service `key`: it says what is lost and
/// offers where the fresh data comes from.
pub fn open_reset(
    state: &Entity<AppState>,
    target: ServiceTarget,
    row: BranchRow,
    window: &mut Window,
    cx: &mut App,
) {
    if !project_is_open(state, &target, window, cx) {
        return;
    }
    let key = target.key.clone();
    let Some(name) = row.workspace.as_ref().map(|w| w.as_str().to_string()) else {
        return;
    };
    let (workspaces, detail, workspace) = {
        let app = state.read(cx);
        let workspaces = bound_workspaces(app, &key);
        let detail = app
            .selection
            .current()
            .and_then(|m| m.services.detail(&key).cloned());
        let workspace = workspaces
            .iter()
            .find(|w| w.workspace.name == name)
            .cloned();
        (workspaces, detail, workspace)
    };
    let Some(workspace) = workspace else {
        window.push_notification(
            Notification::info(format!(
                "{} is no longer a workspace of this project.",
                redact::text(&name)
            )),
            cx,
        );
        return;
    };
    // The row and the workspace's receipt must show the same branch: if not,
    // the window is between two reads, and nothing should be reset from it.
    if workspace.expect.branch != row.id {
        window.push_notification(
            Notification::info(format!(
                "{} has another branch of {key} than the list shows; refresh and look again.",
                redact::text(&name)
            )),
            cx,
        );
        return;
    }
    let others: Vec<Workspace> = workspaces.iter().map(|w| w.workspace.clone()).collect();
    let options = reset_options(detail.as_ref(), &others, &name);
    // What each choice resolves to now, as an identity: the confirmation holds
    // core to it, and the note about the source's server is computed from the
    // same answer.
    let pinned = pin_options(state.read(cx), &target, &workspace.workspace, &options);
    let mut notes = notes_for(
        state,
        &target,
        pinned
            .iter()
            .map(|found| match found {
                Ok(found) => Some(Operation::ResetFrom {
                    source: found.source,
                }),
                Err(_) => None,
            })
            .collect(),
        cx,
    );
    for (note, found) in notes.iter_mut().zip(&pinned) {
        if let Err(why) = found {
            *note = Some(format!("This cannot be used now: {why}"));
        }
    }
    let state = state.clone();
    let view = cx.new(|cx| {
        let items: Vec<SharedString> = options.iter().map(|o| o.label.clone().into()).collect();
        let source = cx.new(|cx| SelectState::new(items, Some(IndexPath::new(0)), window, cx));
        let follow = cx.observe(&source, |_, _, cx| cx.notify());
        ResetDialog {
            state,
            target,
            workspace,
            source,
            options,
            pinned,
            notes,
            _follow: follow,
        }
    });
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title("Reset branch data")
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
    /// What each choice resolved to when the dialog was drawn, or why it
    /// could not be.
    pinned: Vec<Result<ResetSource, String>>,
    /// What each choice does to its source, if anything.
    notes: Vec<Option<String>>,
    _follow: Subscription,
}

impl Render for ResetDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lines = reset_warning(&self.target.key, &self.workspace.workspace.name);
        v_flex()
            .id("service-reset-dialog")
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(lines[0].clone()),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(label("Reset to"))
                    .child(Select::new(&self.source).w_full()),
            )
            .children(lines[1..].iter().map(|line| {
                div()
                    .text_sm()
                    .text_color(cx.theme().warning)
                    .child(line.clone())
            }))
            .children(note_line(&self.notes, &self.source, cx))
            .child(footer([
                cancel_button(),
                Button::new("service-reset-confirm")
                    .danger()
                    .label("Reset data")
                    .on_click(cx.listener(|this, _, window, cx| {
                        let Some(ix) = selected(&this.source, cx) else {
                            return;
                        };
                        let (Some(choice), Some(pinned)) =
                            (this.options.get(ix).cloned(), this.pinned.get(ix).cloned())
                        else {
                            return;
                        };
                        // A choice that could not be resolved when shown is not
                        // asked of core.
                        let pinned = match pinned {
                            Ok(pinned) => pinned,
                            Err(why) => {
                                window.push_notification(
                                    Notification::error(format!(
                                        "This choice cannot be used: {why}"
                                    )),
                                    cx,
                                );
                                return;
                            }
                        };
                        window.close_dialog(cx);
                        let target = this.target.clone();
                        let mut workspace = this.workspace.clone();
                        workspace.expect.source = Some(pinned.source);
                        this.state.update(cx, |s, cx| {
                            s.service_reset(
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

// ---- drop and forget -------------------------------------------------------------------

/// Ask before removing an orphan branch of `key`.
pub fn open_drop(
    state: &Entity<AppState>,
    target: ServiceTarget,
    orphan: OrphanBranch,
    window: &mut Window,
    cx: &mut App,
) {
    let state = state.clone();
    let key = target.key.clone();
    let lines = vec![
        format!("Drop branch {} of {key}?", orphan.name),
        "Its container and volume are removed, by recorded identity. No workspace of this project owns it, but whatever it holds is lost."
            .to_string(),
    ];
    open_confirm(
        window,
        cx,
        "Drop branch",
        lines,
        "Drop",
        move |window, cx| {
            let (target, id, name) = (target.clone(), orphan.id, orphan.name.clone());
            state.update(cx, |s, cx| s.service_drop(target, id, name, window, cx));
        },
    );
}

fn lost_name(branch: uuid::Uuid) -> String {
    let id: String = branch.to_string().chars().take(8).collect();
    format!("branch {id}")
}

/// What the confirmation of [`open_drop_lost`] says.
pub(crate) fn drop_lost_lines(key: &str, branch: uuid::Uuid) -> Vec<String> {
    vec![
        format!("Drop {} of {}?", lost_name(branch), shown(key)),
        "A workspace uses this branch, but its volume no longer exists, so its data is already gone. This removes the branch's record and what is left of its container, by recorded identity. Set the workspace up again afterwards to give it a new branch."
            .to_string(),
    ]
}

/// Ask before removing the branch `branch` of `key`, which a workspace owns
/// but whose volume is gone. Core checks that the volume is still missing
/// when the job runs; a branch whose data is there is kept.
pub fn open_drop_lost(
    state: &Entity<AppState>,
    target: ServiceTarget,
    branch: uuid::Uuid,
    window: &mut Window,
    cx: &mut App,
) {
    let state = state.clone();
    let key = target.key.clone();
    let name = lost_name(branch);
    let lines = drop_lost_lines(&key, branch);
    open_confirm(
        window,
        cx,
        "Drop branch",
        lines,
        "Drop",
        move |window, cx| {
            let (target, name) = (target.clone(), name.clone());
            state.update(cx, |s, cx| {
                s.service_drop_lost(target, branch, name, window, cx)
            });
        },
    );
}

/// Ask before forgetting a service the configuration no longer has.
pub fn open_forget(
    state: &Entity<AppState>,
    target: ServiceTarget,
    window: &mut Window,
    cx: &mut App,
) {
    let state = state.clone();
    let key = target.key.clone();
    let lines = vec![
        format!("Forget service '{}'?", shown(&key)),
        "The configuration no longer has it. This removes every branch (container and volume) and snapshot recorded for it, by recorded identity, and then its entry in the catalog. The data is lost."
            .to_string(),
    ];
    open_confirm(
        window,
        cx,
        "Forget service",
        lines,
        "Forget",
        move |window, cx| {
            let target = target.clone();
            state.update(cx, |s, cx| s.service_forget(target, window, cx));
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::SnapshotRow;
    use std::path::PathBuf;
    // `gpui_kit::*` also exports a `test` attribute; these are plain tests.
    use std::prelude::v1::test;

    fn workspace(name: &str) -> Workspace {
        Workspace {
            name: name.into(),
            branch: Some(name.into()),
            path: PathBuf::from(format!("/tmp/{name}")),
            git_dir: PathBuf::from(format!("/tmp/{name}/.git")),
            is_default: false,
            locked: false,
            missing: false,
            head: None,
            fingerprint: None,
        }
    }

    #[test]
    fn the_reset_says_what_is_lost_and_what_stays() {
        let lines = reset_warning("cache", "feature-x");
        let text = lines.join(" ");
        assert!(text.contains("feature-x wrote to its cache since it was cloned is lost"));
        assert!(text.contains("URL (port and password) stays the same"));
        assert!(text.contains("snapshots are not touched"));
    }

    #[test]
    fn a_reset_can_go_to_the_recorded_source_nothing_another_workspace_or_a_snapshot() {
        let detail = ServiceDetail {
            key: "cache".into(),
            engine: "valkey".into(),
            image: "valkey/valkey:8".into(),
            docker: None,
            cow: crate::services::Cow {
                capability: devflow_core::config::CowCapability::Unknown,
                note: None,
                measured_at: None,
            },
            require_cow: false,
            unfinished: 0,
            last_clone: None,
            branches: Vec::new(),
            snapshots: vec![SnapshotRow {
                id: uuid::Uuid::nil(),
                name: "seed".into(),
                version: 2,
                source: "empty".into(),
                captured: None,
                clone: None,
                allocated: None,
                source_downtime_ms: None,
                created: String::new(),
            }],
            error: None,
        };
        let options = reset_options(
            Some(&detail),
            &[workspace("main"), workspace("feature-x")],
            "feature-x",
        );
        let labels: Vec<_> = options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Recorded source",
                "Empty data",
                "Workspace main",
                "Snapshot seed@v2"
            ]
        );
        assert_eq!(options[0].source, None);
        assert_eq!(
            options[3].source,
            Some(DataSource::Snapshot {
                name: "seed".into(),
                version: Some(2)
            })
        );
    }
}
