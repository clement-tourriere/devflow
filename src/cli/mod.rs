//! Command definitions and dispatch. Handlers parse arguments, prompt and
//! render; every lifecycle decision is made by `devflow-core`.
mod config;
mod db;
mod env;
mod exec;
mod hooks;
mod init;
mod output;
mod processes;
mod project;
mod service;
mod shell;
mod system;
mod trust;
mod workspace;

use anyhow::{Context, Result};
use clap::builder::BoolishValueParser;
use clap::error::ErrorKind;
use clap::{ArgAction, CommandFactory, Parser, Subcommand};
use devflow_core::error::Protected;
use devflow_core::remedy::{self, Remedy};
use devflow_core::{Project, redact};
use serde::Serialize;
use std::cell::RefCell;
use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Exit status when the resulting workspace is not ready (`needs_attention`).
const EXIT_ATTENTION: u8 = 2;

#[derive(Parser)]
#[command(
    name = "devflow",
    version,
    about = "Git worktrees with copy-on-write payloads, instant Postgres branches, and setup hooks",
    arg_required_else_help = true
)]
pub struct Cli {
    /// Print one JSON document instead of human-readable output (never prompts)
    #[arg(long, global = true)]
    json: bool,

    /// Never prompt; untrusted hooks are skipped and reported as needs_attention
    ///
    /// The environment variable accepts 1/0, true/false, yes/no, on/off.
    #[arg(
        long,
        global = true,
        env = "DEVFLOW_NON_INTERACTIVE",
        value_parser = BoolishValueParser::new()
    )]
    non_interactive: bool,

    /// Run as if devflow was started in DIR
    #[arg(short = 'C', global = true, value_name = "DIR", value_hint = clap::ValueHint::DirPath)]
    dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Propose a setup from what the repository contains, review it and write it
    Init(init::InitArgs),
    /// Switch to a workspace; with -c, create it first
    Switch(workspace::SwitchArgs),
    /// List the workspaces of this project (or of every registered project)
    List(workspace::ListArgs),
    /// Print the path of a workspace
    Path(workspace::PathArgs),
    /// Run a command with the workspace's variables (`.devflow/env`) applied last
    Exec(exec::ExecArgs),
    /// Set up (or resume setting up) the current worktree
    Setup(workspace::SetupArgs),
    /// Remove a workspace, its database branch and (optionally) its Git branch
    Remove(workspace::RemoveArgs),
    /// Forget the Git registrations of worktrees whose directory is gone (asks first)
    Prune(workspace::PruneArgs),
    /// Review and approve the project's hook commands
    Trust(trust::TrustArgs),
    /// List and edit the setup, switch and teardown commands (`hook` is the Git hook)
    Hooks {
        #[command(subcommand)]
        command: hooks::HooksCommand,
    },
    /// List and edit the [env] recipes and framework presets
    Env {
        #[command(subcommand)]
        command: env::EnvCommand,
    },
    /// Mend the list of known projects: forget one, point one at its new folder
    Project {
        #[command(subcommand)]
        command: project::ProjectCommand,
    },
    /// Bring the configuration files to the current format
    Config {
        #[command(subcommand)]
        command: config::ConfigCommand,
    },
    /// Manage the post-checkout Git hook that sets up worktrees made by other tools
    Hook {
        #[command(subcommand)]
        action: workspace::HookAction,
    },
    /// Postgres server, branches and snapshots
    Db {
        #[command(subcommand)]
        command: db::DbCommand,
    },
    /// Start the workspace's pitchfork daemons (approved, ready workspaces only)
    Up(processes::UpArgs),
    /// Stop the workspace's pitchfork daemons
    Down(processes::DownArgs),
    /// Show the workspace's daemons: state, port and URL
    Ps(processes::PsArgs),
    /// Is pitchfork installed and is its proxy on; install it (asks first)
    Pitchfork {
        #[command(subcommand)]
        action: processes::PitchforkAction,
    },
    /// Valkey, Redis and MySQL (preview) services: copy-on-write branches, snapshots and reset
    Service(service::ServiceArgs),
    /// Run one journaled clone, snapshot or reset (started by devflow itself)
    #[command(name = "__clone", hide = true)]
    Coordinate(service::CoordinateArgs),
    /// Check git, docker, the Postgres server and the project setup
    Doctor,
    /// Print the shell wrapper that lets `devflow switch` change directory
    ShellInit(shell::ShellInitArgs),
    /// Print shell completions
    Completions(system::CompletionsArgs),
    /// Open the desktop app
    Gui,
}

impl Command {
    /// Document name for `--json`; `None` for commands that cannot emit JSON.
    fn json_name(&self) -> Option<&'static str> {
        Some(match self {
            Command::Init(_) => "init",
            Command::Switch(_) => "switch",
            Command::List(_) => "list",
            Command::Path(_) => "path",
            Command::Setup(_) => "setup",
            Command::Remove(_) => "remove",
            Command::Prune(_) => "prune",
            Command::Trust(_) => "trust",
            Command::Hooks { command } => command.json_name(),
            Command::Env { command } => command.json_name(),
            Command::Hook { action } => action.json_name(),
            Command::Project { command } => command.json_name(),
            Command::Config { command } => command.json_name(),
            Command::Db { command } => command.json_name()?,
            Command::Up(_) => "up",
            Command::Down(_) => "down",
            Command::Ps(_) => "ps",
            Command::Pitchfork { action } => action.json_name(),
            Command::Service(args) => args.command.json_name(),
            Command::Doctor => "doctor",
            Command::ShellInit(_)
            | Command::Completions(_)
            | Command::Gui
            | Command::Exec(_)
            | Command::Coordinate(_) => return None,
        })
    }

    fn display_name(&self) -> &'static str {
        match self {
            Command::Coordinate(_) => "__clone",
            Command::Exec(_) => "exec",
            Command::ShellInit(_) => "shell-init",
            Command::Completions(_) => "completions",
            Command::Gui => "gui",
            Command::Db { .. } => "db psql",
            _ => "devflow",
        }
    }
}

/// Project identity as embedded in JSON documents.
#[derive(Serialize)]
pub struct ProjectInfo<'a> {
    name: &'a str,
    id: uuid::Uuid,
    root: &'a Path,
}

impl<'a> ProjectInfo<'a> {
    pub fn of(project: &'a Project) -> Self {
        Self {
            name: project.name(),
            id: project.id(),
            root: &project.root,
        }
    }
}

/// What every handler needs to know about how it was invoked.
pub struct Ctx {
    pub json: bool,
    non_interactive: bool,
    /// Absolute directory the command runs "in" (`-C` or the process directory).
    pub cwd: PathBuf,
    /// `-C` was given: a command devflow starts runs in `cwd`.
    pub chdir: bool,
    warnings: RefCell<Vec<String>>,
    /// The checkout a command resolved as its target (`up feature` from the
    /// main checkout targets `feature`'s). Remedies that act on "the
    /// workspace" name this directory, never the caller's own.
    target: RefCell<Option<PathBuf>>,
}

impl Ctx {
    /// Prompts are allowed only for a person at a terminal: `--json` and
    /// `--non-interactive` never prompt.
    pub fn interactive(&self) -> bool {
        !self.json
            && !self.non_interactive
            && std::io::stdin().is_terminal()
            && std::io::stderr().is_terminal()
    }

    pub fn project(&self) -> Result<Project> {
        Project::discover(&self.cwd)
    }

    /// Add the project to the machine-local registry; failure only warns.
    pub fn register(&self, project: &Project) {
        if let Err(error) = devflow_core::registry::add(project) {
            self.warn(format!("could not register the project: {error:#}"));
        }
    }

    /// Human mode prints the warning to stderr; JSON mode adds it to `warnings`.
    pub fn warn(&self, message: impl Into<String>) {
        let message = message.into();
        if self.json {
            self.warnings.borrow_mut().push(message);
        } else {
            output::note(format!("warning: {}", redact::text(&message)));
        }
    }

    /// Record the checkout the command resolved as its target.
    pub fn target_is(&self, workspace: &Path) {
        *self.target.borrow_mut() = Some(workspace.to_path_buf());
    }

    pub(crate) fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut *self.warnings.borrow_mut())
    }
}

pub fn main() -> ExitCode {
    shell::take_handoff();
    let args: Vec<OsString> = std::env::args_os().collect();
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(error) => return usage_error(&args, &error),
    };
    let json_name = cli.command.json_name();
    let display = cli.command.display_name();
    let json = cli.json;
    let command = json_name.unwrap_or(display);
    let ctx = match context(&cli) {
        Ok(ctx) => ctx,
        Err(error) => return fail(None, json, command, &error),
    };
    match run(&ctx, cli.command) {
        Ok(code) => code,
        Err(error) => fail(Some(&ctx), json, command, &error),
    }
}

fn context(cli: &Cli) -> Result<Ctx> {
    let cwd = match &cli.dir {
        Some(dir) => dir
            .canonicalize()
            .with_context(|| format!("cannot use -C {}", dir.display()))?,
        None => std::env::current_dir().context("cannot determine the current directory")?,
    };
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    Ok(Ctx {
        json: cli.json,
        non_interactive: cli.non_interactive,
        cwd,
        chdir: cli.dir.is_some(),
        warnings: RefCell::new(Vec::new()),
        target: RefCell::new(None),
    })
}

/// Report a failure: one redacted JSON document on stdout when `--json` was
/// asked for (whatever stage failed), and `error:` plus `hint:` lines on stderr.
fn fail(ctx: Option<&Ctx>, json: bool, command: &str, error: &anyhow::Error) -> ExitCode {
    let message = format!("{error:#}");
    let mut remedies = remedy::remedies_of(error);
    if let Some(refusal) = error.downcast_ref::<devflow_core::processes::Refusal>()
        && let Some(target) = ctx.and_then(|ctx| ctx.target.borrow().clone())
    {
        // The remedies of a refusal act on the workspace the command resolved
        // (`up feature` from the main checkout), not on the caller's directory;
        // without one they would name a place nobody chose, so there are none.
        remedies.extend(refusal.remedies(&target));
    }
    let hints = hints(error, &remedies);
    if json {
        let warnings = ctx.map(Ctx::take_warnings).unwrap_or_default();
        output::emit_error(warnings, command, &message, hints.first(), &remedies);
    }
    output::note(format!("error: {}", redact::text(&message)));
    for hint in hints {
        output::note(format!("hint: {}", redact::text(&hint)));
    }
    ExitCode::FAILURE
}

/// What the person can do about a failure, chosen from the error's type: the
/// remedies core attached to it (or knows for it) and the CLI-specific
/// refusals. The first line is also the JSON document's `hint`.
fn hints(error: &anyhow::Error, remedies: &[Remedy]) -> Vec<String> {
    let mut hints: Vec<String> = Vec::new();
    if let Some(refusal) = error.downcast_ref::<devflow_core::processes::Refusal>() {
        // What only the command line has: its options and sibling commands.
        use devflow_core::processes::{Action, Refusal};
        match refusal {
            Refusal::AutonomousTriggers { .. } => {
                hints.push("start the plain daemons by name: `devflow up --daemon NAME`".into());
            }
            Refusal::UnknownDaemon { .. } => {
                hints.push("see the daemons with `devflow ps`".into());
            }
            Refusal::NotApproved {
                action: Action::Stop,
                ..
            }
            | Refusal::StateUnknown { .. } => {
                hints.extend(refusal.hint());
                hints.push("pass --force to go on without knowing".into());
            }
            _ => hints.extend(refusal.hint()),
        }
    }
    if let Some(protected) = error.downcast_ref::<Protected>()
        && protected.forceable
    {
        hints.push(format!(
            "run `devflow remove {} --force` to override this protection",
            protected.workspace
        ));
    }
    hints.extend(remedies.iter().filter_map(Remedy::hint));
    hints
}

/// An argument error (or `--help`/`--version`) from clap. Help and version
/// keep clap's behavior; every failure honors `--json` and redaction, because
/// arguments can carry credentials (`--database-url`) that clap echoes back.
fn usage_error(args: &[OsString], error: &clap::Error) -> ExitCode {
    if matches!(
        error.kind(),
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
    ) {
        let _ = error.print();
        return ExitCode::SUCCESS;
    }
    let rendered = error.render();
    let palette = output::Palette::stderr();
    // Usage errors exit 1: status 2 is reserved for needs_attention.
    if json_requested(args) {
        let command = command_name(args);
        let message = if error.kind() == ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand {
            let help = if command == "devflow" {
                command.clone()
            } else {
                format!("devflow {command}")
            };
            format!("a command is required; see `{help} --help`")
        } else {
            summary(&rendered.to_string())
        };
        output::emit_error(Vec::new(), &command, &message, None, &[]);
    }
    let text = if palette.is_enabled() {
        rendered.ansi().to_string()
    } else {
        rendered.to_string()
    };
    output::note(redact::text(text.trim_end()));
    ExitCode::FAILURE
}

/// Whether the arguments ask for `--json` (before any `--`).
fn json_requested(args: &[OsString]) -> bool {
    args.iter()
        .skip(1)
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "--json")
}

/// The command a failed invocation was aimed at, as a `--json` document name:
/// the chain of subcommand names found in the arguments (`db snapshot create`).
/// The values of options that take one (`-C DIR`, `--from WS`) are skipped, so
/// a directory or workspace that happens to be called like a command is not
/// mistaken for it; clap's own definition says which options those are.
fn command_name(args: &[OsString]) -> String {
    let root = Cli::command();
    let globals: Vec<&clap::Arg> = root
        .get_arguments()
        .filter(|arg| arg.is_global_set())
        .collect();
    let takes_value =
        |arg: &clap::Arg| matches!(arg.get_action(), ArgAction::Set | ArgAction::Append);
    let mut command = root.clone();
    let mut names: Vec<String> = Vec::new();
    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        let Some(arg) = arg.to_str() else { continue };
        if arg == "--" {
            break;
        }
        let known = |matches: &dyn Fn(&clap::Arg) -> bool| {
            command
                .get_arguments()
                .chain(globals.iter().copied())
                .find(|arg| matches(arg))
                .is_some_and(takes_value)
        };
        if let Some(long) = arg.strip_prefix("--") {
            // `--name=value` carries its value; `--name value` leaves it next.
            if !long.contains('=') && known(&|arg| arg.get_long() == Some(long)) {
                rest.next();
            }
        } else if let Some(shorts) = arg.strip_prefix('-').filter(|shorts| !shorts.is_empty()) {
            // `-abC dir` / `-Cdir`: the first option that takes a value ends
            // the cluster, and the value is the rest of it or the next argument.
            let taking = shorts
                .char_indices()
                .find(|(_, short)| known(&|arg| arg.get_short() == Some(*short)));
            if let Some((at, short)) = taking
                && at + short.len_utf8() == shorts.len()
            {
                rest.next();
            }
        } else if let Some(next) = command.find_subcommand(arg) {
            names.push(next.get_name().to_string());
            command = next.clone();
        }
    }
    if names.is_empty() {
        "devflow".to_string()
    } else {
        names.join(" ")
    }
}

/// The first paragraph of a clap diagnostic as one line, without `error: `.
fn summary(rendered: &str) -> String {
    let first = rendered.trim().split("\n\n").next().unwrap_or("");
    let line = first.split_whitespace().collect::<Vec<_>>().join(" ");
    line.strip_prefix("error: ").unwrap_or(&line).to_string()
}

fn run(ctx: &Ctx, command: Command) -> Result<ExitCode> {
    if ctx.json && command.json_name().is_none() {
        anyhow::bail!(
            "`devflow {}` does not support --json",
            command.display_name()
        );
    }
    match command {
        Command::Init(args) => init::run(ctx, args),
        Command::Switch(args) => workspace::switch(ctx, args),
        Command::List(args) => workspace::list(ctx, args),
        Command::Path(args) => workspace::path(ctx, args),
        Command::Exec(args) => exec::run(ctx, args),
        Command::Setup(args) => workspace::setup(ctx, args),
        Command::Remove(args) => workspace::remove(ctx, args),
        Command::Prune(args) => workspace::prune(ctx, args),
        Command::Trust(args) => trust::command(ctx, args),
        Command::Hooks { command } => hooks::run(ctx, command),
        Command::Env { command } => env::run(ctx, command),
        Command::Hook { action } => workspace::hook(ctx, action),
        Command::Project { command } => project::run(ctx, command),
        Command::Config { command } => config::run(ctx, command),
        Command::Db { command } => db::run(ctx, command),
        Command::Up(args) => processes::up(ctx, args),
        Command::Down(args) => processes::down(ctx, args),
        Command::Ps(args) => processes::ps(ctx, args),
        Command::Pitchfork { action } => processes::pitchfork_command(ctx, action),
        Command::Service(args) => service::run(ctx, args),
        Command::Coordinate(args) => service::coordinate(args),
        Command::Doctor => system::doctor(ctx),
        Command::ShellInit(args) => shell::init(args),
        Command::Completions(args) => system::completions::<Cli>(args),
        Command::Gui => system::gui(ctx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        std::iter::once("devflow")
            .chain(list.iter().copied())
            .map(OsString::from)
            .collect()
    }

    #[test]
    fn json_is_requested_only_before_the_terminator() {
        assert!(json_requested(&args(&["--json", "list"])));
        assert!(json_requested(&args(&["list", "--json"])));
        assert!(!json_requested(&args(&["list"])));
        assert!(!json_requested(&args(&["switch", "--", "--json"])));
    }

    #[test]
    fn failed_invocations_are_named_after_their_subcommands() {
        let name = |list: &[&str]| command_name(&args(list));
        assert_eq!(name(&["--json"]), "devflow");
        assert_eq!(name(&["--json", "bogus"]), "devflow");
        assert_eq!(name(&["--json", "path"]), "path");
        assert_eq!(name(&["-C", "/tmp/x", "--json", "list", "--all"]), "list");
        // The value of an option is not a command, wherever the option sits.
        assert_eq!(name(&["--json", "-C", "list", "path"]), "path");
        assert_eq!(name(&["-C", "db", "--json", "path"]), "path");
        assert_eq!(name(&["--json", "-Clist", "path"]), "path");
        assert_eq!(name(&["--json", "-C=list", "path"]), "path");
        assert_eq!(
            name(&["db", "-C", "snapshot", "snapshot", "list"]),
            "db snapshot list"
        );
        assert_eq!(name(&["--json", "-C", "list"]), "devflow");
        assert_eq!(name(&["--json", "--non-interactive", "path"]), "path");
        assert_eq!(name(&["--json", "-C", "x", "--", "list"]), "devflow");
        assert_eq!(
            name(&["db", "snapshot", "create", "seed"]),
            "db snapshot create"
        );
        assert_eq!(
            name(&["db", "branch", "delete", "list"]),
            "db branch delete"
        );
        assert_eq!(name(&["hook", "install"]), "hook install");
        // A workspace that happens to be called like a command is not one.
        assert_eq!(name(&["switch", "-c", "db"]), "switch");
    }

    #[test]
    fn clap_diagnostics_are_summarized_on_one_line() {
        let error = Cli::try_parse_from(["devflow", "path"]).err().unwrap();
        assert_eq!(
            summary(&error.render().to_string()),
            "the following required arguments were not provided: <NAME>"
        );
    }

    #[test]
    fn hints_follow_the_error_type() {
        let hint = |error: &anyhow::Error| hints(error, &remedy::remedies_of(error));
        let missing = anyhow::Error::new(devflow_core::error::NotInitialized {
            root: PathBuf::from("/repo"),
        })
        .context("while listing");
        let shown = hint(&missing);
        assert_eq!(shown.len(), 1);
        assert!(shown[0].contains("`devflow init` in /repo"), "{shown:?}");

        let protected = |forceable| {
            anyhow::Error::new(Protected {
                workspace: "feature/a".into(),
                reason: "has uncommitted changes".into(),
                forceable,
                remedies: Vec::new(),
            })
        };
        assert!(hint(&protected(true))[0].contains("devflow remove feature/a --force"));
        assert!(hint(&protected(false)).is_empty());
        // The wording of a message never decides.
        assert!(hint(&anyhow::anyhow!("run `devflow init` first")).is_empty());

        // A problem lists every remedy that has a CLI line, in order.
        let problem = remedy::Problem::new("the folder is gone")
            .with(Remedy::Retry)
            .with(Remedy::ForgetProject {
                root: "/gone".into(),
            })
            .into_error();
        let shown = hint(&problem);
        assert_eq!(shown.len(), 1);
        assert!(
            shown[0].contains("devflow project forget /gone"),
            "{shown:?}"
        );
    }
}
