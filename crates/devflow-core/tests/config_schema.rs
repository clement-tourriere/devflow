//! The JSON Schema of `devflow.toml`: the committed copies are the generated
//! ones, the shipped examples and the fixtures validate against it, and what it
//! refuses the parser refuses too.
use devflow_core::config::{Config, valid_env_name, valid_mysql_database, valid_service_key};
use devflow_core::config_schema::{self, Document};
use jsonschema::Validator;
use serde_json::Value;
use std::path::{Path, PathBuf};

fn repo(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path)
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn fixture(name: &str) -> String {
    read(&repo("crates/devflow-core/tests/fixtures/config").join(name))
}

fn validator(document: Document) -> Validator {
    jsonschema::draft7::new(&config_schema::schema(document).unwrap())
        .unwrap_or_else(|e| panic!("the {document:?} schema is not a valid draft-07 schema: {e}"))
}

/// A TOML document as the JSON value a schema is checked against.
fn json_of(toml_text: &str) -> Value {
    let table: toml::Table = toml_text
        .parse()
        .unwrap_or_else(|e| panic!("not TOML: {e}\n{toml_text}"));
    serde_json::to_value(table).unwrap()
}

/// What the schema says about a project file, as the messages of its errors.
fn problems(validator: &Validator, toml_text: &str) -> Vec<String> {
    let instance = json_of(toml_text);
    validator
        .iter_errors(&instance)
        .map(|e| format!("{} at {}", e, e.instance_path))
        .collect()
}

/// The example with the commented-out table that starts at `header` switched on.
fn uncommented(text: &str, header: &str) -> String {
    let mut inside = false;
    let mut lines = Vec::new();
    for line in text.lines() {
        if line.starts_with(header) {
            inside = true;
        } else if inside && !line.starts_with("# ") {
            inside = false;
        }
        match line
            .strip_prefix("# ")
            .filter(|_| inside && !line.contains("Manage with"))
        {
            Some(rest) => lines.push(rest),
            None => lines.push(line),
        }
    }
    lines.join("\n")
}

// -- the committed files ------------------------------------------------------

fn first_difference(committed: &str, generated: &str) -> String {
    let mut left = committed.lines();
    let mut right = generated.lines();
    for number in 1.. {
        match (left.next(), right.next()) {
            (None, None) => return "the files differ only in their end".into(),
            (a, b) if a == b => {}
            (a, b) => {
                return format!(
                    "first difference at line {number}:\n  committed: {}\n  generated: {}",
                    a.unwrap_or("(end of file)"),
                    b.unwrap_or("(end of file)")
                );
            }
        }
    }
    unreachable!()
}

#[test]
fn the_committed_schemas_are_the_generated_ones() {
    for (document, flag) in [(Document::Project, ""), (Document::Local, " --local")] {
        let path = repo(document.committed_path());
        let committed = std::fs::read_to_string(&path).unwrap_or_default();
        let generated = config_schema::render(document).unwrap();
        assert!(
            committed == generated,
            "{} is out of date with the configuration types in crates/devflow-core/src/config.rs.\n\
             Regenerate it and commit the result:\n\n    \
             cargo run -q -p devflow -- config schema{flag} > {}\n\n{}",
            document.committed_path(),
            document.committed_path(),
            first_difference(&committed, &generated)
        );
    }
}

#[test]
fn both_schemas_are_valid_draft_07_and_say_so() {
    for document in [Document::Project, Document::Local] {
        let schema = config_schema::schema(document).unwrap();
        assert_eq!(
            schema["$schema"], "http://json-schema.org/draft-07/schema#",
            "{document:?}"
        );
        assert!(jsonschema::meta::is_valid(&schema), "{document:?}");
        assert_eq!(schema["additionalProperties"], false, "{document:?}");
    }
}

#[test]
fn descriptions_say_nothing_of_the_rust_they_came_from() {
    for document in [Document::Project, Document::Local] {
        let text = config_schema::render(document).unwrap();
        for leftover in [
            "DEFAULT_",
            "valid_",
            "_IMAGE",
            "_URL_ENV",
            "[`",
            "`]",
            "CheckoutBackend`",
        ] {
            // The variables a recipe may name are not constants: only look for
            // the Rust ones.
            let found = text
                .match_indices(leftover)
                .filter(|(at, _)| !text[at.saturating_sub(12)..*at].contains("SHADOW"))
                .count();
            assert!(
                found == 0 || leftover == "CheckoutBackend`" && document == Document::Local,
                "{document:?}: {found} description(s) still mention `{leftover}`"
            );
        }
    }
}

// -- what the schema accepts ------------------------------------------------------

#[test]
fn the_shipped_examples_validate() {
    let project = validator(Document::Project);
    let example = read(&repo("examples/devflow.toml"));
    let documents = [
        ("examples/devflow.toml", example.clone()),
        (
            "examples/devflow.external.toml",
            read(&repo("examples/devflow.external.toml")),
        ),
        (
            "examples/devflow.toml with [services.cache]",
            uncommented(&example, "# [services.cache]"),
        ),
        (
            "examples/devflow.toml with [services.sql]",
            uncommented(&example, "# [services.sql]"),
        ),
    ];
    for (name, text) in documents {
        let found = problems(&project, &text);
        assert!(found.is_empty(), "{name}: {found:#?}");
        // And the parser agrees that these are configurations.
        Config::parse(&text).unwrap_or_else(|e| panic!("{name}: {e:#}"));
    }
}

#[test]
fn the_fixtures_and_templates_the_parser_loads_validate() {
    let project = validator(Document::Project);
    let id = uuid::Uuid::new_v4();
    let documents = [
        ("fixtures/current.toml", fixture("current.toml")),
        (
            "fixtures/container-services.toml",
            fixture("container-services.toml"),
        ),
        ("template, no postgres", Config::template("shop", id, None)),
        (
            "template, managed",
            Config::template(
                "shop",
                id,
                Some(devflow_core::config::PostgresMode::Managed),
            ),
        ),
        (
            "template, external",
            Config::template(
                "shop",
                id,
                Some(devflow_core::config::PostgresMode::External),
            ),
        ),
    ];
    for (name, text) in documents {
        Config::parse(&text).unwrap_or_else(|e| panic!("{name}: {e:#}"));
        let found = problems(&project, &text);
        assert!(found.is_empty(), "{name}: {found:#?}");
    }
}

/// Every section, with every key set to something the parser accepts.
const EVERYTHING: &str = r#"
[project]
name = "shop"
id = "0b3c9d2e-7f4a-4c1e-9a52-6d1f3b8e2a40"

[worktree]
dir = "../{repo}.worktrees"
copy = [".env", "node_modules"]
port_range = [41000, 41999]
focus_ports = [3000, 5173]

[services.db]
engine = "postgres"
mode = "external"
image = "postgres:18"
admin_url = "postgres://prov@db.internal:5432/postgres"
app_host = "db.example"
app_port = 5432
bind = "127.0.0.1"
port = 0
database = "shop"
extra_databases = ["test", "shadow"]
on_busy_source = "wait"
busy_wait = "45s"
wal_log_below = "256MiB"
require_cow = true
cow = "verified"

[services.cache]
engine = "valkey"
image = "valkey/valkey:8-alpine"
env = "CACHE_URL"
require_cow = true

[services.queue]
engine = "redis"

[services.sql]
engine = "mysql"
database = "shop"
env = "MYSQL_URL"

[services.file]
engine = "sqlite"
path = "db/development.sqlite3"
env = "SQLITE_URL"
path_env = "SQLITE_PATH"
busy_wait = "10s"
require_cow = false

[hooks]
setup = ["mise install", { name = "migrate", run = "bun run db:migrate", timeout = "5m" }]
switch = ["true"]
teardown = []
timeout = "20m"

[env]
presets = ["prisma", "laravel"]
API_ORIGIN = "http://localhost:3000"
DIRECT = "${DATABASE_URL}"

[pitchfork]
autostart = true

[data]
noninteractive_from = "snapshot:seed"
noninteractive_max_age = "7d"
noninteractive_fallback = "empty"

[integrations]
git_hook = "background"
"#;

#[test]
fn a_configuration_that_uses_every_key_validates() {
    // The parser takes all of it only if the services do not share variables:
    // the document above gives each its own. `checkout` is the local file's.
    let project = validator(Document::Project);
    let found = problems(&project, EVERYTHING);
    assert!(found.is_empty(), "{found:#?}");
    // ... and so does the parser, so the document means what it says.
    let parsed = Config::parse(EVERYTHING);
    assert!(
        parsed.is_ok(),
        "{:#}",
        parsed.err().map(|e| format!("{e:#}")).unwrap_or_default()
    );
}

// -- what the schema refuses -------------------------------------------------------

/// A project file made of `body` after a valid `[project]`.
fn project_with(body: &str) -> String {
    format!("[project]\nname = \"shop\"\nid = \"0b3c9d2e-7f4a-4c1e-9a52-6d1f3b8e2a40\"\n{body}\n")
}

/// One row of the table below: a document and what each reader says of it.
struct Case {
    name: &'static str,
    text: String,
    /// Whether `Config::parse` takes it.
    parser: bool,
    /// Whether the project schema takes it. Differs from `parser` only for the
    /// rules a schema cannot state (see the module documentation of
    /// `config_schema`).
    schema: bool,
}

fn case(name: &'static str, text: String, parser: bool, schema: bool) -> Case {
    Case {
        name,
        text,
        parser,
        schema,
    }
}

fn project_cases() -> Vec<Case> {
    vec![
        // The schema and the parser refuse alike.
        case(
            "an unknown top-level section",
            project_with("[surprise]\nx = 1"),
            false,
            false,
        ),
        case(
            "the pre-release [postgres] table",
            project_with("[postgres]\nmode = \"managed\""),
            false,
            false,
        ),
        case(
            "a project without [project]",
            "[worktree]\ncopy = []\n".into(),
            false,
            false,
        ),
        case(
            "a project without an id",
            "[project]\nname = \"shop\"\n".into(),
            false,
            false,
        ),
        case(
            "an id that is not a UUID",
            "[project]\nname = \"shop\"\nid = \"not-a-uuid\"\n".into(),
            false,
            false,
        ),
        case(
            "a nil id",
            "[project]\nname = \"shop\"\nid = \"00000000-0000-0000-0000-000000000000\"\n".into(),
            false,
            true,
        ),
        case(
            "an empty project name",
            "[project]\nname = \"  \"\nid = \"0b3c9d2e-7f4a-4c1e-9a52-6d1f3b8e2a40\"\n".into(),
            false,
            false,
        ),
        case(
            "an unknown key in [worktree]",
            project_with("[worktree]\ndirectory = \"x\""),
            false,
            false,
        ),
        case(
            "the machine-local checkout key in the committed file",
            project_with("[worktree]\ncheckout = \"git\""),
            false,
            false,
        ),
        case(
            "a port range of strings",
            project_with("[worktree]\nport_range = [\"a\", \"b\"]"),
            false,
            false,
        ),
        case(
            "a port range with three ports",
            project_with("[worktree]\nport_range = [41000, 41500, 41999]"),
            false,
            false,
        ),
        case(
            "a privileged port in the range",
            project_with("[worktree]\nport_range = [80, 8000]"),
            false,
            false,
        ),
        case(
            "a privileged focus port",
            project_with("[worktree]\nfocus_ports = [443]"),
            false,
            false,
        ),
        case(
            "a repeated focus port",
            project_with("[worktree]\nfocus_ports = [3000, 3000]"),
            false,
            false,
        ),
        case(
            "a copy path that leaves the worktree",
            project_with("[worktree]\ncopy = [\"../secrets\"]"),
            false,
            false,
        ),
        case(
            "an absolute copy path",
            project_with("[worktree]\ncopy = [\"/etc/passwd\"]"),
            false,
            false,
        ),
        case(
            "a service without an engine",
            project_with("[services.db]\nmode = \"managed\""),
            false,
            false,
        ),
        case(
            "an engine devflow does not have",
            project_with("[services.cache]\nengine = \"memcached\""),
            false,
            false,
        ),
        case(
            "a misspelled key of a postgres service",
            project_with("[services.db]\nengine = \"postgres\"\nimmage = \"postgres:18\""),
            false,
            false,
        ),
        case(
            "a postgres key on a valkey service",
            project_with("[services.cache]\nengine = \"valkey\"\nport = 6379"),
            false,
            false,
        ),
        case(
            "mysql's database on a redis service",
            project_with("[services.cache]\nengine = \"redis\"\ndatabase = \"x\""),
            false,
            false,
        ),
        case(
            "a service key with capitals",
            project_with("[services.Main_DB]\nengine = \"postgres\""),
            false,
            false,
        ),
        case(
            "a service key that is too long",
            project_with(&format!(
                "[services.{}]\nengine = \"postgres\"",
                "a".repeat(32)
            )),
            false,
            false,
        ),
        case(
            "a service that is not a table",
            project_with("[services]\ndb = \"postgres\""),
            false,
            false,
        ),
        case(
            "a mode that is not managed or external",
            project_with("[services.db]\nengine = \"postgres\"\nmode = \"remote\""),
            false,
            false,
        ),
        case(
            "an on_busy_source that is not a policy",
            project_with("[services.db]\nengine = \"postgres\"\non_busy_source = \"kill\""),
            false,
            false,
        ),
        case(
            "a port that does not fit",
            project_with("[services.db]\nengine = \"postgres\"\nport = 70000"),
            false,
            false,
        ),
        case(
            "a scratch database called main",
            project_with("[services.db]\nengine = \"postgres\"\nextra_databases = [\"main\"]"),
            false,
            false,
        ),
        case(
            "a scratch database named after a template",
            project_with(
                "[services.db]\nengine = \"postgres\"\nextra_databases = [\"template_x\"]",
            ),
            false,
            false,
        ),
        case(
            "a scratch database listed twice",
            project_with(
                "[services.db]\nengine = \"postgres\"\nextra_databases = [\"test\", \"test\"]",
            ),
            false,
            false,
        ),
        case(
            "a scratch database with capitals",
            project_with("[services.db]\nengine = \"postgres\"\nextra_databases = [\"Test\"]"),
            false,
            false,
        ),
        case(
            "an image that starts with a dash",
            project_with("[services.cache]\nengine = \"valkey\"\nimage = \"--privileged\""),
            false,
            false,
        ),
        case(
            "a connection variable that is not a name",
            project_with("[services.cache]\nengine = \"valkey\"\nenv = \"1CACHE\""),
            false,
            false,
        ),
        case(
            "a mysql schema the server owns",
            project_with("[services.sql]\nengine = \"mysql\"\ndatabase = \"mysql\""),
            // The server's own schemas are a name list, not a pattern.
            false,
            true,
        ),
        case(
            "a mysql schema with capitals",
            project_with("[services.sql]\nengine = \"mysql\"\ndatabase = \"Shop\""),
            false,
            false,
        ),
        case(
            "a sqlite service without a path",
            project_with("[services.file]\nengine = \"sqlite\""),
            false,
            false,
        ),
        case(
            "a sqlite path that leaves the worktree",
            project_with("[services.file]\nengine = \"sqlite\"\npath = \"../x.db\""),
            false,
            false,
        ),
        case(
            "a sqlite path that is a directory",
            project_with("[services.file]\nengine = \"sqlite\"\npath = \"db/\""),
            false,
            false,
        ),
        case(
            "a server key on a sqlite service",
            project_with("[services.file]\nengine = \"sqlite\"\npath = \"a.db\"\nport = 5432"),
            false,
            false,
        ),
        case(
            "a hook that is a number",
            project_with("[hooks]\nsetup = [1]"),
            false,
            false,
        ),
        case(
            "a hook table without run",
            project_with("[hooks]\nsetup = [{ name = \"x\" }]"),
            false,
            false,
        ),
        case(
            "a hook table with an unknown key",
            project_with("[hooks]\nsetup = [{ run = \"x\", cwd = \"y\" }]"),
            false,
            false,
        ),
        case(
            "an empty hook",
            project_with("[hooks]\nsetup = [\" \"]"),
            false,
            false,
        ),
        case(
            "a hook name that is too long",
            project_with(&format!(
                "[hooks]\nsetup = [{{ name = \"{}\", run = \"x\" }}]",
                "n".repeat(65)
            )),
            false,
            false,
        ),
        case(
            "an unknown key in [hooks]",
            project_with("[hooks]\npost_create = [\"x\"]"),
            false,
            false,
        ),
        case(
            "a preset devflow does not have",
            project_with("[env]\npresets = [\"rails\"]"),
            false,
            false,
        ),
        case(
            "a preset listed twice",
            project_with("[env]\npresets = [\"prisma\", \"prisma\"]"),
            false,
            false,
        ),
        case(
            "a recipe that is not text",
            project_with("[env]\nPORT = 3000"),
            false,
            false,
        ),
        case(
            "a recipe named like no variable",
            project_with("[env]\n\"not a name\" = \"x\""),
            false,
            false,
        ),
        case(
            "autostart that is not a boolean",
            project_with("[pitchfork]\nautostart = \"yes\""),
            false,
            false,
        ),
        case(
            "a git hook mode that does not exist",
            project_with("[integrations]\ngit_hook = \"never\""),
            false,
            false,
        ),
        case(
            "a data fallback that does not exist",
            project_with("[data]\nnoninteractive_fallback = \"live\""),
            false,
            false,
        ),
        // Accepted by both.
        case("only [project]", project_with(""), true, true),
        case(
            "a hook table with a name",
            project_with("[hooks]\nsetup = [{ name = \"m\", run = \"x\" }]"),
            true,
            true,
        ),
        case(
            "a preset and a recipe",
            project_with("[env]\npresets = [\"prisma\"]\nAPI = \"http://x\""),
            true,
            true,
        ),
        case(
            "a service key with a dash and digits",
            project_with("[services.cache-2]\nengine = \"valkey\""),
            true,
            true,
        ),
        case(
            "a 31-character service key",
            project_with(&format!(
                "[services.{}]\nengine = \"valkey\"",
                "a".repeat(31)
            )),
            true,
            true,
        ),
        case(
            "mysql with a database",
            project_with("[services.sql]\nengine = \"mysql\"\ndatabase = \"shop_2\""),
            true,
            true,
        ),
        // What only the parser can see.
        case(
            "two postgres services",
            project_with(
                "[services.db]\nengine = \"postgres\"\n[services.replica]\nengine = \"postgres\"",
            ),
            false,
            true,
        ),
        case(
            "two services exporting one variable",
            project_with("[services.a]\nengine = \"valkey\"\n[services.b]\nengine = \"redis\""),
            false,
            true,
        ),
        case(
            "a hook timeout that is not a duration",
            project_with("[hooks]\ntimeout = \"soon\""),
            false,
            true,
        ),
        case(
            "a recipe that references a variable devflow does not generate",
            project_with("[env]\nA = \"${NOPE}\""),
            false,
            true,
        ),
        case(
            "a recipe named after a variable devflow owns",
            project_with("[env]\nDATABASE_URL = \"x\""),
            false,
            true,
        ),
        case(
            "the same hook name twice in a phase",
            project_with(
                "[hooks]\nsetup = [{ name = \"a\", run = \"x\" }, { name = \"a\", run = \"y\" }]",
            ),
            false,
            true,
        ),
        case(
            "a busy_wait without the wait policy",
            project_with("[services.db]\nengine = \"postgres\"\nbusy_wait = \"5s\""),
            false,
            true,
        ),
    ]
}

#[test]
fn the_schema_and_the_parser_agree_on_a_table_of_configurations() {
    let project = validator(Document::Project);
    let mut wrong = Vec::new();
    for Case {
        name,
        text,
        parser,
        schema,
    } in project_cases()
    {
        let parsed = Config::parse(&text).is_ok();
        let found = problems(&project, &text);
        if parsed != parser {
            wrong.push(format!(
                "{name}: the parser says {parsed}, the table {parser}"
            ));
        }
        if found.is_empty() != schema {
            wrong.push(format!(
                "{name}: the schema says {}, the table {schema}: {found:?}",
                found.is_empty()
            ));
        }
        // The one thing that must hold whatever the table says: the schema
        // never refuses what the parser takes.
        if parsed && !found.is_empty() {
            wrong.push(format!(
                "{name}: the parser takes it and the schema refuses it: {found:?}"
            ));
        }
    }
    assert!(wrong.is_empty(), "{:#?}", wrong);
}

#[test]
fn every_fixture_the_parser_refuses_for_its_shape_is_refused_by_the_schema() {
    let project = validator(Document::Project);
    // Refused by the parser for a reason a schema can state ...
    for name in [
        "bad-key.toml",
        "container-services-postgres-option.toml",
        "legacy-array-of-services.toml",
        "legacy-services-of-strings.toml",
        "missing-engine.toml",
        "mixed-current-and-legacy.toml",
        "prerelease-postgres-table.toml",
        "unknown-field.toml",
        "unsupported-engine.toml",
    ] {
        let text = fixture(name);
        assert!(Config::parse(&text).is_err(), "{name}");
        assert!(
            !problems(&project, &text).is_empty(),
            "{name}: the schema takes what the parser refuses"
        );
    }
    // ... and the two the parser refuses for a rule across services.
    for name in ["two-postgres.toml", "container-services-same-variable.toml"] {
        let text = fixture(name);
        assert!(Config::parse(&text).is_err(), "{name}");
        assert!(
            problems(&project, &text).is_empty(),
            "{name}: if the schema learned this rule, move the fixture to the other list"
        );
    }
}

// -- the rules the schema restates, against the code that decides ---------------------

/// Whether `name` passes `schema` as the property name of `[services]`.
fn service_key_passes(validator: &Validator, key: &str) -> bool {
    let text = project_with(&format!("[services.\"{key}\"]\nengine = \"valkey\""));
    validator.is_valid(&json_of(&text))
}

#[test]
fn the_service_key_rule_is_the_one_validate_applies() {
    let project = validator(Document::Project);
    let keys = [
        "db",
        "cache",
        "a",
        "cache-2",
        "cache_2",
        "a-",
        "9db",
        "Db",
        "dB",
        "-a",
        "_a",
        "a b",
        "a.b",
        "",
        "é",
        "up",
        "status",
        "help",
        "x1234567890123456789012345678901",
        "x12345678901234567890123456789012",
    ];
    for key in keys {
        // The schema states the shape; the reserved words are the parser's
        // (they are free for a postgres service).
        let shape = valid_service_key(key);
        assert_eq!(
            service_key_passes(&project, key),
            shape,
            "service key {key:?}"
        );
    }
}

#[test]
fn the_variable_and_database_name_rules_are_the_ones_validate_applies() {
    let project = validator(Document::Project);
    for name in [
        "REDIS_URL",
        "_x",
        "x1",
        "1x",
        "x-y",
        "",
        "a b",
        "é",
        "QUEUE_URL",
        "X",
    ] {
        let text = project_with(&format!(
            "[services.cache]\nengine = \"valkey\"\nenv = \"{name}\""
        ));
        assert_eq!(
            project.is_valid(&json_of(&text)),
            valid_env_name(name),
            "variable {name:?}"
        );
        // `[env]` takes the same names as recipe keys.
        let text = project_with(&format!("[env]\n\"{name}\" = \"x\""));
        assert_eq!(
            project.is_valid(&json_of(&text)),
            valid_env_name(name),
            "recipe {name:?}"
        );
    }
    for name in [
        "shop",
        "shop_2",
        "Shop",
        "2shop",
        "",
        "a-b",
        "sys",
        "mysql",
        "x".repeat(48).as_str(),
        "x".repeat(49).as_str(),
    ] {
        let text = project_with(&format!(
            "[services.sql]\nengine = \"mysql\"\ndatabase = \"{name}\""
        ));
        let schema = project.is_valid(&json_of(&text));
        // The server's own schemas are names, not a shape.
        let reserved = ["mysql", "sys", "information_schema", "performance_schema"];
        assert_eq!(
            schema,
            valid_mysql_database(name) || reserved.contains(&name),
            "mysql database {name:?}"
        );
    }
}

// -- devflow.local.toml ----------------------------------------------------------------

#[test]
fn the_overlay_schema_takes_what_the_overlay_may_say() {
    let local = validator(Document::Local);
    for (name, text) in [
        ("nothing", ""),
        ("the native checkout", "[worktree]\ncheckout = \"git\""),
        ("the default checkout", "[worktree]\ncheckout = \"libgit2\""),
        (
            "the admin URL of a service the project file defines",
            "[services.db]\nadmin_url = \"postgres://p@db/postgres\"",
        ),
        (
            "a declared copy-on-write capability",
            "[services.db]\ncow = \"verified\"",
        ),
        (
            "the admin URL with the engine spelled out",
            "[services.db]\nengine = \"postgres\"\nadmin_url = \"postgres://p@db/postgres\"",
        ),
        (
            "a hook list that replaces the committed one",
            "[hooks]\nsetup = [\"true\"]",
        ),
        ("an env recipe", "[env]\nLOCAL_ONLY = \"1\""),
        ("a preset", "[env]\npresets = [\"spring\"]"),
        ("pitchfork autostart", "[pitchfork]\nautostart = true"),
    ] {
        let found = problems(&local, text);
        assert!(found.is_empty(), "{name}: {found:#?}");
        // The parser takes the same overlay over a project file.
        let layered = Config::parse_layers(
            &project_with("[services.db]\nengine = \"postgres\"\nmode = \"external\""),
            "devflow.toml",
            Some(text),
            "devflow.local.toml",
        );
        assert!(layered.is_ok(), "{name}: {:#}", layered.unwrap_err());
    }
}

#[test]
fn the_overlay_schema_refuses_what_the_parser_refuses_in_the_overlay() {
    let local = validator(Document::Local);
    for (name, text) in [
        ("an unknown section", "[surprise]\nx = 1"),
        ("an unknown key", "[worktree]\ndirectory = \"x\""),
        (
            "a checkout that does not exist",
            "[worktree]\ncheckout = \"svn\"",
        ),
        (
            "a key of another engine",
            "[services.cache]\nengine = \"valkey\"\nport = 1",
        ),
        (
            "an engine that does not exist",
            "[services.cache]\nengine = \"memcached\"",
        ),
        ("a hook that is a number", "[hooks]\nsetup = [1]"),
        (
            "a hook table without run: the overlay's list replaces the committed one whole",
            "[hooks]\nsetup = [{ name = \"x\" }]",
        ),
        ("a bad service key", "[services.Cache]\nengine = \"valkey\""),
    ] {
        assert!(!problems(&local, text).is_empty(), "{name}");
        let layered = Config::parse_layers(
            &project_with("[services.db]\nengine = \"postgres\""),
            "devflow.toml",
            Some(text),
            "devflow.local.toml",
        );
        assert!(layered.is_err(), "{name}: the parser takes it");
    }
}

#[test]
fn the_project_schema_forbids_checkout_and_the_overlay_documents_it() {
    let project = config_schema::schema(Document::Project).unwrap();
    let local = config_schema::schema(Document::Local).unwrap();
    let in_project = &project["definitions"]["WorktreeSection"]["properties"]["checkout"];
    let in_local = &local["definitions"]["WorktreeSection"]["properties"]["checkout"];
    assert_eq!(in_project["not"], serde_json::json!({}));
    assert!(
        in_project["description"]
            .as_str()
            .is_some_and(|d| d.contains("devflow.local.toml") && d.contains("only")),
        "{in_project}"
    );
    assert!(
        in_local["description"]
            .as_str()
            .is_some_and(|d| d.contains("Machine-local") && d.contains("only file that may set")),
        "{in_local}"
    );
    // The values are the code's: the same two backends.
    let backends = &local["definitions"]["CheckoutBackend"]["enum"];
    assert_eq!(*backends, serde_json::json!(["libgit2", "git"]));
    assert!(project["definitions"].get("CheckoutBackend").is_none());
}
