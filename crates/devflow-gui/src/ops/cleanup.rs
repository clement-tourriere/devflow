//! The actions of the Clean up page: read the inventory of workspaces Git
//! forgot, pick some, and remove the ones picked.
//!
//! Reading and removing are core's (`gc::inventory`, `gc::remove`). The read
//! runs on the background executor (it asks the database server and Docker);
//! the removal is a job like any other mutation, so it shows in the activity
//! panel, takes the project's mutation slot and is followed by a new read when
//! the page is open. A removal asks core for exactly the instances picked, each
//! with the generation of the record that was shown.
use super::{Done, FailureHint};
use crate::{jobs, model::Page, state::AppState};
use devflow_core::{gc, setup::Progress as _};
use gpui_kit::{Context, Window};
use std::path::PathBuf;
use uuid::Uuid;

const TITLE: &str = "Clean up workspaces";

impl AppState {
    /// Show the Clean up page of the open project (the one at `root`) and read
    /// its inventory.
    pub fn open_cleanup(&mut self, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if !self.open_project_at(&root, window, cx, TITLE) {
            return;
        }
        self.page = Page::Cleanup;
        self.load_cleanup(window, cx);
        cx.notify();
    }

    /// [`AppState::open_cleanup`] for the selected project (the sidebar, the
    /// palette).
    pub fn open_cleanup_of_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(root) = self.selection.current().map(|m| m.entry.root.clone()) {
            self.open_cleanup(root, window, cx);
        }
    }

    /// Read the inventory again, off the UI thread. The previous one stays on
    /// the page until the new one arrives; a result that is not the newest read's
    /// or is about another project is dropped.
    pub fn load_cleanup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(model) = self.selection.current() else {
            return;
        };
        let key = model.key;
        let read = self.cleanup.begin(key.project);
        let Some(project) = model.project.clone() else {
            self.cleanup.finish(
                read,
                key.project,
                Err("The project is not loaded yet.".into()),
            );
            cx.notify();
            return;
        };
        cx.notify();
        self.spawn_light(
            window,
            cx,
            move || jobs::catch(|| gc::inventory(&project, &gc::Options::default())),
            move |this, result, _, cx| {
                let selected = this
                    .selection
                    .current()
                    .is_some_and(|m| m.key.project == key.project);
                if selected {
                    this.cleanup.finish(read, key.project, result);
                    cx.notify();
                }
            },
        );
    }

    /// ⌘R and the palette's Refresh: the project, and the page when it is open.
    pub fn refresh_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_all(cx);
        if self.page == Page::Cleanup {
            self.load_cleanup(window, cx);
        }
    }

    pub fn pick_cleanup(&mut self, instance: Uuid, on: bool, cx: &mut Context<Self>) {
        self.cleanup.select(instance, on);
        cx.notify();
    }

    pub fn pick_all_cleanup(&mut self, on: bool, cx: &mut Context<Self>) {
        self.cleanup.select_all(on);
        cx.notify();
    }

    pub fn set_cleanup_snapshot(&mut self, on: bool, cx: &mut Context<Self>) {
        self.cleanup.snapshot = on;
        cx.notify();
    }

    /// Remove the instances picked (after the confirmation): a job, so it is
    /// shown as an activity and finished by a new read of the inventory.
    pub fn remove_cleanup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let targets = self.cleanup.targets();
        if targets.is_empty() {
            return;
        }
        let snapshot = self.cleanup.snapshot;
        let pitchfork = self.pitchfork.resolve();
        self.start_job(
            TITLE.into(),
            true,
            FailureHint::None,
            window,
            cx,
            move |project, progress| {
                let request = gc::RemoveRequest {
                    targets,
                    snapshot,
                    force: false,
                    pitchfork,
                };
                let outcome = gc::remove(&project, &request, &mut |line| progress.output(line))?;
                for removed in &outcome.removed {
                    for line in &removed.snapshots {
                        progress.output(&format!("{}: snapshot {line}", removed.workspace));
                    }
                    for line in &removed.dropped {
                        progress.output(&format!("{}: {line}", removed.workspace));
                    }
                }
                for skipped in &outcome.skipped {
                    progress.output(&format!(
                        "kept {}: {}",
                        skipped.workspace.as_deref().unwrap_or("a workspace"),
                        skipped.reason
                    ));
                }
                Ok(done_of(&outcome))
            },
        );
    }
}

/// What a finished clean-up tells the person: what was removed, and, as
/// warnings, what core kept and why, and what stays on record.
fn done_of(outcome: &gc::Outcome) -> Done {
    let mut warnings = Vec::new();
    for removed in &outcome.removed {
        for warning in &removed.warnings {
            warnings.push(format!("{}: {warning}", removed.workspace));
        }
        if let Some(may) = &removed.may_still_run {
            warnings.push(format!(
                "{}: daemons may still be running{}{}",
                removed.workspace,
                may.namespace
                    .as_deref()
                    .map(|namespace| format!(" under the namespace {namespace}"))
                    .unwrap_or_default(),
                if may.ids.is_empty() {
                    String::new()
                } else {
                    format!(": {}", may.ids.join(", "))
                }
            ));
        }
        if !removed.still_recorded.is_empty() {
            warnings.push(format!(
                "{}: still on record for a later clean-up: {}",
                removed.workspace,
                removed.still_recorded.join(", ")
            ));
        }
    }
    for skipped in &outcome.skipped {
        warnings.push(format!(
            "kept {}: {}",
            skipped.workspace.as_deref().unwrap_or("a workspace"),
            skipped.reason
        ));
    }
    let names: Vec<&str> = outcome
        .removed
        .iter()
        .map(|removed| removed.workspace.as_str())
        .collect();
    let snapshots: Vec<&str> = outcome
        .removed
        .iter()
        .flat_map(|removed| removed.snapshots.iter().map(String::as_str))
        .collect();
    let mut summary = if names.is_empty() {
        "Nothing was removed".to_string()
    } else {
        format!("Removed {}", names.join(", "))
    };
    if !snapshots.is_empty() {
        summary.push_str(&format!(". Snapshots kept: {}", snapshots.join(", ")));
    }
    Done {
        summary,
        needs_attention: !warnings.is_empty(),
        warnings,
        ..Done::default()
    }
}
