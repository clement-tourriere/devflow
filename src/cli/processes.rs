//! Processes: `up`, `down`, `ps` and the `pitchfork` commands. Core decides
//! whether anything may be started or stopped ([`processes`]); this module asks
//! for a missing approval at the terminal, turns the answers into requests and
//! renders the result.
use super::output::{self, Palette, key_values, note, say, table};
use super::{Ctx, ProjectInfo, trust};
use anyhow::{Result, anyhow};
use clap::{Args, Subcommand};
use devflow_core::pitchfork::{self, Detection, InstallPlan, Pitchfork, ProxyStatus};
use devflow_core::processes::{
    self, Autostart, DEFAULT_START_TIMEOUT, DEFAULT_STOP_TIMEOUT, DaemonState, Namespace,
    ProcessReport, ProcessStatus, StartOptions, StopOptions,
};
use devflow_core::services::workspace as container_services;
use devflow_core::worktree::{self, Workspace};
use devflow_core::{Project, redact};
use serde::Serialize;
use serde_json::json;
use std::process::ExitCode;
use std::time::Duration;

#[derive(Args)]
pub struct UpArgs {
    /// Workspace name, branch or path (default: the one you are in)
    #[arg(add = super::complete::workspace())]
    pub name: Option<String>,
    /// Also start the workspace's own containers (Valkey, Redis, MySQL), first
    #[arg(long)]
    pub services: bool,
    /// Start only this daemon (and what it depends on); repeatable
    #[arg(long = "daemon", value_name = "NAME")]
    pub daemon: Vec<String>,
    /// Give pitchfork this many seconds
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_START_TIMEOUT.as_secs())]
    pub timeout: u64,
}

#[derive(Args)]
pub struct DownArgs {
    /// Workspace name, branch or path (default: the one you are in)
    #[arg(add = super::complete::workspace(), conflicts_with = "all")]
    pub name: Option<String>,
    /// Also stop the workspace's own containers (Valkey, Redis, MySQL); their
    /// data is kept and the project's PostgreSQL server is never stopped
    #[arg(long)]
    pub services: bool,
    /// Every workspace of the project
    #[arg(long)]
    pub all: bool,
    /// Stop even though the pitchfork configuration changed since it was approved
    #[arg(long)]
    pub force: bool,
    /// Give pitchfork this many seconds
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_STOP_TIMEOUT.as_secs())]
    pub timeout: u64,
}

#[derive(Args)]
pub struct PsArgs {
    /// Workspace name, branch or path (default: the one you are in)
    #[arg(add = super::complete::workspace())]
    pub name: Option<String>,
    /// Every workspace of the project
    #[arg(long, conflicts_with = "name")]
    pub all: bool,
}

#[derive(Subcommand)]
pub enum PitchforkAction {
    /// Is pitchfork installed, is its supervisor up, is the proxy on
    Status,
    /// Install pitchfork with mise (asks first), or print how to
    Install(InstallArgs),
    /// Print the commands that turn the proxy on (devflow never runs them)
    Proxy,
}

#[derive(Args)]
pub struct InstallArgs {
    /// Run the install without asking (it changes your global mise configuration)
    #[arg(short = 'y', long)]
    pub yes: bool,
}

impl PitchforkAction {
    pub fn json_name(&self) -> &'static str {
        match self {
            PitchforkAction::Status => "pitchfork status",
            PitchforkAction::Install(_) => "pitchfork install",
            PitchforkAction::Proxy => "pitchfork proxy",
        }
    }
}

/// The workspace named, or the one containing the working directory.
pub(super) fn target(ctx: &Ctx, project: &Project, name: Option<&str>) -> Result<Workspace> {
    match name {
        Some(name) => worktree::find(project, name)?
            .ok_or_else(|| anyhow!("no workspace named '{name}'; see `devflow list`")),
        None => worktree::containing(project, &ctx.cwd)?.ok_or_else(|| {
            anyhow!(
                "{} is not inside a workspace of {}; name one (see `devflow list`)",
                ctx.cwd.display(),
                project.name()
            )
        }),
    }
}

// --- up / down ------------------------------------------------------------

pub fn up(ctx: &Ctx, args: UpArgs) -> Result<ExitCode> {
    let project = ctx.project()?;
    let workspace = target(ctx, &project, args.name.as_deref())?;
    ctx.target_is(&workspace.path);
    // The containers first: the daemons may connect to them.
    let services = args
        .services
        .then(|| container_services::start_services(&project, &workspace));
    // Without `--services` a workspace with nothing to start says so (a
    // refusal); with it, only the containers may be what was asked for.
    if services.is_some() && !processes::has_daemons(&project, &workspace) {
        return render_both(ctx, "up", &project, vec![(workspace, None, services)]);
    }
    // An unapproved pitchfork configuration is shown and asked about at the
    // terminal, like hooks are; the approval is of exactly what was shown.
    let approval = trust::resolve(ctx, &project, &workspace.path, true)?;
    let options = StartOptions {
        timeout: Duration::from_secs(args.timeout.max(1)),
        only: args.daemon,
        approval,
        ..StartOptions::default()
    };
    let report =
        processes::start_with(Pitchfork::locate().as_ref(), &project, &workspace, &options);
    match services {
        None => render_report(ctx, "up", &project, &report?),
        Some(_) => render_both(
            ctx,
            "up",
            &project,
            vec![(workspace, Some(report), services)],
        ),
    }
}

pub fn down(ctx: &Ctx, args: DownArgs) -> Result<ExitCode> {
    let project = ctx.project()?;
    let workspaces: Vec<Workspace> = if args.all {
        worktree::list(&project)?
            .into_iter()
            .filter(|w| !w.missing)
            .collect()
    } else {
        let workspace = target(ctx, &project, args.name.as_deref())?;
        ctx.target_is(&workspace.path);
        vec![workspace]
    };
    let pitchfork = Pitchfork::locate();
    let options = StopOptions {
        timeout: Duration::from_secs(args.timeout.max(1)),
        force: args.force,
        approval: None,
        expect_instance: None,
    };
    // One workspace, daemons only: the report and the errors are the same as
    // they have always been.
    if !args.all && !args.services {
        let report = processes::stop_with(pitchfork.as_ref(), &project, &workspaces[0], &options)?;
        return render_report(ctx, "down", &project, &report);
    }
    let mut results = Vec::new();
    for workspace in workspaces {
        // Which daemons run is pitchfork's to say under the namespace devflow
        // assigned, not the current configuration's: a daemon whose definition
        // was removed since it started still runs. So pitchfork is asked
        // whenever anything says this workspace has or had daemons (a
        // declaration, a pitchfork file, devflow's own record of a start), and
        // a failure to ask is reported, never skipped. A workspace with none of
        // those never had any: there is nothing to stop.
        let daemons = processes::may_have_daemons(&project, &workspace)
            .then(|| processes::stop_with(pitchfork.as_ref(), &project, &workspace, &options));
        // Daemons first: they may be using the containers.
        let services = args
            .services
            .then(|| container_services::stop_services(&project, &workspace));
        results.push((workspace, daemons, services));
    }
    render_both(ctx, "down", &project, results)
}

/// One workspace's daemons and containers after an `up` or a `down` that
/// touched both (or several workspaces).
type Outcome = (
    Workspace,
    Option<Result<ProcessReport>>,
    Option<container_services::Changes>,
);

fn render_both(
    ctx: &Ctx,
    command: &str,
    project: &Project,
    results: Vec<Outcome>,
) -> Result<ExitCode> {
    let ok = results.iter().all(|(_, daemons, services)| {
        daemons
            .as_ref()
            .is_none_or(|daemons| daemons.as_ref().is_ok_and(ProcessReport::ok))
            && services
                .as_ref()
                .is_none_or(container_services::Changes::ok)
    });
    if ctx.json {
        #[derive(Serialize)]
        struct One<'a> {
            workspace: &'a str,
            path: &'a std::path::Path,
            #[serde(skip_serializing_if = "Option::is_none")]
            daemons: Option<&'a ProcessReport>,
            #[serde(skip_serializing_if = "Option::is_none")]
            daemons_error: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            services: Option<&'a container_services::Changes>,
        }
        let workspaces: Vec<One> = results
            .iter()
            .map(|(workspace, daemons, services)| One {
                workspace: &workspace.name,
                path: &workspace.path,
                daemons: daemons.as_ref().and_then(|d| d.as_ref().ok()),
                daemons_error: daemons
                    .as_ref()
                    .and_then(|d| d.as_ref().err())
                    .map(|error| redact::text(&format!("{error:#}"))),
                services: services.as_ref(),
            })
            .collect();
        let body = json!({
            "project": ProjectInfo::of(project),
            "ok": ok,
            "workspaces": workspaces,
        });
        output::emit(ctx, command, &body)?;
    } else {
        let palette = Palette::stdout();
        let several = results.len() > 1;
        for (workspace, daemons, services) in &results {
            match daemons {
                Some(Ok(report)) => {
                    for line in report_lines(report, palette) {
                        say(line);
                    }
                }
                Some(Err(error)) => say(format!(
                    "{} {}: daemons: {}",
                    palette.yellow("!"),
                    workspace.name,
                    redact::text(&format!("{error:#}"))
                )),
                None => {}
            }
            if let Some(changes) = services {
                for line in service_lines(changes, palette, several || daemons.is_none()) {
                    say(line);
                }
            }
        }
    }
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

pub(super) fn service_lines(
    changes: &container_services::Changes,
    palette: Palette,
    named: bool,
) -> Vec<String> {
    use container_services::Change;
    if let Some(error) = &changes.error {
        return vec![format!(
            "{} {}: container services: {}",
            palette.yellow("!"),
            changes.workspace,
            redact::text(error)
        )];
    }
    if changes.services.is_empty() {
        return if named {
            vec![format!("{}: no container services", changes.workspace)]
        } else {
            Vec::new()
        };
    }
    changes
        .services
        .iter()
        .map(|service| {
            let (symbol, what) = match service.change {
                Change::Stopped => (palette.green("✓"), "stopped; its data is kept".to_string()),
                Change::AlreadyStopped => (palette.green("✓"), "was not running".to_string()),
                Change::Started => (palette.green("✓"), "started".to_string()),
                Change::AlreadyRunning => (palette.green("✓"), "was already running".to_string()),
                Change::Failed => (
                    palette.yellow("!"),
                    format!(
                        "failed: {}",
                        redact::text(service.reason.as_deref().unwrap_or("unknown reason"))
                    ),
                ),
            };
            format!(
                "{symbol} {}: {} ({}) {what}",
                changes.workspace, service.key, service.engine
            )
        })
        .collect()
}

fn render_report(
    ctx: &Ctx,
    command: &str,
    project: &Project,
    report: &ProcessReport,
) -> Result<ExitCode> {
    if ctx.json {
        #[derive(Serialize)]
        struct Doc<'a> {
            project: ProjectInfo<'a>,
            ok: bool,
            #[serde(flatten)]
            report: &'a ProcessReport,
        }
        let doc = Doc {
            project: ProjectInfo::of(project),
            ok: report.ok(),
            report,
        };
        output::emit(ctx, command, &doc)?;
    } else {
        for line in report_lines(report, Palette::stdout()) {
            say(line);
        }
    }
    Ok(if report.ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

pub(super) fn report_lines(report: &ProcessReport, palette: Palette) -> Vec<String> {
    use devflow_core::processes::Action;
    let mut lines: Vec<String> = report
        .warnings
        .iter()
        .map(|warning| format!("warning: {}", redact::text(warning)))
        .collect();
    let verb = match report.action {
        Action::Start => "started",
        Action::Stop => "stopped",
    };
    let active = report.daemons.iter().filter(|d| d.is_active()).count();
    if report.ok() {
        lines.push(match report.action {
            Action::Start => format!(
                "{} {}: {active} daemon(s) running",
                palette.green("✓"),
                report.workspace
            ),
            Action::Stop => format!("{} {}: {verb}", palette.green("✓"), report.workspace),
        });
    } else {
        lines.push(format!(
            "{} {}: not everything {verb}{}",
            palette.yellow("!"),
            report.workspace,
            report
                .error
                .as_deref()
                .map(|e| format!(": {}", redact::text(e)))
                .unwrap_or_default()
        ));
        if let Some(error) = &report.state_error {
            lines.push(format!(
                "  could not read pitchfork's state: {}",
                redact::text(error)
            ));
        }
    }
    if !report.daemons.is_empty() {
        lines.push(daemon_table(&report.daemons, None));
    }
    lines
}

// --- ps -------------------------------------------------------------------

pub fn ps(ctx: &Ctx, args: PsArgs) -> Result<ExitCode> {
    let project = ctx.project()?;
    let pitchfork = Pitchfork::locate();
    let workspaces: Vec<Workspace> = if args.all {
        worktree::list(&project)?
            .into_iter()
            .filter(|w| !w.missing)
            .collect()
    } else {
        vec![target(ctx, &project, args.name.as_deref())?]
    };
    let statuses = processes::status_all(pitchfork.as_ref(), &project, &workspaces);
    if ctx.json {
        let body = json!({ "project": ProjectInfo::of(&project), "workspaces": statuses });
        output::emit(ctx, "ps", &body)?;
        return Ok(ExitCode::SUCCESS);
    }
    for line in ps_lines(&statuses, args.all, Palette::stdout()) {
        say(line);
    }
    Ok(ExitCode::SUCCESS)
}

fn ps_lines(statuses: &[ProcessStatus], all: bool, palette: Palette) -> Vec<String> {
    let mut lines = Vec::new();
    let app_port = |status: &ProcessStatus| {
        status
            .app_port
            .map_or_else(|| "—".to_string(), |port| port.to_string())
    };
    if all {
        let mut rows = Vec::new();
        for status in statuses {
            for daemon in &status.daemons {
                let mut row = daemon_row(Some(&status.workspace), daemon);
                row.insert(1, app_port(status));
                rows.push(row);
            }
        }
        if rows.is_empty() {
            lines.push("No daemons in any workspace.".to_string());
            let ports: Vec<Vec<String>> = statuses
                .iter()
                .filter(|status| status.app_port.is_some())
                .map(|status| vec![status.workspace.clone(), app_port(status)])
                .collect();
            if !ports.is_empty() {
                lines.push(table(&["WORKSPACE", "APP PORT"], &ports));
            }
        } else {
            lines.push(table(
                &["WORKSPACE", "APP PORT", "DAEMON", "STATE", "PORT", "URL"],
                &rows,
            ));
        }
    } else if let Some(status) = statuses.first() {
        if status.daemons.is_empty() {
            lines.push(format!("{}: no daemons.", status.workspace));
        } else {
            lines.push(daemon_table(&status.daemons, None));
        }
        if let Some(port) = status.app_port {
            lines.push(format!(
                "{}: app port {port} (DEVFLOW_PORT)",
                status.workspace
            ));
        }
    }
    for status in statuses {
        if !status.installed {
            lines.push(palette.dim(
                "pitchfork is missing or is not a release devflow supports; see `devflow pitchfork status`",
            ));
            break;
        }
    }
    for status in statuses {
        if let Some(error) = &status.error {
            lines.push(format!(
                "{} {}: {}",
                palette.yellow("!"),
                status.workspace,
                redact::text(error)
            ));
        }
        // What the table shows for a daemon pitchfork did not list ("not
        // started") is not an observation while this is said.
        if let Some(why) = status.observation.summary()
            && status.error.as_deref() != Some(why.as_str())
        {
            lines.push(format!(
                "{} {}: what runs was not observed: {}",
                palette.yellow("!"),
                status.workspace,
                redact::text(&why)
            ));
        }
        if status.configured && !status.can_start.allowed && status.installed {
            lines.push(palette.dim(&format!(
                "{}: cannot start: {}",
                status.workspace,
                status.can_start.reason.as_deref().unwrap_or("")
            )));
        }
        if let Some(why) = &status.autostart_blocked
            && status.autostart
        {
            lines.push(palette.dim(&format!("{}: {why}", status.workspace)));
        }
    }
    lines
}

fn port_cell(daemon: &DaemonState) -> String {
    daemon
        .ports
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn daemon_row(workspace: Option<&str>, daemon: &DaemonState) -> Vec<String> {
    let mut row = Vec::new();
    if let Some(workspace) = workspace {
        row.push(workspace.to_string());
    }
    row.push(daemon.name.clone());
    row.push(match &daemon.error {
        Some(error) => format!("{} ({})", daemon.state, redact::text(error)),
        None => daemon.state.clone(),
    });
    row.push(port_cell(daemon));
    row.push(daemon.url.clone().unwrap_or_default());
    row
}

fn daemon_table(daemons: &[DaemonState], workspace: Option<&str>) -> String {
    let rows: Vec<Vec<String>> = daemons.iter().map(|d| daemon_row(workspace, d)).collect();
    table(&["DAEMON", "STATE", "PORT", "URL"], &rows)
}

/// What `[pitchfork] autostart` did, for the end of `switch` and `setup`.
pub fn autostart_lines(autostart: &Autostart, palette: Palette) -> Vec<String> {
    match (&autostart.report, &autostart.skipped) {
        (Some(report), _) => report_lines(report, palette),
        (None, Some(why)) => vec![format!(
            "{} daemons were not started: {}",
            palette.dim("-"),
            redact::text(why)
        )],
        (None, None) => Vec::new(),
    }
}

// --- pitchfork --------------------------------------------------------------

pub fn pitchfork_command(ctx: &Ctx, action: PitchforkAction) -> Result<ExitCode> {
    match action {
        PitchforkAction::Status => pitchfork_status(ctx),
        PitchforkAction::Install(args) => pitchfork_install(ctx, args),
        PitchforkAction::Proxy => pitchfork_proxy(ctx),
    }
}

#[derive(Serialize)]
struct WorkspaceInfo {
    name: String,
    path: std::path::PathBuf,
    namespace: Namespace,
}

fn pitchfork_status(ctx: &Ctx) -> Result<ExitCode> {
    let project = ctx.project().ok();
    let detection = pitchfork::detect();
    let located = Pitchfork::locate();
    let mut supervisor_up = None;
    let mut proxy: Option<ProxyStatus> = None;
    let mut proxy_error = None;
    if detection.installed
        && let Some(pitchfork) = &located
    {
        supervisor_up = pitchfork.supervisor_up(&ctx.cwd).ok();
        match pitchfork.proxy_status(&ctx.cwd) {
            Ok(status) => proxy = Some(status),
            Err(error) => proxy_error = Some(format!("{error:#}")),
        }
    }
    let mut conflicts = Vec::new();
    let mut workspace = None;
    if let Some(project) = &project
        && let Some(found) = worktree::containing(project, &ctx.cwd).ok().flatten()
    {
        conflicts.extend(processes::namespace_problem(&found.path));
        conflicts.extend(processes::env_wiring(project, &found.path));
        workspace = Some(WorkspaceInfo {
            namespace: processes::namespace_of(&found.path),
            name: found.name,
            path: found.path,
        });
    }
    if let Some(problem) = detection.problem.as_ref().filter(|_| !detection.installed) {
        conflicts.push(format!("the pitchfork on PATH does not run: {problem}"));
    }
    let install = (!detection.usable()).then(pitchfork::install_plan);
    if ctx.json {
        #[derive(Serialize)]
        struct Doc<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            project: Option<ProjectInfo<'a>>,
            pitchfork: &'a Detection,
            usable: bool,
            supervisor_up: Option<bool>,
            proxy: Option<&'a ProxyStatus>,
            #[serde(skip_serializing_if = "Option::is_none")]
            proxy_error: Option<&'a str>,
            proxy_commands: [&'static str; 4],
            #[serde(skip_serializing_if = "Option::is_none")]
            install: Option<&'a InstallPlan>,
            conflicts: &'a [String],
            #[serde(skip_serializing_if = "Option::is_none")]
            workspace: Option<&'a WorkspaceInfo>,
        }
        let doc = Doc {
            project: project.as_ref().map(ProjectInfo::of),
            pitchfork: &detection,
            usable: detection.usable(),
            supervisor_up,
            proxy: proxy.as_ref(),
            proxy_error: proxy_error.as_deref(),
            proxy_commands: pitchfork::PROXY_COMMANDS,
            install: install.as_ref(),
            conflicts: &conflicts,
            workspace: workspace.as_ref(),
        };
        output::emit(ctx, "pitchfork status", &doc)?;
        return Ok(ExitCode::SUCCESS);
    }
    let palette = Palette::stdout();
    let mut pairs = vec![("pitchfork", detection.summary())];
    if detection.installed {
        pairs.push((
            "supervisor",
            match supervisor_up {
                Some(true) => "running".to_string(),
                Some(false) => {
                    "not running (pitchfork starts it when you start a daemon)".to_string()
                }
                None => "unknown".to_string(),
            },
        ));
        pairs.push((
            "proxy",
            proxy_summary(proxy.as_ref(), proxy_error.as_deref()),
        ));
    }
    if let Some(workspace) = &workspace {
        pairs.push((
            "namespace",
            format!("{} ({})", workspace.namespace.value, workspace.name),
        ));
    }
    say(key_values(&pairs));
    for conflict in &conflicts {
        say(format!(
            "{} {}",
            palette.yellow("!"),
            redact::text(conflict)
        ));
    }
    if let Some(plan) = &install {
        say("");
        say("To install pitchfork:");
        say(format!(
            "  devflow pitchfork install    {}",
            if plan.run.is_some() {
                "(runs `mise use -g pitchfork@latest` after asking)"
            } else {
                "(prints how, mise is not installed)"
            }
        ));
    }
    if detection.installed && proxy.as_ref().is_none_or(|p| !p.enabled) {
        say("");
        say("The proxy gives each daemon with a port an HTTPS URL. See `devflow pitchfork proxy`.");
    }
    Ok(ExitCode::SUCCESS)
}

fn proxy_summary(proxy: Option<&ProxyStatus>, error: Option<&str>) -> String {
    match (proxy, error) {
        (Some(p), _) if p.enabled => format!(
            "on, {}://*.{}{}{}",
            p.scheme.as_deref().unwrap_or("https"),
            p.tld.as_deref().unwrap_or("localhost"),
            p.port.map(|port| format!(":{port}")).unwrap_or_default(),
            match p.trusted {
                Some(true) => ", certificate trusted",
                Some(false) => ", certificate not trusted",
                None => "",
            }
        ),
        (Some(_), _) => "off".to_string(),
        (None, Some(error)) => format!("unknown ({})", redact::text(error)),
        (None, None) => "unknown".to_string(),
    }
}

fn pitchfork_proxy(ctx: &Ctx) -> Result<ExitCode> {
    let detection = pitchfork::detect();
    let proxy = if detection.installed {
        Pitchfork::locate().and_then(|p| p.proxy_status(&ctx.cwd).ok())
    } else {
        None
    };
    if ctx.json {
        let body = json!({
            "pitchfork": detection,
            "proxy": proxy,
            "commands": pitchfork::PROXY_COMMANDS,
            "ran": false,
        });
        output::emit(ctx, "pitchfork proxy", &body)?;
        return Ok(ExitCode::SUCCESS);
    }
    say(format!("proxy: {}", proxy_summary(proxy.as_ref(), None)));
    say("");
    say("devflow does not change your proxy, DNS or certificates. To turn the proxy on, run");
    say("these yourself, in order (`proxy setup` asks for sudo):");
    for command in pitchfork::PROXY_COMMANDS {
        say(format!("  {command}"));
    }
    Ok(ExitCode::SUCCESS)
}

fn pitchfork_install(ctx: &Ctx, args: InstallArgs) -> Result<ExitCode> {
    let before = pitchfork::detect();
    let plan = pitchfork::install_plan();
    let mut ran = false;
    let mut message = None;
    let mut ok = before.usable();
    if before.usable() {
        // Nothing to do.
    } else if let Some(argv) = &plan.run {
        let confirmed = if args.yes {
            true
        } else if ctx.interactive() {
            confirm(&format!(
                "Run `{}`? It changes your global mise configuration.",
                argv.join(" ")
            ))?
        } else {
            false
        };
        if confirmed {
            note(format!("$ {}", argv.join(" ")));
            let result = pitchfork::run_install(&plan, Duration::from_secs(15 * 60))?;
            ran = true;
            ok = result.ok;
            message = result.message;
        }
    }
    let after = if ran {
        pitchfork::detect()
    } else {
        before.clone()
    };
    if ctx.json {
        let body = json!({
            "already_installed": before.usable(),
            "plan": plan,
            "ran": ran,
            "ok": after.usable(),
            "needs_confirmation": !before.usable() && plan.run.is_some() && !ran,
            "message": message,
            "pitchfork": after,
        });
        output::emit(ctx, "pitchfork install", &body)?;
        return Ok(if before.usable() || after.usable() || !ran {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }
    if before.usable() {
        say(format!("Already installed: {}", before.summary()));
        return Ok(ExitCode::SUCCESS);
    }
    if ran {
        if after.usable() {
            say(format!("Installed: {}", after.summary()));
            return Ok(ExitCode::SUCCESS);
        }
        say(format!(
            "The install command {}; pitchfork is still not usable: {}",
            if ok { "finished" } else { "failed" },
            after.summary()
        ));
        if let Some(message) = message {
            say(redact::text(&message));
        }
        return Ok(ExitCode::FAILURE);
    }
    if plan.run.is_some() {
        say("Not installed. devflow runs this only after you confirm (rerun with --yes):");
    } else {
        say("Not installed, and mise is not available to install it. Run one of:");
    }
    for command in &plan.commands {
        say(format!("  {command}"));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn state(name: &str, state: &str, ports: &[u16], url: Option<&str>) -> DaemonState {
        DaemonState {
            name: name.into(),
            id: Some(format!("ns/{name}")),
            state: state.into(),
            pid: None,
            ports: ports.to_vec(),
            url: url.map(str::to_string),
            error: None,
            triggers: Vec::new(),
            configured: true,
            startable: true,
            unreadable: None,
        }
    }

    #[test]
    fn daemons_are_listed_with_state_port_and_url() {
        let daemons = [
            state("web", "running", &[3000], Some("https://web.x.localhost")),
            DaemonState {
                error: Some("exit code 3".into()),
                ..state("worker", "errored", &[], None)
            },
        ];
        assert_eq!(
            daemon_table(&daemons, None),
            "DAEMON  STATE                  PORT  URL\n\
             web     running                3000  https://web.x.localhost\n\
             worker  errored (exit code 3)"
        );
    }

    #[test]
    fn the_proxy_is_summarized_in_one_line() {
        assert_eq!(proxy_summary(None, None), "unknown");
        assert_eq!(proxy_summary(Some(&ProxyStatus::default()), None), "off");
        let on = ProxyStatus {
            enabled: true,
            scheme: Some("https".into()),
            tld: Some("localhost".into()),
            port: Some(443),
            trusted: Some(false),
            projects: Vec::new(),
        };
        assert_eq!(
            proxy_summary(Some(&on), None),
            "on, https://*.localhost:443, certificate not trusted"
        );
    }
}
