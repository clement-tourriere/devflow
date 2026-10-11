//! Headless tests of the menu bar setting: the Settings page's switch, its
//! storage, and what the window's shortcut does without a menu bar item (the
//! status item itself is macOS-only and not created in a test).
use super::ui_tests::{frame, install_loaded, open, press_key};
use gpui_kit::{AppContext as _, TestAppContext};

#[cfg(target_os = "macos")]
use super::ui_tests::update;
#[cfg(target_os = "macos")]
use crate::{model::Page, settings::Settings};
#[cfg(target_os = "macos")]
use gpui_kit::test::TestWindowExt as _;

#[gpui_kit::test]
fn keeping_the_app_in_the_menu_bar_is_on_by_default(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    assert!(cx.read_entity(&h.state, |s, _| s.settings.keep_in_menu_bar));
}

#[cfg(target_os = "macos")]
#[gpui_kit::test]
fn the_switch_on_the_settings_page_applies_and_saves_at_once(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    update(cx, &h, |s, _| s.page = Page::Settings);
    frame(cx, &h, |window| {
        assert!(window.find("keep-in-menu-bar").visible(), "the switch");
    });
    let file = cx.read_entity(&h.state, |s, _| s.settings_file().unwrap().to_path_buf());

    cx.update_window(h.window.into(), |_, window, cx| {
        window.click("keep-in-menu-bar", cx)
    })
    .unwrap();
    cx.run_until_parked();
    assert!(!cx.read_entity(&h.state, |s, _| s.settings.keep_in_menu_bar));
    let saved: Settings = toml::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert!(!saved.keep_in_menu_bar, "written without pressing Save");
    // The other settings are as they were.
    assert_eq!(
        Settings {
            keep_in_menu_bar: true,
            ..saved
        },
        Settings::default()
    );

    // Saving the form keeps the switch's value.
    frame(cx, &h, |_| {});
    cx.update_window(h.window.into(), |_, window, cx| {
        window.click("save-settings", cx)
    })
    .unwrap();
    cx.run_until_parked();
    assert!(!cx.read_entity(&h.state, |s, _| s.settings.keep_in_menu_bar));
    let saved: Settings = toml::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert!(!saved.keep_in_menu_bar);

    // And back on.
    cx.update_window(h.window.into(), |_, window, cx| {
        window.click("keep-in-menu-bar", cx)
    })
    .unwrap();
    cx.run_until_parked();
    assert!(cx.read_entity(&h.state, |s, _| s.settings.keep_in_menu_bar));
    let _ = std::fs::remove_dir_all(file.parent().unwrap());
}

#[gpui_kit::test]
fn the_close_shortcut_closes_the_window_when_there_is_no_menu_bar_item(cx: &mut TestAppContext) {
    // The setting is on, but no item was created: a hidden app could not be
    // brought back, so the window really closes.
    let h = open(cx, install_loaded);
    frame(cx, &h, |_| {});
    assert_eq!(cx.update(|cx| cx.windows().len()), 1);
    press_key(cx, &h, "secondary-w");
    assert_eq!(cx.update(|cx| cx.windows().len()), 0);
}
