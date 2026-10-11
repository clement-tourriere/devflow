//! The command palette (⌘K): jump to a project or a tab, or run an action.
//!
//! It is hosted in a dialog, which is outside the shell's view tree, so a
//! choice is applied from `on_confirm` straight on the app state rather than
//! by dispatching an action from inside the dialog.
use super::{db_dialogs, doctor, processes, service_dialogs, ui};
use crate::{
    actions::{NewWorkspace, OpenPalette, OpenSettings, Refresh, ToggleTheme, tab_action},
    model::{Page, ProcessTarget, Tab, shorten_home},
    redaction::shown,
    services::ServiceTarget,
    state::AppState,
};
use gpui_kit::{
    assets::IconName as Lucide,
    component::{
        ActiveTheme as _, Disableable as _, Icon, IndexPath, WindowExt as _,
        command::{Command, CommandGroup, CommandItem, CommandState},
        h_flex,
        kbd::Kbd,
        v_flex,
    },
    prelude::*,
    *,
};
use std::rc::Rc;
use uuid::Uuid;

/// What choosing an entry does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaletteAction {
    Project(Uuid),
    Tab(Tab),
    NewWorkspace,
    Refresh,
    Settings,
    ToggleTheme,
    AddProject,
    /// The setup wizard for the selected project.
    SetUp,
    /// Start the daemons of the workspace the entry was listed for (after the
    /// review when its files are not approved). The target is captured when
    /// the palette opens: choosing an entry never reads the selection.
    StartProcesses {
        target: ProcessTarget,
        review: bool,
    },
    StopProcesses(ProcessTarget),
    RestartProcesses(ProcessTarget),
    /// Open a daemon's proxy URL in the browser.
    OpenUrl(String),
    /// Snapshot the data of the service the Data tab shows.
    SnapshotData,
    /// Reset a workspace's data of that service.
    ResetData,
    /// Show how to install pitchfork (the command and the button are on the
    /// Daemons tab: installing is a click there).
    InstallPitchfork,
    /// The doctor's checks for the selected workspace.
    RunDoctor,
}

impl PaletteAction {
    pub(super) fn run(&self, state: &Entity<AppState>, window: &mut Window, cx: &mut App) {
        match self {
            Self::Project(id) => state.update(cx, |s, cx| s.reveal_project(*id, cx)),
            Self::Tab(tab) => state.update(cx, |s, cx| s.go_to_tab(*tab, cx)),
            Self::NewWorkspace => state.update(cx, |s, cx| s.request_new_workspace(None, cx)),
            Self::Refresh => state.update(cx, |s, cx| s.refresh_all(cx)),
            Self::Settings => state.update(cx, |s, cx| s.set_page(Page::Settings, cx)),
            Self::ToggleTheme => state.update(cx, |s, cx| s.toggle_theme(window, cx)),
            Self::AddProject => state.update(cx, |s, cx| s.add_project(window, cx)),
            Self::SetUp => state.update(cx, |s, cx| s.request_setup_wizard(cx)),
            Self::StartProcesses { target, review } => {
                processes::start(state, target.clone(), *review, window, cx)
            }
            Self::StopProcesses(target) => processes::stop(state, target.clone(), window, cx),
            Self::RestartProcesses(target) => processes::restart(state, target.clone(), window, cx),
            Self::OpenUrl(url) => cx.open_url(url),
            Self::SnapshotData => match data_target(state.read(cx)) {
                Some((_, true)) => db_dialogs::open_snapshot(state, window, cx),
                Some((key, false)) => {
                    // Captured now: the dialog is for the project open at this click.
                    if let Some(project) = state.read(cx).selected_project_ref() {
                        let target = ServiceTarget { project, key };
                        service_dialogs::open_snapshot(state, target, None, window, cx)
                    }
                }
                None => {}
            },
            Self::ResetData => match data_target(state.read(cx)) {
                Some((_, true)) => db_dialogs::open_reset(state, window, cx),
                Some((key, false)) => state.update(cx, |s, cx| {
                    // A container branch is reset from its row, which knows
                    // the workspace; go there.
                    s.select_data_service(key, cx);
                    s.go_to_tab(Tab::Data, cx);
                }),
                None => {}
            },
            Self::InstallPitchfork => state.update(cx, |s, cx| s.go_to_tab(Tab::Daemons, cx)),
            Self::RunDoctor => {
                let path = state
                    .read(cx)
                    .selection
                    .current()
                    .and_then(|m| m.selected_target())
                    .map(|t| t.path);
                if let Some(path) = path {
                    doctor::open(state, path, window, cx);
                }
            }
        }
    }
}

/// The data service the Data tab shows, and whether it is PostgreSQL, when
/// some workspace has data of its own to act on.
fn data_target(state: &AppState) -> Option<(String, bool)> {
    let model = state.selection.current()?;
    let has_data = model.workspaces.iter().any(|w| {
        w.receipt.as_ref().is_some_and(|r| {
            r.services
                .values()
                .any(|b| b.branch_id.is_some() && !b.orphaned)
        })
    });
    if !has_data {
        return None;
    }
    let key = state.selected_data_service()?;
    let postgres = super::data_cards(model)
        .iter()
        .any(|c| c.key == key && c.postgres);
    Some((key, postgres))
}

/// The process actions the palette lists for the loaded project, with the
/// workspace each one was captured for.
#[cfg(test)]
pub(super) fn process_actions(state: &AppState) -> Vec<PaletteAction> {
    dynamic_entries(state)
        .into_iter()
        .map(|entry| entry.action)
        .filter(|action| {
            matches!(
                action,
                PaletteAction::StartProcesses { .. }
                    | PaletteAction::StopProcesses(_)
                    | PaletteAction::RestartProcesses(_)
            )
        })
        .collect()
}

/// One row of the palette.
struct Entry {
    label: SharedString,
    /// Muted text after the label.
    detail: Option<SharedString>,
    keywords: Vec<SharedString>,
    icon: Lucide,
    /// The action whose shortcut is shown on the right, looked up from the
    /// key bindings.
    shortcut: Option<Rc<dyn Action>>,
    /// Marks what is current (the selected project, the open tab).
    current: bool,
    disabled: bool,
    action: PaletteAction,
}

struct Group {
    title: &'static str,
    entries: Vec<Entry>,
}

/// The palette's content for the current state.
fn groups(state: &AppState) -> Vec<Group> {
    let selected = state.selection.current().map(|m| m.key.project);
    let has_project = selected.is_some();
    let on_project_page = state.page == Page::Project;

    let projects = Group {
        title: "Projects",
        entries: state
            .entries
            .iter()
            .map(|entry| Entry {
                label: entry.name.clone().into(),
                detail: Some(shorten_home(&entry.root).into()),
                keywords: vec![entry.root.display().to_string().into()],
                icon: Lucide::FolderGit2,
                shortcut: None,
                current: Some(entry.id) == selected,
                disabled: false,
                action: PaletteAction::Project(entry.id),
            })
            .collect(),
    };
    let tabs = Group {
        title: "Tabs",
        entries: Tab::ALL
            .iter()
            .map(|&tab| Entry {
                label: tab.title().into(),
                detail: None,
                keywords: vec!["tab".into(), "go to".into()],
                icon: tab_icon(tab),
                shortcut: Some(Rc::from(tab_action(tab))),
                current: on_project_page && state.tab == tab,
                disabled: !has_project,
                action: PaletteAction::Tab(tab),
            })
            .collect(),
    };
    let actions = Group {
        title: "Actions",
        entries: vec![
            Entry {
                label: "New workspace".into(),
                detail: None,
                keywords: vec!["create".into(), "worktree".into(), "branch".into()],
                icon: Lucide::GitBranch,
                shortcut: Some(Rc::new(NewWorkspace)),
                current: false,
                disabled: !has_project,
                action: PaletteAction::NewWorkspace,
            },
            Entry {
                label: "Set up devflow…".into(),
                detail: Some("detect, propose, review, apply".into()),
                keywords: vec![
                    "init".into(),
                    "onboarding".into(),
                    "wizard".into(),
                    "hooks".into(),
                    "configure".into(),
                ],
                icon: Lucide::WandSparkles,
                shortcut: None,
                current: false,
                disabled: !has_project,
                action: PaletteAction::SetUp,
            },
            // ---- generated from what core says about the selected workspace ----
            Entry {
                label: "Refresh".into(),
                detail: None,
                keywords: vec!["reload".into(), "update".into()],
                icon: Lucide::RefreshCw,
                shortcut: Some(Rc::new(Refresh)),
                current: false,
                disabled: !has_project,
                action: PaletteAction::Refresh,
            },
            Entry {
                label: "Settings".into(),
                detail: None,
                keywords: vec!["preferences".into(), "editor".into(), "terminal".into()],
                icon: Lucide::Settings,
                shortcut: Some(Rc::new(OpenSettings)),
                current: state.page == Page::Settings,
                disabled: false,
                action: PaletteAction::Settings,
            },
            Entry {
                label: "Toggle theme".into(),
                detail: None,
                keywords: vec!["dark".into(), "light".into(), "appearance".into()],
                icon: Lucide::SunMoon,
                shortcut: Some(Rc::new(ToggleTheme)),
                current: false,
                disabled: false,
                action: PaletteAction::ToggleTheme,
            },
            Entry {
                label: "Add project…".into(),
                detail: None,
                keywords: vec!["open".into(), "folder".into(), "repository".into()],
                icon: Lucide::FolderPlus,
                shortcut: None,
                current: false,
                disabled: false,
                action: PaletteAction::AddProject,
            },
        ],
    };
    let mut actions = actions;
    actions.entries.extend(dynamic_entries(state));
    [projects, tabs, actions]
        .into_iter()
        .filter(|group| !group.entries.is_empty())
        .collect()
}

/// The actions that depend on what is loaded: processes (gated by core's
/// answers, so the entry is off exactly when the button is), the proxy URLs,
/// the data actions, pitchfork's install and the doctor. Each exists only when
/// the thing it acts on does.
fn dynamic_entries(state: &AppState) -> Vec<Entry> {
    let mut entries = Vec::new();
    let Some(model) = state.selection.current() else {
        return entries;
    };
    let busy = state.busy().is_some();
    let workspace = model.selected_workspace.clone();
    let detail = workspace
        .as_deref()
        .map(|name| SharedString::from(shown(name)));
    if let Some(status) = model.pitchfork.status.as_ref().filter(|s| s.configured) {
        let gates = processes::actions(status, busy);
        let target = ProcessTarget::of(model, status);
        entries.push(Entry {
            label: if gates.review {
                "Review and start processes…"
            } else {
                "Start processes"
            }
            .into(),
            detail: detail.clone(),
            keywords: vec!["pitchfork".into(), "run".into(), "servers".into()],
            icon: Lucide::Play,
            shortcut: None,
            current: false,
            disabled: !gates.start,
            action: PaletteAction::StartProcesses {
                target: target.clone(),
                review: gates.review,
            },
        });
        entries.push(Entry {
            label: "Stop processes".into(),
            detail: detail.clone(),
            keywords: vec!["pitchfork".into(), "servers".into()],
            icon: Lucide::Square,
            shortcut: None,
            current: false,
            disabled: !gates.stop,
            action: PaletteAction::StopProcesses(target.clone()),
        });
        entries.push(Entry {
            label: "Restart processes".into(),
            detail: detail.clone(),
            keywords: vec!["pitchfork".into(), "servers".into()],
            icon: Lucide::RotateCw,
            shortcut: None,
            current: false,
            disabled: !gates.restart,
            action: PaletteAction::RestartProcesses(target.clone()),
        });
        for daemon in status.daemons.iter().filter(|d| d.url.is_some()) {
            let url = daemon.url.clone().unwrap_or_default();
            entries.push(Entry {
                label: format!("Open {}", processes::daemon_label(daemon)).into(),
                detail: Some(url.clone().into()),
                keywords: vec!["browser".into(), "url".into(), "web".into()],
                icon: Lucide::Globe,
                shortcut: None,
                current: false,
                disabled: false,
                action: PaletteAction::OpenUrl(url),
            });
        }
    }
    if let Some((key, _)) = data_target(state) {
        let of = Some(SharedString::from(format!("of {key}")));
        entries.push(Entry {
            label: "Snapshot data…".into(),
            detail: of.clone(),
            keywords: vec!["save".into(), "seed".into(), "copy".into()],
            icon: Lucide::Camera,
            shortcut: None,
            current: false,
            disabled: busy,
            action: PaletteAction::SnapshotData,
        });
        entries.push(Entry {
            label: "Reset data…".into(),
            detail: of,
            keywords: vec!["clone".into(), "restore".into(), "fresh".into()],
            icon: Lucide::RotateCw,
            shortcut: None,
            current: false,
            disabled: busy,
            action: PaletteAction::ResetData,
        });
    }
    if model
        .pitchfork
        .detection
        .as_ref()
        .is_some_and(|d| !d.usable())
    {
        entries.push(Entry {
            label: "Install pitchfork…".into(),
            detail: Some("shows the command first".into()),
            keywords: vec!["mise".into(), "setup".into(), "get".into()],
            icon: Lucide::Server,
            shortcut: None,
            current: false,
            disabled: false,
            action: PaletteAction::InstallPitchfork,
        });
    }
    if workspace.is_some() {
        entries.push(Entry {
            label: "Run doctor".into(),
            detail,
            keywords: vec!["check".into(), "mise".into(), "diagnose".into()],
            icon: Lucide::ListChecks,
            shortcut: None,
            current: false,
            disabled: false,
            action: PaletteAction::RunDoctor,
        });
    }
    entries
}

pub fn tab_icon(tab: Tab) -> Lucide {
    match tab {
        Tab::Workspaces => Lucide::GitBranch,
        Tab::Data => Lucide::Database,
        Tab::Daemons => Lucide::Server,
        Tab::Setup => Lucide::SlidersHorizontal,
        Tab::Config => Lucide::FileCog,
        Tab::Logs => Lucide::Terminal,
    }
}

pub fn open(state: &Entity<AppState>, window: &mut Window, cx: &mut App) {
    let view = cx.new(|cx| PaletteView::new(state.clone(), window, cx));
    let command = view.read(cx).command.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .w(px(580.))
            .margin_top(px(72.))
            .p_0()
            .close_button(false)
            .child(view.clone())
    });
    // After the dialog took focus, put the cursor in the search field.
    window.defer(cx, move |window, cx| {
        command.update(cx, |state, cx| state.focus(window, cx))
    });
}

struct PaletteView {
    state: Entity<AppState>,
    command: Entity<CommandState>,
    /// Row by row, what each entry does: `on_confirm` reports a position.
    actions: Rc<Vec<Vec<PaletteAction>>>,
    groups: Rc<Vec<Group>>,
}

impl PaletteView {
    fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let groups = groups(state.read(cx));
        let actions = groups
            .iter()
            .map(|g| g.entries.iter().map(|e| e.action.clone()).collect())
            .collect();
        Self {
            command: cx.new(|cx| CommandState::new(window, cx)),
            state,
            actions: Rc::new(actions),
            groups: Rc::new(groups),
        }
    }
}

fn item(entry: &Entry) -> CommandItem {
    let label = entry.label.clone();
    let detail = entry.detail.clone();
    let icon = entry.icon;
    let shortcut = entry.shortcut.clone();
    CommandItem::new()
        .label(entry.label.clone())
        .keywords(entry.keywords.clone())
        .checked(entry.current)
        .disabled(entry.disabled)
        .child(move |window, cx| {
            let hint = shortcut
                .as_ref()
                .and_then(|action| Kbd::binding_for_action(action.as_ref(), None, window));
            h_flex()
                .flex_1()
                .min_w_0()
                .gap_2()
                .items_center()
                .child(Icon::new(icon).size_4())
                .child(div().child(label.clone()))
                .children(detail.clone().map(|detail| {
                    div()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(ui::muted(cx))
                        .child(detail)
                }))
                .children(hint.map(|hint| hint.ml_auto()))
        })
}

impl Render for PaletteView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.clone();
        let actions = self.actions.clone();
        let mut command = Command::new(&self.command)
            .bordered(false)
            .placeholder("Search projects, tabs and actions…")
            .max_h(px(360.))
            .on_confirm(move |ix: IndexPath, window, cx| {
                let Some(action) = actions
                    .get(ix.section)
                    .and_then(|group| group.get(ix.row))
                    .cloned()
                else {
                    return;
                };
                window.close_dialog(cx);
                action.run(&state, window, cx);
            })
            .footer(|_, _, cx| {
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_4()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .text_xs()
                    .text_color(ui::muted(cx))
                    .child("↑↓ navigate")
                    .child("⏎ select")
                    .child("esc close")
            });
        for group in self.groups.iter() {
            command = command.group(
                CommandGroup::new()
                    .label(group.title)
                    .items(group.entries.iter().map(item)),
            );
        }
        // The shortcut that opened the palette closes it again.
        v_flex()
            .on_action(cx.listener(|_, _: &OpenPalette, window, cx| window.close_dialog(cx)))
            .child(command)
    }
}

/// Every text the palette draws for `state`: each entry's label and detail,
/// from the groups the dialog renders.
#[cfg(test)]
pub(super) fn drawn_texts(state: &AppState) -> Vec<String> {
    groups(state)
        .into_iter()
        .flat_map(|group| group.entries)
        .flat_map(|entry| [Some(entry.label), entry.detail])
        .flatten()
        .map(|text| text.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{AppState, PaletteAction, Tab, Uuid, groups};
    use crate::settings::Settings;

    fn state_with(names: &[&str], selected: Option<usize>) -> AppState {
        let mut state = AppState::for_test(Settings::default());
        state.entries = names
            .iter()
            .map(|name| devflow_core::registry::Entry {
                id: Uuid::new_v4(),
                name: (*name).into(),
                root: std::path::PathBuf::from(format!("/tmp/{name}")),
            })
            .collect();
        if let Some(ix) = selected {
            state.selection.select(state.entries[ix].clone());
        }
        state
    }

    #[test]
    fn groups_list_projects_tabs_and_actions() {
        let state = state_with(&["shop", "blog"], Some(1));
        let groups = groups(&state);
        let titles: Vec<_> = groups.iter().map(|g| g.title).collect();
        assert_eq!(titles, ["Projects", "Tabs", "Actions"]);
        let projects = &groups[0].entries;
        assert_eq!(projects.len(), 2);
        assert!(
            !projects[0].current && projects[1].current,
            "the selected one"
        );
        assert_eq!(groups[1].entries.len(), Tab::ALL.len());
        assert!(
            groups[1].entries.iter().all(|e| !e.disabled),
            "tabs apply to the selected project"
        );
        let labels: Vec<_> = groups[2].entries.iter().map(|e| e.label.as_ref()).collect();
        assert_eq!(
            labels,
            [
                "New workspace",
                "Set up devflow…",
                "Refresh",
                "Settings",
                "Toggle theme",
                "Add project…"
            ]
        );
    }

    fn entry<'a>(groups: &'a [super::Group], label: &str) -> Option<&'a super::Entry> {
        groups
            .iter()
            .flat_map(|g| &g.entries)
            .find(|e| e.label == label)
    }

    fn loaded() -> AppState {
        let mut state = AppState::for_test(Settings::default());
        crate::views::ui_tests::install_loaded(&mut state);
        state
    }

    #[test]
    fn process_entries_are_off_exactly_when_the_buttons_are() {
        use devflow_core::processes::{Action, Refusal};
        let mut state = loaded();
        let groups = groups(&state);
        // Approved: start, stop and restart are on, and each web daemon with a
        // URL can be opened.
        for label in ["Start processes", "Stop processes", "Restart processes"] {
            let e = entry(&groups, label).unwrap_or_else(|| panic!("{label}"));
            assert!(!e.disabled, "{label}");
        }
        let open = entry(&groups, "Open web").expect("a daemon with a URL");
        assert_eq!(open.detail.as_deref(), Some("https://web.main.localhost"));
        assert_eq!(
            open.action,
            PaletteAction::OpenUrl("https://web.main.localhost".into())
        );
        assert!(
            entry(&groups, "Open api").is_none(),
            "a daemon without a URL has nothing to open"
        );
        assert!(entry(&groups, "Snapshot data…").is_some_and(|e| !e.disabled));
        assert!(entry(&groups, "Reset data…").is_some());
        assert!(entry(&groups, "Run doctor").is_some());
        assert!(
            entry(&groups, "Install pitchfork…").is_none(),
            "pitchfork works"
        );

        // Not approved: starting is a review, stopping is refused.
        let status = state
            .selection
            .current_mut()
            .unwrap()
            .pitchfork
            .status
            .as_mut()
            .unwrap();
        let refused = |action| devflow_core::processes::Gate {
            allowed: false,
            reason: Some("not approved".into()),
            refusal: Some(Refusal::NotApproved {
                action,
                changed: false,
                format_changed: false,
            }),
        };
        status.can_start = refused(Action::Start);
        status.can_stop = refused(Action::Stop);
        let groups = super::groups(&state);
        assert!(entry(&groups, "Start processes").is_none());
        let review = entry(&groups, "Review and start processes…").unwrap();
        assert!(!review.disabled);
        assert!(entry(&groups, "Stop processes").unwrap().disabled);
        assert!(entry(&groups, "Restart processes").unwrap().disabled);

        // Mise cannot deliver the variables: no start at all.
        let status = state
            .selection
            .current_mut()
            .unwrap()
            .pitchfork
            .status
            .as_mut()
            .unwrap();
        status.can_start = devflow_core::processes::Gate {
            allowed: false,
            reason: Some("mise".into()),
            refusal: Some(Refusal::MiseDelivery {
                daemons: vec!["web".into()],
                delivery: devflow_core::env_file::MiseDelivery::NotLoaded,
            }),
        };
        let groups = super::groups(&state);
        assert!(entry(&groups, "Start processes").unwrap().disabled);

        // A running job turns the actions that start a job off.
        let project = state.selection.current().unwrap().key.project;
        state.mutating.insert(project, "Set up main".into());
        let groups = super::groups(&state);
        for label in [
            "Stop processes",
            "Restart processes",
            "Snapshot data…",
            "Reset data…",
        ] {
            assert!(entry(&groups, label).unwrap().disabled, "{label}");
        }
    }

    #[test]
    fn pitchfork_is_offered_when_it_is_missing_and_processes_when_it_is_not() {
        let mut state = loaded();
        let model = state.selection.current_mut().unwrap();
        model.pitchfork.status = None;
        model.pitchfork.detection = Some(devflow_core::pitchfork::Detection {
            path: None,
            installed: false,
            version: None,
            missing_features: Vec::new(),
            problem: None,
        });
        let groups = groups(&state);
        let install = entry(&groups, "Install pitchfork…").expect("offered");
        assert_eq!(install.action, PaletteAction::InstallPitchfork);
        assert_eq!(install.detail.as_deref(), Some("shows the command first"));
        for label in ["Start processes", "Stop processes", "Open web"] {
            assert!(entry(&groups, label).is_none(), "{label}");
        }
        // Without a project there is nothing to act on.
        let none = AppState::for_test(Settings::default());
        let groups = super::groups(&none);
        for label in ["Run doctor", "Snapshot data…", "Install pitchfork…"] {
            assert!(entry(&groups, label).is_none(), "{label}");
        }
    }

    #[test]
    fn without_a_project_only_actions_that_need_none_are_enabled() {
        let state = state_with(&[], None);
        let groups = groups(&state);
        assert!(groups[0].entries.is_empty() || groups[0].title != "Projects");
        let disabled = |label: &str| {
            groups
                .iter()
                .flat_map(|g| &g.entries)
                .find(|e| e.label == label)
                .map(|e| e.disabled)
        };
        assert_eq!(disabled("New workspace"), Some(true));
        assert_eq!(disabled("Data"), Some(true));
        assert_eq!(disabled("Settings"), Some(false));
        assert_eq!(disabled("Toggle theme"), Some(false));
        assert_eq!(disabled("Add project…"), Some(false));
    }
}
