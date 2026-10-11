//! The container services' actions on `AppState`: start and stop a branch,
//! snapshots, reset, reconcile, drop and forget. Each is a job on the
//! background executor like the workspace operations: the project is locked
//! while it runs, its progress goes to the activity log, and its result is a
//! notification. They call [`ServiceBackend`] (core, outside tests) and decide
//! nothing: the clone protocol, the journal and the recovery are core's.
use crate::{
    jobs::{self, ChannelProgress, Done},
    model::ProjectRef,
    ops::FailureHint,
    services::{BoundWorkspace, ServiceBackend, ServiceTarget},
    state::AppState,
};
use anyhow::Result;
use devflow_core::{db::DataSource, project::Project, setup::Progress as _};
use gpui_kit::{
    ClipboardItem, Context, Window,
    component::{WindowExt as _, notification::Notification},
};
use std::sync::Arc;
use uuid::Uuid;

impl AppState {
    /// The project `owner` names, if it is the one open right now: a
    /// confirmation opened for one project must never run against another
    /// (the selection may have moved while it was on screen).
    pub(crate) fn open_project_for(&self, owner: &ProjectRef) -> Result<Arc<Project>, String> {
        let open = self.selection.current().filter(|m| {
            m.entry.id == owner.id
                && m.entry.root == owner.root
                && m.project.as_ref().is_some_and(|p| p.id() == owner.id)
        });
        match open.and_then(|m| m.project.clone()) {
            Some(project) => Ok(project),
            None => Err(
                "The project this was opened for is no longer the open one, so nothing was changed. Open it again and repeat the action."
                    .to_string(),
            ),
        }
    }

    fn service_job(
        &mut self,
        target: &ServiceTarget,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
        work: impl FnOnce(&Project, &dyn ServiceBackend, &mut ChannelProgress) -> Result<Done>
        + Send
        + 'static,
    ) {
        if let Err(message) = self.open_project_for(&target.project) {
            Self::notify_error(window, cx, &title, &message);
            return;
        }
        let backend = self.services.clone();
        self.start_job(
            title,
            true,
            FailureHint::None,
            window,
            cx,
            move |project, progress| work(&project, backend.as_ref(), progress),
        );
    }

    /// Start a branch's container. `label` is what the person knows it by
    /// (its workspace).
    pub fn service_start(
        &mut self,
        target: ServiceTarget,
        branch: Uuid,
        label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let service = target.key.clone();
        let title = format!("Start {service} for {label}");
        self.service_job(
            &target,
            title,
            window,
            cx,
            move |project, backend, progress| {
                progress.output(&format!("Starting the {service} container of {label}…"));
                let endpoint = backend.up(project, &service, branch)?;
                progress.output(&format!("It answers on {endpoint}."));
                Ok(Done {
                    summary: format!("{service} of {label} is running on {endpoint}"),
                    ..Done::default()
                })
            },
        );
    }

    /// Stop it; the data stays in its volume.
    pub fn service_stop(
        &mut self,
        target: ServiceTarget,
        branch: Uuid,
        label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let service = target.key.clone();
        let title = format!("Stop {service} for {label}");
        self.service_job(
            &target,
            title,
            window,
            cx,
            move |project, backend, progress| {
                progress.output(&format!("Stopping the {service} container of {label}…"));
                let was_running = backend.down(project, &service, branch)?;
                Ok(Done {
                    summary: if was_running {
                        format!("{service} of {label} stopped; its data is kept")
                    } else {
                        format!("{service} of {label} was not running")
                    },
                    ..Done::default()
                })
            },
        );
    }

    /// Seal a new version of `name` from the workspace's data.
    pub fn service_snapshot(
        &mut self,
        target: ServiceTarget,
        bound: BoundWorkspace,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let service = target.key.clone();
        let BoundWorkspace { workspace, expect } = bound;
        let title = format!(
            "Snapshot {} of {service}",
            devflow_core::redact::text(&name)
        );
        self.service_job(
            &target,
            title,
            window,
            cx,
            move |project, backend, progress| {
                progress.output(&format!(
                    "Saving the {service} data of {} and cloning it into a snapshot…",
                    workspace.name
                ));
                let label = backend.snapshot(project, &workspace, &service, &name, expect)?;
                Ok(Done {
                    summary: format!("Created snapshot {label} of {service}"),
                    ..Done::default()
                })
            },
        );
    }

    pub fn service_delete_snapshot(
        &mut self,
        target: ServiceTarget,
        snapshot: Uuid,
        label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let service = target.key.clone();
        let title = format!("Delete snapshot {label} of {service}");
        self.service_job(
            &target,
            title,
            window,
            cx,
            move |project, backend, progress| {
                progress.output(&format!("Removing {label}…"));
                backend.delete_snapshot(project, &service, snapshot)?;
                Ok(Done {
                    summary: format!("Deleted snapshot {label} of {service}"),
                    ..Done::default()
                })
            },
        );
    }

    /// Replace the workspace's data with a fresh clone of `to` (`source` says
    /// which, in words; `None` is the branch's recorded source).
    pub fn service_reset(
        &mut self,
        target: ServiceTarget,
        bound: BoundWorkspace,
        to: Option<DataSource>,
        source: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let service = target.key.clone();
        let BoundWorkspace { workspace, expect } = bound;
        let title = crate::ops::shown(&format!("Reset {service} of {}", workspace.name));
        self.service_job(
            &target,
            title,
            window,
            cx,
            move |project, backend, progress| {
                progress.output(&format!(
                    "Replacing the {service} data of {} with a fresh clone of {source}…",
                    workspace.name
                ));
                let how = backend.reset(project, &workspace, &service, to, expect)?;
                progress.output(&format!("Cloned: {how}. The URL did not change."));
                Ok(Done {
                    summary: format!("Reset {service} of {} from {source}", workspace.name),
                    ..Done::default()
                })
            },
        );
    }

    /// Finish or roll back what a crash interrupted.
    pub fn service_reconcile(
        &mut self,
        target: ServiceTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let service = target.key.clone();
        let title = format!("Reconcile {service}");
        self.service_job(
            &target,
            title,
            window,
            cx,
            move |project, backend, progress| {
                progress.output(&format!("Looking at the operations of {service}…"));
                let lines = backend.reconcile(project, &service)?;
                for line in &lines {
                    progress.output(&format!("- {line}"));
                }
                Ok(Done {
                    summary: if lines.is_empty() {
                        format!("Nothing to reconcile in {service}")
                    } else {
                        format!("Reconciled {service}: {} item(s)", lines.len())
                    },
                    ..Done::default()
                })
            },
        );
    }

    /// Remove a branch by its recorded identity (an orphan: no workspace has it).
    pub fn service_drop(
        &mut self,
        target: ServiceTarget,
        branch: Uuid,
        label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let service = target.key.clone();
        let title = format!("Drop {label} from {service}");
        self.service_job(
            &target,
            title,
            window,
            cx,
            move |project, backend, progress| {
                progress.output(&format!("Removing the container and volume of {label}…"));
                backend.drop_branch(project, &service, branch)?;
                Ok(Done {
                    summary: format!("Dropped {label} from {service}"),
                    ..Done::default()
                })
            },
        );
    }

    /// Remove a branch a workspace owns whose volume is gone: its data is
    /// lost, and the workspace has to be set up again to get a new one.
    pub fn service_drop_lost(
        &mut self,
        target: ServiceTarget,
        branch: Uuid,
        volume: String,
        label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let service = target.key.clone();
        let title = format!("Drop {label} from {service}");
        self.service_job(
            &target,
            title,
            window,
            cx,
            move |project, backend, progress| {
                progress.output(&format!("Removing what is left of {label}…"));
                backend.drop_lost_branch(project, &service, branch, &volume)?;
                Ok(Done {
                    summary: format!("Dropped {label} from {service}. Set the workspace up again to give it new data."),
                    ..Done::default()
                })
            },
        );
    }

    /// Remove everything recorded for a service the configuration no longer has.
    pub fn service_forget(
        &mut self,
        target: ServiceTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let service = target.key.clone();
        let title = format!("Forget {service}");
        self.service_job(
            &target,
            title,
            window,
            cx,
            move |project, backend, progress| {
                progress.output(&format!("Removing what is recorded for {service}…"));
                let removed = backend.forget(project, &service)?;
                for line in &removed {
                    progress.output(&format!("- removed {line}"));
                }
                Ok(Done {
                    summary: format!("Forgot {service}"),
                    ..Done::default()
                })
            },
        );
    }

    /// Measure copy-on-write on the service's Docker engine: a short helper
    /// container in the Docker VM.
    pub fn service_measure(
        &mut self,
        target: ServiceTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let service = target.key.clone();
        let title = format!("Measure copy-on-write for {service}");
        self.service_job(
            &target,
            title,
            window,
            cx,
            move |project, backend, progress| {
                progress.output("Writing 64 MiB into a scratch volume and cloning it…");
                let cow = backend.measure(project, &service)?;
                let word = crate::model::cow_label(cow.capability);
                progress.output(&format!("Result: {word}."));
                Ok(Done {
                    summary: format!("Copy-on-write on this Docker engine: {word}"),
                    warnings: cow.note.into_iter().collect(),
                    ..Done::default()
                })
            },
        );
    }

    /// Put a branch's connection URL, password included, on the clipboard.
    /// The URL is asked of core when this is clicked and goes nowhere else:
    /// not into the model, not into a message.
    pub fn copy_service_url(
        &mut self,
        target: ServiceTarget,
        branch: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let service = target.key.clone();
        let project = match self.open_project_for(&target.project) {
            Ok(project) => project,
            Err(message) => {
                Self::notify_error(window, cx, "Copy the URL", &message);
                return;
            }
        };
        let backend = self.services.clone();
        let for_job = service.clone();
        self.spawn_light(
            window,
            cx,
            move || jobs::catch(|| backend.url(&project, &for_job, branch)),
            move |_, result, window, cx| match result {
                Ok(url) => {
                    cx.write_to_clipboard(ClipboardItem::new_string(url));
                    window.push_notification(
                        Notification::success(format!(
                            "The connection URL of {service} is on the clipboard."
                        )),
                        cx,
                    );
                }
                Err(message) => AppState::notify_error(window, cx, "Copy the URL", &message),
            },
        );
    }
}
