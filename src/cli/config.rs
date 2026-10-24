//! `devflow config`: bring a project's configuration files to the current
//! format, and print their JSON Schema. Core works out and writes the change
//! (`devflow_core::config_edit::migrate`); this module shows it and asks. The
//! schema is generated from the configuration types
//! (`devflow_core::config_schema`).
use super::Ctx;
use super::output::{self, Palette, note, say, say_raw};
use anyhow::Result;
use clap::{Args, Subcommand};
use devflow_core::config_edit::migrate;
use devflow_core::config_schema::{self, Document};
use devflow_core::project::main_checkout;
use devflow_core::propose::DiffKind;
use devflow_core::redact;
use serde_json::json;
use std::process::ExitCode;

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Rewrite an earlier build's `[postgres]` table as `[services.db]`
    Migrate(MigrateArgs),
    /// Print the JSON Schema of devflow.toml (for editors) on standard output
    Schema(SchemaArgs),
}

impl ConfigCommand {
    pub fn json_name(&self) -> &'static str {
        match self {
            ConfigCommand::Migrate(_) => "config migrate",
            ConfigCommand::Schema(_) => "config schema",
        }
    }
}

#[derive(Args)]
pub struct SchemaArgs {
    /// Print the schema of devflow.local.toml, the machine-local overlay, instead
    #[arg(long)]
    pub local: bool,
}

#[derive(Args)]
pub struct MigrateArgs {
    /// Show the change and write nothing
    #[arg(long)]
    pub dry_run: bool,
    /// Do not ask before writing
    #[arg(long, short = 'y')]
    pub yes: bool,
}

pub fn run(ctx: &Ctx, command: ConfigCommand) -> Result<ExitCode> {
    let name = command.json_name();
    match command {
        ConfigCommand::Migrate(args) => migrate_config(ctx, name, args),
        ConfigCommand::Schema(args) => print_schema(&args),
    }
}

/// The schema is JSON already, so it is printed as it is with or without
/// `--json`: no envelope, no progress, nothing else on standard output.
fn print_schema(args: &SchemaArgs) -> Result<ExitCode> {
    let document = if args.local {
        Document::Local
    } else {
        Document::Project
    };
    say_raw(config_schema::render(document)?);
    Ok(ExitCode::SUCCESS)
}

fn migrate_config(ctx: &Ctx, name: &str, args: MigrateArgs) -> Result<ExitCode> {
    // The project cannot be opened (that is the point): find its checkout only.
    let root = main_checkout(&ctx.cwd)?;
    if !migrate::needed(&root)? {
        if ctx.json {
            output::emit(
                ctx,
                name,
                &json!({ "migrated": false, "dry_run": args.dry_run, "files": [] }),
            )?;
        } else {
            say("Nothing to migrate: no configuration file uses the top-level [postgres] table.");
        }
        return Ok(ExitCode::SUCCESS);
    }
    let migration = migrate::plan(&root)?;
    let palette = Palette::stdout();
    if !ctx.json {
        for file in &migration.files {
            say(format!("{}:", file.preview.file));
            for line in &file.preview.diff {
                let text = redact::text(&line.text);
                say(match line.kind {
                    DiffKind::Added => palette.green(&format!("+ {text}")),
                    DiffKind::Removed => palette.red(&format!("- {text}")),
                    DiffKind::Context => palette.dim(&format!("  {text}")),
                    DiffKind::Gap => palette.dim("  …"),
                });
            }
        }
        for line in &migration.notes {
            note(line);
        }
    }
    let mut write = !args.dry_run;
    if write && ctx.interactive() && !args.yes {
        write = confirm("Write these changes?")?;
    }
    if write {
        migrate::apply(&migration)?;
    }
    if ctx.json {
        let files: Vec<_> = migration
            .files
            .iter()
            .map(|file| {
                json!({
                    "file": file.preview.file,
                    "path": file.path,
                    "added": file.preview.added,
                    "removed": file.preview.removed,
                    "diff": file.preview.diff,
                })
            })
            .collect();
        output::emit(
            ctx,
            name,
            &json!({
                "migrated": write,
                "dry_run": args.dry_run,
                "service_key": migration.service_key,
                "files": files,
                "notes": migration.notes,
            }),
        )?;
    } else if write {
        say(format!(
            "Migrated {} file(s). Run `devflow doctor` to check the project.",
            migration.files.len()
        ));
    } else {
        note("Nothing was written.");
    }
    Ok(ExitCode::SUCCESS)
}

fn confirm(prompt: &str) -> Result<bool> {
    match inquire::Confirm::new(prompt).with_default(false).prompt() {
        Ok(answer) => Ok(answer),
        Err(
            inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted,
        ) => anyhow::bail!("cancelled"),
        Err(error) => Err(error.into()),
    }
}
