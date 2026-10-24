//! Resting workspaces: `suspend`, `resume`, `pin` and `unpin`. Nothing here
//! runs by itself and nothing is suspended unless it is asked for by name or
//! by `--idle`; core ([`devflow_core::suspend`]) decides what stops and what
//! is recorded, this module parses, confirms at a terminal and renders.
use super::output::{self, Palette, Printer, note, say};
use super::{Ctx, ProjectInfo, trust, workspace as workspaces};
use anyhow::Result;
use clap::Args;
use devflow_core::pitchfork::Pitchfork;
use devflow_core::redact;
use devflow_core::remedy::{Problem, Remedy};
use devflow_core::setup::{self, SetupOptions};
use devflow_core::suspend::{self, Idle, SkipReason, Skipped, Suspension};
use devflow_core::worktree::Workspace;
use serde_json::json;
use std::process::ExitCode;

#[derive(Args)]
pub struct SuspendArgs {
    /// Workspace name, branch or path (default: the one you are in)
    #[arg(add = super::complete::workspace(), conflicts_with = "idle")]
    pub name: Option<String>,
    /// Instead, every unpinned workspace not used for this long (for example
    /// 3d, 12h or 90m); the main checkout, the one you are in and anything
    /// whose state cannot be read are never selected
    #[arg(long, value_name = "DURATION", value_parser = suspend::parse_idle)]
    pub idle: Option<std::time::Duration>,
    /// Do not ask before suspending the idle workspaces
    #[arg(short = 'y', long, requires = "idle")]
    pub yes: bool,
}

#[derive(Args)]
pub struct ResumeArgs {
    /// Workspace name, branch or path (default: the one you are in)
    #[arg(add = super::complete::workspace())]
    pub name: Option<String>,
}

#[derive(Args)]
pub struct PinArgs {
    /// Workspace name, branch or path (default: the one you are in)
    #[arg(add = super::complete::workspace())]
    pub name: Option<String>,
}

pub fn suspend(ctx: &Ctx, args: SuspendArgs) -> Result<ExitCode> {
    let project = ctx.project()?;
    let pitchfork = Pitchfork::locate();
    // The workspaces to suspend, each with what was expected of it when it was
    // selected (core looks again, under the workspace's locks).
    let (targets, skipped): (Vec<(Workspace, Option<suspend::Expect>)>, Vec<Skipped>) = match args
        .idle
    {
        None => {
            let workspace = super::processes::target(ctx, &project, args.name.as_deref())?;
            ctx.target_is(&workspace.path);
            (vec![(workspace, None)], Vec::new())
        }
        Some(idle) => {
            let selection =
                suspend::select_idle(&project, idle, chrono::Utc::now(), Some(&ctx.cwd))?;
            let now = chrono::Utc::now();
            if !selection.idle.is_empty() && !args.yes {
                if !ctx.interactive() {
                    return Err(Problem::new(format!(
                        "{} idle workspace(s) would be suspended ({}); suspending them needs \
                         confirmation, which only a person at a terminal can give",
                        selection.idle.len(),
                        names(&selection.idle)
                    ))
                    .into_error());
                }
                for Idle {
                    workspace,
                    last_active_at,
                    ..
                } in &selection.idle
                {
                    note(format!(
                        "  {}  (last used {})",
                        workspace.name,
                        output::ago(*last_active_at, now)
                    ));
                }
                if !confirm(&format!(
                    "Suspend {} workspace(s)? Their containers and daemons stop; their data is kept.",
                    selection.idle.len()
                ))? {
                    anyhow::bail!("cancelled");
                }
            }
            (
                selection
                    .idle
                    .into_iter()
                    .map(|i| {
                        let expect = suspend::Expect {
                            instance: i.instance,
                            idle,
                        };
                        (i.workspace, Some(expect))
                    })
                    .collect(),
                selection.skipped,
            )
        }
    };
    let results: Vec<Suspension> = targets
        .iter()
        .map(|(workspace, expect)| {
            suspend::suspend_expecting(&project, workspace, pitchfork.as_ref(), expect.as_ref())
        })
        .collect();
    // A workspace that changed after it was selected is left alone, which is
    // not a failure.
    let ok = results
        .iter()
        .all(|r| r.suspended || r.left_alone.is_some());
    if ctx.json && !ok {
        // The failure envelope (`error`, `hint`, `remedies`) with what was
        // done kept beside it.
        let failed: Vec<&Suspension> = results
            .iter()
            .filter(|r| !r.suspended && r.left_alone.is_none())
            .collect();
        let message = failed
            .iter()
            .map(|r| {
                format!(
                    "{}: not suspended: {}",
                    r.workspace,
                    r.problem.as_deref().unwrap_or("unknown reason")
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        let mut remedies: Vec<Remedy> = Vec::new();
        for remedy in failed.iter().flat_map(|r| r.remedies.iter()) {
            if !remedies.contains(remedy) {
                remedies.push(remedy.clone());
            }
        }
        let hint = remedies.iter().find_map(Remedy::hint);
        let partial = json!({
            "project": ProjectInfo::of(&project),
            "ok": false,
            "workspaces": results,
            "skipped": skipped,
        });
        output::emit_error_with(
            ctx.take_warnings(),
            "suspend",
            &message,
            hint.as_ref(),
            &remedies,
            partial,
        );
    } else if ctx.json {
        let body = json!({
            "project": ProjectInfo::of(&project),
            "ok": ok,
            "workspaces": results,
            "skipped": skipped,
        });
        output::emit(ctx, "suspend", &body)?;
    } else {
        let palette = Palette::stdout();
        if results.is_empty() && args.idle.is_some() {
            say("No idle workspace to suspend.");
        }
        for result in &results {
            for line in suspension_lines(result, palette) {
                say(line);
            }
        }
        for skip in skipped.iter().filter(|s| !is_expected(&s.reason)) {
            note(format!(
                "{} {}: not considered ({})",
                Palette::stderr().yellow("!"),
                skip.workspace,
                skip_reason(&skip.reason)
            ));
        }
    }
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn names(idle: &[Idle]) -> String {
    idle.iter()
        .map(|i| i.workspace.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Reasons a workspace is left out that need no mention: the main checkout,
/// the caller's own, one already suspended, one pinned, one not set up.
fn is_expected(reason: &SkipReason) -> bool {
    !matches!(reason, SkipReason::Unreadable { .. })
}

fn skip_reason(reason: &SkipReason) -> String {
    match reason {
        SkipReason::Pinned => "pinned".into(),
        SkipReason::Default => "the main checkout".into(),
        SkipReason::Current => "the workspace you are in".into(),
        SkipReason::Suspended => "already suspended".into(),
        SkipReason::NotSetUp => "not set up".into(),
        SkipReason::Unreadable { detail } => {
            format!("its state cannot be read, so it is not known to be idle: {detail}")
        }
    }
}

fn suspension_lines(result: &Suspension, palette: Palette) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(report) = &result.daemons {
        lines.extend(super::processes::report_lines(report, palette));
    }
    if let Some(changes) = &result.services {
        lines.extend(super::processes::service_lines(changes, palette, true));
    }
    if let Some(why) = &result.left_alone {
        lines.push(format!(
            "{} {}: left alone, {why}",
            palette.yellow("!"),
            result.workspace
        ));
        return lines;
    }
    match &result.problem {
        None => lines.push(format!(
            "{} {}: suspended; its data is kept and the shared database is untouched",
            palette.green("✓"),
            result.workspace
        )),
        Some(problem) => {
            lines.push(format!(
                "{} {}: not suspended: {}",
                palette.yellow("!"),
                result.workspace,
                redact::text(problem)
            ));
            lines.extend(
                result
                    .remedies
                    .iter()
                    .filter_map(Remedy::hint)
                    .map(|hint| format!("  hint: {}", redact::text(&hint))),
            );
        }
    }
    lines
}

pub fn resume(ctx: &Ctx, args: ResumeArgs) -> Result<ExitCode> {
    let project = ctx.project()?;
    let workspace = super::processes::target(ctx, &project, args.name.as_deref())?;
    ctx.target_is(&workspace.path);
    ctx.register(&project);
    // Resuming is entering the workspace: the same activation `switch` runs,
    // without the change of directory.
    let mut options = SetupOptions {
        non_interactive: !ctx.interactive(),
        ..SetupOptions::default()
    };
    let relevant = workspaces::switch_needs_trust(&project, &workspace, false);
    options.approval = trust::resolve(ctx, &project, &workspace.path, relevant)?;
    let mut printer = Printer::new(ctx)
        .announcing_database(!project.config.services.is_empty() && !options.no_db);
    match setup::activate(&project, &workspace, options, &mut printer) {
        Ok(outcome) => workspaces::report(ctx, "resume", &project, &outcome, false),
        Err(error) => {
            workspaces::in_progress(ctx, "resume", &project, Some(&workspace), error, None)
        }
    }
}

pub fn pin(ctx: &Ctx, args: PinArgs, pinned: bool) -> Result<ExitCode> {
    let project = ctx.project()?;
    let workspace = super::processes::target(ctx, &project, args.name.as_deref())?;
    ctx.target_is(&workspace.path);
    let changed = suspend::set_pinned(&project, &workspace, pinned)?;
    let command = if pinned { "pin" } else { "unpin" };
    if ctx.json {
        let body = json!({
            "project": ProjectInfo::of(&project),
            "workspace": workspace.name,
            "path": workspace.path,
            "pinned": pinned,
            "changed": changed,
        });
        output::emit(ctx, command, &body)?;
    } else {
        let palette = Palette::stdout();
        say(match (pinned, changed) {
            (true, true) => format!(
                "{} {} is pinned: --idle will not suspend it",
                palette.green("✓"),
                workspace.name
            ),
            (true, false) => format!(
                "{} {} was already pinned",
                palette.green("✓"),
                workspace.name
            ),
            (false, true) => format!(
                "{} {} is not pinned any more",
                palette.green("✓"),
                workspace.name
            ),
            (false, false) => format!("{} {} was not pinned", palette.green("✓"), workspace.name),
        });
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
