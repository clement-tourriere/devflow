//! Rewrite an earlier 1.0 build's configuration in the current format.
//!
//! Before services existed, the PostgreSQL server was the top-level
//! `[postgres]` table. It is now `[services.<key>]` with `engine = "postgres"`
//! and the same keys, and devflow refuses a file that still has the old table
//! (see [`ShapeKind::PreRelease`](crate::error::ShapeKind)). [`plan`] works out
//! the rewrite of `devflow.toml` (or `.devflow.toml`) and `devflow.local.toml`
//! without writing anything and [`apply`] writes it, so a frontend can show the
//! change and ask first.
//!
//! # The service key is `db`
//!
//! The key names the service in receipts and in output; what identifies the
//! server and the data does not depend on it. The managed server lives in
//! `<data dir>/postgres/<project id>` and runs as the compose project
//! `devflow-<name>-<id8>`; its credentials, its volume, the `devflow` metadata
//! database (branches, snapshots, roles) and every database name come from the
//! project's id. A version 1 receipt's `data` binding moves to the project's
//! only PostgreSQL service whatever its key
//! ([`Receipt::migrate`](crate::receipt::Receipt::migrate)), and a service's
//! generation is a hash of its content, not of its key. So `db`, the name the
//! documentation and the refusal message use, keeps the existing server,
//! branches and workspaces working; the tests below check each of these. Only
//! the OrbStack host name (`db.<project>-<id8>.devflow.orb.local`) and the
//! starting point of a first port allocation use the key, and a server that
//! already has a port keeps it.
//!
//! # What is preserved and what is refused
//!
//! The file is edited with `toml_edit`, so comments, ordering and spacing stay;
//! the table keeps its place in the file and only its header changes. Before
//! anything is written the result is checked twice: it must parse to exactly
//! the values the old file had (the table moved, `engine` added, nothing else
//! different) and the merged configuration must be valid. A file that has both
//! `[postgres]` and a PostgreSQL service, a `[services.db]` that is not
//! PostgreSQL, or a 0.x shape, is refused with a [`Problem`] and nothing is
//! written.
use super::{ConfigFiles, FileState, Target, file_name};
use crate::config::{Config, LOCAL_CONFIG_FILE};
use crate::error::ConfigChanged;
use crate::propose::{DiffKind, FileChange, FilePreview, diff_lines};
use crate::remedy::{Problem, Remedy, remedies_of};
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Key, Table, Value};

/// The key the old `[postgres]` table gets, unless the project already names
/// its PostgreSQL service otherwise.
pub const SERVICE_KEY: &str = "db";

/// The rewrite of one configuration file.
#[derive(Debug, Clone)]
pub struct FileMigration {
    pub target: Target,
    pub path: PathBuf,
    /// The file as it is now, and its hash (what [`apply`] expects to find).
    pub before: String,
    pub before_hash: String,
    /// The file as it would be written.
    pub after: String,
    /// The changed lines, for showing the change.
    pub preview: FilePreview,
}

/// What [`apply`] would write.
#[derive(Debug, Clone)]
pub struct Migration {
    pub root: PathBuf,
    /// The key of the PostgreSQL service in the new format.
    pub service_key: String,
    /// The files that change (the project file, then the local one).
    pub files: Vec<FileMigration>,
    /// Things worth telling the person before they accept.
    pub notes: Vec<String>,
}

/// Whether a configuration file of the project in `root` still has the
/// top-level `[postgres]` table (in any form). Reads only; `NotInitialized`
/// without a project file.
pub fn needed(root: &Path) -> Result<bool> {
    let files = ConfigFiles::read(root)?;
    Ok([&files.project, &files.local].into_iter().any(|state| {
        state
            .text
            .as_deref()
            .and_then(|text| text.parse::<toml::Table>().ok())
            .is_some_and(|table| table.contains_key("postgres"))
    }))
}

/// Work out the rewrite for the project in `root`. Reads only.
///
/// Fails with a [`Problem`] when there is nothing to migrate or the rewrite
/// would be wrong (see the module documentation); with `NotInitialized` when
/// there is no project file.
pub fn plan(root: &Path) -> Result<Migration> {
    let files = ConfigFiles::read(root)?;
    let mut docs: Vec<(Target, &FileState, DocumentMut)> = Vec::new();
    for target in [Target::Project, Target::Local] {
        let state = files.file(target);
        let Some(text) = &state.text else { continue };
        let doc: DocumentMut = text.parse().map_err(|_| {
            not_automatic(
                format!("{} is not valid TOML", file_name(&state.path)),
                &files,
            )
        })?;
        docs.push((target, state, doc));
    }
    let with_table: Vec<Target> = docs
        .iter()
        .filter(|(_, _, doc)| doc.contains_key("postgres"))
        .map(|(target, _, _)| *target)
        .collect();
    if with_table.is_empty() {
        return Err(Problem::new(
            "no configuration file uses the top-level [postgres] table; there is nothing to migrate",
        )
        .into_error());
    }

    // The files' own PostgreSQL services, by key.
    let mut own_key: Vec<(Target, String)> = Vec::new();
    for (target, state, doc) in &docs {
        for key in postgres_services(doc) {
            if with_table.contains(target) {
                return Err(not_automatic(
                    format!(
                        "{} has both the top-level [postgres] table and a PostgreSQL service \
                         (services.{}); decide which one is right and delete the other",
                        file_name(&state.path),
                        crate::diag::key_label(&key)
                    ),
                    &files,
                ));
            }
            own_key.push((*target, key));
        }
    }
    let service_key = own_key
        .first()
        .map(|(_, key)| key.clone())
        .unwrap_or_else(|| SERVICE_KEY.to_string());

    let mut notes = Vec::new();
    // The project file carries `engine`; the local file only when the project
    // file, as it will be, has no PostgreSQL service for it to overlay.
    let project_has_service = docs.iter().any(|(target, _, doc)| {
        *target == Target::Project
            && (doc.contains_key("postgres") || !postgres_services(doc).is_empty())
    });
    let mut moved: Vec<(Target, &FileState, String)> = Vec::new();
    for (target, state, mut doc) in docs {
        if !doc.contains_key("postgres") {
            continue;
        }
        let add_engine = target == Target::Project || !project_has_service;
        // What the file must read as afterwards, worked out on values so it
        // does not depend on how the text is edited.
        let expected = state
            .text
            .as_deref()
            .and_then(|text| text.parse::<toml::Table>().ok())
            .and_then(|table| moved_values(table, &service_key, add_engine));
        move_table(&mut doc, &service_key, add_engine).map_err(|reason| {
            not_automatic(format!("{}: {reason}", file_name(&state.path)), &files)
        })?;
        let after = super::text::finish(&doc, state.text.as_deref());
        if expected.is_none() || expected != after.parse::<toml::Table>().ok() {
            return Err(not_automatic(
                format!(
                    "{} has a layout this migration cannot rewrite safely",
                    file_name(&state.path)
                ),
                &files,
            ));
        }
        moved.push((target, state, after));
    }
    // And the merged result must be a valid configuration.
    let text_of = |target: Target| -> Option<String> {
        match moved.iter().find(|(t, _, _)| *t == target) {
            Some((_, _, after)) => Some(after.clone()),
            None => files.file(target).text.clone(),
        }
    };
    let project_text = text_of(Target::Project).unwrap_or_default();
    let local_text = text_of(Target::Local);
    if let Err(error) = Config::parse_layers(
        &project_text,
        &file_name(&files.project.path),
        local_text.as_deref(),
        LOCAL_CONFIG_FILE,
    ) {
        return Err(not_automatic(
            format!("the configuration would still not be valid after the change: {error:#}"),
            &files,
        )
        .context_remedies(&error));
    }

    let mut out = Vec::new();
    for (target, state, after) in moved {
        let before = state.text.clone().unwrap_or_default();
        let diff = diff_lines(&before, &after, 2);
        let count = |kind| diff.iter().filter(|l| l.kind == kind).count();
        out.push(FileMigration {
            target,
            path: state.path.clone(),
            before_hash: state.hash.clone().unwrap_or_default(),
            preview: FilePreview {
                file: file_name(&state.path),
                change: FileChange::Modified,
                after: after.clone(),
                added: count(DiffKind::Added),
                removed: count(DiffKind::Removed),
                diff,
                conflicts: Vec::new(),
            },
            before,
            after,
        });
    }
    notes.push(format!(
        "[postgres] becomes [services.{}] with engine = \"postgres\"; the keys inside are \
         unchanged, so the server, its databases and your workspaces keep working",
        crate::diag::key_label(&service_key)
    ));
    notes.push(
        "each workspace rewrites its .devflow/env the next time it is set up (without touching \
         its database)"
            .to_string(),
    );
    Ok(Migration {
        root: root.to_path_buf(),
        service_key,
        files: out,
        notes,
    })
}

/// Write the files of `migration`. Each is replaced only if it still holds
/// what [`plan`] read (otherwise the error is [`ConfigChanged`] and that file
/// is left alone), the project file first: if the second write fails, the
/// project is migrated and a new [`plan`] finds only what is left.
///
/// Returns the paths written.
pub fn apply(migration: &Migration) -> Result<Vec<PathBuf>> {
    let go = || -> Result<Vec<PathBuf>> {
        let current = ConfigFiles::read(&migration.root)?;
        for file in &migration.files {
            let state = current.file(file.target);
            if state.hash.as_deref() != Some(file.before_hash.as_str()) {
                return Err(ConfigChanged::new(state.path.clone()).into());
            }
            if std::fs::symlink_metadata(&state.path).is_ok_and(|m| m.file_type().is_symlink()) {
                bail!(
                    "{} is a symbolic link; devflow will not rewrite it",
                    file_name(&state.path)
                );
            }
        }
        let mut written = Vec::new();
        for file in &migration.files {
            super::write_unless_changed(&file.path, &file.after, Some(&file.before_hash))?;
            written.push(file.path.clone());
        }
        Ok(written)
    };
    match git2::Repository::open(&migration.root) {
        Ok(repo) => {
            crate::fsutil::with_lock(&repo.commondir().join("devflow/config-edit.lock"), go)
        }
        Err(_) => go(),
    }
}

/// [`plan`] and [`apply`] in one step.
pub fn migrate(root: &Path) -> Result<Migration> {
    let migration = plan(root)?;
    apply(&migration)?;
    Ok(migration)
}

// -- the rewrite --------------------------------------------------------------

/// The keys of `doc`'s `[services.*]` tables whose engine is `postgres`.
fn postgres_services(doc: &DocumentMut) -> Vec<String> {
    let Some(services) = doc.get("services").and_then(Item::as_table_like) else {
        return Vec::new();
    };
    services
        .iter()
        .filter(|(_, entry)| {
            entry
                .as_table_like()
                .and_then(|t| t.get("engine"))
                .and_then(Item::as_str)
                == Some("postgres")
        })
        .map(|(key, _)| key.to_string())
        .collect()
}

/// Move `[postgres]` to `[services.<key>]`, keeping its place, comments and
/// keys; `engine` goes first when `add_engine`.
fn move_table(doc: &mut DocumentMut, key: &str, add_engine: bool) -> Result<(), String> {
    let Some(item) = doc.remove("postgres") else {
        return Ok(());
    };
    let mut table = match item {
        Item::Table(table) => table,
        Item::Value(Value::InlineTable(inline)) => inline.into_table(),
        _ => return Err("`postgres` is not a table".to_string()),
    };
    // A table written as `postgres.mode = "managed"` gets a header of its own.
    table.set_dotted(false);
    if add_engine {
        let mut entries: Vec<(Key, Item)> = Vec::new();
        let names: Vec<String> = table.iter().map(|(name, _)| name.to_string()).collect();
        for name in names {
            if let Some(entry) = table.remove_entry(&name) {
                entries.push(entry);
            }
        }
        table.insert("engine", toml_edit::value("postgres"));
        for (name, item) in entries {
            table.insert_formatted(&name, item);
        }
    }
    let services = match doc.get_mut("services") {
        Some(Item::Table(services)) => services,
        Some(_) => {
            return Err("`services` is not written as tables; migrate it by hand".to_string());
        }
        None => {
            let mut services = Table::new();
            services.set_implicit(true);
            doc.insert("services", Item::Table(services));
            match doc.get_mut("services") {
                Some(Item::Table(services)) => services,
                _ => return Err("cannot add `services`".to_string()),
            }
        }
    };
    if services.is_dotted() {
        return Err("`services` is written with dotted keys; migrate it by hand".to_string());
    }
    if services.contains_key(key) {
        return Err(format!(
            "services.{} already exists and is not the PostgreSQL service",
            crate::diag::key_label(key)
        ));
    }
    services.insert(key, Item::Table(table));
    Ok(())
}

/// `table` with its `postgres` table moved to `services.<key>`, and `engine`
/// added first when `add_engine`. `None` when `postgres` is not a table.
fn moved_values(mut table: toml::Table, key: &str, add_engine: bool) -> Option<toml::Table> {
    let mut section = match table.remove("postgres")? {
        toml::Value::Table(section) => section,
        _ => return None,
    };
    if add_engine {
        section.insert("engine".into(), "postgres".into());
    }
    let services = table
        .entry("services")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()?;
    services.insert(key.to_string(), toml::Value::Table(section));
    Some(table)
}

/// A refusal: what is wrong, with what to do about it (the file to look at, or
/// the guide for a 0.x shape).
fn not_automatic(message: String, files: &ConfigFiles) -> anyhow::Error {
    Problem::new(format!("cannot migrate automatically: {message}"))
        .with(Remedy::OpenFile {
            path: files.project.path.clone(),
        })
        .into_error()
}

trait ContextRemedies {
    fn context_remedies(self, cause: &anyhow::Error) -> anyhow::Error;
}

impl ContextRemedies for anyhow::Error {
    /// Add the cause's own remedies (a 0.x shape points at the guide) to a
    /// [`Problem`], leaving out the migration this very refusal is about.
    fn context_remedies(self, cause: &anyhow::Error) -> anyhow::Error {
        let Some(problem) = self.downcast_ref::<Problem>() else {
            return self;
        };
        let mut problem = problem.clone();
        for remedy in remedies_of(cause) {
            if !matches!(remedy, Remedy::MigrateConfig { .. }) {
                problem = problem.with(remedy);
            }
        }
        problem.into_error()
    }
}

#[cfg(test)]
mod tests;
