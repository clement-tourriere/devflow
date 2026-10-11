//! Application actions and their keyboard shortcuts.
//!
//! The handlers are global (`App::on_action`), so a shortcut works wherever
//! focus is, also when nothing is focused. Dialogs live outside the shell's
//! view tree, so everything they trigger goes through the same handlers.
use crate::{model::Tab, views::Shell};
use gpui_kit::{prelude::*, *};

actions!(
    devflow,
    [
        Quit,
        CloseWindow,
        OpenPalette,
        NewWorkspace,
        Refresh,
        OpenSettings,
        ToggleTheme,
        GoToWorkspaces,
        GoToData,
        GoToDaemons,
        GoToSetup,
        GoToConfig,
        GoToLogs,
    ]
);

/// The tab each `GoTo…` action shows, in shortcut order (⌘1…⌘6).
pub fn tab_action(tab: Tab) -> Box<dyn Action> {
    match tab {
        Tab::Workspaces => Box::new(GoToWorkspaces),
        Tab::Data => Box::new(GoToData),
        Tab::Daemons => Box::new(GoToDaemons),
        Tab::Setup => Box::new(GoToSetup),
        Tab::Config => Box::new(GoToConfig),
        Tab::Logs => Box::new(GoToLogs),
    }
}

/// How to reach the window's shell from a global handler.
#[derive(Clone)]
pub struct ShellHandle {
    window: AnyWindowHandle,
    shell: WeakEntity<Shell>,
}

impl ShellHandle {
    pub fn new(window: &Window, shell: WeakEntity<Shell>) -> Self {
        Self {
            window: window.window_handle(),
            shell,
        }
    }
}

impl Global for ShellHandle {}

/// Register the key bindings and handlers. Call once, after `gpui_kit::init`.
pub fn init(cx: &mut App) {
    // `secondary` is Command on macOS and Control elsewhere.
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("secondary-w", CloseWindow, None),
        KeyBinding::new("secondary-k", OpenPalette, None),
        KeyBinding::new("secondary-n", NewWorkspace, None),
        KeyBinding::new("secondary-r", Refresh, None),
        KeyBinding::new("secondary-,", OpenSettings, None),
        KeyBinding::new("secondary-shift-l", ToggleTheme, None),
        KeyBinding::new("secondary-1", GoToWorkspaces, None),
        KeyBinding::new("secondary-2", GoToData, None),
        KeyBinding::new("secondary-3", GoToDaemons, None),
        KeyBinding::new("secondary-4", GoToSetup, None),
        KeyBinding::new("secondary-5", GoToConfig, None),
        KeyBinding::new("secondary-6", GoToLogs, None),
    ]);
    crate::views::init_wizard(cx);
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &CloseWindow, cx| {
        on_shell(cx, |shell, window, cx| shell.close_window(window, cx))
    });
    // The application menu (macOS): the shortcuts, and a Quit that really quits.
    cx.set_menus([Menu::new("devflow").items([
        MenuItem::action("Close Window", CloseWindow),
        MenuItem::separator(),
        MenuItem::action("Quit devflow", Quit),
    ])]);
    cx.on_action(|_: &OpenPalette, cx| {
        on_shell(cx, |shell, window, cx| shell.open_palette(window, cx))
    });
    cx.on_action(|_: &NewWorkspace, cx| {
        on_shell(cx, |shell, window, cx| shell.new_workspace(window, cx))
    });
    cx.on_action(|_: &Refresh, cx| on_shell(cx, |shell, window, cx| shell.refresh(window, cx)));
    cx.on_action(|_: &OpenSettings, cx| {
        on_shell(cx, |shell, window, cx| shell.open_settings(window, cx))
    });
    cx.on_action(|_: &ToggleTheme, cx| {
        on_shell(cx, |shell, window, cx| shell.toggle_theme(window, cx))
    });
    cx.on_action(|_: &GoToWorkspaces, cx| go_to(cx, Tab::Workspaces));
    cx.on_action(|_: &GoToData, cx| go_to(cx, Tab::Data));
    cx.on_action(|_: &GoToDaemons, cx| go_to(cx, Tab::Daemons));
    cx.on_action(|_: &GoToSetup, cx| go_to(cx, Tab::Setup));
    cx.on_action(|_: &GoToConfig, cx| go_to(cx, Tab::Config));
    cx.on_action(|_: &GoToLogs, cx| go_to(cx, Tab::Logs));
}

fn go_to(cx: &mut App, tab: Tab) {
    on_shell(cx, move |shell, window, cx| {
        shell.go_to_tab(tab, window, cx)
    });
}

/// Run `f` on the window's shell. It runs after the current dispatch has
/// finished: the window is leased while a key press is being handled, so it
/// cannot be updated from inside the handler.
fn on_shell(cx: &mut App, f: impl FnOnce(&mut Shell, &mut Window, &mut Context<Shell>) + 'static) {
    let Some(handle) = cx.try_global::<ShellHandle>().cloned() else {
        return;
    };
    cx.defer(move |cx| {
        handle
            .window
            .update(cx, |_, window, cx| {
                handle
                    .shell
                    .update(cx, |shell, cx| f(shell, window, cx))
                    .ok();
            })
            .ok();
    });
}
