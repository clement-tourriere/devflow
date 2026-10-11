//! devflow desktop app: GPUI + gpui-kit over the synchronous `devflow-core`.
mod activity;
mod jobs;
mod model;
mod ops;
mod settings;
mod state;
mod views;

use gpui_kit::{prelude::*, *};
use std::path::PathBuf;

actions!(devflow, [Quit]);

/// `--project <root>` selects (and registers, if it is one) a project at startup.
fn parse_args() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--project" {
            return args.next().map(PathBuf::from);
        }
        if let Some(value) = arg.strip_prefix("--project=") {
            return Some(PathBuf::from(value));
        }
    }
    None
}

fn main() {
    let preselect = parse_args();
    // Removing the workspace the app was started from is refused, as in the CLI.
    let launch_dir = std::env::current_dir().ok();
    jobs::install_panic_hook();
    let settings = settings::load().unwrap_or_else(|e| {
        eprintln!("devflow-gui: {e:#}; using default settings");
        settings::Settings::default()
    });

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            settings.theme.apply(None, cx);

            cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(1240.), px(800.)), cx)),
                titlebar: Some(TitlebarOptions {
                    title: Some("devflow".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, move |window, cx| {
                let state =
                    cx.new(|cx| state::AppState::new(settings, preselect, launch_dir, window, cx));
                cx.new(|cx| views::Shell::new(state, window, cx))
            })
            .expect("cannot open the window");
            cx.activate(true);
        });
}
