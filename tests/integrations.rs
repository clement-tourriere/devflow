//! Agent-host integrations end to end: the way each host really drives the
//! binary. No test here needs Docker or PostgreSQL, and none touches a real
//! host's configuration: every file lives in a temporary repository.
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
}

impl Env {
    fn new() -> Env {
        let env = Env::bare();
        let out = env.run(&["init", "--yes", "--name", "shop"]);
        assert!(out.status.success(), "{}", describe(&out));
        env
    }

    /// A repository with one commit and no devflow configuration.
    fn bare() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for dir in ["home", "config", "data"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = git2::Repository::init(&repo).unwrap();
        std::fs::write(repo.join("README.md"), "hello\n").unwrap();
        std::fs::write(repo.join(".gitignore"), ".env\n.devflow/\n").unwrap();
        let mut index = git.index().unwrap();
        index.add_path(Path::new("README.md")).unwrap();
        index.add_path(Path::new(".gitignore")).unwrap();
        index.write().unwrap();
        let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        git.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
            .unwrap();
        Env {
            _tmp: tmp,
            root,
            repo,
        }
    }

    /// The environment every process in a test gets: private state, and the
    /// directory of the binary first on `PATH` so a host's script can say
    /// plain `devflow`.
    fn prepare(&self, command: &mut Command) {
        let binary = Path::new(env!("CARGO_BIN_EXE_devflow"));
        command
            .env("HOME", self.root.join("home"))
            .env("DEVFLOW_CONFIG_DIR", self.root.join("config"))
            .env("DEVFLOW_DATA_DIR", self.root.join("data"))
            .env(
                "PATH",
                format!(
                    "{}:/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin",
                    binary.parent().unwrap().display()
                ),
            )
            .env("NO_COLOR", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdin(Stdio::null());
        for var in [
            "DEVFLOW_CD_FILE",
            "DEVFLOW_TRUST_HOOKS",
            "DEVFLOW_NON_INTERACTIVE",
            "DEVFLOW_INSTANCE_ID",
            "DEVFLOW_SETUP_ACTIVE",
            "DEVFLOW_PG_ADMIN_URL",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
        ] {
            command.env_remove(var);
        }
    }

    fn command(&self, dir: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_devflow"));
        command.current_dir(dir);
        self.prepare(&mut command);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(&self.repo).args(args).output().unwrap()
    }

    fn run_in(&self, dir: &Path, args: &[&str]) -> Output {
        self.command(dir).args(args).output().unwrap()
    }

    fn git(&self, dir: &Path, args: &[&str]) -> Output {
        let mut command = Command::new("git");
        command.current_dir(dir).args(args);
        self.prepare(&mut command);
        let out = command.output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", describe(&out));
        out
    }

    /// What a host does with a script: `sh -c COMMAND` in the worktree.
    fn script(&self, dir: &Path, command: &str) -> Command {
        let mut script = Command::new("sh");
        script.current_dir(dir).args(["-c", command]);
        self.prepare(&mut script);
        script
    }

    fn config_path(&self) -> PathBuf {
        self.repo.join("devflow.toml")
    }

    fn append_config(&self, text: &str) {
        let mut current = std::fs::read_to_string(self.config_path()).unwrap();
        current.push_str(text);
        std::fs::write(self.config_path(), current).unwrap();
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn describe(out: &Output) -> String {
    format!(
        "status: {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status.code(),
        stdout(out),
        stderr(out)
    )
}

fn ok(out: &Output) -> &Output {
    assert!(out.status.success(), "{}", describe(out));
    out
}

fn code(out: &Output, expected: i32) {
    assert_eq!(out.status.code(), Some(expected), "{}", describe(out));
}

fn json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|error| panic!("not one JSON document ({error}):\n{}", describe(out)))
}

fn workspace<'a>(list: &'a Value, name: &str) -> Option<&'a Value> {
    list["workspaces"]
        .as_array()?
        .iter()
        .find(|w| w["workspace"]["name"] == name)
}

// --- T3 Code -------------------------------------------------------------------

/// The script T3 Code would run for a new worktree: the first one with
/// `runOnWorktreeCreate`, and whether the agent waits for it.
fn t3_setup_script(repo: &Path) -> (String, bool) {
    let file: Value =
        serde_json::from_str(&std::fs::read_to_string(repo.join("t3.json")).unwrap()).unwrap();
    let script = file["scripts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["runOnWorktreeCreate"] == true)
        .expect("a script that runs when a worktree is created");
    (
        script["command"].as_str().unwrap().to_string(),
        script["async"] == false,
    )
}

#[test]
fn t3_code_creates_a_worktree_the_script_sets_it_up_and_exec_runs_in_it() {
    let env = Env::new();
    env.append_config("\n[hooks]\nsetup = [\"echo hook-ran > hook-ran.txt\"]\n");

    // 1. The integration writes t3.json (and shows what it writes).
    let out = env.run(&["integrate", "t3", "--yes"]);
    ok(&out);
    assert!(stdout(&out).contains("+ "), "{}", describe(&out));
    let (command, holds_the_agent) = t3_setup_script(&env.repo);
    assert_eq!(command, "devflow setup --wait");
    assert!(
        holds_the_agent,
        "async: false holds the agent until it exits"
    );

    // 2. T3 Code creates the worktree with Git, in its own folder.
    let worktree = env.root.join("t3-worktrees").join("task-1");
    env.git(
        &env.repo,
        &[
            "worktree",
            "add",
            "-b",
            "task-1",
            worktree.to_str().unwrap(),
        ],
    );
    assert!(worktree.is_dir());
    assert!(!worktree.join(".devflow").exists(), "nothing set it up yet");

    // 3. It runs the script in the new worktree (the hooks are approved by
    //    the person in the terminal T3 Code opens; here by the variable).
    let started = Instant::now();
    let out = env
        .script(&worktree, &command)
        .env("DEVFLOW_TRUST_HOOKS", "1")
        .env("T3CODE_PROJECT_ROOT", &env.repo)
        .env("T3CODE_WORKTREE_PATH", &worktree)
        .output()
        .unwrap();
    ok(&out);
    assert!(started.elapsed() < Duration::from_secs(60));
    assert!(
        worktree.join(".devflow/env").is_file(),
        "{}",
        describe(&out)
    );
    assert_eq!(
        std::fs::read_to_string(worktree.join("hook-ran.txt")).unwrap(),
        "hook-ran\n"
    );

    // 4. The agent runs its commands through `devflow exec`.
    let out = env.run_in(
        &worktree,
        &["exec", "--", "sh", "-c", "echo $DEVFLOW_WORKSPACE"],
    );
    ok(&out);
    assert_eq!(stdout(&out).trim(), "task-1");

    // A second run of the script is cheap and changes nothing.
    ok(&env
        .script(&worktree, &command)
        .env("DEVFLOW_TRUST_HOOKS", "1")
        .output()
        .unwrap());
    let list = json(&env.run(&["--json", "list"]));
    let found = workspace(&list, "task-1").expect("listed");
    assert_eq!(found["status"], "ready", "{list}");
    let receipt_id = found["receipt"]["instance_id"].clone();
    assert!(receipt_id.is_string());

    // 5. T3 Code deletes the worktree through Git (it has no teardown hook).
    //    Git forgets it, and the receipt with it; the workspace's record in the
    //    ledger is what `devflow gc` reconciles.
    env.git(
        &env.repo,
        &["worktree", "remove", "--force", worktree.to_str().unwrap()],
    );
    assert!(!worktree.exists());
    let list = json(&env.run(&["--json", "list"]));
    assert!(workspace(&list, "task-1").is_none(), "{list}");
    ok(&env.run(&["doctor"]));

    // `gc` classifies it as removed elsewhere (nothing but the app port and the
    // record are left, so it is not counted as still owning data) and removes it.
    let doc = json(&env.run(&["--json", "gc"]));
    let entry = doc["inventory"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["workspace"] == "task-1")
        .unwrap_or_else(|| panic!("{doc}"))
        .clone();
    assert_eq!(entry["class"], "removed_elsewhere", "{doc}");
    assert_eq!(entry["removable"], true, "{doc}");
    assert_eq!(entry["instance"], receipt_id, "{doc}");
    assert!(
        entry["app_port"].is_u64(),
        "the record carries the port: {doc}"
    );
    let short = entry["instance"].as_str().unwrap()[..8].to_string();
    let doc = json(&env.run(&["--json", "gc", "--remove", &short, "--yes"]));
    assert_eq!(doc["removed"][0]["workspace"], "task-1", "{doc}");
    assert_eq!(doc["skipped"], json!([]), "{doc}");
    assert!(
        std::fs::read_dir(env.repo.join(".git/devflow/workspaces"))
            .map(|entries| entries.count() == 0)
            .unwrap_or(true),
        "the record is gone"
    );
    let doc = json(&env.run(&["--json", "gc"]));
    assert!(
        doc["inventory"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["workspace"] != "task-1"),
        "{doc}"
    );
}

#[test]
fn integrate_shows_before_writing_and_print_writes_nothing() {
    let env = Env::new();
    let out = env.run(&["integrate", "t3", "--print"]);
    ok(&out);
    assert!(stdout(&out).contains("+ "), "{}", describe(&out));
    assert!(!env.repo.join("t3.json").exists());

    let out = env.run(&["--json", "integrate", "t3", "--print"]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["written"], false);
    assert_eq!(doc["would_write"], true);
    assert!(doc["diff"].as_array().unwrap().len() > 3);
    assert!(
        doc["notes"][0]
            .as_str()
            .unwrap()
            .contains("no hook that runs when it deletes"),
        "{doc}"
    );

    ok(&env.run(&["integrate", "t3", "--yes"]));
    let before = std::fs::read_to_string(env.repo.join("t3.json")).unwrap();
    let out = env.run(&["--json", "integrate", "t3", "--yes"]);
    ok(&out);
    assert_eq!(json(&out)["already_done"], true);
    assert_eq!(
        std::fs::read_to_string(env.repo.join("t3.json")).unwrap(),
        before
    );

    // Taking it out again.
    ok(&env.run(&["integrate", "t3", "--remove", "--yes"]));
    let after: Value =
        serde_json::from_str(&std::fs::read_to_string(env.repo.join("t3.json")).unwrap()).unwrap();
    assert!(after.get("scripts").is_none(), "{after}");
}

#[test]
fn a_conflicting_script_is_reported_with_exit_1_and_never_overwritten() {
    let env = Env::new();
    let theirs = "{\n  \"scripts\": [\n    {\"name\": \"Setup\", \"command\": \"make setup\", \"runOnWorktreeCreate\": true}\n  ]\n}\n";
    std::fs::write(env.repo.join("t3.json"), theirs).unwrap();
    let out = env.run(&["--json", "integrate", "t3", "--yes"]);
    code(&out, 1);
    let doc = json(&out);
    assert_eq!(doc["written"], false);
    assert_eq!(doc["conflicts"][0]["item"], "script \"Setup\"");
    assert_eq!(doc["add_by_hand"][0], "devflow setup --wait");
    assert_eq!(doc["remedies"][0]["kind"], "open_file");
    assert!(doc["error"].as_str().unwrap().contains("left alone"));
    assert!(
        !stdout(&out).contains("make setup"),
        "scripts are never quoted"
    );
    assert_eq!(
        std::fs::read_to_string(env.repo.join("t3.json")).unwrap(),
        theirs
    );
    let human = env.run(&["integrate", "t3", "--yes"]);
    code(&human, 1);
    assert!(
        stderr(&human).contains("Add this to the script"),
        "{}",
        describe(&human)
    );
}

#[test]
fn integrate_without_a_host_lists_what_the_repository_uses() {
    let env = Env::new();
    std::fs::create_dir_all(env.repo.join(".cursor")).unwrap();
    std::fs::write(env.repo.join("t3.json"), "{}").unwrap();
    let doc = json(&env.run(&["--json", "integrate"]));
    let hosts = doc["hosts"].as_array().unwrap();
    let get = |name: &str| hosts.iter().find(|h| h["host"] == name).unwrap();
    assert_eq!(get("t3")["detected"], true);
    assert_eq!(get("cursor")["detected"], true);
    assert_eq!(get("claude")["detected"], false);
    assert_eq!(get("t3")["state"], "not_configured");
    ok(&env.run(&["integrate", "cursor", "--yes"]));
    let doc = json(&env.run(&["--json", "integrate"]));
    let cursor = doc["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["host"] == "cursor")
        .unwrap()
        .clone();
    assert_eq!(cursor["state"], "configured");
}

// --- waiting -------------------------------------------------------------------

fn spawn_slow_setup(env: &Env, dir: &Path) -> Child {
    let mut command = env.command(dir);
    command
        .args(["setup"])
        .env("DEVFLOW_TRUST_HOOKS", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.spawn().unwrap()
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn wait_and_setup_wait_follow_a_setup_running_elsewhere() {
    let env = Env::new();
    env.append_config("\n[hooks]\nsetup = [\"touch started; sleep 4; echo done > finished\"]\n");
    let worktree = env.root.join("wt").join("slow");
    env.git(
        &env.repo,
        &["worktree", "add", "-b", "slow", worktree.to_str().unwrap()],
    );

    // Nothing runs and nothing was set up: reported, not waited for forever.
    let started = Instant::now();
    let out = env.run_in(&worktree, &["--json", "wait", "--timeout", "0"]);
    code(&out, 2);
    assert_eq!(json(&out)["settled"], "not_set_up");
    assert!(started.elapsed() < Duration::from_secs(10));

    let mut running = spawn_slow_setup(&env, &worktree);
    wait_until("the hook to start", || worktree.join("started").exists());

    // A short timeout while the run goes on: exit 2 and "still running".
    let out = env.run_in(&worktree, &["--json", "wait", "--timeout", "1s"]);
    code(&out, 2);
    let doc = json(&out);
    assert_eq!(doc["settled"], "still_running", "{doc}");
    assert_eq!(doc["status"], "in_progress");
    assert!(doc["waited_ms"].as_u64().unwrap() >= 900);
    assert!(
        !worktree.join("finished").exists(),
        "the waiter did not wait for it to end"
    );

    // `setup --wait` is Busy -> waits for the other run and reports ready.
    let out = env.run_in(&worktree, &["setup", "--wait", "--timeout", "60s"]);
    ok(&out);
    assert!(stderr(&out).contains("is ready"), "{}", describe(&out));
    assert!(worktree.join("finished").exists());
    assert!(running.wait().unwrap().success());

    // `exec --wait` on a ready workspace just runs.
    let out = env.run_in(&worktree, &["exec", "--wait", "--", "echo", "hi"]);
    ok(&out);
    assert_eq!(stdout(&out), "hi\n");
    ok(&env.run_in(&worktree, &["wait"]));
}

#[test]
fn exec_wait_holds_the_command_until_the_workspace_is_ready_and_refuses_when_it_is_not() {
    let env = Env::new();
    env.append_config("\n[hooks]\nsetup = [\"touch started; sleep 3; touch finished\"]\n");
    let worktree = env.root.join("wt").join("held");
    env.git(
        &env.repo,
        &["worktree", "add", "-b", "held", worktree.to_str().unwrap()],
    );
    let mut running = spawn_slow_setup(&env, &worktree);
    wait_until("the hook to start", || worktree.join("started").exists());
    // `.devflow/env` is written before the hooks run, so a plain exec starts
    // at once, while the setup is not over...
    let probe = "test -f finished && touch command-ran";
    code(
        &env.run_in(&worktree, &["exec", "--", "sh", "-c", probe]),
        1,
    );
    assert!(!worktree.join("command-ran").exists());
    // A timeout while the setup still runs is "not ready" (2), and the
    // command did not start.
    let out = env.run_in(
        &worktree,
        &[
            "exec",
            "--wait",
            "--timeout",
            "1s",
            "--",
            "touch",
            "never-ran",
        ],
    );
    code(&out, 2);
    assert!(
        stderr(&out).contains("still being set up"),
        "{}",
        describe(&out)
    );
    assert!(!worktree.join("never-ran").exists());
    // ...and with --wait it starts after the setup is over.
    let out = env.run_in(&worktree, &["exec", "--wait", "--", "sh", "-c", probe]);
    ok(&out);
    assert!(worktree.join("command-ran").exists());
    assert!(running.wait().unwrap().success());
    // Once it starts, the command's own status is the result.
    code(
        &env.run_in(&worktree, &["exec", "--wait", "--", "sh", "-c", "exit 7"]),
        7,
    );
    code(
        &env.run_in(&worktree, &["exec", "--wait", "--", "sh", "-c", "exit 2"]),
        2,
    );

    // A workspace nobody set up is refused after the grace, with the remedy.
    let bare = env.root.join("wt").join("bare");
    env.git(
        &env.repo,
        &["worktree", "add", "-b", "bare", bare.to_str().unwrap()],
    );
    let out = env.run_in(&bare, &["exec", "--wait", "--timeout", "0", "--", "true"]);
    code(&out, 2);
    assert!(
        stderr(&out).contains("has not been set up"),
        "{}",
        describe(&out)
    );
    assert!(stderr(&out).contains("hint:"), "{}", describe(&out));
}

// --- Claude Code ---------------------------------------------------------------

fn claude_input(name: &str, cwd: &Path) -> String {
    serde_json::json!({
        "session_id": "abc123",
        "transcript_path": "/tmp/t.jsonl",
        "cwd": cwd,
        "hook_event_name": "WorktreeCreate",
        "name": name,
    })
    .to_string()
}

fn with_stdin(mut command: Command, input: &str) -> Output {
    use std::io::Write;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn claude_create_prints_only_the_path_and_remove_takes_the_workspace_down() {
    let env = Env::new();
    // A hook that writes to standard output must not reach Claude Code's
    // reading of the path.
    env.append_config(
        "\n[hooks]\nsetup = [\"echo NOISE-ON-STDOUT\", \"echo hook-ran > hook-ran.txt\"]\n",
    );
    ok(&env.run(&["integrate", "claude", "--yes"]));

    let mut create = env.command(&env.repo);
    create
        .args(["integrate", "claude-create"])
        .env("DEVFLOW_TRUST_HOOKS", "1");
    let out = with_stdin(create, &claude_input("bold-oak-a3f2", &env.repo));
    ok(&out);
    let printed = stdout(&out);
    assert_eq!(printed.lines().count(), 1, "{}", describe(&out));
    let path = PathBuf::from(printed.trim_end());
    assert!(path.is_absolute() && path.is_dir(), "{}", describe(&out));
    assert!(!printed.contains("NOISE"), "{}", describe(&out));
    assert!(path.join(".devflow/env").is_file());
    assert_eq!(
        std::fs::read_to_string(path.join("hook-ran.txt")).unwrap(),
        "hook-ran\n"
    );
    assert!(
        stderr(&out).contains("is ready"),
        "progress goes to stderr: {}",
        describe(&out)
    );
    assert!(
        path.ends_with("worktree-bold-oak-a3f2"),
        "{}",
        path.display()
    );
    // Claude Code refuses paths with `.` or `..` segments.
    assert!(!printed.contains("/./") && !printed.contains(".."));
    let list = json(&env.run(&["--json", "list"]));
    assert_eq!(
        workspace(&list, "worktree-bold-oak-a3f2").unwrap()["status"],
        "ready"
    );

    // The same name again enters the worktree that is there (Claude Code reuses names).
    let mut again = env.command(&env.repo);
    again
        .args(["integrate", "claude-create"])
        .env("DEVFLOW_TRUST_HOOKS", "1");
    let out = with_stdin(again, &claude_input("bold-oak-a3f2", &env.repo));
    ok(&out);
    assert_eq!(PathBuf::from(stdout(&out).trim_end()), path);

    // WorktreeRemove is the last check: a worktree with uncommitted work is
    // refused (the hook exits non-zero, Claude Code keeps the directory)...
    let remove_input = serde_json::json!({
        "hook_event_name": "WorktreeRemove",
        "cwd": env.repo,
        "worktree_path": path,
    })
    .to_string();
    let mut remove = env.command(&env.repo);
    remove.args(["integrate", "claude-remove"]);
    let out = with_stdin(remove, &remove_input);
    code(&out, 1);
    assert!(path.is_dir(), "{}", describe(&out));
    assert!(
        stderr(&out).contains("uncommitted changes"),
        "{}",
        describe(&out)
    );
    // ...and once the work is committed or discarded it is taken down.
    std::fs::remove_file(path.join("hook-ran.txt")).unwrap();
    let mut remove = env.command(&env.repo);
    remove.args(["integrate", "claude-remove"]);
    let out = with_stdin(remove, &remove_input);
    ok(&out);
    assert!(stdout(&out).is_empty(), "{}", describe(&out));
    assert!(!path.exists());
    let branches = String::from_utf8_lossy(
        &env.git(&env.repo, &["branch", "--list", "worktree-bold-oak-a3f2"])
            .stdout,
    )
    .into_owned();
    assert!(branches.contains("worktree-bold-oak-a3f2"), "{branches}");

    // A path that is not a devflow workspace is not ours: exit 0, nothing done.
    let stranger = env.root.join("elsewhere");
    std::fs::create_dir_all(&stranger).unwrap();
    let mut remove = env.command(&env.repo);
    remove.args(["integrate", "claude-remove"]);
    let out = with_stdin(
        remove,
        &serde_json::json!({ "worktree_path": stranger }).to_string(),
    );
    ok(&out);
    assert!(stranger.is_dir());
    assert!(stderr(&out).contains("not a devflow workspace"));
}

#[test]
fn claude_create_fails_the_creation_when_the_workspace_is_not_ready_and_leaves_stdout_empty() {
    let env = Env::new();
    env.append_config("\n[hooks]\nsetup = [\"true\"]\n");
    // No approval of the hooks and nobody to ask: the workspace needs attention.
    let mut create = env.command(&env.repo);
    create.args(["integrate", "claude-create"]);
    let out = with_stdin(create, &claude_input("needs-trust", &env.repo));
    code(&out, 1);
    assert!(stdout(&out).is_empty(), "{}", describe(&out));
    assert!(stderr(&out).contains("not ready"), "{}", describe(&out));
    assert!(stderr(&out).contains("hint:"), "{}", describe(&out));

    // A malformed document fails too, and says nothing on stdout.
    let mut create = env.command(&env.repo);
    create.args(["integrate", "claude-create"]);
    let out = with_stdin(create, "not json");
    code(&out, 1);
    assert!(stdout(&out).is_empty());

    // --json has no meaning for a hook with its own output contract.
    let mut create = env.command(&env.repo);
    create.args(["--json", "integrate", "claude-create"]);
    code(&with_stdin(create, &claude_input("x", &env.repo)), 1);
}

// --- Conductor, Cursor -------------------------------------------------------------

#[test]
fn conductor_and_cursor_scripts_run_the_way_those_hosts_run_them() {
    let env = Env::new();
    ok(&env.run(&["integrate", "conductor", "--yes"]));
    ok(&env.run(&["integrate", "cursor", "--yes"]));

    let settings = std::fs::read_to_string(env.repo.join(".conductor/settings.toml")).unwrap();
    let parsed: toml::Table = settings.parse().unwrap();
    let scripts = parsed["scripts"].as_table().unwrap();
    let setup = scripts["setup"].as_str().unwrap();
    let archive = scripts["archive"].as_str().unwrap();

    let worktree = env.root.join("conductor").join("lima");
    env.git(
        &env.repo,
        &["worktree", "add", "-b", "lima", worktree.to_str().unwrap()],
    );
    let conductor_env = |command: &str, dir: &Path| {
        let mut script = env.script(dir, command);
        script
            .env("CONDUCTOR_WORKSPACE_PATH", &worktree)
            .env("CONDUCTOR_ROOT_PATH", &env.repo)
            .env("CONDUCTOR_WORKSPACE_NAME", "lima")
            .env("CONDUCTOR_PORT", "55100");
        script.output().unwrap()
    };
    ok(&conductor_env(setup, &worktree));
    assert!(worktree.join(".devflow/env").is_file());
    // Archive runs in the workspace: the workspace goes, the branch stays.
    let out = conductor_env(archive, &worktree);
    ok(&out);
    assert!(!worktree.exists(), "{}", describe(&out));
    let branches =
        String::from_utf8_lossy(&env.git(&env.repo, &["branch", "--list", "lima"]).stdout)
            .into_owned();
    assert!(branches.contains("lima"));

    // Cursor: the first command of `setup-worktree`, run in the worktree.
    let file: Value = serde_json::from_str(
        &std::fs::read_to_string(env.repo.join(".cursor/worktrees.json")).unwrap(),
    )
    .unwrap();
    let command = file["setup-worktree"][0].as_str().unwrap();
    let worktree = env.root.join("cursor").join("mike");
    env.git(
        &env.repo,
        &["worktree", "add", "-b", "mike", worktree.to_str().unwrap()],
    );
    let mut script = env.script(&worktree, command);
    script.env("ROOT_WORKTREE_PATH", &env.repo);
    ok(&script.output().unwrap());
    assert!(worktree.join(".devflow/env").is_file());
}

// --- the Git hook ---------------------------------------------------------------

/// `git worktree add` as the hook sees it: the time Git is held, and the
/// worktree.
fn worktree_add_timed(env: &Env, name: &str) -> (PathBuf, Duration) {
    let worktree = env.root.join("hooked").join(name);
    let mut git = Command::new("git");
    git.current_dir(&env.repo)
        .args(["worktree", "add", "-b", name, worktree.to_str().unwrap()])
        .env("DEVFLOW_TRUST_HOOKS", "1");
    env.prepare(&mut git);
    git.env("DEVFLOW_TRUST_HOOKS", "1");
    let started = Instant::now();
    let out = git.output().unwrap();
    let held = started.elapsed();
    assert!(out.status.success(), "{}", describe(&out));
    (worktree, held)
}

#[test]
fn the_background_hook_returns_at_once_and_the_setup_finishes_detached() {
    let env = Env::new();
    env.append_config("\n[hooks]\nsetup = [\"sleep 4; touch finished\"]\n");
    ok(&env.run(&["hook", "install"]));

    // Default: Git waits for the whole setup.
    let (waited, held) = worktree_add_timed(&env, "sync");
    assert!(held >= Duration::from_secs(4), "{held:?}");
    assert!(waited.join("finished").exists());

    // Background: Git returns long before the hook is over...
    env.append_config("\n[integrations]\ngit_hook = \"background\"\n");
    let (worktree, held) = worktree_add_timed(&env, "background");
    assert!(held < Duration::from_secs(3), "git was held for {held:?}");
    assert!(!worktree.join("finished").exists());
    // ...and the setup goes on in the worktree's Git directory's log.
    let git_dir = String::from_utf8_lossy(
        &env.git(&worktree, &["rev-parse", "--absolute-git-dir"])
            .stdout,
    )
    .trim()
    .to_string();
    let log = Path::new(&git_dir).join("devflow/logs/git-hook.log");
    assert!(log.is_file(), "{}", log.display());
    // Whoever needs the workspace waits for it.
    let out = env.run_in(&worktree, &["--json", "wait", "--timeout", "60s"]);
    ok(&out);
    assert_eq!(json(&out)["settled"], "ready");
    assert!(worktree.join("finished").exists());
    // Once it is set up the hook starts nothing.
    let again = env.run_in(&worktree, &["setup", "--if-new", "--from-git-hook"]);
    ok(&again);
    assert!(
        !stderr(&again).contains("background"),
        "{}",
        describe(&again)
    );
}

#[test]
fn an_unknown_git_hook_mode_is_a_configuration_error() {
    let env = Env::new();
    env.append_config("\n[integrations]\ngit_hook = \"later\"\n");
    let out = env.run(&["list"]);
    code(&out, 1);
    assert!(stderr(&out).contains("git_hook"), "{}", describe(&out));
}

// --- .worktreeinclude -------------------------------------------------------------

#[test]
fn worktreeinclude_files_join_the_payload_next_to_the_copy_entries() {
    let env = Env::new();
    // `.env` is ignored (the repository's .gitignore); `.worktreeinclude` names
    // it and a file that is tracked, which must not be copied twice.
    std::fs::write(env.repo.join(".env"), "SECRET=1\n").unwrap();
    std::fs::write(env.repo.join(".env.local"), "LOCAL=1\n").unwrap();
    std::fs::write(env.repo.join(".gitignore"), ".env\n.env.local\ncache/\n").unwrap();
    std::fs::create_dir_all(env.repo.join("cache")).unwrap();
    std::fs::write(env.repo.join("cache/blob"), "x").unwrap();
    std::fs::write(
        env.repo.join(".worktreeinclude"),
        "# local state\n.env\n.env.local\nREADME.md\n",
    )
    .unwrap();
    // `init` wrote a [worktree] table: add the copy entry to it.
    let text = std::fs::read_to_string(env.config_path()).unwrap();
    std::fs::write(
        env.config_path(),
        text.replace("[worktree]\n", "[worktree]\ncopy = [\"cache\"]\n"),
    )
    .unwrap();
    env.git(&env.repo, &["add", "-A"]);
    env.git(&env.repo, &["commit", "-qm", "files"]);

    let out = env.run(&["--json", "--non-interactive", "switch", "-c", "feature"]);
    ok(&out);
    let doc = json(&out);
    let path = PathBuf::from(doc["path"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(path.join(".env")).unwrap(),
        "SECRET=1\n"
    );
    assert_eq!(
        std::fs::read_to_string(path.join(".env.local")).unwrap(),
        "LOCAL=1\n"
    );
    assert!(
        path.join("cache/blob").is_file(),
        "the copy entry still works"
    );
    let copied: Vec<&str> = doc["payload"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["path"].as_str().unwrap())
        .collect();
    assert_eq!(copied, ["cache", ".env", ".env.local"], "{doc}");
    assert!(
        doc["receipt"]["phases"]["payload"]["detail"]
            .as_str()
            .unwrap()
            .contains("selected by .worktreeinclude"),
        "{doc}"
    );
}

// --- Codex (detached HEAD) ---------------------------------------------------------

#[test]
fn a_detached_head_worktree_is_adopted_under_its_directory_name() {
    let env = Env::new();
    let worktree = env.root.join("codex").join("a1b2");
    env.git(
        &env.repo,
        &["worktree", "add", "--detach", worktree.to_str().unwrap()],
    );
    let out = env.run_in(&worktree, &["--json", "setup", "--wait"]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["status"], "ready", "{doc}");
    assert_eq!(doc["workspace"]["name"], "a1b2");
    assert!(doc["workspace"]["branch"].is_null(), "{doc}");
    assert!(worktree.join(".devflow/env").is_file());

    let list = json(&env.run(&["--json", "list"]));
    let found = workspace(&list, "a1b2").expect("listed under the directory name");
    assert_eq!(found["status"], "ready");
    // It can be named, entered and run in like any workspace.
    let out = env.run(&["path", "a1b2"]);
    ok(&out);
    assert_eq!(
        PathBuf::from(stdout(&out).trim()).canonicalize().unwrap(),
        worktree.canonicalize().unwrap()
    );
    let out = env.run_in(
        &worktree,
        &["exec", "--", "sh", "-c", "echo $DEVFLOW_WORKSPACE"],
    );
    ok(&out);
    assert_eq!(stdout(&out).trim(), "a1b2");
}

// --- init ---------------------------------------------------------------------------

#[test]
fn init_lists_the_hosts_the_repository_uses_and_sets_up_the_ones_it_is_told_to() {
    let env = Env::bare();
    std::fs::write(env.repo.join("t3.json"), "{}").unwrap();
    std::fs::create_dir_all(env.repo.join(".cursor")).unwrap();
    std::fs::create_dir_all(env.repo.join(".codex")).unwrap();

    let out = env.run(&["init", "--plan"]);
    ok(&out);
    let text = stdout(&out);
    assert!(text.contains("Agent hosts used here"), "{text}");
    assert!(
        text.contains("T3 Code (t3.json)") && text.contains("Cursor (.cursor)"),
        "{text}"
    );
    assert!(!text.contains("Codex"), "{text}");
    assert!(
        !env.repo.join(".cursor/worktrees.json").exists(),
        "--plan writes nothing"
    );

    // Unattended, only the host named with --integrate is written; the other
    // is left alone and listed as a next step.
    let out = env.run(&[
        "--json",
        "init",
        "--yes",
        "--name",
        "shop",
        "--integrate",
        "cursor",
    ]);
    ok(&out);
    let doc = json(&out);
    let hosts = doc["integrations"].as_array().unwrap();
    let cursor = hosts.iter().find(|h| h["host"] == "cursor").unwrap();
    assert_eq!(cursor["written"], true, "{doc}");
    let t3 = hosts.iter().find(|h| h["host"] == "t3").unwrap();
    assert_eq!(t3["configured"], false);
    assert_eq!(t3["written"], false);
    assert!(
        doc["next_steps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s.as_str().unwrap().contains("devflow integrate t3")),
        "{doc}"
    );
    assert_eq!(
        std::fs::read_to_string(env.repo.join("t3.json")).unwrap(),
        "{}"
    );
    assert!(env.repo.join(".cursor/worktrees.json").is_file());
}

#[test]
fn claude_remove_of_a_directory_that_is_already_gone_still_takes_the_workspace_down() {
    let env = Env::new();
    ok(&env.run(&["integrate", "claude", "--yes"]));
    let mut create = env.command(&env.repo);
    create.args(["integrate", "claude-create"]);
    let out = with_stdin(create, &claude_input("gone-soon", &env.repo));
    ok(&out);
    let path = PathBuf::from(stdout(&out).trim_end());
    // Something else deleted the directory; Git still has it registered.
    std::fs::remove_dir_all(&path).unwrap();
    let list = json(&env.run(&["--json", "list"]));
    assert_eq!(
        workspace(&list, "worktree-gone-soon").unwrap()["workspace"]["missing"],
        true
    );

    let input = serde_json::json!({ "cwd": env.repo, "worktree_path": path }).to_string();
    // Without the session's directory there is no project to ask.
    let mut blind = env.command(&env.root);
    blind.args(["integrate", "claude-remove"]);
    let out = with_stdin(
        blind,
        &serde_json::json!({ "worktree_path": path }).to_string(),
    );
    ok(&out);
    assert!(stderr(&out).contains("already gone"), "{}", describe(&out));

    let mut remove = env.command(&env.root);
    remove.args(["integrate", "claude-remove"]);
    let out = with_stdin(remove, &input);
    ok(&out);
    assert!(stdout(&out).is_empty(), "{}", describe(&out));
    assert!(
        stderr(&out).contains("removed worktree-gone-soon"),
        "{}",
        describe(&out)
    );
    let list = json(&env.run(&["--json", "list"]));
    assert!(workspace(&list, "worktree-gone-soon").is_none(), "{list}");
    let branches = String::from_utf8_lossy(
        &env.git(&env.repo, &["branch", "--list", "worktree-gone-soon"])
            .stdout,
    )
    .into_owned();
    assert!(
        branches.contains("worktree-gone-soon"),
        "the branch is kept"
    );

    // Nothing is registered any more: the same call is a no-op.
    let mut again = env.command(&env.root);
    again.args(["integrate", "claude-remove"]);
    let out = with_stdin(again, &input);
    ok(&out);
    assert!(stderr(&out).contains("already gone"), "{}", describe(&out));
}

#[test]
fn conductors_archive_script_never_forces_and_says_why() {
    let env = Env::new();
    ok(&env.run(&["integrate", "conductor", "--yes"]));
    let settings: toml::Table = std::fs::read_to_string(env.repo.join(".conductor/settings.toml"))
        .unwrap()
        .parse()
        .unwrap();
    let archive = settings["scripts"]["archive"].as_str().unwrap().to_string();
    let worktree = env.root.join("conductor").join("dirty");
    env.git(
        &env.repo,
        &["worktree", "add", "-b", "dirty", worktree.to_str().unwrap()],
    );
    let mut setup = env.script(&worktree, "devflow setup --wait");
    ok(&setup.output().unwrap());
    std::fs::write(worktree.join("unsaved.txt"), "work\n").unwrap();
    let mut script = env.script(&worktree, &archive);
    script
        .env("CONDUCTOR_ROOT_PATH", &env.repo)
        .env("CONDUCTOR_WORKSPACE_PATH", &worktree);
    let out = script.output().unwrap();
    assert!(!out.status.success(), "{}", describe(&out));
    assert!(
        stderr(&out).contains("uncommitted changes"),
        "{}",
        describe(&out)
    );
    assert!(worktree.join("unsaved.txt").is_file());
}

// --- Claude Code: identity, discovery and descriptors --------------------------------

fn claude_create_path(env: &Env, name: &str) -> PathBuf {
    let mut create = env.command(&env.repo);
    create.args(["integrate", "claude-create"]);
    let out = with_stdin(create, &claude_input(name, &env.repo));
    ok(&out);
    PathBuf::from(stdout(&out).trim_end())
}

fn claude_remove_out(env: &Env, path: &Path) -> Output {
    let mut remove = env.command(&env.root);
    remove.args(["integrate", "claude-remove"]);
    with_stdin(
        remove,
        &serde_json::json!({ "cwd": env.repo, "worktree_path": path }).to_string(),
    )
}

#[cfg(unix)]
#[test]
fn claude_remove_never_follows_a_link_to_another_worktree() {
    let env = Env::new();
    let a = claude_create_path(&env, "alpha");
    let b = claude_create_path(&env, "bravo");
    // A's stored path becomes a link to the clean workspace B.
    std::fs::remove_dir_all(&a).unwrap();
    std::os::unix::fs::symlink(&b, &a).unwrap();
    let out = claude_remove_out(&env, &a);
    code(&out, 1);
    assert!(stderr(&out).contains("symbolic link"), "{}", describe(&out));
    assert!(b.join(".devflow/env").is_file(), "B was not touched");
    let list = json(&env.run(&["--json", "list"]));
    assert!(workspace(&list, "worktree-bravo").is_some(), "{list}");
    // The link's parent directory counts as well.
    let linked = env.root.join("via-link");
    std::os::unix::fs::symlink(b.parent().unwrap(), &linked).unwrap();
    let through = linked.join(b.file_name().unwrap());
    code(&claude_remove_out(&env, &through), 1);
    assert!(b.join(".devflow/env").is_file());
}

#[test]
fn claude_create_never_hands_out_the_main_checkout() {
    let env = Env::new();
    env.git(&env.repo, &["checkout", "-q", "-b", "worktree-foo"]);
    let mut create = env.command(&env.repo);
    create.args(["integrate", "claude-create"]);
    let out = with_stdin(create, &claude_input("foo", &env.repo));
    code(&out, 1);
    assert!(stdout(&out).is_empty(), "{}", describe(&out));
    assert!(stderr(&out).contains("main checkout"), "{}", describe(&out));
}

#[test]
fn claude_remove_reports_a_project_it_cannot_read_instead_of_acknowledging() {
    let env = Env::new();
    let path = claude_create_path(&env, "sturdy");
    let good = std::fs::read_to_string(env.config_path()).unwrap();
    std::fs::write(env.config_path(), "[project\nname = ").unwrap();
    let out = claude_remove_out(&env, &path);
    code(&out, 1);
    assert!(path.is_dir(), "{}", describe(&out));
    std::fs::write(env.config_path(), good).unwrap();
    // A path in a repository that has no devflow.toml at all is positively foreign.
    let foreign = env.root.join("foreign");
    std::fs::create_dir_all(&foreign).unwrap();
    git2::Repository::init(&foreign).unwrap();
    let out = claude_remove_out(&env, &foreign);
    ok(&out);
    assert!(
        stderr(&out).contains("not a devflow workspace"),
        "{}",
        describe(&out)
    );
    assert!(foreign.is_dir());
}

#[test]
fn a_hook_that_leaves_a_process_running_does_not_hold_claude_codes_pipe_open() {
    let env = Env::new();
    env.append_config("\n[hooks]\nsetup = [\"(sleep 8) >/dev/null 2>&1 &\"]\n");
    let mut create = env.command(&env.repo);
    create
        .args(["integrate", "claude-create"])
        .env("DEVFLOW_TRUST_HOOKS", "1");
    let started = Instant::now();
    // `output()` reads standard output until every holder of the pipe closed it.
    let out = with_stdin(create, &claude_input("lingering", &env.repo));
    ok(&out);
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "the background process kept the pipe open for {:?}",
        started.elapsed()
    );
}

#[cfg(unix)]
#[test]
fn claude_remove_refuses_a_git_file_pointed_at_another_worktrees_admin_directory() {
    let env = Env::new();
    let a = claude_create_path(&env, "gamma");
    let b = claude_create_path(&env, "delta");
    // A's ordinary `.git` file now names B's administrative directory: the
    // checkout's own claim can no longer be what identifies the registration.
    let b_git = std::fs::read_to_string(b.join(".git")).unwrap();
    std::fs::write(a.join(".git"), &b_git).unwrap();
    let out = claude_remove_out(&env, &a);
    code(&out, 1);
    assert!(
        stderr(&out).contains("nothing was removed"),
        "{}",
        describe(&out)
    );
    // B, its receipt, its data and its registration are all still there.
    assert!(b.join(".devflow/env").is_file());
    assert_eq!(std::fs::read_to_string(b.join(".git")).unwrap(), b_git);
    let git_list = String::from_utf8_lossy(
        &env.git(&env.repo, &["worktree", "list", "--porcelain"])
            .stdout,
    )
    .into_owned();
    assert!(git_list.contains(b.to_str().unwrap()), "{git_list}");
    let admin = PathBuf::from(b_git.trim().strip_prefix("gitdir:").unwrap().trim());
    assert!(
        admin.join("devflow/receipt.json").is_file(),
        "B's receipt was not consumed"
    );
    assert!(a.is_dir());
}

#[test]
fn claude_create_does_not_adopt_a_worktree_somebody_made_on_that_branch() {
    let env = Env::new();
    let theirs = env.root.join("theirs").join("foo");
    env.git(
        &env.repo,
        &[
            "worktree",
            "add",
            "-b",
            "worktree-foo",
            theirs.to_str().unwrap(),
        ],
    );
    for adopted in [false, true] {
        if adopted {
            // Adopting it with `devflow setup` is not creating it for Claude Code.
            ok(&env.run_in(&theirs, &["setup"]));
        }
        let mut create = env.command(&env.repo);
        create.args(["integrate", "claude-create"]);
        let out = with_stdin(create, &claude_input("foo", &env.repo));
        code(&out, 1);
        assert!(stdout(&out).is_empty(), "{}", describe(&out));
        assert!(
            stderr(&out).contains("did not create it for Claude Code"),
            "{}",
            describe(&out)
        );
        assert!(theirs.is_dir());
        assert_eq!(adopted, theirs.join(".devflow/env").exists());
    }
}

#[cfg(unix)]
#[test]
fn claude_remove_does_not_read_a_config_that_is_not_a_file_as_not_ours() {
    let env = Env::new();
    let path = claude_create_path(&env, "firm");
    let good = std::fs::read_to_string(env.config_path()).unwrap();
    // A directory where devflow.toml belongs.
    std::fs::remove_file(env.config_path()).unwrap();
    std::fs::create_dir(env.config_path()).unwrap();
    let out = claude_remove_out(&env, &path);
    code(&out, 1);
    assert!(path.is_dir(), "{}", describe(&out));
    // A link to nothing.
    std::fs::remove_dir(env.config_path()).unwrap();
    std::os::unix::fs::symlink(env.root.join("nowhere"), env.config_path()).unwrap();
    code(&claude_remove_out(&env, &path), 1);
    assert!(path.is_dir());
    // Restored, it is removed as usual.
    std::fs::remove_file(env.config_path()).unwrap();
    std::fs::write(env.config_path(), good).unwrap();
    ok(&claude_remove_out(&env, &path));
    assert!(!path.exists());
}
