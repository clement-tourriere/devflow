//! `AppState`: the registry, the selected project's model, and the background
//! work that keeps it fresh. Views read it and call its methods; every core
//! call runs on the background executor and results are applied only if their
//! `RequestKey` still matches the selected project.
use crate::{
    activity::Activity,
    jobs::{
        self, DoctorSource, Entries, LoadError, PitchforkSource, ProcessWant, RefreshRequest,
        Refreshed, ServicesRead,
    },
    model::{
        ConfigModel, LoadPhase, Page, ProjectModel, ProjectSummary, RefreshStamp, RequestKey,
        Section, Selection, SummaryStamp, Tab, WorkspaceRef, WorkspaceSummary, workspace_summaries,
    },
    services::{CoreServices, ServiceBackend},
    settings::{self, Settings, SettingsWriter, ThemeChoice},
};
use devflow_core::{
    health::ProjectProblem,
    project::Project,
    redact,
    registry::Entry,
    trust::{self, Scope, TrustState},
};
use gpui_kit::{
    AppContext as _, AsyncWindowContext, Context, EventEmitter, Task, Window,
    component::{Theme, WindowExt as _, notification::Notification},
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use uuid::Uuid;

const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Background core calls allowed at once; further refreshes wait for the next tick.
pub const MAX_JOBS: usize = 4;

/// Things the state asks the UI to show; the shell opens the dialogs.
pub enum AppEvent {
    OfferInitialize(PathBuf),
    /// Open the New workspace dialog for the selected project (which is loaded).
    OpenNewWorkspace,
    /// A removal was refused by a protection `force` overrides. The target is
    /// the exact workspace of the exact project the failed attempt was for.
    OfferForceRemove {
        target: WorkspaceRef,
        keep_data: bool,
        delete_branch: bool,
    },
}

/// Where installing pitchfork stands. It is the person's click that starts
/// it; nothing else does.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PitchforkInstall {
    #[default]
    Idle,
    /// The command being run.
    Running(String),
    /// It ran; what came of it.
    Done(String),
    Failed(String),
}

/// What an operation is about to run, which decides when a review is worth asking for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustPurpose {
    /// Setting up: setup commands and `[env]` recipes.
    Setup,
    /// Removing: teardown commands.
    Teardown,
    /// Starting pitchfork daemons.
    Processes,
}

/// What the user must review before an operation may run it. It keeps the
/// reviewed content, the workspace it is the content of and the selection it
/// came from, so an answer given later is applied only to that same project,
/// workspace and content.
#[derive(Clone)]
pub struct TrustPrompt {
    pub key: RequestKey,
    pub project: Arc<Project>,
    /// The directory whose scope was read: the workspace the operation acts on
    /// (or, for a new workspace, the one it is made from).
    pub target: PathBuf,
    pub scope: Scope,
}

/// Does an operation that runs `teardown` or setup hooks (or applies `[env]`
/// recipes) have to ask first? Only when what it runs exists and is not
/// approved. With
/// `DEVFLOW_TRUST_HOOKS` set, core runs them without approval and the GUI
/// simply asks core.
pub fn needs_trust_prompt(
    trust: Option<&TrustState>,
    scope: &Scope,
    teardown: bool,
    env_override: bool,
) -> bool {
    // Removal runs teardown hooks; setting up runs setup hooks and applies
    // `[env]` recipes. The rest of the scope (pitchfork processes) is not
    // started by either, so it alone is no reason to ask.
    let relevant = if teardown {
        !scope.hooks.teardown.is_empty()
    } else {
        scope.gates_setup()
    };
    relevant && !env_override && matches!(trust, Some(TrustState::Untrusted { .. }))
}

/// Does starting a workspace's daemons have to ask first? Only when it has
/// pitchfork configuration and the scope is not approved.
pub fn needs_process_review(trust: Option<&TrustState>, scope: &Scope, env_override: bool) -> bool {
    !scope.processes.is_empty()
        && !env_override
        && matches!(trust, Some(TrustState::Untrusted { .. }))
}

pub struct AppState {
    pub settings: Settings,
    pub page: Page,
    pub tab: Tab,
    pub entries: Vec<Entry>,
    pub entries_error: Option<String>,
    /// The listed projects that cannot be opened as they are, by id (core's
    /// `health`): the sidebar marks them and the project page explains them.
    pub problems: HashMap<Uuid, ProjectProblem>,
    pub selection: Selection,
    /// Output of the last workspace operation (running or finished).
    pub activity: Option<Activity>,
    /// Projects with a running mutation (one per project) and its title.
    pub(crate) mutating: HashMap<Uuid, String>,
    pub(crate) inflight: usize,
    /// How many times the user asked for a refresh (the button, ⌘R, the
    /// palette). Views that hold files of their own, like the Setup tab,
    /// compare it with the count they saw to know they should look at the disk.
    pub refresh_requests: u64,
    adhoc: Option<PathBuf>,
    /// Where the app was started from: removing the workspace that contains
    /// it is refused, as on the command line.
    pub launch_dir: Option<PathBuf>,
    /// Projects unfolded in the sidebar tree.
    pub expanded: HashSet<Uuid>,
    /// Cached workspaces per project, for the sidebar tree.
    pub summaries: HashMap<Uuid, ProjectSummary>,
    /// Unfolded projects whose workspaces are to be read as soon as a job slot
    /// is free, in the order they were asked for, each at most once.
    pub(crate) pending_summaries: VecDeque<Uuid>,
    /// A project to open the New workspace dialog for once it is loaded.
    pending_new_workspace: Option<Uuid>,
    /// Which pitchfork core is asked (the one on `PATH`; tests fix one).
    pub pitchfork: PitchforkSource,
    /// Installing pitchfork, on a click.
    pub pitchfork_install: PitchforkInstall,
    /// Moves when pitchfork may have changed (an install ran): views that
    /// show its state read it again.
    pub pitchfork_epoch: u64,
    /// The service the Data tab shows (its key); the first one when unset
    /// or when it is no longer configured.
    pub data_service: Option<String>,
    /// Where the doctor's checks come from (core; tests fix them).
    pub doctor: DoctorSource,
    /// What performs the container services' actions (core; tests fake it).
    pub services: Arc<dyn ServiceBackend>,
    /// Where settings are stored; `None` is the user's `gui.toml`.
    settings_path: Option<PathBuf>,
    /// Writes settings off the UI thread, one at a time, newest last.
    settings_writer: Arc<SettingsWriter>,
    /// The window is hidden in the menu bar: the poll runs less often.
    pub background: bool,
    poll_ticks: u64,
    _poll: Task<()>,
}

impl EventEmitter<AppEvent> for AppState {}

impl AppState {
    pub fn new(
        settings: Settings,
        preselect: Option<PathBuf>,
        launch_dir: Option<PathBuf>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Refresh only the selected project, and not while it is being mutated.
        let poll = cx.spawn_in(window, async move |this, cx| {
            loop {
                sleep(cx, POLL_INTERVAL).await;
                if this.update(cx, |this, cx| this.poll(cx)).is_err() {
                    break;
                }
            }
        });
        let mut state = Self {
            settings,
            page: Page::Project,
            tab: Tab::default(),
            entries: Vec::new(),
            entries_error: None,
            problems: HashMap::new(),
            selection: Selection::default(),
            activity: None,
            mutating: HashMap::new(),
            inflight: 0,
            refresh_requests: 0,
            adhoc: None,
            launch_dir,
            expanded: HashSet::new(),
            summaries: HashMap::new(),
            pending_summaries: VecDeque::new(),
            pending_new_workspace: None,
            pitchfork: PitchforkSource::OnPath,
            pitchfork_install: PitchforkInstall::Idle,
            pitchfork_epoch: 0,
            data_service: None,
            doctor: DoctorSource::Core,
            services: Arc::new(CoreServices),
            settings_path: None,
            settings_writer: Arc::default(),
            background: false,
            poll_ticks: 0,
            _poll: poll,
        };
        state.reload_entries(None, preselect, cx);
        state
    }

    /// A state with no background work, for headless view tests.
    #[cfg(test)]
    pub fn for_test(settings: Settings) -> Self {
        Self {
            settings,
            page: Page::Project,
            tab: Tab::default(),
            entries: Vec::new(),
            entries_error: None,
            problems: HashMap::new(),
            selection: Selection::default(),
            activity: None,
            mutating: HashMap::new(),
            // No free job slots: refreshes (which would reread the disk) never start.
            inflight: MAX_JOBS,
            refresh_requests: 0,
            adhoc: None,
            launch_dir: None,
            expanded: HashSet::new(),
            summaries: HashMap::new(),
            pending_summaries: VecDeque::new(),
            pending_new_workspace: None,
            pitchfork: PitchforkSource::OnPath,
            pitchfork_install: PitchforkInstall::Idle,
            pitchfork_epoch: 0,
            data_service: None,
            doctor: DoctorSource::Core,
            services: Arc::new(CoreServices),
            // Never the real `gui.toml`: tests that change a setting write here.
            settings_path: Some(
                std::env::temp_dir()
                    .join(format!("devflow-gui-test-{}", Uuid::new_v4()))
                    .join("gui.toml"),
            ),
            settings_writer: Arc::default(),
            background: false,
            poll_ticks: 0,
            _poll: Task::ready(()),
        }
    }

    /// Where settings are written, for tests that read it back.
    #[cfg(test)]
    pub fn settings_file(&self) -> Option<&std::path::Path> {
        self.settings_path.as_deref()
    }

    /// Write settings elsewhere, for tests of a file that cannot be written.
    #[cfg(test)]
    pub fn set_settings_file(&mut self, path: PathBuf) {
        self.settings_path = Some(path);
    }

    // ---- registry -----------------------------------------------------------

    pub fn reload_entries(
        &mut self,
        select: Option<Uuid>,
        preselect: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let adhoc = self.adhoc.clone();
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move { jobs::load_entries(adhoc, preselect) })
                .await;
            this.update(cx, |this, cx| this.apply_entries(loaded, select, cx))
                .ok();
        })
        .detach();
    }

    fn apply_entries(
        &mut self,
        loaded: Section<Entries>,
        select: Option<Uuid>,
        cx: &mut Context<Self>,
    ) {
        match loaded {
            Ok(loaded) => {
                self.entries = loaded.entries;
                self.problems = loaded.problems;
                self.adhoc = loaded.adhoc;
                self.entries_error = None;
                self.forget_unknown_projects();
                let wanted = loaded.select.or(select).or_else(|| {
                    let current = self.selection.current().map(|m| m.key.project);
                    current
                        .filter(|id| self.entries.iter().any(|e| e.id == *id))
                        .or_else(|| self.entries.first().map(|e| e.id))
                });
                match wanted {
                    Some(id) => self.select_project(id, cx),
                    None => self.selection.clear(),
                }
            }
            Err(message) => self.entries_error = Some(message),
        }
        cx.notify();
    }

    /// Forget a project (the registry entry only; the project is untouched).
    pub fn remove_entry(&mut self, entry: Entry, cx: &mut Context<Self>) {
        if entry.id.is_nil() {
            self.adhoc = None;
            self.entries.retain(|e| e.id != entry.id);
            self.forget_unknown_projects();
            if self
                .selection
                .current()
                .is_some_and(|m| m.key.project == entry.id)
            {
                self.selection.clear();
            }
            cx.notify();
            return;
        }
        let root = entry.root;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    jobs::catch(|| devflow_core::registry::remove(&root).map(|_| ()))
                })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(()) => this.reload_entries(None, None, cx),
                Err(message) => {
                    this.entries_error = Some(message);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Drop what the sidebar cached for projects that are no longer listed.
    fn forget_unknown_projects(&mut self) {
        let known: HashSet<Uuid> = self.entries.iter().map(|e| e.id).collect();
        self.expanded.retain(|id| known.contains(id));
        self.summaries.retain(|id, _| known.contains(id));
        self.pending_summaries.retain(|id| known.contains(id));
        if self
            .pending_new_workspace
            .is_some_and(|id| !known.contains(&id))
        {
            self.pending_new_workspace = None;
        }
    }

    // ---- selection ----------------------------------------------------------

    pub fn select_project(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.select_project_at(id, None, cx);
    }

    /// Select a project, and in it `workspace` when given. A project that was
    /// not selected is unfolded in the sidebar, so its workspaces show.
    fn select_project_at(&mut self, id: Uuid, workspace: Option<String>, cx: &mut Context<Self>) {
        self.page = Page::Project;
        if self
            .pending_new_workspace
            .is_some_and(|pending| pending != id)
        {
            self.pending_new_workspace = None;
        }
        if self
            .selection
            .current()
            .is_some_and(|m| m.key.project == id)
        {
            if let Some(name) = workspace {
                self.select_workspace(name, cx);
            }
            cx.notify();
            return;
        }
        let Some(entry) = self.entries.iter().find(|e| e.id == id).cloned() else {
            return;
        };
        self.selection.select(entry);
        self.expanded.insert(id);
        if let Some(model) = self.selection.current_mut() {
            // Kept once the list loads if it is still a workspace of the project.
            model.selected_workspace = workspace;
        }
        self.refresh(cx);
        cx.notify();
    }

    /// Select a project and unfold it in the sidebar (the command palette).
    pub fn reveal_project(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if self.entries.iter().any(|e| e.id == id) {
            self.expanded.insert(id);
        }
        self.select_project(id, cx);
    }

    /// A click on a project in the sidebar: show it, or fold and unfold it
    /// when it is the one already shown.
    pub fn open_project(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let shown = self.page == Page::Project
            && self
                .selection
                .current()
                .is_some_and(|m| m.key.project == id);
        if shown {
            self.toggle_expanded(id, cx);
        } else {
            self.reveal_project(id, cx);
        }
    }

    /// Show one workspace of a project: select the project, go to the
    /// Workspaces tab and select the workspace's row.
    pub fn open_workspace(&mut self, project: Uuid, name: String, cx: &mut Context<Self>) {
        self.tab = Tab::Workspaces;
        self.select_project_at(project, Some(name), cx);
    }

    /// Show a tab of the selected project.
    pub fn go_to_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.page = Page::Project;
        self.set_tab(tab, cx);
    }

    /// Ask the shell to open the New workspace dialog for `project` (the
    /// selected one when `None`). A project that is not loaded yet is selected
    /// first, and the dialog opens when its workspaces are known.
    pub fn request_new_workspace(&mut self, project: Option<Uuid>, cx: &mut Context<Self>) {
        let selected = self.selection.current().map(|m| m.key.project);
        let Some(id) = project.or(selected) else {
            return;
        };
        self.page = Page::Project;
        self.tab = Tab::Workspaces;
        let phase = self
            .selection
            .current()
            .filter(|m| Some(m.key.project) == Some(id))
            .map(|m| m.phase.clone());
        match phase {
            Some(LoadPhase::Ready) => {
                self.pending_new_workspace = None;
                cx.emit(AppEvent::OpenNewWorkspace);
            }
            // Nothing to create a workspace in: the page says why.
            Some(LoadPhase::NotInitialized | LoadPhase::Problem(_)) => {
                self.pending_new_workspace = None;
            }
            Some(LoadPhase::Loading) | None => {
                self.pending_new_workspace = Some(id);
                self.select_project(id, cx);
                if !self.entries.iter().any(|e| e.id == id) {
                    // Not a listed project: there is nothing to wait for.
                    self.pending_new_workspace = None;
                }
            }
        }
        cx.notify();
    }

    pub fn set_page(&mut self, page: Page, cx: &mut Context<Self>) {
        self.page = page;
        cx.notify();
    }

    pub fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        if tab.reads_on_open() {
            self.refresh(cx);
        }
        cx.notify();
    }

    pub fn select_workspace(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(model) = self.selection.current_mut() else {
            return;
        };
        if model.selected_workspace.as_deref() == Some(&name) {
            return;
        }
        model.selected_workspace = Some(name);
        model.logs = Default::default();
        // Both belong to the workspace that was selected before.
        model.pitchfork.status = None;
        model.trust = None;
        // A refresh already running was started for the previous workspace.
        model.invalidate_refresh();
        self.refresh(cx);
        cx.notify();
    }

    pub fn select_log(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if let Some(model) = self.selection.current_mut() {
            model.logs.selected = Some(path);
            model.invalidate_refresh();
        }
        self.refresh(cx);
    }

    /// The key of the service the Data tab shows: the chosen one if the
    /// project still configures it, else the first (PostgreSQL's, if any).
    pub fn selected_data_service(&self) -> Option<String> {
        let model = self.selection.current()?;
        let cards = crate::views::data_cards(model);
        self.data_service
            .as_ref()
            .and_then(|key| cards.iter().find(|c| &c.key == key))
            .or_else(|| cards.first())
            .map(|c| c.key.clone())
    }

    pub fn select_data_service(&mut self, key: String, cx: &mut Context<Self>) {
        self.data_service = Some(key);
        cx.notify();
    }

    /// Forget the refusal kept for the workspace at `path`.
    pub fn dismiss_refusal(&mut self, path: &Path, cx: &mut Context<Self>) {
        if let Some(model) = self.selection.current_mut()
            && model.refusals.remove(path).is_some()
        {
            cx.notify();
        }
    }

    /// The selected project by identity and place, to capture in an action.
    pub fn selected_project_ref(&self) -> Option<crate::model::ProjectRef> {
        self.selection.current().map(|m| m.project_ref())
    }

    /// Identifies the selected project; changes whenever the selection does.
    pub fn selected_key(&self) -> Option<RequestKey> {
        self.selection.current_key()
    }

    /// The running mutation's title, if the selected project has one.
    pub fn busy(&self) -> Option<&str> {
        let id = self.selection.current()?.key.project;
        self.mutating.get(&id).map(String::as_str)
    }

    /// What to review before an operation runs it in the workspace at
    /// `target` (the one it acts on; for a new workspace, the one it is made
    /// from), or `None` when no review is needed (nothing relevant, already
    /// approved, or the environment override is set and core decides). The
    /// scope and its approval state are those of `target` itself.
    pub fn trust_prompt(&self, target: &Path, purpose: TrustPurpose) -> Option<TrustPrompt> {
        let model = self.selection.current()?;
        let project = model.project.clone()?;
        let scope = trust::scope_at(&project, target);
        let state = trust::state_at(&project, target).ok();
        let ask = match purpose {
            TrustPurpose::Setup => {
                needs_trust_prompt(state.as_ref(), &scope, false, trust::env_override())
            }
            TrustPurpose::Teardown => {
                needs_trust_prompt(state.as_ref(), &scope, true, trust::env_override())
            }
            TrustPurpose::Processes => {
                needs_process_review(state.as_ref(), &scope, trust::env_override())
            }
        };
        ask.then_some(TrustPrompt {
            key: model.key,
            project,
            target: target.to_path_buf(),
            scope,
        })
    }

    /// Replace the selected project's loaded content with a fresh read (for
    /// example after the hooks changed under an open trust dialog).
    pub fn adopt_project(
        &mut self,
        key: RequestKey,
        project: Arc<Project>,
        cx: &mut Context<Self>,
    ) {
        if let Some(model) = self.selection.accept(key) {
            model.project = Some(project);
            model.invalidate_refresh();
            cx.notify();
        }
    }

    // ---- refresh ------------------------------------------------------------

    fn poll(&mut self, cx: &mut Context<Self>) {
        self.poll_ticks = self.poll_ticks.wrapping_add(1);
        if !crate::tray::poll_due(self.background, self.poll_ticks) {
            return;
        }
        let mutating = self
            .selection
            .current()
            .is_some_and(|m| self.mutating.contains_key(&m.key.project));
        if self.page == Page::Project && !mutating {
            self.refresh(cx);
        }
        // An unfolded project whose workspaces could not be read yet (an
        // error) is tried again.
        for id in self.unfolded_in_order() {
            if !self.summaries.get(&id).is_some_and(|s| s.loaded) {
                self.refresh_summary(id, cx);
            }
        }
    }

    /// The user asked for a refresh: the selected project, and the workspaces
    /// of every other unfolded project. The ones that find no free job slot
    /// are read as slots free up, not dropped.
    pub fn refresh_all(&mut self, cx: &mut Context<Self>) {
        self.refresh_requests += 1;
        self.refresh(cx);
        for id in self.unfolded_in_order() {
            self.refresh_summary(id, cx);
        }
        // Observers (the Setup tab) hear about the request even when the
        // refresh found nothing to do.
        cx.notify();
    }

    /// The unfolded projects, in the order the sidebar lists them.
    fn unfolded_in_order(&self) -> Vec<Uuid> {
        self.entries
            .iter()
            .map(|e| e.id)
            .filter(|id| self.expanded.contains(id))
            .collect()
    }

    // ---- sidebar tree ---------------------------------------------------------

    pub fn is_expanded(&self, id: Uuid) -> bool {
        self.expanded.contains(&id)
    }

    /// Fold or unfold a project in the sidebar. Unfolding reads its workspaces
    /// in the background when they are not already kept up to date.
    pub fn toggle_expanded(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
            self.refresh_summary(id, cx);
        }
        cx.notify();
    }

    /// Read a project's workspaces for the sidebar, off the UI thread. The
    /// selected project is skipped: its own refreshes write the summary. With
    /// no job slot free, the read waits in a queue (once per project) and
    /// starts when one is.
    pub(crate) fn refresh_summary(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let selected = self
            .selection
            .current()
            .is_some_and(|m| m.key.project == id);
        if selected || id.is_nil() {
            return;
        }
        let Some(root) = self
            .entries
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.root.clone())
        else {
            return;
        };
        if self.summaries.get(&id).is_some_and(|s| s.is_reading()) {
            return;
        }
        if self.inflight >= MAX_JOBS {
            if !self.pending_summaries.contains(&id) {
                self.pending_summaries.push_back(id);
            }
            return;
        }
        let generation = self.summaries.entry(id).or_default().begin_read();
        let stamp = SummaryStamp {
            project: id,
            root: root.clone(),
            generation,
        };
        self.inflight += 1;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { jobs::summarize(&root) })
                .await;
            this.update(cx, |this, cx| this.apply_summary(stamp, result, cx))
                .ok();
        })
        .detach();
        cx.notify();
    }

    /// Start the reads that waited for a job slot, as many as there are slots.
    /// Called whenever a job ends. A project that was folded or forgotten in
    /// the meantime is not read.
    pub(crate) fn start_queued_summaries(&mut self, cx: &mut Context<Self>) {
        while self.inflight < MAX_JOBS {
            let Some(id) = self.pending_summaries.pop_front() else {
                break;
            };
            if self.expanded.contains(&id) {
                self.refresh_summary(id, cx);
            }
        }
    }

    /// Apply a finished summary read if it still describes the project. A
    /// read from before a refresh, a mutation or a change of the project's
    /// location is dropped (and made again if the project is still unfolded).
    pub(crate) fn apply_summary(
        &mut self,
        stamp: SummaryStamp,
        result: Section<Vec<WorkspaceSummary>>,
        cx: &mut Context<Self>,
    ) {
        self.inflight = self.inflight.saturating_sub(1);
        self.apply_summary_result(stamp, result, cx);
        self.start_queued_summaries(cx);
    }

    fn apply_summary_result(
        &mut self,
        stamp: SummaryStamp,
        result: Section<Vec<WorkspaceSummary>>,
        cx: &mut Context<Self>,
    ) {
        let root = self
            .entries
            .iter()
            .find(|e| e.id == stamp.project)
            .map(|e| e.root.clone());
        // The project was forgotten while this was running; a project added
        // again since has its own reads.
        let Some(summary) = self.summaries.get_mut(&stamp.project) else {
            return;
        };
        summary.end_read(stamp.generation);
        if root.as_ref() != Some(&stamp.root) {
            return;
        }
        if !summary.accepts(stamp.generation) {
            if self.expanded.contains(&stamp.project) {
                self.refresh_summary(stamp.project, cx);
            }
            return;
        }
        match result {
            Ok(list) => {
                summary.workspaces = list;
                summary.loaded = true;
                summary.error = None;
            }
            Err(message) => summary.error = Some(message),
        }
        cx.notify();
    }

    /// Reads of this project's workspaces that are in flight are now stale.
    pub(crate) fn invalidate_summary(&mut self, id: Uuid) {
        if let Some(summary) = self.summaries.get_mut(&id) {
            summary.invalidate();
        }
    }

    /// Reload the selected project in the background. If one is already
    /// running, run again right after it so a fresh change is not missed.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.inflight >= MAX_JOBS {
            return;
        }
        let tab = self.tab;
        let Some(model) = self.selection.current_mut() else {
            return;
        };
        if model.refreshing {
            model.refresh_again = true;
            return;
        }
        model.refreshing = true;
        model.refresh_again = false;
        let stamp = model.begin_refresh();
        let workspace = model.selected_target();
        let needs_target = tab.needs_workspace() && workspace.is_none();
        let request = RefreshRequest {
            root: model.entry.root.clone(),
            listed: Some((model.entry.id, model.entry.name.clone())),
            load_config: model.config.is_none(),
            workspace,
            processes: match tab {
                Tab::Daemons => ProcessWant::Always,
                Tab::Workspaces => ProcessWant::WhenUsed,
                _ => ProcessWant::No,
            },
            want_logs: tab == Tab::Logs,
            selected_log: model.logs.selected.clone(),
            pitchfork: self.pitchfork.clone(),
            services: ServicesRead {
                backend: self.services.clone(),
                full: tab == Tab::Data,
            },
        };
        self.inflight += 1;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { jobs::refresh(request) })
                .await;
            this.update(cx, |this, cx| {
                this.apply_refresh(stamp, needs_target, result, cx)
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Apply a finished refresh if it still describes the model. A result for
    /// another project, another workspace, or from before a mutation or a
    /// newer request is dropped (and the current state looked at again).
    pub(crate) fn apply_refresh(
        &mut self,
        stamp: RefreshStamp,
        needs_target: bool,
        result: Result<Refreshed, LoadError>,
        cx: &mut Context<Self>,
    ) {
        self.inflight = self.inflight.saturating_sub(1);
        self.apply_refresh_result(stamp, needs_target, result, cx);
        // The selected project's follow-up refresh had the first claim on the slot.
        self.start_queued_summaries(cx);
    }

    fn apply_refresh_result(
        &mut self,
        stamp: RefreshStamp,
        needs_target: bool,
        result: Result<Refreshed, LoadError>,
        cx: &mut Context<Self>,
    ) {
        let mutating = self.mutating.contains_key(&stamp.key.project);
        let Some(model) = self.selection.accept(stamp.key) else {
            return;
        };
        model.refreshing = false;
        if !model.accepts(&stamp) {
            model.refresh_again = false;
            // A running mutation looks again when it ends.
            if !mutating {
                self.refresh(cx);
            }
            cx.notify();
            return;
        }
        match result {
            Err(LoadError::NotInitialized) => {
                model.phase = LoadPhase::NotInitialized;
                model.project = None;
            }
            Err(LoadError::Problem(problem)) => {
                model.phase = LoadPhase::Problem(problem.clone());
                model.project = None;
                self.problems.insert(stamp.key.project, problem);
            }
            Ok(refreshed) => {
                apply_loaded(model, refreshed);
                self.problems.remove(&stamp.key.project);
            }
        }
        // The sidebar's view of this project follows what was just read, and a
        // read of it that is still in flight is now older than that.
        let summary = self.summaries.entry(stamp.key.project).or_default();
        summary.invalidate();
        match &model.phase {
            LoadPhase::Ready => match &model.workspaces_error {
                None => {
                    summary.workspaces = workspace_summaries(&model.workspaces);
                    summary.loaded = true;
                    summary.error = None;
                }
                Some(message) => summary.error = Some(message.clone()),
            },
            LoadPhase::NotInitialized => {
                summary.workspaces.clear();
                summary.loaded = false;
                summary.error = Some("not initialized".into());
            }
            LoadPhase::Problem(problem) => summary.error = Some(problem.message.clone()),
            LoadPhase::Loading => {}
        }
        if self.pending_new_workspace == Some(stamp.key.project) {
            match model.phase {
                LoadPhase::Ready => {
                    self.pending_new_workspace = None;
                    cx.emit(AppEvent::OpenNewWorkspace);
                }
                LoadPhase::NotInitialized | LoadPhase::Problem(_) => {
                    self.pending_new_workspace = None;
                }
                LoadPhase::Loading => {}
            }
        }
        // The first load had no workspace to fetch daemons/logs for.
        let again = std::mem::take(&mut model.refresh_again)
            || (needs_target && model.selected_workspace.is_some());
        if again {
            self.refresh(cx);
        }
        cx.notify();
    }

    // ---- settings and theme ----------------------------------------------------

    /// Where settings are stored: the test file, or the user's `gui.toml`.
    fn settings_file_path(&self) -> anyhow::Result<PathBuf> {
        match &self.settings_path {
            Some(path) => Ok(path.clone()),
            None => settings::path(),
        }
    }

    /// Store `settings` (to `gui.toml`) and use them from now on. They are
    /// kept only when the write succeeded. This is the Settings form's Save:
    /// it waits for the file, and its result is shown right away.
    pub fn store_settings(&mut self, settings: Settings) -> anyhow::Result<()> {
        let path = self.settings_file_path()?;
        self.settings_writer.write_now(&path, &settings)?;
        self.settings = settings;
        Ok(())
    }

    /// Switch the theme now and remember the choice. Whatever the stored
    /// setting was (also `System`), this makes it `choice`. Other settings
    /// stay as saved: unsaved edits in the Settings form are not stored here.
    ///
    /// The choice is in effect before this returns; the file is written in
    /// the background, newest choice last, and a failure is reported when it
    /// happens (the choice stays in effect for the session).
    pub fn set_theme(&mut self, choice: ThemeChoice, window: &mut Window, cx: &mut Context<Self>) {
        choice.apply(Some(window), cx);
        self.settings.theme = choice;
        self.save_settings_in_background("Cannot save the theme", window, cx);
        cx.notify();
    }

    /// Turn "keep running in the menu bar" on or off now and remember it. The
    /// file is written in the background, as for the theme.
    pub fn set_keep_in_menu_bar(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.keep_in_menu_bar = on;
        self.save_settings_in_background("Cannot save the setting", window, cx);
        cx.notify();
    }

    /// Write the current settings to their file off the UI thread. Writes are
    /// one at a time and the newest value wins, so changes made in quick
    /// succession cannot be saved out of order.
    fn save_settings_in_background(
        &mut self,
        failure: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = match self.settings_file_path() {
            Ok(path) => path,
            Err(e) => {
                Self::notify_error(window, cx, failure, &format!("{e:#}"));
                return;
            }
        };
        if !self.settings_writer.submit(self.settings.clone()) {
            // A writer is running; it picks this value up when it is done.
            return;
        }
        let writer = self.settings_writer.clone();
        cx.spawn_in(window, async move |this, cx| {
            let failed = cx
                .background_spawn(async move { writer.drain(&path) })
                .await;
            if let Some(e) = failed {
                this.update_in(cx, |_, window, cx| {
                    Self::notify_error(window, cx, failure, &format!("{e:#}"))
                })
                .ok();
            }
        })
        .detach();
    }

    /// Flip between light and dark, from whatever is showing now.
    pub fn toggle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dark = Theme::global(cx).is_dark();
        self.set_theme(ThemeChoice::toggled(dark), window, cx);
    }

    /// The OS appearance changed: follow it when the setting is `System`.
    pub fn follow_system_appearance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.theme == ThemeChoice::System {
            ThemeChoice::System.apply(Some(window), cx);
        }
    }

    // ---- shared helpers for mutations ----------------------------------------

    pub(crate) fn notify_error(
        window: &mut Window,
        cx: &mut gpui_kit::App,
        title: &str,
        message: &str,
    ) {
        // Whatever the source of `message`, no credential reaches a notification.
        window.push_notification(
            Notification::error(truncate(&redact::text(message), 600))
                .title(title.to_string())
                .autohide(false),
            cx,
        );
    }
}

fn apply_loaded(model: &mut ProjectModel, refreshed: Refreshed) {
    model.phase = LoadPhase::Ready;
    model.project = Some(refreshed.project);
    match refreshed.workspaces {
        Ok(list) => {
            model.workspaces = list;
            model.workspaces_error = None;
        }
        Err(message) => model.workspaces_error = Some(message),
    }
    model.fix_selected_workspace();
    model.database = refreshed.database;
    // The detail is read on the Data tab only; the other tabs keep the last.
    let mut services = refreshed.services;
    if services.details.is_none() {
        services.details = model.services.details.take();
    }
    model.services = services;
    model.trust = refreshed.trust.ok();
    if let Some(pitchfork) = refreshed.pitchfork {
        model.pitchfork = pitchfork;
    }
    if let Some(config) = refreshed.config {
        match config {
            Ok(doc) => {
                let loaded = ConfigModel::new(doc, model.entry.id, &model.entry.root);
                model.config = Some(loaded);
                model.config_error = None;
            }
            Err(message) => model.config_error = Some(message),
        }
    }
    if let Some(logs) = refreshed.logs {
        model.logs = logs;
    }
}

pub(crate) async fn sleep(cx: &mut AsyncWindowContext, duration: Duration) {
    if let Ok(timer) = cx.update(|_, cx| cx.background_executor().timer(duration)) {
        timer.await;
    }
}

pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("héllo wörld", 5), "héllo…");
    }
}
