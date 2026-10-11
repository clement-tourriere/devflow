//! Telling a person what is wrong with a file without quoting the file.
//!
//! Configuration, receipts and plans hold credentials: an `admin_url`, a token
//! in `[env]`, an argument of a hook command. An error travels: it is printed,
//! logged, put into `--json` documents and shown by the desktop app, so the
//! text of a value must never be in one, nor any piece of it. An error about
//! such a file names the file, the key path and the rule that was broken.
//!
//! Two rules keep that true:
//!
//! * Errors raised by devflow's own code are built from `&'static str` texts,
//!   indexes and key names that pass [`key_label`]. They never format a value.
//! * A deserializer's message is not shown, and is not searched for a "safe"
//!   part with delimiters (a value can contain any delimiter). It is
//!   *classified* by the fixed words serde starts it with, and the error is
//!   rebuilt from the key path the deserializer was at plus a fixed
//!   description of the class. The only text taken from a message is the tail
//!   that follows the value, after the value itself was recognised and cut off
//!   from the front *by comparing it with the input* (see [`describe`]).
use anyhow::Result;
use serde::de::DeserializeOwned;
use std::borrow::Cow;
use std::fmt::Write as _;
use std::ops::Range;
use std::path::Path;

/// One step of a key path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Seg {
    Key(String),
    Index(usize),
}

impl Seg {
    pub(crate) fn key(key: impl Into<String>) -> Seg {
        Seg::Key(key.into())
    }
}

/// The longest key [`key_label`] shows.
const MAX_KEY_LABEL: usize = 64;

/// How a table key is named in an error: as written when it is a plain bare
/// key (letters, digits, `_` and `-`), otherwise a placeholder. Keys are part
/// of the structure and make an error useful, but a quoted key can hold
/// anything.
pub(crate) fn key_label(key: &str) -> Cow<'_, str> {
    let plain = !key.is_empty()
        && key.len() <= MAX_KEY_LABEL
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if plain {
        Cow::Borrowed(key)
    } else {
        Cow::Borrowed("<key not shown>")
    }
}

/// `services.db.extra_databases[1]`, keys through [`key_label`].
pub(crate) fn render_path(path: &[Seg]) -> String {
    let mut out = String::new();
    for seg in path {
        match seg {
            Seg::Key(key) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(&key_label(key));
            }
            Seg::Index(index) => {
                let _ = write!(out, "[{index}]");
            }
        }
    }
    out
}

fn located(path: &[Seg], text: &str) -> String {
    if path.is_empty() {
        text.to_string()
    } else {
        format!("{}: {text}", render_path(path))
    }
}

// ---- errors raised by our own deserializers --------------------------------

/// Marks a message built by [`own`]. A deserializer never starts a message
/// with the user's text (serde's own start with `invalid type: …`,
/// `unknown field `, and so on), so the mark cannot be forged by a value.
const OWN: &str = "\u{1f}devflow\u{1f}";
const SEPARATOR: char = '\u{1f}';

/// An error from a hand-written `Deserialize`, to be shown as
/// `<path>.<key>: <text>`. Only a `&'static str` can be the text, so nothing
/// from the input gets in by accident; `key` is shown through [`key_label`].
pub(crate) fn own<E: serde::de::Error>(key: Option<&str>, text: &'static str) -> E {
    let key = key.map(key_label).unwrap_or_default();
    E::custom(format!("{OWN}{key}{SEPARATOR}{text}"))
}

fn split_own(message: &str) -> Option<(&str, &str)> {
    message.strip_prefix(OWN)?.split_once(SEPARATOR)
}

// ---- what the input holds where a deserializer failed ----------------------

/// What the input holds at the failing path: enough to recognise, in the
/// message, the part that is the input's.
#[derive(Debug, Default, Clone)]
pub(crate) struct Found<'a> {
    /// How serde renders the value in a message (`string "x"`, ``integer `3` ``).
    shown: Option<String>,
    /// Its kind, for people.
    kind: Option<&'static str>,
    /// The value, when it is a string.
    text: Option<&'a str>,
}

impl<'a> Found<'a> {
    fn of_toml(value: &'a toml::Value) -> Found<'a> {
        let (shown, kind, text) = match value {
            toml::Value::String(s) => (format!("string {s:?}"), "a string", Some(s.as_str())),
            toml::Value::Integer(n) => (format!("integer `{n}`"), "an integer", None),
            toml::Value::Float(f) => (format!("floating point `{f}`"), "a float", None),
            toml::Value::Boolean(b) => (format!("boolean `{b}`"), "a boolean", None),
            toml::Value::Datetime(d) => (format!("string {:?}", d.to_string()), "a date", None),
            toml::Value::Array(_) => ("sequence".to_string(), "an array", None),
            toml::Value::Table(_) => ("map".to_string(), "a table", None),
        };
        Found {
            shown: Some(shown),
            kind: Some(kind),
            text,
        }
    }
}

fn toml_at<'a>(root: &'a toml::Value, path: &[Seg]) -> Option<&'a toml::Value> {
    let mut at = root;
    for seg in path {
        at = match (seg, at) {
            (Seg::Key(key), toml::Value::Table(table)) => table.get(key)?,
            (Seg::Index(index), toml::Value::Array(items)) => items.get(*index)?,
            _ => return None,
        };
    }
    Some(at)
}

fn segments(path: &serde_path_to_error::Path) -> Vec<Seg> {
    use serde_path_to_error::Segment;
    path.iter()
        .filter_map(|segment| match segment {
            Segment::Seq { index } => Some(Seg::Index(*index)),
            Segment::Map { key } => Some(Seg::Key(key.clone())),
            Segment::Enum { variant } => Some(Seg::Key(variant.clone())),
            _ => None,
        })
        .collect()
}

/// Whether `name` has the shape of a field name written in Rust source.
fn is_field_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Describe a deserializer's `message`, which may quote the input, as
/// `<path>: <what is wrong>`.
///
/// The message is classified by the words serde puts first, and the text is a
/// fixed one. Where the message continues with something worth keeping (what
/// was expected instead), that tail is taken only after the input's own
/// rendering of the value at the path has been removed from the front of the
/// message *by comparing with the input*; if that does not match exactly, the
/// tail is dropped. What is left is the schema's text, never a value.
fn describe(path: &[Seg], found: &Found<'_>, message: &str) -> String {
    if let Some((key, text)) = split_own(message) {
        let mut at = path.to_vec();
        if !key.is_empty() {
            at.push(Seg::key(key));
        }
        return located(&at, text);
    }
    for (prefix, what) in [
        ("invalid type: ", "wrong type of value"),
        ("invalid value: ", "invalid value"),
    ] {
        if let Some(rest) = message.strip_prefix(prefix) {
            let expected = found
                .shown
                .as_deref()
                .and_then(|shown| rest.strip_prefix(shown))
                .and_then(|after| after.strip_prefix(", expected "));
            let text = match (found.kind, expected) {
                (Some(kind), Some(expected)) => {
                    format!("{what}: found {kind}, expected {expected}")
                }
                _ => what.to_string(),
            };
            return located(path, &text);
        }
    }
    if let Some(rest) = message.strip_prefix("unknown field `") {
        let tail = match path.last() {
            Some(Seg::Key(key)) => rest
                .strip_prefix(key.as_str())
                .and_then(|after| after.strip_prefix("`, expected ")),
            _ => None,
        };
        let text = match tail {
            Some(tail) => format!("unknown key (expected {tail})"),
            None => "unknown key".to_string(),
        };
        return located(path, &text);
    }
    if let Some(rest) = message.strip_prefix("unknown variant `") {
        let tail = found
            .text
            .and_then(|value| rest.strip_prefix(value))
            .and_then(|after| after.strip_prefix("`, expected "));
        let text = match tail {
            Some(tail) => format!("unknown value (expected {tail})"),
            None => "unknown value".to_string(),
        };
        return located(path, &text);
    }
    if let Some(name) = message
        .strip_prefix("missing field `")
        .and_then(|rest| rest.strip_suffix('`'))
        .filter(|name| is_field_name(name))
    {
        let mut at = path.to_vec();
        at.push(Seg::key(name));
        return located(&at, "required key is missing");
    }
    if message.starts_with("duplicate field `") {
        return located(path, "key given more than once");
    }
    if message.starts_with("invalid length ") {
        return located(path, "wrong number of elements");
    }
    if message.starts_with("UUID parsing failed") {
        return located(path, "not a valid UUID");
    }
    located(path, "invalid value")
}

// ---- TOML --------------------------------------------------------------------

/// Deserialize `value` as `T`. A failure is described by its key path (from
/// `base`, where `value` sits in the file, then within it) and its kind.
pub(crate) fn read_toml<T: DeserializeOwned>(
    value: toml::Value,
    file: &Path,
    base: &[Seg],
) -> Result<T> {
    let root = value.clone();
    serde_path_to_error::deserialize(value).map_err(|error| {
        failure(
            file,
            base,
            &root,
            &segments(error.path()),
            error.inner().message(),
        )
    })
}

/// [`read_toml`] for a table whose type is chosen by the key `tag` (an
/// internally tagged enum). The path tracker cannot follow into those, so on
/// a failure each key is offered alone, with the tag, to find the one that is
/// wrong.
pub(crate) fn read_tagged<T: DeserializeOwned>(
    table: toml::Table,
    tag: &str,
    file: &Path,
    base: &[Seg],
) -> Result<T> {
    let whole = toml::Value::Table(table.clone());
    let first = match whole.clone().try_into::<T>() {
        Ok(read) => return Ok(read),
        Err(error) => error,
    };
    for (key, value) in &table {
        if key == tag {
            continue;
        }
        let mut alone = toml::Table::new();
        if let Some(tag_value) = table.get(tag) {
            alone.insert(tag.to_string(), tag_value.clone());
        }
        alone.insert(key.clone(), value.clone());
        if let Err(error) = toml::Value::Table(alone).try_into::<T>() {
            return Err(failure(
                file,
                base,
                &whole,
                &[Seg::key(key.as_str())],
                error.message(),
            ));
        }
    }
    Err(failure(file, base, &whole, &[], first.message()))
}

fn failure(
    file: &Path,
    base: &[Seg],
    root: &toml::Value,
    path: &[Seg],
    message: &str,
) -> anyhow::Error {
    let found = toml_at(root, path).map(Found::of_toml).unwrap_or_default();
    let full = [base, path].concat();
    anyhow::anyhow!(
        "invalid configuration in {}: {}",
        file.display(),
        describe(&full, &found, message)
    )
}

/// An error for text that is not TOML at all: where it went wrong, and the
/// parser's description. The description is a fixed text of the parser (the
/// TOML crates build theirs from `&'static str` pieces; a test feeds them
/// markers to hold that), and the parser's own rendering, which quotes the
/// offending line, is never used. `what` names the file.
pub fn toml_syntax_error(
    what: &str,
    text: &str,
    message: &str,
    span: Option<Range<usize>>,
) -> anyhow::Error {
    anyhow::anyhow!("invalid TOML in {what}{}: {message}", position(text, span))
}

/// ` at line L, column C` for a byte `span` of `text` (empty without one).
pub fn position(text: &str, span: Option<Range<usize>>) -> String {
    span.map(|span| {
        let before = &text[..floor_char_boundary(text, span.start)];
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        format!(" at line {line}, column {column}")
    })
    .unwrap_or_default()
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

// ---- JSON --------------------------------------------------------------------

/// Read JSON `text` as `T`. A failure says where (line and column for
/// syntax, the key path for a wrong shape) and what kind of problem it is,
/// never the text. `what` names the file ("receipt .git/devflow/receipt.json").
pub fn json_from_str<T: DeserializeOwned>(text: &str, what: &str) -> Result<T> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let read = serde_path_to_error::deserialize(&mut deserializer)
        .map_err(|error| json_failure(what, &segments(error.path()), error.inner()))?;
    deserializer
        .end()
        .map_err(|error| json_failure(what, &[], &error))?;
    Ok(read)
}

/// [`json_from_str`] for a value already parsed.
pub fn json_from_value<T: DeserializeOwned>(value: serde_json::Value, what: &str) -> Result<T> {
    serde_path_to_error::deserialize(value)
        .map_err(|error| json_failure(what, &segments(error.path()), error.inner()))
}

fn json_failure(what: &str, path: &[Seg], error: &serde_json::Error) -> anyhow::Error {
    use serde_json::error::Category;
    let position = if error.line() > 0 {
        format!(" at line {}, column {}", error.line(), error.column())
    } else {
        String::new()
    };
    let text = match error.classify() {
        Category::Io => "cannot be read".to_string(),
        Category::Syntax => "not valid JSON".to_string(),
        Category::Eof => "ends unexpectedly".to_string(),
        Category::Data => {
            // serde_json adds " at line L column C" to the message of a
            // deserializer error; the numbers are known, so cut it exactly.
            let message = error.to_string();
            let suffix = format!(" at line {} column {}", error.line(), error.column());
            let message = message.strip_suffix(&suffix).unwrap_or(&message);
            describe(path, &Found::default(), message)
        }
    };
    anyhow::anyhow!("{what}: {text}{position}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARK: &str = "ZZMARKER4172";

    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Inner {
        #[allow(dead_code)]
        port: u16,
        #[allow(dead_code)]
        mode: Mode,
        #[allow(dead_code)]
        name: String,
    }

    #[derive(Debug, serde::Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum Mode {
        Managed,
        External,
    }

    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Outer {
        #[allow(dead_code)]
        inner: Inner,
        #[allow(dead_code)]
        list: Vec<String>,
    }

    fn explain_toml(text: &str) -> String {
        let table: toml::Table = text.parse().unwrap();
        let error =
            read_toml::<Outer>(toml::Value::Table(table), Path::new("f.toml"), &[]).unwrap_err();
        format!("{error:#}")
    }

    #[test]
    fn keys_are_named_only_when_they_are_plain() {
        assert_eq!(key_label("api-key_2"), "api-key_2");
        for odd in ["", "a b", "a.b", "a`b", "é", &"k".repeat(65), "a\nb"] {
            assert_eq!(key_label(odd), "<key not shown>", "{odd:?}");
        }
        assert_eq!(
            render_path(&[
                Seg::key("services"),
                Seg::key("db"),
                Seg::key("extra"),
                Seg::Index(2)
            ]),
            "services.db.extra[2]"
        );
        assert_eq!(
            render_path(&[Seg::key("env"), Seg::key("a b")]),
            "env.<key not shown>"
        );
    }

    #[test]
    fn a_failure_names_the_path_and_the_kind_and_never_the_value() {
        let base = "[inner]\nport = 1\nmode = \"managed\"\nname = \"n\"\n";
        for (text, wanted) in [
            (
                format!(
                    "list = []\n[inner]\nport = \"{MARK}\"\nmode = \"managed\"\nname = \"n\"\n"
                ),
                "inner.port: wrong type of value: found a string, expected u16",
            ),
            (
                format!("list = []\n[inner]\nport = 1\nmode = \"{MARK}\"\nname = \"n\"\n"),
                "inner.mode: unknown value (expected `managed` or `external`)",
            ),
            (
                format!("list = [\"a\", 3]\n{base}"),
                "list[1]: wrong type of value: found an integer, expected a string",
            ),
            (
                format!(
                    "list = []\n[inner]\nport = 1\nmode = \"managed\"\nname = \"n\"\n{MARK}_key = 1\n"
                ),
                "inner.ZZMARKER4172_key: unknown key (expected one of `port`, `mode`, `name`)",
            ),
            (
                format!(
                    "list = []\n[inner]\nport = 99999\nmode = \"managed\"\nname = \"{MARK}\"\n"
                ),
                "inner.port: invalid value: found an integer, expected u16",
            ),
            (
                format!("list = []\n[inner]\nport = 1\nmode = \"managed\"\n{MARK} = 3\n"),
                "unknown key",
            ),
        ] {
            let error = explain_toml(&text);
            assert!(error.contains(wanted), "{text:?}\n{error}");
            // A key is a name and may be shown; no value ever is.
            let without_keys = error.replace("ZZMARKER4172_key", "").replace(MARK, "");
            assert!(!without_keys.contains("ZZMARKER"), "{error}");
        }
        // A quoted key that is not plain is not repeated either.
        let error = explain_toml(&format!(
            "list = []\n[inner]\nport = 1\nmode = \"managed\"\nname = \"n\"\n\"{MARK} x\" = 3\n"
        ));
        assert!(
            error.contains("inner.<key not shown>: unknown key"),
            "{error}"
        );
        assert!(!error.contains(MARK), "{error}");
        assert!(explain_toml("list = []\n").contains("inner: required key is missing"));
    }

    #[test]
    fn a_value_cannot_forge_the_part_of_a_message_that_is_kept() {
        // The delimiters a sanitizer would look for, inside the value.
        for payload in [
            format!("bad`, expected {MARK}"),
            format!("bad\", expected {MARK}"),
            format!("`, expected one of `{MARK}`"),
            format!("{MARK}`, expected `managed`"),
        ] {
            let text = format!("list = []\n[inner]\nport = 1\nmode = {payload:?}\nname = \"n\"\n");
            let error = explain_toml(&text);
            assert!(!error.contains(MARK), "{payload}\n{error}");
            assert!(error.contains("inner.mode: unknown value"), "{error}");
        }
    }

    #[test]
    fn own_errors_show_their_text_and_a_plain_key() {
        #[derive(Debug)]
        struct Probe;
        impl<'de> serde::Deserialize<'de> for Probe {
            fn deserialize<D: serde::Deserializer<'de>>(_: D) -> Result<Self, D::Error> {
                Err(own(Some("odd key"), "cannot be used"))
            }
        }
        let error =
            read_toml::<Probe>(toml::Value::Boolean(true), Path::new("f.toml"), &[]).unwrap_err();
        assert_eq!(
            format!("{error}"),
            "invalid configuration in f.toml: <key not shown>: cannot be used"
        );
    }

    #[test]
    fn json_errors_carry_a_position_or_a_path_but_no_text() {
        #[derive(Debug, serde::Deserialize)]
        struct Receipt {
            #[allow(dead_code)]
            url: String,
            #[allow(dead_code)]
            version: u32,
        }
        for (text, wanted) in [
            (
                format!("{{\"url\": \"{MARK}\", \"version\": \"{MARK}\"}}"),
                "receipt: version: wrong type of value at line 1",
            ),
            (
                format!("{{\"url\": \"{MARK}\"}}"),
                "receipt: version: required key is missing at line 1",
            ),
            (
                format!("{{\"url\": \"{MARK}\", \"version\": 1,, }}"),
                "receipt: not valid JSON at line 1",
            ),
            (format!("{{\"url\": \"{MARK}"), "receipt: ends unexpectedly"),
            (
                format!("{{\"url\": 7, \"version\": \"{MARK}\"}}"),
                "receipt: url: wrong type of value at line 1",
            ),
        ] {
            let error = json_from_str::<Receipt>(&text, "receipt").unwrap_err();
            let shown = format!("{error:#} {error:?}");
            assert!(shown.contains(wanted), "{text}\n{shown}");
            assert!(!shown.contains(MARK), "{text}\n{shown}");
        }
        // Text after the document is a syntax error too.
        let error = json_from_str::<u32>(&format!("1 {MARK}"), "receipt").unwrap_err();
        assert_eq!(
            format!("{error}"),
            "receipt: not valid JSON at line 1, column 3"
        );
        let value = serde_json::json!({"url": 3, "version": MARK});
        let error = json_from_value::<Receipt>(value, "plan").unwrap_err();
        assert_eq!(format!("{error}"), "plan: url: wrong type of value");
    }

    #[test]
    fn toml_syntax_errors_locate_the_problem_without_the_line() {
        let text = format!("a = 1\nb = \"{MARK}\nc = 2\n");
        let error = text.parse::<toml::Table>().unwrap_err();
        let shown = toml_syntax_error("f.toml", &text, error.message(), error.span());
        let shown = format!("{shown:#}");
        assert!(
            shown.starts_with("invalid TOML in f.toml at line 2"),
            "{shown}"
        );
        assert!(!shown.contains(MARK), "{shown}");
        // A span in the middle of a multibyte character does not panic.
        let shown = toml_syntax_error("f.toml", "é = 1", "x", Some(1..2));
        assert!(format!("{shown}").contains("line 1, column 1"), "{shown}");
    }
}
