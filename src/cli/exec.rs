//! `devflow exec`: run a command with the workspace's `.devflow/env` applied
//! last. The behavior (finding the workspace, what it sets, never starting a
//! command against an unknown database) is `devflow_core::exec`; this module
//! parses the arguments and turns "could not start" into an exit status.
use super::Ctx;
use anyhow::{Result, anyhow};
use clap::Args;
use devflow_core::exec;
use devflow_core::worktree;
use std::ffi::OsString;
use std::process::ExitCode;

#[derive(Args)]
pub struct ExecArgs {
    /// Use this workspace's variables (name, branch or path) instead of the
    /// one containing the current directory
    #[arg(long, value_name = "NAME")]
    pub workspace: Option<String>,

    /// The command and its arguments, after `--`
    #[arg(
        last = true,
        required = true,
        num_args = 1..,
        value_name = "CMD",
        allow_hyphen_values = true
    )]
    pub command: Vec<OsString>,
}

pub fn run(ctx: &Ctx, args: ExecArgs) -> Result<ExitCode> {
    // Find the variables first: the command never starts without them.
    let prepared = match &args.workspace {
        Some(name) => {
            let project = ctx.project()?;
            let workspace = worktree::find(&project, name)?
                .ok_or_else(|| anyhow!("no workspace named '{name}'; see `devflow list`"))?;
            exec::prepare_root(&workspace.path)?
        }
        None => exec::prepare(&ctx.cwd)?,
    };
    let Some((program, rest)) = args.command.split_first() else {
        return Err(anyhow!("a command is required after `--`"));
    };
    // With `-C DIR` the command runs in DIR; otherwise where it was started,
    // without the canonical path ctx.cwd carries.
    let dir = ctx.chdir.then_some(ctx.cwd.as_path());
    match exec::run(&prepared, program, rest, dir) {
        // Elsewhere than Unix the command ran as a child: mirror its status.
        Ok(code) => Ok(ExitCode::from(code)),
        Err(failed) => {
            super::output::note(format!("devflow exec: {}", failed.reason));
            Ok(ExitCode::from(failed.code))
        }
    }
}
