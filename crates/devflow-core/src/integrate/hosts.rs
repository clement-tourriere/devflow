//! The editors: one function per host, from the file as it is to the file as
//! devflow wants it. Each keeps what it does not own and reports, instead of
//! replacing, what stands in its way.
use super::json::Json;
use super::{
    CLAUDE_CREATE_COMMAND, CLAUDE_REMOVE_COMMAND, CONDUCTOR_ARCHIVE_COMMAND, Conflict, Edit,
    SETUP_COMMAND, T3_SCRIPT_NAME, unreadable,
};
use anyhow::Result;
use std::path::Path;

fn conflict(item: impl Into<String>, reason: impl Into<String>) -> Conflict {
    Conflict {
        item: item.into(),
        reason: reason.into(),
    }
}

fn parse_object(file: &Path, before: Option<&str>) -> Result<Json> {
    let Some(text) = before else {
        return Ok(Json::object());
    };
    let value = Json::parse(text).map_err(|error| unreadable(file, error))?;
    if !value.is_object() {
        return Err(unreadable(file, "its top level is not an object"));
    }
    Ok(value)
}

// --- T3 Code -------------------------------------------------------------

/// `t3.json`: one script that runs when a worktree is created for a thread
/// and holds the agent until it exits (`async: false`).
///
/// T3 Code runs the first script marked `runOnWorktreeCreate`, so a script of
/// somebody else's with the flag is in the way even though it has another name.
pub(super) fn t3(file: &Path, before: Option<&str>, remove: bool) -> Result<Edit> {
    let mut root = parse_object(file, before)?;
    let create = before.is_none();
    let is_ours = |script: &Json| script.get("name").and_then(Json::as_str) == Some(T3_SCRIPT_NAME);
    let as_wanted = |script: &Json| {
        script.get("command").and_then(Json::as_str) == Some(SETUP_COMMAND)
            && script.get("runOnWorktreeCreate").and_then(Json::as_bool) == Some(true)
            && script.get("async").and_then(Json::as_bool) == Some(false)
    };
    let item = format!("script \"{T3_SCRIPT_NAME}\"");
    let scripts = match root.get("scripts") {
        None => Vec::new(),
        Some(Json::Array(items)) => items.clone(),
        Some(_) => {
            return Err(unreadable(file, "\"scripts\" is not a list"));
        }
    };
    let mut edit = Edit::unchanged();
    let ours = scripts.iter().position(is_ours);
    if remove {
        let Some(at) = ours else {
            return Ok(edit);
        };
        if !as_wanted(&scripts[at]) {
            edit.conflicts.push(conflict(
                item,
                "it was edited, so it is left in place; delete it by hand",
            ));
            return Ok(edit);
        }
        if let Some(list) = root.get_mut("scripts").and_then(Json::as_array_mut) {
            list.remove(at);
            if list.is_empty() {
                root.remove("scripts");
            }
        }
        edit.after = Some(root.to_pretty());
        return Ok(edit);
    }
    if let Some(at) = ours {
        if !as_wanted(&scripts[at]) {
            edit.conflicts.push(conflict(
                item,
                "a script with this name exists and is not the one devflow writes, so it is left alone",
            ));
            edit.manual.push(SETUP_COMMAND.to_string());
        }
        return Ok(edit);
    }
    if let Some(other) = scripts
        .iter()
        .find(|script| script.get("runOnWorktreeCreate").and_then(Json::as_bool) == Some(true))
    {
        let name = other
            .get("name")
            .and_then(Json::as_str)
            .unwrap_or("(unnamed)");
        edit.conflicts.push(conflict(
            format!("script \"{name}\""),
            "it already runs when a worktree is created, and T3 Code runs only the first such script",
        ));
        edit.manual.push(SETUP_COMMAND.to_string());
        return Ok(edit);
    }
    if scripts.len() >= 50 {
        edit.conflicts
            .push(conflict("scripts", "t3.json holds at most 50 scripts"));
        return Ok(edit);
    }
    let mut script = Json::object();
    script.set("name", Json::string(T3_SCRIPT_NAME));
    script.set("command", Json::string(SETUP_COMMAND));
    script.set("icon", Json::string("configure"));
    script.set("runOnWorktreeCreate", Json::Bool(true));
    script.set("async", Json::Bool(false));
    if create {
        root.set("$schema", Json::string("https://t3.codes/schema/t3.json"));
    }
    match root.get_mut("scripts").and_then(Json::as_array_mut) {
        Some(list) => list.push(script),
        None => root.set("scripts", Json::Array(vec![script])),
    }
    edit.after = Some(root.to_pretty());
    Ok(edit)
}

// --- Claude Code ---------------------------------------------------------

/// The handlers of one hook event, flattened: (group index, handler index,
/// the command when it is a `command` handler).
fn handlers(event: &Json) -> Vec<(usize, usize, Option<&str>)> {
    let mut found = Vec::new();
    for (group_at, group) in event.as_array().into_iter().flatten().enumerate() {
        for (handler_at, handler) in group
            .get("hooks")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let command = (handler.get("type").and_then(Json::as_str) == Some("command"))
                .then(|| handler.get("command").and_then(Json::as_str))
                .flatten();
            found.push((group_at, handler_at, command));
        }
    }
    found
}

/// `.claude/settings.json`: `WorktreeCreate` creates the workspace through
/// devflow and `WorktreeRemove` takes it down. A worktree has one creator, so
/// a hook of somebody else's on either event is in the way of both: nothing is
/// written then.
pub(super) fn claude(file: &Path, before: Option<&str>, remove: bool) -> Result<Edit> {
    let mut root = parse_object(file, before)?;
    let events = [
        ("WorktreeCreate", CLAUDE_CREATE_COMMAND),
        ("WorktreeRemove", CLAUDE_REMOVE_COMMAND),
    ];
    if let Some(hooks) = root.get("hooks")
        && !hooks.is_object()
    {
        return Err(unreadable(file, "\"hooks\" is not an object"));
    }
    let mut edit = Edit::unchanged();
    if remove {
        let mut changed = false;
        for (event, ours) in events {
            let Some(hooks) = root.get_mut("hooks") else {
                break;
            };
            let Some(list) = hooks.get_mut(event) else {
                continue;
            };
            // Drop our handlers; a group left without handlers goes too.
            let mut drops: Vec<(usize, usize)> = handlers(list)
                .into_iter()
                .filter(|(_, _, command)| *command == Some(ours))
                .map(|(group, handler, _)| (group, handler))
                .collect();
            drops.sort_unstable_by(|a, b| b.cmp(a));
            if drops.is_empty() {
                continue;
            }
            changed = true;
            if let Some(groups) = list.as_array_mut() {
                for (group, handler) in drops {
                    if let Some(inner) = groups
                        .get_mut(group)
                        .and_then(|g| g.get_mut("hooks"))
                        .and_then(Json::as_array_mut)
                    {
                        inner.remove(handler);
                    }
                }
                groups.retain(|group| {
                    !group
                        .get("hooks")
                        .and_then(Json::as_array)
                        .is_some_and(Vec::is_empty)
                });
            }
            if list.is_empty_container() {
                hooks.remove(event);
            }
        }
        if root.get("hooks").is_some_and(Json::is_empty_container) {
            root.remove("hooks");
        }
        if changed {
            edit.after = Some(root.to_pretty());
        }
        return Ok(edit);
    }
    let mut missing = Vec::new();
    for (event, ours) in events {
        let existing = root.get("hooks").and_then(|hooks| hooks.get(event));
        match existing {
            None => missing.push((event, ours)),
            Some(list) if !matches!(list, Json::Array(_)) => {
                return Err(unreadable(file, format!("\"hooks.{event}\" is not a list")));
            }
            Some(list) => {
                let all = handlers(list);
                if all.iter().any(|(_, _, command)| *command != Some(ours)) {
                    edit.conflicts.push(conflict(
                        format!("hooks.{event}"),
                        "another hook is set for this event, so devflow's is not added",
                    ));
                } else if all.is_empty() {
                    missing.push((event, ours));
                }
            }
        }
    }
    if !edit.conflicts.is_empty() {
        edit.manual = vec![
            format!("WorktreeCreate: {CLAUDE_CREATE_COMMAND}"),
            format!("WorktreeRemove: {CLAUDE_REMOVE_COMMAND}"),
        ];
        return Ok(edit);
    }
    if missing.is_empty() {
        return Ok(edit);
    }
    if root.get("hooks").is_none() {
        root.set("hooks", Json::object());
    }
    for (event, ours) in missing {
        let mut handler = Json::object();
        handler.set("type", Json::string("command"));
        handler.set("command", Json::string(ours));
        let mut group = Json::object();
        group.set("hooks", Json::Array(vec![handler]));
        let Some(hooks) = root.get_mut("hooks") else {
            continue;
        };
        match hooks.get_mut(event).and_then(Json::as_array_mut) {
            Some(list) => list.push(group),
            None => hooks.set(event, Json::Array(vec![group])),
        }
    }
    edit.after = Some(root.to_pretty());
    Ok(edit)
}

// --- Conductor -----------------------------------------------------------

/// `.conductor/settings.toml`: `[scripts] setup` and `archive`, each added
/// when absent and reported when something else is there. Edited with
/// `toml_edit`, so comments and layout stay.
pub(super) fn conductor(file: &Path, before: Option<&str>, remove: bool) -> Result<Edit> {
    let mut doc: toml_edit::DocumentMut =
        before
            .unwrap_or("")
            .parse()
            .map_err(|error: toml_edit::TomlError| {
                let span = error.span().map(|s| s.start).unwrap_or(0);
                let text = before.unwrap_or("");
                let line = text[..span.min(text.len())].matches('\n').count() + 1;
                unreadable(file, format!("it is not valid TOML (line {line})"))
            })?;
    let wanted = [
        ("setup", SETUP_COMMAND),
        ("archive", CONDUCTOR_ARCHIVE_COMMAND),
    ];
    let mut edit = Edit::unchanged();
    let mut changed = false;
    let existing = |doc: &toml_edit::DocumentMut, key: &str| -> Option<Option<String>> {
        let scripts = doc.get("scripts")?;
        let item = scripts.as_table_like()?.get(key)?;
        Some(item.as_str().map(str::to_string))
    };
    if doc
        .get("scripts")
        .is_some_and(|scripts| scripts.as_table_like().is_none())
    {
        return Err(unreadable(file, "\"scripts\" is not a table"));
    }
    for (key, ours) in wanted {
        let item = format!("scripts.{key}");
        match existing(&doc, key) {
            Some(Some(text)) if text == ours => {
                if remove {
                    if let Some(table) = doc.get_mut("scripts").and_then(|s| s.as_table_like_mut())
                    {
                        table.remove(key);
                    }
                    changed = true;
                }
            }
            Some(_) if remove => {
                edit.conflicts.push(conflict(
                    item,
                    "it is not the script devflow writes, so it is left in place",
                ));
            }
            Some(_) => {
                edit.conflicts.push(conflict(
                    item,
                    "a script is already set, so devflow's is not added",
                ));
                edit.manual.push(ours.to_string());
            }
            None if remove => {}
            None => {
                if doc.get("scripts").is_none() {
                    doc["scripts"] = toml_edit::table();
                }
                if let Some(table) = doc.get_mut("scripts").and_then(|s| s.as_table_like_mut()) {
                    table.insert(key, toml_edit::value(ours));
                }
                changed = true;
            }
        }
    }
    if remove
        && changed
        && doc
            .get("scripts")
            .and_then(|s| s.as_table_like())
            .is_some_and(|table| table.is_empty())
    {
        doc.remove("scripts");
    }
    if changed {
        edit.after = Some(doc.to_string());
    }
    Ok(edit)
}

// --- Cursor --------------------------------------------------------------

/// `.cursor/worktrees.json`: the command goes first in the array of
/// `setup-worktree` (and of `setup-worktree-unix` when that exists, since it
/// wins on macOS and Linux). A key whose value is a path to a script cannot be
/// added to and is reported.
pub(super) fn cursor(file: &Path, before: Option<&str>, remove: bool) -> Result<Edit> {
    let mut root = parse_object(file, before)?;
    const KEYS: [&str; 2] = ["setup-worktree-unix", "setup-worktree"];
    let present: Vec<&str> = KEYS
        .into_iter()
        .filter(|key| root.get(key).is_some())
        .collect();
    let mut edit = Edit::unchanged();
    let mut changed = false;
    if remove {
        for key in present {
            let Some(list) = root.get_mut(key).and_then(Json::as_array_mut) else {
                continue;
            };
            let before_len = list.len();
            list.retain(|command| command.as_str() != Some(SETUP_COMMAND));
            if list.len() != before_len {
                changed = true;
                if list.is_empty() {
                    root.remove(key);
                }
            }
        }
        if changed {
            edit.after = Some(root.to_pretty());
        }
        return Ok(edit);
    }
    let targets: Vec<&str> = if present.is_empty() {
        vec!["setup-worktree"]
    } else {
        present
    };
    for key in targets {
        match root.get_mut(key) {
            None => {
                root.set(key, Json::Array(vec![Json::string(SETUP_COMMAND)]));
                changed = true;
            }
            Some(Json::Array(list)) => {
                if !list.iter().any(|c| c.as_str() == Some(SETUP_COMMAND)) {
                    list.insert(0, Json::string(SETUP_COMMAND));
                    changed = true;
                }
            }
            Some(_) => {
                edit.conflicts.push(conflict(
                    key,
                    "it points at a script file, which devflow does not edit",
                ));
                edit.manual.push(SETUP_COMMAND.to_string());
            }
        }
    }
    if changed {
        edit.after = Some(root.to_pretty());
    }
    Ok(edit)
}
