use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Tabs},
    Frame,
};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use super::action::*;
use super::components::help;
use super::components::logs::LogsComponent;
use super::components::proxy_tab::ProxyTabComponent;
use super::components::services_tab::ServicesTabComponent;
use super::components::system::SystemComponent;
use super::components::workspaces::WorkspacesComponent;
use super::components::Component;
use super::context::DevflowContext;
use super::event::{AppEvent, EventHandler};
use super::theme;

/// Modal overlay state
#[derive(Debug)]
enum ModalState {
    None,
    Help,
    Confirm {
        title: String,
        message: String,
        on_confirm: Box<Action>,
    },
    Input {
        title: String,
        input: String,
        target: InputTarget,
    },
    Select {
        title: String,
        options: Vec<String>,
        selected: usize,
        on_select: SelectTarget,
    },
}

/// Main TUI application
pub struct App {
    pub(super) context: DevflowContext,
    // Components (5 tabs)
    workspaces: WorkspacesComponent,
    services_tab: ServicesTabComponent,
    proxy_tab: ProxyTabComponent,
    system: SystemComponent,
    logs: LogsComponent,
    // State
    active_tab: usize,
    modal: ModalState,
    status_message: Option<(String, bool, Instant)>, // (msg, is_error, when)
    running: bool,
    open_branch_on_exit: Option<String>,
    tab_names: Vec<&'static str>,
    spinner_tick: usize,
    // Background task channel
    bg_tx: mpsc::UnboundedSender<Action>,
    bg_rx: mpsc::UnboundedReceiver<Action>,
}

impl App {
    pub fn new(context: DevflowContext) -> Self {
        let tab_names = vec!["Workspaces", "Services", "Proxy", "System", "Logs"];
        let (bg_tx, bg_rx) = mpsc::unbounded_channel();
        Self {
            context,
            workspaces: WorkspacesComponent::new(),
            services_tab: ServicesTabComponent::new(),
            proxy_tab: ProxyTabComponent::new(),
            system: SystemComponent::new(),
            logs: LogsComponent::new(),
            active_tab: 0,
            modal: ModalState::None,
            status_message: None,
            running: true,
            open_branch_on_exit: None,
            tab_names,
            spinner_tick: 0,
            bg_tx,
            bg_rx,
        }
    }

    pub fn take_open_branch_on_exit(&mut self) -> Option<String> {
        self.open_branch_on_exit.take()
    }

    /// Kick off initial data loads on background tasks.
    fn load_initial_data(&mut self) {
        self.spawn_fetch_branches();
        self.spawn_fetch_services();
        self.spawn_fetch_capabilities();
        self.spawn_fetch_proxy_status();
        self.load_sync_data();
    }

    /// Load data that is synchronous (config YAML, hooks) — fine to do inline.
    fn load_sync_data(&mut self) {
        // Load config
        match self.context.fetch_config_yaml() {
            Ok(yaml) => {
                let action = Action::DataLoaded(DataPayload::ConfigYaml(yaml));
                self.dispatch_action(&action);
            }
            Err(e) => {
                self.set_status(format!("Failed to load config: {}", e), true);
            }
        }

        // Load hooks
        let hooks_data = self.context.fetch_hooks();
        let action = Action::DataLoaded(DataPayload::HooksData(hooks_data));
        self.dispatch_action(&action);
    }

    // ── Background task spawners ────────────────────────────────────

    /// Spawn a background task to fetch workspaces.
    fn spawn_fetch_branches(&self) {
        let config = self.context.config.clone();
        let vcs_data = self.context.snapshot_vcs_data();
        let branch_registry = self.context.snapshot_branch_registry();
        let context_branch = self.context.snapshot_context_branch();
        let tx = self.bg_tx.clone();
        tokio::spawn(async move {
            match DevflowContext::fetch_branches_bg(
                &config,
                vcs_data,
                branch_registry,
                context_branch,
            )
            .await
            {
                Ok(data) => {
                    let _ = tx.send(Action::DataLoaded(DataPayload::Branches(data)));
                }
                Err(e) => {
                    let _ = tx.send(Action::Error(format!("Failed to load workspaces: {}", e)));
                }
            }
        });
    }

    /// Spawn a background task to fetch services.
    fn spawn_fetch_services(&self) {
        let config = self.context.config.clone();
        let tx = self.bg_tx.clone();
        tokio::spawn(async move {
            match DevflowContext::fetch_services_bg(&config).await {
                Ok(data) => {
                    let _ = tx.send(Action::DataLoaded(DataPayload::Services(data)));
                }
                Err(e) => {
                    let _ = tx.send(Action::Error(format!("Failed to load services: {}", e)));
                }
            }
        });
    }

    /// Spawn a background task to fetch capability matrix.
    fn spawn_fetch_capabilities(&self) {
        let config = self.context.config.clone();
        let tx = self.bg_tx.clone();
        tokio::spawn(async move {
            match DevflowContext::fetch_capabilities_bg(&config).await {
                Ok(data) => {
                    let _ = tx.send(Action::DataLoaded(DataPayload::Capabilities(data)));
                }
                Err(e) => {
                    let _ = tx.send(Action::Error(format!("Failed to load capabilities: {}", e)));
                }
            }
        });
    }

    /// Spawn a background task to fetch proxy status + targets.
    fn spawn_fetch_proxy_status(&self) {
        let tx = self.bg_tx.clone();
        tokio::spawn(async move {
            match DevflowContext::fetch_proxy_status_bg().await {
                Ok((status, targets)) => {
                    let _ = tx.send(Action::DataLoaded(DataPayload::ProxyStatus(status)));
                    let _ = tx.send(Action::DataLoaded(DataPayload::ProxyTargets(targets)));
                }
                Err(_) => {
                    // Proxy not running is not an error — just set a "not running" state
                    let status = super::components::proxy_tab::ProxyStatusData {
                        running: false,
                        https_port: 0,
                        http_port: 0,
                        api_port: 0,
                        ca_installed: false,
                    };
                    let _ = tx.send(Action::DataLoaded(DataPayload::ProxyStatus(status)));
                    let _ = tx.send(Action::DataLoaded(DataPayload::ProxyTargets(vec![])));
                }
            }
        });
    }

    /// Spawn a background task to fetch logs.
    fn spawn_fetch_logs(&self, service: String, workspace: String) {
        let config = self.context.config.clone();
        let tx = self.bg_tx.clone();
        tokio::spawn(async move {
            match DevflowContext::fetch_logs_bg(&config, &service, &workspace).await {
                Ok(content) => {
                    let _ = tx.send(Action::DataLoaded(DataPayload::Logs { service, content }));
                }
                Err(e) => {
                    let _ = tx.send(Action::Error(format!("Failed to fetch logs: {}", e)));
                }
            }
        });
    }

    /// Spawn a background task to run doctor checks.
    fn spawn_doctor(&self) {
        let config = self.context.config.clone();
        let tx = self.bg_tx.clone();
        tokio::spawn(async move {
            match DevflowContext::fetch_doctor_bg(&config).await {
                Ok(results) => {
                    let _ = tx.send(Action::DataLoaded(DataPayload::DoctorResults(results)));
                    let _ = tx.send(Action::OperationComplete {
                        success: true,
                        message: "Doctor checks complete".to_string(),
                    });
                }
                Err(e) => {
                    let _ = tx.send(Action::Error(format!("Doctor failed: {}", e)));
                }
            }
        });
    }

    /// Spawn a background task to align services to a workspace.
    fn spawn_switch_services(&self, workspace_name: String) {
        let config = self.context.config.clone();
        let project_dir = self
            .context
            .config_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|d| d.to_path_buf())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let tx = self.bg_tx.clone();
        tokio::spawn(async move {
            match DevflowContext::switch_services_bg(&config, &workspace_name, &project_dir).await {
                Ok(msg) => {
                    let _ = tx.send(Action::OperationComplete {
                        success: true,
                        message: msg,
                    });
                    // Trigger a full reload
                    let _ = tx.send(Action::Refresh);
                }
                Err(e) => {
                    let _ = tx.send(Action::Error(format!("Service switch failed: {}", e)));
                }
            }
        });
    }

    /// Spawn a background task for creating a workspace (service orchestration).
    fn spawn_create_workspace(&self, name: String, from: Option<String>) {
        let config = self.context.config.clone();
        let project_dir = self
            .context
            .config_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|d| d.to_path_buf())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let tx = self.bg_tx.clone();
        tokio::spawn(async move {
            match DevflowContext::create_workspace_bg(&config, &name, from.as_deref(), &project_dir)
                .await
            {
                Ok(msg) => {
                    let _ = tx.send(Action::OperationComplete {
                        success: true,
                        message: msg,
                    });
                    let _ = tx.send(Action::Refresh);
                }
                Err(e) => {
                    let _ = tx.send(Action::Error(format!("Create failed: {}", e)));
                }
            }
        });
    }

    /// Spawn a background task for deleting a workspace (service orchestration).
    /// After service workspaces are deleted, sends `DeleteVcsBranch` back to the
    /// main thread so VCS deletion can happen synchronously.
    fn spawn_delete_workspace(&self, name: String) {
        let config = self.context.config.clone();
        let project_dir = self
            .context
            .config_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|d| d.to_path_buf())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let tx = self.bg_tx.clone();
        tokio::spawn(async move {
            match DevflowContext::delete_workspace_bg(&config, &name, &project_dir).await {
                Ok(msg) => {
                    let _ = tx.send(Action::OperationComplete {
                        success: true,
                        message: msg,
                    });
                    // Ask main thread to delete the VCS workspace
                    let _ = tx.send(Action::DeleteVcsBranch(name));
                }
                Err(e) => {
                    let _ = tx.send(Action::Error(format!("Delete failed: {}", e)));
                }
            }
        });
    }

    /// Spawn a background task for a service operation (start/stop/reset/delete).
    fn spawn_service_op(&self, service: String, workspace: String, op: ServiceOp) {
        let config = self.context.config.clone();
        let tx = self.bg_tx.clone();
        tokio::spawn(async move {
            let result = match op {
                ServiceOp::Start => {
                    DevflowContext::start_service_bg(&config, &service, &workspace).await
                }
                ServiceOp::Stop => {
                    DevflowContext::stop_service_bg(&config, &service, &workspace).await
                }
                ServiceOp::Reset => {
                    DevflowContext::reset_service_bg(&config, &service, &workspace).await
                }
            };
            match result {
                Ok(msg) => {
                    let _ = tx.send(Action::OperationComplete {
                        success: true,
                        message: msg,
                    });
                    // Reload services after any service operation
                    if let Ok(data) = DevflowContext::fetch_services_bg(&config).await {
                        let _ = tx.send(Action::DataLoaded(DataPayload::Services(data)));
                    }
                }
                Err(e) => {
                    let _ = tx.send(Action::Error(format!("{:?} failed: {}", op, e)));
                }
            }
        });
    }

    // ── Main event loop ─────────────────────────────────────────────

    /// Main event loop.
    pub async fn run(
        &mut self,
        terminal: &mut ratatui::Terminal<impl ratatui::backend::Backend>,
    ) -> Result<()> {
        let mut events = EventHandler::new(Duration::from_millis(250));

        self.load_initial_data();

        while self.running {
            // Draw
            terminal.draw(|frame| self.render(frame))?;

            // Wait for either a terminal event or a background task result
            tokio::select! {
                event = events.next() => {
                    let event = event?;
                    let action = self.handle_event(event);
                    self.process_action(action);
                }
                bg_action = self.bg_rx.recv() => {
                    if let Some(action) = bg_action {
                        self.process_action(action);
                    }
                }
            }
        }

        Ok(())
    }

    /// Handle a terminal event and return an action.
    fn handle_event(&mut self, event: AppEvent) -> Action {
        match event {
            AppEvent::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    return Action::None;
                }
                self.handle_key_event(key)
            }
            AppEvent::Mouse(_event) => Action::None,
            AppEvent::Resize(_cols, _rows) => Action::None,
            AppEvent::Tick => {
                // Advance spinner
                self.spinner_tick = (self.spinner_tick + 1) % theme::SPINNER_FRAMES.len();

                // Clear old status messages (errors last 10s, success 5s)
                if let Some((_, is_error, when)) = &self.status_message {
                    let timeout = if *is_error {
                        Duration::from_secs(10)
                    } else {
                        Duration::from_secs(5)
                    };
                    if when.elapsed() > timeout {
                        self.status_message = None;
                    }
                }
                Action::None
            }
        }
    }

    /// Handle key events, considering modal state.
    fn handle_key_event(&mut self, key: KeyEvent) -> Action {
        // Modal takes priority — use &mut self.modal for modals that need mutation
        match &mut self.modal {
            ModalState::Help => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
                        self.modal = ModalState::None;
                    }
                    _ => {}
                }
                return Action::None;
            }
            ModalState::Confirm { .. } => {
                return match key.code {
                    KeyCode::Char('y') | KeyCode::Enter => Action::ConfirmYes,
                    KeyCode::Char('n') | KeyCode::Esc => Action::ConfirmNo,
                    _ => Action::None,
                };
            }
            ModalState::Input { ref mut input, .. } => {
                return match key.code {
                    KeyCode::Enter => {
                        let text = input.clone();
                        Action::SubmitInput(text)
                    }
                    KeyCode::Esc => Action::CancelInput,
                    KeyCode::Backspace => {
                        input.pop();
                        Action::None
                    }
                    KeyCode::Char(c) => {
                        input.push(c);
                        Action::None
                    }
                    _ => Action::None,
                };
            }
            ModalState::Select {
                ref options,
                ref mut selected,
                ..
            } => {
                let count = options.len();
                return match key.code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        *selected = (*selected + 1) % count;
                        Action::None
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        *selected = (*selected + count - 1) % count;
                        Action::None
                    }
                    KeyCode::Enter => Action::SelectOption(*selected),
                    KeyCode::Esc => Action::CancelSelect,
                    _ => Action::None,
                };
            }
            ModalState::None => {}
        }

        // Global keybindings (3 tabs only)
        match key.code {
            KeyCode::Char('q') | KeyCode::Char('c')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                return Action::Quit;
            }
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Char('?') => return Action::ToggleHelp,
            KeyCode::Char(']') => return Action::NextTab,
            KeyCode::Char('[') => return Action::PrevTab,
            KeyCode::Tab => return Action::NextTab,
            KeyCode::BackTab => return Action::PrevTab,
            _ => {}
        }

        // Tab selection: number keys switch top-level views, except inside
        // the System tab where 1-4 are reserved for sub-sections.
        if self.active_tab != 3 {
            match key.code {
                KeyCode::Char('1') => return Action::SelectTab(0),
                KeyCode::Char('2') => return Action::SelectTab(1),
                KeyCode::Char('3') => return Action::SelectTab(2),
                KeyCode::Char('4') => return Action::SelectTab(3),
                KeyCode::Char('5') => return Action::SelectTab(4),
                _ => {}
            }
        }

        // Backward-compatible fallback for terminals where function keys are convenient.
        match key.code {
            KeyCode::F(1) => return Action::SelectTab(0),
            KeyCode::F(2) => return Action::SelectTab(1),
            KeyCode::F(3) => return Action::SelectTab(2),
            KeyCode::F(4) => return Action::SelectTab(3),
            KeyCode::F(5) => return Action::SelectTab(4),
            _ => {}
        }

        // Delegate to active component
        match self.active_tab {
            0 => self.workspaces.handle_key_event(key),
            1 => self.services_tab.handle_key_event(key),
            2 => self.proxy_tab.handle_key_event(key),
            3 => self.system.handle_key_event(key),
            4 => self.logs.handle_key_event(key),
            _ => Action::None,
        }
    }

    /// Switch active tab, calling on_blur/on_focus lifecycle methods.
    fn switch_tab(&mut self, new_tab: usize) {
        if new_tab == self.active_tab || new_tab >= self.tab_names.len() {
            return;
        }
        // Blur old tab
        match self.active_tab {
            0 => self.workspaces.on_blur(),
            1 => self.services_tab.on_blur(),
            2 => self.proxy_tab.on_blur(),
            3 => self.system.on_blur(),
            4 => self.logs.on_blur(),
            _ => {}
        }
        self.active_tab = new_tab;
        // Focus new tab
        match self.active_tab {
            0 => self.workspaces.on_focus(),
            1 => self.services_tab.on_focus(),
            2 => self.proxy_tab.on_focus(),
            3 => self.system.on_focus(),
            4 => self.logs.on_focus(),
            _ => {}
        }
    }

    /// Process an action — fully synchronous. Async work is spawned to background.
    fn process_action(&mut self, action: Action) {
        match action {
            Action::Quit => {
                self.running = false;
            }
            Action::NextTab => {
                let next = (self.active_tab + 1) % self.tab_names.len();
                self.switch_tab(next);
            }
            Action::PrevTab => {
                let prev = (self.active_tab + self.tab_names.len() - 1) % self.tab_names.len();
                self.switch_tab(prev);
            }
            Action::SelectTab(idx) => {
                self.switch_tab(idx);
            }
            Action::ToggleHelp => {
                self.modal = match self.modal {
                    ModalState::Help => ModalState::None,
                    _ => ModalState::Help,
                };
            }
            Action::Refresh => {
                self.set_status("Refreshing...".to_string(), false);
                // Re-snapshot VCS data (sync, fast) and spawn async fetches
                self.context.refresh_vcs_snapshot();
                self.load_initial_data();
            }
            Action::SwitchServices(ref name) => {
                if self.context.service_configs().is_empty() {
                    self.set_status(
                        "No services configured. Press 'o' to open the workspace/worktree."
                            .to_string(),
                        true,
                    );
                    return;
                }

                self.set_status(format!("Aligning services to '{}'...", name), false);
                self.spawn_switch_services(name.clone());
            }
            Action::OpenBranchAndExit(ref name) => {
                self.open_branch_on_exit = Some(name.clone());
                self.running = false;
            }
            Action::CreateBranch { ref name, ref from } => {
                self.set_status(format!("Creating workspace '{}'...", name), false);
                // VCS create + checkout is fast + local
                if let Err(e) = self
                    .context
                    .create_and_checkout_workspace(name, from.as_deref())
                {
                    self.set_status(format!("Create failed: {}", e), true);
                    return;
                }
                // Spawn async service orchestration
                self.spawn_create_workspace(name.clone(), from.clone());
            }
            Action::DeleteBranch(ref name) => {
                self.set_status(format!("Deleting workspace '{}'...", name), false);
                // Spawn async service delete; VCS delete happens when DeleteVcsBranch comes back
                self.spawn_delete_workspace(name.clone());
            }
            Action::DeleteVcsBranch(ref name) => {
                // Sync VCS workspace deletion on main thread, after services are cleaned up
                if let Err(e) = self.context.delete_vcs_branch(name) {
                    self.set_status(format!("VCS workspace delete failed: {}", e), true);
                } else {
                    // Fire post-remove hooks in background
                    let config = self.context.config.clone();
                    let project_dir = self
                        .context
                        .config_path
                        .as_ref()
                        .and_then(|p| p.parent())
                        .map(|d| d.to_path_buf())
                        .or_else(|| std::env::current_dir().ok())
                        .unwrap_or_else(|| std::path::PathBuf::from("."));
                    let ws_name = name.clone();
                    tokio::spawn(async move {
                        let hook_opts = devflow_core::workspace::LifecycleOptions::default();
                        devflow_core::workspace::hooks::run_lifecycle_hooks_best_effort(
                            &config,
                            &project_dir,
                            &ws_name,
                            devflow_core::hooks::HookPhase::PostRemove,
                            &hook_opts,
                        )
                        .await;
                    });
                    // Refresh everything after workspace deletion
                    self.context.refresh_vcs_snapshot();
                    self.load_initial_data();
                }
            }
            Action::MergeWorkspace {
                ref source,
                ref target,
            } => {
                self.set_status(
                    format!("Merging '{}' into '{}'...", source, target),
                    false,
                );
                let source = source.clone();
                let target = target.clone();
                let config = self.context.config.clone();
                let project_dir = self
                    .context
                    .config_path
                    .as_ref()
                    .and_then(|p| p.parent())
                    .map(|d| d.to_path_buf())
                    .or_else(|| std::env::current_dir().ok())
                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                let bg_tx = self.bg_tx.clone();
                tokio::spawn(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        let vcs = devflow_core::vcs::detect_vcs_provider(&project_dir)?;

                        // Run readiness checks first
                        if let Some(ref merge_config) = config.merge {
                            let checks =
                                devflow_core::merge::build_checks_from_config(merge_config);
                            if !checks.is_empty() {
                                let report = devflow_core::merge::run_checks(
                                    &checks,
                                    vcs.as_ref(),
                                    &source,
                                    &target,
                                );
                                if !report.ready {
                                    return Ok(Action::MergeChecksComplete(report));
                                }
                            }
                        }

                        // Checkout target, merge source
                        let merge_dir = vcs
                            .worktree_path(&target)?
                            .unwrap_or_else(|| project_dir.clone());
                        let merge_vcs = devflow_core::vcs::detect_vcs_provider(&merge_dir)?;
                        if merge_dir == project_dir {
                            merge_vcs.checkout_workspace(&target)?;
                        }
                        merge_vcs.merge_branch(&source)?;
                        Ok::<Action, anyhow::Error>(Action::OperationComplete {
                            success: true,
                            message: format!("Merged '{}' into '{}'", source, target),
                        })
                    })
                    .await;

                    let action = match result {
                        Ok(Ok(action)) => action,
                        Ok(Err(e)) => Action::OperationComplete {
                            success: false,
                            message: format!("Merge failed: {}", e),
                        },
                        Err(e) => Action::Error(format!("Merge task panicked: {}", e)),
                    };
                    let _ = bg_tx.send(action);
                });
            }
            Action::RebaseWorkspace {
                ref source,
                ref target,
            } => {
                self.set_status(
                    format!("Rebasing '{}' onto '{}'...", source, target),
                    false,
                );
                let source = source.clone();
                let target = target.clone();
                let project_dir = self
                    .context
                    .config_path
                    .as_ref()
                    .and_then(|p| p.parent())
                    .map(|d| d.to_path_buf())
                    .or_else(|| std::env::current_dir().ok())
                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                let bg_tx = self.bg_tx.clone();
                let source_display = source.clone();
                let target_display = target.clone();
                tokio::spawn(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        let vcs = devflow_core::vcs::detect_vcs_provider(&project_dir)?;
                        vcs.checkout_workspace(&source)?;
                        let rebase_result = vcs.rebase(&target)?;
                        Ok::<devflow_core::merge::RebaseResult, anyhow::Error>(rebase_result)
                    })
                    .await;

                    let action = match result {
                        Ok(Ok(r)) => {
                            if r.success {
                                Action::OperationComplete {
                                    success: true,
                                    message: format!(
                                        "Rebased '{}' onto '{}' ({} commits)",
                                        source_display, target_display, r.commits_replayed
                                    ),
                                }
                            } else {
                                Action::OperationComplete {
                                    success: false,
                                    message: format!(
                                        "Rebase conflicts in: {}",
                                        r.conflict_files.join(", ")
                                    ),
                                }
                            }
                        }
                        Ok(Err(e)) => Action::OperationComplete {
                            success: false,
                            message: format!("Rebase failed: {}", e),
                        },
                        Err(e) => Action::Error(format!("Rebase task panicked: {}", e)),
                    };
                    let _ = bg_tx.send(action);
                });
            }
            Action::MergeChecksComplete(ref report) => {
                let msg = if report.ready {
                    "Merge readiness: READY".to_string()
                } else {
                    let failures: Vec<&str> = report
                        .checks
                        .iter()
                        .filter(|c| !c.passed)
                        .map(|c| c.message.as_str())
                        .collect();
                    format!("Merge blocked: {}", failures.join("; "))
                };
                self.set_status(msg, !report.ready);
            }
            Action::RebaseComplete(ref _result) => {
                // Handled by OperationComplete already
            }
            Action::TrainAdd { .. }
            | Action::TrainRun { .. }
            | Action::TrainStatus { .. }
            | Action::MergeTrainProgress(_) => {
                // Merge train TUI tab actions — placeholder for future tab
            }
            Action::StartService {
                ref service,
                ref workspace,
            } => {
                self.set_status(format!("Starting {} on '{}'...", service, workspace), false);
                self.spawn_service_op(service.clone(), workspace.clone(), ServiceOp::Start);
            }
            Action::StopService {
                ref service,
                ref workspace,
            } => {
                self.set_status(format!("Stopping {} on '{}'...", service, workspace), false);
                self.spawn_service_op(service.clone(), workspace.clone(), ServiceOp::Stop);
            }
            Action::ResetService {
                ref service,
                ref workspace,
            } => {
                self.set_status(
                    format!("Resetting {} on '{}'...", service, workspace),
                    false,
                );
                self.spawn_service_op(service.clone(), workspace.clone(), ServiceOp::Reset);
            }
            Action::ViewLogs {
                ref service,
                ref workspace,
            } => {
                self.logs.set_loading(service, workspace);
                self.switch_tab(4); // Switch to logs tab
                self.spawn_fetch_logs(service.clone(), workspace.clone());
            }
            Action::RunDoctor => {
                self.set_status("Running doctor checks...".to_string(), false);
                self.spawn_doctor();
            }
            Action::ShowConfirm {
                title,
                message,
                on_confirm,
            } => {
                self.modal = ModalState::Confirm {
                    title,
                    message,
                    on_confirm,
                };
            }
            Action::ConfirmYes => {
                if let ModalState::Confirm { on_confirm, .. } =
                    std::mem::replace(&mut self.modal, ModalState::None)
                {
                    let action = *on_confirm;
                    self.process_action(action);
                }
            }
            Action::ConfirmNo => {
                self.modal = ModalState::None;
            }
            Action::ShowInput { title, on_submit } => {
                self.modal = ModalState::Input {
                    title,
                    input: String::new(),
                    target: on_submit,
                };
            }
            Action::SubmitInput(text) => {
                if let ModalState::Input { target, .. } =
                    std::mem::replace(&mut self.modal, ModalState::None)
                {
                    match target {
                        InputTarget::CreateBranch { from } => {
                            if !text.is_empty() {
                                let action = Action::CreateBranch { name: text, from };
                                self.process_action(action);
                            }
                        }
                        InputTarget::FilterBranches => {
                            self.workspaces.set_filter(text);
                        }
                        InputTarget::FilterLogsPicker => {
                            self.logs.set_filter(text);
                        }
                        InputTarget::AddServiceName { service_type } => {
                            if !text.is_empty() {
                                self.process_action(Action::AddServiceConfig {
                                    service_type,
                                    name: text,
                                });
                            }
                        }
                    }
                }
            }
            Action::CancelInput => {
                self.modal = ModalState::None;
            }
            Action::ShowSelect {
                title,
                options,
                on_select,
            } => {
                self.modal = ModalState::Select {
                    title,
                    options,
                    selected: 0,
                    on_select,
                };
            }
            Action::SelectOption(idx) => {
                if let ModalState::Select {
                    options, on_select, ..
                } = std::mem::replace(&mut self.modal, ModalState::None)
                {
                    if let Some(selected_value) = options.get(idx) {
                        match on_select {
                            SelectTarget::AddServiceType => {
                                // Extract service type from label (e.g., "postgres" from "postgres — PostgreSQL database")
                                let service_type = selected_value
                                    .split_whitespace()
                                    .next()
                                    .unwrap_or("postgres")
                                    .to_string();
                                // Show name input next
                                self.process_action(Action::ShowInput {
                                    title: format!("Service name (type: {})", service_type),
                                    on_submit: InputTarget::AddServiceName { service_type },
                                });
                            }
                        }
                    }
                }
            }
            Action::CancelSelect => {
                self.modal = ModalState::None;
            }
            Action::AddServiceConfig {
                ref service_type,
                ref name,
            } => {
                self.set_status(format!("Adding service '{}'...", name), false);
                let config = self.context.config.clone();
                let config_path = self
                    .context
                    .config_path
                    .clone()
                    .unwrap_or_else(|| std::env::current_dir().unwrap().join(".devflow.yml"));
                let svc_type = service_type.clone();
                let svc_name = name.clone();
                let tx = self.bg_tx.clone();
                tokio::spawn(async move {
                    let result: anyhow::Result<()> = async {
                        let is_local = true; // TUI always creates local services
                        let named_cfg = devflow_core::config::NamedServiceConfig {
                            name: svc_name.clone(),
                            provider_type: "local".to_string(),
                            service_type: svc_type.clone(),
                            auto_workspace: devflow_core::config::default_auto_branch(),
                            default: false,
                            local: if is_local {
                                Some(devflow_core::config::LocalServiceConfig {
                                    image: None,
                                    data_root: None,
                                    storage: None,
                                    port_range_start: None,
                                    postgres_user: None,
                                    postgres_password: None,
                                    postgres_db: None,
                                })
                            } else {
                                None
                            },
                            neon: None,
                            dblab: None,
                            xata: None,
                            clickhouse: if svc_type == "clickhouse" {
                                Some(devflow_core::config::ClickHouseConfig {
                                    image: "clickhouse/clickhouse-server:latest".to_string(),
                                    port_range_start: None,
                                    data_root: None,
                                    user: "default".to_string(),
                                    password: None,
                                })
                            } else {
                                None
                            },
                            mysql: if svc_type == "mysql" {
                                Some(devflow_core::config::MySQLConfig {
                                    image: "mysql:8".to_string(),
                                    port_range_start: None,
                                    data_root: None,
                                    root_password: "dev".to_string(),
                                    database: None,
                                    user: None,
                                    password: None,
                                })
                            } else {
                                None
                            },
                            generic: None,
                            plugin: None,
                            docker: None,
                        };

                        let mut state = devflow_core::state::LocalStateManager::new()?;
                        state.add_service(&config_path, named_cfg.clone(), false)?;

                        // Create main workspace for local providers
                        if is_local {
                            let mut config_with_service = config.clone();
                            if let Some(state_services) = state.get_services(&config_path) {
                                config_with_service.services = Some(state_services);
                            }
                            match devflow_core::services::factory::create_provider_from_named_config(
                                &config_with_service,
                                &named_cfg,
                            )
                            .await
                            {
                                Ok(be) => {
                                    if let Err(e) = be.create_workspace("main", None).await {
                                        log::warn!("Could not create main workspace: {}", e);
                                    }
                                }
                                Err(e) => {
                                    log::warn!("Could not initialize service: {}", e);
                                }
                            }
                        }

                        Ok(())
                    }
                    .await;

                    match result {
                        Ok(()) => {
                            let _ = tx.send(Action::OperationComplete {
                                success: true,
                                message: format!("Service '{}' added", svc_name),
                            });
                            let _ = tx.send(Action::Refresh);
                        }
                        Err(e) => {
                            let _ = tx.send(Action::Error(format!("Failed to add service: {}", e)));
                        }
                    }
                });
            }
            Action::RemoveServiceConfig(ref name) => {
                let config_path = self
                    .context
                    .config_path
                    .clone()
                    .unwrap_or_else(|| std::env::current_dir().unwrap().join(".devflow.yml"));
                match devflow_core::state::LocalStateManager::new() {
                    Ok(mut state) => match state.remove_service(&config_path, name) {
                        Ok(()) => {
                            self.set_status(format!("Removed service '{}'", name), false);
                            self.context.refresh_vcs_snapshot();
                            self.load_initial_data();
                        }
                        Err(e) => {
                            self.set_status(format!("Failed to remove service: {}", e), true);
                        }
                    },
                    Err(e) => {
                        self.set_status(format!("Failed to load state: {}", e), true);
                    }
                }
            }
            Action::DataLoaded(ref _payload) => {
                self.dispatch_action(&action);
            }
            Action::OperationComplete {
                success,
                ref message,
            } => {
                self.set_status(message.clone(), !success);
            }
            Action::Error(ref msg) => {
                self.set_status(msg.clone(), true);
            }
            Action::StartAllServices(ref workspace) => {
                let services = self.workspaces.services_for_branch(workspace);
                if services.is_empty() {
                    self.set_status(
                        format!("No services to start on workspace '{}'", workspace),
                        true,
                    );
                } else {
                    self.set_status(
                        format!(
                            "Starting {} service(s) on '{}'...",
                            services.len(),
                            workspace
                        ),
                        false,
                    );
                    for service in services {
                        self.spawn_service_op(service, workspace.clone(), ServiceOp::Start);
                    }
                }
            }
            Action::StopAllServices(ref workspace) => {
                let services = self.workspaces.services_for_branch(workspace);
                if services.is_empty() {
                    self.set_status(
                        format!("No services to stop on workspace '{}'", workspace),
                        true,
                    );
                } else {
                    self.set_status(
                        format!(
                            "Stopping {} service(s) on '{}'...",
                            services.len(),
                            workspace
                        ),
                        false,
                    );
                    for service in services {
                        self.spawn_service_op(service, workspace.clone(), ServiceOp::Stop);
                    }
                }
            }
            // Proxy actions — spawn background fetch from proxy API
            Action::StartProxy => {
                self.set_status("Starting proxy...".to_string(), false);
                let tx = self.bg_tx.clone();
                tokio::spawn(async move {
                    match DevflowContext::start_proxy_bg().await {
                        Ok(msg) => {
                            let _ = tx.send(Action::OperationComplete {
                                success: true,
                                message: msg,
                            });
                            let _ = tx.send(Action::Refresh);
                        }
                        Err(e) => {
                            let _ = tx.send(Action::Error(format!("Proxy start failed: {}", e)));
                        }
                    }
                });
            }
            Action::StopProxy => {
                self.set_status("Stopping proxy...".to_string(), false);
                let tx = self.bg_tx.clone();
                tokio::spawn(async move {
                    match DevflowContext::stop_proxy_bg().await {
                        Ok(msg) => {
                            let _ = tx.send(Action::OperationComplete {
                                success: true,
                                message: msg,
                            });
                            let _ = tx.send(Action::Refresh);
                        }
                        Err(e) => {
                            let _ = tx.send(Action::Error(format!("Proxy stop failed: {}", e)));
                        }
                    }
                });
            }
            Action::InstallAgentSkills => {
                self.set_status("Installing agent skills...".to_string(), false);
                let config = self.context.config.clone();
                let project_dir = self
                    .context
                    .config_path
                    .as_ref()
                    .and_then(|p| p.parent())
                    .map(|d| d.to_path_buf())
                    .or_else(|| std::env::current_dir().ok())
                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                let tx = self.bg_tx.clone();
                tokio::spawn(async move {
                    match devflow_core::agent::install_agent_skills(&config, &project_dir) {
                        Ok(paths) => {
                            let _ = tx.send(Action::OperationComplete {
                                success: true,
                                message: format!("Installed {} agent skill files", paths.len()),
                            });
                            let _ = tx.send(Action::RunDoctor);
                        }
                        Err(e) => {
                            let _ = tx.send(Action::Error(format!(
                                "Failed to install agent skills: {}",
                                e
                            )));
                        }
                    }
                });
            }
            Action::None => {}
        }
    }

    /// Dispatch an action to all components.
    fn dispatch_action(&mut self, action: &Action) {
        self.workspaces.update(action);
        self.services_tab.update(action);
        self.proxy_tab.update(action);
        self.system.update(action);
        self.logs.update(action);
    }

    fn set_status(&mut self, message: String, is_error: bool) {
        self.status_message = Some((message, is_error, Instant::now()));
    }

    /// Get the current spinner frame character.
    fn spinner_frame(&self) -> &'static str {
        theme::SPINNER_FRAMES[self.spinner_tick % theme::SPINNER_FRAMES.len()]
    }

    /// Render the full TUI.
    fn render(&self, frame: &mut Frame) {
        let size = frame.area();

        // Main layout: header + content + footer
        let main_layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Tab bar
                Constraint::Min(5),    // Content
                Constraint::Length(1), // Status bar
            ])
            .split(size);

        self.render_tab_bar(frame, main_layout[0]);
        self.render_content(frame, main_layout[1]);
        self.render_status(frame, main_layout[2]);

        // Render modal overlays
        match &self.modal {
            ModalState::Help => {
                help::render_help(frame);
            }
            ModalState::Confirm { title, message, .. } => {
                help::render_confirm(frame, title, message);
            }
            ModalState::Input { title, input, .. } => {
                help::render_input(frame, title, input);
            }
            ModalState::Select {
                title,
                options,
                selected,
                ..
            } => {
                help::render_select(frame, title, options, *selected);
            }
            ModalState::None => {}
        }
    }

    fn render_tab_bar(&self, frame: &mut Frame, area: Rect) {
        let titles: Vec<Line> = self
            .tab_names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let style = if i == self.active_tab {
                    Style::default().fg(theme::TAB_ACTIVE).bold()
                } else {
                    Style::default().fg(theme::TAB_INACTIVE)
                };
                Line::styled(format!(" {} {} ", i + 1, name), style)
            })
            .collect();

        let tabs = Tabs::new(titles)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::BORDER_INACTIVE))
                    .title(Span::styled(
                        " devflow ",
                        Style::default().fg(theme::TAB_TITLE).bold(),
                    )),
            )
            .select(self.active_tab)
            .highlight_style(Style::default().fg(theme::TAB_ACTIVE).bold())
            .divider(Span::styled(
                " | ",
                Style::default().fg(theme::TAB_INACTIVE),
            ));

        frame.render_widget(tabs, area);
    }

    fn render_content(&self, frame: &mut Frame, area: Rect) {
        match self.active_tab {
            0 => self.workspaces.render(frame, area, self.spinner_frame()),
            1 => self.services_tab.render(frame, area, self.spinner_frame()),
            2 => self.proxy_tab.render(frame, area, self.spinner_frame()),
            3 => self.system.render(frame, area, self.spinner_frame()),
            4 => self.logs.render(frame, area, self.spinner_frame()),
            _ => {}
        }
    }

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        // Build the status line: left = hints, right = status message (or nothing)
        let tab_hints = theme::tab_hints(self.active_tab);
        let global_hints = "q:Quit  ?:Help  Tab/Shift+Tab:Views  1-5:View";

        match &self.status_message {
            Some((msg, is_error, _)) => {
                // Show status message with visual prominence
                help::render_status_bar(frame, area, msg, *is_error, tab_hints);
            }
            None => {
                // Default: show global + tab-specific hints
                let line = Line::from(vec![
                    Span::styled(
                        format!(" {} ", global_hints),
                        Style::default()
                            .fg(theme::STATUS_HINT_FG)
                            .bg(theme::STATUS_BAR_BG),
                    ),
                    Span::styled(
                        " | ",
                        Style::default()
                            .fg(theme::TEXT_MUTED)
                            .bg(theme::STATUS_BAR_BG),
                    ),
                    Span::styled(
                        format!("{} ", tab_hints),
                        Style::default()
                            .fg(theme::KEY_HINT)
                            .bg(theme::STATUS_BAR_BG),
                    ),
                ]);
                let paragraph =
                    Paragraph::new(line).style(Style::default().bg(theme::STATUS_BAR_BG));
                frame.render_widget(paragraph, area);
            }
        }
    }
}

/// Types of service operations for background dispatch.
#[derive(Debug, Clone, Copy)]
enum ServiceOp {
    Start,
    Stop,
    Reset,
}

impl std::fmt::Display for ServiceOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServiceOp::Start => write!(f, "Start"),
            ServiceOp::Stop => write!(f, "Stop"),
            ServiceOp::Reset => write!(f, "Reset"),
        }
    }
}
