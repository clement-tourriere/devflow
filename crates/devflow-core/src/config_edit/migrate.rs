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
//! PostgreSQL in either layer (the project file's cache must not become a
//! database because a local overlay says `[postgres]`), or a 0.x shape, is
//! refused with a [`Problem`] and nothing is written. When the key is taken by
//! another engine devflow refuses rather than choosing another key: the data
//! would survive (it follows the project's id), but the documentation, the
//! refusal message and the host name all say `db`, and a second guess about
//! what the person meant is theirs to make.
//!
//! # Previews hold no credentials
//!
//! `devflow.local.toml` is where a person puts `SESSION_KEY = "..."` or
//! `password = "..."`, and a diff shows the lines around a change. The preview
//! ([`FileMigration::preview`]) is therefore built from a masked copy of both
//! texts: every value is masked by its *name* (the rules of `devflow env list`,
//! [`masked_template`]) and by its content (the patterns of [`redact::text`]).
//! Comments are masked too (a commented `# SESSION_KEY = "..."` is as much a
//! credential as the line it was before), in the diff, in `preview.after` and
//! so in `Debug`. The real text is kept in the crate for [`apply`] only;
//! neither `Debug` nor the preview carries it.
//!
//! # Apply
//!
//! [`apply`] writes only if both layers are what [`plan`] read (the contents,
//! their absence, and which file is the project file), checks the merged result
//! again, and writes the project file and then the local one. If the second
//! write fails the first is put back, and the error says what was written. A
//! refused write may still have installed its replacement (the fallback
//! without an atomic exchange only finds a concurrent edit afterwards), so the
//! failed file is reconciled too, by what is on disk; the error names what was
//! put back, what is still migrated and every file that keeps a version a
//! concurrent save would have lost.
use super::{ConfigFiles, FileState, Target, file_name, masked_template};
use crate::config::{Config, LOCAL_CONFIG_FILE};
use crate::error::{ConfigChanged, NotInitialized};
use crate::propose::{DiffKind, FileChange, FilePreview, diff_lines, sha256_hex};
use crate::redact::{self, MASK};
use crate::remedy::{Problem, Remedy, remedies_of};
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Key, Table, Value};

/// The key the old `[postgres]` table gets, unless the project already names
/// its PostgreSQL service otherwise.
pub const SERVICE_KEY: &str = "db";

/// The rewrite of one configuration file.
#[derive(Clone)]
pub struct FileMigration {
    pub target: Target,
    pub path: PathBuf,
    /// The file as it is now, and its hash (what [`apply`] expects to find).
    /// May hold credentials: for [`apply`] only, never shown.
    pub(crate) before: String,
    pub before_hash: String,
    /// The file as it would be written (credentials included, as `before`).
    pub(crate) after: String,
    /// The changed lines, for showing the change: masked (see the module
    /// documentation).
    pub preview: FilePreview,
}

impl std::fmt::Debug for FileMigration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileMigration")
            .field("target", &self.target)
            .field("path", &self.path)
            .field("before_hash", &self.before_hash)
            .field("preview", &self.preview)
            .finish_non_exhaustive()
    }
}

/// What [`plan`] saw of the two layers, which [`apply`] requires to still be
/// true: absence counts, and so does which file is the project file
/// (`devflow.toml` or `.devflow.toml`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Layers {
    project_path: PathBuf,
    project_hash: Option<String>,
    local_path: PathBuf,
    local_hash: Option<String>,
}

impl Layers {
    fn of(files: &ConfigFiles) -> Self {
        Self {
            project_path: files.project.path.clone(),
            project_hash: files.project.hash.clone(),
            local_path: files.local.path.clone(),
            local_hash: files.local.hash.clone(),
        }
    }

    /// The first file that is not what it was.
    fn changed(&self, now: &Layers) -> Option<PathBuf> {
        if self.project_path != now.project_path || self.project_hash != now.project_hash {
            Some(now.project_path.clone())
        } else if self.local_hash != now.local_hash {
            Some(now.local_path.clone())
        } else {
            None
        }
    }
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
    layers: Layers,
}

/// Whether a configuration file of the project in `root` still has the
/// top-level `[postgres]` table (in any form). Reads only; `NotInitialized`
/// without a project file.
///
/// `Ok(false)` means the configuration is valid as it is: there is nothing to
/// migrate. A configuration that cannot be read (a file that is not valid
/// TOML, a 0.x layout, a value that is refused) is an error with the problem's
/// own remedies, never "nothing to migrate": when the files cannot be read the
/// answer to "does this need migrating" is not "no".
pub fn needed(root: &Path) -> Result<bool> {
    let files = read_files(root)?;
    let mut found = false;
    for state in [&files.project, &files.local] {
        let Some(text) = state.text.as_deref() else {
            continue;
        };
        let table = text
            .parse::<toml::Table>()
            .map_err(|error| invalid_toml(state, text, error.message(), error.span()))?;
        found |= table.contains_key("postgres");
    }
    if found {
        return Ok(true);
    }
    // No `[postgres]`: the configuration is current, or it is something else
    // devflow refuses (0.x, invalid); loading it says which.
    Config::load(root)?;
    Ok(false)
}

/// [`ConfigFiles::read`], except that a checkout without a TOML project file
/// but with a 0.x one (`.devflow.yml`) gets the error loading gives it (0.x
/// layout, with the migration guide) rather than "not initialized".
fn read_files(root: &Path) -> Result<ConfigFiles> {
    match ConfigFiles::read(root) {
        Err(error) if error.downcast_ref::<NotInitialized>().is_some() => {
            Err(Config::load(root).err().unwrap_or(error))
        }
        other => other,
    }
}

/// Work out the rewrite for the project in `root`. Reads only.
///
/// Fails with a [`Problem`] when there is nothing to migrate or the rewrite
/// would be wrong (see the module documentation); with `NotInitialized` when
/// there is no project file.
pub fn plan(root: &Path) -> Result<Migration> {
    let files = read_files(root)?;
    let mut docs: Vec<(Target, &FileState, DocumentMut)> = Vec::new();
    for target in [Target::Project, Target::Local] {
        let state = files.file(target);
        let Some(text) = &state.text else { continue };
        let doc: DocumentMut = text.parse().map_err(|error: toml_edit::TomlError| {
            invalid_toml(state, text, error.message(), error.span())
        })?;
        docs.push((target, state, doc));
    }
    let with_table: Vec<Target> = docs
        .iter()
        .filter(|(_, _, doc)| doc.contains_key("postgres"))
        .map(|(target, _, _)| *target)
        .collect();
    if with_table.is_empty() {
        // Nothing to move; but a configuration that is refused for another
        // reason (0.x, invalid) is not "nothing to migrate".
        Config::load(root)?;
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

    // The key must not belong to another engine in either layer: the local
    // file's overlay is read on top of the project file's service.
    if let Some((engine, file)) = taken_by_other_engine(&docs, &service_key) {
        return Err(not_automatic(
            format!(
                "services.{} already exists in {} and is not the PostgreSQL service \
                 (engine {}); migrating would turn it into one. Rename or remove that \
                 service first",
                crate::diag::key_label(&service_key),
                file_name(&file),
                engine.as_deref().unwrap_or("unset"),
            ),
            &files,
        ));
    }

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
    let replacements: Vec<(Target, &str)> = moved
        .iter()
        .map(|(target, _, after)| (*target, after.as_str()))
        .collect();
    if let Err(error) = check_merged(&files, &replacements) {
        return Err(not_automatic(
            format!("the configuration would still not be valid after the change: {error:#}"),
            &files,
        )
        .context_remedies(&error));
    }

    let mut out = Vec::new();
    for (target, state, after) in moved {
        let before = state.text.clone().unwrap_or_default();
        // The preview is built from masked copies; the real texts stay for apply.
        let shown_before = masked_text(&before);
        let shown_after = masked_text(&after);
        let diff: Vec<_> = diff_lines(&shown_before, &shown_after, 2)
            .into_iter()
            .map(|mut line| {
                line.text = redact::text(&line.text);
                line
            })
            .collect();
        let count = |kind| diff.iter().filter(|l| l.kind == kind).count();
        out.push(FileMigration {
            target,
            path: state.path.clone(),
            before_hash: state.hash.clone().unwrap_or_default(),
            preview: FilePreview {
                file: file_name(&state.path),
                change: FileChange::Modified,
                after: shown_after,
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
        layers: Layers::of(&files),
    })
}

/// Write the files of `migration`. Nothing is written unless both layers are
/// still what [`plan`] read (a file that changed, appeared or disappeared, or
/// a different project file name, is [`ConfigChanged`]) and the merged result
/// is valid. The project file is written first; if the second write fails the
/// first is put back (unless it was changed meanwhile) and the error says what
/// was written and what was not.
///
/// Returns the paths written.
pub fn apply(migration: &Migration) -> Result<Vec<PathBuf>> {
    let go = || -> Result<Vec<PathBuf>> {
        let current = ConfigFiles::read(&migration.root)?;
        if let Some(path) = migration.layers.changed(&Layers::of(&current)) {
            return Err(ConfigChanged::new(path).into());
        }
        for file in &migration.files {
            let state = current.file(file.target);
            if std::fs::symlink_metadata(&state.path).is_ok_and(|m| m.file_type().is_symlink()) {
                bail!(
                    "{} is a symbolic link; devflow will not rewrite it",
                    file_name(&state.path)
                );
            }
        }
        // The same check as the plan's, on what is on disk now.
        let replacements: Vec<(Target, &str)> = migration
            .files
            .iter()
            .map(|file| (file.target, file.after.as_str()))
            .collect();
        if let Err(error) = check_merged(&current, &replacements) {
            return Err(not_automatic(
                format!("the configuration would still not be valid after the change: {error:#}"),
                &current,
            )
            .context_remedies(&error));
        }
        let mut written: Vec<&FileMigration> = Vec::new();
        for file in &migration.files {
            if let Err(error) =
                super::write_unless_changed(&file.path, &file.after, Some(&file.before_hash))
            {
                // A refused replacement can still have installed ours (the
                // fallback without an atomic exchange swaps the file in and
                // only then finds the edit it displaced): reconcile the
                // failed target as well as the earlier writes.
                let installed = std::fs::read(&file.path).is_ok_and(|bytes| {
                    crate::fsutil::sha256_hex(&bytes) == sha256_hex(&file.after)
                });
                return Err(undo(error, file, installed, &written));
            }
            written.push(file);
        }
        Ok(written.iter().map(|file| file.path.clone()).collect())
    };
    match git2::Repository::open(&migration.root) {
        Ok(repo) => {
            crate::fsutil::with_lock(&repo.commondir().join("devflow/config-edit.lock"), go)
        }
        Err(_) => go(),
    }
}

/// `failed` could not be written: put back every file that this migration
/// left rewritten (the `written` ones, and `failed` itself when its
/// replacement went in before the refusal: `installed`) unless it changed
/// since, and say how it ended: what was put back, what is still migrated,
/// and every file that keeps a version a concurrent save would have lost.
fn undo(
    error: anyhow::Error,
    failed: &FileMigration,
    installed: bool,
    written: &[&FileMigration],
) -> anyhow::Error {
    let name = file_name(&failed.path);
    if written.is_empty() && !installed {
        return error.context(format!("{name} was not written; nothing was changed"));
    }
    let mut restored = Vec::new();
    let mut stuck = Vec::new();
    let mut preserved: Vec<PathBuf> = kept_by(&error).into_iter().collect();
    let rewritten = installed
        .then_some(failed)
        .into_iter()
        .chain(written.iter().rev().copied());
    for file in rewritten {
        let wrote = sha256_hex(&file.after);
        match super::write_unless_changed(&file.path, &file.before, Some(&wrote)) {
            Ok(()) => restored.push(file_name(&file.path)),
            Err(again) => {
                preserved.extend(kept_by(&again));
                // A refusal can come after the original went back in (the
                // fallback without an atomic exchange finds the edit it
                // displaced only afterwards): judge by what is on disk.
                let back = std::fs::read(&file.path).is_ok_and(|bytes| {
                    crate::fsutil::sha256_hex(&bytes) == sha256_hex(&file.before)
                });
                if back {
                    restored.push(file_name(&file.path));
                } else {
                    stuck.push(file_name(&file.path));
                }
            }
        }
    }
    let mut message = if stuck.is_empty() {
        format!(
            "{name} could not be written, so {} put back as it was; nothing was changed",
            listed(&restored)
        )
    } else {
        let mut message = format!(
            "{name} could not be written and {} already migrated and could not be put back \
             (changed meanwhile or cannot be written); run the migration again or finish by hand",
            listed(&stuck)
        );
        if !restored.is_empty() {
            message.push_str(&format!("; {} put back as it was", listed(&restored)));
        }
        message
    };
    preserved.sort();
    preserved.dedup();
    if !preserved.is_empty() {
        let paths: Vec<_> = preserved.iter().map(|p| p.display().to_string()).collect();
        message.push_str(&format!(
            "; a version saved meanwhile is kept at {} (compare it before deleting it)",
            paths.join(", ")
        ));
    }
    error.context(message)
}

/// The file a refused write kept a concurrent save in, if any.
fn kept_by(error: &anyhow::Error) -> Option<PathBuf> {
    error
        .downcast_ref::<ConfigChanged>()
        .and_then(|changed| changed.kept.clone())
}

/// `a was` / `a, b were`.
fn listed(names: &[String]) -> String {
    format!(
        "{} {}",
        names.join(", "),
        if names.len() == 1 { "was" } else { "were" }
    )
}

/// [`plan`] and [`apply`] in one step.
pub fn migrate(root: &Path) -> Result<Migration> {
    let migration = plan(root)?;
    apply(&migration)?;
    Ok(migration)
}

// -- the rewrite --------------------------------------------------------------

/// The merged configuration with `replacements` in place of the layers' texts,
/// parsed the way [`Config::load`] reads it.
fn check_merged(files: &ConfigFiles, replacements: &[(Target, &str)]) -> Result<()> {
    let text_of = |target: Target| -> Option<String> {
        match replacements.iter().find(|(t, _)| *t == target) {
            Some((_, text)) => Some((*text).to_string()),
            None => files.file(target).text.clone(),
        }
    };
    let project_text = text_of(Target::Project).unwrap_or_default();
    let local_text = text_of(Target::Local);
    Config::parse_layers(
        &project_text,
        &file_name(&files.project.path),
        local_text.as_deref(),
        LOCAL_CONFIG_FILE,
    )
    .map(|_| ())
}

/// `Some((engine, file))` when `services.<key>` exists in a layer and the
/// engine it has once the layers are merged (the local file's wins) is not
/// `postgres`; `file` is the layer that decides the engine, or the first one
/// that has the key.
fn taken_by_other_engine(
    docs: &[(Target, &FileState, DocumentMut)],
    key: &str,
) -> Option<(Option<String>, PathBuf)> {
    let mut found: Option<(Option<String>, PathBuf)> = None;
    for (_, state, doc) in docs {
        let Some(entry) = doc
            .get("services")
            .and_then(Item::as_table_like)
            .and_then(|services| services.get(key))
            .and_then(Item::as_table_like)
        else {
            continue;
        };
        let engine = entry.get("engine").and_then(Item::as_str).map(String::from);
        found = match (found, engine) {
            (None, engine) => Some((engine, state.path.clone())),
            (Some(_), Some(engine)) => Some((Some(engine), state.path.clone())),
            (Some(previous), None) => Some(previous),
        };
    }
    found.filter(|(engine, _)| engine.as_deref() != Some("postgres"))
}

// -- the masked copy -----------------------------------------------------------

/// `text` with every value that may be a credential replaced by [`MASK`],
/// for showing. TOML-aware: a value is judged by the name of its key, as
/// `devflow env list` does ([`masked_template`]), and by its content.
///
/// Comments are text the value masking never sees (they are trivia around the
/// values), and a person comments out `# SESSION_KEY = "..."` as readily as
/// they write it: [`mask_comments`] runs over the result, and a last pass of
/// [`redact::text`] over every line covers keys and headers.
fn masked_text(text: &str) -> String {
    let masked = match text.parse::<DocumentMut>() {
        Ok(mut doc) => {
            for (key, item) in doc.iter_mut() {
                mask_item(item, &key);
            }
            doc.to_string()
        }
        Err(_) => text.to_string(),
    };
    redact_lines(&mask_comments(&masked))
}

/// [`redact::text`] over each line, so that no pattern spans two of them.
fn redact_lines(text: &str) -> String {
    text.split_inclusive('\n')
        .map(|line| {
            let body = line.trim_end_matches(['\n', '\r']);
            format!("{}{}", redact::text(body), &line[body.len()..])
        })
        .collect()
}

/// `text` with the credentials in its comments masked. A small TOML lexer
/// finds the comments (a `#` inside a string is not one), wherever they are:
/// above a table, after a value, inside an array or an inline table.
fn mask_comments(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '#' {
            let start = i;
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            let comment: String = chars[start..i].iter().collect();
            // A `\r` before the newline is not part of the comment.
            let body = comment.trim_end_matches('\r');
            out.push_str(&masked_comment(body));
            out.push_str(&comment[body.len()..]);
        } else if c == '"' || c == '\'' {
            let triple = chars.get(i + 1) == Some(&c) && chars.get(i + 2) == Some(&c);
            let open = if triple { 3 } else { 1 };
            out.extend(&chars[i..i + open]);
            i += open;
            while i < chars.len() {
                let d = chars[i];
                if c == '"' && d == '\\' {
                    out.push(d);
                    if let Some(&next) = chars.get(i + 1) {
                        out.push(next);
                    }
                    i += 2;
                } else if d == c
                    && (!triple || (chars.get(i + 1) == Some(&c) && chars.get(i + 2) == Some(&c)))
                {
                    let close = if triple { 3 } else { 1 };
                    out.extend(&chars[i..i + close]);
                    i += close;
                    break;
                } else if d == '\n' && !triple {
                    // Unterminated: the line ends the string.
                    break;
                } else {
                    out.push(d);
                    i += 1;
                }
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// One comment (starting at its `#`) with credentials masked: by content
/// ([`redact::text`]) and by name, as `devflow env list` judges a variable
/// ([`redact::is_secret_variable`]): in `NAME = value`, `NAME: value` or
/// `"NAME" = "value"` with a secret-like NAME everything after the `=` or `:`
/// is replaced, up to the end of the comment.
fn masked_comment(comment: &str) -> String {
    let chars: Vec<char> = redact::text(comment).chars().collect();
    let is_name = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-');
    let mut out = String::with_capacity(comment.len());
    let mut i = 0;
    while i < chars.len() {
        if !is_name(chars[i]) {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && is_name(chars[i]) {
            i += 1;
        }
        let name: String = chars[start..i].iter().collect();
        out.push_str(&name);
        // The name, an optional closing quote, then `=` or `:`.
        let mut j = i;
        if matches!(chars.get(j), Some('"' | '\'')) {
            j += 1;
        }
        while matches!(chars.get(j), Some(' ' | '\t')) {
            j += 1;
        }
        let assign = match chars.get(j) {
            Some('=') => true,
            Some(':') => chars.get(j + 1) != Some(&'/'),
            _ => false,
        };
        if !assign || !redact::is_secret_variable(&name) {
            continue;
        }
        out.extend(&chars[i..=j]);
        j += 1;
        while matches!(chars.get(j), Some(' ' | '\t')) {
            out.push(chars[j]);
            j += 1;
        }
        // Everything after the assignment is masked: a comment is not TOML,
        // so its quotes (triple, escaped, unbalanced) cannot be trusted to
        // say where the value ends.
        if j < chars.len() {
            out.push_str(MASK);
        }
        break;
    }
    out
}

fn mask_item(item: &mut Item, key: &str) {
    match item {
        Item::Table(table) => {
            for (name, entry) in table.iter_mut() {
                mask_item(entry, &name);
            }
        }
        Item::ArrayOfTables(tables) => {
            for table in tables.iter_mut() {
                for (name, entry) in table.iter_mut() {
                    mask_item(entry, &name);
                }
            }
        }
        Item::Value(value) => mask_value(value, key),
        Item::None => {}
    }
}

fn mask_value(value: &mut Value, key: &str) {
    let masked = match value {
        Value::Array(array) => {
            for element in array.iter_mut() {
                mask_value(element, key);
            }
            return;
        }
        Value::InlineTable(table) => {
            for (name, entry) in table.iter_mut() {
                mask_value(entry, &name);
            }
            return;
        }
        Value::String(text) => masked_template(key, text.value()),
        // A number or a date under a secret name (`PIN = 1234`).
        _ if redact::is_secret_variable(key) => MASK.to_string(),
        _ => return,
    };
    if value.as_str() != Some(masked.as_str()) {
        let decor = value.decor().clone();
        let mut replacement = Value::from(masked);
        *replacement.decor_mut() = decor;
        *value = replacement;
    }
}

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

/// A layer that is not valid TOML: the parser's message with its position
/// (never a value; see [`crate::diag::toml_syntax_error`]) and the file to open.
fn invalid_toml(
    state: &FileState,
    text: &str,
    message: &str,
    span: Option<std::ops::Range<usize>>,
) -> anyhow::Error {
    let error = crate::diag::toml_syntax_error(&file_name(&state.path), text, message, span);
    Problem::new(format!("{error:#}"))
        .with(Remedy::OpenFile {
            path: state.path.clone(),
        })
        .with(Remedy::Retry)
        .into_error()
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
