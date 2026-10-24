//! GUI preferences in `<config dir>/devflow/gui.toml`, and launching the
//! user's editor and terminal.
//!
//! The editor and terminal commands are not the app's alone: `devflow open`
//! uses them too, so they live in core's `settings.toml` next to `gui.toml`
//! ([`devflow_core::settings`]). An older `gui.toml` holds them; [`load`] moves
//! them over once and never replaces a value `settings.toml` already has.
use anyhow::{Context as _, Result};
use devflow_core::settings as shared;
pub use devflow_core::settings::command_line;
use gpui_kit::{
    App, Window, WindowAppearance,
    component::{Theme, ThemeMode},
};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeChoice {
    Light,
    Dark,
    #[default]
    System,
}

impl ThemeChoice {
    pub const ALL: [ThemeChoice; 3] = [ThemeChoice::Light, ThemeChoice::Dark, ThemeChoice::System];

    pub fn label(self) -> &'static str {
        match self {
            Self::Light => "Light",
            Self::Dark => "Dark",
            Self::System => "System",
        }
    }

    /// Apply the choice now. `System` follows the OS appearance at this moment;
    /// the window keeps following it afterwards (see `Shell`).
    ///
    /// The app's native appearance (window edges, system menus) follows an
    /// explicit Light or Dark. It is released for `System`, because while one
    /// is forced the window no longer reports changes of the OS appearance.
    pub fn apply(self, window: Option<&mut Window>, cx: &mut App) {
        match self {
            Self::Light => {
                cx.set_window_appearance(Some(WindowAppearance::Light));
                Theme::change(ThemeMode::Light, window, cx);
            }
            Self::Dark => {
                cx.set_window_appearance(Some(WindowAppearance::Dark));
                Theme::change(ThemeMode::Dark, window, cx);
            }
            Self::System => {
                cx.set_window_appearance(None);
                Theme::sync_system_appearance(window, cx);
            }
        }
        // A theme change loads the library's colors afresh.
        crate::theme::apply_accent(cx);
    }

    /// What the quick toggle switches to when the UI is showing `dark` now:
    /// always an explicit Light or Dark, whatever the setting was.
    pub fn toggled(dark: bool) -> Self {
        if dark { Self::Light } else { Self::Dark }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Program and arguments; the workspace path is appended unless the
    /// command contains `{path}`. Stored in core's `settings.toml` (shared with
    /// `devflow open`); read from an older `gui.toml` for the migration.
    #[serde(skip_serializing)]
    pub editor_command: String,
    #[serde(skip_serializing)]
    pub terminal_command: String,
    pub theme: ThemeChoice,
    /// Closing the window hides it to the menu bar and the app keeps running;
    /// off, closing the window quits. Only macOS has the menu bar item, and
    /// elsewhere closing always quits.
    pub keep_in_menu_bar: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            editor_command: shared::DEFAULT_EDITOR.into(),
            terminal_command: shared::default_terminal().into(),
            theme: ThemeChoice::System,
            keep_in_menu_bar: true,
        }
    }
}

pub fn path() -> Result<PathBuf> {
    Ok(devflow_core::paths::config_dir()?.join("gui.toml"))
}

/// The file the editor and terminal commands are kept in: core's
/// `settings.toml`, beside the given `gui.toml`.
fn shared_path(gui: &Path) -> PathBuf {
    gui.with_file_name("settings.toml")
}

/// Missing file means defaults; an unreadable one is an error the caller reports.
pub fn load() -> Result<Settings> {
    load_from(&path()?)
}

/// [`load`] from the given `gui.toml` (and the `settings.toml` beside it).
///
/// The editor and terminal come from `settings.toml`. A value an older
/// `gui.toml` still holds that `settings.toml` does not have is moved there
/// first and only then dropped from `gui.toml`; if it cannot be moved it
/// stays where it is and is used from there, so no setting is ever lost.
pub fn load_from(gui: &Path) -> Result<Settings> {
    let mut settings = match std::fs::read_to_string(gui) {
        Ok(text) => toml::from_str::<Settings>(&text)
            .with_context(|| format!("invalid {}", gui.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", gui.display())),
    };
    // What the older file held, by key (a default is not a setting).
    let legacy = std::fs::read_to_string(gui)
        .ok()
        .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
        .unwrap_or_default();
    let old = |key: &str| legacy.get(key).and_then(|v| v.as_str()).map(str::to_string);
    let (old_editor, old_terminal) = (old("editor_command"), old("terminal_command"));

    let file = shared_path(gui);
    let mut stored = shared::load_from(&file)?;
    if stored.adopt(old_editor.as_deref(), old_terminal.as_deref()) {
        // A failed write leaves `gui.toml` alone, and the values in use.
        if shared::save_to(&file, &stored).is_ok() {
            let _ = save_gui_to(gui, &settings);
        }
    } else if (old_editor.is_some() || old_terminal.is_some())
        && legacy_is_settled(&stored, &old_editor, &old_terminal)
    {
        // `settings.toml` already decided both: nothing is left to move.
        let _ = save_gui_to(gui, &settings);
    }
    settings.editor_command = stored
        .editor_command
        .clone()
        .or(old_editor)
        .unwrap_or_else(|| shared::DEFAULT_EDITOR.into());
    settings.terminal_command = stored
        .terminal_command
        .clone()
        .or(old_terminal)
        .unwrap_or_else(|| shared::default_terminal().into());
    Ok(settings)
}

/// Every old key that is there has been decided by `settings.toml`.
fn legacy_is_settled(
    stored: &shared::Settings,
    editor: &Option<String>,
    terminal: &Option<String>,
) -> bool {
    (editor.is_none() || stored.editor_command.is_some())
        && (terminal.is_none() || stored.terminal_command.is_some())
}

/// Write `gui.toml`: the theme and the menu bar setting, and nothing of the
/// editor and terminal, which are the command line's as much as the app's
/// (`settings.toml`, see [`save_shared_changes`]). Atomically. This is what every
/// background save is: a stale copy of the commands held in memory can never
/// overwrite what was edited in `settings.toml` meanwhile.
pub fn save_to(path: &Path, settings: &Settings) -> Result<()> {
    save_gui_to(path, settings)
}

fn save_gui_to(path: &Path, settings: &Settings) -> Result<()> {
    let dir = path.parent().context("settings path has no parent")?;
    std::fs::create_dir_all(dir)?;
    let mut table = toml::Table::try_from(settings)?;
    // An older `gui.toml` may still hold the only copy of the editor and
    // terminal commands: they stay until `settings.toml` has them (readable,
    // and the key set), whatever this save is for. Never copied into
    // `settings.toml` from here: what is in memory may be stale.
    if let Some(old) = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
    {
        let stored = shared::load_from(&shared_path(path)).ok();
        for (key, settled) in [
            (
                "editor_command",
                stored.as_ref().is_some_and(|s| s.editor_command.is_some()),
            ),
            (
                "terminal_command",
                stored
                    .as_ref()
                    .is_some_and(|s| s.terminal_command.is_some()),
            ),
        ] {
            if let (false, Some(value)) = (settled, old.get(key)) {
                table.insert(key.to_string(), value.clone());
            }
        }
    }
    // Write beside the target and rename, so a crash never leaves a torn file.
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, toml::to_string_pretty(&table)?)?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot write {}", path.display()))
}

/// Write the editor and terminal commands the user changed (`after` against
/// `before`, what the form was opened with) to the `settings.toml` beside `gui`.
/// The file is read again first and only the keys that changed are written, so
/// an edit made to the other one meanwhile is kept. A command set back to the
/// default that was never chosen is removed rather than written down. A file
/// that cannot be read is an error, never replaced.
pub fn save_shared_changes(gui: &Path, before: &Settings, after: &Settings) -> Result<()> {
    let editor = before.editor_command != after.editor_command;
    let terminal = before.terminal_command != after.terminal_command;
    if !editor && !terminal {
        return Ok(());
    }
    let file = shared_path(gui);
    let mut stored = shared::load_from(&file)?;
    let pick = |current: &Option<String>, new: &str, default: &str| {
        (current.is_some() || new != default).then(|| new.to_string())
    };
    if editor {
        stored.editor_command = pick(
            &stored.editor_command,
            &after.editor_command,
            shared::DEFAULT_EDITOR,
        );
    }
    if terminal {
        stored.terminal_command = pick(
            &stored.terminal_command,
            &after.terminal_command,
            shared::default_terminal(),
        );
    }
    if stored != shared::Settings::default() || file.exists() {
        shared::save_to(&file, &stored)?;
    }
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Saves settings without blocking the UI thread, newest value last.
///
/// `submit` (UI thread, never waits for the disk) records the value to write
/// and says whether a `drain` has to be started. `drain` runs on a background
/// thread and writes the newest recorded value until nothing newer is left,
/// one file write at a time: a value superseded before it was written is never
/// written, so a quick series of changes cannot land out of order.
/// `write_now` is the synchronous path (the Settings form's Save); it takes
/// its turn between the background writes and supersedes what is waiting.
#[derive(Default)]
pub struct SettingsWriter {
    state: Mutex<WriterState>,
    /// Held while a file is written. `state` never is, so `submit` is quick.
    io: Mutex<()>,
}

#[derive(Default)]
struct WriterState {
    pending: Option<Settings>,
    draining: bool,
}

impl SettingsWriter {
    /// Record `settings` as the newest value. `true` means no `drain` is
    /// running and the caller must start one; otherwise the running one
    /// picks the value up.
    #[must_use]
    pub fn submit(&self, settings: Settings) -> bool {
        let mut state = lock(&self.state);
        state.pending = Some(settings);
        !std::mem::replace(&mut state.draining, true)
    }

    /// Write recorded values to `path` until none is left (blocking: run it
    /// off the UI thread). The error is that of the last write, if it failed:
    /// a failure followed by a write that went through left the file right.
    pub fn drain(&self, path: &Path) -> Option<anyhow::Error> {
        self.drain_with(|settings| save_to(path, settings))
    }

    fn drain_with(&self, mut write: impl FnMut(&Settings) -> Result<()>) -> Option<anyhow::Error> {
        let mut last = None;
        loop {
            // Taking the value and writing it is one step as far as
            // `write_now` is concerned.
            let _turn = lock(&self.io);
            let next = {
                let mut state = lock(&self.state);
                let next = state.pending.take();
                if next.is_none() {
                    state.draining = false;
                }
                next
            };
            let Some(settings) = next else {
                return last;
            };
            last = write(&settings).err();
        }
    }

    /// Write `settings` now, after a background write in progress. Whatever
    /// was waiting is older and is dropped, once this write went through.
    pub fn write_now(&self, path: &Path, settings: &Settings) -> Result<()> {
        let _turn = lock(&self.io);
        save_to(path, settings)?;
        lock(&self.state).pending = None;
        Ok(())
    }
}

/// How a launched program ended, as far as the user needs to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchExit {
    pub program: String,
    pub success: bool,
    /// `None` when a signal ended it.
    pub code: Option<i32>,
    /// What it printed on stderr (masked, capped).
    pub stderr: String,
}

impl LaunchExit {
    /// Text for the user when the program failed; `None` when it succeeded.
    pub fn failure(&self) -> Option<String> {
        if self.success {
            return None;
        }
        let how = match self.code {
            Some(code) => format!("exited with status {code}"),
            None => "was terminated by a signal".to_string(),
        };
        let stderr = self.stderr.trim();
        Some(if stderr.is_empty() {
            format!("`{}` {how}.", self.program)
        } else {
            format!("`{}` {how}: {stderr}", self.program)
        })
    }
}

/// Stderr kept from a launched program.
const STDERR_CAP: usize = 8 * 1024;

fn read_capped(mut pipe: impl std::io::Read, into: &Mutex<Vec<u8>>) {
    let mut chunk = [0u8; 4096];
    // Keep reading after the cap so the program never blocks on a full pipe.
    while let Ok(n) = pipe.read(&mut chunk) {
        if n == 0 {
            break;
        }
        let mut kept = into.lock().unwrap_or_else(|e| e.into_inner());
        let room = STDERR_CAP.saturating_sub(kept.len());
        kept.extend_from_slice(&chunk[..n.min(room)]);
    }
}

/// Start `command` for `dir` and leave it running independently. A program
/// that cannot be started is an error here; one that starts and then fails
/// (`open -a MissingApp`) is reported later through the returned receiver,
/// which gets one [`LaunchExit`] when the program ends.
pub fn launch(command: &str, dir: &Path) -> Result<mpsc::Receiver<LaunchExit>> {
    let argv = command_line(command, dir)?;
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "cannot run `{}` (apps started from the Dock have a short PATH; use the full path in Settings)",
                argv[0]
            )
        })?;
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (drained_tx, drained_rx) = mpsc::channel::<()>();
    if let Some(pipe) = child.stderr.take() {
        let captured = captured.clone();
        std::thread::spawn(move || {
            read_capped(pipe, &captured);
            let _ = drained_tx.send(());
        });
    }
    // Waiting blocks, and an editor may run for hours: use a thread of its
    // own rather than a slot of the shared background pool.
    let (tx, rx) = mpsc::channel();
    let program = argv[0].clone();
    std::thread::spawn(move || {
        let status = child.wait();
        // A launcher that detached a child may keep stderr open; do not wait for it.
        let _ = drained_rx.recv_timeout(Duration::from_millis(300));
        let stderr = String::from_utf8_lossy(&captured.lock().unwrap_or_else(|e| e.into_inner()))
            .into_owned();
        let exit = match status {
            Ok(status) => LaunchExit {
                program,
                success: status.success(),
                code: status.code(),
                stderr: devflow_core::redact::text(&stderr),
            },
            Err(e) => LaunchExit {
                program,
                success: false,
                code: None,
                stderr: format!("cannot wait for it: {e}"),
            },
        };
        let _ = tx.send(exit);
    });
    Ok(rx)
}

/// What a launched program is doing, seen from the UI.
#[derive(Debug, PartialEq, Eq)]
pub enum LaunchPoll {
    Running,
    Succeeded,
    Failed(String),
}

pub fn poll_launch(exit: &mpsc::Receiver<LaunchExit>) -> LaunchPoll {
    match exit.try_recv() {
        Ok(exit) => match exit.failure() {
            Some(message) => LaunchPoll::Failed(message),
            None => LaunchPoll::Succeeded,
        },
        Err(mpsc::TryRecvError::Empty) => LaunchPoll::Running,
        Err(mpsc::TryRecvError::Disconnected) => {
            LaunchPoll::Failed("the launched program ended without reporting a result".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;

    #[test]
    fn settings_round_trip_and_tolerate_missing_keys() {
        // The app's own file holds the theme and the menu bar setting; the
        // commands are core's (`settings.toml`).
        let dir = tempfile::tempdir().unwrap();
        let gui = dir.path().join("gui.toml");
        let settings = Settings {
            editor_command: "zed".into(),
            terminal_command: "kitty".into(),
            theme: ThemeChoice::Dark,
            keep_in_menu_bar: false,
        };
        save_to(&gui, &settings).unwrap();
        save_shared_changes(&gui, &Settings::default(), &settings).unwrap();
        let text = std::fs::read_to_string(&gui).unwrap();
        assert!(
            !text.contains("editor_command") && !text.contains("terminal_command"),
            "{text}"
        );
        let shared = shared::load_from(&dir.path().join("settings.toml")).unwrap();
        assert_eq!(shared.editor_command.as_deref(), Some("zed"));
        assert_eq!(shared.terminal_command.as_deref(), Some("kitty"));
        assert_eq!(load_from(&gui).unwrap(), settings);
        let partial: Settings = toml::from_str("theme = \"light\"").unwrap();
        assert_eq!(partial.editor_command, "code");
        assert_eq!(partial.theme, ThemeChoice::Light);
    }

    #[test]
    fn the_old_commands_in_gui_toml_move_to_settings_toml_once() {
        let dir = tempfile::tempdir().unwrap();
        let gui = dir.path().join("gui.toml");
        let file = dir.path().join("settings.toml");
        std::fs::write(
            &gui,
            "editor_command = \"zed\"\nterminal_command = \"kitty\"\ntheme = \"dark\"\n",
        )
        .unwrap();
        let loaded = load_from(&gui).unwrap();
        assert_eq!(loaded.editor_command, "zed");
        assert_eq!(loaded.terminal_command, "kitty");
        assert_eq!(loaded.theme, ThemeChoice::Dark);
        // Moved: settings.toml has them, gui.toml keeps the rest only.
        let stored = shared::load_from(&file).unwrap();
        assert_eq!(stored.editor_command.as_deref(), Some("zed"));
        assert_eq!(stored.terminal_command.as_deref(), Some("kitty"));
        let text = std::fs::read_to_string(&gui).unwrap();
        assert!(
            text.contains("theme = \"dark\"") && !text.contains("zed"),
            "{text}"
        );
        // A second load changes nothing.
        assert_eq!(load_from(&gui).unwrap(), loaded);
    }

    #[test]
    fn a_value_settings_toml_has_wins_and_nothing_is_lost_when_it_cannot_be_moved() {
        let dir = tempfile::tempdir().unwrap();
        let gui = dir.path().join("gui.toml");
        let file = dir.path().join("settings.toml");
        // settings.toml chose the editor; the old file also names a terminal.
        std::fs::write(&file, "editor_command = \"nvim\"\n").unwrap();
        std::fs::write(
            &gui,
            "editor_command = \"zed\"\nterminal_command = \"kitty\"\n",
        )
        .unwrap();
        let loaded = load_from(&gui).unwrap();
        assert_eq!(loaded.editor_command, "nvim", "the newer choice");
        assert_eq!(loaded.terminal_command, "kitty", "taken over");
        assert_eq!(
            shared::load_from(&file)
                .unwrap()
                .terminal_command
                .as_deref(),
            Some("kitty")
        );

        // settings.toml cannot be written (its place is a directory): the old
        // values stay in gui.toml and are what the app uses.
        let dir = tempfile::tempdir().unwrap();
        let gui = dir.path().join("gui.toml");
        std::fs::create_dir(dir.path().join("settings.toml")).unwrap();
        std::fs::write(&gui, "editor_command = \"zed\"\n").unwrap();
        // (An unreadable settings.toml is an error the caller reports.)
        assert!(load_from(&gui).is_err());
        assert!(std::fs::read_to_string(&gui).unwrap().contains("zed"));
        std::fs::remove_dir(dir.path().join("settings.toml")).unwrap();
        // Unwritable: a read-only directory.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let ro = tempfile::tempdir().unwrap();
            let gui = ro.path().join("gui.toml");
            std::fs::write(&gui, "editor_command = \"zed\"\n").unwrap();
            std::fs::set_permissions(ro.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
            let loaded = load_from(&gui).unwrap();
            std::fs::set_permissions(ro.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(loaded.editor_command, "zed");
            assert!(std::fs::read_to_string(&gui).unwrap().contains("zed"));
        }
    }

    #[test]
    fn an_unchosen_default_is_not_written_down_and_only_edited_keys_are_written() {
        let dir = tempfile::tempdir().unwrap();
        let gui = dir.path().join("gui.toml");
        let file = dir.path().join("settings.toml");
        let before = Settings::default();
        save_to(&gui, &before).unwrap();
        save_shared_changes(&gui, &before, &before).unwrap();
        assert!(!file.exists());
        let chosen = Settings {
            editor_command: "zed".into(),
            ..Settings::default()
        };
        save_shared_changes(&gui, &before, &chosen).unwrap();
        let stored = shared::load_from(&file).unwrap();
        assert_eq!(stored.editor_command.as_deref(), Some("zed"));
        assert_eq!(stored.terminal_command, None);

        // The command line edited the terminal meanwhile; the form changed the
        // editor only: the terminal is kept as the other wrote it.
        std::fs::write(
            &file,
            "editor_command = \"zed\"\nterminal_command = \"kitty\"\n",
        )
        .unwrap();
        let again = Settings {
            editor_command: "nvim".into(),
            ..chosen.clone()
        };
        save_shared_changes(&gui, &chosen, &again).unwrap();
        let stored = shared::load_from(&file).unwrap();
        assert_eq!(stored.editor_command.as_deref(), Some("nvim"));
        assert_eq!(stored.terminal_command.as_deref(), Some("kitty"));
    }

    #[test]
    fn a_theme_save_keeps_legacy_commands_until_they_are_migrated() {
        let dir = tempfile::tempdir().unwrap();
        let gui = dir.path().join("gui.toml");
        let file = dir.path().join("settings.toml");
        std::fs::write(
            &gui,
            "editor_command = \"zed\"\nterminal_command = \"kitty\"\ntheme = \"light\"\n",
        )
        .unwrap();
        // The migration cannot happen yet: settings.toml's place is a directory.
        std::fs::create_dir(&file).unwrap();
        assert!(load_from(&gui).is_err());
        // (The app would run on defaults and still save a theme.)
        let toggled = Settings {
            theme: ThemeChoice::Dark,
            ..Settings::default()
        };
        let writer = SettingsWriter::default();
        assert!(writer.submit(toggled.clone()));
        assert!(writer.drain(&gui).is_none());
        let text = std::fs::read_to_string(&gui).unwrap();
        assert!(text.contains("theme = \"dark\""), "{text}");
        assert!(
            text.contains("editor_command = \"zed\"")
                && text.contains("terminal_command = \"kitty\""),
            "the only copy survives: {text}"
        );
        assert!(file.is_dir(), "nothing was written into the shared place");

        // Storage recovers: the next start migrates them, and only then does
        // gui.toml let them go.
        std::fs::remove_dir(&file).unwrap();
        let loaded = load_from(&gui).unwrap();
        assert_eq!(
            (
                loaded.editor_command.as_str(),
                loaded.terminal_command.as_str()
            ),
            ("zed", "kitty")
        );
        let stored = shared::load_from(&file).unwrap();
        assert_eq!(stored.editor_command.as_deref(), Some("zed"));
        assert_eq!(stored.terminal_command.as_deref(), Some("kitty"));
        let text = std::fs::read_to_string(&gui).unwrap();
        assert!(
            !text.contains("zed") && text.contains("theme = \"dark\""),
            "{text}"
        );
        // A later save keeps them out of gui.toml.
        save_to(&gui, &toggled).unwrap();
        assert!(
            !std::fs::read_to_string(&gui)
                .unwrap()
                .contains("editor_command")
        );
    }

    #[test]
    fn a_background_save_never_writes_the_shared_commands() {
        // A theme toggle saves the in-memory settings, whose commands may be
        // stale: what `settings.toml` holds now is not touched.
        let dir = tempfile::tempdir().unwrap();
        let gui = dir.path().join("gui.toml");
        let file = dir.path().join("settings.toml");
        std::fs::write(&file, "editor_command = \"edited-outside\"\n").unwrap();
        let stale = Settings {
            editor_command: "stale".into(),
            terminal_command: "stale-terminal".into(),
            theme: ThemeChoice::Dark,
            ..Settings::default()
        };
        save_to(&gui, &stale).unwrap();
        let writer = SettingsWriter::default();
        assert!(writer.submit(stale.clone()));
        assert!(writer.drain(&gui).is_none());
        writer.write_now(&gui, &stale).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "editor_command = \"edited-outside\"\n"
        );
        let text = std::fs::read_to_string(&gui).unwrap();
        assert!(text.contains("theme = \"dark\""), "{text}");
        // An unreadable settings.toml does not stop the theme from being saved.
        std::fs::write(&file, "not = [valid").unwrap();
        save_to(&gui, &Settings::default()).unwrap();
        assert!(save_shared_changes(&gui, &Settings::default(), &stale).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "not = [valid");
    }

    #[test]
    fn keeping_the_app_in_the_menu_bar_is_on_unless_turned_off() {
        assert!(Settings::default().keep_in_menu_bar);
        // A file written before the setting existed keeps the default.
        let old: Settings = toml::from_str("editor_command = \"zed\"").unwrap();
        assert!(old.keep_in_menu_bar);
        let off: Settings = toml::from_str("keep_in_menu_bar = false").unwrap();
        assert!(!off.keep_in_menu_bar);
        let text = toml::to_string_pretty(&off).unwrap();
        assert!(text.contains("keep_in_menu_bar = false"), "{text}");
    }

    #[test]
    fn the_quick_switch_always_leads_to_an_explicit_light_or_dark() {
        assert_eq!(ThemeChoice::toggled(true), ThemeChoice::Light);
        assert_eq!(ThemeChoice::toggled(false), ThemeChoice::Dark);
    }

    #[test]
    fn saving_creates_the_directory_and_replaces_the_file_whole() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nested").join("gui.toml");
        let mut settings = Settings {
            theme: ThemeChoice::Dark,
            ..Settings::default()
        };
        save_to(&file, &settings).unwrap();
        settings.theme = ThemeChoice::Light;
        save_to(&file, &settings).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(toml::from_str::<Settings>(&text).unwrap(), settings);
        assert!(
            !file.with_extension("toml.tmp").exists(),
            "no torn leftover"
        );
    }

    fn named(editor: &str) -> Settings {
        Settings {
            editor_command: editor.into(),
            ..Settings::default()
        }
    }

    fn themed(theme: ThemeChoice) -> Settings {
        Settings {
            theme,
            ..Settings::default()
        }
    }

    fn theme_in(file: &Path) -> ThemeChoice {
        load_from(file).unwrap().theme
    }

    #[test]
    fn a_series_of_submitted_values_writes_only_the_newest() {
        let writer = SettingsWriter::default();
        assert!(writer.submit(named("one")), "the first starts a writer");
        assert!(!writer.submit(named("two")), "a writer is already due");
        assert!(!writer.submit(named("three")));
        let mut written = Vec::new();
        let failed = writer.drain_with(|s| {
            written.push(s.editor_command.clone());
            Ok(())
        });
        assert!(failed.is_none());
        assert_eq!(written, ["three"], "superseded values are never written");
        assert!(
            writer.submit(named("four")),
            "after a drain a new one is due"
        );
    }

    #[test]
    fn a_value_submitted_during_a_write_neither_waits_for_it_nor_is_lost() {
        let writer = Arc::new(SettingsWriter::default());
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        assert!(writer.submit(named("first")));
        let background = {
            let writer = writer.clone();
            std::thread::spawn(move || {
                let mut written = Vec::new();
                let mut blocked = false;
                let failed = writer.drain_with(|s| {
                    written.push(s.editor_command.clone());
                    if !blocked {
                        blocked = true;
                        started_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                    }
                    Ok(())
                });
                (written, failed.is_none())
            })
        };
        started_rx.recv().unwrap();
        // The first write is on the disk right now; the UI thread is not held.
        assert!(!writer.submit(named("second")), "the writer picks it up");
        assert!(!writer.submit(named("third")));
        release_tx.send(()).unwrap();
        let (written, ok) = background.join().unwrap();
        assert!(ok);
        assert_eq!(written, ["first", "third"], "in order, newest last");
    }

    #[test]
    fn only_the_last_write_decides_whether_a_failure_is_reported() {
        let writer = SettingsWriter::default();
        let _ = writer.submit(named("one"));
        let mut calls = 0;
        let failed = writer.drain_with(|_| {
            calls += 1;
            bail!("disk full")
        });
        assert_eq!(calls, 1);
        assert!(format!("{:#}", failed.unwrap()).contains("disk full"));

        // A failure followed by a write that went through left the file right.
        let writer = Arc::new(SettingsWriter::default());
        let _ = writer.submit(named("one"));
        let mut first = true;
        let again = writer.clone();
        let failed = writer.drain_with(|_| {
            if std::mem::take(&mut first) {
                let _ = again.submit(named("two"));
                bail!("busy")
            }
            Ok(())
        });
        assert!(failed.is_none());
    }

    #[test]
    fn a_synchronous_save_supersedes_what_is_waiting_and_is_not_overwritten_later() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("gui.toml");
        let writer = SettingsWriter::default();
        // A theme change is waiting for its background write ...
        assert!(writer.submit(themed(ThemeChoice::Light)));
        // ... when the Settings form saves.
        writer.write_now(&file, &themed(ThemeChoice::Dark)).unwrap();
        let mut written = 0;
        let failed = writer.drain_with(|_| {
            written += 1;
            Ok(())
        });
        assert!(failed.is_none());
        assert_eq!(written, 0, "the older value is not written after it");
        assert_eq!(theme_in(&file), ThemeChoice::Dark);

        // A failed save leaves what was waiting to be written.
        let blocked = dir.path().join("a-file");
        std::fs::write(&blocked, "x").unwrap();
        assert!(writer.submit(themed(ThemeChoice::Light)));
        assert!(
            writer
                .write_now(&blocked.join("gui.toml"), &themed(ThemeChoice::System))
                .is_err()
        );
        assert!(writer.drain(&file).is_none());
        assert_eq!(theme_in(&file), ThemeChoice::Light);
    }

    #[test]
    fn draining_writes_the_file_and_reports_a_path_that_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("deep").join("gui.toml");
        let writer = SettingsWriter::default();
        assert!(writer.submit(themed(ThemeChoice::Dark)));
        assert!(writer.drain(&file).is_none());
        assert_eq!(theme_in(&file), ThemeChoice::Dark);

        // The parent is a regular file: nothing can be created under it.
        let blocked = dir.path().join("a-file");
        std::fs::write(&blocked, "x").unwrap();
        assert!(writer.submit(themed(ThemeChoice::Light)));
        assert!(writer.drain(&blocked.join("gui.toml")).is_some());
        assert!(
            writer.submit(themed(ThemeChoice::System)),
            "a failure frees the writer"
        );
    }

    /// Wait for a launched program's verdict (it is reported from a thread).
    fn verdict(exit: &mpsc::Receiver<LaunchExit>) -> LaunchPoll {
        for _ in 0..200 {
            match poll_launch(exit) {
                LaunchPoll::Running => std::thread::sleep(Duration::from_millis(25)),
                other => return other,
            }
        }
        LaunchPoll::Running
    }

    #[test]
    fn a_launcher_that_starts_and_then_fails_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        // Like `open -a MissingApp`: it spawns fine, then exits non-zero.
        let exit = launch(
            "sh -c 'echo \"Unable to find application named MissingApp\" >&2; exit 1' ignored {path}",
            dir.path(),
        )
        .unwrap();
        match verdict(&exit) {
            LaunchPoll::Failed(message) => {
                assert!(message.contains("exited with status 1"), "{message}");
                assert!(message.contains("MissingApp"), "stderr is shown: {message}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn a_launcher_that_succeeds_reports_nothing_and_a_missing_binary_errors_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let exit = launch("sh -c 'exit 0' ignored {path}", dir.path()).unwrap();
        assert_eq!(verdict(&exit), LaunchPoll::Succeeded);
        let error = launch("/definitely/not/installed", dir.path()).unwrap_err();
        assert!(format!("{error:#}").contains("cannot run"), "{error:#}");
    }

    #[test]
    fn launcher_stderr_is_masked_and_capped() {
        let dir = tempfile::tempdir().unwrap();
        let script =
            "echo postgres://u:hunter2@h/db >&2; head -c 100000 /dev/zero | tr '\\0' x >&2; exit 2"; // kingfisher:ignore
        let exit = launch(&format!("sh -c \"{script}\" ignored {{path}}"), dir.path()).unwrap();
        match verdict(&exit) {
            LaunchPoll::Failed(message) => {
                assert!(!message.contains("hunter2"), "{message}");
                assert!(message.contains("u:***@h"), "{message}");
                assert!(message.len() < 2 * STDERR_CAP, "{} bytes", message.len());
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }
}
