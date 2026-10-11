//! `devflow env`: list and edit the `[env]` recipes and presets of
//! `devflow.toml` (or `devflow.local.toml`). A recipe is `NAME = "template"`
//! with `${VARIABLE}` references to what devflow generates; a preset is a
//! framework's set of variables. Core edits the files and validates them; this
//! module parses arguments and renders. Changing `[env]` changes what an
//! approval covers: the commands say so and never approve it.
use super::hooks::{Guard, TrustNote, apply, files_json, read_files, say_notes, target_of};
use super::output::{self, Palette, note, say, table};
use super::{Ctx, ProjectInfo};
use anyhow::Result;
use clap::builder::PossibleValuesParser;
use clap::{Args, Subcommand};
use devflow_core::config::EnvPreset;
use devflow_core::config_edit::{
    self, Applied, ConfigFiles, Edit, EnvPreview, PreviewOrigin, PreviewRow, RecipeRow, Target,
};
use devflow_core::remedy::Remedy;
use devflow_core::{Project, redact, trust, worktree};
use serde_json::{Value, json};
use std::process::ExitCode;

#[derive(Subcommand)]
pub enum EnvCommand {
    /// List the recipes and presets, and what they resolve to in a workspace
    List(ListArgs),
    /// Set a recipe: NAME = a value that may reference ${DATABASE_URL} and the like
    Set(SetArgs),
    /// Remove a recipe
    Unset(UnsetArgs),
    /// Add or remove a framework preset
    Preset {
        #[command(subcommand)]
        action: PresetAction,
    },
}

impl EnvCommand {
    pub fn json_name(&self) -> &'static str {
        match self {
            EnvCommand::List(_) => "env list",
            EnvCommand::Set(_) => "env set",
            EnvCommand::Unset(_) => "env unset",
            EnvCommand::Preset { action } => match action {
                PresetAction::Add(_) => "env preset add",
                PresetAction::Remove(_) => "env preset remove",
            },
        }
    }
}

#[derive(Args)]
pub struct ListArgs {
    /// Resolve against this workspace (default: the one you are in, else the default workspace)
    #[arg(long, value_name = "NAME")]
    pub workspace: Option<String>,
    /// Print the real templates and resolved values, credentials included (by default a
    /// credential is shown as `***`)
    #[arg(long)]
    pub show_values: bool,
}

#[derive(Args)]
pub struct SetArgs {
    /// The variable name (letters, digits and `_`)
    pub key: String,
    /// The value. `${DATABASE_URL}`, `${PGHOST}`, … are replaced when `.devflow/env` is written
    pub value: String,
    /// Write to devflow.local.toml (this machine only), where a credential may go
    #[arg(long)]
    pub local: bool,
    #[command(flatten)]
    pub guard: Guard,
}

#[derive(Args)]
pub struct UnsetArgs {
    pub key: String,
    /// Edit devflow.local.toml instead of devflow.toml
    #[arg(long)]
    pub local: bool,
    #[command(flatten)]
    pub guard: Guard,
}

#[derive(Subcommand)]
pub enum PresetAction {
    /// Add a preset
    Add(PresetArgs),
    /// Remove a preset
    Remove(PresetArgs),
}

fn preset_names() -> PossibleValuesParser {
    PossibleValuesParser::new(EnvPreset::ALL.map(EnvPreset::as_str))
}

#[derive(Args)]
pub struct PresetArgs {
    /// The preset
    #[arg(value_parser = preset_names())]
    pub name: String,
    /// Edit devflow.local.toml instead of devflow.toml
    #[arg(long)]
    pub local: bool,
    #[command(flatten)]
    pub guard: Guard,
}

pub fn run(ctx: &Ctx, command: EnvCommand) -> Result<ExitCode> {
    let name = command.json_name();
    match command {
        EnvCommand::List(args) => list(ctx, name, args),
        EnvCommand::Set(args) => {
            let edit = Edit::SetEnv {
                key: args.key.clone(),
                value: args.value,
            };
            let verb = format!("Set env.{}", args.key);
            change(ctx, name, args.local, &args.guard, &edit, &verb)
        }
        EnvCommand::Unset(args) => {
            let edit = Edit::UnsetEnv {
                key: args.key.clone(),
            };
            let verb = format!("Unset env.{}", args.key);
            change(ctx, name, args.local, &args.guard, &edit, &verb)
        }
        EnvCommand::Preset { action } => {
            let (args, add) = match action {
                PresetAction::Add(args) => (args, true),
                PresetAction::Remove(args) => (args, false),
            };
            let preset = EnvPreset::parse(&args.name).expect("validated by clap");
            let (edit, verb) = if add {
                (
                    Edit::AddPreset(preset),
                    format!("Added the {preset} preset"),
                )
            } else {
                (
                    Edit::RemovePreset(preset),
                    format!("Removed the {preset} preset"),
                )
            };
            change(ctx, name, args.local, &args.guard, &edit, &verb)
        }
    }
}

// --- list ------------------------------------------------------------------------

/// A recipe for `env list`. The template is the masked text of the view; only
/// `--show-values` (`reveal`) prints what is written in the file.
fn recipe_json(recipe: &RecipeRow, reveal: bool) -> Value {
    json!({
        "key": recipe.key,
        "template": if reveal { recipe.reveal() } else { recipe.template.as_str() },
        "masked": !reveal && recipe.masked,
        "source": recipe.source,
    })
}

fn preview_json(preview: &EnvPreview, reveal: bool) -> Value {
    json!({
        "variables": preview.rows.iter().map(|row| json!({
            "key": row.key,
            "origin": match &row.origin {
                PreviewOrigin::Recipe => json!("recipe"),
                PreviewOrigin::Preset(preset) => json!(format!("preset:{preset}")),
            },
            "value": if reveal { row.reveal() } else { row.masked.as_str() },
            // Not named `secret`: the structural redaction of every JSON
            // document replaces whatever sits under such a key.
            "masked": !reveal && row.secret,
        })).collect::<Vec<_>>(),
        // The document may be emitted unredacted (`--show-values`): warnings
        // are never values, but they are masked here all the same.
        "warnings": preview.warnings.iter().map(|w| redact::text(w)).collect::<Vec<_>>(),
    })
}

/// A resolved value as `env list` prints it: masked unless `--show-values`.
fn shown(row: &PreviewRow, reveal: bool) -> &str {
    if reveal { row.reveal() } else { &row.masked }
}

fn list(ctx: &Ctx, name: &str, args: ListArgs) -> Result<ExitCode> {
    let files = read_files(ctx)?;
    let view = files.env()?;
    let project = Project::open(&files.root)?;

    // The workspace the values resolve in: asked for, else the one the
    // command runs in, else the default workspace.
    let workspace = match &args.workspace {
        Some(wanted) => Some(worktree::find(&project, wanted)?.ok_or_else(|| {
            anyhow::anyhow!("workspace '{wanted}' does not exist; `devflow list` shows them")
        })?),
        None => worktree::containing(&project, &ctx.cwd)
            .ok()
            .flatten()
            .or_else(|| {
                worktree::list(&project)
                    .ok()?
                    .into_iter()
                    .find(|w| w.is_default)
            }),
    };
    let preview = match &workspace {
        Some(workspace) => config_edit::preview_env(&project, workspace)?,
        None => None,
    };
    let state = trust::state(&project)?;
    let references = config_edit::available_references(&project.config);
    let reveal = args.show_values;

    if ctx.json {
        let body = json!({
            "project": ProjectInfo::of(&project),
            "files": files_json(&files),
            // Whether `recipes[].template` and `preview` hold the real values.
            "revealed": reveal,
            "recipes": view.recipes.iter().map(|r| recipe_json(r, reveal)).collect::<Vec<_>>(),
            "presets": view.presets.iter().map(|p| json!({
                "name": p.preset,
                "description": config_edit::preset_description(p.preset),
                "variables": p.keys,
            })).collect::<Vec<_>>(),
            "presets_source": view.presets_source,
            "shadowed_presets": view.shadowed_presets,
            "available_presets": EnvPreset::ALL.map(EnvPreset::as_str),
            "references": references,
            "preview": preview.as_ref().map(|p| preview_json(p, reveal)),
            "preview_workspace": workspace.as_ref().map(|w| &w.name),
            "trust": state,
            // Valid configuration that is probably not what was meant.
            "warnings": project
                .config
                .env
                .hints()
                .iter()
                .map(|hint| hint.message.as_str())
                .collect::<Vec<_>>(),
        });
        if reveal {
            // The one document that prints credentials on purpose, like
            // `db url`: the structural masking would undo the request.
            output::emit_unredacted(ctx, name, &body)?;
        } else {
            output::emit(ctx, name, &body)?;
        }
        return Ok(ExitCode::SUCCESS);
    }

    let palette = Palette::stdout();
    for hint in project.config.env.hints() {
        note(format!("warning: {}", hint.message));
        let add = Remedy::AddEnvPreset {
            root: project.root.clone(),
            preset: hint.preset.to_string(),
        };
        note(format!("hint: {}", add.hint().unwrap_or_default()));
    }
    if view.presets.is_empty() && view.recipes.is_empty() {
        say("No [env] recipes or presets.");
        say(palette.dim(&format!(
            "Add a recipe with `devflow env set NAME '${{DATABASE_URL}}'`; presets: {}.",
            EnvPreset::ALL.map(EnvPreset::as_str).join(", ")
        )));
        return Ok(ExitCode::SUCCESS);
    }
    if !view.presets.is_empty() {
        say("presets");
        for preset in &view.presets {
            say(format!(
                "  {}  {}",
                preset.preset,
                palette.dim(&preset.keys.join(", "))
            ));
        }
    }
    let rows: Vec<Vec<String>> = view
        .recipes
        .iter()
        .map(|recipe| {
            let resolved = preview
                .as_ref()
                .and_then(|p| p.rows.iter().find(|row| row.key == recipe.key))
                .map(|row| shown(row, reveal).to_string())
                .unwrap_or_else(|| "-".to_string());
            let source = match recipe.source {
                Target::Local => " (local)",
                Target::Project => "",
            };
            let template = if reveal {
                recipe.reveal()
            } else {
                &recipe.template
            };
            vec![
                format!("{}{source}", recipe.key),
                template.to_string(),
                resolved,
            ]
        })
        .collect();
    if !rows.is_empty() {
        say(table(&["RECIPE", "TEMPLATE", "RESOLVED"], &rows));
    }
    match (&preview, &workspace) {
        (Some(preview), Some(workspace)) => {
            for row in preview
                .rows
                .iter()
                .filter(|r| matches!(r.origin, PreviewOrigin::Preset(_)))
            {
                say(format!("  {} = {}", row.key, shown(row, reveal)));
            }
            if reveal {
                note(format!("Resolved in workspace '{}'.", workspace.name));
            } else {
                note(format!(
                    "Resolved in workspace '{}'; credentials are masked (`--show-values` prints \
                     them, `devflow db url` prints the real connection URL).",
                    workspace.name
                ));
            }
            for warning in &preview.warnings {
                note(format!("warning: {}", redact::text(warning)));
            }
        }
        _ => note("Nothing to resolve against: the workspace has no setup receipt yet."),
    }
    if reveal {
        note(
            "--show-values: the real values are printed above and may include credentials; do not \
             share or log this output.",
        );
    } else if view.recipes.iter().any(|r| r.masked) {
        note("A template shown as *** is a credential; --show-values prints it.");
    }
    if matches!(state, trust::TrustState::Untrusted { .. }) {
        note(
            "These [env] values are not approved on this machine yet: run `devflow trust` to \
             review and approve them.",
        );
    }
    Ok(ExitCode::SUCCESS)
}

// --- edits -------------------------------------------------------------------------

fn change(
    ctx: &Ctx,
    name: &str,
    local: bool,
    guard: &Guard,
    edit: &Edit,
    verb: &str,
) -> Result<ExitCode> {
    let files: ConfigFiles = read_files(ctx)?;
    let target = target_of(local);
    let applied: Applied = apply(&files, target, edit, guard)?;
    let trust = TrustNote::after(&files.root, &applied)?;
    let after = ConfigFiles::read(&files.root)?;
    let view = after.env()?;
    let project = Project::open(&files.root)?;
    let file = applied
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if ctx.json {
        let body = json!({
            "project": ProjectInfo::of(&project),
            "file": file,
            "path": applied.path,
            "target": applied.target,
            "changed": applied.changed,
            // The keys only: an edit's response never repeats a value (the one
            // just set may be a credential). `devflow env list` shows them.
            "recipes": view.recipes.iter().map(|r| json!({
                "key": r.key, "source": r.source,
            })).collect::<Vec<_>>(),
            "presets": view.presets.iter().map(|p| p.preset).collect::<Vec<_>>(),
            "notes": applied.notes,
            "trust": trust.json(),
        });
        output::emit(ctx, name, &body)?;
        return Ok(ExitCode::SUCCESS);
    }
    if applied.changed {
        say(format!("{verb} ({file})."));
    } else {
        say(format!("Nothing to change ({file})."));
    }
    say_notes(&applied);
    trust.say();
    Ok(ExitCode::SUCCESS)
}
