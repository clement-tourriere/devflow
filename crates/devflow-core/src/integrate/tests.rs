use super::json::Json;
use super::*;
use std::path::Path;

fn root() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn read(root: &Path, relative: &str) -> String {
    std::fs::read_to_string(root.join(relative)).unwrap()
}

fn done(root: &Path, host: Host) -> Change {
    let change = plan(root, host, false).unwrap();
    assert!(apply(&change).unwrap() || !change.writes());
    change
}

/// Planning again after applying finds nothing to do.
fn is_stable(root: &Path, host: Host) {
    let again = plan(root, host, false).unwrap();
    assert!(again.is_done(), "{:?}: {:?}", again.conflicts, again.after);
}

#[test]
fn t3_creates_the_file_with_a_script_that_holds_the_agent() {
    let dir = root();
    let change = done(dir.path(), Host::T3);
    assert!(change.before.is_none());
    let value = Json::parse(&read(dir.path(), "t3.json")).unwrap();
    assert_eq!(
        value.get("$schema").and_then(Json::as_str),
        Some("https://t3.codes/schema/t3.json")
    );
    let script = &value.get("scripts").and_then(Json::as_array).unwrap()[0];
    assert_eq!(
        script.get("name").and_then(Json::as_str),
        Some("devflow setup")
    );
    assert_eq!(
        script.get("command").and_then(Json::as_str),
        Some("devflow setup --wait")
    );
    assert_eq!(
        script.get("runOnWorktreeCreate").and_then(Json::as_bool),
        Some(true)
    );
    assert_eq!(script.get("async").and_then(Json::as_bool), Some(false));
    assert!(change.notes.contains(&Note::NoTeardown(Host::T3)));
    is_stable(dir.path(), Host::T3);
}

#[test]
fn t3_keeps_what_is_there_in_the_order_it_is_there() {
    let dir = root();
    write(
        dir.path(),
        "t3.json",
        r#"{"iconPath": "a.svg", "scripts": [{"name": "Test", "command": "npm test", "icon": "test"}], "worktreeSubmodules": "none"}"#,
    );
    done(dir.path(), Host::T3);
    let text = read(dir.path(), "t3.json");
    let value = Json::parse(&text).unwrap();
    // Their keys are in their order, ours is appended to the list.
    let keys: Vec<&str> = match &value {
        Json::Object(members) => members.iter().map(|(k, _)| k.as_str()).collect(),
        _ => unreachable!(),
    };
    assert_eq!(keys, ["iconPath", "scripts", "worktreeSubmodules"]);
    let scripts = value.get("scripts").and_then(Json::as_array).unwrap();
    assert_eq!(scripts.len(), 2);
    assert_eq!(scripts[0].get("name").and_then(Json::as_str), Some("Test"));
    // No $schema is added to a file that did not have one.
    assert!(value.get("$schema").is_none());
    is_stable(dir.path(), Host::T3);

    // Removing takes only our script out.
    let removal = plan(dir.path(), Host::T3, true).unwrap();
    apply(&removal).unwrap();
    let value = Json::parse(&read(dir.path(), "t3.json")).unwrap();
    assert_eq!(
        value.get("scripts").and_then(Json::as_array).unwrap().len(),
        1
    );
    assert!(plan(dir.path(), Host::T3, true).unwrap().is_done());
}

#[test]
fn t3_never_overwrites_a_setup_script_and_says_what_to_add() {
    let dir = root();
    let theirs = r#"{"scripts": [{"name": "Setup Worktree", "command": "node setup.js", "runOnWorktreeCreate": true}]}"#;
    write(dir.path(), "t3.json", theirs);
    let change = plan(dir.path(), Host::T3, false).unwrap();
    assert!(!change.writes());
    assert_eq!(change.conflicts.len(), 1);
    assert!(change.conflicts[0].item.contains("Setup Worktree"));
    assert!(!change.conflicts[0].to_string().contains("setup.js"));
    assert_eq!(change.manual, [SETUP_COMMAND]);
    assert!(!apply(&change).unwrap());
    assert_eq!(read(dir.path(), "t3.json"), theirs);
    assert_eq!(state_of(dir.path(), Host::T3), State::Conflict);

    // A script of ours that was edited is left alone, in both directions.
    write(
        dir.path(),
        "t3.json",
        r#"{"scripts": [{"name": "devflow setup", "command": "devflow setup --wait --skip-mise-check", "runOnWorktreeCreate": true}]}"#,
    );
    for remove in [false, true] {
        let change = plan(dir.path(), Host::T3, remove).unwrap();
        assert!(!change.writes());
        assert_eq!(change.conflicts.len(), 1, "remove={remove}");
    }
}

#[test]
fn files_devflow_cannot_read_are_never_rewritten() {
    let dir = root();
    for (host, text) in [
        (Host::T3, "// comment\n{}"),
        (Host::T3, "[]"),
        (Host::Claude, "{\"hooks\": []}"),
        (Host::Claude, "{\"hooks\": {\"WorktreeCreate\": {}}}"),
        (Host::Cursor, "not json"),
        (Host::Conductor, "[scripts\nsetup = 1"),
    ] {
        write(dir.path(), host.file(), text);
        let error = plan(dir.path(), host, false).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("cannot be edited"), "{host:?}: {message}");
        assert!(message.contains(host.file()), "{message}");
        assert_eq!(read(dir.path(), host.file()), text);
        assert_eq!(
            crate::remedy::remedies_of(&error),
            [Remedy::OpenFile {
                path: dir.path().join(host.file())
            }]
        );
    }
}

#[test]
fn claude_adds_both_hooks_and_keeps_every_other_setting() {
    let dir = root();
    write(
        dir.path(),
        ".claude/settings.json",
        r#"{"permissions": {"allow": ["Bash(ls)"]}, "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "check.sh"}]}]}}"#,
    );
    let change = done(dir.path(), Host::Claude);
    let value = Json::parse(&read(dir.path(), ".claude/settings.json")).unwrap();
    assert!(value.get("permissions").is_some());
    let hooks = value.get("hooks").unwrap();
    assert!(hooks.get("PreToolUse").is_some());
    let create = hooks
        .get("WorktreeCreate")
        .and_then(Json::as_array)
        .unwrap();
    assert_eq!(
        create[0].get("hooks").and_then(Json::as_array).unwrap()[0]
            .get("command")
            .and_then(Json::as_str),
        Some("devflow integrate claude-create")
    );
    assert!(hooks.get("WorktreeRemove").is_some());
    assert!(change.notes.contains(&Note::WorktreeInclude));
    is_stable(dir.path(), Host::Claude);

    let removal = plan(dir.path(), Host::Claude, true).unwrap();
    apply(&removal).unwrap();
    let value = Json::parse(&read(dir.path(), ".claude/settings.json")).unwrap();
    let hooks = value.get("hooks").unwrap();
    assert!(hooks.get("WorktreeCreate").is_none());
    assert!(hooks.get("WorktreeRemove").is_none());
    assert!(hooks.get("PreToolUse").is_some());
    assert!(value.get("permissions").is_some());
}

#[test]
fn claude_writes_nothing_when_another_hook_creates_worktrees() {
    let dir = root();
    let theirs = r#"{"hooks": {"WorktreeCreate": [{"hooks": [{"type": "command", "command": "make-worktree.sh"}]}]}}"#;
    write(dir.path(), ".claude/settings.json", theirs);
    let change = plan(dir.path(), Host::Claude, false).unwrap();
    assert!(!change.writes());
    assert_eq!(change.conflicts.len(), 1);
    assert_eq!(change.conflicts[0].item, "hooks.WorktreeCreate");
    assert!(!change.manual.is_empty());
    assert_eq!(read(dir.path(), ".claude/settings.json"), theirs);

    // A remove hook of theirs blocks the pair as well: a worktree has one
    // creator and one remover.
    write(
        dir.path(),
        ".claude/settings.json",
        r#"{"hooks": {"WorktreeRemove": [{"hooks": [{"type": "http", "url": "http://x"}]}]}}"#,
    );
    let change = plan(dir.path(), Host::Claude, false).unwrap();
    assert!(!change.writes());
    assert_eq!(change.conflicts[0].item, "hooks.WorktreeRemove");
}

#[test]
fn conductor_adds_scripts_keeping_comments_and_reports_the_rest() {
    let dir = root();
    write(
        dir.path(),
        ".conductor/settings.toml",
        "# team settings\n[scripts]\nrun = \"pnpm dev\"\narchive = \"./cleanup.sh\"\n",
    );
    let change = plan(dir.path(), Host::Conductor, false).unwrap();
    assert!(change.writes());
    // The archive script is somebody's: reported, the setup script is added.
    assert_eq!(change.conflicts.len(), 1);
    assert_eq!(change.conflicts[0].item, "scripts.archive");
    apply(&change).unwrap();
    let text = read(dir.path(), ".conductor/settings.toml");
    assert!(text.starts_with("# team settings\n"));
    assert!(text.contains("run = \"pnpm dev\""));
    assert!(text.contains("archive = \"./cleanup.sh\""));
    assert!(text.contains("setup = \"devflow setup --wait\""));
    assert_eq!(state_of(dir.path(), Host::Conductor), State::Conflict);
}

#[test]
fn conductor_creates_both_scripts_and_removes_only_its_own() {
    let dir = root();
    let change = done(dir.path(), Host::Conductor);
    let text = read(dir.path(), ".conductor/settings.toml");
    assert!(text.contains("setup = \"devflow setup --wait\""));
    assert!(text.contains("archive = "));
    assert!(text.contains("$CONDUCTOR_WORKSPACE_PATH"));
    assert!(change.notes.contains(&Note::ConductorPort));
    is_stable(dir.path(), Host::Conductor);

    write(
        dir.path(),
        ".conductor/settings.toml",
        &format!("{text}run = \"pnpm dev\"\n"),
    );
    let removal = plan(dir.path(), Host::Conductor, true).unwrap();
    apply(&removal).unwrap();
    let text = read(dir.path(), ".conductor/settings.toml");
    assert!(!text.contains("devflow"));
    assert!(text.contains("run = \"pnpm dev\""));
}

#[test]
fn cursor_puts_the_command_first_in_every_array_that_applies() {
    let dir = root();
    write(
        dir.path(),
        ".cursor/worktrees.json",
        r#"{"setup-worktree": ["npm ci"], "setup-worktree-unix": ["bun install"]}"#,
    );
    done(dir.path(), Host::Cursor);
    let value = Json::parse(&read(dir.path(), ".cursor/worktrees.json")).unwrap();
    for key in ["setup-worktree", "setup-worktree-unix"] {
        let commands = value.get(key).and_then(Json::as_array).unwrap();
        assert_eq!(commands[0].as_str(), Some("devflow setup --wait"), "{key}");
        assert_eq!(commands.len(), 2);
    }
    is_stable(dir.path(), Host::Cursor);
    apply(&plan(dir.path(), Host::Cursor, true).unwrap()).unwrap();
    let value = Json::parse(&read(dir.path(), ".cursor/worktrees.json")).unwrap();
    assert_eq!(
        value
            .get("setup-worktree")
            .and_then(Json::as_array)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn cursor_reports_a_script_file_it_cannot_add_to() {
    let dir = root();
    write(
        dir.path(),
        ".cursor/worktrees.json",
        r#"{"setup-worktree": "scripts/setup.sh"}"#,
    );
    let change = plan(dir.path(), Host::Cursor, false).unwrap();
    assert!(!change.writes());
    assert_eq!(change.conflicts[0].item, "setup-worktree");
    assert_eq!(change.manual, [SETUP_COMMAND]);

    let empty = root();
    done(empty.path(), Host::Cursor);
    let value = Json::parse(&read(empty.path(), ".cursor/worktrees.json")).unwrap();
    assert_eq!(
        value
            .get("setup-worktree")
            .and_then(Json::as_array)
            .unwrap()[0]
            .as_str(),
        Some("devflow setup --wait")
    );
}

#[test]
fn detection_reads_the_markers_and_the_state() {
    let dir = root();
    assert!(detect(dir.path()).is_empty());
    write(dir.path(), "t3.json", r#"{"scripts": []}"#);
    std::fs::create_dir_all(dir.path().join(".cursor")).unwrap();
    std::fs::create_dir_all(dir.path().join(".codex")).unwrap();
    let found = detect(dir.path());
    let hosts: Vec<Host> = found.iter().map(|d| d.host).collect();
    assert_eq!(hosts, [Host::T3, Host::Cursor], "Codex has no integration");
    assert!(found.iter().all(|d| d.state == State::NotConfigured));
    done(dir.path(), Host::T3);
    assert_eq!(detect(dir.path())[0].state, State::Configured);
}

#[cfg(unix)]
#[test]
fn a_symbolic_link_is_not_written_through() {
    let dir = root();
    let elsewhere = root();
    std::os::unix::fs::symlink(elsewhere.path(), dir.path().join(".cursor")).unwrap();
    let error = plan(dir.path(), Host::Cursor, false).unwrap_err();
    assert!(format!("{error:#}").contains("symbolic link"));
    assert!(
        std::fs::read_dir(elsewhere.path())
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn a_file_that_changed_since_planning_is_not_overwritten() {
    let dir = root();
    write(dir.path(), "t3.json", "{}");
    let change = plan(dir.path(), Host::T3, false).unwrap();
    write(dir.path(), "t3.json", r#"{"iconPath": "x"}"#);
    let error = apply(&change).unwrap_err();
    assert!(format!("{error:#}").contains("changed while"));
    assert_eq!(read(dir.path(), "t3.json"), r#"{"iconPath": "x"}"#);
}

#[test]
fn claude_hook_documents_are_read_strictly() {
    let input = parse_create_input(
        r#"{"session_id":"a","cwd":"/work/shop","hook_event_name":"WorktreeCreate","name":"bold-oak-a3f2"}"#,
    )
    .unwrap();
    assert_eq!(input.name, "bold-oak-a3f2");
    assert_eq!(input.cwd.as_deref(), Some(Path::new("/work/shop")));
    assert!(parse_create_input(r#"{"name": ""}"#).is_err());
    assert!(parse_create_input("not json").is_err());
    assert!(parse_create_input("[]").is_err());
    let removal =
        parse_remove_input(r#"{"hook_event_name":"WorktreeRemove","worktree_path":"/w/x"}"#)
            .unwrap();
    assert_eq!(removal.worktree_path, Path::new("/w/x"));
    assert_eq!(removal.cwd, None);
    assert!(parse_remove_input("{}").is_err());
    // A message about a bad document never quotes it.
    let error = parse_create_input("{\"password\": hunter2").unwrap_err();
    assert!(!format!("{error:#}").contains("hunter2"));
}

#[cfg(unix)]
#[test]
fn a_directory_swapped_for_a_link_after_planning_is_not_written_through() {
    let dir = root();
    let outside = root();
    // Planned with no `.claude` at all, then `.claude` becomes a link to a
    // place outside the checkout while a prompt is open.
    let change = plan(dir.path(), Host::Claude, false).unwrap();
    assert!(change.writes());
    std::os::unix::fs::symlink(outside.path(), dir.path().join(".claude")).unwrap();
    let error = apply(&change).unwrap_err();
    assert!(format!("{error:#}").contains("symbolic link"), "{error:#}");
    assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());

    // The same for a directory that existed when it was planned, and for the
    // file itself.
    let other = root();
    write(other.path(), ".cursor/worktrees.json", "{}");
    let change = plan(other.path(), Host::Cursor, false).unwrap();
    std::fs::remove_dir_all(other.path().join(".cursor")).unwrap();
    std::os::unix::fs::symlink(outside.path(), other.path().join(".cursor")).unwrap();
    assert!(apply(&change).is_err());
    assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());

    let third = root();
    write(third.path(), "t3.json", "{}");
    let change = plan(third.path(), Host::T3, false).unwrap();
    std::fs::remove_file(third.path().join("t3.json")).unwrap();
    let target = outside.path().join("elsewhere.json");
    std::fs::write(&target, "{}").unwrap();
    std::os::unix::fs::symlink(&target, third.path().join("t3.json")).unwrap();
    assert!(apply(&change).is_err());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "{}");
}

#[test]
fn taking_an_integration_out_of_a_file_that_is_not_there_writes_nothing() {
    let dir = root();
    for host in Host::ALL {
        let change = plan(dir.path(), host, true).unwrap();
        assert!(!change.writes(), "{host:?}");
        assert!(change.is_done(), "{host:?}");
        assert!(!apply(&change).unwrap());
        assert!(!dir.path().join(host.file()).exists(), "{host:?}");
    }
    assert!(!dir.path().join(".claude").exists());
}

#[test]
fn the_diff_masks_secrets_but_the_file_keeps_them() {
    let dir = root();
    let theirs = r#"{"scripts": [{"name": "Seed", "command": "psql postgres://app:hunter2pw@db.example/shop -f seed.sql", "token": "tok-9f8e7d6c"}]}"#; // kingfisher:ignore
    write(dir.path(), "t3.json", theirs);
    let change = plan(dir.path(), Host::T3, false).unwrap();
    let shown: String = change
        .diff()
        .iter()
        .map(|l| format!("{}\n", l.text))
        .collect();
    assert!(!shown.contains("hunter2pw"), "{shown}");
    assert!(!shown.contains("tok-9f8e7d6c"), "{shown}");
    assert!(shown.contains("devflow setup"));
    // The write is the original content, reformatted.
    apply(&change).unwrap();
    let written = read(dir.path(), "t3.json");
    assert!(written.contains("hunter2pw") && written.contains("tok-9f8e7d6c"));
    // And the change itself serializes to nothing a file held.
    let json = serde_json::to_string(&change).unwrap();
    assert!(
        !json.contains("hunter2pw") && !json.contains("tok-9f8e7d6c"),
        "{json}"
    );
}

#[cfg(unix)]
#[test]
fn a_checkout_moved_after_planning_is_not_written_into_through_its_old_path() {
    let outer = root();
    let outside = root();
    let checkout = outer.path().canonicalize().unwrap().join("repo");
    std::fs::create_dir_all(&checkout).unwrap();
    let change = plan(&checkout, Host::T3, false).unwrap();
    assert!(change.writes());
    // While the prompt is open the checkout is moved and a link to another
    // directory takes its place.
    let moved = outer.path().canonicalize().unwrap().join("moved");
    std::fs::rename(&checkout, &moved).unwrap();
    std::os::unix::fs::symlink(outside.path(), &checkout).unwrap();
    let error = apply(&change).unwrap_err();
    assert!(
        format!("{error:#}").contains("not the directory"),
        "{error:#}"
    );
    assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
    assert!(!moved.join("t3.json").exists());
    // The same when another real directory is put at the path.
    std::fs::remove_file(&checkout).unwrap();
    std::fs::create_dir(&checkout).unwrap();
    assert!(apply(&change).is_err());
    assert!(std::fs::read_dir(&checkout).unwrap().next().is_none());
    // And with the checkout back where it was, the plan applies.
    std::fs::remove_dir(&checkout).unwrap();
    std::fs::rename(&moved, &checkout).unwrap();
    assert!(apply(&change).unwrap());
}

#[test]
fn a_secret_that_spans_lines_is_masked_on_every_line_of_the_diff() {
    let dir = root();
    let theirs = "[scripts]\nrun = \"pnpm dev\"\ntoken = \"\"\"\nfirst-line-of-a-token-aaaa\nsecond-line-of-a-token-bbbb\n\"\"\"\n";
    write(dir.path(), ".conductor/settings.toml", theirs);
    let change = plan(dir.path(), Host::Conductor, false).unwrap();
    assert!(change.writes());
    let shown: String = change
        .diff()
        .iter()
        .map(|l| format!("{}\n", l.text))
        .collect();
    for leak in ["first-line-of-a-token-aaaa", "second-line-of-a-token-bbbb"] {
        assert!(!shown.contains(leak), "{shown}");
    }
    assert!(shown.contains("devflow setup"), "{shown}");
    apply(&change).unwrap();
    let written = read(dir.path(), ".conductor/settings.toml");
    assert!(written.contains("second-line-of-a-token-bbbb"));
}

#[test]
fn environment_variables_are_judged_by_their_names_in_the_diff() {
    let dir = root();
    // `DB_PASS` and `SENTRY_DSN` are not secret-looking JSON keys but are
    // secret variable names; a `{name, value}` pair is judged the same way.
    let theirs = r#"{"env": {"DB_PASS": "pw-env-1234", "SESSION_KEY": "sk-env-5678", "MODE": "dev"}, "vars": [{"name": "SENTRY_DSN", "value": "https://abc@sentry.example/1"}], "scripts": []}"#; // kingfisher:ignore
    write(dir.path(), "t3.json", theirs);
    let change = plan(dir.path(), Host::T3, false).unwrap();
    let shown: String = change
        .diff()
        .iter()
        .map(|l| format!("{}\n", l.text))
        .collect();
    for leak in ["pw-env-1234", "sk-env-5678", "abc@sentry.example"] {
        assert!(!shown.contains(leak), "{leak} in {shown}");
    }
    assert!(shown.contains("\"MODE\": \"dev\""), "{shown}");
    // A JSON string with escapes is masked in the form the file writes it.
    let escaped = r#"{"scripts": [], "token": "line1\nline2 \"quoted\" secret"}"#;
    write(dir.path(), "t3.json", escaped);
    let change = plan(dir.path(), Host::T3, false).unwrap();
    let shown: String = change
        .diff()
        .iter()
        .map(|l| format!("{}\n", l.text))
        .collect();
    assert!(!shown.contains("quoted"), "{shown}");
    assert!(!shown.contains("line2"), "{shown}");
}
