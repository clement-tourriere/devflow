//! The sidebar: registered projects as a tree (each unfolds to its workspaces,
//! with a status dot and "New workspace"), then "Add project…" and Settings.
//!
//! Which projects are unfolded, and what each lists, lives in `AppState`
//! (`expanded`, `summaries`), not in the menu items: their own open state is
//! keyed by position, so it would stick to the wrong project when the list
//! changes. The tree is therefore drawn here, from plain menu items.
use super::ui;
use crate::{
    actions::OpenSettings,
    model::{Page, WorkspaceSummary},
    state::{AppEvent, AppState},
};
use devflow_core::registry::Entry;
use gpui_kit::{
    assets::IconName as Lucide,
    component::{
        ActiveTheme as _, Collapsible, Icon, IconName, Sizable as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        kbd::Kbd,
        menu::PopupMenuItem,
        sidebar::{Sidebar, SidebarGroup, SidebarHeader, SidebarItem, SidebarMenuItem},
        spinner::Spinner,
        v_flex,
    },
    prelude::*,
    *,
};
use std::rc::Rc;
use uuid::Uuid;

/// A project row and, when unfolded, its workspaces.
#[derive(Clone)]
struct ProjectNode {
    entry: Entry,
    /// The project shown in the main area (and so given the accent).
    selected: bool,
    expanded: bool,
    /// Registered projects fold; a folder that is not one yet has nothing to list.
    foldable: bool,
    /// `None` until the workspaces were read.
    workspaces: Option<Vec<WorkspaceSummary>>,
    /// Why they could not be read.
    error: Option<SharedString>,
    /// The workspace shown in the main area, for the selected project.
    selected_workspace: Option<String>,
    state: Entity<AppState>,
}

/// Everything in the sidebar is one of these, because a sidebar's children
/// all have the same type.
#[derive(Clone)]
enum Node {
    Project(Box<ProjectNode>),
    Item(Box<SidebarMenuItem>),
    Hint(SharedString),
}

impl Collapsible for Node {
    fn is_collapsed(&self) -> bool {
        false
    }

    fn collapsed(self, _: bool) -> Self {
        self
    }
}

impl SidebarItem for Node {
    fn render(
        self,
        id: impl Into<ElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> impl IntoElement {
        match self {
            Node::Project(node) => render_project(*node, window, cx),
            Node::Item(item) => (*item).render(id, window, cx).into_any_element(),
            Node::Hint(text) => div()
                .px_2()
                .py_2()
                .text_sm()
                .text_color(ui::muted(cx))
                .child(text)
                .into_any_element(),
        }
    }
}

/// The element id of a project's row, and of its workspaces' rows.
pub fn project_id(project: Uuid) -> SharedString {
    format!("project-{project}").into()
}

pub fn workspace_id(project: Uuid, workspace: &str) -> SharedString {
    format!("workspace-{project}-{workspace}").into()
}

pub fn new_workspace_id(project: Uuid) -> SharedString {
    format!("new-workspace-{project}").into()
}

fn render_project(node: ProjectNode, window: &mut Window, cx: &mut App) -> AnyElement {
    let id = node.entry.id;
    let state = node.state.clone();
    let primary = cx.theme().primary;
    let border = cx.theme().sidebar_border;

    let select = {
        let state = state.clone();
        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            state.update(cx, |s, cx| s.open_project(id, cx))
        }
    };
    let fold = {
        let state = state.clone();
        Rc::new(move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            // Only fold: the click is not a selection of the row beneath it.
            cx.stop_propagation();
            state.update(cx, |s, cx| s.toggle_expanded(id, cx))
        })
    };
    let remove = state.clone();
    let set_up = state.clone();
    let entry = node.entry.clone();
    let expanded = node.expanded && node.foldable;

    let row = SidebarMenuItem::new(node.entry.name.clone())
        .icon(Icon::new(Lucide::FolderGit2).when(node.selected, |icon| icon.text_color(primary)))
        .when(node.selected, |item| item.font_weight(FontWeight::SEMIBOLD))
        .on_click(select)
        .when(node.foldable, |item| {
            item.suffix(move |_, _| {
                let fold = fold.clone();
                Button::new("fold")
                    .xsmall()
                    .ghost()
                    .icon(
                        Icon::new(IconName::ChevronRight)
                            .size_4()
                            .when(expanded, |icon| icon.rotate(percentage(90. / 360.))),
                    )
                    .on_click(move |event, window, cx| fold(event, window, cx))
            })
        })
        .context_menu(move |menu, _, _| {
            let remove = remove.clone();
            let set_up = set_up.clone();
            let entry = entry.clone();
            let root = entry.root.clone();
            menu.item(
                PopupMenuItem::new("Set up devflow…")
                    .icon(Lucide::WandSparkles)
                    .on_click(move |_, _, cx| {
                        set_up.update(cx, |_, cx| cx.emit(AppEvent::OfferInitialize(root.clone())))
                    }),
            )
            .item(
                PopupMenuItem::new("Remove from list")
                    .icon(Lucide::Trash)
                    .on_click(move |_, _, cx| {
                        remove.update(cx, |s, cx| s.remove_entry(entry.clone(), cx))
                    }),
            )
        })
        .render(project_id(id), window, cx);

    // The accent of the selected project: a bar at the edge of its row.
    let row = div().relative().child(row).when(node.selected, |this| {
        this.child(
            div()
                .absolute()
                .left(px(-8.))
                .top(px(6.))
                .bottom(px(6.))
                .w(px(3.))
                .rounded_full()
                .bg(primary),
        )
    });

    let children = expanded.then(|| {
        let mut items: Vec<AnyElement> = Vec::new();
        match (&node.workspaces, &node.error) {
            (Some(list), _) => {
                for ws in list {
                    let open = {
                        let (state, name) = (state.clone(), ws.name.clone());
                        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                            state.update(cx, |s, cx| s.open_workspace(id, name.clone(), cx))
                        }
                    };
                    let dot = ws.state;
                    items.push(
                        SidebarMenuItem::new(workspace_label(&ws.name))
                            .icon(Icon::new(Lucide::GitBranch))
                            .active(node.selected_workspace.as_deref() == Some(ws.name.as_str()))
                            .suffix(move |_, cx| ui::workspace_dot(dot, cx))
                            .on_click(open)
                            .render(workspace_id(id, &ws.name), window, cx)
                            .into_any_element(),
                    );
                }
            }
            (None, Some(error)) => items.push(
                div()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .truncate()
                    .text_color(ui::muted(cx))
                    .child(error.clone())
                    .into_any_element(),
            ),
            (None, None) => items.push(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .items_center()
                    .text_xs()
                    .text_color(ui::muted(cx))
                    .child(Spinner::new().xsmall())
                    .child("Loading workspaces…")
                    .into_any_element(),
            ),
        }
        let new = state.clone();
        items.push(
            SidebarMenuItem::new("New workspace")
                .icon(Icon::new(IconName::Plus))
                .text_color(ui::muted(cx))
                .on_click(move |_, _, cx| {
                    new.update(cx, |s, cx| s.request_new_workspace(Some(id), cx))
                })
                .render(new_workspace_id(id), window, cx)
                .into_any_element(),
        );
        v_flex()
            .id(SharedString::from(format!("project-children-{id}")))
            .border_l_1()
            .border_color(border)
            .gap_1()
            .ml_3p5()
            .pl_2p5()
            .py_0p5()
            .children(items)
    });

    v_flex()
        .gap_1()
        .child(row)
        .children(children)
        .into_any_element()
}

/// A workspace as the sidebar names it: the name is a legal one even when it
/// looks like a credential, so it is masked here; `open_workspace` keeps the
/// raw one.
pub(crate) fn workspace_label(name: &str) -> String {
    crate::redaction::shown(name)
}

pub fn render(state: &Entity<AppState>, cx: &mut App) -> impl IntoElement {
    let app = state.read(cx);
    let shown = app.selection.current().map(|m| m.key.project);
    let on_projects = app.page == Page::Project;
    let shown_workspace = app
        .selection
        .current()
        .and_then(|m| m.selected_workspace.clone());

    let mut projects: Vec<Node> = app
        .entries
        .iter()
        .map(|entry| {
            let selected = on_projects && shown == Some(entry.id);
            let summary = app.summaries.get(&entry.id);
            let loaded = summary.is_some_and(|s| s.loaded);
            Node::Project(Box::new(ProjectNode {
                entry: entry.clone(),
                selected,
                expanded: app.is_expanded(entry.id),
                foldable: !entry.id.is_nil(),
                workspaces: summary.filter(|_| loaded).map(|s| s.workspaces.clone()),
                error: summary
                    .and_then(|s| s.error.clone())
                    .filter(|_| !loaded)
                    .map(Into::into),
                selected_workspace: shown_workspace.clone().filter(|_| selected),
                state: state.clone(),
            }))
        })
        .collect();
    if projects.is_empty() {
        projects.push(Node::Hint("No projects yet.".into()));
    }
    let settings_active = app.page == Page::Settings;

    let add = state.clone();
    let settings = state.clone();
    let app_items = vec![
        Node::Item(Box::new(
            SidebarMenuItem::new("Add project…")
                .icon(Icon::new(IconName::Plus))
                .on_click(move |_, window, cx| add.update(cx, |s, cx| s.add_project(window, cx))),
        )),
        Node::Item(Box::new(
            SidebarMenuItem::new("Settings")
                .icon(Icon::new(IconName::Settings))
                .active(settings_active)
                .suffix(|window, _| {
                    h_flex().children(
                        Kbd::binding_for_action(&OpenSettings, None, window)
                            .map(|hint| hint.appearance(false)),
                    )
                })
                .on_click(move |_, _, cx| {
                    settings.update(cx, |s, cx| s.set_page(Page::Settings, cx))
                }),
        )),
    ];

    let brand = h_flex()
        .gap_2()
        .items_center()
        .child(
            div()
                .size_6()
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .bg(cx.theme().primary)
                .text_color(cx.theme().primary_foreground)
                .child(Icon::new(Lucide::Zap).size_4()),
        )
        .child(div().font_weight(FontWeight::SEMIBOLD).child("devflow"));

    Sidebar::new("sidebar")
        .w(px(240.))
        .collapsible(false)
        .header(SidebarHeader::new().child(brand))
        .child(SidebarGroup::new("Projects").children(projects))
        .child(SidebarGroup::new("App").children(app_items))
}
