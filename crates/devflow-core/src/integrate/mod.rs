//! Agent-host integrations: the files that make T3 Code, Claude Code,
//! Conductor and Cursor set a worktree up with devflow (and, where the host
//! has a hook for it, take it down).
//!
//! Each host reads a small project file. devflow edits it **additively**: JSON
//! and TOML keep what is there (key order, comments in TOML, everything it does
//! not own), a script somebody else wrote is **never overwritten** (it is
//! reported as a [`Conflict`] together with the line to add by hand), and
//! nothing is written until the caller has seen the [`Change`]. What a host
//! calls is verified against its current documentation, and noted next to the
//! code that depends on it:
//!
//! | Host | File | Contract |
//! | --- | --- | --- |
//! | T3 Code | `t3.json` | `T3ProjectFile` in `packages/contracts/src/t3ProjectFile.ts` of pingdotgg/t3code: `scripts[]` with `name`, `command`, `icon`, `runOnWorktreeCreate`, `async` (false holds the agent until the script exits). The script runs in the new worktree and T3 Code uses the first script with `runOnWorktreeCreate`. Users import the file's scripts into the project. |
//! | Claude Code | `.claude/settings.json` | `hooks.WorktreeCreate` replaces Git's creation: `{"name": …}` arrives on stdin, the path is the last non-empty stdout line, a non-zero exit fails the creation. `hooks.WorktreeRemove` receives `worktree_path` and must delete the directory (exit 0 counts as removed); it fires only for worktrees a `WorktreeCreate` hook made. |
//! | Conductor | `.conductor/settings.toml` | `[scripts]` `setup` (after creating a workspace) and `archive` (before archiving), run in the workspace with `CONDUCTOR_WORKSPACE_PATH`, `CONDUCTOR_ROOT_PATH`, `CONDUCTOR_WORKSPACE_NAME` and `CONDUCTOR_PORT` (the first of ten ports). |
//! | Cursor | `.cursor/worktrees.json` | `setup-worktree` (or `setup-worktree-unix` / `-windows`, which win on their system): an array of commands run in the worktree, or a path to a script. No teardown. |
//!
//! Codex has a local-environment setup script under `.codex/`, but its file
//! format is not documented, so devflow writes nothing there; its worktrees
//! are detached HEAD and are adopted under their directory name.
use crate::project::Project;
use crate::remedy::{Problem, Remedy};
use anyhow::Result;
use serde::Serialize;
use std::path::{Path, PathBuf};

mod claude;
mod confined;
mod hosts;
pub mod json;

pub use claude::{
    ClaudeCreated, ClaudeRemoved, CreateInput, RemoveInput, StdoutGuard, claude_create,
    claude_remove, parse_create_input, parse_remove_input, printable_path, workspace_name,
};

/// The command every host's setup script runs.
pub const SETUP_COMMAND: &str = "devflow setup --wait";
/// The name of the script devflow adds to `t3.json`.
pub const T3_SCRIPT_NAME: &str = "devflow setup";
/// What Claude Code's `WorktreeCreate` hook runs.
pub const CLAUDE_CREATE_COMMAND: &str = "devflow integrate claude-create";
/// What Claude Code's `WorktreeRemove` hook runs.
pub const CLAUDE_REMOVE_COMMAND: &str = "devflow integrate claude-remove";
/// Conductor's `archive` script: remove the workspace (the Git branch stays).
pub const CONDUCTOR_ARCHIVE_COMMAND: &str =
    r#"devflow -C "$CONDUCTOR_ROOT_PATH" remove "$CONDUCTOR_WORKSPACE_PATH""#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Host {
    T3,
    Claude,
    Conductor,
    Cursor,
}

impl Host {
    pub const ALL: [Host; 4] = [Host::T3, Host::Claude, Host::Conductor, Host::Cursor];

    /// The name on the command line and in JSON.
    pub fn name(self) -> &'static str {
        match self {
            Host::T3 => "t3",
            Host::Claude => "claude",
            Host::Conductor => "conductor",
            Host::Cursor => "cursor",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Host::T3 => "T3 Code",
            Host::Claude => "Claude Code",
            Host::Conductor => "Conductor",
            Host::Cursor => "Cursor",
        }
    }

    pub fn parse(name: &str) -> Option<Host> {
        Host::ALL.into_iter().find(|host| host.name() == name)
    }

    /// The one file the integration edits, relative to the main checkout.
    pub fn file(self) -> &'static str {
        match self {
            Host::T3 => "t3.json",
            Host::Claude => ".claude/settings.json",
            Host::Conductor => ".conductor/settings.toml",
            Host::Cursor => ".cursor/worktrees.json",
        }
    }

    /// Paths whose presence says the repository is used with the host.
    pub fn markers(self) -> &'static [&'static str] {
        match self {
            Host::T3 => &["t3.json"],
            Host::Claude => &[".claude"],
            Host::Conductor => &[".conductor", "conductor.json"],
            Host::Cursor => &[".cursor"],
        }
    }
}

/// Something worth knowing about an integration that is not a problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Note {
    /// The host has no hook that runs when a workspace is deleted.
    NoTeardown(Host),
    /// Cursor deletes worktrees past its cap, ones it did not create included.
    CursorCleansUp,
    /// Conductor's port becomes the workspace's app port.
    ConductorPort,
    /// Claude Code does not read `.worktreeinclude` when a hook creates the
    /// worktree; devflow does.
    WorktreeInclude,
    /// Removal keeps the Git branch.
    BranchKept,
    /// The file is for the whole team.
    Commit,
}

impl Note {
    /// One sentence; names the host's concepts, never a command line.
    pub fn describe(self) -> &'static str {
        match self {
            Note::NoTeardown(Host::T3) => {
                "T3 Code has no hook that runs when it deletes a worktree: the data, containers and daemons a deleted workspace leaves are found again and cleaned up by gc"
            }
            Note::NoTeardown(_) => {
                "this host has no hook that runs when it deletes a worktree: what a deleted workspace leaves is found again and cleaned up by gc"
            }
            Note::CursorCleansUp => {
                "Cursor deletes worktrees past its limit (25 by default), even ones it did not create; gc finds what they leave behind"
            }
            Note::ConductorPort => {
                "a CONDUCTOR_PORT present when a workspace is created becomes the workspace's app port (DEVFLOW_PORT)"
            }
            Note::WorktreeInclude => {
                "Claude Code does not read .worktreeinclude when a hook creates the worktree, so devflow does: matching ignored files are cloned with the [worktree] copy entries"
            }
            Note::BranchKept => {
                "removing a workspace keeps its Git branch; the branch is deleted by the person"
            }
            Note::Commit => "commit the file so everyone who uses the host gets the setup",
        }
    }
}

/// An existing entry that stands in the way. Names the script or key, never
/// what it holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Conflict {
    /// What is in the way ("script \"Setup Worktree\"", "scripts.setup").
    pub item: String,
    /// Why it was left alone.
    pub reason: String,
}

impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.item, self.reason)
    }
}

/// What an integration would do to a host's file.
#[derive(Debug, Clone, Serialize)]
pub struct Change {
    pub host: Host,
    /// Taking the integration out instead of putting it in.
    pub remove: bool,
    /// The checkout, held open from the plan to the write (never serialized).
    #[serde(skip)]
    checkout: confined::Checkout,
    /// Values the file holds under secret-looking keys, masked wherever the
    /// change is shown.
    #[serde(skip)]
    secrets: Vec<String>,
    pub file: PathBuf,
    /// The file relative to the main checkout.
    pub relative: &'static str,
    /// The file as read; `None` when it does not exist. Whatever the file
    /// holds, so it is never serialized and shown only through [`Change::diff`].
    #[serde(skip)]
    pub before: Option<String>,
    /// The file as it would be written; equal to `before` when nothing changes.
    #[serde(skip)]
    pub after: String,
    /// Entries that were left alone, with why. The rest of the change is
    /// still made.
    pub conflicts: Vec<Conflict>,
    /// The lines to put into the script that is in the way, by hand.
    pub manual: Vec<String>,
    pub notes: Vec<Note>,
}

impl Change {
    /// Whether applying it writes the file.
    pub fn writes(&self) -> bool {
        match &self.before {
            // Taking an entry out of a file that is not there changes nothing.
            None => !self.after.is_empty(),
            Some(before) => *before != self.after,
        }
    }

    /// Whether the host already has what was asked (nothing to write and
    /// nothing in the way).
    pub fn is_done(&self) -> bool {
        !self.writes() && self.conflicts.is_empty()
    }

    /// The change as a line diff, for showing before writing. A file keeps
    /// what it holds (a script may carry a URL with a password), so the texts
    /// are masked **whole, before they are split into lines**: a secret that
    /// spans lines (a TOML multi-line string) is one span, not a first line
    /// that carries the key and later lines that do not. What is masked: every
    /// string the file holds under a secret-looking key (and, in an
    /// environment map, under a name [`crate::redact::is_secret_variable`]
    /// recognises), in the forms the file writes it, and whatever the redaction
    /// patterns recognise. The write uses the original text.
    pub fn diff(&self) -> Vec<crate::propose::DiffLine> {
        let secrets: Vec<&str> = self.secrets.iter().map(String::as_str).collect();
        let mask = |text: &str| crate::redact::text_with(text, &secrets);
        crate::propose::diff_lines(
            &mask(self.before.as_deref().unwrap_or("")),
            &mask(&self.after),
            3,
        )
    }
}

/// The edit a host's editor decided on.
pub(crate) struct Edit {
    /// The new text; `None` leaves the file as it is.
    pub after: Option<String>,
    pub conflicts: Vec<Conflict>,
    pub manual: Vec<String>,
}

impl Edit {
    pub(crate) fn unchanged() -> Edit {
        Edit {
            after: None,
            conflicts: Vec::new(),
            manual: Vec::new(),
        }
    }
}

/// The edit failed for a reason about the file, not about devflow.
pub(crate) fn unreadable(file: &Path, why: impl std::fmt::Display) -> anyhow::Error {
    Problem::new(format!(
        "{} cannot be edited: {why}; devflow does not rewrite a file it cannot read",
        file.display()
    ))
    .with(Remedy::OpenFile {
        path: file.to_path_buf(),
    })
    .into_error()
}

/// Plan putting `host`'s integration into the checkout at `root` (or taking it
/// out). Reads, never writes.
pub fn plan(root: &Path, host: Host, remove: bool) -> Result<Change> {
    let relative = host.file();
    let file = root.join(relative);
    // Read through descriptors confined to the checkout: a link anywhere on
    // the way is refused.
    let checkout = confined::Checkout::open(root)?;
    let before = confined::read(&checkout, relative)?;
    let edit = match host {
        Host::T3 => hosts::t3(&file, before.as_deref(), remove)?,
        Host::Claude => hosts::claude(&file, before.as_deref(), remove)?,
        Host::Conductor => hosts::conductor(&file, before.as_deref(), remove)?,
        Host::Cursor => hosts::cursor(&file, before.as_deref(), remove)?,
    };
    let after = match edit.after {
        Some(after) => after,
        None => before.clone().unwrap_or_default(),
    };
    let notes = if remove {
        Vec::new()
    } else {
        let mut notes = match host {
            Host::T3 => vec![Note::NoTeardown(Host::T3)],
            Host::Claude => vec![Note::WorktreeInclude, Note::BranchKept],
            Host::Conductor => vec![Note::ConductorPort, Note::BranchKept],
            Host::Cursor => vec![Note::NoTeardown(Host::Cursor), Note::CursorCleansUp],
        };
        notes.push(Note::Commit);
        notes
    };
    let secrets = before.as_deref().map(known_secrets).unwrap_or_default();
    Ok(Change {
        host,
        remove,
        checkout,
        secrets,
        file,
        relative,
        before,
        after,
        conflicts: edit.conflicts,
        manual: edit.manual,
        notes,
    })
}

/// Write a planned change. Nothing is written when the file changed since
/// [`plan`] read it (the caller plans again), and the write replaces the file
/// atomically. Returns whether the file was written.
pub fn apply(change: &Change) -> Result<bool> {
    if !change.writes() {
        return Ok(false);
    }
    let expected = change
        .before
        .as_ref()
        .map(|before| crate::fsutil::sha256_hex(before.as_bytes()));
    // Every directory on the way is looked at again and opened from its
    // parent, here, whatever was checked when the change was planned.
    confined::write(
        &change.checkout,
        change.relative,
        expected.as_deref(),
        change.after.as_bytes(),
    )?;
    Ok(true)
}

/// Whether an object key names a map of environment variables.
fn is_environment(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    ["env", "environment", "vars", "variables"]
        .iter()
        .any(|word| {
            key == *word || key.ends_with(&format!("_{word}")) || key.ends_with(&format!("-{word}"))
        })
}

/// The secrets a host file holds: every string under a secret-looking key (all
/// the strings below it when its value is a list or a table), in an
/// environment map judged by the variable classifier, and the `value` of a
/// `{name, value}` pair whose name is such a variable. Each is returned as it
/// is and as the file would write it (escaped), and a multi-line secret also
/// by its longer lines, for a file that folds a string.
fn known_secrets(text: &str) -> Vec<String> {
    use json::Json;
    fn all_json(value: &Json, out: &mut Vec<String>) {
        match value {
            Json::String(text) => out.push(text.clone()),
            Json::Array(items) => items.iter().for_each(|item| all_json(item, out)),
            Json::Object(members) => members.iter().for_each(|(_, v)| all_json(v, out)),
            _ => {}
        }
    }
    fn walk_json(value: &Json, env: bool, out: &mut Vec<String>) {
        match value {
            Json::Object(members) => {
                // `{"name": "DB_PASS", "value": "…"}`
                if let (Some(Json::String(name)), Some(Json::String(v))) =
                    (value.get("name"), value.get("value"))
                    && crate::redact::is_secret_variable(name)
                {
                    out.push(v.clone());
                }
                for (key, member) in members {
                    let secret = if env {
                        crate::redact::is_secret_variable(key)
                    } else {
                        crate::redact::is_secret_key(key)
                    };
                    if secret {
                        all_json(member, out);
                    } else {
                        walk_json(member, is_environment(key), out);
                    }
                }
            }
            Json::Array(items) => items.iter().for_each(|item| walk_json(item, env, out)),
            _ => {}
        }
    }
    fn all_toml(value: &toml::Value, out: &mut Vec<String>) {
        match value {
            toml::Value::String(text) => out.push(text.clone()),
            toml::Value::Array(items) => items.iter().for_each(|item| all_toml(item, out)),
            toml::Value::Table(table) => table.values().for_each(|v| all_toml(v, out)),
            _ => {}
        }
    }
    fn walk_toml(value: &toml::Value, env: bool, out: &mut Vec<String>) {
        match value {
            toml::Value::Table(table) => {
                if let (Some(toml::Value::String(name)), Some(toml::Value::String(v))) =
                    (table.get("name"), table.get("value"))
                    && crate::redact::is_secret_variable(name)
                {
                    out.push(v.clone());
                }
                for (key, member) in table {
                    let secret = if env {
                        crate::redact::is_secret_variable(key)
                    } else {
                        crate::redact::is_secret_key(key)
                    };
                    if secret {
                        all_toml(member, out);
                    } else {
                        walk_toml(member, is_environment(key), out);
                    }
                }
            }
            toml::Value::Array(items) => items.iter().for_each(|item| walk_toml(item, env, out)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    if let Ok(value) = Json::parse(text) {
        walk_json(&value, false, &mut found);
    } else if let Ok(value) = text.parse::<toml::Table>() {
        walk_toml(&toml::Value::Table(value), false, &mut found);
    }
    let mut out: Vec<String> = Vec::new();
    for secret in found.into_iter().filter(|s| !s.is_empty()) {
        // As written in a JSON or TOML basic string.
        let escaped = secret
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t");
        if secret.contains('\n') {
            out.extend(
                secret
                    .lines()
                    .map(str::trim)
                    .filter(|line| line.len() >= 6)
                    .map(str::to_string),
            );
        }
        out.push(escaped);
        out.push(secret);
    }
    out.sort();
    out.dedup();
    out
}

/// How a repository stands with one host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// devflow's entries are in the file.
    Configured,
    /// Not there (yet).
    NotConfigured,
    /// An entry of someone else's stands in the way of devflow's.
    Conflict,
}

/// A host the repository is used with.
#[derive(Debug, Clone, Serialize)]
pub struct Detected {
    pub host: Host,
    /// The paths that say so, relative to the main checkout.
    pub found: Vec<String>,
    pub state: State,
}

/// The hosts `root` shows signs of (their files or folders exist), and how
/// each stands. A host whose file cannot be read is `not_configured`: the
/// integration command says why.
pub fn detect(root: &Path) -> Vec<Detected> {
    Host::ALL
        .into_iter()
        .filter_map(|host| {
            let found: Vec<String> = host
                .markers()
                .iter()
                .filter(|marker| root.join(marker).exists())
                .map(|marker| marker.to_string())
                .collect();
            if found.is_empty() {
                return None;
            }
            Some(Detected {
                host,
                found,
                state: state_of(root, host),
            })
        })
        .collect()
}

/// How `root` stands with `host`, whether or not the repository shows signs of it.
pub fn state_of(root: &Path, host: Host) -> State {
    match plan(root, host, false) {
        Ok(change) if change.is_done() => State::Configured,
        Ok(change) if !change.conflicts.is_empty() => State::Conflict,
        _ => State::NotConfigured,
    }
}

/// The hosts the project's main checkout shows signs of and that are not set up.
pub fn suggestions(project: &Project) -> Vec<Detected> {
    detect(&project.root)
        .into_iter()
        .filter(|detected| detected.state != State::Configured)
        .collect()
}

#[cfg(test)]
mod tests;
