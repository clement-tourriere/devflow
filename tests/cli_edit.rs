//! End-to-end tests of `devflow hooks` and `devflow env`: the commands that
//! list and edit the configuration for people and for agents. None needs
//! Docker or PostgreSQL.
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const HEAD: &str = "[project]\nname = \"shop\"\nid = \"0b3c9d2e-1111-4222-8333-444455556666\"\n";

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
}

impl Env {
    /// A Git repository with one commit and a `devflow.toml` of `HEAD` and `rest`.
    fn new(rest: &str) -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for dir in ["home", "config", "data"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = git2::Repository::init(&repo).unwrap();
        std::fs::write(repo.join("README.md"), "hello\n").unwrap();
        std::fs::write(repo.join("devflow.toml"), format!("{HEAD}{rest}")).unwrap();
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

    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_devflow"));
        command
            .current_dir(&self.repo)
            .env("HOME", self.root.join("home"))
            .env("DEVFLOW_CONFIG_DIR", self.root.join("config"))
            .env("DEVFLOW_DATA_DIR", self.root.join("data"))
            .env("NO_COLOR", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::null());
        for var in [
            "DEVFLOW_TRUST_HOOKS",
            "DEVFLOW_NON_INTERACTIVE",
            "DEVFLOW_SETUP_ACTIVE",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
        ] {
            command.env_remove(var);
        }
        command.args(args).output().unwrap()
    }

    /// Set the default workspace up (no services, no hooks), which writes its
    /// receipt: what `env list` resolves recipes against.
    fn set_up(&self) {
        let mut setup = Command::new(env!("CARGO_BIN_EXE_devflow"));
        let out = setup
            .current_dir(&self.repo)
            .env("HOME", self.root.join("home"))
            .env("DEVFLOW_CONFIG_DIR", self.root.join("config"))
            .env("DEVFLOW_DATA_DIR", self.root.join("data"))
            .env("DEVFLOW_TRUST_HOOKS", "1")
            .args(["--json", "--non-interactive", "setup"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            out.status.code().is_some_and(|c| c == 0 || c == 2),
            "{}",
            describe(&out)
        );
    }

    fn config(&self) -> String {
        std::fs::read_to_string(self.repo.join("devflow.toml")).unwrap()
    }

    fn local(&self) -> String {
        std::fs::read_to_string(self.repo.join("devflow.local.toml")).unwrap_or_default()
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

fn failed(out: &Output) -> &Output {
    assert_eq!(out.status.code(), Some(1), "{}", describe(out));
    out
}

fn json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not one JSON document ({error}):\n{}",
            describe(out)
        )
    })
}

fn names(doc: &Value, phase: &str) -> Vec<String> {
    doc["hooks"]["hooks"]
        .as_array()
        .or_else(|| doc["phases"][phase]["hooks"].as_array())
        .unwrap()
        .iter()
        .map(|h| {
            h["name"]
                .as_str()
                .or_else(|| h["run"].as_str())
                .unwrap()
                .to_string()
        })
        .collect()
}

const HOOKS: &str =
    "\n[hooks]\n# what a new worktree needs\nsetup = [\"bun install\"] # deps\nteardown = []\n";

#[test]
fn help_lists_hooks_and_env_apart_from_the_git_hook() {
    let env = Env::new("");
    let help = stdout(ok(&env.run(&["--help"])));
    for needle in ["hooks", "env", "hook "] {
        assert!(help.contains(needle), "{needle} in:\n{help}");
    }
    let text = stdout(ok(&env.run(&["hooks", "--help"])));
    for sub in ["list", "add", "update", "remove", "move"] {
        assert!(text.contains(sub), "{sub} in:\n{text}");
    }
    let text = stdout(ok(&env.run(&["env", "--help"])));
    for sub in ["list", "set", "unset", "preset"] {
        assert!(text.contains(sub), "{sub} in:\n{text}");
    }
}

#[test]
fn hooks_list_shows_each_phase_with_positions_and_the_file_hashes() {
    let env = Env::new(
        "\n[hooks]\nsetup = [\"bun install\", { name = \"migrate\", run = \"bun run db\", timeout = \"5m\" }]\n",
    );
    let doc = json(ok(&env.run(&["--json", "hooks", "list"])));
    assert_eq!(doc["version"], 1);
    assert_eq!(doc["command"], "hooks list");
    assert_eq!(doc["timeout"], "15m");
    assert_eq!(doc["phases"]["setup"]["source"], "project");
    let setup = doc["phases"]["setup"]["hooks"].as_array().unwrap();
    assert_eq!(setup.len(), 2);
    assert_eq!(setup[0]["position"], 1);
    assert_eq!(setup[0]["run"], "bun install");
    assert_eq!(setup[0]["name"], Value::Null);
    assert_eq!(setup[1]["position"], 2);
    assert_eq!(setup[1]["name"], "migrate");
    assert_eq!(setup[1]["timeout"], "5m");
    assert_eq!(setup[1]["effective_timeout"], "5m");
    assert_eq!(setup[0]["effective_timeout"], "15m");
    assert!(
        doc["phases"]["switch"]["hooks"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(doc["files"]["project"]["name"], "devflow.toml");
    assert_eq!(
        doc["files"]["project"]["sha256"].as_str().unwrap().len(),
        64
    );
    assert_eq!(doc["files"]["local"]["exists"], false);
    assert_eq!(doc["trust"]["state"], "untrusted");

    let human = stdout(ok(&env.run(&["hooks", "list"])));
    assert!(
        human.contains("setup\n   1  $ bun install\n   2  [migrate] $ bun run db"),
        "{human}"
    );
    assert!(
        human.contains("(timeout 5m)") && human.contains("switch\n  (none)"),
        "{human}"
    );
    assert!(human.contains("timeout: 15m per command"), "{human}");
}

#[test]
fn adding_a_hook_edits_the_file_in_place_and_says_it_needs_review() {
    let env = Env::new(HOOKS);
    let out = env.run(&[
        "--json",
        "hooks",
        "add",
        "setup",
        "bun run codegen",
        "--name",
        "codegen",
    ]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["command"], "hooks add");
    assert_eq!(doc["file"], "devflow.toml");
    assert_eq!(doc["target"], "project");
    assert_eq!(doc["changed"], true);
    assert_eq!(doc["phase"], "setup");
    assert_eq!(doc["hook"]["name"], "codegen");
    assert_eq!(doc["hook"]["run"], "bun run codegen");
    assert_eq!(names(&doc, "setup"), ["bun install", "codegen"]);
    assert_eq!(doc["trust"]["needs_review"], true);
    assert_eq!(doc["trust"]["review"], "devflow trust");
    assert_eq!(doc["trust"]["scope_changed"], true);

    let text = env.config();
    assert!(
        text.contains("# what a new worktree needs\nsetup = ["),
        "{text}"
    );
    assert!(text.contains("# deps"), "{text}");
    assert!(
        text.contains("{ name = \"codegen\", run = \"bun run codegen\" }"),
        "{text}"
    );
    assert!(text.contains("teardown = []"), "{text}");

    // The human form says what happened and what to do about trust.
    let out = env.run(&[
        "hooks",
        "add",
        "teardown",
        "docker compose down",
        "--timeout",
        "30s",
    ]);
    ok(&out);
    assert!(
        stdout(&out).contains("Added in hooks.teardown (devflow.toml)."),
        "{}",
        describe(&out)
    );
    let err = stderr(&out);
    assert!(
        err.contains("devflow trust") && err.contains("never approves"),
        "{err}"
    );
    assert!(env.config().contains("timeout = \"30s\""));
}

#[test]
fn hooks_are_updated_moved_and_removed_by_name_or_position() {
    let env = Env::new(
        "\n[hooks]\nsetup = [\n  { name = \"install\", run = \"bun install\" },\n  { name = \"gen\", run = \"bun run gen\" },\n  \"bun run seed\",\n]\n",
    );
    let positions = |env: &Env| -> Vec<String> {
        let doc = json(ok(&env.run(&["--json", "hooks", "list"])));
        names(&doc, "setup")
    };
    assert_eq!(positions(&env), ["install", "gen", "bun run seed"]);

    // Before and after a named command, and at a position.
    ok(&env.run(&[
        "hooks",
        "add",
        "setup",
        "echo first",
        "--name",
        "first",
        "--before",
        "install",
    ]));
    ok(&env.run(&[
        "hooks", "add", "setup", "echo mid", "--name", "mid", "--after", "2",
    ]));
    assert_eq!(
        positions(&env),
        ["first", "install", "mid", "gen", "bun run seed"]
    );
    // Move by name to a position, to first and to last.
    ok(&env.run(&["hooks", "move", "setup", "gen", "1"]));
    ok(&env.run(&["hooks", "move", "setup", "first", "last"]));
    assert_eq!(
        positions(&env),
        ["gen", "install", "mid", "bun run seed", "first"]
    );
    ok(&env.run(&["hooks", "move", "setup", "5", "first"]));
    assert_eq!(positions(&env)[0], "first");
    // Update keeps what is not changed.
    let out = env.run(&[
        "--json",
        "hooks",
        "update",
        "setup",
        "mid",
        "--run",
        "echo middle",
        "--timeout",
        "10s",
    ]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["hook"]["name"], "mid");
    assert_eq!(doc["hook"]["run"], "echo middle");
    assert_eq!(doc["hook"]["timeout"], "10s");
    ok(&env.run(&[
        "hooks",
        "update",
        "setup",
        "mid",
        "--clear-timeout",
        "--clear-name",
    ]));
    assert!(positions(&env).contains(&"echo middle".to_string()));
    // Remove by name and by position.
    ok(&env.run(&["hooks", "remove", "setup", "gen"]));
    ok(&env.run(&["hooks", "remove", "setup", "1"]));
    assert_eq!(positions(&env), ["install", "echo middle", "bun run seed"]);
    // Nothing to change is not an edit.
    let out = env.run(&["hooks", "update", "setup", "1"]);
    failed(&out);
    assert!(
        stderr(&out).contains("nothing to change"),
        "{}",
        describe(&out)
    );
}

#[test]
fn local_edits_go_to_the_local_file_and_say_what_that_means() {
    let env = Env::new(HOOKS);
    let out = env.run(&["--json", "hooks", "add", "setup", "echo mine", "--local"]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["file"], "devflow.local.toml");
    assert_eq!(doc["target"], "local");
    let notes = doc["notes"].as_array().unwrap();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0]
            .as_str()
            .unwrap()
            .contains("copied from devflow.toml")
    );
    assert_eq!(
        env.local(),
        "[hooks]\nsetup = [\"bun install\", \"echo mine\"]\n"
    );
    assert!(!env.config().contains("echo mine"));
    let list = json(ok(&env.run(&["--json", "hooks", "list"])));
    assert_eq!(list["phases"]["setup"]["source"], "local");
    assert_eq!(
        list["phases"]["setup"]["shadowed"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(list["files"]["local"]["exists"], true);
}

#[test]
fn an_edit_that_would_break_the_configuration_is_refused_and_leaves_the_file() {
    let env = Env::new(HOOKS);
    let before = env.config();
    for args in [
        &["hooks", "add", "setup", "echo x", "--timeout", "forever"][..],
        &["hooks", "add", "setup", "   "][..],
        &["hooks", "remove", "setup", "nope"][..],
        &["hooks", "remove", "setup", "9"][..],
        &["hooks", "move", "setup", "1", "0"][..],
        &["hooks", "add", "bogus", "echo x"][..],
        &["env", "set", "BAD NAME", "x"][..],
        &["env", "unset", "NOT_THERE"][..],
        &["env", "preset", "remove", "prisma"][..],
        &["env", "preset", "add", "rails"][..],
    ] {
        let out = env.run(args);
        failed(&out);
        assert_eq!(env.config(), before, "{args:?}\n{}", describe(&out));
    }
    // The JSON form names the command and carries the message, not the value.
    let out = env.run(&[
        "--json",
        "hooks",
        "add",
        "setup",
        "echo secret-sauce",
        "--timeout",
        "forever",
    ]);
    failed(&out);
    let doc = json(&out);
    assert_eq!(doc["command"], "hooks add");
    let error = doc["error"].as_str().unwrap();
    assert!(error.contains("hooks.setup[1].timeout"), "{error}");
    assert!(
        !error.contains("forever") && !error.contains("secret-sauce"),
        "{error}"
    );
    // A duplicate name.
    ok(&env.run(&["hooks", "add", "setup", "echo a", "--name", "dup"]));
    let out = env.run(&["--json", "hooks", "add", "setup", "echo b", "--name", "dup"]);
    failed(&out);
    assert!(
        json(&out)["error"]
            .as_str()
            .unwrap()
            .contains("is already used by")
    );
}

#[test]
fn if_unchanged_refuses_a_file_that_changed_since_it_was_listed() {
    let env = Env::new(HOOKS);
    let listed = json(ok(&env.run(&["--json", "hooks", "list"])));
    let hash = listed["files"]["project"]["sha256"]
        .as_str()
        .unwrap()
        .to_string();
    // Somebody edits the file after the listing.
    let theirs = format!("{}\n# edited meanwhile\n", env.config());
    std::fs::write(env.repo.join("devflow.toml"), &theirs).unwrap();
    let out = env.run(&[
        "--json",
        "hooks",
        "add",
        "setup",
        "echo x",
        "--if-unchanged",
        &hash,
    ]);
    failed(&out);
    let doc = json(&out);
    assert!(
        doc["error"].as_str().unwrap().contains("changed on disk"),
        "{doc}"
    );
    assert_eq!(env.config(), theirs);
    // With the current hash it goes through; an absent local file is `none`.
    let listed = json(ok(&env.run(&["--json", "hooks", "list"])));
    let hash = listed["files"]["project"]["sha256"]
        .as_str()
        .unwrap()
        .to_string();
    ok(&env.run(&["hooks", "add", "setup", "echo x", "--if-unchanged", &hash]));
    ok(&env.run(&[
        "hooks",
        "add",
        "setup",
        "echo y",
        "--local",
        "--if-unchanged",
        "none",
    ]));
    failed(&env.run(&[
        "hooks",
        "add",
        "setup",
        "echo z",
        "--local",
        "--if-unchanged",
        "none",
    ]));
}

#[test]
fn env_recipes_and_presets_are_listed_set_and_unset() {
    let env = Env::new(
        "\n[services.db]\nengine = \"postgres\"\n\n[env]\n# for the ORM\nDIRECT_URL = \"${DATABASE_URL}\"\n",
    );
    let doc = json(ok(&env.run(&["--json", "env", "list"])));
    assert_eq!(doc["command"], "env list");
    assert_eq!(doc["recipes"][0]["key"], "DIRECT_URL");
    assert_eq!(doc["recipes"][0]["template"], "${DATABASE_URL}");
    assert_eq!(doc["recipes"][0]["source"], "project");
    assert!(doc["presets"].as_array().unwrap().is_empty());
    let available: Vec<&str> = doc["available_presets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert!(available.contains(&"prisma") && available.contains(&"db_parts"));
    assert!(
        doc["references"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r == "DATABASE_URL")
    );

    let out = env.run(&["--json", "env", "set", "APP_DB", "${DATABASE_URL}"]);
    ok(&out);
    let set = json(&out);
    assert_eq!(set["command"], "env set");
    assert_eq!(set["changed"], true);
    assert_eq!(set["trust"]["needs_review"], true);
    assert_eq!(set["recipes"].as_array().unwrap().len(), 2);
    assert!(env.config().contains(
        "# for the ORM\nDIRECT_URL = \"${DATABASE_URL}\"\nAPP_DB = \"${DATABASE_URL}\"\n"
    ));

    // A preset that clashes with a recipe is refused; a free one is added.
    failed(&env.run(&["env", "preset", "add", "prisma"]));
    let out = env.run(&["--json", "env", "preset", "add", "spring"]);
    ok(&out);
    assert_eq!(json(&out)["command"], "env preset add");
    assert_eq!(json(&out)["presets"][0], "spring");
    assert!(env.config().contains("presets = [\"spring\"]"));
    let out = env.run(&["--json", "env", "preset", "remove", "spring"]);
    ok(&out);
    assert!(!env.config().contains("presets"));
    let out = env.run(&["--json", "env", "unset", "APP_DB"]);
    ok(&out);
    assert_eq!(json(&out)["command"], "env unset");
    assert!(!env.config().contains("APP_DB"));

    // Human output.
    let human = stdout(ok(&env.run(&["env", "list"])));
    assert!(
        human.contains("RECIPE") && human.contains("DIRECT_URL"),
        "{human}"
    );
    ok(&env.run(&["env", "preset", "add", "spring"]));
    let human = stdout(ok(&env.run(&["env", "list"])));
    assert!(
        human.contains("presets\n  spring  SPRING_DATASOURCE_URL"),
        "{human}"
    );
}

#[test]
fn credentials_go_to_the_local_file_and_never_into_output() {
    let env = Env::new("\n[services.db]\nengine = \"postgres\"\n");
    let secret = "hunter2hunter2"; // kingfisher:ignore
    let url = format!("postgres://app:{secret}@db.example.com/shop"); // kingfisher:ignore
    let out = env.run(&["--json", "env", "set", "LEGACY_DB", &url]);
    failed(&out);
    let all = format!("{}{}", stdout(&out), stderr(&out));
    assert!(!all.contains(secret), "{all}");
    assert!(json(&out)["error"].as_str().unwrap().contains("--local"));
    assert!(!env.config().contains(secret));
    // The local file takes it, and the output still does not show it.
    let out = env.run(&["--json", "env", "set", "LEGACY_DB", &url, "--local"]);
    ok(&out);
    assert!(!stdout(&out).contains(secret) && !stderr(&out).contains(secret));
    assert!(env.local().contains(secret));
    let list = env.run(&["--json", "env", "list"]);
    ok(&list);
    assert!(!stdout(&list).contains(secret), "{}", stdout(&list));
    let human = env.run(&["env", "list"]);
    assert!(!stdout(&human).contains(secret) && !stderr(&human).contains(secret));
    // Hooks with a credential: same rule.
    let out = env.run(&["hooks", "add", "setup", &format!("psql {url} -f seed.sql")]);
    failed(&out);
    assert!(!stderr(&out).contains(secret), "{}", stderr(&out));
}

#[test]
fn env_list_resolves_recipes_in_a_workspace_with_credentials_masked() {
    let env = Env::new(
        "\n[env]\nPROJECT_LABEL = \"app-${DEVFLOW_PROJECT}\"\nWORKSPACE = \"${DEVFLOW_WORKSPACE}\"\n",
    );
    // Before any setup there is nothing to resolve against.
    let doc = json(ok(&env.run(&["--json", "env", "list"])));
    assert_eq!(doc["preview"], Value::Null);
    env.set_up();
    let doc = json(ok(&env.run(&["--json", "env", "list"])));
    assert!(doc["preview_workspace"].as_str().is_some(), "{doc}");
    let variables = doc["preview"]["variables"].as_array().unwrap();
    let value = |key: &str| {
        variables
            .iter()
            .find(|v| v["key"] == key)
            .map(|v| v["value"].as_str().unwrap().to_string())
    };
    assert_eq!(value("PROJECT_LABEL").as_deref(), Some("app-shop"));
    assert!(value("WORKSPACE").is_some());
    let human = stdout(ok(&env.run(&["env", "list"])));
    assert!(human.contains("app-shop"), "{human}");
}

/// A credential-looking value that appears nowhere by chance.
const SENTINEL: &str = "sentinel-7f3a91"; // kingfisher:ignore

/// Everything a command printed.
fn printed(out: &Output) -> String {
    format!("{}{}", stdout(out), stderr(out))
}

fn variable(doc: &Value, key: &str) -> Option<Value> {
    doc["preview"]["variables"]
        .as_array()?
        .iter()
        .find(|v| v["key"] == key)
        .cloned()
}

#[test]
fn a_literal_credential_under_a_secret_name_is_never_printed_unless_asked() {
    let env = Env::new("\n[env]\nPROJECT_LABEL = \"app-${DEVFLOW_PROJECT}\"\n");

    // Setting it does not echo it, in JSON or in words.
    let out = env.run(&["--json", "env", "set", "APP_PASSWORD", SENTINEL, "--local"]);
    ok(&out);
    assert!(!printed(&out).contains(SENTINEL), "{}", describe(&out));
    assert!(
        json(&out)["recipes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r.get("template").is_none()),
        "{}",
        describe(&out)
    );
    let out = env.run(&["env", "set", "APP_PASSWORD", SENTINEL, "--local"]);
    ok(&out);
    assert!(!printed(&out).contains(SENTINEL), "{}", describe(&out));
    // Nor does a name that says nothing about its value (the response lists
    // keys; `env list` is where values are read).
    let out = env.run(&["--json", "env", "set", "FEATURE_FLAG", "sentinel-flag"]);
    ok(&out);
    assert!(
        !printed(&out).contains("sentinel-flag"),
        "{}",
        describe(&out)
    );
    // The credential is in the local file, where it belongs.
    assert!(
        env.local()
            .contains(&format!("APP_PASSWORD = \"{SENTINEL}\""))
    );
    assert!(!env.config().contains(SENTINEL));

    // Listing masks it: the key decides, the text has nothing to find.
    let check_listing = || {
        let out = env.run(&["--json", "env", "list"]);
        ok(&out);
        assert!(!printed(&out).contains(SENTINEL), "{}", describe(&out));
        let doc = json(&out);
        assert_eq!(doc["revealed"], false);
        let recipe = |key: &str| {
            doc["recipes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["key"] == key)
                .cloned()
                .unwrap()
        };
        let password = recipe("APP_PASSWORD");
        assert_eq!(password["template"], "***");
        assert_eq!(password["masked"], true);
        assert_eq!(password["source"], "local");
        // What is not a credential stays readable.
        assert_eq!(
            recipe("PROJECT_LABEL")["template"],
            "app-${DEVFLOW_PROJECT}"
        );
        assert_eq!(recipe("PROJECT_LABEL")["masked"], false);
        assert_eq!(recipe("FEATURE_FLAG")["template"], "sentinel-flag");
        let human = env.run(&["env", "list"]);
        ok(&human);
        assert!(!printed(&human).contains(SENTINEL), "{}", describe(&human));
        assert!(stdout(&human).contains("APP_PASSWORD (local)"));
        assert!(
            stderr(&human).contains("--show-values"),
            "{}",
            describe(&human)
        );
        doc
    };
    check_listing();

    // After setup there is a receipt, and the preview resolves the recipe.
    env.set_up();
    let doc = check_listing();
    let preview = variable(&doc, "APP_PASSWORD").expect("previewed");
    assert_eq!(preview["value"], "***");
    assert_eq!(preview["masked"], true);
    assert_eq!(
        variable(&doc, "PROJECT_LABEL").unwrap()["value"],
        "app-shop"
    );

    // Asking to see the values prints them, and says what that means.
    let out = env.run(&["--json", "env", "list", "--show-values"]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["revealed"], true);
    let password = doc["recipes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == "APP_PASSWORD")
        .unwrap();
    assert_eq!(password["template"], SENTINEL);
    assert_eq!(password["masked"], false);
    assert_eq!(variable(&doc, "APP_PASSWORD").unwrap()["value"], SENTINEL);
    assert_eq!(variable(&doc, "APP_PASSWORD").unwrap()["masked"], false);
    let human = env.run(&["env", "list", "--show-values"]);
    ok(&human);
    assert!(stdout(&human).contains(SENTINEL), "{}", describe(&human));
    assert!(
        stderr(&human).contains("may include credentials"),
        "{}",
        describe(&human)
    );
    // A later edit does not echo it either.
    let out = env.run(&["--json", "env", "unset", "APP_PASSWORD", "--local"]);
    ok(&out);
    assert!(!printed(&out).contains(SENTINEL), "{}", describe(&out));
}

#[test]
fn removing_the_last_local_preset_switches_the_committed_ones_off_here() {
    let env = Env::new(
        "\n[services.db]\nengine = \"postgres\"\n\n[env]\npresets = [\"prisma\"]\n\n[hooks]\nsetup = [\"bun install\"]\n",
    );
    let names = |doc: &Value| -> Vec<String> {
        doc["presets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap().to_string())
            .collect()
    };
    let doc = json(ok(&env.run(&["--json", "env", "list"])));
    assert_eq!(names(&doc), ["prisma"]);

    let out = env.run(&["--json", "env", "preset", "remove", "prisma", "--local"]);
    ok(&out);
    let removed = json(&out);
    assert_eq!(removed["target"], "local");
    assert!(
        removed["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().is_some_and(|n| n.contains("env.presets = []"))),
        "{}",
        describe(&out)
    );
    // The local file says "no presets"; writing nothing would inherit them.
    assert!(env.local().contains("presets = []"), "{}", env.local());
    assert!(env.config().contains("presets = [\"prisma\"]"));
    let doc = json(ok(&env.run(&["--json", "env", "list"])));
    assert!(names(&doc).is_empty(), "prisma is still on: {doc}");
    assert_eq!(doc["presets_source"], "local");
    assert_eq!(doc["shadowed_presets"][0], "prisma");

    // Hooks behave the same way: the last local removal leaves `[]`.
    ok(&env.run(&["hooks", "remove", "setup", "1", "--local"]));
    assert!(env.local().contains("setup = []"), "{}", env.local());
    let hooks = json(ok(&env.run(&["--json", "hooks", "list"])));
    assert!(
        hooks["phases"]["setup"]["hooks"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{hooks}"
    );
    assert_eq!(hooks["phases"]["setup"]["source"], "local");
}

#[test]
fn a_secret_name_in_the_committed_file_is_masked_in_listings_too() {
    // A reference is fine in the committed file and is shown as written; a
    // value that slipped in by hand is masked all the same.
    let env = Env::new(
        "\n[env]\nSESSION_SECRET = \"${DEVFLOW_PROJECT}\"\nSTRIPE_KEY = \"sentinel-by-hand\"\n",
    );
    let out = env.run(&["--json", "env", "list"]);
    ok(&out);
    assert!(
        !printed(&out).contains("sentinel-by-hand"),
        "{}",
        describe(&out)
    );
    let doc = json(&out);
    assert_eq!(doc["recipes"][0]["template"], "${DEVFLOW_PROJECT}");
    assert_eq!(doc["recipes"][1]["template"], "***");
}

#[test]
fn the_trust_review_masks_credential_named_variables_like_env_list() {
    let env = Env::new("\n[env]\nPROJECT_LABEL = \"app-${DEVFLOW_PROJECT}\"\n");
    // Names the generic JSON key classifier does not know are credentials too.
    let literals = [
        ("DB_PASS", "sentinel-pass-1"),       // kingfisher:ignore
        ("SESSION_KEY", "sentinel-key-2"),    // kingfisher:ignore
        ("MY_CREDENTIAL", "sentinel-cred-3"), // kingfisher:ignore
        ("SENTRY_DSN", "sentinel-dsn-4"),     // kingfisher:ignore
    ];
    for (name, value) in literals {
        ok(&env.run(&["env", "set", "--local", name, value]));
    }
    ok(&env.run(&["env", "set", "CACHE_PASS", "${PGPASSWORD}"]));

    // `env list` masks them, and so do both forms of the review.
    let listed = json(ok(&env.run(&["--json", "env", "list"])));
    for (name, _) in literals {
        let recipe = listed["recipes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["key"] == name)
            .unwrap();
        assert_eq!(recipe["template"], "***", "{name}");
    }

    let out = env.run(&["--json", "trust"]);
    ok(&out);
    let doc = json(&out);
    let vars = &doc["scope"]["env"]["vars"];
    for (name, value) in literals {
        assert_eq!(vars[name], "***", "{name}: {doc}");
        assert!(
            !printed(&out).contains(value),
            "{value}\n{}",
            describe(&out)
        );
    }
    // Readable where it holds nothing secret: the label, and a reference.
    assert_eq!(vars["PROJECT_LABEL"], "app-${DEVFLOW_PROJECT}");
    assert_eq!(vars["CACHE_PASS"], "${PGPASSWORD}");

    let text = env.run(&["trust"]);
    ok(&text);
    for (name, value) in literals {
        assert!(
            stdout(&text).contains(&format!("{name} = ***")),
            "{name}\n{}",
            describe(&text)
        );
        assert!(!printed(&text).contains(value), "{}", describe(&text));
    }
    assert!(stdout(&text).contains("PROJECT_LABEL = app-${DEVFLOW_PROJECT}"));

    // The approval is of the real scope: the digest shown is the one that is
    // approved, and changing the literal reopens the review.
    let digest = doc["scope"]["digest"].as_str().unwrap().to_string();
    ok(&env.run(&["trust", "--yes"]));
    let approved = json(ok(&env.run(&["--json", "trust"])));
    assert_eq!(approved["trust"]["state"], "trusted", "{approved}");
    assert_eq!(approved["scope"]["digest"], digest.as_str());
    ok(&env.run(&["env", "set", "--local", "DB_PASS", "sentinel-pass-changed"])); // kingfisher:ignore
    let changed = json(ok(&env.run(&["--json", "trust"])));
    assert_eq!(changed["trust"]["state"], "untrusted", "{changed}");
    assert_ne!(changed["scope"]["digest"], digest.as_str());
    assert_eq!(changed["scope"]["env"]["vars"]["DB_PASS"], "***");
}

#[test]
fn a_preset_written_as_a_variable_is_warned_about_in_env_list_and_doctor() {
    let env = Env::new("\n[env]\npreset = \"prisma\"\n");
    let want = "did you mean `presets = [\"prisma\"]`?";

    let listed = json(ok(&env.run(&["--json", "env", "list"])));
    let warnings: Vec<&str> = listed["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        warnings.iter().any(|w| w.contains(want)),
        "{warnings:?}\n{listed}"
    );
    // It is still a recipe: nothing is refused, and no preset is on.
    assert!(listed["presets"].as_array().unwrap().is_empty());

    let human = env.run(&["env", "list"]);
    ok(&human);
    assert!(
        stderr(&human).contains("warning:") && stderr(&human).contains(want),
        "{}",
        describe(&human)
    );

    // `doctor` runs without the project being set up; Docker may be absent,
    // so only the check is read.
    let doctor = json(&env.run(&["--json", "doctor"]));
    let check = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "env-presets")
        .unwrap_or_else(|| panic!("no env-presets check in {doctor}"));
    assert_eq!(check["status"], "warn");
    assert!(check["detail"].as_str().unwrap().contains(want), "{check}");

    // The fix removes the warning.
    ok(&env.run(&["env", "unset", "preset"]));
    ok(&env.run(&["env", "preset", "add", "prisma"]));
    let listed = json(ok(&env.run(&["--json", "env", "list"])));
    assert!(
        listed
            .get("warnings")
            .is_none_or(|w| w.as_array().is_some_and(|w| w.is_empty())),
        "{listed}"
    );
}

#[test]
fn the_review_message_follows_the_trust_state() {
    let env = Env::new(HOOKS);
    // Not approved yet: listing says so.
    let human = env.run(&["hooks", "list"]);
    assert!(
        stderr(&human).contains("not approved"),
        "{}",
        stderr(&human)
    );
    ok(&env.run(&["trust", "--yes"]));
    let listed = json(ok(&env.run(&["--json", "hooks", "list"])));
    assert_eq!(listed["trust"]["state"], "trusted");
    // A change to what runs reopens the review; devflow never approves it.
    let out = env.run(&["--json", "hooks", "add", "setup", "echo changed"]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["trust"]["needs_review"], true);
    assert_eq!(doc["trust"]["state"]["state"], "untrusted");
    assert_eq!(doc["trust"]["state"]["previously_trusted"], true);
    // Changing it back is the approved content again: nothing to review.
    let out = env.run(&["--json", "hooks", "remove", "setup", "2"]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["trust"]["needs_review"], false);
    assert_eq!(doc["trust"]["state"]["state"], "trusted");
    let err = stderr(&env.run(&["hooks", "add", "setup", "echo again"]));
    assert!(err.contains("`devflow trust`"), "{err}");
}
