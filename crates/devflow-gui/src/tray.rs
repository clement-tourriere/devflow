//! Keeping devflow open in the menu bar when its window is closed.
//!
//! macOS has a status item with a small menu: "Open devflow", the registered
//! projects, "Quit devflow". Closing the window (the red button, ⌘W) then
//! hides it instead of ending the app: the window and the state behind it
//! stay alive, so showing it again is instant and nothing is reloaded. While
//! it is hidden the Dock icon goes too (an accessory app) and comes back
//! with the window. Other platforms have no status item here, and closing the
//! window quits as before.
//!
//! What to do is decided by plain functions ([`close_action`], [`menu_items`],
//! [`poll_due`]) so it is tested without a status item.
use crate::state::AppState;
use devflow_core::registry::Entry;
use gpui_kit::{ActivationPolicy, AnyWindowHandle, App, Entity, Global, WeakEntity, Window};
use uuid::Uuid;

/// Projects listed in the menu; the window has the complete list.
pub const MAX_PROJECTS: usize = 12;
/// While the window is hidden the state's 5 s poll only acts every this many ticks.
pub const BACKGROUND_POLL_EVERY: u64 = 6;

/// What closing the window does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseAction {
    /// Hide it; the app keeps running behind the menu bar item.
    HideToMenuBar,
    /// End the app.
    Quit,
}

/// `keep_in_menu_bar` is the setting; `menu_bar_item` says whether there is
/// an item to come back through (without one, a hidden app is unreachable).
pub fn close_action(keep_in_menu_bar: bool, menu_bar_item: bool) -> CloseAction {
    if keep_in_menu_bar && menu_bar_item {
        CloseAction::HideToMenuBar
    } else {
        CloseAction::Quit
    }
}

/// Is this tick of the state's poll one that refreshes? Always while the
/// window is showing; one in [`BACKGROUND_POLL_EVERY`] while it is hidden.
pub fn poll_due(background: bool, tick: u64) -> bool {
    !background || tick.is_multiple_of(BACKGROUND_POLL_EVERY)
}

/// A choice in the menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Open,
    Project(Uuid),
    Quit,
}

impl Command {
    /// The id the menu item carries.
    pub fn id(&self) -> String {
        match self {
            Self::Open => "open".into(),
            Self::Quit => "quit".into(),
            Self::Project(id) => format!("project:{id}"),
        }
    }

    pub fn parse(id: &str) -> Option<Self> {
        match id {
            "open" => Some(Self::Open),
            "quit" => Some(Self::Quit),
            _ => id
                .strip_prefix("project:")
                .and_then(|id| id.parse().ok())
                .map(Self::Project),
        }
    }
}

/// One row of the menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    Command(Command, String),
    Separator,
}

/// The menu for the registered projects.
pub fn menu_items(entries: &[Entry]) -> Vec<Item> {
    let mut items = vec![Item::Command(Command::Open, "Open devflow".into())];
    if !entries.is_empty() {
        items.push(Item::Separator);
        items.extend(entries.iter().take(MAX_PROJECTS).map(|entry| {
            Item::Command(
                Command::Project(entry.id),
                crate::state::truncate(&entry.name, 40),
            )
        }));
    }
    items.push(Item::Separator);
    items.push(Item::Command(Command::Quit, "Quit devflow".into()));
    items
}

/// Template images are drawn in black with transparency only; macOS tints
/// them for the light or dark menu bar. This draws devflow's glyph (a branch,
/// like the app's own branch icon) on a `size` square, as RGBA.
pub fn template_icon(size: u32) -> Vec<u8> {
    const SAMPLES: u32 = 4;
    // The glyph is laid out on a 24-unit grid with a 2-unit stroke.
    let scale = 24.0 / size as f32;
    let covered = |x: f32, y: f32| -> bool {
        let to = |(px, py): (f32, f32)| ((x - px).powi(2) + (y - py).powi(2)).sqrt();
        // The stem, with round ends.
        let stem = {
            let t = (y - 3.0).clamp(0.0, 12.0);
            ((x - 6.0).powi(2) + (y - 3.0 - t).powi(2)).sqrt() <= 1.0
        };
        let ring = |center: (f32, f32)| (to(center) - 3.0).abs() <= 1.0;
        // A quarter circle from (18, 9) to (9, 18) around (9, 9).
        let arc = {
            let (dx, dy) = (x - 9.0, y - 9.0);
            (dx >= 0.0 && dy >= 0.0 && ((dx * dx + dy * dy).sqrt() - 9.0).abs() <= 1.0)
                || to((18.0, 9.0)) <= 1.0
                || to((9.0, 18.0)) <= 1.0
        };
        stem || ring((18.0, 6.0)) || ring((6.0, 18.0)) || arc
    };
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for row in 0..size {
        for col in 0..size {
            let mut hits = 0;
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let x = (col as f32 + (sx as f32 + 0.5) / SAMPLES as f32) * scale;
                    let y = (row as f32 + (sy as f32 + 0.5) / SAMPLES as f32) * scale;
                    hits += u32::from(covered(x, y));
                }
            }
            let alpha = (hits * 255 / (SAMPLES * SAMPLES)) as u8;
            rgba.extend_from_slice(&[0, 0, 0, alpha]);
        }
    }
    rgba
}

/// What the app keeps while it lives: where the window is, whether it is
/// hidden, and the status item (`None` where there is none).
struct MenuBar {
    window: AnyWindowHandle,
    state: WeakEntity<AppState>,
    hidden: bool,
    item: Option<native::Item>,
    /// The menu as last given to the item.
    shown: Vec<Item>,
}

impl Global for MenuBar {}

/// Is there a menu bar item to come back through?
pub fn available(cx: &App) -> bool {
    cx.try_global::<MenuBar>().is_some_and(|m| m.item.is_some())
}

/// Does the setting ask to keep the app running?
fn keep_in_menu_bar(cx: &App) -> bool {
    cx.try_global::<MenuBar>()
        .and_then(|m| m.state.upgrade())
        .is_some_and(|state| state.read(cx).settings.keep_in_menu_bar)
}

/// Create the status item and route the window's close to it. Call once,
/// from the closure that creates the window.
pub fn install(window: &mut Window, state: &Entity<AppState>, cx: &mut App) {
    let shown = menu_items(&state.read(cx).entries);
    let item = native::create(&shown, cx);
    let has_item = item.is_some();
    cx.set_global(MenuBar {
        window: window.window_handle(),
        state: state.downgrade(),
        hidden: false,
        item,
        shown,
    });
    // The red button. Returning `false` keeps the window; it is hidden after
    // this event is handled.
    window.on_window_should_close(cx, |_, cx| {
        match close_action(keep_in_menu_bar(cx), available(cx)) {
            CloseAction::HideToMenuBar => {
                cx.defer(hide);
                false
            }
            CloseAction::Quit => true,
        }
    });
    if has_item {
        // The project list follows the registry.
        cx.observe(state, |state, cx| refresh_menu(&state, cx))
            .detach();
    }
}

/// A click on the Dock icon, or the app launched again: show the window.
/// (Registered on the application, before it runs.)
pub fn reopen(cx: &mut App) {
    if available(cx) {
        show(cx);
    }
}

/// ⌘W: the same as the red button.
pub fn close_window(window: &mut Window, cx: &mut App) {
    match close_action(keep_in_menu_bar(cx), available(cx)) {
        CloseAction::HideToMenuBar => hide(cx),
        CloseAction::Quit => window.remove_window(),
    }
}

fn refresh_menu(state: &Entity<AppState>, cx: &mut App) {
    let items = menu_items(&state.read(cx).entries);
    if let Some(bar) = cx.try_global::<MenuBar>()
        && bar.shown != items
        && let Some(item) = &bar.item
    {
        item.set_menu(&items);
        cx.global_mut::<MenuBar>().shown = items;
    }
}

fn set_hidden(hidden: bool, cx: &mut App) {
    let Some(bar) = cx.try_global::<MenuBar>() else {
        return;
    };
    let state = bar.state.clone();
    cx.global_mut::<MenuBar>().hidden = hidden;
    if let Some(state) = state.upgrade() {
        state.update(cx, |state, _| state.background = hidden);
    }
}

/// Hide the window and the Dock icon. The window is kept, with all its state.
fn hide(cx: &mut App) {
    set_hidden(true, cx);
    cx.set_activation_policy(ActivationPolicy::Accessory);
    cx.hide();
}

/// Show and focus the window, and bring the Dock icon back.
fn show(cx: &mut App) {
    set_hidden(false, cx);
    cx.set_activation_policy(ActivationPolicy::Regular);
    cx.activate(true);
    if let Some(window) = cx.try_global::<MenuBar>().map(|m| m.window) {
        window
            .update(cx, |_, window, _| window.activate_window())
            .ok();
    }
}

/// A choice made in the menu.
fn run(command: Command, cx: &mut App) {
    match command {
        Command::Quit => cx.quit(),
        Command::Open => show(cx),
        Command::Project(id) => {
            show(cx);
            let state = cx.try_global::<MenuBar>().and_then(|m| m.state.upgrade());
            if let Some(state) = state {
                state.update(cx, |state, cx| state.select_project(id, cx));
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod native {
    use super::{Command, Item as Row, run, template_icon};
    use futures::{StreamExt as _, channel::mpsc};
    use gpui_kit::App;
    use tray_icon::{
        Icon, TrayIcon, TrayIconBuilder, TrayIconEvent,
        menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    };

    /// 18 points tall on a Retina menu bar.
    const ICON_PX: u32 = 36;

    /// The status item. Dropping it removes it from the menu bar.
    pub struct Item(TrayIcon);

    fn build(items: &[Row]) -> Option<Menu> {
        let menu = Menu::new();
        for item in items {
            match item {
                Row::Command(command, label) => {
                    menu.append(&MenuItem::with_id(command.id(), label, true, None))
                }
                Row::Separator => menu.append(&PredefinedMenuItem::separator()),
            }
            .ok()?;
        }
        Some(menu)
    }

    /// Take the status item's own events (mouse moves, enter, leave, clicks)
    /// out of tray-icon's channel. Without a handler the library queues each
    /// one on an unbounded channel nobody reads, which grows for as long as the
    /// pointer moves over the icon. The menu needs none of them: it opens by
    /// itself and its choices arrive as [`MenuEvent`]s. Must run before the
    /// item exists; the library keeps the first handler it is given.
    pub(super) fn drop_tray_events() {
        TrayIconEvent::set_event_handler(Some(|_: TrayIconEvent| {}));
    }

    /// Must run on the main thread once the app has launched, which is where
    /// GPUI calls the closure given to `run`.
    pub fn create(items: &[Row], cx: &mut App) -> Option<Item> {
        drop_tray_events();
        let made = (|| {
            let icon = Icon::from_rgba(template_icon(ICON_PX), ICON_PX, ICON_PX).ok()?;
            let tray = TrayIconBuilder::new()
                .with_icon(icon)
                .with_icon_as_template(true)
                .with_tooltip("devflow")
                .with_menu(Box::new(build(items)?))
                .build()
                .ok()?;
            Some(tray)
        })();
        let Some(tray) = made else {
            eprintln!("devflow-gui: cannot create the menu bar item; closing the window quits");
            return None;
        };
        // AppKit calls the handler on the main thread; the choice is handled
        // by a GPUI task, which the channel wakes.
        let (tx, mut rx) = mpsc::unbounded::<String>();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let _ = tx.unbounded_send(event.id.0);
        }));
        cx.spawn(async move |cx| {
            while let Some(id) = rx.next().await {
                if let Some(command) = Command::parse(&id) {
                    cx.update(|cx| run(command, cx));
                }
            }
        })
        .detach();
        Some(Item(tray))
    }

    impl Item {
        pub fn set_menu(&self, items: &[Row]) {
            if let Some(menu) = build(items) {
                self.0.set_menu(Some(Box::new(menu)));
            }
        }
    }
}

/// No status item: closing the window quits.
#[cfg(not(target_os = "macos"))]
mod native {
    use super::Item as Row;
    use gpui_kit::App;

    pub struct Item;

    pub fn create(_: &[Row], _: &mut App) -> Option<Item> {
        None
    }

    impl Item {
        pub fn set_menu(&self, _: &[Row]) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(name: &str) -> Entry {
        Entry {
            id: Uuid::new_v4(),
            name: name.into(),
            root: PathBuf::from("/work").join(name),
        }
    }

    #[test]
    fn closing_hides_only_when_asked_to_and_there_is_a_way_back() {
        use CloseAction::*;
        assert_eq!(close_action(true, true), HideToMenuBar);
        assert_eq!(close_action(false, true), Quit, "the setting is off");
        assert_eq!(close_action(true, false), Quit, "no menu bar item");
        assert_eq!(close_action(false, false), Quit);
    }

    #[test]
    fn the_poll_slows_down_while_hidden() {
        assert!((1..=20).all(|tick| poll_due(false, tick)));
        let due: Vec<u64> = (1..=24).filter(|tick| poll_due(true, *tick)).collect();
        assert_eq!(due, [6, 12, 18, 24]);
    }

    #[test]
    fn commands_survive_their_menu_id() {
        let id = Uuid::new_v4();
        for command in [Command::Open, Command::Quit, Command::Project(id)] {
            assert_eq!(Command::parse(&command.id()), Some(command));
        }
        assert_eq!(Command::parse("project:not-a-uuid"), None);
        assert_eq!(Command::parse("something else"), None);
    }

    #[test]
    fn the_menu_opens_lists_projects_and_quits() {
        let none = menu_items(&[]);
        assert_eq!(
            none,
            [
                Item::Command(Command::Open, "Open devflow".into()),
                Item::Separator,
                Item::Command(Command::Quit, "Quit devflow".into()),
            ]
        );

        let (shop, blog) = (entry("shop"), entry("blog"));
        let items = menu_items(&[shop.clone(), blog.clone()]);
        assert_eq!(
            items,
            [
                Item::Command(Command::Open, "Open devflow".into()),
                Item::Separator,
                Item::Command(Command::Project(shop.id), "shop".into()),
                Item::Command(Command::Project(blog.id), "blog".into()),
                Item::Separator,
                Item::Command(Command::Quit, "Quit devflow".into()),
            ]
        );
    }

    #[test]
    fn a_long_project_list_is_capped_and_names_are_kept_short() {
        let many: Vec<Entry> = (0..30).map(|i| entry(&format!("project-{i}"))).collect();
        let items = menu_items(&many);
        let projects = items
            .iter()
            .filter(|i| matches!(i, Item::Command(Command::Project(_), _)))
            .count();
        assert_eq!(projects, MAX_PROJECTS);
        // First and last are always there.
        assert!(matches!(
            items.first(),
            Some(Item::Command(Command::Open, _))
        ));
        assert!(matches!(
            items.last(),
            Some(Item::Command(Command::Quit, _))
        ));

        let long = entry(&"x".repeat(100));
        let Item::Command(_, label) = &menu_items(&[long])[2] else {
            panic!("a project row")
        };
        assert!(label.chars().count() <= 41, "{label}");
    }

    /// The handler is installed before the item is built (`create` calls it
    /// first), so nothing the item sends is ever queued. The library's own
    /// sender is private, so what can be checked is that installing is
    /// repeatable and leaves the receiver empty.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_status_items_raw_events_are_not_queued() {
        native::drop_tray_events();
        native::drop_tray_events();
        assert!(tray_icon::TrayIconEvent::receiver().is_empty());
    }

    #[test]
    fn the_template_icon_is_black_with_transparency_and_not_empty() {
        let size = 36;
        let rgba = template_icon(size);
        assert_eq!(rgba.len(), (size * size * 4) as usize);
        assert!(
            rgba.chunks(4).all(|px| px[..3] == [0, 0, 0]),
            "a template carries its shape in the alpha channel only"
        );
        let alphas: Vec<u8> = rgba.chunks(4).map(|px| px[3]).collect();
        let drawn = alphas.iter().filter(|a| **a > 0).count();
        let solid = alphas.iter().filter(|a| **a == 255).count();
        let total = alphas.len();
        assert!(
            drawn > total / 10 && drawn < total / 2,
            "{drawn} of {total}"
        );
        assert!(solid > 0, "there is solid ink");
        assert!(
            alphas.iter().any(|a| *a > 0 && *a < 255),
            "the edges are anti-aliased"
        );
        // The four corners of the square are empty.
        let at = |row: u32, col: u32| alphas[(row * size + col) as usize];
        assert_eq!(
            [
                at(0, 0),
                at(0, size - 1),
                at(size - 1, 0),
                at(size - 1, size - 1)
            ],
            [0, 0, 0, 0]
        );
    }
}
