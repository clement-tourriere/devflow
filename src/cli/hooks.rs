//! `devflow hooks`: list and edit the `[hooks]` commands of `devflow.toml`
//! (or of the machine-local `devflow.local.toml`). Core edits the files
//! (`devflow_core::config_edit`): comments and order survive, the result is
//! validated before it is written, and a file that changed meanwhile is never
//! overwritten. This module parses arguments and renders.
//!
//! Not to be confused with `devflow hook`, which manages the Git
//! `post-checkout` hook.
//!
//! Changing a command changes what an approval covers; the commands print
//! that the new configuration needs review and never approve it.
use super::output::{self, Palette, note, say};
use super::{Ctx, ProjectInfo};
use anyhow::{Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use devflow_core::config::HookEntry;
use devflow_core::config_edit::{
    self, Applied, ConfigFiles, Edit, HookRef, HookRow, HooksView, PhaseView, Placement, Target,
};
use devflow_core::hooks::HookPhase;
use devflow_core::trust::{self, TrustState};
use devflow_core::{Project, project, redact};
use serde_json::{Value, json};
use std::path::Path;
use std::process::ExitCode;

#[derive(Clone, Copy, ValueEnum)]
pub enum PhaseArg {
    /// Runs once in a new (or adopted) worktree, after payloads, services and env
    Setup,
    /// Runs on every `devflow switch` into an existing workspace
    Switch,
    /// Runs before a workspace is removed; a failure blocks the removal
    Teardown,
}

impl From<PhaseArg> for HookPhase {
    fn from(phase: PhaseArg) -> Self {
        match phase {
            PhaseArg::Setup => HookPhase::Setup,
            PhaseArg::Switch => HookPhase::Switch,
            PhaseArg::Teardown => HookPhase::Teardown,
        }
    }
}

#[derive(Subcommand)]
pub enum HooksCommand {
    /// List the hooks of each phase (setup, switch, teardown)
    List,
    /// Add a command to a phase
    Add(AddArgs),
    /// Change a command (its text, name or timeout)
    Update(UpdateArgs),
    /// Remove a command
    Remove(RemoveArgs),
    /// Move a command to another position in its phase
    Move(MoveArgs),
}

impl HooksCommand {
    pub fn json_name(&self) -> &'static str {
        match self {
            HooksCommand::List => "hooks list",
            HooksCommand::Add(_) => "hooks add",
            HooksCommand::Update(_) => "hooks update",
            HooksCommand::Remove(_) => "hooks remove",
            HooksCommand::Move(_) => "hooks move",
        }
    }
}

#[derive(Args)]
pub struct AddArgs {
    /// When the command runs
    #[arg(value_enum)]
    pub phase: PhaseArg,
    /// The shell command (run with `sh -c` in the worktree)
    pub command: String,
    /// A name shown in output and logs (and to refer to the command later)
    #[arg(long)]
    pub name: Option<String>,
    /// How long it may run, overriding `[hooks] timeout` (for example 30s, 5m)
    #[arg(long, value_name = "DURATION")]
    pub timeout: Option<String>,
    /// Put it before this command (a name or a 1-based position)
    #[arg(long, value_name = "NAME|POSITION", conflicts_with = "after")]
    pub before: Option<String>,
    /// Put it after this command (a name or a 1-based position)
    #[arg(long, value_name = "NAME|POSITION")]
    pub after: Option<String>,
    /// Write to devflow.local.toml (this machine only) instead of devflow.toml
    #[arg(long)]
    pub local: bool,
    #[command(flatten)]
    pub guard: Guard,
}

#[derive(Args)]
pub struct UpdateArgs {
    #[arg(value_enum)]
    pub phase: PhaseArg,
    /// The command to change: a name or a 1-based position
    #[arg(value_name = "NAME|POSITION")]
    pub hook: String,
    /// The new shell command
    #[arg(long, value_name = "COMMAND")]
    pub run: Option<String>,
    /// The new name
    #[arg(long, conflicts_with = "clear_name")]
    pub name: Option<String>,
    /// Remove the name
    #[arg(long)]
    pub clear_name: bool,
    /// The new timeout (for example 30s, 5m)
    #[arg(long, value_name = "DURATION", conflicts_with = "clear_timeout")]
    pub timeout: Option<String>,
    /// Use the section's timeout again
    #[arg(long)]
    pub clear_timeout: bool,
    /// Edit devflow.local.toml instead of devflow.toml
    #[arg(long)]
    pub local: bool,
    #[command(flatten)]
    pub guard: Guard,
}

#[derive(Args)]
pub struct RemoveArgs {
    #[arg(value_enum)]
    pub phase: PhaseArg,
    /// The command to remove: a name or a 1-based position
    #[arg(value_name = "NAME|POSITION")]
    pub hook: String,
    /// Edit devflow.local.toml instead of devflow.toml
    #[arg(long)]
    pub local: bool,
    #[command(flatten)]
    pub guard: Guard,
}

#[derive(Args)]
pub struct MoveArgs {
    #[arg(value_enum)]
    pub phase: PhaseArg,
    /// The command to move: a name or a 1-based position
    #[arg(value_name = "NAME|POSITION")]
    pub hook: String,
    /// Where it goes: a 1-based position, `first` or `last`
    pub position: String,
    /// Edit devflow.local.toml instead of devflow.toml
    #[arg(long)]
    pub local: bool,
    #[command(flatten)]
    pub guard: Guard,
}

pub fn run(ctx: &Ctx, command: HooksCommand) -> Result<ExitCode> {
    let name = command.json_name();
    match command {
        HooksCommand::List => list(ctx, name),
        HooksCommand::Add(args) => add(ctx, name, args),
        HooksCommand::Update(args) => update(ctx, name, args),
        HooksCommand::Remove(args) => remove(ctx, name, args),
        HooksCommand::Move(args) => move_hook(ctx, name, args),
    }
}

// --- shared with `devflow env` ---------------------------------------------------

pub(super) fn target_of(local: bool) -> Target {
    if local {
        Target::Local
    } else {
        Target::Project
    }
}

/// Read the files of the project this command is about: the main checkout's,
/// whichever worktree the command runs in.
pub(super) fn read_files(ctx: &Ctx) -> Result<ConfigFiles> {
    let root = project::main_checkout(&ctx.cwd)?;
    ConfigFiles::read(&root)
}

/// Optimistic concurrency for scripts and agents: edit only if the file is
/// still the one that was read.
#[derive(Args, Default)]
pub struct Guard {
    /// Refuse unless the file still has this SHA-256 (`files.*.sha256` of `list --json`;
    /// `none` for a file that does not exist yet)
    #[arg(long, value_name = "SHA256")]
    pub if_unchanged: Option<String>,
}

/// Apply `edit` to the file the caller just looked at, or, with a
/// [`Guard`], to the file it looked at earlier.
pub(super) fn apply(
    files: &ConfigFiles,
    target: Target,
    edit: &Edit,
    guard: &Guard,
) -> Result<Applied> {
    let expected = match guard.if_unchanged.as_deref() {
        Some("none") => None,
        Some(hash) => Some(hash.to_ascii_lowercase()),
        None => files.file(target).hash.clone(),
    };
    config_edit::apply(&files.root, target, expected.as_deref(), edit)
}

/// What the edit means for trust: the new content needs review when what an
/// approval covers changed and the project is not trusted now. Never approves.
pub(super) struct TrustNote {
    pub scope_changed: bool,
    pub state: TrustState,
}

impl TrustNote {
    pub fn after(root: &Path, applied: &Applied) -> Result<Self> {
        let project = Project::open(root)?;
        Ok(Self {
            scope_changed: applied.scope_changed,
            state: trust::state(&project)?,
        })
    }

    pub fn needs_review(&self) -> bool {
        self.scope_changed && matches!(self.state, TrustState::Untrusted { .. })
    }

    pub fn json(&self) -> Value {
        json!({
            "state": self.state,
            "scope_changed": self.scope_changed,
            "needs_review": self.needs_review(),
            "review": self.needs_review().then_some("devflow trust"),
        })
    }

    pub fn say(&self) {
        if self.needs_review() {
            note(
                "The configuration changed, so it needs review before it runs: look at it with \
                 `devflow trust` and approve it there (devflow never approves on your behalf). \
                 Until then these hooks and [env] values are skipped.",
            );
        }
    }
}

pub(super) fn say_notes(applied: &Applied) {
    for text in &applied.notes {
        note(format!("note: {text}"));
    }
}

// --- list ------------------------------------------------------------------------

fn row_json(row: &HookRow) -> Value {
    json!({
        "position": row.index + 1,
        "name": row.name,
        "run": row.run,
        "timeout": row.timeout,
        "effective_timeout": row.effective_timeout,
    })
}

fn phase_json(view: &PhaseView) -> Value {
    json!({
        "source": view.source,
        "hooks": view.hooks.iter().map(row_json).collect::<Vec<_>>(),
        "shadowed": view.shadowed.iter().map(row_json).collect::<Vec<_>>(),
    })
}

fn hooks_json(view: &HooksView) -> Value {
    json!({
        "setup": phase_json(&view.setup),
        "switch": phase_json(&view.switch),
        "teardown": phase_json(&view.teardown),
    })
}

/// The two configuration files and their content hashes, for `--if-unchanged`.
pub(super) fn files_json(files: &ConfigFiles) -> Value {
    let one = |target: Target| {
        let file = files.file(target);
        json!({
            "name": file.path.file_name().map(|n| n.to_string_lossy().into_owned()),
            "path": file.path,
            "exists": file.text.is_some(),
            "sha256": file.hash,
        })
    };
    json!({ "project": one(Target::Project), "local": one(Target::Local) })
}

fn render_row(row: &HookRow, palette: Palette) -> String {
    let mut line = format!("  {:>2}  ", row.index + 1);
    match &row.name {
        Some(name) => line.push_str(&format!("[{name}] $ {}", row.run)),
        None => line.push_str(&format!("$ {}", row.run)),
    }
    if let Some(timeout) = &row.timeout {
        line.push_str(&palette.dim(&format!("  (timeout {timeout})")));
    }
    redact::text(&line)
}

fn list(ctx: &Ctx, name: &str) -> Result<ExitCode> {
    let files = read_files(ctx)?;
    let view = files.hooks()?;
    let project = Project::open(&files.root)?;
    let state = trust::state(&project)?;
    if ctx.json {
        let body = json!({
            "project": ProjectInfo::of(&project),
            "files": files_json(&files),
            "timeout": view.timeout,
            "timeout_source": view.timeout_source,
            "phases": hooks_json(&view),
            "trust": state,
        });
        output::emit(ctx, name, &body)?;
        return Ok(ExitCode::SUCCESS);
    }
    let palette = Palette::stdout();
    for phase in [&view.setup, &view.switch, &view.teardown] {
        let source = match phase.source {
            Target::Local => format!(" {}", palette.dim("(devflow.local.toml)")),
            Target::Project => String::new(),
        };
        say(format!("{}{source}", phase.phase.as_str()));
        if phase.hooks.is_empty() {
            say(palette.dim("  (none)"));
        }
        for row in &phase.hooks {
            say(render_row(row, palette));
        }
        if !phase.shadowed.is_empty() {
            say(palette.dim(&format!(
                "  devflow.local.toml replaces {} committed command(s) of this phase",
                phase.shadowed.len()
            )));
        }
    }
    say(format!("timeout: {} per command", view.timeout));
    match state {
        TrustState::Untrusted { .. } => note(
            "These commands are not approved on this machine yet: run `devflow trust` to review \
             and approve them.",
        ),
        TrustState::Trusted | TrustState::NothingToApprove => {}
    }
    Ok(ExitCode::SUCCESS)
}

// --- edits -------------------------------------------------------------------------

/// The hooks a typed reference is resolved against: the list of the file the
/// edit changes.
fn entries_of(files: &ConfigFiles, phase: HookPhase, target: Target) -> Result<Vec<HookEntry>> {
    let view = files.hooks()?;
    let rows = view.phase(phase).rows_for(target);
    Ok(rows
        .iter()
        .map(|row| HookEntry {
            name: row.name.clone(),
            run: row.run.clone(),
            timeout: row.timeout.clone(),
        })
        .collect())
}

fn reference(files: &ConfigFiles, phase: HookPhase, target: Target, text: &str) -> Result<HookRef> {
    let entries = entries_of(files, phase, target)?;
    Ok(HookRef::resolve_text(text, &entries))
}

/// A position typed by a person: `first`, `last` or a 1-based number.
fn position(text: &str) -> Result<Placement> {
    match text {
        "first" => Ok(Placement::Start),
        "last" => Ok(Placement::End),
        number => match number.parse::<usize>() {
            Ok(n) if n >= 1 => Ok(Placement::Index(n - 1)),
            _ => bail!("the position is a number from 1, `first` or `last`"),
        },
    }
}

/// Report an applied edit: JSON document or a line, plus notes and trust.
fn report(
    ctx: &Ctx,
    name: &str,
    files: &ConfigFiles,
    applied: &Applied,
    phase: HookPhase,
    verb: &str,
    extra: Value,
) -> Result<ExitCode> {
    let trust = TrustNote::after(&files.root, applied)?;
    let after = ConfigFiles::read(&files.root)?;
    let view = after.hooks()?;
    let project = Project::open(&files.root)?;
    if ctx.json {
        let mut body = json!({
            "project": ProjectInfo::of(&project),
            "file": applied.path.file_name().map(|n| n.to_string_lossy().into_owned()),
            "path": applied.path,
            "target": applied.target,
            "phase": phase.as_str(),
            "changed": applied.changed,
            "hooks": phase_json(view.phase(phase)),
            "notes": applied.notes,
            "trust": trust.json(),
        });
        if let (Value::Object(map), Value::Object(more)) = (&mut body, extra) {
            map.extend(more);
        }
        output::emit(ctx, name, &body)?;
        return Ok(ExitCode::SUCCESS);
    }
    let file = applied
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if applied.changed {
        say(format!("{verb} in hooks.{} ({file}).", phase.as_str()));
    } else {
        say(format!(
            "Nothing to change in hooks.{} ({file}).",
            phase.as_str()
        ));
    }
    say_notes(applied);
    trust.say();
    Ok(ExitCode::SUCCESS)
}

fn add(ctx: &Ctx, name: &str, args: AddArgs) -> Result<ExitCode> {
    let files = read_files(ctx)?;
    let target = target_of(args.local);
    let phase: HookPhase = args.phase.into();
    let at = match (&args.before, &args.after) {
        (Some(text), _) => Placement::Before(reference(&files, phase, target, text)?),
        (None, Some(text)) => Placement::After(reference(&files, phase, target, text)?),
        (None, None) => Placement::End,
    };
    let entry = HookEntry {
        name: args.name,
        run: args.command,
        timeout: args.timeout,
    };
    let edit = Edit::AddHook {
        phase,
        entry: entry.clone(),
        at,
    };
    let applied = apply(&files, target, &edit, &args.guard)?;
    let extra = json!({ "hook": {
        "name": entry.name, "run": entry.run, "timeout": entry.timeout,
    }});
    report(ctx, name, &files, &applied, phase, "Added", extra)
}

fn update(ctx: &Ctx, name: &str, args: UpdateArgs) -> Result<ExitCode> {
    if args.run.is_none()
        && args.name.is_none()
        && !args.clear_name
        && args.timeout.is_none()
        && !args.clear_timeout
    {
        bail!("nothing to change: pass --run, --name, --timeout, --clear-name or --clear-timeout");
    }
    let files = read_files(ctx)?;
    let target = target_of(args.local);
    let phase: HookPhase = args.phase.into();
    let entries = entries_of(&files, phase, target)?;
    let hook = HookRef::resolve_text(&args.hook, &entries);
    let current = match &hook {
        HookRef::Index(index) => entries.get(*index),
        HookRef::Name(name) => entries.iter().find(|e| e.name.as_deref() == Some(name)),
    }
    .cloned()
    .ok_or_else(|| {
        anyhow::anyhow!(
            "hooks.{} has no such command; `devflow hooks list` shows them",
            phase.as_str()
        )
    })?;
    let entry = HookEntry {
        name: if args.clear_name {
            None
        } else {
            args.name.or(current.name)
        },
        run: args.run.unwrap_or(current.run),
        timeout: if args.clear_timeout {
            None
        } else {
            args.timeout.or(current.timeout)
        },
    };
    let edit = Edit::UpdateHook {
        phase,
        hook,
        entry: entry.clone(),
    };
    let applied = apply(&files, target, &edit, &args.guard)?;
    let extra = json!({ "hook": {
        "name": entry.name, "run": entry.run, "timeout": entry.timeout,
    }});
    report(ctx, name, &files, &applied, phase, "Updated", extra)
}

fn remove(ctx: &Ctx, name: &str, args: RemoveArgs) -> Result<ExitCode> {
    let files = read_files(ctx)?;
    let target = target_of(args.local);
    let phase: HookPhase = args.phase.into();
    let hook = reference(&files, phase, target, &args.hook)?;
    let applied = apply(
        &files,
        target,
        &Edit::RemoveHook { phase, hook },
        &args.guard,
    )?;
    report(ctx, name, &files, &applied, phase, "Removed", json!({}))
}

fn move_hook(ctx: &Ctx, name: &str, args: MoveArgs) -> Result<ExitCode> {
    let files = read_files(ctx)?;
    let target = target_of(args.local);
    let phase: HookPhase = args.phase.into();
    let hook = reference(&files, phase, target, &args.hook)?;
    let to = position(&args.position)?;
    let applied = apply(
        &files,
        target,
        &Edit::MoveHook { phase, hook, to },
        &args.guard,
    )?;
    report(ctx, name, &files, &applied, phase, "Moved", json!({}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_are_numbers_from_one_first_or_last() {
        assert_eq!(position("first").unwrap(), Placement::Start);
        assert_eq!(position("last").unwrap(), Placement::End);
        assert_eq!(position("1").unwrap(), Placement::Index(0));
        assert_eq!(position("3").unwrap(), Placement::Index(2));
        for bad in ["0", "-1", "two", ""] {
            assert!(position(bad).is_err(), "{bad}");
        }
    }
}
