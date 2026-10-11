//! `AppState`: the registry, the selected project's model, and the background
//! work that keeps it fresh. Views read it and call its methods; every core
//! call runs on the background executor and results are applied only if their
//! `RequestKey` still matches the selected project.
use crate::{
    activity::Activity,
    jobs::{self, Entries, LoadError, RefreshRequest, Refreshed},
    model::{
        ConfigModel, LoadPhase, Page, ProjectModel, RefreshStamp, RequestKey, Section, Selection,
        Tab, WorkspaceRef,
    },
    settings::Settings,
};
use devflow_core::{
    project::Project,
    redact,
    registry::Entry,
    trust::{self, Scope, TrustState},
};
use gpui_kit::{
    AppContext as _, AsyncWindowContext, Context, EventEmitter, Task, Window,
    component::{WindowExt as _, notification::Notification},
};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
use uuid::Uuid;

const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Background core calls allowed at once; further refreshes wait for the next tick.
pub const MAX_JOBS: usize = 4;

/// Things the state asks the UI to show; the shell opens the dialogs.
pub enum AppEvent {
    OfferInitialize(PathBuf),
    /// A removal was refused by a protection `force` overrides. The target is
    /// the exact workspace of the exact project the failed attempt was for.
    OfferForceRemove {
        target: WorkspaceRef,
        keep_data: bool,
        delete_branch: bool,
    },
}

/// Hooks the user must review before an operation may run them. It keeps the
/// reviewed content and the selection it came from, so an answer given later
/// is applied only to that same project and content.
#[derive(Clone)]
pub struct TrustPrompt {
    pub key: RequestKey,
    pub project: Arc<Project>,
    pub scope: Scope,
}

/// Does an operation that runs `teardown` or setup hooks have to ask first?
/// Only when the relevant hooks exist and are not approved. With
/// `DEVFLOW_TRUST_HOOKS` set, core runs them without approval and the GUI
/// simply asks core.
pub fn needs_trust_prompt(
    trust: Option<&TrustState>,
    scope: &Scope,
    teardown: bool,
    env_override: bool,
) -> bool {
    let relevant = if teardown {
        &scope.teardown
    } else {
        &scope.setup
    };
    !relevant.is_empty() && !env_override && matches!(trust, Some(TrustState::Untrusted { .. }))
}

pub struct AppState {
    pub settings: Settings,
    pub page: Page,
    pub tab: Tab,
    pub entries: Vec<Entry>,
    pub entries_error: Option<String>,
    pub selection: Selection,
    /// Output of the last workspace operation (running or finished).
    pub activity: Option<Activity>,
    /// Projects with a running mutation (one per project) and its title.
    pub(crate) mutating: HashMap<Uuid, String>,
    pub(crate) inflight: usize,
    adhoc: Option<PathBuf>,
    /// Where the app was started from: removing the workspace that contains
    /// it is refused, as on the command line.
    pub launch_dir: Option<PathBuf>,
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
            selection: Selection::default(),
            activity: None,
            mutating: HashMap::new(),
            inflight: 0,
            adhoc: None,
            launch_dir,
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
            selection: Selection::default(),
            activity: None,
            mutating: HashMap::new(),
            // No free job slots: refreshes (which would reread the disk) never start.
            inflight: MAX_JOBS,
            adhoc: None,
            launch_dir: None,
            _poll: Task::ready(()),
        }
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
                self.adhoc = loaded.adhoc;
                self.entries_error = None;
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

    // ---- selection ----------------------------------------------------------

    pub fn select_project(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.page = Page::Project;
        if self
            .selection
            .current()
            .is_some_and(|m| m.key.project == id)
        {
            cx.notify();
            return;
        }
        let Some(entry) = self.entries.iter().find(|e| e.id == id).cloned() else {
            return;
        };
        self.selection.select(entry);
        self.refresh(cx);
        cx.notify();
    }

    pub fn set_page(&mut self, page: Page, cx: &mut Context<Self>) {
        self.page = page;
        cx.notify();
    }

    pub fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        if tab.needs_workspace() {
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
        model.pitchfork.daemons.clear();
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

    /// Identifies the selected project; changes whenever the selection does.
    pub fn selected_key(&self) -> Option<RequestKey> {
        self.selection.current_key()
    }

    /// The running mutation's title, if the selected project has one.
    pub fn busy(&self) -> Option<&str> {
        let id = self.selection.current()?.key.project;
        self.mutating.get(&id).map(String::as_str)
    }

    /// The hooks to review before an operation runs them, or `None` when no
    /// review is needed (no relevant hooks, already trusted, or the
    /// environment override is set and core decides).
    pub fn trust_prompt(&self, teardown: bool) -> Option<TrustPrompt> {
        let model = self.selection.current()?;
        let project = model.project.clone()?;
        let scope = trust::scope(&project);
        let ask = needs_trust_prompt(
            model.trust.as_ref(),
            &scope,
            teardown,
            trust::env_override(),
        );
        ask.then_some(TrustPrompt {
            key: model.key,
            project,
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
        let mutating = self
            .selection
            .current()
            .is_some_and(|m| self.mutating.contains_key(&m.key.project));
        if self.page == Page::Project && !mutating {
            self.refresh(cx);
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
            load_config: model.config.is_none(),
            workspace,
            want_daemons: tab == Tab::Daemons,
            want_logs: tab == Tab::Logs,
            selected_log: model.logs.selected.clone(),
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
            Err(LoadError::Failed(message)) => model.phase = LoadPhase::Failed(message),
            Ok(refreshed) => apply_loaded(model, refreshed),
        }
        // The first load had no workspace to fetch daemons/logs for.
        let again = std::mem::take(&mut model.refresh_again)
            || (needs_target && model.selected_workspace.is_some());
        if again {
            self.refresh(cx);
        }
        cx.notify();
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
