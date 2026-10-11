//! Rendering helpers: writers that tolerate closed pipes, colors, tables, the
//! versioned JSON envelope and the setup progress printer. Secret redaction
//! is `devflow_core::redact`; nothing here implements its own.
use super::Ctx;
use anyhow::Result;
use devflow_core::receipt::{Phase, PhaseState};
use devflow_core::redact;
use devflow_core::setup::Progress;
use serde::Serialize;
use serde_json::Value;
use std::io::{IsTerminal, Write};

pub const JSON_VERSION: u32 = 1;

/// Write a line to stdout. A closed pipe (`devflow list | head`) is not an error.
pub fn say(text: impl AsRef<str>) {
    let _ = writeln!(std::io::stdout().lock(), "{}", text.as_ref());
}

/// Write text to stdout exactly as given.
pub fn say_raw(text: impl AsRef<str>) {
    let _ = std::io::stdout().lock().write_all(text.as_ref().as_bytes());
}

/// Write a line to stderr (progress, hints, warnings).
pub fn note(text: impl AsRef<str>) {
    let _ = writeln!(std::io::stderr().lock(), "{}", text.as_ref());
}

/// ANSI styling that switches itself off for pipes, `NO_COLOR` and dumb terminals.
#[derive(Clone, Copy)]
pub struct Palette {
    enabled: bool,
}

impl Palette {
    pub fn stdout() -> Self {
        Self::new(std::io::stdout().is_terminal())
    }

    pub fn stderr() -> Self {
        Self::new(std::io::stderr().is_terminal())
    }

    fn new(tty: bool) -> Self {
        let plain = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
            || std::env::var("TERM").is_ok_and(|t| t == "dumb");
        Self {
            enabled: tty && !plain,
        }
    }

    pub fn is_enabled(self) -> bool {
        self.enabled
    }

    fn paint(self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub fn green(self, text: &str) -> String {
        self.paint("32", text)
    }

    pub fn yellow(self, text: &str) -> String {
        self.paint("33", text)
    }

    pub fn red(self, text: &str) -> String {
        self.paint("31", text)
    }

    pub fn dim(self, text: &str) -> String {
        self.paint("2", text)
    }
}

/// Left-aligned columns separated by two spaces; the last column is not padded.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let render = |cells: &[&str]| {
        let mut line = String::new();
        for (index, (cell, width)) in cells.iter().zip(&widths).enumerate() {
            line.push_str(cell);
            if index + 1 < widths.len() {
                line.push_str(&" ".repeat(width - cell.chars().count() + 2));
            }
        }
        line.trim_end().to_string()
    };
    let mut lines = vec![render(headers)];
    for row in rows {
        let cells: Vec<&str> = row.iter().map(String::as_str).collect();
        lines.push(render(&cells));
    }
    lines.join("\n")
}

/// Aligned `key  value` lines.
pub fn key_values(pairs: &[(&str, String)]) -> String {
    let width = pairs
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0);
    pairs
        .iter()
        .map(|(key, value)| format!("{key:<width$}  {value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn bytes(size: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = size.max(0) as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{size} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// The snake_case name a unit enum serializes to (`needs_attention`, `verified`).
pub fn snake<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(Value::String(text)) => text,
        _ => String::new(),
    }
}

/// Replace `$HOME` with `~` for display.
pub fn tilde(path: &std::path::Path) -> String {
    if let Some(home) = std::env::var_os("HOME")
        && let Ok(rest) = path.strip_prefix(home)
    {
        return if rest.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", rest.display())
        };
    }
    path.display().to_string()
}

// --- JSON ---------------------------------------------------------------

#[derive(Serialize)]
struct Envelope<'a, T: Serialize> {
    version: u32,
    command: &'a str,
    #[serde(flatten)]
    body: T,
}

/// Print one `{"version": 1, "command": ..., ...}` document with secrets redacted.
pub fn emit<T: Serialize>(ctx: &Ctx, command: &str, body: &T) -> Result<()> {
    print_doc(ctx.take_warnings(), command, body, true)
}

/// Like [`emit`] for the commands whose purpose is handing out credentials
/// (`db url`, `db branch create`).
pub fn emit_unredacted<T: Serialize>(ctx: &Ctx, command: &str, body: &T) -> Result<()> {
    print_doc(ctx.take_warnings(), command, body, false)
}

/// Failures in `--json` mode still produce one document on stdout. Needs no
/// [`Ctx`]: argument and context errors happen before one exists.
pub fn emit_error(warnings: Vec<String>, command: &str, message: &str, hint: Option<&str>) {
    let mut body = serde_json::json!({ "error": message });
    if let Some(hint) = hint {
        body["hint"] = Value::String(hint.to_string());
    }
    let _ = print_doc(warnings, command, &body, true);
}

fn print_doc<T: Serialize>(
    warnings: Vec<String>,
    command: &str,
    body: &T,
    redact_secrets: bool,
) -> Result<()> {
    let mut doc = serde_json::to_value(Envelope {
        version: JSON_VERSION,
        command,
        body,
    })?;
    if !warnings.is_empty()
        && let Value::Object(map) = &mut doc
    {
        let entry = map
            .entry("warnings")
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Value::Array(list) = entry {
            list.extend(warnings.into_iter().map(Value::String));
        }
    }
    if redact_secrets {
        redact::json(&mut doc);
    }
    say(serde_json::to_string_pretty(&doc)?);
    Ok(())
}

// --- progress -----------------------------------------------------------

/// Prints setup phases and hook output to stderr. Phase lines are suppressed in
/// `--json` mode; hook output is always shown.
pub struct Printer {
    json: bool,
    palette: Palette,
    announce_database: bool,
}

impl Printer {
    pub fn new(ctx: &Ctx) -> Self {
        Self {
            json: ctx.json,
            palette: Palette::stderr(),
            announce_database: false,
        }
    }

    /// Announce the start of the database phase: starting a server can take a
    /// while. Only worth showing when a database will actually be provisioned.
    pub fn announcing_database(mut self, announce: bool) -> Self {
        self.announce_database = announce;
        self
    }
}

impl Progress for Printer {
    fn phase(&mut self, phase: Phase, state: PhaseState, detail: Option<&str>) {
        if self.json {
            return;
        }
        let symbol = match state {
            PhaseState::Pending => return,
            PhaseState::Started if phase == Phase::Services && self.announce_database => {
                self.palette.dim("…")
            }
            PhaseState::Started => return,
            PhaseState::Done => self.palette.green("✓"),
            PhaseState::Skipped => self.palette.dim("-"),
            PhaseState::Failed => self.palette.red("✗"),
        };
        let mut line = format!("{symbol} {:<11}", phase.as_str());
        if let Some(detail) = detail.filter(|d| !d.is_empty()) {
            line.push(' ');
            line.push_str(&redact::text(detail));
        }
        note(line.trim_end());
    }

    fn output(&mut self, line: &str) {
        note(format!("{} {}", self.palette.dim("│"), redact::text(line)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_aligned_tables() {
        let rows = vec![
            vec!["a".to_string(), "long value".to_string()],
            vec!["bbb".to_string(), "x".to_string()],
        ];
        assert_eq!(
            table(&["NAME", "V"], &rows),
            "NAME  V\na     long value\nbbb   x"
        );
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(bytes(12), "12 B");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(392 * 1024 * 1024), "392.0 MiB");
    }
}
