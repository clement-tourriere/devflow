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
use devflow_core::worktree::{self, Workspace};
use devflow_core::{Project, redact};
use serde::Serialize;
use serde_json::json;
use std::process::ExitCode;
use std::time::Duration;

#[derive(Args)]
pub struct UpArgs {
    /// Workspace name, branch or path (default: the one you are in)
    pub name: Option<String>,
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
    pub name: Option<String>,
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
fn target(ctx: &Ctx, project: &Project, name: Option<&str>) -> Result<Workspace> {
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
        processes::start_with(Pitchfork::locate().as_ref(), &project, &workspace, &options)?;
    render_report(ctx, "up", &project, &report)
}

pub fn down(ctx: &Ctx, args: DownArgs) -> Result<ExitCode> {
    let project = ctx.project()?;
    let workspace = target(ctx, &project, args.name.as_deref())?;
    let options = StopOptions {
        timeout: Duration::from_secs(args.timeout.max(1)),
        force: args.force,
        approval: None,
        expect_instance: None,
    };
    let report =
        processes::stop_with(Pitchfork::locate().as_ref(), &project, &workspace, &options)?;
    render_report(ctx, "down", &project, &report)
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

fn report_lines(report: &ProcessReport, palette: Palette) -> Vec<String> {
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
    if all {
        let mut rows = Vec::new();
        for status in statuses {
            for daemon in &status.daemons {
                rows.push(daemon_row(Some(&status.workspace), daemon));
            }
        }
        if rows.is_empty() {
            lines.push("No daemons in any workspace.".to_string());
        } else {
            lines.push(table(
                &["WORKSPACE", "DAEMON", "STATE", "PORT", "URL"],
                &rows,
            ));
        }
    } else if let Some(status) = statuses.first() {
        if status.daemons.is_empty() {
            lines.push(format!("{}: no daemons.", status.workspace));
        } else {
            lines.push(daemon_table(&status.daemons, None));
        }
    }
    for status in statuses {
        if !status.installed {
            lines.push(palette.dim(
                "pitchfork is missing or lacks a feature devflow needs; see `devflow pitchfork status`",
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
