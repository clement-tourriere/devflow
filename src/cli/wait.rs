//! `devflow wait`, and the rendering `setup --wait` and `exec --wait` share:
//! the waiting itself is `devflow_core::wait`.
use super::output::{self, Palette, note};
use super::{Ctx, EXIT_ATTENTION, ProjectInfo};
use anyhow::{Result, anyhow};
use clap::Args;
use devflow_core::receipt::{Phase, PhaseState};
use devflow_core::remedy::{self, Remedy};
use devflow_core::wait::{self, Settled, Waited};
use devflow_core::worktree::{self, Workspace};
use devflow_core::{Project, redact};
use serde_json::json;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

#[derive(Args)]
pub struct WaitArgs {
    /// The workspace to wait for: a directory inside it, or its name (default: the current directory)
    pub path: Option<PathBuf>,
    /// Give up waiting after this long (for example 90s or 10m); the status then says it is still running
    #[arg(
        long,
        value_name = "DURATION",
        value_parser = wait::parse_timeout,
        default_value = "15m"
    )]
    pub timeout: Duration,
}

pub fn run(ctx: &Ctx, args: WaitArgs) -> Result<ExitCode> {
    let project = ctx.project()?;
    let workspace = target(ctx, &project, args.path.as_deref())?;
    let waited = wait_for(ctx, &project, &workspace, args.timeout)?;
    report(ctx, "wait", &project, &waited)
}

/// The workspace a path or a name stands for.
fn target(ctx: &Ctx, project: &Project, given: Option<&std::path::Path>) -> Result<Workspace> {
    let Some(given) = given else {
        return containing(ctx, project);
    };
    let path = if given.is_absolute() {
        given.to_path_buf()
    } else {
        ctx.cwd.join(given)
    };
    if path.is_dir() {
        let path = path.canonicalize().unwrap_or(path);
        return worktree::containing(project, &path)?.ok_or_else(|| {
            anyhow!(
                "{} is not inside a workspace of {}",
                path.display(),
                project.name()
            )
        });
    }
    let name = given.to_string_lossy();
    worktree::find(project, &name)?.ok_or_else(|| {
        anyhow!(
            "no workspace named '{name}' and no directory at {}",
            path.display()
        )
    })
}

/// The workspace containing the caller's directory.
pub fn containing(ctx: &Ctx, project: &Project) -> Result<Workspace> {
    worktree::containing(project, &ctx.cwd)?.ok_or_else(|| {
        anyhow!(
            "{} is not inside a workspace of {}",
            ctx.cwd.display(),
            project.name()
        )
    })
}

/// Wait, saying once (on standard error) that a run is being waited for.
pub fn wait_for(
    ctx: &Ctx,
    project: &Project,
    workspace: &Workspace,
    timeout: Duration,
) -> Result<Waited> {
    let mut said = false;
    let name = workspace.name.clone();
    let json = ctx.json;
    wait::until_settled(project, workspace, timeout, |elapsed| {
        // Only once there is something to say: a workspace that is already
        // settled answers on the first look.
        if !said && !json && elapsed >= Duration::from_millis(300) {
            said = true;
            note(format!(
                "{} waiting for {name} to finish setting up (up to {})",
                Palette::stderr().dim("…"),
                wait::format_duration(timeout)
            ));
        }
    })
}

/// Print how a wait ended and map it to the exit status: 0 ready, 2 needs
/// attention or still running at the deadline.
pub fn report(ctx: &Ctx, command: &str, project: &Project, waited: &Waited) -> Result<ExitCode> {
    let workspace = &waited.workspace;
    let remedies = remedies(waited);
    if ctx.json {
        let failed: Vec<_> = waited
            .receipt
            .iter()
            .flat_map(|receipt| {
                Phase::ALL.into_iter().filter_map(|phase| {
                    let record = receipt.phase(phase);
                    (record.state == PhaseState::Failed
                        || (record.state == PhaseState::Skipped && record.required_skip))
                        .then(|| {
                            json!({
                                "phase": phase.as_str(),
                                "state": record.state,
                                "detail": record.detail.as_deref().map(redact::text),
                            })
                        })
                })
            })
            .collect();
        let body = json!({
            "project": ProjectInfo::of(project),
            "workspace": workspace,
            "path": workspace.path,
            "status": waited.settled.status(),
            "settled": waited.settled,
            "waited_ms": waited.waited_ms,
            "timeout_ms": waited.timeout_ms,
            "problems": failed,
            "detail": waited.describe(),
            "remedies": remedy::describe(&remedies),
        });
        output::emit(ctx, command, &body)?;
    } else {
        let palette = Palette::stderr();
        match waited.settled {
            Settled::Ready => note(format!(
                "{} {} is ready",
                palette.green("✓"),
                workspace.name
            )),
            _ => {
                if let Some(message) = waited.describe() {
                    note(format!("{} {message}", palette.yellow("!")));
                }
                if let Some(receipt) = &waited.receipt {
                    for phase in Phase::ALL {
                        let record = receipt.phase(phase);
                        let skipped = record.state == PhaseState::Skipped && record.required_skip;
                        if record.state == PhaseState::Failed || skipped {
                            note(format!(
                                "  {}: {}",
                                phase.as_str(),
                                redact::text(
                                    record.detail.as_deref().unwrap_or("no details recorded")
                                )
                            ));
                        }
                    }
                }
                for hint in remedies.iter().filter_map(Remedy::hint) {
                    note(format!("hint: {}", redact::text(&hint)));
                }
            }
        }
    }
    Ok(if waited.settled.is_ready() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_ATTENTION)
    })
}

/// What can be done about a workspace that is not ready.
fn remedies(waited: &Waited) -> Vec<Remedy> {
    match waited.settled {
        Settled::Ready | Settled::StillRunning => Vec::new(),
        Settled::NeedsAttention | Settled::NotSetUp => vec![Remedy::Setup {
            workspace: waited.workspace.path.clone(),
        }],
    }
}
