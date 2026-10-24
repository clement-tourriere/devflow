//! Workspace commands: switch, list, path, setup, remove, hook.
use super::output::{self, Palette, Printer, note, say};
use super::{Ctx, EXIT_ATTENTION, ProjectInfo, shell, trust};
use anyhow::{Result, anyhow, bail};
use clap::{Args, Subcommand};
use devflow_core::config::GitHookMode;
use devflow_core::db::DataSource;
use devflow_core::error::{Busy, PartialRemoval, Protected};
use devflow_core::health::{self, ProjectProblem};
use devflow_core::pitchfork::Pitchfork;
use devflow_core::receipt::{Phase, PhaseState, Receipt, Status};
use devflow_core::remedy::{Problem, Remedy};
use devflow_core::services::workspace as container_services;
use devflow_core::setup::{
    self, CreateRequest, RemoveOptions, RemoveOutcome, ServiceRun, SetupOptions, SetupOutcome,
    WorkspaceStatus,
};
use devflow_core::wide::{self, Wide};
use devflow_core::worktree::{self, Workspace};
use devflow_core::{Project, githook, redact, registry};
use serde::Serialize;
use serde_json::json;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Args)]
pub struct SwitchArgs {
    /// Workspace (branch) name; `-` for the previous workspace of this shell;
    /// without a name, pick one from a list (at a terminal)
    #[arg(add = super::complete::workspace())]
    pub name: Option<String>,
    /// Create the workspace
    #[arg(short = 'c', long, requires = "name")]
    pub create: bool,
    /// Code origin for a new workspace: where payloads and data come from (default: the default workspace)
    #[arg(long, value_name = "WS", requires = "create", add = super::complete::workspace())]
    pub from: Option<String>,
    /// Where a new branch starts: a branch, tag, commit or remote branch such as origin/NAME, which NAME then tracks (default: the remote branch of that name if there is one, else the HEAD of --from or of the default workspace)
    #[arg(long, value_name = "REF", requires = "create")]
    pub base: Option<String>,
    /// Data origin: a workspace, snapshot:NAME[@vN], branch:ID or empty
    #[arg(long, value_name = "SRC")]
    pub data_from: Option<DataSource>,
    /// Do not create or attach a database
    #[arg(long)]
    pub no_db: bool,
    /// Do not run hooks
    #[arg(long)]
    pub no_hooks: bool,
    /// Run setup hooks without checking that mise hands them this workspace's variables (recorded in the receipt)
    #[arg(long)]
    pub skip_mise_check: bool,
}

#[derive(Args)]
pub struct ListArgs {
    /// List every registered project instead of the current one
    #[arg(long)]
    pub all: bool,
    /// Add what costs more to find: uncommitted changes, distance from the default
    /// branch's upstream, merge evidence, last activity, port, containers and
    /// daemons (asks Docker and pitchfork)
    #[arg(long, conflicts_with = "all")]
    pub wide: bool,
}

#[derive(Args)]
pub struct PathArgs {
    /// Workspace name, branch or path
    #[arg(add = super::complete::workspace())]
    pub name: String,
}

#[derive(Args)]
pub struct SetupArgs {
    /// Do nothing when the worktree is already set up (status `ready`); resume any other receipt
    #[arg(long)]
    pub if_new: bool,
    /// Give a forked worktree a fresh identity and its own database branch
    #[arg(long)]
    pub new_instance: bool,
    /// Attach to an existing database instead of provisioning one
    #[arg(long, value_name = "URL", conflicts_with = "data_from")]
    pub database_url: Option<String>,
    /// Data origin: a workspace, snapshot:NAME[@vN], branch:ID or empty
    #[arg(long, value_name = "SRC")]
    pub data_from: Option<DataSource>,
    /// Run setup hooks without checking that mise hands them this workspace's variables (recorded in the receipt)
    #[arg(long)]
    pub skip_mise_check: bool,
    /// Entry point of the post-checkout Git hook: never prompts and never fails
    #[arg(long, hide = true)]
    pub from_git_hook: bool,
    /// The background run the Git hook started (`[integrations] git_hook = "background"`): setup, with nobody waiting for it
    #[arg(long, hide = true, requires = "from_git_hook")]
    pub detached: bool,
    /// When another run is setting this worktree up, wait for it and report how it ended (0 ready, 2 not ready or still running at --timeout)
    #[arg(long, conflicts_with = "from_git_hook")]
    pub wait: bool,
    /// With --wait: give up waiting after this long (for example 90s or 10m; default 15m)
    #[arg(
        long,
        value_name = "DURATION",
        value_parser = devflow_core::wait::parse_timeout,
        requires = "wait"
    )]
    pub timeout: Option<std::time::Duration>,
}

#[derive(Args)]
pub struct RemoveArgs {
    /// Workspace name, branch or path
    #[arg(add = super::complete::workspace())]
    pub name: String,
    /// Remove dirty or locked worktrees and continue past failing teardown hooks
    #[arg(long)]
    pub force: bool,
    /// Keep the workspace's database branch
    #[arg(long)]
    pub keep_data: bool,
    /// Also delete the Git branch
    #[arg(long)]
    pub delete_branch: bool,
}

#[derive(Subcommand)]
pub enum HookAction {
    /// Install the post-checkout hook (a hook another tool wrote is left alone)
    Install,
    /// Remove the post-checkout hook (only if devflow wrote it)
    Uninstall,
}

impl HookAction {
    pub fn json_name(&self) -> &'static str {
        match self {
            HookAction::Install => "hook install",
            HookAction::Uninstall => "hook uninstall",
        }
    }
}

// --- switch -------------------------------------------------------------

/// Where `switch` goes: the project, the workspace as `find` takes it, and the
/// exact directory to land in when the person named one (`switch -` returns to
/// the directory this shell left, not to a workspace's root).
struct Target {
    project: Project,
    name: String,
    land_in: Option<PathBuf>,
}

fn switch_target(ctx: &Ctx, args: &SwitchArgs) -> Result<Target> {
    match args.name.as_deref() {
        Some("-") if !args.create => previous_target(),
        Some(name) => Ok(Target {
            project: ctx.project()?,
            name: name.to_string(),
            land_in: None,
        }),
        None => {
            let project = ctx.project()?;
            let name = pick(ctx, &project)?;
            Ok(Target {
                project,
                name,
                land_in: None,
            })
        }
    }
}

/// `switch -`: the workspace this shell was in before its last `devflow
/// switch`, found from the directory the shell wrapper remembers.
fn previous_target() -> Result<Target> {
    if !shell::wrapped() {
        return Err(Problem::new(
            "going back to the previous workspace needs the shell integration: it is the \
             wrapper that remembers where this shell came from",
        )
        .with(shell::remedy())
        .into_error());
    }
    let Some(previous) = shell::previous() else {
        bail!(
            "this shell has not changed workspace yet, so there is no previous one to go back to"
        );
    };
    let directory = previous.canonicalize().map_err(|_| {
        anyhow!(
            "the previous directory {} no longer exists",
            previous.display()
        )
    })?;
    let project = Project::discover(&directory).map_err(|error| {
        error.context(format!(
            "the previous directory {} is not in a devflow project",
            directory.display()
        ))
    })?;
    let workspace = worktree::containing(&project, &directory)?.ok_or_else(|| {
        anyhow!(
            "the previous directory {} is not inside a workspace of {}",
            directory.display(),
            project.name()
        )
    })?;
    Ok(Target {
        name: workspace.path.display().to_string(),
        project,
        land_in: Some(directory),
    })
}

/// Choose a workspace from a list: the ones entered most recently first, with
/// their branch, status and where they drifted. Needs a terminal.
fn pick(ctx: &Ctx, project: &Project) -> Result<String> {
    if !ctx.interactive() {
        return Err(Problem::new(
            "a workspace name is required: there is no terminal to pick one from",
        )
        .with(Remedy::ChooseWorkspace {
            root: project.root.clone(),
        })
        .into_error());
    }
    let mut items = setup::list(project)?;
    items.retain(|item| !item.workspace.missing);
    setup::recent_first(&mut items);
    let current = current_path(ctx, &items);
    let now = chrono::Utc::now();
    let width = items
        .iter()
        .map(|item| item.workspace.name.chars().count())
        .max()
        .unwrap_or(0);
    let branch_width = items
        .iter()
        .map(|item| branch_label(item).chars().count())
        .max()
        .unwrap_or(0);
    let choices: Vec<Choice> = items
        .iter()
        .map(|item| {
            let marker = if current == Some(&item.workspace.path) {
                "*"
            } else {
                " "
            };
            let active = item
                .receipt
                .as_ref()
                .and_then(|receipt| receipt.last_active_at)
                .map(|at| output::ago(at, now))
                .unwrap_or_default();
            let mut label = format!(
                "{marker} {:<width$}  {:<branch_width$}  {}",
                item.workspace.name,
                branch_label(item),
                status_label(item),
            );
            if let Some(drift) = &item.drift {
                label.push_str(&format!("  ({})", drift.describe()));
            }
            if !active.is_empty() {
                label.push_str(&format!("  {active}"));
            }
            Choice {
                label,
                path: item.workspace.path.display().to_string(),
            }
        })
        .collect();
    anyhow::ensure!(!choices.is_empty(), "this project has no workspaces");
    // Enter switches to the workspace entered last before this one.
    let start = choices
        .iter()
        .position(|choice| Some(choice.path.as_str()) != current.map(|p| p.to_str().unwrap_or("")))
        .unwrap_or(0);
    match inquire::Select::new("Switch to", choices)
        .with_starting_cursor(start)
        .with_page_size(15)
        .prompt()
    {
        Ok(choice) => Ok(choice.path),
        Err(
            inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted,
        ) => bail!("cancelled"),
        Err(error) => Err(error.into()),
    }
}

struct Choice {
    label: String,
    path: String,
}

impl std::fmt::Display for Choice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

fn branch_label(item: &WorkspaceStatus) -> String {
    item.workspace
        .branch
        .clone()
        .unwrap_or_else(|| "(detached)".into())
}

pub fn switch(ctx: &Ctx, args: SwitchArgs) -> Result<ExitCode> {
    let Target {
        project,
        name,
        land_in,
    } = switch_target(ctx, &args)?;
    ctx.register(&project);
    let mut options = SetupOptions {
        data_from: args.data_from,
        no_db: args.no_db,
        no_hooks: args.no_hooks,
        skip_mise_check: args.skip_mise_check,
        non_interactive: !ctx.interactive(),
        ..SetupOptions::default()
    };
    let mut printer =
        Printer::new(ctx).announcing_database(!project.config.services.is_empty() && !args.no_db);
    let (outcome, created) = if args.create {
        // A workspace that does not exist yet is judged by the one it is made from.
        let source = trust::target_of(&project, args.from.as_deref())?;
        let relevant = trust::governs_setup(&project, !args.no_hooks)
            || (!args.no_hooks && !project.config.hooks.switch.is_empty());
        options.approval = trust::resolve(ctx, &project, &source, relevant)?;
        let mut request = CreateRequest {
            name,
            from: args.from,
            base: args.base,
            options: options.clone(),
        };
        let mut outcome = match setup::create_and_activate(&project, request.clone(), &mut printer)
        {
            Ok(outcome) => outcome,
            Err(error) => return in_progress(ctx, "switch", &project, None, error, None),
        };
        // The review was of the workspace this one is made from. An existing
        // branch can hold other hooks, [env] or pitchfork files: core ran none
        // of it on that approval. Ask again about what the new workspace
        // actually holds, then resume.
        if outcome.approval_differs && relevant && ctx.interactive() {
            note(format!(
                "{} holds a different configuration than the workspace it was made from.",
                outcome.workspace.name
            ));
            options.approval = trust::resolve(ctx, &project, &outcome.workspace.path, true)?;
            if options.approval.is_some() {
                request.options = options.clone();
                outcome = match setup::create_and_activate(&project, request, &mut printer) {
                    Ok(outcome) => outcome,
                    Err(error) => return in_progress(ctx, "switch", &project, None, error, None),
                };
            }
        }
        (outcome, true)
    } else {
        let workspace = worktree::find(&project, &name)?.ok_or_else(|| {
            anyhow!(
                "workspace '{0}' does not exist; create it with `devflow switch -c {0}`",
                name
            )
        })?;
        let relevant = switch_needs_trust(&project, &workspace, args.no_hooks);
        options.approval = trust::resolve(ctx, &project, &workspace.path, relevant)?;
        match setup::activate(&project, &workspace, options, &mut printer) {
            Ok(outcome) => (outcome, false),
            Err(error) => {
                return in_progress(
                    ctx,
                    "switch",
                    &project,
                    Some(&workspace),
                    error,
                    land_in.as_deref(),
                );
            }
        }
    };
    // Setup, the switch hooks and `[pitchfork] autostart` all ran in core; the
    // outcome reports each apart from readiness, which alone sets the exit code.
    let code = report(ctx, "switch", &project, &outcome, created)?;
    if !ctx.json {
        hand_off(&landing(
            ctx,
            &project,
            &outcome.workspace,
            land_in.as_deref(),
        ));
    }
    Ok(code)
}

/// Where the shell lands in `target`: the directory `switch -` returns to, else
/// the subdirectory the caller is in (`apps/web`) when `target` has it, else
/// the workspace's root.
fn landing(ctx: &Ctx, project: &Project, target: &Workspace, exact: Option<&Path>) -> PathBuf {
    if let Some(exact) = exact.filter(|dir| dir.starts_with(&target.path)) {
        return exact.to_path_buf();
    }
    let here = worktree::containing(project, &ctx.cwd).ok().flatten();
    worktree::landing(&ctx.cwd, here.as_ref(), target)
}

/// `Busy` is not a failure: another devflow process is setting the workspace
/// up. Report it as `in_progress` (exit 2, the same as an outcome in that
/// state) instead of an error; any other error is passed on.
fn in_progress(
    ctx: &Ctx,
    command: &str,
    project: &Project,
    workspace: Option<&Workspace>,
    error: anyhow::Error,
    exact: Option<&Path>,
) -> Result<ExitCode> {
    let Some(busy) = error.downcast_ref::<Busy>() else {
        return Err(error);
    };
    let found;
    let workspace = match workspace {
        Some(workspace) => Some(workspace),
        None => {
            found = worktree::find(project, &busy.workspace).ok().flatten();
            found.as_ref()
        }
    };
    if ctx.json {
        let body = json!({
            "project": ProjectInfo::of(project),
            "created": false,
            "skipped": false,
            "status": Status::InProgress,
            "workspace": workspace,
            "path": workspace.map(|workspace| &workspace.path),
            "detail": busy.to_string(),
        });
        output::emit(ctx, command, &body)?;
    } else {
        note(still_running(Palette::stderr(), &busy.workspace));
        if command == "switch"
            && let Some(workspace) = workspace
        {
            hand_off(&landing(ctx, project, workspace, exact));
        }
    }
    Ok(ExitCode::from(EXIT_ATTENTION))
}

fn still_running(palette: Palette, workspace: &str) -> String {
    format!(
        "{} {workspace} is still being set up by another devflow process; check `devflow list`",
        palette.yellow("…")
    )
}

/// Whether switching into `workspace` can run something an approval governs,
/// so a trust prompt is worth showing: switch hooks exist, or setup is not
/// finished and would run setup hooks or apply `[env]` recipes.
fn switch_needs_trust(project: &Project, workspace: &Workspace, no_hooks: bool) -> bool {
    let setup_runs = match setup::receipt(project, workspace) {
        Ok(Some(receipt)) => receipt.status() != Status::Ready,
        _ => true,
    };
    let hooks = !no_hooks
        && (!project.config.hooks.switch.is_empty()
            || (setup_runs && !project.config.hooks.setup.is_empty()));
    hooks || (setup_runs && !project.config.env.is_empty())
}

/// Move the caller's shell into `path`: through the wrapper when installed,
/// otherwise print the path on stdout so `cd "$(devflow switch x)"` works.
fn hand_off(path: &Path) {
    if shell::request_cd(path) {
        return;
    }
    say(path.display().to_string());
    if std::io::stdout().is_terminal() {
        note(shell::manual_hint());
    }
}

// --- list / path --------------------------------------------------------

pub fn list(ctx: &Ctx, args: ListArgs) -> Result<ExitCode> {
    if args.all {
        return list_all(ctx);
    }
    let project = ctx.project()?;
    // Workspaces Git or an agent host removed, which still own data: from the
    // ledger's records alone, never a server or Docker.
    let gone = devflow_core::gc::summary(&project)
        .ok()
        .filter(|summary| summary.count() > 0);
    let cleanup = |body: &mut serde_json::Value| {
        if let Some(gone) = &gone {
            body["cleanup"] = json!({
                "removed_elsewhere": gone.removed_elsewhere,
                "missing": gone.missing,
                "message": gone.sentence(),
                "remedies": devflow_core::remedy::describe(&[Remedy::CleanUp {
                    root: project.root.clone(),
                }]),
            });
        }
    };
    let note_cleanup = || {
        if let (Some(sentence), Some(hint)) = (
            gone.as_ref().and_then(|gone| gone.sentence()),
            Remedy::CleanUp {
                root: project.root.clone(),
            }
            .hint(),
        ) {
            note(format!("{sentence}; {hint}"));
        }
    };
    if args.wide {
        let items = wide::list(&project)?;
        if ctx.json {
            let mut body = json!({ "project": ProjectInfo::of(&project), "workspaces": items });
            cleanup(&mut body);
            output::emit(ctx, "list", &body)?;
        } else {
            let rows: Vec<Row> = items
                .iter()
                .map(|item| (&item.status, Some(&item.wide)))
                .collect();
            say(workspace_table(ctx, &rows));
            note_cleanup();
        }
        return Ok(ExitCode::SUCCESS);
    }
    let items = setup::list(&project)?;
    if ctx.json {
        let mut body = json!({ "project": ProjectInfo::of(&project), "workspaces": items });
        cleanup(&mut body);
        output::emit(ctx, "list", &body)?;
    } else {
        say(workspace_table(ctx, &plain(&items)));
        note_cleanup();
    }
    Ok(ExitCode::SUCCESS)
}

/// A listed workspace and, in the wide listing, its extra columns.
type Row<'a> = (&'a WorkspaceStatus, Option<&'a Wide>);

fn plain(items: &[WorkspaceStatus]) -> Vec<Row<'_>> {
    items.iter().map(|item| (item, None)).collect()
}

/// The checkout containing the caller's directory (deepest path wins).
fn current_path<'a>(ctx: &Ctx, items: &'a [WorkspaceStatus]) -> Option<&'a PathBuf> {
    items
        .iter()
        .filter(|item| ctx.cwd.starts_with(&item.workspace.path))
        .max_by_key(|item| item.workspace.path.components().count())
        .map(|item| &item.workspace.path)
}

fn list_all(ctx: &Ctx) -> Result<ExitCode> {
    #[derive(Serialize)]
    struct Row {
        name: String,
        id: uuid::Uuid,
        root: std::path::PathBuf,
        #[serde(skip_serializing_if = "Option::is_none")]
        workspaces: Option<Vec<WorkspaceStatus>>,
        /// Why the project cannot be opened, with what to do about it.
        #[serde(skip_serializing_if = "Option::is_none")]
        problem: Option<ProjectProblem>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    }
    let rows: Vec<Row> = registry::list()?
        .into_iter()
        .map(|entry| {
            // A project in trouble is classified from the disk first, so the
            // row says what is wrong and what to do instead of a parser's text.
            let problem = health::check(&entry).problem();
            let listed = match &problem {
                Some(_) => None,
                None => Some(Project::open(&entry.root).and_then(|project| setup::list(&project))),
            };
            let (workspaces, error) = match listed {
                Some(Ok(items)) => (Some(items), None),
                Some(Err(error)) => (None, Some(format!("{error:#}"))),
                None => (None, problem.as_ref().map(|p| p.message.clone())),
            };
            Row {
                name: entry.name,
                id: entry.id,
                root: entry.root,
                workspaces,
                problem,
                error,
            }
        })
        .collect();
    if ctx.json {
        output::emit(ctx, "list", &json!({ "projects": rows }))?;
        return Ok(ExitCode::SUCCESS);
    }
    if rows.is_empty() {
        note("No registered projects; run `devflow init` or `devflow switch` inside a repository.");
    }
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            say("");
        }
        say(format!("{}  {}", row.name, output::tilde(&row.root)));
        match (&row.workspaces, &row.error) {
            (Some(items), _) => say(workspace_table(ctx, &plain(items))),
            (None, Some(error)) => say(format!("  ! {}", redact::text(error))),
            (None, None) => {}
        }
        if let Some(problem) = &row.problem {
            for hint in problem.remedies.iter().filter_map(Remedy::hint) {
                say(format!("    hint: {}", redact::text(&hint)));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn workspace_table(ctx: &Ctx, listed: &[Row<'_>]) -> String {
    let wide = listed.iter().any(|(_, wide)| wide.is_some());
    // The workspace containing the caller's directory (deepest path wins).
    let current = listed
        .iter()
        .map(|(item, _)| &item.workspace.path)
        .filter(|path| ctx.cwd.starts_with(path))
        .max_by_key(|path| path.components().count());
    let now = chrono::Utc::now();
    let rows: Vec<Vec<String>> = listed
        .iter()
        .map(|(item, wide)| {
            let workspace = &item.workspace;
            let marker = if current == Some(&workspace.path) {
                "*"
            } else {
                " "
            };
            let database =
                item.receipt.as_ref().map_or("—".to_string(), |receipt| {
                    match receipt.first_postgres() {
                        Some(binding) => binding.database.clone(),
                        None if receipt.unmapped.is_some() => "(unmapped)".to_string(),
                        None => "—".to_string(),
                    }
                });
            let mut cells = vec![
                format!("{marker} {}", workspace.name),
                workspace
                    .branch
                    .clone()
                    .unwrap_or_else(|| "(detached)".into()),
                status_label(item),
                database,
            ];
            if let Some(wide) = wide {
                cells.extend(wide_cells(wide, now));
            }
            cells.push(output::tilde(&workspace.path));
            cells
        })
        .collect();
    let mut headers = vec!["  NAME", "BRANCH", "STATUS", "DATABASE"];
    if wide {
        headers.extend(WIDE_HEADERS);
    }
    headers.push("PATH");
    let mut text = output::table(&headers, &rows);
    // A checkout on another branch than the workspace was created for, and a
    // derived file that went missing: said under the table, not in a column.
    for (item, wide) in listed {
        let notes = [
            item.drift.as_ref().map(|drift| drift.describe()),
            item.env_file.map(|problem| problem.describe().to_string()),
        ];
        for note in notes.into_iter().flatten() {
            text.push_str(&format!("\n  ! {}: {note}", item.workspace.name));
        }
        if let Some(wide) = wide
            && let Some(why) = &wide.daemons_unobserved
        {
            text.push_str(&format!(
                "\n  ? {}: what runs was not observed: {}",
                item.workspace.name,
                redact::text(why)
            ));
        }
        // The URLs of the daemons that have one: too long for a column.
        if let Some(wide) = wide {
            for daemon in &wide.daemons {
                if let Some(url) = &daemon.url {
                    text.push_str(&format!(
                        "\n  {}: {} {url}",
                        item.workspace.name, daemon.name
                    ));
                }
            }
        }
    }
    text
}

const WIDE_HEADERS: [&str; 7] = [
    "PORT", "CHANGES", "UPSTREAM", "MERGED", "ACTIVE", "SERVICES", "DAEMONS",
];

fn wide_cells(wide: &Wide, now: chrono::DateTime<chrono::Utc>) -> Vec<String> {
    let dash = || "—".to_string();
    let list = |cells: Vec<String>| {
        if cells.is_empty() {
            dash()
        } else {
            cells.join(" ")
        }
    };
    vec![
        wide.app_port.map_or_else(dash, |port| port.to_string()),
        match wide.dirty {
            Some(true) => "dirty".to_string(),
            Some(false) => "clean".to_string(),
            None => "?".to_string(),
        },
        wide.ahead_behind.as_ref().map_or_else(dash, |ab| {
            format!("↑{} ↓{} {}", ab.ahead, ab.behind, ab.against)
        }),
        wide.merged
            .map_or_else(dash, |merged| merged.describe().to_string()),
        wide.last_active_at
            .map_or_else(dash, |at| output::ago(at, now)),
        if wide.services_error.is_some() {
            "?".to_string()
        } else {
            list(
                wide.services
                    .iter()
                    .map(|service| format!("{}:{}", service.key, service_state(service)))
                    .collect(),
            )
        },
        {
            let shown = list(
                wide.daemons
                    .iter()
                    .map(|daemon| {
                        let ports: Vec<String> =
                            daemon.ports.iter().map(|port| format!(":{port}")).collect();
                        format!("{}:{}{}", daemon.name, daemon.state, ports.join(""))
                    })
                    .collect(),
            );
            // What runs could not be observed: say so, never "none".
            if wide.daemons_unobserved.is_some() {
                format!("? {shown}").replace("? —", "?")
            } else {
                shown
            }
        },
    ]
}

fn service_state(service: &container_services::View) -> &'static str {
    use container_services::Run;
    match service.state {
        Run::Running => "running",
        Run::Stopped => "stopped",
        Run::Missing => "missing",
        Run::Unknown => "?",
    }
}

fn status_label(item: &WorkspaceStatus) -> String {
    if item.workspace.missing {
        "missing"
    } else if item.busy {
        "busy"
    } else {
        match item.status {
            None => "not set up",
            Some(Status::Ready) => {
                // Ready, but nobody checked what mise hands the setup hooks.
                let skipped = item
                    .receipt
                    .as_ref()
                    .is_some_and(|r| r.mise_check_skipped_at.is_some());
                if skipped {
                    "ready (mise check skipped)"
                } else {
                    "ready"
                }
            }
            Some(Status::NeedsAttention) => "needs attention",
            Some(Status::InProgress) => "in progress",
        }
    }
    .to_string()
}

pub fn path(ctx: &Ctx, args: PathArgs) -> Result<ExitCode> {
    let project = ctx.project()?;
    let workspace = find(&project, &args.name)?;
    if ctx.json {
        let body = json!({ "workspace": workspace, "path": workspace.path });
        output::emit(ctx, "path", &body)?;
    } else {
        say(workspace.path.display().to_string());
    }
    Ok(ExitCode::SUCCESS)
}

fn find(project: &Project, name: &str) -> Result<Workspace> {
    worktree::find(project, name)?
        .ok_or_else(|| anyhow!("no workspace named '{name}'; see `devflow list`"))
}

// --- setup --------------------------------------------------------------

pub fn setup(ctx: &Ctx, args: SetupArgs) -> Result<ExitCode> {
    if args.from_git_hook {
        // The hook entry point reports in one line on stderr and always exits
        // 0, which cannot carry a JSON document: refuse before doing anything.
        anyhow::ensure!(!ctx.json, "--from-git-hook does not support --json");
        return Ok(setup_from_git_hook(ctx, args));
    }
    let project = ctx.project()?;
    ctx.register(&project);
    let workspace = current_workspace(ctx, &project)?;
    let provisions = project.config.containers().next().is_some()
        || (project.config.postgres().is_some() && args.database_url.is_none());
    let options = SetupOptions {
        data_from: args.data_from,
        database_url: args.database_url,
        approval: trust::resolve(
            ctx,
            &project,
            &workspace.path,
            trust::governs_setup(&project, true),
        )?,
        skip_mise_check: args.skip_mise_check,
        non_interactive: !ctx.interactive(),
        ..SetupOptions::default()
    };
    let mut printer = Printer::new(ctx).announcing_database(provisions);
    let outcome = match setup::setup(
        &project,
        &workspace,
        options,
        args.if_new,
        args.new_instance,
        &mut printer,
    ) {
        Ok(outcome) => outcome,
        // Another run holds the workspace: `--wait` waits for it instead of
        // reporting it as running, and reports how it ended.
        Err(error) if args.wait && error.downcast_ref::<Busy>().is_some() => {
            let timeout = args.timeout.unwrap_or(devflow_core::wait::DEFAULT_TIMEOUT);
            let waited = super::wait::wait_for(ctx, &project, &workspace, timeout)?;
            return super::wait::report(ctx, "setup", &project, &waited);
        }
        Err(error) => return in_progress(ctx, "setup", &project, Some(&workspace), error, None),
    };
    match outcome {
        Some(mut outcome) => {
            // Process state is reported apart from readiness: a start that
            // fails or is refused never changes the status or the exit code.
            setup::autostart(&project, &mut outcome, Pitchfork::locate().as_ref());
            report(ctx, "setup", &project, &outcome, false)
        }
        None => {
            if ctx.json {
                let body = json!({
                    "project": ProjectInfo::of(&project),
                    "skipped": true,
                    "workspace": workspace,
                    "path": workspace.path,
                });
                output::emit(ctx, "setup", &body)?;
            } else {
                note(format!(
                    "{} is already set up; nothing to do",
                    workspace.name
                ));
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// The post-checkout hook must never fail Git or prompt: report in one line.
fn setup_from_git_hook(ctx: &Ctx, args: SetupArgs) -> ExitCode {
    if std::env::var_os("DEVFLOW_SETUP_ACTIVE").is_some_and(|v| v == "1") {
        return ExitCode::SUCCESS;
    }
    let result = (|| -> Result<Option<SetupOutcome>> {
        let project = ctx.project()?;
        ctx.register(&project);
        let workspace = current_workspace(ctx, &project)?;
        // `[integrations] git_hook = "background"`: Git does not wait for the
        // setup. Start it detached and return; whoever needs the workspace
        // waits for it (`devflow wait`). A worktree that is already set up
        // starts nothing.
        if !args.detached
            && project.config.integrations.git_hook == GitHookMode::Background
            && args.data_from.is_none()
            && args.database_url.is_none()
            && !args.new_instance
            && start_in_background(&project, &workspace, args.if_new)
        {
            return Ok(None);
        }
        let options = SetupOptions {
            data_from: args.data_from,
            database_url: args.database_url,
            // Git runs the hook: nobody can be asked.
            non_interactive: true,
            ..SetupOptions::default()
        };
        setup::setup(
            &project,
            &workspace,
            options,
            args.if_new,
            args.new_instance,
            &mut Printer::new(ctx).announcing_database(!project.config.services.is_empty()),
        )
    })();
    match result {
        Ok(Some(outcome)) if outcome.status != Status::Ready => note(format!(
            "devflow: {} needs attention; run `devflow setup` in {} to review",
            outcome.workspace.name,
            outcome.workspace.path.display()
        )),
        Ok(_) => {}
        // Another run is already doing this work: nothing to report to Git.
        Err(error) if error.downcast_ref::<Busy>().is_some() => {}
        Err(error) => note(format!(
            "devflow: setup did not complete ({}); run `devflow setup` to retry",
            redact::text(&format!("{error:#}"))
        )),
    }
    ExitCode::SUCCESS
}

/// Start the hook's setup detached; `true` when the hook has nothing left to
/// do (it started, or the worktree is ready already), `false` to set up here
/// instead.
fn start_in_background(project: &Project, workspace: &Workspace, if_new: bool) -> bool {
    if if_new
        && matches!(
            setup::receipt(project, workspace),
            Ok(Some(receipt)) if receipt.status() == Status::Ready
        )
    {
        return true;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let mut args = vec!["setup", "--from-git-hook", "--detached"];
    if if_new {
        args.push("--if-new");
    }
    match githook::spawn_background(&exe, &args, &workspace.path, &workspace.git_dir) {
        Ok(started) => {
            note(format!(
                "devflow: setting up {} in the background; its log is {}",
                workspace.name,
                started.log.display()
            ));
            true
        }
        Err(error) => {
            note(format!(
                "devflow: cannot set up in the background ({}); setting it up now",
                redact::text(&format!("{error:#}"))
            ));
            false
        }
    }
}

fn current_workspace(ctx: &Ctx, project: &Project) -> Result<Workspace> {
    worktree::containing(project, &ctx.cwd)?.ok_or_else(|| {
        anyhow!(
            "{} is not inside a workspace of {}",
            ctx.cwd.display(),
            project.name()
        )
    })
}

// --- shared outcome rendering --------------------------------------------

/// Print a setup outcome (JSON document or human summary) and map its status
/// to the exit code: 0 ready, 2 when the workspace needs attention.
fn report(
    ctx: &Ctx,
    command: &str,
    project: &Project,
    outcome: &SetupOutcome,
    created: bool,
) -> Result<ExitCode> {
    if ctx.json {
        #[derive(Serialize)]
        struct Doc<'a> {
            project: ProjectInfo<'a>,
            created: bool,
            skipped: bool,
            path: &'a Path,
            /// `hooks` is the setup run's, `switch_hooks` the hooks that ran
            /// on entering the workspace, `processes` what `[pitchfork]
            /// autostart` did, `services_state` how each container service
            /// stands: all apart from `status`.
            #[serde(flatten)]
            outcome: &'a SetupOutcome,
        }
        let doc = Doc {
            project: ProjectInfo::of(project),
            created,
            skipped: false,
            path: &outcome.workspace.path,
            outcome,
        };
        output::emit(ctx, command, &doc)?;
    } else {
        print_summary(outcome);
        if let Some(autostart) = &outcome.autostart {
            for line in super::processes::autostart_lines(autostart, Palette::stderr()) {
                note(line);
            }
        }
    }
    Ok(match outcome.status {
        Status::Ready => ExitCode::SUCCESS,
        Status::NeedsAttention | Status::InProgress => ExitCode::from(EXIT_ATTENTION),
    })
}

fn print_summary(outcome: &SetupOutcome) {
    for line in summary_lines(outcome, Palette::stderr()) {
        note(line);
    }
}

fn summary_lines(outcome: &SetupOutcome, palette: Palette) -> Vec<String> {
    let workspace = &outcome.workspace;
    let mut lines: Vec<String> = outcome
        .warnings
        .iter()
        .map(|warning| format!("warning: {}", redact::text(warning)))
        .collect();
    // What was repaired, and the branch the checkout holds when it is not the
    // one the workspace was created for.
    lines.extend(outcome.repaired.iter().map(|repair| {
        format!(
            "{} {:<11} {}",
            palette.green("✓"),
            "repaired",
            repair.describe()
        )
    }));
    if let Some(drift) = &outcome.drift {
        lines.push(format!(
            "{} {}: {}",
            palette.yellow("!"),
            workspace.name,
            drift.describe()
        ));
    }
    // The services that had to be started, and the ones that could not.
    for service in &outcome.services_state {
        match service.state {
            ServiceRun::Started => lines.push(format!(
                "{} {:<11} {} started",
                palette.green("✓"),
                "services",
                service.key
            )),
            ServiceRun::Failed => lines.extend(
                outcome
                    .remedies
                    .iter()
                    .filter(|remedy| {
                        matches!(remedy, Remedy::StartService { key, .. } if *key == service.key)
                    })
                    .filter_map(Remedy::hint)
                    .map(|hint| format!("  hint: {}", redact::text(&hint))),
            ),
            ServiceRun::Running => {}
        }
    }
    match outcome.status {
        Status::Ready => {
            lines.push(format!(
                "{} {} is ready",
                palette.green("✓"),
                workspace.name
            ));
            if outcome.receipt.mise_check_skipped_at.is_some() {
                lines.push(
                    "  The setup hooks ran with --skip-mise-check: nobody checked that mise \
                     hands them this workspace's variables."
                        .to_string(),
                );
            }
        }
        Status::InProgress => lines.push(still_running(palette, &workspace.name)),
        Status::NeedsAttention => {
            lines.push(format!(
                "{} {} needs attention",
                palette.yellow("!"),
                workspace.name
            ));
            let needs_trust = outcome
                .remedies
                .iter()
                .any(|r| matches!(r, Remedy::Trust { .. }));
            let mut failed = false;
            for phase in Phase::ALL {
                let record = outcome.receipt.phase(phase);
                let skipped = record.state == PhaseState::Skipped && record.required_skip;
                if record.state != PhaseState::Failed && !skipped {
                    continue;
                }
                failed |= record.state == PhaseState::Failed;
                let detail = record.detail.as_deref().unwrap_or("no details recorded");
                lines.push(format!("  {}: {}", phase.as_str(), redact::text(detail)));
            }
            if needs_trust {
                lines.push(
                    "  Review what this project runs with `devflow trust` (or set DEVFLOW_TRUST_HOOKS=1), then run `devflow setup` here to finish."
                        .to_string(),
                );
            }
            if failed {
                lines.push(format!(
                    "  Logs: {}. Fix the problem, then run `devflow setup` in {} to resume.",
                    Receipt::logs_dir(&workspace.git_dir).display(),
                    workspace.path.display()
                ));
            }
            if !failed && !needs_trust {
                // Phases reopened by a configuration change.
                lines.push(format!(
                    "  Run `devflow setup` in {} to apply the change.",
                    workspace.path.display()
                ));
            }
        }
    }
    lines
}

// --- remove -------------------------------------------------------------

pub fn remove(ctx: &Ctx, args: RemoveArgs) -> Result<ExitCode> {
    let project = ctx.project()?;
    let mut options = RemoveOptions {
        force: args.force,
        keep_data: args.keep_data,
        delete_branch: args.delete_branch,
        approval: trust::resolve(
            ctx,
            &project,
            &trust::target_of(&project, Some(&args.name))?,
            !project.config.hooks.teardown.is_empty(),
        )?,
        cwd: Some(ctx.cwd.clone()),
        pitchfork: None,
        // The user named the workspace on the command line just now.
        expect: None,
        ..RemoveOptions::default()
    };
    // A hint to run this again with `--force` repeats what decides what is
    // removed: left out, the forced run would remove the worktree and keep
    // the branch or the data the person asked to delete.
    let mut repeated = Vec::new();
    if args.delete_branch {
        repeated.push("--delete-branch");
    }
    if args.keep_data {
        repeated.push("--keep-data");
    }
    ctx.forced_rerun_keeps(&repeated);
    let mut printer = Printer::new(ctx);
    let outcome = match setup::remove(&project, &args.name, options.clone(), &mut printer) {
        Ok(outcome) => outcome,
        // Core refused before any effect. A person at the terminal may override
        // a protection `--force` can override; everyone else gets the error and
        // its hint.
        Err(error) if !options.force && ctx.interactive() => {
            // (A removal that stopped part-way asks too: forcing goes on past
            // what stopped it, where force can.)
            let forceable = match (
                error.downcast_ref::<Protected>(),
                error.downcast_ref::<PartialRemoval>(),
            ) {
                (Some(protected), _) if protected.forceable => Some(protected.reason.clone()),
                (_, Some(partial)) if partial.forceable => Some(partial.to_string()),
                _ => None,
            };
            match forceable {
                Some(reason) if confirm_forced_removal(&reason, args.delete_branch)? => {
                    options.force = true;
                    setup::remove(&project, &args.name, options, &mut printer)?
                }
                _ => return Err(error),
            }
        }
        Err(error) => return Err(error),
    };
    if ctx.json {
        #[derive(Serialize)]
        struct Doc<'a> {
            project: ProjectInfo<'a>,
            #[serde(flatten)]
            outcome: &'a RemoveOutcome,
        }
        let doc = Doc {
            project: ProjectInfo::of(&project),
            outcome: &outcome,
        };
        output::emit(ctx, "remove", &doc)?;
        return Ok(ExitCode::SUCCESS);
    }
    for warning in &outcome.warnings {
        note(format!("warning: {}", redact::text(warning)));
    }
    // What nothing records any more: the receipt went with the worktree.
    if let Some(may) = &outcome.may_still_run {
        note(format!(
            "warning: pitchfork daemons of this workspace may still be running{}{}",
            may.namespace
                .as_deref()
                .map(|namespace| format!(" under the namespace {namespace}"))
                .unwrap_or_default(),
            if may.ids.is_empty() {
                String::new()
            } else {
                format!(": {}", may.ids.join(", "))
            }
        ));
    }
    let palette = Palette::stdout();
    say(format!(
        "{} Removed {} ({})",
        palette.green("✓"),
        outcome.workspace.name,
        output::tilde(&outcome.workspace.path)
    ));
    if !outcome.stopped_daemons.is_empty() {
        say(format!(
            "  stopped {} pitchfork daemon(s): {}",
            outcome.stopped_daemons.len(),
            outcome.stopped_daemons.join(", ")
        ));
    }
    if let Some(id) = outcome.dropped_branch {
        say(format!("  dropped database branch {id}"));
    } else if args.keep_data {
        say("  kept the database branch");
    }
    if outcome.deleted_branch {
        say("  deleted the Git branch");
    } else if args.delete_branch {
        // (The reason is among the warnings above.)
        say("  kept the Git branch");
    }
    Ok(ExitCode::SUCCESS)
}

/// Ask whether to remove a workspace despite a protection `--force` overrides.
fn confirm_forced_removal(reason: &str, delete_branch: bool) -> Result<bool> {
    let prompt = format!(
        "{}. Remove it{} anyway?",
        reason.trim_end_matches('.'),
        // Saying yes goes on with what was asked, the branch included.
        if delete_branch {
            " and delete its branch"
        } else {
            ""
        }
    );
    match inquire::Confirm::new(&prompt).with_default(false).prompt() {
        Ok(answer) => Ok(answer),
        Err(
            inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted,
        ) => {
            bail!("cancelled")
        }
        Err(error) => Err(error.into()),
    }
}

// --- prune --------------------------------------------------------------

#[derive(Args)]
pub struct PruneArgs {
    /// Workspaces (or Git worktree names) to forget; default: every missing
    /// worktree that owns nothing
    pub names: Vec<String>,
    /// Forget them without asking (without it, `--json` and a non-terminal only list them)
    #[arg(short = 'y', long)]
    pub yes: bool,
}

/// Forget the registrations of worktrees whose directory is gone, as far as
/// core allows: one that still owns data, or whose folder is missing too (an
/// unplugged drive), is listed with why and kept.
pub fn prune(ctx: &Ctx, args: PruneArgs) -> Result<ExitCode> {
    let project = ctx.project()?;
    let candidates = worktree::prune_candidates(&project)?;
    // What the person asked for: the named ones (refused ones included, so
    // that core says why), or everything that can go.
    let chosen: Vec<&worktree::PruneCandidate> = if args.names.is_empty() {
        candidates.iter().filter(|c| c.refusal.is_none()).collect()
    } else {
        let mut chosen = Vec::new();
        for name in &args.names {
            let found = candidates
                .iter()
                .find(|c| c.name == *name || c.id == *name)
                .ok_or_else(|| anyhow!("no worktree called '{name}' has a missing directory"))?;
            chosen.push(found);
        }
        chosen
    };
    let palette = Palette::stdout();
    if !ctx.json {
        if candidates.is_empty() {
            say("No worktree has a missing directory.");
        } else {
            let rows: Vec<Vec<String>> = candidates
                .iter()
                .map(|c| {
                    vec![
                        c.name.clone(),
                        output::tilde(&c.path),
                        match &c.refusal {
                            None => "can be forgotten".to_string(),
                            Some(refusal) => format!("kept: {}", refusal.message()),
                        },
                    ]
                })
                .collect();
            say(output::table(&["WORKSPACE", "WAS AT", "STATE"], &rows));
        }
    }
    let confirmed = if chosen.is_empty() {
        false
    } else if args.yes {
        true
    } else if ctx.interactive() {
        confirm(&format!(
            "Forget the Git registration of {} worktree(s)? Their branches are kept.",
            chosen.len()
        ))?
    } else {
        false
    };
    let targets: Vec<worktree::PruneTarget> = chosen.iter().map(|c| c.target()).collect();
    let outcome = if confirmed {
        worktree::prune(&project, &targets)?
    } else {
        worktree::PruneOutcome::default()
    };
    let root = &project.root;
    let refused: Vec<serde_json::Value> = outcome
        .refused
        .iter()
        .map(|r| {
            json!({
                "name": r.name, "path": r.path, "refusal": r.refusal,
                "message": r.refusal.message(),
                "remedies": devflow_core::remedy::describe(&r.refusal.remedies(root, &r.name)),
            })
        })
        .collect();
    if ctx.json {
        let mut doc = json!({
            "project": ProjectInfo::of(&project),
            "candidates": candidates,
            "confirmed": confirmed,
            "pruned": outcome.pruned,
            "refused": refused,
            "changed": outcome.changed,
        });
        // A run that kept something exits 1, and every failure has an `error`
        // (and the way on as `remedies`) at the top; what was forgotten and
        // what was kept stay in the same document.
        if !outcome.refused.is_empty() {
            let mut remedies: Vec<Remedy> = Vec::new();
            for r in &outcome.refused {
                for remedy in r.refusal.remedies(root, &r.name) {
                    if !remedies.contains(&remedy) {
                        remedies.push(remedy);
                    }
                }
            }
            doc["error"] = serde_json::Value::String(format!(
                "{} kept: {}",
                devflow_core::text::counted_as(
                    outcome.refused.len() as u64,
                    "worktree was",
                    "worktrees were"
                ),
                outcome
                    .refused
                    .iter()
                    .map(|r| format!("{}: {}", r.name, r.refusal.message()))
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
            if let Some(hint) = remedies.iter().find_map(Remedy::hint) {
                doc["hint"] = serde_json::Value::String(hint);
            }
            doc["remedies"] = serde_json::to_value(devflow_core::remedy::describe(&remedies))?;
        }
        output::emit(ctx, "prune", &doc)?;
    } else {
        for name in &outcome.pruned {
            say(format!(
                "{} forgot the registration of {name} (its branch is kept)",
                palette.green("✓")
            ));
        }
        for r in &outcome.refused {
            say(format!(
                "{} kept {}: {}",
                palette.yellow("!"),
                r.name,
                r.refusal.message()
            ));
            for hint in r
                .refusal
                .remedies(root, &r.name)
                .iter()
                .filter_map(Remedy::hint)
            {
                note(format!("hint: {hint}"));
            }
        }
        for id in &outcome.changed {
            say(format!(
                "{id} is not a missing worktree any more; left alone"
            ));
        }
        if !confirmed && !chosen.is_empty() {
            note("hint: pass --yes to forget them without being asked");
        }
    }
    Ok(if outcome.refused.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn confirm(prompt: &str) -> Result<bool> {
    match inquire::Confirm::new(prompt).with_default(false).prompt() {
        Ok(answer) => Ok(answer),
        Err(
            inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted,
        ) => bail!("cancelled"),
        Err(error) => Err(error.into()),
    }
}

// --- hook ---------------------------------------------------------------

pub fn hook(ctx: &Ctx, action: HookAction) -> Result<ExitCode> {
    let project = ctx.project()?;
    let name = action.json_name();
    match action {
        HookAction::Install => {
            let path = githook::install(&project)?;
            if ctx.json {
                output::emit(ctx, name, &json!({ "installed": true, "path": path }))?;
            } else {
                say(format!(
                    "Installed the post-checkout hook at {}",
                    path.display()
                ));
            }
        }
        HookAction::Uninstall => {
            let removed = githook::uninstall(&project)?;
            if ctx.json {
                output::emit(ctx, name, &json!({ "removed": removed }))?;
            } else if removed {
                say("Removed the post-checkout hook.");
            } else {
                say("No devflow post-checkout hook is installed.");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn outcome(receipt: Receipt) -> SetupOutcome {
        outcome_with(receipt, Vec::new())
    }

    /// The outcome of a run whose remedies core decided (approve what the
    /// workspace asks to run, then run setup).
    fn outcome_with(receipt: Receipt, remedies: Vec<Remedy>) -> SetupOutcome {
        SetupOutcome {
            workspace: Workspace {
                name: "feature/a".into(),
                branch: Some("feature/a".into()),
                path: PathBuf::from("/work/feature-a"),
                git_dir: PathBuf::from("/repo/.git/worktrees/feature-a"),
                is_default: false,
                locked: false,
                missing: false,
                head: None,
                fingerprint: None,
            },
            status: receipt.status(),
            receipt,
            payload: Vec::new(),
            branch_start: None,
            hooks: None,
            warnings: vec!["could not clone x".into()],
            remedies,
            approval_differs: false,
            drift: None,
            repaired: Vec::new(),
            first_setup: false,
            services_state: Vec::new(),
            switch_hooks: None,
            autostart: None,
        }
    }

    fn receipt() -> Receipt {
        let mut receipt = Receipt::new(uuid::Uuid::new_v4(), "feature/a", None);
        for phase in [
            Phase::Materialize,
            Phase::Payload,
            Phase::Services,
            Phase::Env,
        ] {
            receipt.set_phase(phase, PhaseState::Done, None);
        }
        receipt
    }

    #[test]
    fn summarizes_a_ready_workspace() {
        let mut receipt = receipt();
        receipt.set_phase(Phase::Hooks, PhaseState::Done, None);
        let lines = summary_lines(&outcome(receipt), Palette::stdout());
        assert_eq!(
            lines,
            ["warning: could not clone x", "✓ feature/a is ready"]
        );
    }

    #[test]
    fn explains_skipped_hooks_and_failed_phases() {
        let mut receipt = receipt();
        receipt.skip_required(
            Phase::Hooks,
            "setup hooks are not trusted; review them with `devflow trust`".into(),
        );
        let trust = Remedy::Trust {
            workspace: PathBuf::from("/work/feature-a"),
        };
        let lines = summary_lines(
            &outcome_with(receipt.clone(), vec![trust.clone()]),
            Palette::stdout(),
        )
        .join("\n");
        assert!(lines.contains("feature/a needs attention"), "{lines}");
        assert!(
            lines.contains("hooks: setup hooks are not trusted"),
            "{lines}"
        );
        assert!(
            lines.contains("Review what this project runs with `devflow trust`"),
            "{lines}"
        );
        assert!(!lines.contains("Logs:"), "{lines}");

        // Untrusted [env] recipes say the same; a skip a configuration change
        // reopened asks for a setup, not for a review.
        let mut env = self::receipt();
        env.skip_required(
            Phase::Env,
            "the [env] recipes and presets are not trusted; review them with `devflow trust`"
                .into(),
        );
        let lines = summary_lines(&outcome_with(env, vec![trust]), Palette::stdout()).join("\n");
        assert!(lines.contains("env: the [env] recipes"), "{lines}");
        assert!(lines.contains("`devflow trust`"), "{lines}");
        let mut drift = self::receipt();
        drift.skip_required(
            Phase::Env,
            "configuration changed since .devflow/env was written; run `devflow setup`".into(),
        );
        let lines = summary_lines(&outcome(drift), Palette::stdout()).join("\n");
        assert!(!lines.contains("Review what"), "{lines}");
        assert!(lines.contains("to apply the change"), "{lines}");

        receipt.set_phase(
            Phase::Hooks,
            PhaseState::Failed,
            Some("exit 1: bun install".into()),
        );
        let lines = summary_lines(&outcome(receipt), Palette::stdout()).join("\n");
        assert!(lines.contains("hooks: exit 1: bun install"), "{lines}");
        assert!(
            lines.contains("Logs: /repo/.git/worktrees/feature-a/devflow/logs"),
            "{lines}"
        );
        assert!(!lines.contains("`devflow trust`"), "{lines}");
    }
}
