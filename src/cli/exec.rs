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

    /// Keep the caller's value of this variable when the workspace sets none
    /// (repeatable; `DEVFLOW_INHERIT=NAME,NAME` says the same). By default
    /// every variable the workspace owns, set or not, is taken out of the
    /// inherited environment
    #[arg(long = "inherit", value_name = "NAME", value_parser = env_name)]
    pub inherit: Vec<String>,

    /// Go on although the environment carries the variables of a workspace
    /// that cannot be resolved (its directory is gone, say): every variable
    /// the workspace owns and every name devflow can generate is taken out
    /// first (`DEVFLOW_CLEAR_INHERITED=1` says the same)
    #[arg(long)]
    pub clear_inherited: bool,

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

/// A variable name for `--inherit`.
fn env_name(name: &str) -> Result<String, String> {
    if devflow_core::config::valid_env_name(name) {
        Ok(name.to_string())
    } else {
        Err(format!(
            "'{name}' is not a variable name (letters, digits and underscores)"
        ))
    }
}

pub fn run(ctx: &Ctx, args: ExecArgs) -> Result<ExitCode> {
    // Find the variables first: the command never starts without them.
    let options = exec::Options {
        inherit: args.inherit.clone(),
        clear_stale: args.clear_inherited,
    };
    let prepared = match &args.workspace {
        Some(name) => {
            let project = ctx.project()?;
            let workspace = worktree::find(&project, name)?
                .ok_or_else(|| anyhow!("no workspace named '{name}'; see `devflow list`"))?;
            exec::prepare_root_with(&workspace.path, &options)?
        }
        None => exec::prepare_with(&ctx.cwd, &options)?,
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
