//! Settings shared by the command line and the desktop app: `settings.toml` in
//! the configuration directory.
//!
//! Today that is how to open a workspace: the editor and the terminal
//! (`devflow open`, the app's "Open in editor" and "Open in terminal"). They
//! used to live in the app's `gui.toml`; the app reads those values once and
//! moves them here ([`Settings::adopt`]), so the two open the same tools.
//!
//! A command is a program and its arguments, split on whitespace with single
//! and double quotes honoured. The workspace's path is appended as the last
//! argument unless the command contains `{path}`, which is replaced instead.
//! The command runs with the workspace as its working directory.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The editor when nothing is configured.
pub const DEFAULT_EDITOR: &str = "code";

/// The terminal when nothing is configured.
pub fn default_terminal() -> &'static str {
    if cfg!(target_os = "macos") {
        "open -a Terminal"
    } else {
        "x-terminal-emulator --working-directory={path}"
    }
}

/// What can be opened with a configured command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    Editor,
    Terminal,
}

impl Tool {
    pub fn label(self) -> &'static str {
        match self {
            Tool::Editor => "editor",
            Tool::Terminal => "terminal",
        }
    }
}

/// The contents of `settings.toml`. A key that is not there means the default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Program and arguments that open a directory in the editor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub editor_command: Option<String>,
    /// Program and arguments that open a directory in a terminal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_command: Option<String>,
}

impl Settings {
    /// The editor command: the configured one or [`DEFAULT_EDITOR`].
    pub fn editor(&self) -> &str {
        self.editor_command.as_deref().unwrap_or(DEFAULT_EDITOR)
    }

    /// The terminal command: the configured one or [`default_terminal`].
    pub fn terminal(&self) -> &str {
        self.terminal_command
            .as_deref()
            .unwrap_or(default_terminal())
    }

    /// The command for `tool`.
    pub fn command(&self, tool: Tool) -> &str {
        match tool {
            Tool::Editor => self.editor(),
            Tool::Terminal => self.terminal(),
        }
    }

    /// Take over values from an older place (the app's `gui.toml`) for the
    /// keys this file does not have yet. A value set here is never replaced.
    /// Returns whether anything was taken, which is when the file has to be
    /// written.
    pub fn adopt(&mut self, editor: Option<&str>, terminal: Option<&str>) -> bool {
        let mut taken = false;
        for (mine, old) in [
            (&mut self.editor_command, editor),
            (&mut self.terminal_command, terminal),
        ] {
            if mine.is_none()
                && let Some(old) = old.map(str::trim).filter(|old| !old.is_empty())
            {
                *mine = Some(old.to_string());
                taken = true;
            }
        }
        taken
    }
}

/// Where `settings.toml` is.
pub fn path() -> Result<PathBuf> {
    Ok(crate::paths::config_dir()?.join("settings.toml"))
}

/// The settings; a missing file means the defaults, an unreadable one is an error.
pub fn load() -> Result<Settings> {
    load_from(&path()?)
}

/// [`load`] from `path`.
pub fn load_from(path: &Path) -> Result<Settings> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("invalid {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Write the settings (atomically: a crash never leaves a torn file).
pub fn save(settings: &Settings) -> Result<()> {
    save_to(&path()?, settings)
}

/// [`save`] to `path`.
pub fn save_to(path: &Path, settings: &Settings) -> Result<()> {
    crate::fsutil::write_atomic(path, toml::to_string_pretty(settings)?.as_bytes(), false)
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
        bail!("no command is configured; set one in the settings");
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
    fn the_path_is_appended_unless_templated() {
        let dir = Path::new("/work/my app");
        assert_eq!(command_line("code", dir).unwrap(), ["code", "/work/my app"]);
        assert_eq!(
            command_line("term --cwd={path}", dir).unwrap(),
            ["term", "--cwd=/work/my app"]
        );
        assert!(command_line("", dir).is_err());
    }

    #[test]
    fn defaults_apply_to_keys_that_are_missing() {
        let none = Settings::default();
        assert_eq!(none.editor(), "code");
        assert_eq!(none.terminal(), default_terminal());
        let some: Settings = toml::from_str("editor_command = \"zed\"").unwrap();
        assert_eq!(some.editor(), "zed");
        assert_eq!(some.terminal(), default_terminal());
        assert_eq!(some.command(Tool::Editor), "zed");
        assert!(toml::from_str::<Settings>("bogus = 1").is_err());
    }

    #[test]
    fn old_values_fill_only_what_is_missing() {
        let mut settings = Settings {
            editor_command: Some("zed".into()),
            terminal_command: None,
        };
        assert!(settings.adopt(Some("code"), Some(" kitty ")));
        assert_eq!(settings.editor(), "zed", "a value set here wins");
        assert_eq!(settings.terminal(), "kitty");
        assert!(
            !settings.adopt(Some("nvim"), Some("alacritty")),
            "nothing left to take"
        );
        // An empty old value is no value.
        let mut empty = Settings::default();
        assert!(!empty.adopt(Some("  "), None));
        assert_eq!(empty, Settings::default());
    }

    #[test]
    fn settings_round_trip_through_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nested").join("settings.toml");
        assert_eq!(load_from(&file).unwrap(), Settings::default());
        let settings = Settings {
            editor_command: Some("zed".into()),
            terminal_command: Some("kitty".into()),
        };
        save_to(&file, &settings).unwrap();
        assert_eq!(load_from(&file).unwrap(), settings);
        std::fs::write(&file, "editor_command = [").unwrap();
        assert!(load_from(&file).is_err());
    }
}
