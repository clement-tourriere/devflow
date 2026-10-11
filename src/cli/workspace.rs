//! Project and workspace commands: init, switch, list, path, setup, remove, hook.
use super::output::{self, Palette, Printer, note, say};
use super::{Ctx, EXIT_ATTENTION, ProjectInfo, shell, trust};
use anyhow::{Result, anyhow, bail};
use clap::{Args, Subcommand, ValueEnum};
use devflow_core::config::PostgresMode;
use devflow_core::db::DataSource;
use devflow_core::error::{Busy, Protected};
use devflow_core::hooks::HookReport;
use devflow_core::project::{self, InitOptions};
use devflow_core::receipt::{Phase, PhaseState, Receipt, Status};
use devflow_core::setup::{
    self, CreateRequest, RemoveOptions, RemoveOutcome, SetupOptions, SetupOutcome, WorkspaceStatus,
};
use devflow_core::worktree::{self, Workspace};
use devflow_core::{Project, githook, redact, registry};
use serde::Serialize;
use serde_json::json;
use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;

#[derive(Clone, Copy, ValueEnum)]
pub enum PostgresChoice {
    /// devflow runs a PostgreSQL container with docker compose
    Managed,
    /// An existing PostgreSQL 18+ server
    External,
}

impl From<PostgresChoice> for PostgresMode {
    fn from(choice: PostgresChoice) -> Self {
        match choice {
            PostgresChoice::Managed => PostgresMode::Managed,
            PostgresChoice::External => PostgresMode::External,
        }
    }
}

#[derive(Args)]
pub struct InitArgs {
    /// Project name (default: the checkout's directory name)
    #[arg(long)]
    pub name: Option<String>,
    /// Add a [postgres] section for database branches
    #[arg(long, value_enum)]
    pub postgres: Option<PostgresChoice>,
    /// Also install the post-checkout Git hook
    #[arg(long)]
    pub hook: bool,
}

#[derive(Args)]
pub struct SwitchArgs {
    /// Workspace (branch) name
    pub name: String,
    /// Create the workspace
    #[arg(short = 'c', long)]
    pub create: bool,
    /// Code origin for a new workspace (default: the default workspace)
    #[arg(long, value_name = "WS", requires = "create")]
    pub from: Option<String>,
    /// Data origin: a workspace, snapshot:NAME[@vN], branch:ID or empty
    #[arg(long, value_name = "SRC")]
    pub data_from: Option<DataSource>,
    /// Do not create or attach a database
    #[arg(long)]
    pub no_db: bool,
    /// Do not run hooks
    #[arg(long)]
    pub no_hooks: bool,
}

#[derive(Args)]
pub struct ListArgs {
    /// List every registered project instead of the current one
    #[arg(long)]
    pub all: bool,
}

#[derive(Args)]
pub struct PathArgs {
    /// Workspace name, branch or path
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
    /// Entry point of the post-checkout Git hook: never prompts and never fails
    #[arg(long, hide = true)]
    pub from_git_hook: bool,
}

#[derive(Args)]
pub struct RemoveArgs {
    /// Workspace name, branch or path
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
    /// Install the post-checkout hook
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

// --- init ---------------------------------------------------------------

pub fn init(ctx: &Ctx, args: InitArgs) -> Result<ExitCode> {
    let project = project::init(
        &ctx.cwd,
        InitOptions {
            name: args.name,
            postgres: args.postgres.map(Into::into),
        },
    )?;
    // The project is usable even if the optional hook cannot be installed.
    let hook = args.hook.then(|| githook::install(&project));
    if ctx.json {
        let hook = hook.as_ref().map(|result| match result {
            Ok(path) => json!({ "installed": true, "path": path }),
            Err(error) => json!({ "installed": false, "error": format!("{error:#}") }),
        });
        let body = json!({
            "project": ProjectInfo::of(&project),
            "config": project.config_path,
            "postgres": project.config.postgres.as_ref().map(|pg| pg.mode),
            "hook": hook,
        });
        output::emit(ctx, "init", &body)?;
        return Ok(ExitCode::SUCCESS);
    }
    say(format!("Created {}", project.config_path.display()));
    match hook {
        Some(Ok(path)) => note(format!(
            "Installed the post-checkout hook at {}",
            path.display()
        )),
        Some(Err(error)) => note(format!(
            "warning: could not install the Git hook: {}",
            redact::text(&format!("{error:#}"))
        )),
        None => {}
    }
    note(
        "Next: review devflow.toml, commit it, then `devflow switch -c NAME` to create a workspace.",
    );
    Ok(ExitCode::SUCCESS)
}

// --- switch -------------------------------------------------------------

pub fn switch(ctx: &Ctx, args: SwitchArgs) -> Result<ExitCode> {
    let project = ctx.project()?;
    ctx.register(&project);
    let mut options = SetupOptions {
        data_from: args.data_from,
        no_db: args.no_db,
        no_hooks: args.no_hooks,
        ..SetupOptions::default()
    };
    let mut printer =
        Printer::new(ctx).announcing_database(project.config.postgres.is_some() && !args.no_db);
    let (outcome, created, switch_hooks) = if args.create {
        options.trust_hooks = trust::resolve(ctx, &project, !args.no_hooks)?;
        let request = CreateRequest {
            name: args.name,
            from: args.from,
            options: options.clone(),
        };
        let mut outcome = match setup::create(&project, request, &mut printer) {
            Ok(outcome) => outcome,
            Err(error) => return in_progress(ctx, "switch", &project, None, error),
        };
        // Entering the new workspace is a switch too: its switch hooks run once
        // setup has finished.
        let hooks = first_entry_hooks(&project, &mut outcome, &options, &mut printer)?;
        (outcome, true, hooks)
    } else {
        let workspace = worktree::find(&project, &args.name)?.ok_or_else(|| {
            anyhow!(
                "workspace '{0}' does not exist; create it with `devflow switch -c {0}`",
                args.name
            )
        })?;
        let hooks_may_run = !args.no_hooks && switch_runs_hooks(&project, &workspace);
        options.trust_hooks = trust::resolve(ctx, &project, hooks_may_run)?;
        match setup::switch(&project, &workspace, options, &mut printer) {
            Ok(outcome) => (outcome, false, None),
            Err(error) => return in_progress(ctx, "switch", &project, Some(&workspace), error),
        }
    };
    let code = report(
        ctx,
        "switch",
        &project,
        &outcome,
        created,
        switch_hooks.as_ref(),
    )?;
    if !ctx.json {
        hand_off(&outcome.workspace.path);
    }
    Ok(code)
}

/// Run the switch hooks of a freshly created workspace once it is ready: the
/// first entry is a switch too. A failing hook is a warning, like in
/// `setup::switch`, because the workspace exists; so is a skipped one.
fn first_entry_hooks(
    project: &Project,
    outcome: &mut SetupOutcome,
    options: &SetupOptions,
    printer: &mut Printer,
) -> Result<Option<HookReport>> {
    if project.config.hooks.switch.is_empty() || options.no_hooks {
        return Ok(None);
    }
    if outcome.status != Status::Ready {
        outcome
            .warnings
            .push("switch hooks were not run: the workspace is not ready".into());
        return Ok(None);
    }
    let report = setup::run_switch_hooks(project, &outcome.workspace, options, printer)?;
    match report.as_ref().map(HookReport::failure) {
        Some(Some(failure)) => outcome.warnings.push(failure),
        Some(None) => {}
        None => outcome.warnings.push(
            "switch hooks were not run: hooks are not trusted; review them with `devflow trust`"
                .into(),
        ),
    }
    Ok(report)
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
            hand_off(&workspace.path);
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

/// Whether switching into `workspace` can run hooks, so a trust prompt is
/// worth showing: switch hooks exist, or setup is not finished.
fn switch_runs_hooks(project: &Project, workspace: &Workspace) -> bool {
    if !project.config.hooks.switch.is_empty() {
        return true;
    }
    match Receipt::load(&workspace.git_dir) {
        Ok(Some(receipt)) => receipt.status() != Status::Ready,
        _ => true,
    }
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
    let items = setup::list(&project)?;
    if ctx.json {
        let body = json!({ "project": ProjectInfo::of(&project), "workspaces": items });
        output::emit(ctx, "list", &body)?;
    } else {
        say(workspace_table(ctx, &items));
    }
    Ok(ExitCode::SUCCESS)
}

fn list_all(ctx: &Ctx) -> Result<ExitCode> {
    #[derive(Serialize)]
    struct Row {
        name: String,
        id: uuid::Uuid,
        root: std::path::PathBuf,
        #[serde(skip_serializing_if = "Option::is_none")]
        workspaces: Option<Vec<WorkspaceStatus>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    }
    let rows: Vec<Row> = registry::list()?
        .into_iter()
        .map(|entry| {
            let listed = Project::open(&entry.root).and_then(|project| setup::list(&project));
            let (workspaces, error) = match listed {
                Ok(items) => (Some(items), None),
                Err(error) => (None, Some(format!("{error:#}"))),
            };
            Row {
                name: entry.name,
                id: entry.id,
                root: entry.root,
                workspaces,
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
            (Some(items), _) => say(workspace_table(ctx, items)),
            (None, Some(error)) => say(format!("  ! {}", redact::text(error))),
            (None, None) => {}
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn workspace_table(ctx: &Ctx, items: &[WorkspaceStatus]) -> String {
    // The workspace containing the caller's directory (deepest path wins).
    let current = items
        .iter()
        .filter(|item| ctx.cwd.starts_with(&item.workspace.path))
        .max_by_key(|item| item.workspace.path.components().count())
        .map(|item| &item.workspace.path);
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|item| {
            let workspace = &item.workspace;
            let marker = if current == Some(&workspace.path) {
                "*"
            } else {
                " "
            };
            let database = item
                .receipt
                .as_ref()
                .and_then(|receipt| receipt.data.as_ref())
                .map_or("—".to_string(), |data| data.database.clone());
            vec![
                format!("{marker} {}", workspace.name),
                workspace
                    .branch
                    .clone()
                    .unwrap_or_else(|| "(detached)".into()),
                status_label(item).to_string(),
                database,
                output::tilde(&workspace.path),
            ]
        })
        .collect();
    output::table(&["  NAME", "BRANCH", "STATUS", "DATABASE", "PATH"], &rows)
}

fn status_label(item: &WorkspaceStatus) -> &'static str {
    if item.workspace.missing {
        "missing"
    } else if item.busy {
        "busy"
    } else {
        match item.status {
            None => "not set up",
            Some(Status::Ready) => "ready",
            Some(Status::NeedsAttention) => "needs attention",
            Some(Status::InProgress) => "in progress",
        }
    }
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
    let provisions = project.config.postgres.is_some() && args.database_url.is_none();
    let options = SetupOptions {
        data_from: args.data_from,
        database_url: args.database_url,
        trust_hooks: trust::resolve(ctx, &project, true)?,
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
        Err(error) => return in_progress(ctx, "setup", &project, Some(&workspace), error),
    };
    match outcome {
        Some(outcome) => report(ctx, "setup", &project, &outcome, false, None),
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
        let options = SetupOptions {
            data_from: args.data_from,
            database_url: args.database_url,
            trust_hooks: devflow_core::trust::env_override(),
            ..SetupOptions::default()
        };
        setup::setup(
            &project,
            &workspace,
            options,
            args.if_new,
            args.new_instance,
            &mut Printer::new(ctx).announcing_database(project.config.postgres.is_some()),
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
    switch_hooks: Option<&HookReport>,
) -> Result<ExitCode> {
    if ctx.json {
        #[derive(Serialize)]
        struct Doc<'a> {
            project: ProjectInfo<'a>,
            created: bool,
            skipped: bool,
            path: &'a Path,
            /// Switch hooks run on entering a newly created workspace; they are
            /// reported apart from `hooks`, which holds the setup run.
            #[serde(skip_serializing_if = "Option::is_none")]
            switch_hooks: Option<&'a HookReport>,
            #[serde(flatten)]
            outcome: &'a SetupOutcome,
        }
        let doc = Doc {
            project: ProjectInfo::of(project),
            created,
            skipped: false,
            path: &outcome.workspace.path,
            switch_hooks,
            outcome,
        };
        output::emit(ctx, command, &doc)?;
    } else {
        print_summary(outcome);
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
    match outcome.status {
        Status::Ready => lines.push(format!(
            "{} {} is ready",
            palette.green("✓"),
            workspace.name
        )),
        Status::InProgress => lines.push(still_running(palette, &workspace.name)),
        Status::NeedsAttention => {
            lines.push(format!(
                "{} {} needs attention",
                palette.yellow("!"),
                workspace.name
            ));
            let mut hooks_skipped = false;
            let mut failed = false;
            for phase in Phase::ALL {
                let record = outcome.receipt.phase(phase);
                let skipped = record.state == PhaseState::Skipped && record.required_skip;
                if record.state != PhaseState::Failed && !skipped {
                    continue;
                }
                hooks_skipped |= skipped && phase == Phase::Hooks;
                failed |= record.state == PhaseState::Failed;
                let detail = record.detail.as_deref().unwrap_or("no details recorded");
                lines.push(format!("  {}: {}", phase.as_str(), redact::text(detail)));
            }
            if hooks_skipped {
                lines.push(
                    "  Review the hooks with `devflow trust` (or set DEVFLOW_TRUST_HOOKS=1), then run `devflow setup` here to finish."
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
        trust_hooks: trust::resolve(ctx, &project, !project.config.hooks.teardown.is_empty())?,
        cwd: Some(ctx.cwd.clone()),
        // The user named the workspace on the command line just now.
        expect: None,
    };
    let mut printer = Printer::new(ctx);
    let outcome = match setup::remove(&project, &args.name, options.clone(), &mut printer) {
        Ok(outcome) => outcome,
        // Core refused before any effect. A person at the terminal may override
        // a protection `--force` can override; everyone else gets the error and
        // its hint.
        Err(error) if !options.force && ctx.interactive() => {
            match error.downcast_ref::<Protected>() {
                Some(protected) if protected.forceable && confirm_forced_removal(protected)? => {
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
    let palette = Palette::stdout();
    say(format!(
        "{} Removed {} ({})",
        palette.green("✓"),
        outcome.workspace.name,
        output::tilde(&outcome.workspace.path)
    ));
    if let Some(id) = outcome.dropped_branch {
        say(format!("  dropped database branch {id}"));
    } else if args.keep_data {
        say("  kept the database branch");
    }
    if outcome.deleted_branch {
        say("  deleted the Git branch");
    }
    Ok(ExitCode::SUCCESS)
}

/// Ask whether to remove a workspace despite a protection `--force` overrides.
fn confirm_forced_removal(protected: &Protected) -> Result<bool> {
    let prompt = format!(
        "{}. Remove it anyway?",
        protected.reason.trim_end_matches('.')
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
            hooks: None,
            warnings: vec!["could not clone x".into()],
        }
    }

    fn receipt() -> Receipt {
        let mut receipt = Receipt::new(uuid::Uuid::new_v4(), "feature/a", None);
        for phase in [
            Phase::Materialize,
            Phase::Payload,
            Phase::Database,
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
        receipt.skip_required(Phase::Hooks, "hooks are not trusted".into());
        let lines = summary_lines(&outcome(receipt.clone()), Palette::stdout()).join("\n");
        assert!(lines.contains("feature/a needs attention"), "{lines}");
        assert!(lines.contains("hooks: hooks are not trusted"), "{lines}");
        assert!(lines.contains("`devflow trust`"), "{lines}");
        assert!(!lines.contains("Logs:"), "{lines}");

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
