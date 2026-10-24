//! `devflow focus`: one stable loopback address for the workspace in use. Core
//! ([`devflow_core::focus`]) owns the forwarder process and its state; this
//! module parses the ports, resolves the workspace and renders the state.
use super::output::{self, Palette, note, say};
use super::{Ctx, ProjectInfo};
use anyhow::Result;
use clap::Args;
use devflow_core::focus::{self, PortSpec, Status, Target};
use devflow_core::redact;
use serde_json::json;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Args)]
pub struct FocusArgs {
    /// Workspace name, branch or path to point focus at; without one, show the focus
    #[arg(add = super::complete::workspace(), conflicts_with = "stop")]
    pub name: Option<String>,
    /// A canonical port to forward to the workspace's port: PORT (its app port,
    /// DEVFLOW_PORT) or PORT=DAEMON (that daemon's port); repeatable. Default:
    /// the ports forwarded now, else [worktree] focus_ports
    #[arg(
        long = "port",
        value_name = "PORT[=DAEMON]",
        value_parser = PortSpec::parse,
        requires = "name"
    )]
    pub ports: Vec<PortSpec>,
    /// End the forwarder
    #[arg(long)]
    pub stop: bool,
}

/// `devflow __focus DIR`: the forwarder itself, started by `focus`.
#[derive(Args)]
pub struct ServeArgs {
    pub dir: PathBuf,
}

pub fn serve(args: ServeArgs) -> Result<ExitCode> {
    focus::serve(&args.dir)?;
    Ok(ExitCode::SUCCESS)
}

pub fn run(ctx: &Ctx, args: FocusArgs) -> Result<ExitCode> {
    let project = ctx.project()?;
    let status = if args.stop {
        focus::stop(&project)?
    } else if let Some(name) = args.name.as_deref() {
        let workspace = super::processes::target(ctx, &project, Some(name))?;
        ctx.target_is(&workspace.path);
        focus::focus(&project, &workspace, &args.ports)?
    } else {
        focus::status(&project)?
    };
    if ctx.json {
        let body = json!({ "project": ProjectInfo::of(&project), "focus": status });
        output::emit(ctx, "focus", &body)?;
    } else {
        for line in status_lines(&status, Palette::stdout()) {
            say(line);
        }
        if args.name.is_some() && status.running {
            note(
                "Only TCP is forwarded: a PostgreSQL database name cannot be re-pointed this way.",
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn status_lines(status: &Status, palette: Palette) -> Vec<String> {
    if !status.running {
        return match &status.ended {
            Some(reason) => vec![format!(
                "Focus is off: it ended by itself, {}.",
                redact::text(reason)
            )],
            None => vec!["Focus is off.".to_string()],
        };
    }
    let mut lines = Vec::new();
    match &status.workspace {
        Some(workspace) => lines.push(format!(
            "{} focus is on {} ({})",
            palette.green("✓"),
            workspace.name,
            output::tilde(&workspace.path)
        )),
        None => lines.push(format!("{} focus is running", palette.green("✓"))),
    }
    for forward in &status.forwards {
        let target = match &forward.target {
            Target::App => "app port".to_string(),
            Target::Daemon { name } => format!("daemon {name}"),
        };
        lines.push(format!(
            "  127.0.0.1:{} → 127.0.0.1:{}  ({target}; {})",
            forward.listen,
            forward.to,
            match forward.reachable {
                Some(true) => "listening".to_string(),
                Some(false) => palette.yellow("nothing listens there yet"),
                None => "not tried".to_string(),
            }
        ));
    }
    if let Some(failure) = &status.failure {
        lines.push(format!(
            "{} the last change was not applied: {}",
            palette.yellow("!"),
            redact::text(failure)
        ));
    }
    lines.push(format!("  log: {}", output::tilde(&status.log)));
    lines
}
