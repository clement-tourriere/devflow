//! The JSON Schema of `devflow.toml` and of the machine-local
//! `devflow.local.toml`, derived from the types in [`crate::config`].
//!
//! The structure (sections, keys, `deny_unknown_fields`, the engine-tagged
//! `[services.<key>]` tables, value types, the doc comments as descriptions)
//! comes from `#[derive(JsonSchema)]` on the configuration types, so a key added
//! to a section appears here without anyone remembering to. What serde cannot
//! say about its own types is added in this module:
//!
//! - `[env]` and a hook entry have hand-written (de)serializers, so their
//!   schemas are written here: `NAME = "value"` recipes beside `presets`, and a
//!   hook that is a string or a `{name, run, timeout}` table;
//! - the rules `validate()` checks that a schema can express (the shape of a
//!   service key, a variable name, a scratch database, a port, the keys that
//!   only `mysql` has) are applied in [`constrain`] to named places that must
//!   exist: renaming a field makes the generator fail, and the test that
//!   compares the committed files with the generated ones fails with it, so a
//!   rule cannot be silently dropped;
//! - the two files differ: `devflow.toml` requires `[project]`, forbids the
//!   machine-local `[worktree] checkout` and tags every service by `engine`;
//!   `devflow.local.toml` is an overlay merged over it before anything is
//!   parsed, so no section is required (a service may be just the keys it
//!   overrides) and it is the one file that may set `checkout`.
//!
//! What a schema cannot see stays with the parser: durations and sizes,
//! at-most-one postgres and mysql service, two services exporting one variable,
//! `${NAME}` references in recipes, reserved variable names, 0.x shapes. A
//! document that passes the schema can still be refused; the tests hold the
//! other direction for a corpus: what the parser refuses for a reason the schema
//! can state, the schema refuses too.
use crate::config::{
    Config, DEFAULT_BUSY_WAIT, DEFAULT_CACHE_ENV, DEFAULT_MYSQL_ENV, DEFAULT_SQLITE_BUSY_WAIT,
    DEFAULT_SQLITE_ENV, DEFAULT_SQLITE_PATH_ENV, DEFAULT_WAL_LOG_BELOW, EnvPreset, EnvSection,
    HookEntry, MAX_KEY_LEN, MYSQL_IMAGE, MYSQL_URL_ENV, REDIS_IMAGE, SERVICE_SUBCOMMANDS,
    SQLITE_URL_ENV, VALKEY_IMAGE,
};
use anyhow::{Context, Result};
use schemars::{JsonSchema, Schema, SchemaGenerator, generate::SchemaSettings, json_schema};
use serde_json::{Map, Value, json};
use std::borrow::Cow;
use std::collections::BTreeSet;

/// Where the committed copies live, relative to the repository root.
pub const PROJECT_SCHEMA_PATH: &str = "docs/devflow.schema.json";
/// See [`PROJECT_SCHEMA_PATH`].
pub const LOCAL_SCHEMA_PATH: &str = "docs/devflow.local.schema.json";
/// Where the schema of the project file is published, for the `#:schema` line
/// of an editor.
pub const PROJECT_SCHEMA_URL: &str =
    "https://raw.githubusercontent.com/clement-tourriere/devflow/main/docs/devflow.schema.json";
/// See [`PROJECT_SCHEMA_URL`].
pub const LOCAL_SCHEMA_URL: &str = "https://raw.githubusercontent.com/clement-tourriere/devflow/main/docs/devflow.local.schema.json";

/// Which file a schema describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Document {
    /// `devflow.toml` (or `.devflow.toml`): committed, complete.
    Project,
    /// `devflow.local.toml`: machine-local, merged over the project file.
    Local,
}

impl Document {
    /// The committed file that holds this schema, from the repository root.
    pub fn committed_path(self) -> &'static str {
        match self {
            Document::Project => PROJECT_SCHEMA_PATH,
            Document::Local => LOCAL_SCHEMA_PATH,
        }
    }

    /// The URL of the published schema, for an editor's `#:schema` line.
    pub fn url(self) -> &'static str {
        match self {
            Document::Project => PROJECT_SCHEMA_URL,
            Document::Local => LOCAL_SCHEMA_URL,
        }
    }
}

/// `^[A-Za-z_][A-Za-z0-9_]*$`: what `valid_env_name` accepts.
const NAME_PATTERN: &str = r"^[A-Za-z_][A-Za-z0-9_]*$";
/// A path that is absolute or leaves the worktree (`validate_relative`).
const OUTSIDE_PATTERN: &str = r"^/|(^|/)\.\.(/|$)";
/// A port devflow hands out or forwards: not a privileged one.
const PORT: fn() -> Value = || json!({ "type": "integer", "minimum": 1024, "maximum": 65535 });

// -- the types serde reads by hand --------------------------------------------

impl JsonSchema for EnvSection {
    fn schema_name() -> Cow<'static, str> {
        "EnvSection".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let presets = generator.subschema_for::<EnvPreset>();
        json_schema!({
            "description": "Extra variables for `.devflow/env`: `NAME = \"value\"` recipes built from the variables devflow generates (`${DATABASE_URL}`, `${PGHOST}`, …, one pass, no defaults, never another recipe), and framework presets. Recipes and presets are part of what `devflow trust` approves.",
            "type": "object",
            "properties": {
                "presets": {
                    "description": "Framework presets rendered from the PostgreSQL connection. A preset cannot set a name that a recipe or another preset sets.",
                    "type": "array",
                    "items": presets,
                    "uniqueItems": true
                }
            },
            "propertyNames": { "pattern": NAME_PATTERN },
            "additionalProperties": {
                "description": "A recipe: the value template of the variable named by the key. `${NAME}` references a variable devflow generates.",
                "type": "string"
            }
        })
    }
}

impl JsonSchema for HookEntry {
    fn schema_name() -> Cow<'static, str> {
        "HookEntry".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "One hook command, run with `sh -c` (no templating): a string, or a table when it deserves a name in logs and output or a timeout of its own.",
            "anyOf": [
                {
                    "description": "The command.",
                    "type": "string",
                    "pattern": "\\S"
                },
                {
                    "type": "object",
                    "properties": {
                        "name": {
                            "description": "Shown in logs and output; unique within the phase.",
                            "type": "string",
                            "minLength": 1,
                            "maxLength": 64
                        },
                        "run": {
                            "description": "The shell command.",
                            "type": "string",
                            "pattern": "\\S"
                        },
                        "timeout": {
                            "description": "Overrides `[hooks] timeout` for this command (a duration such as \"30s\" or \"5m\").",
                            "type": "string"
                        }
                    },
                    "required": ["run"],
                    "additionalProperties": false
                }
            ]
        })
    }
}

// -- generation ----------------------------------------------------------------

/// The schema of `document`, as a JSON value with every object's keys in
/// alphabetical order (so the committed file does not depend on which Cargo
/// features other crates of the build turned on).
///
/// # Errors
///
/// When a rule of [`constrain`] names a place the configuration types no longer
/// have: a key or section was renamed or removed and its rule has to follow.
/// That is a bug in this module, found by its tests.
pub fn schema(document: Document) -> Result<Value> {
    let mut root = generate()?;
    constrain(&mut root)?;
    match document {
        Document::Project => project_file(&mut root)?,
        Document::Local => local_file(&mut root)?,
    }
    describe(&mut root, document)?;
    prune_definitions(&mut root);
    sort_keys(&mut root);
    Ok(root)
}

/// [`schema`] as the text of the committed file: two-space indentation and a
/// final newline.
pub fn render(document: Document) -> Result<String> {
    let mut text = serde_json::to_string_pretty(&schema(document)?)?;
    text.push('\n');
    Ok(text)
}

/// What the derive makes of [`Config`], without what a TOML document never has
/// or an editor does not need: the `null` that `Option<T>` allows, and the
/// Rust that the descriptions came with.
fn generate() -> Result<Value> {
    let generator = SchemaSettings::draft07().into_generator();
    let mut root = serde_json::to_value(generator.into_root_schema_for::<Config>())
        .context("the schema of the configuration is not JSON")?;
    clean(&mut root);
    Ok(root)
}

fn clean(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::Array(types)) = map.get_mut("type") {
                types.retain(|t| t != "null");
                if types.len() == 1 {
                    let only = types.remove(0);
                    map.insert("type".into(), only);
                }
            }
            if let Some(Value::Array(options)) = map.get_mut("anyOf") {
                options.retain(|option| option != &json!({ "type": "null" }));
                if options.len() == 1 {
                    let only = options.remove(0);
                    map.remove("anyOf");
                    if let Value::Object(only) = only {
                        for (key, value) in only {
                            map.entry(key).or_insert(value);
                        }
                    }
                }
            }
            if let Some(Value::String(text)) = map.get_mut("description") {
                *text = plain(text);
            }
            map.values_mut().for_each(clean);
        }
        Value::Array(items) => items.iter_mut().for_each(clean),
        _ => {}
    }
}

/// A doc comment as text for a person who has never seen the Rust: the
/// intra-doc links lose their brackets and the constants they name are
/// replaced by their values.
fn plain(text: &str) -> String {
    let mut text = text.replace("[`", "`").replace("`]", "`");
    let constants: [(&str, String); 12] = [
        ("DEFAULT_BUSY_WAIT", duration(DEFAULT_BUSY_WAIT)),
        (
            "DEFAULT_SQLITE_BUSY_WAIT",
            duration(DEFAULT_SQLITE_BUSY_WAIT),
        ),
        ("DEFAULT_WAL_LOG_BELOW", mebibytes(DEFAULT_WAL_LOG_BELOW)),
        ("DEFAULT_CACHE_ENV", DEFAULT_CACHE_ENV.into()),
        ("DEFAULT_MYSQL_ENV", DEFAULT_MYSQL_ENV.into()),
        ("MYSQL_URL_ENV", MYSQL_URL_ENV.into()),
        ("DEFAULT_SQLITE_ENV", DEFAULT_SQLITE_ENV.into()),
        ("SQLITE_URL_ENV", SQLITE_URL_ENV.into()),
        ("DEFAULT_SQLITE_PATH_ENV", DEFAULT_SQLITE_PATH_ENV.into()),
        ("VALKEY_IMAGE", VALKEY_IMAGE.into()),
        ("REDIS_IMAGE", REDIS_IMAGE.into()),
        ("MYSQL_IMAGE", MYSQL_IMAGE.into()),
    ];
    for (name, value) in constants {
        text = text.replace(&format!("`{name}`"), &format!("`{value}`"));
    }
    let text = text.replace(" (see `CheckoutBackend`)", "").replace(
        " See `valid_extra_database`.",
        " Each name: a lowercase letter, then lowercase letters, digits or `_` (21 characters \
             at most), listed once, and not main, postgres, devflow or template*.",
    );
    unwrap_lines(&text)
}

/// A doc comment is wrapped at 80 columns; an editor wraps by itself. Each
/// paragraph becomes one line, except a list, whose items stay on theirs.
fn unwrap_lines(text: &str) -> String {
    text.split("\n\n")
        .map(|paragraph| {
            if paragraph.lines().any(|line| line.starts_with("- ")) {
                paragraph.to_string()
            } else {
                paragraph.lines().collect::<Vec<_>>().join(" ")
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn duration(value: std::time::Duration) -> String {
    humantime::format_duration(value).to_string()
}

fn mebibytes(bytes: u64) -> String {
    format!("{}MiB", bytes >> 20)
}

// -- the rules a schema can state -----------------------------------------------

/// The object at `pointer` (a JSON pointer into the generated schema).
fn at<'a>(root: &'a mut Value, pointer: &str) -> Result<&'a mut Map<String, Value>> {
    root.pointer_mut(pointer)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| gone(pointer))
}

/// The schema of the `[services.<key>]` table whose `engine` is `engine`.
fn variant<'a>(root: &'a mut Value, engine: &str) -> Result<&'a mut Map<String, Value>> {
    let options = root
        .pointer_mut("/definitions/ServiceConfig/oneOf")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| gone("/definitions/ServiceConfig/oneOf"))?;
    options
        .iter_mut()
        .find(|option| option.pointer("/properties/engine/const") == Some(&json!(engine)))
        .and_then(Value::as_object_mut)
        .ok_or_else(|| gone(&format!("the {engine} service")))
}

/// The schema of `key` among the `properties` of `owner`.
fn property<'a>(
    owner: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Map<String, Value>> {
    owner
        .get_mut("properties")
        .and_then(Value::as_object_mut)
        .and_then(|properties| properties.get_mut(key))
        .and_then(Value::as_object_mut)
        .ok_or_else(|| gone(&format!("the key {key}")))
}

fn gone(place: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "the generated schema has nothing at {place}: a configuration key or section was renamed \
         or removed, and its rule in config_schema has to follow"
    )
}

/// Merge `keywords` (an object) into `target`, replacing what is there.
fn put(target: &mut Map<String, Value>, keywords: Value) {
    if let Value::Object(keywords) = keywords {
        target.extend(keywords);
    }
}

fn constrain(root: &mut Value) -> Result<()> {
    // -- the project and the sections that have no doc comment of their own
    let top = at(root, "")?;
    put(
        property(top, "project")?,
        json!({ "description": "The project's identity: its name and the id that ties trust, the registry and the database server to it across machines." }),
    );
    put(
        property(top, "worktree")?,
        json!({ "description": "Where new worktrees go and what they take with them." }),
    );
    put(
        property(top, "hooks")?,
        json!({ "description": "Shell commands (`sh -c`) run in a workspace, gated by `devflow trust`. Write them so that running them twice is safe." }),
    );
    put(
        property(top, "services")?,
        json!({
            "description": format!(
                "Services by key (`[services.db]`), each selected by its `engine`. The key names \
                 the service in receipts and output: a lowercase letter, then lowercase letters, \
                 digits, `_` or `-`, at most {MAX_KEY_LEN} characters (a container or sqlite \
                 service cannot be named {}). At most one `postgres` and one `mysql` service.",
                SERVICE_SUBCOMMANDS.join(", ")
            ),
            "propertyNames": { "pattern": format!("^[a-z][a-z0-9_-]{{0,{}}}$", MAX_KEY_LEN - 1) }
        }),
    );
    let project = at(root, "/definitions/ProjectSection")?;
    // Draft 7 has no `uuid` format to assert, so the shape is a pattern too: the
    // forms `Uuid` reads (hyphenated, simple, braced, URN), nothing stricter.
    put(
        property(project, "id")?,
        json!({
            "pattern": "^(urn:uuid:)?\\{?[0-9a-fA-F]{8}-?[0-9a-fA-F]{4}-?[0-9a-fA-F]{4}-?[0-9a-fA-F]{4}-?[0-9a-fA-F]{12}\\}?$"
        }),
    );
    put(
        property(project, "name")?,
        json!({
            "description": "The project's name (`{project}` in `[worktree] dir`).",
            "pattern": "\\S"
        }),
    );

    // -- [worktree]
    let worktree = at(root, "/definitions/WorktreeSection")?;
    put(
        property(worktree, "copy")?,
        json!({
            "items": {
                "type": "string",
                "minLength": 1,
                "not": { "pattern": OUTSIDE_PATTERN }
            }
        }),
    );
    put(
        property(worktree, "port_range")?,
        json!({
            "description": "The ports `DEVFLOW_PORT` is chosen from, `[first, last]` inclusive, with 1024 <= first <= last <= 65535 (default `[40000, 49999]`). A port a workspace already has is kept whatever this says now.",
            "items": PORT()
        }),
    );
    put(
        property(worktree, "focus_ports")?,
        json!({ "items": PORT(), "uniqueItems": true }),
    );
    put(
        property(at(root, "/definitions/HooksSection")?, "timeout")?,
        json!({ "minLength": 1 }),
    );

    // -- [services.<key>]
    let postgres = variant(root, "postgres")?;
    put(
        property(postgres, "extra_databases")?,
        json!({
            "items": {
                "type": "string",
                "pattern": "^[a-z][a-z0-9_]{0,20}$",
                "not": { "pattern": "^(main|postgres|devflow|template.*)$" }
            },
            "uniqueItems": true
        }),
    );
    for engine in ["valkey", "redis", "mysql"] {
        let container = variant(root, engine)?;
        put(
            property(container, "env")?,
            json!({ "pattern": NAME_PATTERN }),
        );
        put(
            property(container, "image")?,
            json!({ "pattern": "^[^\\s-]\\S*$" }),
        );
        if engine == "mysql" {
            put(
                property(container, "database")?,
                json!({ "pattern": "^[a-z][a-z0-9_]{0,47}$" }),
            );
        } else {
            // `database` is a key of the section both engines share, and an
            // error for every engine but mysql.
            let properties = container
                .get_mut("properties")
                .and_then(Value::as_object_mut)
                .ok_or_else(|| gone("the properties of a container service"))?;
            properties.remove("database");
        }
    }
    let sqlite = variant(root, "sqlite")?;
    put(
        property(sqlite, "path")?,
        json!({
            "description": "The database file, relative to the worktree (`db/development.sqlite3`). Each workspace has its own, at the same relative path: a workspace's file is cloned from its source's when the workspace is set up. The file need not exist yet in a project that has not created it.",
            "minLength": 1,
            "not": { "pattern": format!("{OUTSIDE_PATTERN}|/$") }
        }),
    );
    put(property(sqlite, "env")?, json!({ "pattern": NAME_PATTERN }));
    put(
        property(sqlite, "path_env")?,
        json!({ "pattern": NAME_PATTERN }),
    );
    // The path has a default only so that a section can be built in code.
    property(sqlite, "path")?.remove("default");
    sqlite.insert("required".into(), json!(["engine", "path"]));
    for engine in ["postgres", "valkey", "redis", "mysql", "sqlite"] {
        variant(root, engine)?.insert("title".into(), engine.into());
    }
    Ok(())
}

// -- the two files ---------------------------------------------------------------

/// `devflow.toml`: complete, and without the machine-local key.
fn project_file(root: &mut Value) -> Result<()> {
    let worktree = at(root, "/definitions/WorktreeSection")?;
    let checkout = property(worktree, "checkout")?;
    checkout.clear();
    put(
        checkout,
        json!({
            "description": "Machine-local: only devflow.local.toml may set it (it chooses who \
                            checks worktrees out, and `git` authorizes Git's own filters and \
                            hooks), so a committed file that sets it is a configuration error.",
            "not": {}
        }),
    );
    Ok(())
}

/// `devflow.local.toml`: an overlay, so nothing is required, and a service is
/// recognised by the keys it has when it does not say which `engine` it is.
fn local_file(root: &mut Value) -> Result<()> {
    strip_required(root);
    // A service table of an overlay may lack `engine`: it matches every engine
    // whose keys it uses, so "exactly one" becomes "at least one".
    let service = at(root, "/definitions/ServiceConfig")?;
    let options = service
        .remove("oneOf")
        .ok_or_else(|| gone("the services' oneOf"))?;
    service.insert("anyOf".into(), options);
    let worktree = at(root, "/definitions/WorktreeSection")?;
    put(
        property(worktree, "checkout")?,
        json!({
            "description": "Who checks a new worktree out. Machine-local: this is the only file that may set it. `libgit2` (the default) makes devflow check the files out and refuses Git LFS and partial clones; `git` runs the `git` on PATH, which handles them but runs Git's own filters and hooks, something `devflow trust` does not cover."
        }),
    );
    Ok(())
}

/// Drop the `required` lists of an overlay, where a section may hold only the
/// keys it overrides. A hook entry is not such a section: the overlay's list
/// replaces the committed one whole, so an entry without `run` is as wrong
/// there as anywhere.
fn strip_required(value: &mut Value) {
    match value {
        Value::Object(map) => {
            // A key named `required` would be a property, not the keyword.
            if map.get("required").is_some_and(Value::is_array) {
                map.remove("required");
            }
            for (key, child) in map.iter_mut() {
                if key != "HookEntry" {
                    strip_required(child);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(strip_required),
        _ => {}
    }
}

fn describe(root: &mut Value, document: Document) -> Result<()> {
    let (title, description) = match document {
        Document::Project => (
            "devflow.toml",
            "devflow 1.0 project configuration (devflow.toml or .devflow.toml), committed at the \
             repository root. Generated from the configuration types by `devflow config schema`; \
             a document can pass it and still be refused (durations, at most one postgres and one \
             mysql service, two services exporting one variable, `${NAME}` references).",
        ),
        Document::Local => (
            "devflow.local.toml",
            "The machine-local overlay beside devflow.toml (git-ignored). It is merged over the \
             project file before anything is parsed, so every key is optional here, and it is the \
             one file that may set `[worktree] checkout`. Generated from the configuration types \
             by `devflow config schema --local`.",
        ),
    };
    let top = at(root, "")?;
    top.insert("title".into(), title.into());
    top.insert("description".into(), description.into());
    Ok(())
}

// -- housekeeping ----------------------------------------------------------------

fn referenced(value: &Value, found: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get("$ref")
                && let Some(name) = reference.strip_prefix("#/definitions/")
            {
                found.insert(name.to_string());
            }
            map.values().for_each(|child| referenced(child, found));
        }
        Value::Array(items) => items.iter().for_each(|child| referenced(child, found)),
        _ => {}
    }
}

/// Drop the definitions nothing reaches from the root any more (the project file
/// loses `CheckoutBackend` with its `checkout` key).
fn prune_definitions(root: &mut Value) {
    let Some(Value::Object(definitions)) = root.get("definitions") else {
        return;
    };
    let mut reachable = BTreeSet::new();
    if let Value::Object(top) = &*root {
        for (key, child) in top {
            if key != "definitions" {
                referenced(child, &mut reachable);
            }
        }
    }
    let mut queue: Vec<String> = reachable.iter().cloned().collect();
    while let Some(name) = queue.pop() {
        let mut inner = BTreeSet::new();
        if let Some(definition) = definitions.get(&name) {
            referenced(definition, &mut inner);
        }
        for next in inner {
            if reachable.insert(next.clone()) {
                queue.push(next);
            }
        }
    }
    if let Some(Value::Object(definitions)) = root.get_mut("definitions") {
        definitions.retain(|name, _| reachable.contains(name));
    }
}

/// Every object's keys in alphabetical order.
fn sort_keys(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = std::mem::take(map).into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for (_, child) in &mut entries {
                sort_keys(child);
            }
            *map = entries.into_iter().collect::<Map<String, Value>>();
        }
        Value::Array(items) => items.iter_mut().for_each(sort_keys),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rule_whose_place_is_gone_is_an_error_not_a_dropped_rule() {
        let mut root = generate().unwrap();
        // The key was renamed in the configuration types: its rule must fail.
        root["definitions"]["WorktreeSection"]["properties"]
            .as_object_mut()
            .unwrap()
            .remove("port_range");
        let error = constrain(&mut root).unwrap_err().to_string();
        assert!(error.contains("port_range"), "{error}");
        assert!(error.contains("config_schema"), "{error}");
        // The same for a service that was removed.
        let mut root = generate().unwrap();
        root["definitions"]["ServiceConfig"]["oneOf"]
            .as_array_mut()
            .unwrap()
            .retain(|option| option["properties"]["engine"]["const"] != "sqlite");
        let error = constrain(&mut root).unwrap_err().to_string();
        assert!(error.contains("sqlite"), "{error}");
    }

    #[test]
    fn the_output_does_not_depend_on_the_order_serde_json_keeps() {
        let text = render(Document::Project).unwrap();
        let again: Value = serde_json::from_str(&text).unwrap();
        let mut sorted = again.clone();
        sort_keys(&mut sorted);
        assert_eq!(again, sorted);
        // Keys are written in alphabetical order, whatever the features.
        let positions: Vec<usize> = ["\"$schema\"", "\"additionalProperties\"", "\"definitions\""]
            .iter()
            .map(|key| text.find(key).unwrap())
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
