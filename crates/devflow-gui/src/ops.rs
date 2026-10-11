//! Mutations and user-initiated actions on `AppState`. A mutation is a job on
//! the background executor; a project runs one at a time, and progress is
//! forwarded through a bounded channel that a UI task drains in 100 ms ticks.
use crate::{
    activity::Activity,
    jobs::{
        self, ChannelProgress, Done, EVENT_QUEUE, Event, Failure, FailureKind, MAX_EVENTS_PER_TICK,
        Probe,
    },
    model::{ConfigIdentity, ConfigModel, ReloadTarget, RequestKey, Section, WorkspaceRef},
    settings::{self, LaunchPoll},
    state::{AppEvent, AppState, MAX_JOBS, sleep, truncate},
};
use anyhow::{Context as _, Result};
use devflow_core::{
    config::ConfigDocument,
    db::{DataSource, Server},
    pitchfork,
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
    Reviewed(Scope),
    /// Run without hooks' approval: core skips them and marks the workspace
    /// `needs_attention`.
    Skip,
}

impl TrustChoice {
    fn trust_hooks(&self) -> bool {
        matches!(self, Self::Reviewed(_))
    }
}

/// Is the loaded project still the one whose hooks were reviewed?
pub fn reviewed_matches(project: &Project, trust: &TrustChoice) -> bool {
    match trust {
        TrustChoice::Reviewed(scope) => trust::scope(project).digest == scope.digest,
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
        scope: Scope,
    },
    Failed(String),
}

/// Approve exactly `scope` for `project` (blocking; run it off the UI thread).
pub fn approve_reviewed(project: &Project, scope: &Scope) -> Approval {
    match jobs::catch(|| trust::approve_scope(project, scope)) {
        Ok(()) => Approval::Approved,
        Err(message) => match jobs::catch(|| project.reload()) {
            Ok(fresh) => {
                let fresh_scope = trust::scope(&fresh);
                if fresh_scope.digest == scope.digest {
                    Approval::Failed(message)
                } else {
                    Approval::Changed {
                        message,
                        project: Arc::new(fresh),
                        scope: fresh_scope,
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
        trust_hooks: trust.trust_hooks(),
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
    fn start_job(
        &mut self,
        title: String,
        show_activity: bool,
        hint: FailureHint,
        window: &mut Window,
        cx: &mut Context<Self>,
        work: impl FnOnce(Arc<Project>, &mut ChannelProgress) -> Result<Done> + Send + 'static,
    ) {
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
        if let Some(activity) = self
            .activity
            .as_mut()
            .filter(|a| a.key.project == key.project)
        {
            activity.outcome = Some(result.clone().map_err(|failure| failure.message));
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
        }
        cx.notify();
    }

    /// One-shot background work that does not hold the project's mutation lock.
    fn spawn_light<T: Send + 'static>(
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
        title: &str,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let unchanged = self
            .selection
            .current()
            .and_then(|m| m.project.as_ref())
            .is_none_or(|project| reviewed_matches(project, trust));
        if !unchanged {
            Self::notify_error(
                window,
                cx,
                title,
                "The project's hooks changed after you reviewed them. Start again to review them.",
            );
        }
        unchanged
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
        let title = format!("Create workspace {name}");
        if !self.hooks_unchanged(&trust, &title, window, cx) {
            return;
        }
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
                        trust_hooks: trust.trust_hooks(),
                        ..Default::default()
                    },
                };
                let outcome = setup::create(&project, request, progress)?;
                Ok(done_from(format!("Created workspace {name}"), &outcome))
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
        let title = format!("Set up {}", workspace.name);
        if !self.hooks_unchanged(&trust, &title, window, cx) {
            return;
        }
        self.start_job(
            title,
            true,
            FailureHint::None,
            window,
            cx,
            move |project, progress| {
                let options = SetupOptions {
                    trust_hooks: trust.trust_hooks(),
                    ..Default::default()
                };
                let outcome = setup::setup(&project, &workspace, options, false, false, progress)?
                    .context("setup did not run")?;
                Ok(done_from(format!("Set up {}", workspace.name), &outcome))
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
        let title = format!("Remove workspace {}", target.name);
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
        if !self.hooks_unchanged(&trust, &title, window, cx) {
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
            Ok(Done {
                summary: format!("Removed workspace {}", target.name),
                warnings: outcome.warnings,
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
        let title = format!("Snapshot {name}");
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
        let title = format!("Delete snapshot {name}@v{version}");
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
        let title = format!("Reset data of {}", workspace.name);
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

    // ---- daemons ------------------------------------------------------------------

    pub fn daemon_action(
        &mut self,
        id: String,
        start: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.selection.current().and_then(|m| m.selected_target()) else {
            return;
        };
        let title = format!("{} daemon {id}", if start { "Start" } else { "Stop" });
        self.start_job(title, false, FailureHint::None, window, cx, move |_, _| {
            if start {
                pitchfork::start(&target.path, &id)?;
            } else {
                pitchfork::stop(&target.path, &id)?;
            }
            Ok(Done {
                summary: format!("{} {id}", if start { "Started" } else { "Stopped" }),
                ..Default::default()
            })
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
    Done {
        summary: if needs_attention {
            format!("{summary}, but it needs attention (see the output)")
        } else {
            summary
        },
        warnings: outcome.warnings.clone(),
        needs_attention,
        select_workspace: Some(outcome.workspace.name.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::needs_trust_prompt;
    use devflow_core::{setup::ExpectedWorkspace, trust::TrustState};
    use uuid::Uuid;

    fn scope(setup: &[&str], switch: &[&str], teardown: &[&str]) -> Scope {
        Scope {
            setup: setup.iter().map(|s| s.to_string()).collect(),
            switch: switch.iter().map(|s| s.to_string()).collect(),
            teardown: teardown.iter().map(|s| s.to_string()).collect(),
            timeout: "15m".into(),
            digest: format!("{setup:?}{switch:?}{teardown:?}"),
        }
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
        assert!(!options.trust_hooks);
        let reviewed = TrustChoice::Reviewed(scope(&["a"], &[], &[]));
        assert!(remove_options(&choice, &reviewed, None, &target).trust_hooks);
        assert!(!remove_options(&choice, &TrustChoice::Skip, None, &target).trust_hooks);
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
            Some(&TrustState::NoHooks),
            &both,
            false,
            false
        ));
        assert!(!needs_trust_prompt(None, &both, false, false));
        // DEVFLOW_TRUST_HOOKS: no dialog; core honours the variable itself.
        assert!(!needs_trust_prompt(Some(&untrusted), &both, false, true));
    }

    #[test]
    fn reviewed_hooks_must_still_match_the_loaded_project() {
        let project = tests_support::project_with_hooks(&["bun install"]);
        let reviewed = trust::scope(&project);
        assert!(reviewed_matches(
            &project,
            &TrustChoice::Reviewed(reviewed.clone())
        ));
        assert!(reviewed_matches(&project, &TrustChoice::NotNeeded));
        assert!(reviewed_matches(&project, &TrustChoice::Skip));

        let changed = tests_support::project_with_hooks(&["bun install", "rm -rf ~"]);
        assert!(
            !reviewed_matches(&changed, &TrustChoice::Reviewed(reviewed)),
            "hooks nobody reviewed must not run"
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
                    setup: setup.iter().map(|s| s.to_string()).collect(),
                    ..HooksSection::default()
                },
                ..Config::default()
            },
        }
    }
}
