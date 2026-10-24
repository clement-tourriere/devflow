//! `devflow integrate`: set an agent host up to use devflow, and the two
//! entry points Claude Code calls (`claude-create`, `claude-remove`). What the
//! files hold and how they are edited is `devflow_core::integrate`.
use super::output::{self, Palette, Printer, note, say};
use super::{Ctx, ProjectInfo};
use anyhow::{Result, bail};
use clap::{Args, ValueEnum};
use devflow_core::integrate::{
    self, Change, ClaudeRemoved, Host, State, StdoutGuard, claude_create, claude_remove,
};
use devflow_core::propose::DiffKind;
use devflow_core::redact;
use devflow_core::remedy::{self, Remedy};
use serde_json::json;
use std::io::Read;
use std::process::ExitCode;

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HostArg {
    /// T3 Code: a script in t3.json that runs when a worktree is created
    T3,
    /// Claude Code: WorktreeCreate and WorktreeRemove hooks in .claude/settings.json
    Claude,
    /// Conductor: setup and archive scripts in .conductor/settings.toml
    Conductor,
    /// Cursor: a setup command in .cursor/worktrees.json
    Cursor,
    /// The WorktreeCreate hook of Claude Code (reads its JSON on standard input; prints the worktree path)
    #[value(name = "claude-create", hide = true)]
    ClaudeCreate,
    /// The WorktreeRemove hook of Claude Code (reads its JSON on standard input)
    #[value(name = "claude-remove", hide = true)]
    ClaudeRemove,
}

impl HostArg {
    fn host(self) -> Option<Host> {
        match self {
            HostArg::T3 => Some(Host::T3),
            HostArg::Claude => Some(Host::Claude),
            HostArg::Conductor => Some(Host::Conductor),
            HostArg::Cursor => Some(Host::Cursor),
            HostArg::ClaudeCreate | HostArg::ClaudeRemove => None,
        }
    }
}

#[derive(Args)]
pub struct IntegrateArgs {
    /// The host to set up (without one: the hosts this repository shows signs of, and how each stands)
    pub host: Option<HostArg>,
    /// Take devflow's entries out again (only those devflow wrote, unedited)
    #[arg(long, requires = "host")]
    pub remove: bool,
    /// Show what would change and write nothing
    #[arg(long, requires = "host")]
    pub print: bool,
    /// Write without asking
    #[arg(short = 'y', long, requires = "host")]
    pub yes: bool,
}

pub fn run(ctx: &Ctx, args: IntegrateArgs) -> Result<ExitCode> {
    match args.host {
        None => list(ctx),
        Some(HostArg::ClaudeCreate) => claude_create_hook(ctx),
        Some(HostArg::ClaudeRemove) => claude_remove_hook(ctx),
        Some(host) => match host.host() {
            Some(host) => integrate(ctx, host, &args),
            None => bail!("unknown host"),
        },
    }
}

// --- the list ------------------------------------------------------------

fn state_label(state: State) -> &'static str {
    match state {
        State::Configured => "set up",
        State::NotConfigured => "not set up",
        State::Conflict => "in the way of another entry",
    }
}

fn list(ctx: &Ctx) -> Result<ExitCode> {
    let project = ctx.project()?;
    let detected = integrate::detect(&project.root);
    let rows: Vec<serde_json::Value> = Host::ALL
        .into_iter()
        .map(|host| {
            let found = detected.iter().find(|d| d.host == host);
            let state = found
                .map(|d| d.state)
                .unwrap_or_else(|| integrate::state_of(&project.root, host));
            json!({
                "host": host,
                "label": host.label(),
                "file": host.file(),
                "detected": found.is_some(),
                "found": found.map(|d| &d.found),
                "state": state,
            })
        })
        .collect();
    if ctx.json {
        output::emit(
            ctx,
            "integrate",
            &json!({ "project": ProjectInfo::of(&project), "hosts": rows }),
        )?;
        return Ok(ExitCode::SUCCESS);
    }
    let table: Vec<Vec<String>> = Host::ALL
        .into_iter()
        .map(|host| {
            let found = detected.iter().find(|d| d.host == host);
            let state = found
                .map(|d| d.state)
                .unwrap_or_else(|| integrate::state_of(&project.root, host));
            vec![
                host.name().to_string(),
                host.label().to_string(),
                host.file().to_string(),
                if found.is_some() { "yes" } else { "-" }.to_string(),
                state_label(state).to_string(),
            ]
        })
        .collect();
    say(output::table(
        &["HOST", "NAME", "FILE", "IN USE HERE", "STATE"],
        &table,
    ));
    note("`devflow integrate HOST` shows the change and writes it; `--print` only shows it.");
    Ok(ExitCode::SUCCESS)
}

// --- a host ----------------------------------------------------------------

fn integrate(ctx: &Ctx, host: Host, args: &IntegrateArgs) -> Result<ExitCode> {
    let project = ctx.project()?;
    ctx.register(&project);
    let change = integrate::plan(&project.root, host, args.remove)?;
    if !ctx.json {
        show(&change);
    }
    let write = !args.print && change.writes();
    let confirmed = write && (args.yes || !ctx.interactive() || confirm(&change)?);
    let written = confirmed && integrate::apply(&change)?;
    let blocked = !change.conflicts.is_empty();
    let remedies = if blocked {
        vec![Remedy::OpenFile {
            path: change.file.clone(),
        }]
    } else {
        Vec::new()
    };
    if ctx.json {
        let diff: Vec<_> = change
            .diff()
            .into_iter()
            .map(|line| json!({ "kind": line.kind, "text": line.text }))
            .collect();
        let mut body = json!({
            "project": ProjectInfo::of(&project),
            "host": host,
            "file": change.file,
            "removing": change.remove,
            "written": written,
            "would_write": change.writes(),
            "printed_only": args.print,
            "already_done": change.is_done(),
            "conflicts": change.conflicts,
            "add_by_hand": change.manual,
            "notes": change.notes.iter().map(|n| n.describe()).collect::<Vec<_>>(),
            "diff": diff,
            "remedies": remedy::describe(&remedies),
        });
        if blocked {
            body["error"] = json!(format!(
                "{} left alone: {}",
                change.relative,
                change
                    .conflicts
                    .iter()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
        output::emit(ctx, "integrate", &body)?;
    } else {
        finish(&change, written, args);
    }
    Ok(if blocked {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

pub(super) fn show(change: &Change) {
    let palette = Palette::stdout();
    let verb = match (change.remove, change.before.is_some()) {
        (true, _) => "removing devflow's entries from",
        (false, false) => "creating",
        (false, true) => "changing",
    };
    if change.writes() {
        say(format!(
            "{} {verb} {}",
            change.host.label(),
            change.relative
        ));
        for line in change.diff() {
            say(match line.kind {
                DiffKind::Added => palette.green(&format!("+ {}", line.text)),
                DiffKind::Removed => palette.red(&format!("- {}", line.text)),
                DiffKind::Context => format!("  {}", line.text),
                DiffKind::Gap => palette.dim("  …"),
            });
        }
    }
}

fn confirm(change: &Change) -> Result<bool> {
    let prompt = format!("Write {}?", change.relative);
    match inquire::Confirm::new(&prompt).with_default(true).prompt() {
        Ok(answer) => Ok(answer),
        Err(
            inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted,
        ) => bail!("cancelled"),
        Err(error) => Err(error.into()),
    }
}

fn finish(change: &Change, written: bool, args: &IntegrateArgs) {
    let palette = Palette::stderr();
    if written {
        note(format!(
            "{} {} {}",
            palette.green("✓"),
            if change.remove { "updated" } else { "wrote" },
            change.relative
        ));
    } else if change.is_done() {
        note(format!(
            "{} {} {}",
            palette.green("✓"),
            change.relative,
            if change.remove {
                "has no devflow entries"
            } else {
                "already has devflow's setup; nothing to do"
            }
        ));
    } else if args.print && change.writes() {
        note("Nothing was written (--print).");
    } else if change.writes() {
        note("Nothing was written.");
    }
    for conflict in &change.conflicts {
        note(format!("{} {conflict}", palette.yellow("!")));
    }
    if !change.manual.is_empty() {
        note(format!(
            "Add this to the script that is in the way, in {}:",
            change.relative
        ));
        for line in &change.manual {
            note(format!("  {line}"));
        }
    }
    if written && !change.remove {
        for item in &change.notes {
            note(format!("  {}", redact::text(item.describe())));
        }
    }
}

// --- Claude Code's hooks -------------------------------------------------------

fn read_stdin() -> Result<String> {
    let mut text = String::new();
    std::io::stdin()
        .take(1024 * 1024)
        .read_to_string(&mut text)?;
    Ok(text)
}

/// `WorktreeCreate`: the one line on standard output is the worktree's path;
/// everything else, children's output included, goes to standard error.
fn claude_create_hook(ctx: &Ctx) -> Result<ExitCode> {
    if ctx.json {
        bail!(
            "`devflow integrate claude-create` prints the worktree path for Claude Code; it has no --json"
        );
    }
    let stdout = StdoutGuard::redirect()?;
    let input = integrate::parse_create_input(&read_stdin()?)?;
    let start = input
        .cwd
        .filter(|dir| dir.is_dir())
        .unwrap_or_else(|| ctx.cwd.clone());
    let project = devflow_core::Project::discover(&start)?;
    ctx.register(&project);
    let mut printer = Printer::new(ctx).announcing_database(!project.config.services.is_empty());
    let created = claude_create(&project, &input.name, &mut printer)?;
    for warning in &created.outcome.warnings {
        note(format!("warning: {}", redact::text(warning)));
    }
    let path = integrate::printable_path(&created.workspace)?;
    note(format!(
        "{} {} is ready at {path}",
        Palette::stderr().green("✓"),
        created.workspace.name
    ));
    stdout.print_line(&path)?;
    Ok(ExitCode::SUCCESS)
}

/// `WorktreeRemove`: take the workspace down; exit 0 only when the directory
/// is gone (or was never a devflow workspace).
fn claude_remove_hook(ctx: &Ctx) -> Result<ExitCode> {
    if ctx.json {
        bail!("`devflow integrate claude-remove` is called by Claude Code; it has no --json");
    }
    let input = integrate::parse_remove_input(&read_stdin()?)?;
    let mut printer = Printer::new(ctx);
    match claude_remove(&input.worktree_path, input.cwd.as_deref(), &mut printer)? {
        ClaudeRemoved::Removed(outcome) => {
            for warning in &outcome.warnings {
                note(format!("warning: {}", redact::text(warning)));
            }
            note(format!(
                "removed {} (the Git branch is kept)",
                outcome.workspace.name
            ));
        }
        ClaudeRemoved::AlreadyGone => {
            note("the worktree is already gone and nothing is registered for it")
        }
        ClaudeRemoved::NotOurs(why) => {
            note(format!(
                "{} is not a devflow workspace ({why}); nothing was done",
                input.worktree_path.display()
            ));
        }
    }
    Ok(ExitCode::SUCCESS)
}

// --- offered by `devflow init` ------------------------------------------------------

/// What `devflow init` did about one agent host.
pub(super) struct Offered {
    pub host: Host,
    /// The paths that show the repository uses the host.
    pub found: Vec<String>,
    /// Devflow's entries are in the host's file now.
    pub configured: bool,
    pub written: bool,
    pub conflicts: Vec<devflow_core::integrate::Conflict>,
    pub error: Option<String>,
}

impl Offered {
    pub fn to_json(&self) -> serde_json::Value {
        json!({
            "host": self.host,
            "label": self.host.label(),
            "file": self.host.file(),
            "found": self.found,
            "configured": self.configured,
            "written": self.written,
            "conflicts": self.conflicts,
            "error": self.error,
        })
    }
}

/// The hosts the repository is used with, plus the `requested` ones. A
/// requested host is set up; a detected one is set up when a person at the
/// terminal says yes, and otherwise only listed (`configured: false`), so no
/// unattended run writes into another tool's files it was not asked about.
pub(super) fn offer(
    ctx: &Ctx,
    project: &devflow_core::Project,
    requested: &[Host],
) -> Vec<Offered> {
    let detected = integrate::detect(&project.root);
    let mut hosts: Vec<(Host, Vec<String>)> =
        detected.iter().map(|d| (d.host, d.found.clone())).collect();
    for host in requested {
        if !hosts.iter().any(|(h, _)| h == host) {
            hosts.push((*host, Vec::new()));
        }
    }
    hosts
        .into_iter()
        .map(|(host, found)| {
            let mut offered = Offered {
                host,
                found,
                configured: false,
                written: false,
                conflicts: Vec::new(),
                error: None,
            };
            let change = match integrate::plan(&project.root, host, false) {
                Ok(change) => change,
                Err(error) => {
                    offered.error = Some(format!("{error:#}"));
                    return offered;
                }
            };
            offered.conflicts = change.conflicts.clone();
            if change.is_done() {
                offered.configured = true;
                return offered;
            }
            let wanted = if requested.contains(&host) {
                true
            } else if ctx.interactive() && change.writes() {
                show(&change);
                let prompt = format!(
                    "{} is used here ({}). Set up its worktrees with devflow?",
                    host.label(),
                    offered.found.join(", ")
                );
                inquire::Confirm::new(&prompt)
                    .with_default(true)
                    .prompt()
                    .unwrap_or(false)
            } else {
                false
            };
            if wanted {
                match integrate::apply(&change) {
                    Ok(written) => {
                        offered.written = written;
                        offered.configured = change.conflicts.is_empty();
                    }
                    Err(error) => offered.error = Some(format!("{error:#}")),
                }
            }
            offered
        })
        .collect()
}
