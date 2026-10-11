//! Structured, additive edits of TOML files.
//!
//! A proposal never carries file contents. It carries [`PatchOp`]s: create a
//! table, set a key that does not exist yet, add values to an array. They are
//! applied to the current text of a file with `toml_edit`, so formatting and
//! comments of what is already there survive, and they are idempotent: an
//! operation whose effect is already in the file does nothing.
//!
//! An existing key that holds a *different* value is never overwritten. It is
//! reported as [`Effect::Conflict`], which makes the item that proposed it
//! impossible to apply until a person resolves it by hand. A conflict names
//! the key (or the entry's name) and what the proposal wanted; it never
//! repeats what the file holds there, because that may be a credential: an
//! existing hook command with a password inline, say.
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, TableLike, Value as TomlValue};

/// The only files a patch may name: devflow's project file, pitchfork's and
/// mise's. A plan from elsewhere cannot make `init --apply` write anywhere else.
pub const TARGET_FILES: [&str; 5] = [
    "devflow.toml",
    ".devflow.toml",
    "pitchfork.toml",
    "mise.toml",
    ".mise.toml",
];

/// Edits to one file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilePatch {
    /// File name relative to the project root, one of [`TARGET_FILES`].
    pub file: String,
    /// Applied in order. Empty for an item that only reports something to
    /// do by hand.
    pub ops: Vec<PatchOp>,
}

impl FilePatch {
    /// A patch with no operations: nothing devflow can write.
    pub fn none(file: &str) -> Self {
        Self {
            file: file.to_string(),
            ops: Vec::new(),
        }
    }
}

/// One additive operation. `path` components are table and key names
/// (`["services", "db", "image"]`); a name is never split on dots.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum PatchOp {
    /// Make sure the table at `path` exists. `dotted` writes a table that is
    /// created here as dotted keys of its parent (`_.file = …`).
    Table {
        path: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        comment: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        dotted: bool,
    },
    /// Set the key at `path` to `value` when it is absent. Strings, booleans,
    /// integers, arrays and objects (inline tables) are supported.
    Set {
        path: Vec<String>,
        value: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        comment: Option<String>,
    },
    /// Add each of `values` to the array at `path` (created when absent)
    /// unless it is already there. A hook object (`{name, run}`) counts as
    /// present when an entry has the same `run`, as a conflict when an entry
    /// has the same `name` and a different `run`. `front` inserts before the
    /// existing entries (keeping the order of `values`), for steps that must
    /// run first.
    Append {
        path: Vec<String>,
        values: Vec<Value>,
        #[serde(default, skip_serializing_if = "is_false")]
        front: bool,
    },
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl PatchOp {
    /// The path the operation works on.
    pub fn path(&self) -> &[String] {
        match self {
            PatchOp::Table { path, .. }
            | PatchOp::Set { path, .. }
            | PatchOp::Append { path, .. } => path,
        }
    }

    /// The path the operation works on, dotted, for messages.
    pub fn target(&self) -> String {
        self.path().join(".")
    }

    /// Whether this operation is one a proposal for `file` may make. A plan
    /// is a document somebody could have edited: it may only touch the
    /// sections `init` writes, and never carry anything that looks like a
    /// credential.
    ///
    /// The check covers everything the operation would write, not just its
    /// `path`: table and inline-table values are expanded into full key
    /// paths (`Set ["env"]` of `{"_": {"source": …}}` writes `env._.source`),
    /// and each one must be allowed for `file` and must not name a secret.
    pub fn check_allowed(&self, file: &str) -> std::result::Result<(), String> {
        for (path, value) in self.written_paths()? {
            if !path_allowed(file, &path, value) {
                return Err(format!(
                    "{} is not something devflow init writes to {file}",
                    path.join(".")
                ));
            }
            if path.iter().any(|part| crate::redact::is_secret_key(part)) {
                return Err(format!("{} names a secret", path.join(".")));
            }
        }
        let mut strings = Vec::new();
        match self {
            PatchOp::Table { comment, .. } => strings.extend(comment.iter().map(String::as_str)),
            PatchOp::Set { value, comment, .. } => {
                collect_strings(value, &mut strings);
                strings.extend(comment.iter().map(String::as_str));
            }
            PatchOp::Append { values, .. } => {
                for value in values {
                    collect_strings(value, &mut strings);
                }
            }
        }
        if strings
            .iter()
            .any(|text| crate::redact::text(text) != *text)
        {
            return Err(format!(
                "{} carries what looks like a credential; plans never contain secrets",
                self.target()
            ));
        }
        Ok(())
    }

    /// Every key path the operation writes, with the value stored there when
    /// there is one: `path` itself, then each key of the table or inline
    /// table values below it, recursively. The elements of an array share its
    /// path (the keys of a hook table belong to the array they are added to).
    /// Values TOML cannot hold (null, floats) and values nested more than
    /// [`MAX_DEPTH`] levels are errors.
    fn written_paths(&self) -> std::result::Result<Vec<Written<'_>>, String> {
        let mut written = Vec::new();
        match self {
            PatchOp::Table { path, .. } => written.push((path.clone(), None)),
            PatchOp::Set { path, value, .. } => {
                written.push((path.clone(), Some(value)));
                expand(path, value, 0, &mut written)?;
            }
            PatchOp::Append { path, values, .. } => {
                written.push((path.clone(), None));
                for value in values {
                    if !value.is_object() {
                        written.push((path.clone(), Some(value)));
                    }
                    expand(path, value, 0, &mut written)?;
                }
            }
        }
        Ok(written)
    }
}

/// A key path an operation writes, and the value stored there, when it has one.
type Written<'a> = (Vec<String>, Option<&'a Value>);

/// How deep a patch value may nest. The files `init` writes are flat.
const MAX_DEPTH: usize = 6;

fn expand<'a>(
    prefix: &[String],
    value: &'a Value,
    depth: usize,
    out: &mut Vec<Written<'a>>,
) -> std::result::Result<(), String> {
    if depth > MAX_DEPTH {
        return Err(format!(
            "{} nests values more than {MAX_DEPTH} levels deep",
            prefix.join(".")
        ));
    }
    match value {
        Value::Object(map) => {
            for (key, entry) in map {
                let mut path = prefix.to_vec();
                path.push(key.clone());
                out.push((path.clone(), Some(entry)));
                expand(&path, entry, depth + 1, out)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                expand(prefix, item, depth + 1, out)?;
            }
        }
        Value::Null => {
            return Err(format!(
                "{} is null, which cannot be written to TOML",
                prefix.join(".")
            ));
        }
        Value::Number(number) if number.as_i64().is_none() => {
            return Err(format!(
                "{} is {number}; only integers can be written to TOML",
                prefix.join(".")
            ));
        }
        Value::Number(_) | Value::Bool(_) | Value::String(_) => {}
    }
    Ok(())
}

/// Whether a plan for `file` may write the key at `path` (holding `value`,
/// when it holds one).
fn path_allowed(file: &str, path: &[String], value: Option<&Value>) -> bool {
    let first = path.first().map(String::as_str).unwrap_or("");
    match file {
        "devflow.toml" | ".devflow.toml" => {
            ["project", "worktree", "services", "hooks", "env"].contains(&first)
                // Only the switch that starts approved daemons.
                || (first == "pitchfork"
                    && match path.len() {
                        1 => true,
                        2 => path[1] == "autostart" && value.is_none_or(Value::is_boolean),
                        _ => false,
                    })
        }
        "pitchfork.toml" => first == "daemons",
        // mise: only the environment file devflow generates, and only that
        // file: anything else loaded there is code or variables of somebody
        // else's choosing.
        _ => {
            first == "env"
                && match path.len() {
                    1 => true,
                    2 => path[1] == "_",
                    3 => {
                        path[1] == "_"
                            && path[2] == "file"
                            && value.is_none_or(|v| v.as_str() == Some(".devflow/env"))
                    }
                    _ => false,
                }
        }
    }
}

fn collect_strings<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
    match value {
        Value::String(text) => out.push(text),
        Value::Array(items) => items.iter().for_each(|item| collect_strings(item, out)),
        Value::Object(map) => {
            for (key, item) in map {
                out.push(key);
                collect_strings(item, out);
            }
        }
        _ => {}
    }
}

/// What an operation does to the text it is applied to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// The text changes.
    Apply,
    /// The text already has this; nothing changes.
    Satisfied,
    /// Something incompatible is there; the operation cannot be applied.
    Conflict(String),
}

/// Parse `text` (`None` for a file that does not exist yet) for editing.
///
/// A syntax error is reported by position and reason only: the parser's own
/// display quotes the offending line, which may hold a credential.
pub fn parse(text: Option<&str>) -> Result<DocumentMut> {
    match text {
        // The parser's own rendering quotes the offending line, which may hold
        // a credential: say where, and its description, not what the file says.
        Some(text) => text.parse::<DocumentMut>().map_err(|error| {
            anyhow::anyhow!(
                "not valid TOML{}: {}",
                crate::diag::position(text, error.span()),
                crate::redact::text(error.message())
            )
        }),
        None => Ok(DocumentMut::new()),
    }
}

/// Apply `ops` to `text` and return the new text and what each operation did.
/// Operations with a conflict leave the text as it was for that operation.
pub fn render(text: Option<&str>, ops: &[PatchOp]) -> Result<(String, Vec<Effect>)> {
    let mut doc = parse(text)?;
    let mut effects = Vec::with_capacity(ops.len());
    for op in ops {
        effects.push(apply(&mut doc, op)?);
    }
    let rendered = doc.to_string();
    let rendered = if text.is_none() || text.is_some_and(|t| t.trim().is_empty()) {
        // A new document starts with its first table, not a blank line.
        let trimmed = rendered.trim_start_matches('\n').to_string();
        if trimmed.is_empty() || trimmed.ends_with('\n') {
            trimmed
        } else {
            format!("{trimmed}\n")
        }
    } else if rendered.ends_with('\n') {
        rendered
    } else {
        format!("{rendered}\n")
    };
    Ok((rendered, effects))
}

/// Apply `op` to `doc`. An error means the operation itself is malformed
/// (empty path, a value TOML cannot hold), not that the file is in the way.
pub fn apply(doc: &mut DocumentMut, op: &PatchOp) -> Result<Effect> {
    match op {
        PatchOp::Table {
            path,
            comment,
            dotted,
        } => {
            non_empty(path)?;
            let mut created = false;
            let table = match descend(doc.as_table_mut(), path, true, *dotted, &mut created) {
                Ok(table) => table,
                Err(conflict) => return Ok(Effect::Conflict(conflict)),
            };
            let _ = table;
            if !created {
                return Ok(Effect::Satisfied);
            }
            if let Some(comment) = comment
                && let Some(table) = table_at(doc.as_table_mut(), path)
            {
                table.decor_mut().set_prefix(comment_block(comment, true));
            }
            Ok(Effect::Apply)
        }
        PatchOp::Set {
            path,
            value,
            comment,
        } => {
            non_empty(path)?;
            let (parents, key) = path.split_at(path.len() - 1);
            let key = &key[0];
            let wanted = to_toml(value)?;
            let mut created = false;
            let table = match descend(doc.as_table_mut(), parents, true, false, &mut created) {
                Ok(table) => table,
                Err(conflict) => return Ok(Effect::Conflict(conflict)),
            };
            if let Some(existing) = table.get(key) {
                return Ok(match json_of(existing) {
                    Some(current) if &current == value => Effect::Satisfied,
                    _ => Effect::Conflict(format!(
                        "`{}` is already set to something else (proposed {})",
                        path.join("."),
                        wanted
                    )),
                });
            }
            table.insert(key, Item::Value(wanted));
            if let Some(comment) = comment
                && let Some(mut key) = table.key_mut(key)
            {
                key.leaf_decor_mut()
                    .set_prefix(comment_block(comment, false));
            }
            Ok(Effect::Apply)
        }
        PatchOp::Append {
            path,
            values,
            front,
        } => {
            non_empty(path)?;
            let (parents, key) = path.split_at(path.len() - 1);
            let key = &key[0];
            let mut created = false;
            let table = match descend(doc.as_table_mut(), parents, true, false, &mut created) {
                Ok(table) => table,
                Err(conflict) => return Ok(Effect::Conflict(conflict)),
            };
            if !table.contains_key(key) {
                table.insert(key, Item::Value(TomlValue::Array(Array::new())));
            }
            let Some(array) = table.get_mut(key).and_then(Item::as_array_mut) else {
                return Ok(Effect::Conflict(format!(
                    "`{}` is not an array",
                    path.join(".")
                )));
            };
            let mut additions = Vec::new();
            for value in values {
                match presence(array, value) {
                    Presence::Present => {}
                    Presence::Missing => additions.push(value),
                    Presence::NameTaken => {
                        return Ok(Effect::Conflict(format!(
                            "`{}` already has an entry named \"{}\" with a different command",
                            path.join("."),
                            value.get("name").and_then(Value::as_str).unwrap_or("")
                        )));
                    }
                }
            }
            if additions.is_empty() {
                return Ok(Effect::Satisfied);
            }
            insert_values(array, &additions, *front)?;
            Ok(Effect::Apply)
        }
    }
}

fn non_empty(path: &[String]) -> Result<()> {
    if path.is_empty() || path.iter().any(String::is_empty) {
        bail!("a patch path needs at least one non-empty name");
    }
    Ok(())
}

/// Walk `path` from `root`, creating tables that are missing. The last table
/// created is explicit (it gets a `[header]`) when `explicit_leaf`; the ones
/// above it are implicit. `Err` carries the reason a component is not a table.
fn descend<'a>(
    root: &'a mut Table,
    path: &[String],
    explicit_leaf: bool,
    dotted_leaf: bool,
    created: &mut bool,
) -> Result<&'a mut dyn TableLike, String> {
    let mut current: &'a mut dyn TableLike = root;
    let last = path.len().saturating_sub(1);
    for (index, key) in path.iter().enumerate() {
        if !current.contains_key(key) {
            let mut table = Table::new();
            let is_last = index == last;
            table.set_implicit(!(is_last && explicit_leaf));
            if is_last && dotted_leaf {
                table.set_dotted(true);
            }
            current.insert(key, Item::Table(table));
            *created = true;
        }
        let Some(next) = current.get_mut(key).and_then(Item::as_table_like_mut) else {
            return Err(format!("`{}` is not a table", path[..=index].join(".")));
        };
        current = next;
    }
    Ok(current)
}

/// The table at `path`, when it is a regular table with a header.
fn table_at<'a>(root: &'a mut Table, path: &[String]) -> Option<&'a mut Table> {
    let mut current = root;
    for key in path {
        current = current.get_mut(key)?.as_table_mut()?;
    }
    Some(current)
}

fn comment_block(comment: &str, blank_line_before: bool) -> String {
    let mut text = String::new();
    if blank_line_before {
        text.push('\n');
    }
    for paragraph in comment.lines() {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            if !line.is_empty() && line.len() + 1 + word.len() > COMMENT_WIDTH {
                text.push_str(&format!("# {line}\n"));
                line.clear();
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        text.push_str(&format!("# {line}\n"));
    }
    text
}

/// Comment text is wrapped to this many characters after the `# `.
const COMMENT_WIDTH: usize = 76;

// -- values ---------------------------------------------------------------------

/// A JSON value as TOML. Floats and null have no use in these files.
fn to_toml(value: &Value) -> Result<TomlValue> {
    Ok(match value {
        Value::String(text) => TomlValue::from(text.as_str()),
        Value::Bool(flag) => TomlValue::from(*flag),
        Value::Number(number) => match number.as_i64() {
            Some(n) => TomlValue::from(n),
            None => bail!("only integers can be written to TOML, not {number}"),
        },
        Value::Array(items) => {
            let mut array = Array::new();
            for item in items {
                array.push(to_toml(item)?);
            }
            TomlValue::Array(array)
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by_key(|key| (key_rank(key), key.as_str()));
            let mut table = InlineTable::new();
            for key in keys {
                table.insert(key, to_toml(&map[key])?);
            }
            table.fmt();
            TomlValue::InlineTable(table)
        }
        Value::Null => bail!("null cannot be written to TOML"),
    })
}

/// Field order inside inline tables: how a person would write them.
fn key_rank(key: &str) -> u8 {
    match key {
        "name" => 0,
        "run" => 1,
        "timeout" => 2,
        "expect" => 3,
        "bump" => 4,
        _ => 10,
    }
}

/// A TOML item as the JSON value with the same meaning, when it has one.
fn json_of(item: &Item) -> Option<Value> {
    match item {
        Item::Value(value) => json_of_value(value),
        Item::Table(table) => {
            let mut map = serde_json::Map::new();
            for (key, item) in table.iter() {
                map.insert(key.to_string(), json_of(item)?);
            }
            Some(Value::Object(map))
        }
        _ => None,
    }
}

fn json_of_value(value: &TomlValue) -> Option<Value> {
    match value {
        TomlValue::String(text) => Some(Value::String(text.value().clone())),
        TomlValue::Integer(number) => Some(Value::from(*number.value())),
        TomlValue::Boolean(flag) => Some(Value::Bool(*flag.value())),
        TomlValue::Array(array) => array
            .iter()
            .map(json_of_value)
            .collect::<Option<Vec<_>>>()
            .map(Value::Array),
        TomlValue::InlineTable(table) => {
            let mut map = serde_json::Map::new();
            for (key, value) in table.iter() {
                map.insert(key.to_string(), json_of_value(value)?);
            }
            Some(Value::Object(map))
        }
        TomlValue::Float(_) | TomlValue::Datetime(_) => None,
    }
}

// -- arrays -----------------------------------------------------------------------

enum Presence {
    Present,
    Missing,
    /// An entry has the wanted name but runs something else.
    NameTaken,
}

/// A hook entry's `run` and `name`, whether written as a string or a table.
fn hook_fields(value: &TomlValue) -> Option<(String, Option<String>)> {
    match value {
        TomlValue::String(text) => Some((text.value().clone(), None)),
        TomlValue::InlineTable(table) => {
            let run = table.get("run")?.as_str()?.to_string();
            let name = table
                .get("name")
                .and_then(TomlValue::as_str)
                .map(str::to_string);
            Some((run, name))
        }
        _ => None,
    }
}

fn presence(array: &Array, wanted: &Value) -> Presence {
    if let Value::Object(map) = wanted
        && let Some(Value::String(run)) = map.get("run")
    {
        let name = map.get("name").and_then(Value::as_str);
        for entry in array.iter() {
            let Some((entry_run, entry_name)) = hook_fields(entry) else {
                continue;
            };
            if &entry_run == run {
                return Presence::Present;
            }
            if name.is_some() && entry_name.as_deref() == name {
                return Presence::NameTaken;
            }
        }
        return Presence::Missing;
    }
    let present = array
        .iter()
        .any(|entry| json_of_value(entry).as_ref() == Some(wanted));
    if present {
        Presence::Present
    } else {
        Presence::Missing
    }
}

fn has_comment(value: &TomlValue) -> bool {
    let decor = value.decor();
    [decor.prefix(), decor.suffix()]
        .into_iter()
        .flatten()
        .any(|raw| raw.as_str().is_none_or(|text| text.contains('#')))
}

fn is_multiline(array: &Array) -> bool {
    array.iter().any(|value| {
        value
            .decor()
            .prefix()
            .and_then(|raw| raw.as_str())
            .is_some_and(|text| text.contains('\n'))
    })
}

fn insert_values(array: &mut Array, additions: &[&Value], front: bool) -> Result<()> {
    let had_entries = !array.is_empty();
    let already_multiline = had_entries && is_multiline(array);
    let tables = additions.iter().any(|value| value.is_object());
    let multiline = already_multiline || tables;
    let existing = array.len();

    for (offset, value) in additions.iter().enumerate() {
        let toml = to_toml(value)?;
        if front {
            array.insert(offset, toml);
        } else {
            array.push(toml);
        }
    }
    if !multiline {
        return Ok(());
    }

    let reflow_existing = had_entries && !already_multiline && !array.iter().any(has_comment);
    let new_positions: Vec<usize> = if front {
        (0..additions.len()).collect()
    } else {
        (existing..existing + additions.len()).collect()
    };
    for (index, entry) in array.iter_mut().enumerate() {
        if new_positions.contains(&index) || reflow_existing {
            let decor = entry.decor_mut();
            decor.set_prefix("\n  ");
            decor.set_suffix("");
        }
    }
    array.set_trailing_comma(true);
    if !already_multiline {
        array.set_trailing("\n");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_file_that_is_not_toml_is_described_without_quoting_it() {
        for text in [
            "admin_url = \"postgres://u@h/REVIEWSECRET\nx = 1\n",
            "[env]\nTOKEN = REVIEWSECRET\n",
            "REVIEWSECRET\n",
        ] {
            let error = parse(Some(text)).unwrap_err();
            let shown = format!("{error:#}");
            assert!(shown.starts_with("not valid TOML at line"), "{shown}");
            assert!(!shown.contains("REVIEWSECRET"), "{shown}");
        }
    }

    fn table(path: &[&str], comment: Option<&str>) -> PatchOp {
        PatchOp::Table {
            path: path.iter().map(|s| s.to_string()).collect(),
            comment: comment.map(str::to_string),
            dotted: false,
        }
    }

    fn set(path: &[&str], value: Value) -> PatchOp {
        PatchOp::Set {
            path: path.iter().map(|s| s.to_string()).collect(),
            value,
            comment: None,
        }
    }

    fn append(path: &[&str], values: Vec<Value>, front: bool) -> PatchOp {
        PatchOp::Append {
            path: path.iter().map(|s| s.to_string()).collect(),
            values,
            front,
        }
    }

    #[test]
    fn a_new_document_gets_tables_in_the_order_of_the_operations() {
        let ops = vec![
            table(&["project"], Some("Identity.\nKeep it committed.")),
            set(&["project", "name"], json!("shop")),
            table(&["services", "db"], None),
            set(&["services", "db", "engine"], json!("postgres")),
            append(
                &["hooks", "setup"],
                vec![
                    json!({"name": "install", "run": "bun install"}),
                    json!({"name": "migrate", "run": "bunx prisma migrate deploy"}),
                ],
                false,
            ),
        ];
        let (text, effects) = render(None, &ops).unwrap();
        assert!(effects.iter().all(|e| *e == Effect::Apply), "{effects:?}");
        assert_eq!(
            text,
            "# Identity.\n# Keep it committed.\n[project]\nname = \"shop\"\n\n[services.db]\nengine = \"postgres\"\n\n[hooks]\nsetup = [\n  { name = \"install\", run = \"bun install\" },\n  { name = \"migrate\", run = \"bunx prisma migrate deploy\" },\n]\n"
        );
        // And it is valid TOML.
        text.parse::<toml::Table>().unwrap();
    }

    #[test]
    fn existing_content_and_comments_survive_additions() {
        let existing = "# my project\n[project]\nname = \"shop\" # keep\n\n[hooks]\nsetup = [\"bun install\"] # deps\nteardown = []\n";
        let ops = vec![
            append(
                &["hooks", "setup"],
                vec![json!({"name": "migrate", "run": "bunx prisma migrate deploy"})],
                false,
            ),
            append(
                &["hooks", "teardown"],
                vec![json!({"name": "down", "run": "docker compose down"})],
                false,
            ),
        ];
        let (text, effects) = render(Some(existing), &ops).unwrap();
        assert_eq!(effects, vec![Effect::Apply, Effect::Apply]);
        assert!(text.starts_with("# my project\n[project]\nname = \"shop\" # keep\n"));
        assert!(text.contains("# deps"), "{text}");
        assert!(text.contains("\"bun install\""));
        assert!(text.contains("{ name = \"migrate\", run = \"bunx prisma migrate deploy\" }"));
        assert!(text.contains("{ name = \"down\", run = \"docker compose down\" }"));
        text.parse::<toml::Table>().unwrap();
    }

    #[test]
    fn set_never_overwrites_and_reports_the_difference() {
        let existing = "[services.db]\nengine = \"postgres\"\nimage = \"postgres:17\"\n";
        let same = set(&["services", "db", "engine"], json!("postgres"));
        let other = set(&["services", "db", "image"], json!("postgres:18"));
        let absent = set(&["services", "db", "mode"], json!("managed"));
        let (text, effects) = render(Some(existing), &[same, other, absent]).unwrap();
        assert_eq!(effects[0], Effect::Satisfied);
        match &effects[1] {
            Effect::Conflict(why) => {
                assert!(why.contains("services.db.image"), "{why}");
                assert!(why.contains("postgres:18"), "what was proposed: {why}");
                assert!(!why.contains("postgres:17"), "what the file holds: {why}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(effects[2], Effect::Apply);
        assert!(text.contains("image = \"postgres:17\""));
        assert!(text.contains("mode = \"managed\""));
    }

    #[test]
    fn arrays_dedupe_by_value_run_and_name() {
        let existing =
            "[hooks]\nsetup = [\"bun install\", { name = \"migrate\", run = \"make migrate\" }]\n";
        let ops = vec![
            // Same command, different shape: already there.
            append(
                &["hooks", "setup"],
                vec![json!({"name": "install", "run": "bun install"})],
                false,
            ),
            // Same name, different command: a conflict, not a second entry.
            append(
                &["hooks", "setup"],
                vec![json!({"name": "migrate", "run": "bunx prisma migrate deploy"})],
                false,
            ),
            append(&["worktree", "copy"], vec![json!(".env")], false),
        ];
        let (text, effects) = render(Some(existing), &ops).unwrap();
        assert_eq!(effects[0], Effect::Satisfied);
        assert!(matches!(&effects[1], Effect::Conflict(why) if why.contains("migrate")));
        assert_eq!(effects[2], Effect::Apply);
        assert!(text.contains("[worktree]\ncopy = [\".env\"]"), "{text}");
        // Running the same operations on the result changes nothing more.
        let (again, effects) = render(Some(&text), &ops).unwrap();
        assert_eq!(again, text);
        assert_eq!(effects[0], Effect::Satisfied);
        assert_eq!(effects[2], Effect::Satisfied);
    }

    #[test]
    fn front_insertion_keeps_the_order_of_the_new_entries_ahead_of_old_ones() {
        let existing = "[hooks]\nsetup = [\"bun install\"]\n";
        let op = append(
            &["hooks", "setup"],
            vec![
                json!({"name": "mise-trust", "run": "mise trust"}),
                json!({"name": "mise-install", "run": "mise install"}),
            ],
            true,
        );
        let (text, _) = render(Some(existing), &[op]).unwrap();
        let trust = text.find("mise trust").unwrap();
        let install = text.find("mise install").unwrap();
        let bun = text.find("\"bun install\"").unwrap();
        assert!(trust < install && install < bun, "{text}");
        text.parse::<toml::Table>().unwrap();
    }

    #[test]
    fn mise_env_file_is_written_as_a_dotted_key() {
        let ops = vec![
            table(&["env"], Some("devflow writes .devflow/env.")),
            PatchOp::Table {
                path: vec!["env".into(), "_".into()],
                comment: None,
                dotted: true,
            },
            set(&["env", "_", "file"], json!(".devflow/env")),
        ];
        let (text, _) = render(None, &ops).unwrap();
        assert_eq!(
            text,
            "# devflow writes .devflow/env.\n[env]\n_.file = \".devflow/env\"\n"
        );
        // Added to a mise file that has its own tools and an `[env]` already.
        let existing = "[tools]\nnode = \"22\"\n\n[env]\nNODE_ENV = \"development\"\n";
        let (text, effects) = render(Some(existing), &ops).unwrap();
        assert_eq!(effects[0], Effect::Satisfied);
        assert!(text.contains("_.file = \".devflow/env\""), "{text}");
        assert!(text.contains("NODE_ENV"));
        let parsed: toml::Table = text.parse().unwrap();
        assert_eq!(
            parsed["env"]["_"]["file"].as_str(),
            Some(".devflow/env"),
            "{text}"
        );
    }

    #[test]
    fn inline_tables_are_written_in_the_order_a_person_would() {
        let op = set(
            &["daemons", "web", "port"],
            json!({"bump": true, "expect": [3000]}),
        );
        let (text, _) = render(None, &[op]).unwrap();
        assert_eq!(
            text,
            "[daemons.web]\nport = { expect = [3000], bump = true }\n"
        );
    }

    #[test]
    fn empty_and_commented_arrays_are_filled_in_place() {
        let existing = "[hooks]\nsetup = []\nteardown = []\ntimeout = \"5m\"\n";
        let ops = vec![append(
            &["hooks", "setup"],
            vec![
                json!({"name": "install", "run": "bun install"}),
                json!({"name": "migrate", "run": "bun run db:migrate"}),
            ],
            false,
        )];
        let (text, _) = render(Some(existing), &ops).unwrap();
        assert_eq!(
            text,
            "[hooks]\nsetup = [\n  { name = \"install\", run = \"bun install\" },\n  { name = \"migrate\", run = \"bun run db:migrate\" },\n]\nteardown = []\ntimeout = \"5m\"\n"
        );

        // A multi-line array with a comment keeps its comment and layout.
        let existing = "[hooks]\nsetup = [\n  \"mise trust\", # first\n  \"mise install\",\n]\n";
        let (text, _) = render(
            Some(existing),
            &[append(
                &["hooks", "setup"],
                vec![json!({"name": "install", "run": "bun install"})],
                false,
            )],
        )
        .unwrap();
        assert_eq!(
            text,
            "[hooks]\nsetup = [\n  \"mise trust\", # first\n  \"mise install\",\n  { name = \"install\", run = \"bun install\" },\n]\n"
        );
        text.parse::<toml::Table>().unwrap();

        // A string array stays on one line.
        let (text, _) = render(
            Some("[worktree]\ncopy = [\".env\"]\n"),
            &[append(
                &["worktree", "copy"],
                vec![json!("node_modules")],
                false,
            )],
        )
        .unwrap();
        assert_eq!(text, "[worktree]\ncopy = [\".env\", \"node_modules\"]\n");
    }

    #[test]
    fn nested_values_are_checked_by_their_full_key_paths() {
        // What init writes passes, however it is spelled.
        let ok = [
            set(&["env", "_", "file"], json!(".devflow/env")),
            set(&["env", "_"], json!({"file": ".devflow/env"})),
            set(&["env"], json!({"_": {"file": ".devflow/env"}})),
            set(
                &["daemons", "web", "port"],
                json!({"expect": [3000], "bump": true}),
            ),
            append(
                &["hooks", "setup"],
                vec![json!({"name": "install", "run": "bun install", "timeout": "5m"})],
                false,
            ),
        ];
        for op in &ok {
            let file = match op.path()[0].as_str() {
                "env" => "mise.toml",
                "daemons" => "pitchfork.toml",
                _ => "devflow.toml",
            };
            op.check_allowed(file)
                .unwrap_or_else(|why| panic!("{op:?}: {why}"));
        }

        let refused = [
            (
                "mise.toml",
                set(&["env"], json!({"_": {"source": "./bootstrap.sh"}})),
                "env._.source is not something",
            ),
            (
                "mise.toml",
                set(&["env", "_", "file"], json!("../elsewhere/.env")),
                "env._.file is not something",
            ),
            (
                "pitchfork.toml",
                set(&["daemons", "web", "env"], json!({"API_TOKEN": "abc"})),
                "daemons.web.env.API_TOKEN names a secret",
            ),
            (
                "pitchfork.toml",
                set(
                    &["daemons", "web"],
                    json!({"deep": {"deeper": {"x": {"y": {"z": {"w": {"v": 1}}}}}}}),
                ),
                "nests values",
            ),
            (
                "devflow.toml",
                append(&["worktree", "copy"], vec![json!([null])], false),
                "null",
            ),
        ];
        for (file, op, why) in refused {
            let error = op.check_allowed(file).unwrap_err();
            assert!(error.contains(why), "{op:?}: {error}");
        }
    }

    #[test]
    fn a_key_that_is_not_a_table_or_array_is_a_conflict() {
        let existing = "hooks = \"nope\"\nworktree = { copy = \"x\" }\n";
        let (_, effects) = render(
            Some(existing),
            &[
                append(&["hooks", "setup"], vec![json!("x")], false),
                append(&["worktree", "copy"], vec![json!(".env")], false),
            ],
        )
        .unwrap();
        assert!(matches!(&effects[0], Effect::Conflict(why) if why.contains("hooks")));
        assert!(matches!(&effects[1], Effect::Conflict(why) if why.contains("not an array")));
    }
}
