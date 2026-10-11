//! Edits of the `[env]` table (recipes and presets), the checks every edit
//! goes through before it is applied, and the preview of what the recipes
//! produce in a workspace.
use super::text::{Context, Slots};
use super::{Edit, Target};
use crate::config::{Config, EnvPreset, valid_env_name};
use crate::env_file;
use crate::project::Project;
use crate::worktree::Workspace;
use crate::{conn, redact, setup};
use anyhow::{Result, bail};
use toml_edit::{Array, DocumentMut, Item, Table, TableLike, Value, value};

/// A short name for a person: `Spring Boot`.
pub fn preset_label(preset: EnvPreset) -> &'static str {
    match preset {
        EnvPreset::Prisma => "Prisma",
        EnvPreset::Laravel => "Laravel",
        EnvPreset::Spring => "Spring Boot",
        EnvPreset::Sqlalchemy => "SQLAlchemy",
        EnvPreset::DbParts => "Plain DB_* parts",
    }
}

/// What the preset sets, in a sentence.
pub fn preset_description(preset: EnvPreset) -> &'static str {
    match preset {
        EnvPreset::Prisma => {
            "DIRECT_URL, the same URL as DATABASE_URL, for Prisma migrations. SHADOW_DATABASE_URL \
             comes with a `shadow` scratch database."
        }
        EnvPreset::Laravel => {
            "DB_CONNECTION, DB_HOST, DB_PORT, DB_DATABASE, DB_USERNAME and DB_PASSWORD, as Laravel \
             reads them."
        }
        EnvPreset::Spring => {
            "SPRING_DATASOURCE_URL (JDBC), SPRING_DATASOURCE_USERNAME and \
             SPRING_DATASOURCE_PASSWORD for Spring Boot."
        }
        EnvPreset::Sqlalchemy => {
            "SQLALCHEMY_DATABASE_URI with the postgresql+psycopg:// driver, for SQLAlchemy and Flask."
        }
        EnvPreset::DbParts => {
            "DB_HOST, DB_PORT, DB_NAME, DB_USER and DB_PASSWORD, for tools that want the \
             connection in parts."
        }
    }
}

/// The variables a recipe can reference as `${NAME}` in this project: the
/// ones devflow generates for a workspace, and the URLs of the scratch
/// databases the configuration lists.
pub fn available_references(config: &Config) -> Vec<String> {
    let mut names: Vec<String> = env_file::GENERATED
        .iter()
        .map(|n| (*n).to_string())
        .collect();
    names.extend(conn::pg_env_names().map(str::to_string));
    if let Some((_, pg)) = config.postgres() {
        names.extend(pg.extra_database_vars());
    }
    names.sort();
    names.dedup();
    names
}

// -- the preview ---------------------------------------------------------------------

/// Where a previewed variable comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewOrigin {
    Recipe,
    Preset(EnvPreset),
}

/// One variable of the `[env]` section as it resolves in a workspace.
#[derive(Clone, PartialEq, Eq)]
pub struct PreviewRow {
    pub key: String,
    pub origin: PreviewOrigin,
    /// The value with credentials replaced by `***`.
    pub masked: String,
    /// The value is (or contains) a credential.
    pub secret: bool,
    raw: String,
}

impl PreviewRow {
    /// The real value, for a person who asked to see it. Never log it.
    pub fn reveal(&self) -> &str {
        &self.raw
    }
}

impl std::fmt::Debug for PreviewRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreviewRow")
            .field("key", &self.key)
            .field("origin", &self.origin)
            .field("masked", &self.masked)
            .field("secret", &self.secret)
            .finish_non_exhaustive()
    }
}

/// What the `[env]` section resolves to in one workspace.
#[derive(Debug, Clone, Default)]
pub struct EnvPreview {
    pub rows: Vec<PreviewRow>,
    /// Recipes and presets left out, and why.
    pub warnings: Vec<String>,
}

/// Resolve the project's `[env]` section against the variables devflow
/// generated for `workspace` (read from its receipt; nothing is written and
/// no server is contacted). `None` when the workspace has no receipt yet.
pub fn preview_env(project: &Project, workspace: &Workspace) -> Result<Option<EnvPreview>> {
    let Some(receipt) = setup::receipt(project, workspace)? else {
        return Ok(None);
    };
    let generated = setup::env_vars(project, workspace, &receipt).len();
    let full = setup::workspace_env(project, workspace, &receipt, true);
    let secrets: Vec<&str> = full.secrets.iter().map(String::as_str).collect();
    let rows = full
        .vars
        .iter()
        .skip(generated)
        .map(|(key, raw)| {
            let masked = super::masked_value(key, raw, &secrets);
            let origin = if project.config.env.vars.contains_key(key) {
                PreviewOrigin::Recipe
            } else {
                project
                    .config
                    .env
                    .presets
                    .iter()
                    .find(|p| p.keys().contains(&key.as_str()))
                    .map_or(PreviewOrigin::Recipe, |p| PreviewOrigin::Preset(*p))
            };
            PreviewRow {
                key: key.clone(),
                origin,
                secret: masked != *raw,
                masked,
                raw: raw.clone(),
            }
        })
        .collect();
    Ok(Some(EnvPreview {
        rows,
        warnings: full.warnings,
    }))
}

// -- checks --------------------------------------------------------------------------

impl Edit {
    /// What can be refused before any file is read. Messages name the key
    /// path, never the value.
    pub(super) fn check_input(&self, target: Target) -> Result<()> {
        match self {
            Edit::AddHook { phase, entry, .. } | Edit::UpdateHook { phase, entry, .. } => {
                if target == Target::Project && looks_secret(&entry.run) {
                    bail!(
                        "hooks.{}: the command looks like it holds a credential; devflow.toml is \
                         committed, so put it in devflow.local.toml (--local) or read it from the \
                         environment",
                        phase.as_str()
                    );
                }
            }
            Edit::RemoveHook { .. } | Edit::MoveHook { .. } => {}
            Edit::SetHooksTimeout(_) | Edit::UnsetEnv { .. } => {}
            Edit::SetEnv { key, value } => {
                if key == "presets" {
                    bail!("env.presets is the list of presets, not a variable; use a preset edit");
                }
                if !valid_env_name(key) {
                    bail!(
                        "env: the variable name is not valid (letters, digits and `_`, not \
                         starting with a digit)"
                    );
                }
                let segments = env_file::parse_template(value).map_err(|_| {
                    anyhow::anyhow!(
                        "env.{key}: the value has a malformed `${{…}}` reference (write \
                         `${{NAME}}` with a plain variable name, or `$${{` for a literal `${{`)"
                    )
                })?;
                let literal = segments
                    .iter()
                    .any(|s| matches!(s, env_file::Segment::Text(t) if !t.is_empty()));
                if target == Target::Project
                    && (looks_secret(value) || (redact::is_secret_key(key) && literal))
                {
                    bail!(
                        "env.{key}: the value looks like it holds a credential; devflow.toml is \
                         committed, so reference a variable devflow generates (`${{DATABASE_URL}}`) \
                         or put it in devflow.local.toml (--local)"
                    );
                }
            }
            Edit::AddPreset(_) | Edit::RemovePreset(_) => {}
        }
        Ok(())
    }

    pub(super) fn apply_to(&self, doc: &mut DocumentMut, ctx: &mut Context<'_>) -> Result<()> {
        if self.apply_hooks_edit(doc, ctx)? {
            return Ok(());
        }
        self.apply_env_edit(doc, ctx)
    }

    fn apply_env_edit(&self, doc: &mut DocumentMut, ctx: &mut Context<'_>) -> Result<()> {
        match self {
            Edit::SetEnv { key, value: text } => {
                let table = env_table(doc)?;
                match table.get_mut(key) {
                    Some(item) => {
                        if !item.is_str() {
                            bail!("env.{key}: not a string");
                        }
                        let decor = item.as_value().map(|v| v.decor().clone());
                        let mut new = Value::from(text.as_str());
                        if let Some(decor) = decor {
                            *new.decor_mut() = decor;
                        }
                        *item = Item::Value(new);
                    }
                    None => {
                        table.insert(key, value(text.as_str()));
                    }
                }
                if ctx.target == Target::Project
                    && ctx.local_doc().is_some_and(|local| env_has(&local, key))
                {
                    ctx.note(format!(
                        "{} sets env.{key} too, and wins on this machine",
                        super::LOCAL_CONFIG_FILE
                    ));
                }
            }
            Edit::UnsetEnv { key } => {
                let table = env_table_existing(doc);
                let removed = table.and_then(|t| t.remove(key)).is_some();
                if !removed {
                    let committed = ctx.target == Target::Local
                        && ctx
                            .project_doc()
                            .is_some_and(|project| env_has(project, key));
                    if committed {
                        bail!(
                            "env.{key} is set in {}, not here: a local file cannot remove a \
                             committed variable; unset it there",
                            ctx.project_name()
                        );
                    }
                    bail!("env.{key} is not set in {}", ctx.file_name());
                }
                if ctx.target == Target::Project
                    && ctx.local_doc().is_some_and(|local| env_has(&local, key))
                {
                    ctx.note(format!(
                        "{} still sets env.{key} on this machine",
                        super::LOCAL_CONFIG_FILE
                    ));
                }
                // The local recipe overrode a committed one: unsetting it
                // brings the committed value back.
                if ctx.target == Target::Local
                    && ctx
                        .project_doc()
                        .is_some_and(|project| env_has(project, key))
                {
                    let project = ctx.project_name();
                    ctx.note(format!(
                        "{project} sets env.{key} too, and its value applies on this machine again"
                    ));
                }
            }
            Edit::AddPreset(preset) => {
                seed_presets(doc, ctx)?;
                note_presets_override(ctx);
                let array = presets_array(doc, true)?.expect("created");
                let mut slots = Slots::read(array);
                let name = preset.as_str();
                if slots.entries.iter().any(|s| s.value.as_str() == Some(name)) {
                    return Ok(());
                }
                let slot = slots.new_slot(Value::from(name));
                slots.entries.push(slot);
                slots.write(array, false);
            }
            Edit::RemovePreset(preset) => {
                seed_presets(doc, ctx)?;
                note_presets_override(ctx);
                let name = preset.as_str();
                let Some(array) = presets_array(doc, false)? else {
                    bail!("env.presets does not list \"{name}\"");
                };
                let mut slots = Slots::read(array);
                let Some(at) = slots
                    .entries
                    .iter()
                    .position(|s| s.value.as_str() == Some(name))
                else {
                    bail!("env.presets does not list \"{name}\"");
                };
                slots.entries.remove(at);
                let empty = slots.entries.is_empty();
                slots.write(array, false);
                if empty {
                    // An empty list is no list in the committed file. In the
                    // local one it is the only way to say "none here": a
                    // local file without `presets` inherits the committed
                    // list, so dropping the key would switch them all back on.
                    let hides_committed = ctx.target == Target::Local
                        && committed_presets(ctx).is_some_and(|names| !names.is_empty());
                    if hides_committed {
                        let project = ctx.project_name();
                        ctx.note(format!(
                            "{} keeps env.presets = [] so that the presets of {project} stay off \
                             on this machine: without the key they would apply again",
                            ctx.file_name()
                        ));
                    } else {
                        env_table(doc)?.remove("presets");
                    }
                }
            }
            _ => unreachable!("hook edits are handled first"),
        }
        Ok(())
    }
}

/// Whether text looks like it carries a credential (a password in a URL, a
/// `password=…` assignment, a token).
fn looks_secret(text: &str) -> bool {
    redact::text(text) != text
}

fn env_has(doc: &DocumentMut, key: &str) -> bool {
    doc.get("env")
        .and_then(Item::as_table_like)
        .is_some_and(|t| t.contains_key(key))
}

/// `[env]`, created when the file has none.
fn env_table(doc: &mut DocumentMut) -> Result<&mut dyn TableLike> {
    let root = doc.as_table_mut();
    if !root.contains_key("env") {
        root.insert("env", Item::Table(Table::new()));
    }
    match root.get_mut("env").and_then(Item::as_table_like_mut) {
        Some(table) => Ok(table),
        None => bail!("env: not a table"),
    }
}

/// `[env]` if the file has it.
fn env_table_existing(doc: &mut DocumentMut) -> Option<&mut dyn TableLike> {
    doc.as_table_mut()
        .get_mut("env")
        .and_then(Item::as_table_like_mut)
}

fn presets_array(doc: &mut DocumentMut, create: bool) -> Result<Option<&mut Array>> {
    if !create && !env_has(doc, "presets") {
        return Ok(None);
    }
    let table = env_table(doc)?;
    if !table.contains_key("presets") {
        table.insert("presets", Item::Value(Value::Array(Array::new())));
    }
    match table.get_mut("presets") {
        Some(Item::Value(Value::Array(array))) => Ok(Some(array)),
        _ => bail!("env.presets: not a list"),
    }
}

/// The `env.presets` list of the committed file, `None` when it has none.
fn committed_presets(ctx: &mut Context<'_>) -> Option<Vec<String>> {
    ctx.project_doc().and_then(|project| {
        let array = project
            .get("env")?
            .as_table_like()?
            .get("presets")?
            .as_array()?;
        Some(
            array
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
        )
    })
}

/// Local file only: a `presets` list replaces the committed one whole, so a
/// local list starts as a copy of it.
fn seed_presets(doc: &mut DocumentMut, ctx: &mut Context<'_>) -> Result<()> {
    if ctx.target != Target::Local || env_has(doc, "presets") {
        return Ok(());
    }
    // A fresh list: the comments of the committed file stay there.
    let Some(names) = committed_presets(ctx) else {
        return Ok(());
    };
    let mut array = Array::new();
    let mut slots = Slots::read(&array);
    for name in &names {
        let slot = slots.new_slot(Value::from(name.as_str()));
        slots.entries.push(slot);
    }
    slots.write(&mut array, false);
    env_table(doc)?.insert("presets", Item::Value(Value::Array(array)));
    let project = ctx.project_name();
    ctx.note(format!(
        "{} now holds the whole env.presets list, copied from {project}: a list in the local \
         file replaces the committed one on this machine",
        ctx.file_name()
    ));
    Ok(())
}

fn note_presets_override(ctx: &mut Context<'_>) {
    if ctx.target == Target::Project && ctx.local_doc().is_some_and(|l| env_has(&l, "presets")) {
        ctx.note(format!(
            "{} sets env.presets too, and its list replaces this one on this machine: the change \
             has no effect here until that override is removed",
            super::LOCAL_CONFIG_FILE
        ));
    }
}
