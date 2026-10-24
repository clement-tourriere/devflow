//! `devflow open`: a workspace in the editor or the terminal.
//!
//! Which program that is comes from `settings.toml` in the configuration
//! directory ([`devflow_core::settings`]), the file the desktop app reads too,
//! so both open the same tools.
use super::output;
use super::{Ctx, ProjectInfo};
use anyhow::{Result, anyhow};
use clap::Args;
use devflow_core::remedy::{Problem, Remedy};
use devflow_core::settings::{self, Tool};
use devflow_core::worktree;
use serde_json::json;
use std::process::{Command, ExitCode, Stdio};

#[derive(Args)]
pub struct OpenArgs {
    /// Workspace name, branch or path (default: the one you are in)
    #[arg(add = super::complete::workspace())]
    pub name: Option<String>,
    /// Open it in the editor (the default)
    #[arg(long, conflicts_with = "terminal")]
    pub editor: bool,
    /// Open it in the terminal
    #[arg(long)]
    pub terminal: bool,
}

pub fn run(ctx: &Ctx, args: OpenArgs) -> Result<ExitCode> {
    let project = ctx.project()?;
    let workspace = match args.name.as_deref() {
        Some(name) => worktree::find(&project, name)?
            .ok_or_else(|| anyhow!("no workspace named '{name}'; see `devflow list`"))?,
        None => worktree::containing(&project, &ctx.cwd)?.ok_or_else(|| {
            anyhow!(
                "{} is not inside a workspace of {}; name one (see `devflow list`)",
                ctx.cwd.display(),
                project.name()
            )
        })?,
    };
    anyhow::ensure!(
        !workspace.missing,
        "the directory of workspace '{}' is missing: {}",
        workspace.name,
        workspace.path.display()
    );
    let tool = if args.terminal {
        Tool::Terminal
    } else {
        Tool::Editor
    };
    let settings = settings::load()?;
    let command = settings.command(tool);
    let argv = settings::command_line(command, &workspace.path)?;
    let mut launch = Command::new(&argv[0]);
    launch.args(&argv[1..]).current_dir(&workspace.path);
    if ctx.json {
        // Nobody is at the terminal to hand it to: start it and report.
        launch
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        launch
            .spawn()
            .map_err(|error| cannot_run(&argv[0], tool, &error))?;
        let body = json!({
            "project": ProjectInfo::of(&project),
            "workspace": workspace.name,
            "path": workspace.path,
            "tool": tool,
            "argv": argv,
        });
        output::emit(ctx, "open", &body)?;
        return Ok(ExitCode::SUCCESS);
    }
    output::note(format!(
        "Opening {} in the {}",
        workspace.name,
        tool.label()
    ));
    // A terminal editor (vim) needs the terminal: on Unix this process is replaced.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(cannot_run(&argv[0], tool, &launch.exec()))
    }
    #[cfg(not(unix))]
    {
        let status = launch
            .status()
            .map_err(|error| cannot_run(&argv[0], tool, &error))?;
        Ok(ExitCode::from(
            status.code().unwrap_or(1).clamp(0, 255) as u8
        ))
    }
}

fn cannot_run(program: &str, tool: Tool, error: &std::io::Error) -> anyhow::Error {
    let mut problem = Problem::new(format!(
        "cannot run `{program}` for the {}: {error}",
        tool.label()
    ));
    if let Ok(path) = settings::path() {
        problem = problem.with(Remedy::OpenFile { path });
    }
    problem.into_error()
}
