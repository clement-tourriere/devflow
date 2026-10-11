//! GUI preferences in `<config dir>/devflow/gui.toml`, and launching the
//! user's editor and terminal.
use anyhow::{Context as _, Result, bail};
use gpui_kit::{
    App, Window,
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

    pub fn apply(self, window: Option<&mut Window>, cx: &mut App) {
        match self {
            Self::Light => Theme::change(ThemeMode::Light, window, cx),
            Self::Dark => Theme::change(ThemeMode::Dark, window, cx),
            Self::System => Theme::sync_system_appearance(window, cx),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Program and arguments; the workspace path is appended unless the
    /// command contains `{path}`.
    pub editor_command: String,
    pub terminal_command: String,
    pub theme: ThemeChoice,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            editor_command: "code".into(),
            terminal_command: if cfg!(target_os = "macos") {
                "open -a Terminal".into()
            } else {
                "x-terminal-emulator --working-directory={path}".into()
            },
            theme: ThemeChoice::System,
        }
    }
}

pub fn path() -> Result<PathBuf> {
    Ok(devflow_core::paths::config_dir()?.join("gui.toml"))
}

/// Missing file means defaults; an unreadable one is an error the caller reports.
pub fn load() -> Result<Settings> {
    let path = path()?;
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("invalid {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

pub fn save(settings: &Settings) -> Result<()> {
    let path = path()?;
    let dir = path.parent().context("settings path has no parent")?;
    std::fs::create_dir_all(dir)?;
    // Write beside the target and rename, so a crash never leaves a torn file.
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, toml::to_string_pretty(settings)?)?;
    std::fs::rename(&tmp, &path).with_context(|| format!("cannot write {}", path.display()))
}

/// Split a command line on whitespace, honouring single and double quotes.
pub fn split_command(command: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in command.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => current.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started || !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (None, c) => current.push(c),
        }
    }
    if started || !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// The argv for opening `dir` with `command`.
pub fn command_line(command: &str, dir: &Path) -> Result<Vec<String>> {
    let mut parts = split_command(command);
    if parts.is_empty() {
        bail!("no command is configured; set one in Settings");
    }
    let dir_text = dir.to_string_lossy();
    if command.contains("{path}") {
        for part in &mut parts {
            *part = part.replace("{path}", &dir_text);
        }
    } else {
        parts.push(dir_text.into_owned());
    }
    Ok(parts)
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

    #[test]
    fn splits_quoted_commands() {
        assert_eq!(
            split_command("open -a Terminal"),
            ["open", "-a", "Terminal"]
        );
        assert_eq!(
            split_command(r#""/Applications/Visual Studio Code.app/bin/code" --wait"#),
            ["/Applications/Visual Studio Code.app/bin/code", "--wait"]
        );
        assert_eq!(split_command("  "), Vec::<String>::new());
        assert_eq!(split_command("a '' b"), ["a", "", "b"]);
    }

    #[test]
    fn path_is_appended_unless_templated() {
        let dir = Path::new("/work/my app");
        assert_eq!(command_line("code", dir).unwrap(), ["code", "/work/my app"]);
        assert_eq!(
            command_line("term --cwd={path}", dir).unwrap(),
            ["term", "--cwd=/work/my app"]
        );
        assert!(command_line("", dir).is_err());
    }

    #[test]
    fn settings_round_trip_and_tolerate_missing_keys() {
        let settings = Settings {
            editor_command: "zed".into(),
            terminal_command: "kitty".into(),
            theme: ThemeChoice::Dark,
        };
        let text = toml::to_string_pretty(&settings).unwrap();
        assert_eq!(toml::from_str::<Settings>(&text).unwrap(), settings);
        let partial: Settings = toml::from_str("theme = \"light\"").unwrap();
        assert_eq!(partial.editor_command, "code");
        assert_eq!(partial.theme, ThemeChoice::Light);
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
