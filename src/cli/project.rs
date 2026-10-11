//! `devflow project`: mend the list of known projects. Core decides (the
//! registry checks that a folder is the project it is asked to be); this module
//! parses arguments and renders. Nothing here touches a project's folder or data.
use super::{Ctx, output};
use anyhow::Result;
use clap::{Args, Subcommand};
use devflow_core::project::main_checkout;
use devflow_core::registry::{self, Entry};
use serde::Serialize;
use serde_json::json;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Subcommand)]
pub enum ProjectCommand {
    /// Remove a project from the list of known projects (its folder and data are not touched)
    Forget(ForgetArgs),
    /// Point a listed project at the folder it moved to
    Relocate(RelocateArgs),
}

impl ProjectCommand {
    pub fn json_name(&self) -> &'static str {
        match self {
            ProjectCommand::Forget(_) => "project forget",
            ProjectCommand::Relocate(_) => "project relocate",
        }
    }
}

#[derive(Args)]
pub struct ForgetArgs {
    /// The project's folder or id, as `devflow list --all --json` shows them
    /// (default: the project containing the current directory)
    #[arg(value_name = "PROJECT", conflicts_with = "missing")]
    pub project: Option<String>,
    /// Forget every listed project whose folder was not found (moved, deleted,
    /// or on a drive that is not connected); lists them first and asks
    #[arg(long)]
    pub missing: bool,
    /// Do not ask before forgetting (with --missing)
    #[arg(long, short = 'y', requires = "missing")]
    pub yes: bool,
}

#[derive(Args)]
pub struct RelocateArgs {
    /// The listed project: its (old) folder or its id
    #[arg(value_name = "PROJECT")]
    pub project: String,
    /// The folder it is in now (any folder of the repository)
    #[arg(value_name = "NEW_PATH", value_hint = clap::ValueHint::DirPath)]
    pub new_path: PathBuf,
}

#[derive(Serialize)]
struct Listed<'a> {
    name: &'a str,
    id: uuid::Uuid,
    root: &'a std::path::Path,
}

impl<'a> Listed<'a> {
    fn of(entry: &'a Entry) -> Self {
        Self {
            name: &entry.name,
            id: entry.id,
            root: &entry.root,
        }
    }
}

pub fn run(ctx: &Ctx, command: ProjectCommand) -> Result<ExitCode> {
    let name = command.json_name();
    match command {
        ProjectCommand::Forget(args) => forget(ctx, name, args),
        ProjectCommand::Relocate(args) => relocate(ctx, name, args),
    }
}

fn forget(ctx: &Ctx, name: &str, args: ForgetArgs) -> Result<ExitCode> {
    let removed: Vec<Entry> = if args.missing {
        let candidates = registry::unavailable()?;
        if candidates.is_empty() {
            Vec::new()
        } else {
            if !ctx.json {
                output::say(
                    "These listed projects have a folder that was not found (moved, deleted, or on a drive that is not connected):",
                );
                for entry in &candidates {
                    output::say(format!("  {} ({})", entry.name, output::tilde(&entry.root)));
                }
            }
            // A person at a terminal confirms; `--json` and scripts do not prompt.
            if ctx.interactive() && !args.yes && !confirm("Remove them from the list?")? {
                output::note("Nothing was removed.");
                return Ok(ExitCode::SUCCESS);
            }
            // Only what was listed is removed, and only while it is still not found.
            registry::forget_unavailable(&candidates)?
        }
    } else {
        let entry = match &args.project {
            Some(selector) => registry::find(selector)?,
            None => {
                let root = main_checkout(&ctx.cwd)?;
                registry::find(&root.display().to_string())?
            }
        };
        registry::remove(&entry.root)?;
        vec![entry]
    };
    if ctx.json {
        let listed: Vec<Listed> = removed.iter().map(Listed::of).collect();
        output::emit(ctx, name, &json!({ "forgotten": listed }))?;
        return Ok(ExitCode::SUCCESS);
    }
    if removed.is_empty() {
        output::say("No listed project has a folder that was not found.");
    }
    for entry in &removed {
        output::say(format!(
            "Removed '{}' ({}) from the list. Its folder and data were not touched.",
            entry.name,
            output::tilde(&entry.root)
        ));
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

fn relocate(ctx: &Ctx, name: &str, args: RelocateArgs) -> Result<ExitCode> {
    let entry = registry::find(&args.project)?;
    let moved = registry::relocate(entry.id, &args.new_path)?;
    if ctx.json {
        output::emit(
            ctx,
            name,
            &json!({
                "project": Listed::of(&moved),
                "previous_root": entry.root,
            }),
        )?;
        return Ok(ExitCode::SUCCESS);
    }
    output::say(format!(
        "'{}' is now at {} (it was {}).",
        moved.name,
        output::tilde(&moved.root),
        output::tilde(&entry.root)
    ));
    Ok(ExitCode::SUCCESS)
}
