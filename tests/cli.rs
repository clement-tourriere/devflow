//! End-to-end tests of the `devflow` binary against temporary Git repositories.
//!
//! Every run uses private `DEVFLOW_CONFIG_DIR` / `DEVFLOW_DATA_DIR` / `HOME`
//! directories. Tests that need the workspace pipeline (create, list, remove,
//! trust) run the real core; none of them need Docker or PostgreSQL.
use devflow_core::receipt::Receipt;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Env {
    /// Owns every directory below.
    _tmp: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
}

impl Env {
    /// A Git repository with one commit at `<tmp>/repo`.
    fn new() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for dir in ["home", "config", "data"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = git2::Repository::init(&repo).unwrap();
        std::fs::write(repo.join("README.md"), "hello\n").unwrap();
        let mut index = git.index().unwrap();
        index.add_path(Path::new("README.md")).unwrap();
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

    fn command(&self, dir: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_devflow"));
        command
            .current_dir(dir)
            .env("HOME", self.root.join("home"))
            .env("DEVFLOW_CONFIG_DIR", self.root.join("config"))
            .env("DEVFLOW_DATA_DIR", self.root.join("data"))
            .env("NO_COLOR", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
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
        command
    }

    /// Run in the main checkout.
    fn run(&self, args: &[&str]) -> Output {
        self.run_in(&self.repo, args)
    }

    fn run_in(&self, dir: &Path, args: &[&str]) -> Output {
        self.command(dir).args(args).output().unwrap()
    }

    fn init(&self) {
        let out = self.run(&["init", "--name", "shop"]);
        ok(&out);
    }

    fn config_path(&self) -> PathBuf {
        self.repo.join("devflow.toml")
    }

    /// Replace `from` in `devflow.toml` (a line the generated file contains).
    fn edit_config(&self, from: &str, to: &str) {
        let text = std::fs::read_to_string(self.config_path()).unwrap();
        assert!(text.contains(from), "{from:?} not in:\n{text}");
        std::fs::write(self.config_path(), text.replace(from, to)).unwrap();
    }

    /// A directory holding an executable `name` that runs `script` with
    /// `FAKE_LOG` pointing at the returned log file.
    fn fake_program(&self, name: &str, script: &str) -> FakeProgram {
        let dir = self.root.join(format!("fake-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join(name);
        std::fs::write(&program, format!("#!/bin/sh\n{script}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        FakeProgram {
            dir,
            log: self.root.join(format!("{name}.log")),
        }
    }
}

struct FakeProgram {
    dir: PathBuf,
    log: PathBuf,
}

impl FakeProgram {
    /// What the program wrote: `KEY=value` lines (`ARG` repeats).
    fn lines(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_else(|error| panic!("{} was not run: {error}", self.log.display()))
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn args(&self) -> Vec<String> {
        self.lines()
            .iter()
            .filter_map(|line| line.strip_prefix("ARG=").map(str::to_string))
            .collect()
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

/// Assert exit status 0.
fn ok(out: &Output) -> &Output {
    assert!(out.status.success(), "{}", describe(out));
    out
}

fn failed(out: &Output) -> &Output {
    assert_eq!(out.status.code(), Some(1), "{}", describe(out));
    out
}

/// Parse stdout as the single JSON document the command promises.
fn json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not one JSON document ({error}):\n{}",
            describe(out)
        )
    })
}

fn same_path(a: &str, b: &Path) -> bool {
    Path::new(a.trim()).canonicalize().ok() == b.canonicalize().ok()
}

// --- no core pipeline needed ------------------------------------------------

#[test]
fn help_lists_the_commands() {
    let env = Env::new();
    let out = env.run(&["--help"]);
    ok(&out);
    let text = stdout(&out);
    for command in [
        "init",
        "switch",
        "list",
        "path",
        "setup",
        "remove",
        "trust",
        "hook",
        "db",
        "doctor",
        "shell-init",
        "completions",
        "gui",
    ] {
        assert!(
            text.lines()
                .any(|line| line.trim_start().starts_with(command)),
            "`{command}` missing from --help:\n{text}"
        );
    }
    for flag in ["--json", "--non-interactive", "-C"] {
        assert!(text.contains(flag), "{flag} missing from --help:\n{text}");
    }
}

#[test]
fn db_help_lists_the_subcommands() {
    let env = Env::new();
    let text = stdout(ok(&env.run(&["db", "--help"])));
    for command in [
        "status",
        "up",
        "down",
        "url",
        "psql",
        "reset",
        "reconcile",
        "snapshot",
        "branch",
    ] {
        assert!(text.contains(command), "`db {command}` missing:\n{text}");
    }
    let text = stdout(ok(&env.run(&["db", "branch", "create", "--help"])));
    assert!(text.contains("--instance-id"), "{text}");
    assert!(text.contains("--data-from"), "{text}");
}

#[test]
fn init_writes_the_config_and_refuses_a_second_time() {
    let env = Env::new();
    ok(&env.run(&["init", "--name", "shop"]));
    let text = std::fs::read_to_string(env.config_path()).unwrap();
    assert!(text.contains("name = \"shop\""), "{text}");

    let second = env.run(&["init"]);
    failed(&second);
    assert!(
        stderr(&second).contains("already exists"),
        "{}",
        describe(&second)
    );

    // `init` registers the project for `list --all` and the desktop app.
    let registry = std::fs::read_to_string(env.root.join("config/projects.toml")).unwrap();
    assert!(registry.contains("shop"), "{registry}");
}

#[test]
fn init_supports_json_postgres_and_dash_c() {
    let env = Env::new();
    // `-C` lets any directory act as the project; here the temp root is not a repository.
    let out = env.run_in(
        &env.root,
        &[
            "--json",
            "-C",
            env.repo.to_str().unwrap(),
            "init",
            "--postgres",
            "external",
        ],
    );
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["version"], 1);
    assert_eq!(doc["command"], "init");
    assert_eq!(doc["project"]["name"], "repo");
    assert_eq!(doc["postgres"], "external");
    assert!(
        std::fs::read_to_string(env.config_path())
            .unwrap()
            .contains("mode = \"external\"")
    );
}

#[test]
fn legacy_configuration_is_an_error_that_mentions_0x() {
    let env = Env::new();
    std::fs::write(env.repo.join(".devflow.yml"), "services: []\n").unwrap();
    let out = env.run(&["list"]);
    failed(&out);
    assert!(stderr(&out).contains("0.x"), "{}", describe(&out));

    let out = env.run(&["--json", "list"]);
    failed(&out);
    let doc = json(&out);
    assert_eq!(doc["command"], "list");
    assert!(doc["error"].as_str().unwrap().contains("0.x"), "{doc}");
}

#[test]
fn shell_init_prints_the_wrapper() {
    let env = Env::new();
    for shell in ["zsh", "bash", "fish"] {
        let out = env.run(&["shell-init", shell]);
        ok(&out);
        let text = stdout(&out);
        assert!(text.contains("DEVFLOW_CD_FILE"), "{shell}: {text}");
        assert!(text.contains("command devflow"), "{shell}: {text}");
        assert!(text.contains(shell), "{shell}: {text}");
    }
    assert!(stdout(&env.run(&["shell-init", "zsh"])).contains("devflow() {"));
    assert!(stdout(&env.run(&["shell-init", "fish"])).contains("function devflow"));
    failed(&env.run(&["shell-init", "powershell"]));
}

#[test]
fn json_is_rejected_for_commands_that_cannot_emit_it() {
    let env = Env::new();
    for (args, command) in [
        (["--json", "shell-init", "zsh"].as_slice(), "shell-init"),
        (["--json", "completions", "bash"].as_slice(), "completions"),
        (["--json", "gui"].as_slice(), "gui"),
        (["--json", "db", "psql"].as_slice(), "db psql"),
    ] {
        let out = env.run(args);
        failed(&out);
        // The refusal itself honors the one-document contract.
        let doc = json(&out);
        assert_eq!(doc["command"], command, "{args:?}: {doc}");
        assert!(
            doc["error"].as_str().unwrap().contains("--json"),
            "{args:?}: {doc}"
        );
        assert!(
            stderr(&out).contains("--json"),
            "{args:?}: {}",
            describe(&out)
        );
    }
}

#[test]
fn completions_cover_the_binary() {
    let env = Env::new();
    for shell in ["bash", "zsh", "fish"] {
        let out = env.run(&["completions", shell]);
        ok(&out);
        assert!(stdout(&out).contains("devflow"), "{shell}");
    }
}

#[test]
fn usage_errors_do_not_collide_with_the_attention_exit_code() {
    let env = Env::new();
    let out = env.run(&["switch"]);
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    let out = env.run(&["no-such-command"]);
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
}

#[test]
fn early_json_errors_name_the_command_not_an_option_value() {
    let env = Env::new();
    // `list` is the directory given to -C, not the command that failed.
    for args in [
        ["--json", "-C", "list", "path"].as_slice(),
        ["-C", "db", "--json", "path"].as_slice(),
        ["--json", "-Clist", "path"].as_slice(),
    ] {
        let out = env.run(args);
        failed(&out);
        let doc = json(&out);
        assert_eq!(doc["command"], "path", "{args:?}: {doc}");
        assert!(
            doc["error"].as_str().unwrap().contains("<NAME>"),
            "{args:?}: {doc}"
        );
    }
    // A -C without a command is not aimed at the directory's name either.
    let out = env.run(&["--json", "-C", "list"]);
    failed(&out);
    assert_eq!(json(&out)["command"], "devflow");
}

#[test]
fn git_hook_entry_point_does_not_accept_json() {
    let env = Env::new();
    // One JSON document and exit 1, not a stderr line and exit 0; the hook
    // re-entry guard does not turn the refusal into a silent success.
    for guard in [false, true] {
        let mut command = env.command(&env.repo);
        command.args(["--json", "setup", "--if-new", "--from-git-hook"]);
        if guard {
            command.env("DEVFLOW_SETUP_ACTIVE", "1");
        }
        let out = command.output().unwrap();
        failed(&out);
        let doc = json(&out);
        assert_eq!(doc["command"], "setup", "{doc}");
        assert!(
            doc["error"].as_str().unwrap().contains("--from-git-hook"),
            "{doc}"
        );
        assert!(stderr(&out).contains("--json"), "{}", describe(&out));
    }
}

#[test]
fn git_hook_setup_never_fails_the_hook() {
    let env = Env::new();
    // Outside any project: one line on stderr, exit 0.
    let out = env.run_in(
        &env.root.join("home"),
        &["setup", "--if-new", "--from-git-hook"],
    );
    ok(&out);
    let text = stderr(&out);
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.contains("devflow setup"), "{text}");

    // Re-entry from a hook that devflow itself triggered is silent.
    let out = env
        .command(&env.repo)
        .args(["setup", "--if-new", "--from-git-hook"])
        .env("DEVFLOW_SETUP_ACTIVE", "1")
        .output()
        .unwrap();
    ok(&out);
    assert!(stderr(&out).is_empty(), "{}", describe(&out));
}

#[test]
fn db_commands_explain_a_missing_postgres_section() {
    let env = Env::new();
    env.init();
    for args in [
        ["db", "status"].as_slice(),
        ["db", "up"].as_slice(),
        ["db", "snapshot", "list"].as_slice(),
    ] {
        let out = env.run(args);
        failed(&out);
        assert!(
            stderr(&out).contains("[postgres]"),
            "{args:?}: {}",
            describe(&out)
        );
    }
    let out = env.run(&["--json", "db", "status"]);
    failed(&out);
    let doc = json(&out);
    assert_eq!(doc["version"], 1);
    assert_eq!(doc["command"], "db status");
    assert!(
        doc["error"].as_str().unwrap().contains("[postgres]"),
        "{doc}"
    );
}

#[test]
fn snapshot_deletion_needs_a_version() {
    let env = Env::new();
    env.init();
    // The reference is validated before any server is contacted.
    let mut text = std::fs::read_to_string(env.config_path()).unwrap();
    text.push_str("\n[postgres]\nmode = \"external\"\n");
    std::fs::write(env.config_path(), text).unwrap();
    let out = env.run(&["db", "snapshot", "delete", "seed"]);
    failed(&out);
    assert!(stderr(&out).contains("NAME@vN"), "{}", describe(&out));
}

// --- need the workspace pipeline of devflow-core -----------------------------

#[test]
fn list_json_has_the_versioned_envelope() {
    let env = Env::new();
    env.init();
    let out = env.run(&["--json", "list"]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["version"], 1);
    assert_eq!(doc["command"], "list");
    assert_eq!(doc["project"]["name"], "shop");
    let workspaces = doc["workspaces"].as_array().unwrap();
    assert_eq!(workspaces.len(), 1, "{doc}");
    assert_eq!(workspaces[0]["workspace"]["is_default"], true);
    assert!(stderr(&out).is_empty(), "{}", describe(&out));

    let out = env.run(&["--json", "list", "--all"]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["projects"][0]["name"], "shop");
    assert_eq!(
        doc["projects"][0]["workspaces"].as_array().unwrap().len(),
        1
    );
}

#[test]
fn switch_create_list_path_and_remove_without_postgres() {
    let env = Env::new();
    env.init();

    let out = env.run(&["--json", "--non-interactive", "switch", "-c", "feature/a"]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["version"], 1);
    assert_eq!(doc["command"], "switch");
    assert_eq!(doc["created"], true);
    assert_eq!(doc["status"], "ready", "{doc}");
    let path = PathBuf::from(doc["path"].as_str().unwrap());
    assert!(path.is_dir(), "{}", path.display());

    let doc = json(&env.run(&["--json", "list"]));
    let names: Vec<&str> = doc["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["workspace"]["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"feature/a"), "{names:?}");
    let item = doc["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["workspace"]["name"] == "feature/a")
        .unwrap();
    assert_eq!(item["status"], "ready");

    let human = env.run(&["list"]);
    ok(&human);
    let table = stdout(&human);
    let line = table.lines().find(|l| l.contains("feature/a")).unwrap();
    assert!(line.contains("ready"), "{table}");
    assert!(table.lines().next().unwrap().contains("STATUS"), "{table}");

    let out = env.run(&["path", "feature/a"]);
    ok(&out);
    assert!(same_path(&stdout(&out), &path), "{}", describe(&out));

    // Without the shell wrapper, `switch` prints the path on stdout.
    let out = env.run(&["--non-interactive", "switch", "feature/a"]);
    ok(&out);
    assert!(same_path(&stdout(&out), &path), "{}", describe(&out));

    // With the wrapper's handoff file, it writes the path there instead.
    let handoff = env.root.join("cd-file");
    std::fs::write(&handoff, "").unwrap();
    let out = env
        .command(&env.repo)
        .args(["--non-interactive", "switch", "feature/a"])
        .env("DEVFLOW_CD_FILE", &handoff)
        .output()
        .unwrap();
    ok(&out);
    assert!(stdout(&out).is_empty(), "{}", describe(&out));
    assert!(same_path(
        &std::fs::read_to_string(&handoff).unwrap(),
        &path
    ));

    // JSON mode never moves the caller's shell.
    std::fs::write(&handoff, "").unwrap();
    let out = env
        .command(&env.repo)
        .args(["--json", "switch", "feature/a"])
        .env("DEVFLOW_CD_FILE", &handoff)
        .output()
        .unwrap();
    ok(&out);
    assert!(std::fs::read_to_string(&handoff).unwrap().is_empty());

    // The workspace containing the caller's directory is protected.
    let out = env.run_in(&path, &["remove", "feature/a"]);
    failed(&out);
    assert!(path.is_dir());

    let out = env.run(&["--json", "remove", "feature/a"]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["command"], "remove");
    assert_eq!(doc["workspace"]["name"], "feature/a");
    assert!(!path.exists(), "{}", path.display());
    let doc = json(&env.run(&["--json", "list"]));
    assert_eq!(doc["workspaces"].as_array().unwrap().len(), 1);
}

#[test]
fn switching_to_a_missing_workspace_suggests_create() {
    let env = Env::new();
    env.init();
    let out = env.run(&["switch", "nope"]);
    failed(&out);
    let text = stderr(&out);
    assert!(text.contains("switch -c nope"), "{text}");
    failed(&env.run(&["path", "nope"]));
}

#[test]
fn untrusted_hooks_need_attention_until_trusted() {
    let env = Env::new();
    env.init();
    let text = std::fs::read_to_string(env.config_path()).unwrap();
    assert!(text.contains("setup = []"));
    std::fs::write(
        env.config_path(),
        text.replace("setup = []", "setup = [\"echo ran > hook-ran.txt\"]"),
    )
    .unwrap();

    // Non-interactive and untrusted: the worktree is kept, hooks do not run, exit 2.
    let out = env.run(&["--json", "--non-interactive", "switch", "-c", "feature/h"]);
    assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
    let doc = json(&out);
    assert_eq!(doc["status"], "needs_attention", "{doc}");
    let path = PathBuf::from(doc["path"].as_str().unwrap());
    assert!(path.is_dir());
    assert!(!path.join("hook-ran.txt").exists());

    // Human mode says what to do.
    let out = env.run(&["--non-interactive", "switch", "-c", "feature/h2"]);
    assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
    assert!(stderr(&out).contains("devflow trust"), "{}", describe(&out));
    assert!(
        !stdout(&out).is_empty(),
        "the path is still printed: {}",
        describe(&out)
    );

    let doc = json(&env.run(&["--json", "trust"]));
    assert_eq!(doc["command"], "trust");
    assert_eq!(doc["trust"]["state"], "untrusted", "{doc}");
    assert_eq!(doc["approved"], false);

    let doc = json(ok(&env.run(&["--json", "trust", "--yes"])));
    assert_eq!(doc["approved"], true);
    assert_eq!(doc["trust"]["state"], "trusted", "{doc}");

    // `setup` resumes the skipped phase and finishes.
    let out = env.run_in(&path, &["--json", "--non-interactive", "setup"]);
    ok(&out);
    assert_eq!(json(&out)["status"], "ready");
    assert!(path.join("hook-ran.txt").is_file());

    let doc = json(ok(&env.run(&["--json", "trust", "--revoke"])));
    assert_eq!(doc["revoked"], true);

    // DEVFLOW_TRUST_HOOKS=1 trusts for one run only.
    let out = env
        .command(&env.repo)
        .args(["--json", "--non-interactive", "switch", "-c", "feature/i"])
        .env("DEVFLOW_TRUST_HOOKS", "1")
        .output()
        .unwrap();
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["status"], "ready", "{doc}");
    assert!(
        Path::new(doc["path"].as_str().unwrap())
            .join("hook-ran.txt")
            .is_file()
    );
    assert_eq!(
        json(&env.run(&["--json", "trust"]))["trust"]["state"],
        "untrusted"
    );
}

#[test]
fn hook_install_and_uninstall_round_trip() {
    let env = Env::new();
    env.init();
    let doc = json(ok(&env.run(&["--json", "hook", "install"])));
    assert_eq!(doc["command"], "hook install");
    assert_eq!(doc["installed"], true);
    let hook = PathBuf::from(doc["path"].as_str().unwrap());
    assert!(hook.is_file(), "{}", hook.display());
    let doc = json(ok(&env.run(&["--json", "hook", "uninstall"])));
    assert_eq!(doc["removed"], true);
    assert!(!hook.exists());
}

#[test]
fn doctor_reports_checks() {
    let env = Env::new();
    env.init();
    let out = env.run(&["--json", "doctor"]);
    assert!(
        matches!(out.status.code(), Some(0 | 1)),
        "{}",
        describe(&out)
    );
    let doc = json(&out);
    assert_eq!(doc["command"], "doctor");
    assert!(
        doc["checks"].as_array().is_some_and(|c| !c.is_empty()),
        "{doc}"
    );
    assert_eq!(doc["ok"], out.status.success());
}

// --- review regressions -------------------------------------------------------

/// Run with `DEVFLOW_NON_INTERACTIVE=value`.
fn run_with_non_interactive(env: &Env, value: &str, args: &[&str]) -> Output {
    env.command(&env.repo)
        .args(args)
        .env("DEVFLOW_NON_INTERACTIVE", value)
        .output()
        .unwrap()
}

#[test]
fn non_interactive_environment_variable_accepts_boolean_spellings() {
    let env = Env::new();
    env.init();
    for value in ["1", "0", "true", "false", "TRUE", "yes", "no", "on", "off"] {
        let out = run_with_non_interactive(&env, value, &["--json", "list"]);
        assert!(
            out.status.success(),
            "DEVFLOW_NON_INTERACTIVE={value}\n{}",
            describe(&out)
        );
        assert_eq!(json(&out)["command"], "list");
    }
    // Anything else is still an error, and a JSON one when --json was asked for.
    let out = run_with_non_interactive(&env, "maybe", &["--json", "list"]);
    failed(&out);
    let doc = json(&out);
    assert_eq!(doc["command"], "list");
    assert!(
        doc["error"].as_str().unwrap().contains("--non-interactive"),
        "{doc}"
    );
}

#[test]
fn json_mode_reports_every_early_failure_as_one_document() {
    let env = Env::new();
    env.init();
    for (args, command, mentions) in [
        (vec!["--json", "path"], "path", "NAME"),
        (
            vec!["--json", "-C", "/nonexistent/devflow-dir", "list"],
            "list",
            "cannot use -C",
        ),
        (vec!["--json", "bogus"], "devflow", "bogus"),
        (
            vec!["--json", "db", "snapshot", "create"],
            "db snapshot create",
            "NAME",
        ),
        (vec!["--json", "--bogus", "list"], "list", "--bogus"),
        (vec!["list", "--oops", "--json"], "list", "--oops"),
        (
            vec!["--json", "db", "branch", "delete", "x"],
            "db branch delete",
            "x",
        ),
    ] {
        let out = env.run(&args);
        failed(&out);
        let doc = json(&out);
        assert_eq!(doc["version"], 1, "{args:?}");
        assert_eq!(doc["command"], command, "{args:?}: {doc}");
        let message = doc["error"].as_str().unwrap();
        assert!(message.contains(mentions), "{args:?}: {doc}");
        assert!(!message.starts_with("error:"), "{args:?}: {doc}");
        assert!(!stderr(&out).is_empty(), "{args:?}: {}", describe(&out));
    }

    // --help and --version keep clap's behavior.
    for flag in ["--help", "--version"] {
        let out = env.run(&["--json", flag]);
        ok(&out);
        assert!(
            serde_json::from_slice::<Value>(&out.stdout).is_err(),
            "{flag}: {}",
            describe(&out)
        );
        assert!(stdout(&out).contains("devflow") || stdout(&out).contains("Git worktrees"));
    }
}

#[test]
fn argument_errors_do_not_echo_credentials() {
    let env = Env::new();
    env.init();
    let url = "postgres://u:SECRETPW@h/db?password=SECRETQUERY";
    for args in [
        ["--json", "db", "branch", "delete", url].as_slice(),
        ["db", "branch", "delete", url].as_slice(),
    ] {
        let out = env.run(args);
        failed(&out);
        let text = describe(&out);
        assert!(
            !text.contains("SECRETPW") && !text.contains("SECRETQUERY"),
            "{args:?}\n{text}"
        );
        assert!(text.contains("invalid value"), "{args:?}\n{text}");
    }
}

#[test]
fn credentials_in_urls_are_masked_in_every_output_but_db_url() {
    let env = Env::new();
    env.init();
    // Attaching records the URL in the receipt, which later commands print.
    let attached = "postgres://demo@127.0.0.1:1/appdb?password=QUERYSECRET";
    let out = env.run(&["--non-interactive", "setup", "--database-url", attached]);
    ok(&out);
    for (args, label) in [
        (["--json", "list"].as_slice(), "list --json"),
        (["list"].as_slice(), "list"),
        (["--json", "doctor"].as_slice(), "doctor --json"),
    ] {
        let out = env.run(args);
        let text = describe(&out);
        assert!(!text.contains("QUERYSECRET"), "{label}:\n{text}");
    }
    let doc = json(&env.run(&["--json", "list"]));
    assert_eq!(
        doc["workspaces"][0]["receipt"]["data"]["url"],
        "postgres://demo@127.0.0.1:1/appdb?password=***"
    );
    // `db url` exists to hand the credential out.
    assert_eq!(stdout(&env.run(&["db", "url"])).trim(), attached);
    let doc = json(&env.run(&["--json", "db", "url"]));
    assert_eq!(doc["url"], attached);
}

#[test]
fn hook_output_is_redacted() {
    let env = Env::new();
    env.init();
    env.edit_config(
        "setup = []",
        "setup = [\"echo postgres://u:HUNTERTWO@h/db and password=QUERYSECRET\"]",
    );
    let out = env
        .command(&env.repo)
        .args(["--json", "--non-interactive", "switch", "-c", "feature/r"])
        .env("DEVFLOW_TRUST_HOOKS", "1")
        .output()
        .unwrap();
    ok(&out);
    let text = describe(&out);
    assert!(text.contains("postgres://u:***@h/db"), "{text}");
    assert!(
        !text.contains("HUNTERTWO") && !text.contains("QUERYSECRET"),
        "{text}"
    );
}

/// The environment of the process under test with a fake program first on PATH.
fn with_fake_path(command: &mut Command, programs: &[&FakeProgram], system: bool) {
    let mut dirs: Vec<String> = programs
        .iter()
        .map(|program| program.dir.display().to_string())
        .collect();
    if system {
        dirs.extend(["/usr/bin".to_string(), "/bin".to_string()]);
    }
    command.env("PATH", dirs.join(":"));
    for program in programs {
        command.env("FAKE_LOG", &program.log);
    }
}

/// Point the main checkout's receipt at `url`. The receipt is edited directly
/// (after a first `setup --database-url`), so forms that `setup` itself might
/// refuse still reach the commands that read the receipt.
fn attach(env: &Env, url: &str) {
    let receipt = env.repo.join(".git/devflow/receipt.json");
    if !receipt.exists() {
        ok(&env.run(&[
            "--non-interactive",
            "setup",
            "--database-url",
            "postgres://placeholder@127.0.0.1:1/placeholder",
        ]));
    }
    let mut doc: Value = serde_json::from_str(&std::fs::read_to_string(&receipt).unwrap()).unwrap();
    doc["data"]["url"] = Value::String(url.to_string());
    std::fs::write(&receipt, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
}

/// Mark the attached binding as a branch provisioned by the project's server.
fn provisioned(env: &Env) {
    let receipt = env.repo.join(".git/devflow/receipt.json");
    let mut doc: Value = serde_json::from_str(&std::fs::read_to_string(&receipt).unwrap()).unwrap();
    doc["data"]["branch_id"] = Value::String(uuid::Uuid::new_v4().to_string());
    std::fs::write(&receipt, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
}

#[cfg(unix)]
#[test]
fn db_psql_never_redirects_an_attached_url_into_the_managed_container() {
    let env = Env::new();
    env.init();
    env.edit_config(
        "[hooks]",
        "[postgres]\nmode = \"managed\"\nimage = \"postgres:18\"\n\n[hooks]",
    );
    attach(
        &env,
        "postgres://remote_user:pw@remote.example:6000/remote_db",
    );
    let docker = env.fake_program("docker", &recording_script(0));
    let mut command = env.command(&env.repo);
    command.args(["db", "psql"]);
    with_fake_path(&mut command, &[&docker], false);
    let out = command.output().unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("psql was not found"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!docker.log.exists(), "docker must not be invoked");
}

/// Marks a connection variable that was not set in the recorded environment.
#[cfg(unix)]
const UNSET: &str = "<unset>";

/// A fake `psql` or `docker`: records its arguments and every libpq
/// connection variable of its environment, then exits with `code`. Shell
/// builtins only, so it also runs where PATH holds nothing else.
#[cfg(unix)]
fn recording_script(code: i32) -> String {
    let names: Vec<&str> = devflow_core::conn::libpq_env_vars().collect();
    format!(
        r#"{{
  for a in "$@"; do printf 'ARG=%s\n' "$a"; done
  for v in {names}; do
    eval "val=\${{$v-{UNSET}}}"
    printf 'ENV=%s=%s\n' "$v" "$val"
  done
}} > "$FAKE_LOG"
exit {code}"#,
        names = names.join(" "),
    )
}

#[cfg(unix)]
impl FakeProgram {
    /// The connection variables set in the program's environment.
    fn connection_env(&self) -> std::collections::BTreeMap<String, String> {
        self.lines()
            .iter()
            .filter_map(|line| line.strip_prefix("ENV="))
            .filter_map(|pair| pair.split_once('='))
            .filter(|(_, value)| *value != UNSET)
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }
}

#[cfg(unix)]
fn vars(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

/// `devflow db psql` with a fake `psql` first on PATH.
#[cfg(unix)]
fn run_psql(env: &Env, psql: &FakeProgram, extra_env: &[(&str, &str)]) -> Output {
    let mut command = env.command(&env.repo);
    command.args(["db", "psql"]).envs(extra_env.iter().copied());
    with_fake_path(&mut command, &[psql], true);
    command.output().unwrap()
}

#[cfg(unix)]
#[test]
fn db_psql_passes_the_connection_in_the_environment_only() {
    let env = Env::new();
    env.init();
    attach(
        &env,
        "postgres://demo:s3cr%3At@127.0.0.1:1/appdb?sslmode=disable",
    );
    let psql = env.fake_program("psql", &recording_script(7));
    let tmp = env.root.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let out = run_psql(&env, &psql, &[("TMPDIR", tmp.to_str().unwrap())]);

    // psql's status is devflow's status.
    assert_eq!(out.status.code(), Some(7), "{}", describe(&out));
    let log = psql.lines().join("\n");
    assert!(psql.args().is_empty(), "connection data in argv:\n{log}");
    assert_eq!(
        psql.connection_env(),
        vars(&[
            ("PGDATABASE", "appdb"),
            ("PGHOST", "127.0.0.1"),
            ("PGPASSWORD", "s3cr:t"),
            ("PGPORT", "1"),
            ("PGSSLMODE", "disable"),
            ("PGUSER", "demo"),
        ]),
        "{log}"
    );
    // No temporary file of any kind, and nothing echoed.
    assert_eq!(std::fs::read_dir(&tmp).unwrap().count(), 0);
    assert!(!describe(&out).contains("s3cr"), "{}", describe(&out));
}

#[cfg(unix)]
#[test]
fn db_psql_reads_the_url_the_way_libpq_does() {
    let env = Env::new();
    env.init();
    let psql = env.fake_program("psql", &recording_script(0));
    for (url, expected) in [
        // A percent-encoded parameter name is still the password.
        (
            "postgres://demo@h:5/appdb?%70assword=ENCSECRET",
            vars(&[
                ("PGDATABASE", "appdb"),
                ("PGHOST", "h"),
                ("PGPASSWORD", "ENCSECRET"),
                ("PGPORT", "5"),
                ("PGUSER", "demo"),
            ]),
        ),
        // Query parameters override the URL's own components, as in libpq:
        // the password, the host and the port all follow the query.
        (
            "postgres://demo:authpw@h:5/appdb?password=querypw&host=otherhost&port=7",
            vars(&[
                ("PGDATABASE", "appdb"),
                ("PGHOST", "otherhost"),
                ("PGPASSWORD", "querypw"),
                ("PGPORT", "7"),
                ("PGUSER", "demo"),
            ]),
        ),
        // Without a password, nothing is invented: ~/.pgpass applies.
        (
            "postgres://demo@h:5/appdb",
            vars(&[
                ("PGDATABASE", "appdb"),
                ("PGHOST", "h"),
                ("PGPORT", "5"),
                ("PGUSER", "demo"),
            ]),
        ),
        // The key/value form is accepted too.
        (
            "host=h port=5 dbname=appdb user=demo password='a b'",
            vars(&[
                ("PGDATABASE", "appdb"),
                ("PGHOST", "h"),
                ("PGPASSWORD", "a b"),
                ("PGPORT", "5"),
                ("PGUSER", "demo"),
            ]),
        ),
        // Options libpq does not know stay out of the environment.
        (
            "postgres://demo@h/appdb?schema=public",
            vars(&[("PGDATABASE", "appdb"), ("PGHOST", "h"), ("PGUSER", "demo")]),
        ),
    ] {
        attach(&env, url);
        ok(&run_psql(&env, &psql, &[]));
        assert!(psql.args().is_empty(), "{url}: {:?}", psql.args());
        assert_eq!(psql.connection_env(), expected, "{url}");
    }
}

#[cfg(unix)]
#[test]
fn db_psql_ignores_connection_variables_inherited_from_the_caller() {
    let env = Env::new();
    env.init();
    let psql = env.fake_program("psql", &recording_script(0));
    // Every connection variable libpq knows, exported by the caller's shell.
    let inherited: Vec<(&str, &str)> = devflow_core::conn::libpq_env_vars()
        .map(|name| (name, "inherited"))
        .collect();

    attach(&env, "postgres://demo:wspw@h:5/appdb");
    ok(&run_psql(&env, &psql, &inherited));
    assert_eq!(
        psql.connection_env(),
        vars(&[
            ("PGDATABASE", "appdb"),
            ("PGHOST", "h"),
            ("PGPASSWORD", "wspw"),
            ("PGPORT", "5"),
            ("PGUSER", "demo"),
        ]),
        "an inherited variable overrode or joined the workspace's"
    );

    // A workspace without a password does not borrow the caller's.
    attach(&env, "postgres://demo@h:5/appdb");
    ok(&run_psql(&env, &psql, &inherited));
    assert!(!psql.connection_env().contains_key("PGPASSWORD"));
    assert!(!psql.connection_env().contains_key("PGPASSFILE"));
}

#[cfg(unix)]
#[test]
fn db_psql_rejects_urls_the_environment_cannot_express() {
    let env = Env::new();
    env.init();
    let psql = env.fake_program("psql", &recording_script(0));
    for (url, mention) in [
        (
            "postgres://demo@h:5/appdb?sslpassword=SSLSECRET",
            "sslpassword",
        ),
        ("postgres://demo:HOSTSECRET@h1,h2/appdb", "several hosts"),
    ] {
        attach(&env, url);
        let out = run_psql(&env, &psql, &[]);
        failed(&out);
        let text = describe(&out);
        assert!(text.contains(mention), "{url}:\n{text}");
        assert!(
            !text.contains("SSLSECRET") && !text.contains("HOSTSECRET"),
            "{text}"
        );
        assert!(!psql.log.exists(), "psql ran for {url}");
    }
}

#[cfg(unix)]
#[test]
fn db_psql_leaves_signal_handling_to_psql() {
    use std::os::unix::process::ExitStatusExt;
    // A test run started as a background job of a non-interactive shell ignores
    // SIGINT itself, and then an inherited disposition cannot be told apart.
    let probe = Command::new("sh")
        .args(["-c", "kill -INT $$; echo survived"])
        .output()
        .unwrap();
    if String::from_utf8_lossy(&probe.stdout).contains("survived") {
        eprintln!("SIGINT is ignored in this environment; skipping");
        return;
    }
    let env = Env::new();
    env.init();
    attach(&env, "postgres://demo:pw@h:5/appdb");
    let psql = env.fake_program(
        "psql",
        r#"printf 'started\n' > "$FAKE_LOG"
kill -INT $$
printf 'survived\n' >> "$FAKE_LOG""#,
    );
    let out = run_psql(&env, &psql, &[]);
    let log = std::fs::read_to_string(&psql.log).unwrap();
    // Ctrl-C reaches psql with its default disposition: it ends the program.
    assert_eq!(log, "started\n", "psql inherited an ignored SIGINT");
    assert!(
        out.status.signal() == Some(2) || out.status.code() == Some(130),
        "{}",
        describe(&out)
    );
}

#[cfg(unix)]
#[test]
fn db_psql_in_the_container_gets_the_connection_from_the_environment() {
    let env = Env::new();
    env.init();
    env.edit_config(
        "[hooks]",
        "[postgres]\nmode = \"managed\"\nimage = \"postgres:18\"\n\n[hooks]",
    );
    attach(
        &env,
        "postgres://demo:dockerpw@127.0.0.1:55432/appdb?sslmode=disable",
    );
    // A branch the managed server provisioned (only those go through its container).
    provisioned(&env);
    // No psql on PATH at all: only the fake docker.
    let docker = env.fake_program("docker", &recording_script(0));
    let mut command = env.command(&env.repo);
    command.args(["db", "psql"]);
    for name in devflow_core::conn::libpq_env_vars() {
        command.env(name, "inherited");
    }
    with_fake_path(&mut command, &[&docker], false);
    let out = command.output().unwrap();
    ok(&out);

    let args = docker.args();
    let log = docker.lines().join("\n");
    let exec = args.iter().position(|arg| arg == "exec").expect(&log);
    assert_eq!(&args[..2], ["compose", "-p"], "{args:?}");
    // Variables are forwarded by name; no value is on the command line.
    let rest = &args[exec + 1..];
    assert_eq!(&rest[rest.len() - 2..], ["pg", "psql"], "{args:?}");
    let forwarded = &rest[..rest.len() - 2];
    assert!(forwarded.chunks(2).all(|pair| pair[0] == "-e"), "{args:?}");
    let mut names: Vec<&str> = forwarded.chunks(2).map(|pair| pair[1].as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "PGDATABASE",
            "PGHOST",
            "PGPASSWORD",
            "PGPORT",
            "PGSSLMODE",
            "PGUSER"
        ],
        "{args:?}"
    );
    assert!(
        !args.iter().any(|arg| arg.contains("dockerpw")
            || arg.contains("inherited")
            || arg.contains("55432")),
        "connection data reached argv: {args:?}"
    );
    // The client's environment: the workspace's values, the server as seen
    // from inside its container, and nothing inherited.
    assert_eq!(
        docker.connection_env(),
        vars(&[
            ("PGDATABASE", "appdb"),
            ("PGHOST", "127.0.0.1"),
            ("PGPASSWORD", "dockerpw"),
            ("PGPORT", "5432"),
            ("PGSSLMODE", "disable"),
            ("PGUSER", "demo"),
        ]),
        "{log}"
    );
}

#[test]
fn switch_create_runs_switch_hooks_on_first_entry() {
    let env = Env::new();
    env.init();
    env.edit_config(
        "setup = []",
        "setup = []\nswitch = [\"echo entered >> switch-ran.txt\"]",
    );
    let trusted = |args: &[&str]| {
        env.command(&env.repo)
            .args(args)
            .env("DEVFLOW_TRUST_HOOKS", "1")
            .output()
            .unwrap()
    };

    let out = trusted(&["--json", "--non-interactive", "switch", "-c", "feature/s"]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["status"], "ready", "{doc}");
    assert_eq!(doc["created"], true);
    assert_eq!(doc["switch_hooks"]["phase"], "switch", "{doc}");
    assert_eq!(doc["switch_hooks"]["ok"], true, "{doc}");
    let path = PathBuf::from(doc["path"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(path.join("switch-ran.txt")).unwrap(),
        "entered\n"
    );

    // Switching to the existing workspace runs them again.
    ok(&trusted(&[
        "--json",
        "--non-interactive",
        "switch",
        "feature/s",
    ]));
    assert_eq!(
        std::fs::read_to_string(path.join("switch-ran.txt")).unwrap(),
        "entered\nentered\n"
    );

    // A failing switch hook is a warning: the workspace exists and is ready.
    env.edit_config("echo entered >> switch-ran.txt", "exit 3");
    let out = trusted(&["--json", "--non-interactive", "switch", "-c", "feature/f"]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["status"], "ready", "{doc}");
    assert_eq!(doc["switch_hooks"]["ok"], false, "{doc}");
    assert!(
        doc["warnings"][0].as_str().unwrap().contains("exit 3"),
        "{doc}"
    );

    // --no-hooks and untrusted hooks run nothing; untrusted says so.
    env.edit_config("exit 3", "echo entered >> switch-ran.txt");
    let out = trusted(&[
        "--json",
        "--non-interactive",
        "switch",
        "-c",
        "feature/n",
        "--no-hooks",
    ]);
    ok(&out);
    let doc = json(&out);
    assert!(doc.get("switch_hooks").is_none(), "{doc}");
    assert!(
        !Path::new(doc["path"].as_str().unwrap())
            .join("switch-ran.txt")
            .exists()
    );
    let out = env.run(&["--json", "--non-interactive", "switch", "-c", "feature/u"]);
    ok(&out);
    let doc = json(&out);
    assert!(doc.get("switch_hooks").is_none(), "{doc}");
    assert!(
        doc["warnings"][0].as_str().unwrap().contains("not trusted"),
        "{doc}"
    );
}

#[test]
fn a_busy_workspace_is_in_progress_not_an_error() {
    let env = Env::new();
    env.init();
    // Another devflow process holds the main checkout's setup lock.
    let lock = Receipt::try_lock(&env.repo.join(".git")).unwrap().unwrap();

    let out = env.run(&["--json", "--non-interactive", "setup"]);
    assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
    let doc = json(&out);
    assert_eq!(doc["command"], "setup");
    assert_eq!(doc["status"], "in_progress", "{doc}");
    assert!(doc.get("error").is_none(), "{doc}");
    assert!(
        doc["detail"].as_str().unwrap().contains("already running"),
        "{doc}"
    );

    let listed = json(&env.run(&["--json", "list"]));
    let default = listed["workspaces"][0]["workspace"]["name"]
        .as_str()
        .unwrap();
    let out = env.run(&["--json", "--non-interactive", "switch", default]);
    assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
    let doc = json(&out);
    assert_eq!(doc["command"], "switch");
    assert_eq!(doc["status"], "in_progress", "{doc}");
    assert_eq!(doc["workspace"]["name"], default, "{doc}");

    let out = env.run(&["--non-interactive", "setup"]);
    assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
    assert!(
        stderr(&out).contains("still being set up"),
        "{}",
        describe(&out)
    );

    // The Git hook stays silent and never fails Git.
    let out = env.run(&["setup", "--from-git-hook"]);
    ok(&out);
    assert!(stderr(&out).is_empty(), "{}", describe(&out));
    assert!(stdout(&out).is_empty(), "{}", describe(&out));

    drop(lock);
    ok(&env.run(&["--json", "--non-interactive", "setup"]));
}

#[test]
fn removal_protections_carry_a_typed_hint() {
    let env = Env::new();
    env.init();
    let doc = json(ok(&env.run(&[
        "--json",
        "--non-interactive",
        "switch",
        "-c",
        "feature/d",
    ])));
    let path = PathBuf::from(doc["path"].as_str().unwrap());
    std::fs::write(path.join("README.md"), "changed\n").unwrap();

    let out = env.run(&["remove", "feature/d"]);
    failed(&out);
    assert!(path.is_dir());
    assert!(
        stderr(&out).contains("hint: run `devflow remove feature/d --force`"),
        "{}",
        describe(&out)
    );
    let out = env.run(&["--json", "remove", "feature/d"]);
    failed(&out);
    let doc = json(&out);
    assert!(
        doc["hint"]
            .as_str()
            .unwrap()
            .contains("devflow remove feature/d --force"),
        "{doc}"
    );

    // A protection --force cannot override has no such hint.
    let out = env.run(&["--json", "remove", "main"]);
    failed(&out);
    assert!(json(&out).get("hint").is_none());

    ok(&env.run(&["--json", "remove", "feature/d", "--force"]));
    assert!(!path.exists());
}

#[test]
fn a_missing_config_suggests_init() {
    let env = Env::new();
    let out = env.run(&["list"]);
    failed(&out);
    assert!(
        stderr(&out).contains("hint: run `devflow init`"),
        "{}",
        describe(&out)
    );
    let doc = json(&env.run(&["--json", "list"]));
    assert!(
        doc["hint"].as_str().unwrap().contains("devflow init"),
        "{doc}"
    );

    let doc = json(&env.run(&["--json", "doctor"]));
    let project = doc["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "project")
        .unwrap();
    assert!(
        project["hint"].as_str().unwrap().contains("devflow init"),
        "{project}"
    );
}

#[test]
fn trust_shows_and_approves_the_complete_scope() {
    let env = Env::new();
    env.init();
    env.edit_config(
        "setup = []\nteardown = []",
        "setup = [\"echo SETUP-CMD\"]\nswitch = [\"echo SWITCH-CMD\"]\nteardown = [\"echo TEARDOWN-CMD\"]",
    );

    let doc = json(ok(&env.run(&["--json", "trust"])));
    assert_eq!(doc["approved"], false);
    assert_eq!(doc["scope"]["setup"][0], "echo SETUP-CMD", "{doc}");
    assert_eq!(doc["scope"]["switch"][0], "echo SWITCH-CMD", "{doc}");
    assert_eq!(doc["scope"]["teardown"][0], "echo TEARDOWN-CMD", "{doc}");
    assert!(doc["scope"]["timeout"].as_str().is_some(), "{doc}");
    let digest = doc["scope"]["digest"].as_str().unwrap().to_string();
    assert_eq!(digest.len(), 64);

    // Not trusted: the human listing shows everything the approval would cover.
    let out = ok(&env.run(&["trust"])).clone();
    let text = stdout(&out);
    for needle in ["SETUP-CMD", "SWITCH-CMD", "TEARDOWN-CMD", "timeout:"] {
        assert!(text.contains(needle), "{needle} missing:\n{text}");
    }
    assert!(text.contains("not trusted"), "{text}");

    // --yes prints the scope it approved.
    let out = ok(&env.run(&["trust", "--yes"])).clone();
    let text = stdout(&out);
    for needle in [
        "SETUP-CMD",
        "SWITCH-CMD",
        "TEARDOWN-CMD",
        "timeout:",
        "Approved.",
    ] {
        assert!(text.contains(needle), "{needle} missing:\n{text}");
    }
    let doc = json(ok(&env.run(&["--json", "trust"])));
    assert_eq!(doc["trust"]["state"], "trusted", "{doc}");
    assert_eq!(doc["scope"]["digest"], digest.as_str());

    // Editing only the teardown hooks needs a new approval.
    env.edit_config("echo TEARDOWN-CMD", "echo OTHER-TEARDOWN");
    let doc = json(ok(&env.run(&["--json", "trust"])));
    assert_eq!(doc["trust"]["state"], "untrusted", "{doc}");
    assert_eq!(doc["trust"]["previously_trusted"], true, "{doc}");
}
