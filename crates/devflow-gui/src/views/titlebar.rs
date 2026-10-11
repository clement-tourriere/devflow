//! The window's title bar: where you are (project / tab), and the commands
//! that are always at hand: search, refresh and the light/dark switch.
use super::ui;
use crate::{
    actions::{OpenPalette, Refresh, ToggleTheme},
    model::Page,
    state::{AppState, PitchforkInstall},
};
use gpui_kit::{
    component::{
        ActiveTheme as _, IconName, Sizable as _, TitleBar,
        breadcrumb::{Breadcrumb, BreadcrumbItem},
        button::{Button, ButtonVariants as _},
        h_flex,
        kbd::Kbd,
        spinner::Spinner,
    },
    prelude::*,
    *,
};

/// What the bar shows, read once per frame.
struct Where {
    project: Option<String>,
    page: Page,
    tab: &'static str,
    refreshing: bool,
    /// A long operation that is running: its title.
    working: Option<String>,
}

impl Where {
    fn of(state: &AppState) -> Self {
        let model = state.selection.current();
        Self {
            project: model.map(|m| m.entry.name.clone()),
            page: state.page,
            tab: state.tab.title(),
            refreshing: model.is_some_and(|m| m.refreshing) || state.busy().is_some(),
            working: working(state),
        }
    }

    /// `project / Tab`, or `devflow / Settings`, or just `devflow`.
    fn crumbs(&self) -> Vec<String> {
        match (self.page, &self.project) {
            (Page::Settings, _) => vec!["devflow".into(), "Settings".into()],
            (Page::Project, Some(project)) => vec![project.clone(), self.tab.into()],
            (Page::Project, None) => vec!["devflow".into()],
        }
    }
}

/// What is running that takes a while: the selected project's operation (a
/// setup, a clone, a reset…) or the install of pitchfork.
fn working(state: &AppState) -> Option<String> {
    state.busy().map(str::to_string).or_else(|| {
        matches!(state.pitchfork_install, PitchforkInstall::Running(_))
            .then(|| "Installing pitchfork".to_string())
    })
}

/// An action as a click: the same path as its keyboard shortcut.
fn dispatch(action: &dyn Action) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let action = action.boxed_clone();
    move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx)
}

pub fn render(state: &Entity<AppState>, window: &Window, cx: &App) -> impl IntoElement {
    let here = Where::of(state.read(cx));
    let dark = cx.theme().is_dark();
    let crumbs = here.crumbs();
    let hint = Kbd::binding_for_action(&OpenPalette, None, window);

    let progress = here.working.clone().map(|title| {
        h_flex()
            .id("titlebar-progress")
            .gap_1p5()
            .items_center()
            .max_w(px(300.))
            .px_2()
            .text_xs()
            .text_color(ui::muted(cx))
            .child(Spinner::new().xsmall())
            .child(div().truncate().child(format!("{title}…")))
            .test_support()
    });
    let search = Button::new("palette")
        .outline()
        .small()
        .w(px(260.))
        .tooltip_with_action("Search and commands", &OpenPalette, None)
        .on_click(dispatch(&OpenPalette))
        .child(
            h_flex()
                .w_full()
                .gap_2()
                .items_center()
                .text_color(ui::muted(cx))
                .child(gpui_kit::component::Icon::new(IconName::Search).size_3p5())
                .child(div().flex_1().text_left().child("Search or jump to…"))
                .children(hint),
        );
    let refresh = Button::new("refresh")
        .ghost()
        .small()
        .icon(IconName::RefreshCw)
        .loading(here.refreshing)
        .tooltip_with_action("Refresh", &Refresh, None)
        .on_click(dispatch(&Refresh));
    let theme = Button::new("theme-toggle")
        .ghost()
        .small()
        // The icon is where a click leads.
        .icon(if dark { IconName::Sun } else { IconName::Moon })
        .tooltip_with_action(
            if dark {
                "Switch to light theme"
            } else {
                "Switch to dark theme"
            },
            &ToggleTheme,
            None,
        )
        .on_click(dispatch(&ToggleTheme));

    TitleBar::new()
        .child(Breadcrumb::new().children(crumbs.into_iter().map(BreadcrumbItem::new)))
        .child(
            h_flex()
                .gap_1()
                .pr_2()
                .items_center()
                .children(progress)
                .child(search)
                .child(refresh)
                .child(theme),
        )
}

#[cfg(test)]
mod tests {
    use super::Where;
    use crate::model::Page;

    fn here(project: Option<&str>, page: Page, tab: &'static str) -> Where {
        Where {
            project: project.map(str::to_string),
            page,
            tab,
            refreshing: false,
            working: None,
        }
    }

    #[test]
    fn breadcrumb_names_the_project_and_tab_or_settings() {
        assert_eq!(
            here(Some("shop"), Page::Project, "Data").crumbs(),
            ["shop", "Data"]
        );
        assert_eq!(
            here(Some("shop"), Page::Settings, "Data").crumbs(),
            ["devflow", "Settings"]
        );
        assert_eq!(
            here(None, Page::Project, "Workspaces").crumbs(),
            ["devflow"]
        );
    }
}
