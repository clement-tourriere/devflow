//! Registered projects, "Add project…" and Settings.
use crate::{model::Page, state::AppState};
use gpui_kit::{
    component::{
        IconName,
        menu::PopupMenuItem,
        sidebar::{Sidebar, SidebarGroup, SidebarHeader, SidebarMenu, SidebarMenuItem},
    },
    prelude::*,
    *,
};

pub fn render(state: &Entity<AppState>, cx: &mut App) -> impl IntoElement {
    let app = state.read(cx);
    let selected = app.selection.current().map(|m| m.key.project);
    let on_projects = app.page == Page::Project;

    let projects = app.entries.iter().map(|entry| {
        let id = entry.id;
        let select = state.clone();
        let remove = state.clone();
        let entry = entry.clone();
        SidebarMenuItem::new(entry.name.clone())
            .icon(IconName::FolderClosed)
            .active(on_projects && selected == Some(id))
            .on_click(move |_, _, cx| select.update(cx, |s, cx| s.select_project(id, cx)))
            .context_menu(move |menu, _, _| {
                let remove = remove.clone();
                let entry = entry.clone();
                menu.item(
                    PopupMenuItem::new("Remove from list")
                        .icon(IconName::Delete)
                        .on_click(move |_, _, cx| {
                            remove.update(cx, |s, cx| s.remove_entry(entry.clone(), cx))
                        }),
                )
            })
    });

    let add = state.clone();
    let settings = state.clone();
    let actions = [
        SidebarMenuItem::new("Add project…")
            .icon(IconName::Plus)
            .on_click(move |_, window, cx| add.update(cx, |s, cx| s.add_project(window, cx))),
        SidebarMenuItem::new("Settings")
            .icon(IconName::Settings)
            .active(app.page == Page::Settings)
            .on_click(move |_, _, cx| settings.update(cx, |s, cx| s.set_page(Page::Settings, cx))),
    ];

    Sidebar::new("sidebar")
        .w(px(240.))
        .collapsible(false)
        .header(SidebarHeader::new().child("devflow"))
        .child(SidebarGroup::new("Projects").child(SidebarMenu::new().children(projects)))
        .child(SidebarGroup::new("App").child(SidebarMenu::new().children(actions)))
}
