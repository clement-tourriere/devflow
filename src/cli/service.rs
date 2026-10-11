//! `devflow service [NAME] ...`: the project's container services (Valkey,
//! Redis, MySQL) and their copy-on-write branches and snapshots. `devflow db`
//! stays the command for PostgreSQL.
//!
//! Handlers parse arguments, resolve the workspace and render; every
//! lifecycle decision (clone protocol, journal, recovery, ports) is made by
//! `devflow_core::services`.
use super::db::workspace_arg;
use super::output::{self, Palette, note, say};
use super::{Ctx, ProjectInfo};
use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};
use devflow_core::Project;
use devflow_core::config::Engine;
use devflow_core::db::DataSource;
use devflow_core::redact;
use devflow_core::services::catalog::{self, RecordState};
use devflow_core::services::{BranchView, Service, ServiceStatus, SnapshotView, workspace};
use devflow_core::setup;
use serde_json::json;
use std::process::ExitCode;
use uuid::Uuid;

#[derive(Args)]
pub struct ServiceArgs {
    /// The service key from `[services.KEY]` (default: the project's only
    /// valkey, redis or mysql service)
    #[arg(value_name = "NAME")]
    pub name: Option<String>,
    #[command(subcommand)]
    pub command: ServiceCommand,
}

#[derive(Subcommand)]
pub enum ServiceCommand {
    /// Show the service: Docker engine, copy-on-write capability, branches
    Status(StatusArgs),
    /// Start a workspace's container (or every branch's with --all)
    Up(TargetArgs),
    /// Stop a workspace's container (or every branch's with --all); data stays
    Down(TargetArgs),
    /// Print a workspace's connection URL, including its password
    Url(WorkspaceArg),
    /// List branches and snapshots (--orphans: recorded data nothing uses)
    List(ListArgs),
    /// Named, versioned, read-only copies of a workspace's data
    Snapshot {
        #[command(subcommand)]
        command: SnapshotCommand,
    },
    /// Replace a workspace's data with a fresh clone of its source (or of --to)
    Reset(ResetArgs),
    /// Finish or roll back operations a crash interrupted
    Reconcile,
    /// Remove a branch (its container and volume) by id
    Drop {
        /// Branch id: a UUID or a unique prefix of at least 8 hex characters
        /// (see `devflow service NAME list`)
        id: devflow_core::ids::IdRef,
    },
    /// Remove everything recorded for a service the configuration no longer has
    Forget,
}

#[derive(Args)]
pub struct StatusArgs {
    /// Measure copy-on-write again instead of using the cached result
    #[arg(long)]
    pub probe: bool,
}

#[derive(Args)]
pub struct WorkspaceArg {
    /// Workspace name (default: the workspace containing the current directory)
    pub workspace: Option<String>,
}

#[derive(Args)]
pub struct TargetArgs {
    /// Workspace name (default: the workspace containing the current directory)
    pub workspace: Option<String>,
    /// Every branch of the service instead of one workspace's
    #[arg(long, conflicts_with = "workspace")]
    pub all: bool,
}

#[derive(Args)]
pub struct ListArgs {
    /// Report what nothing configured or checked out uses any more
    #[arg(long)]
    pub orphans: bool,
}

#[derive(Args)]
pub struct ResetArgs {
    /// Workspace name (default: the workspace containing the current directory)
    pub workspace: Option<String>,
    /// New data origin: a workspace, snapshot:NAME[@vN], branch:ID or empty
    #[arg(long, value_name = "SRC")]
    pub to: Option<DataSource>,
}

#[derive(Subcommand)]
pub enum SnapshotCommand {
    /// Seal a new version of NAME from a workspace's data
    Create {
        name: String,
        /// Workspace to snapshot (default: the workspace containing the current directory)
        #[arg(long, value_name = "WS")]
        from: Option<String>,
    },
    /// List snapshots
    List,
    /// Delete one snapshot version, e.g. seed@v2
    Delete {
        /// NAME@vN
        snapshot: String,
    },
}

/// The hidden coordinator: `devflow __clone PROJECT SERVICE OP`.
#[derive(Args)]
pub struct CoordinateArgs {
    pub project: Uuid,
    pub service: String,
    pub op: Uuid,
}

impl ServiceCommand {
    pub fn json_name(&self) -> &'static str {
        match self {
            ServiceCommand::Status(_) => "service status",
            ServiceCommand::Up(_) => "service up",
            ServiceCommand::Down(_) => "service down",
            ServiceCommand::Url(_) => "service url",
            ServiceCommand::List(_) => "service list",
            ServiceCommand::Snapshot { command } => match command {
                SnapshotCommand::Create { .. } => "service snapshot create",
                SnapshotCommand::List => "service snapshot list",
                SnapshotCommand::Delete { .. } => "service snapshot delete",
            },
            ServiceCommand::Reset(_) => "service reset",
            ServiceCommand::Reconcile => "service reconcile",
            ServiceCommand::Drop { .. } => "service drop",
            ServiceCommand::Forget => "service forget",
        }
    }
}

/// `devflow __clone`: run one journaled operation. It prints a handshake on
/// stdout for the process that started it and its errors on stderr (the
/// operation's log file).
pub fn coordinate(args: CoordinateArgs) -> Result<ExitCode> {
    devflow_core::services::coordinate(args.project, &args.service, args.op)?;
    Ok(ExitCode::SUCCESS)
}

pub fn run(ctx: &Ctx, args: ServiceArgs) -> Result<ExitCode> {
    let command = args.command.json_name();
    let project = ctx.project()?;
    let name = args.name.as_deref();
    match args.command {
        ServiceCommand::Status(status) => show_status(ctx, command, &project, name, status.probe),
        ServiceCommand::List(list) => show_list(ctx, command, &project, name, list.orphans),
        ServiceCommand::Reconcile => reconcile(ctx, command, &project, name),
        ServiceCommand::Forget => forget(ctx, command, &project, name),
        ServiceCommand::Up(target) => start(ctx, command, &project, name, target),
        ServiceCommand::Down(target) => stop(ctx, command, &project, name, target),
        ServiceCommand::Url(arg) => url(ctx, command, &project, name, arg),
        ServiceCommand::Snapshot { command: action } => {
            snapshot(ctx, command, &project, name, action)
        }
        ServiceCommand::Reset(reset_args) => reset(ctx, command, &project, name, reset_args),
        ServiceCommand::Drop { id } => drop_branch(ctx, command, &project, name, id),
    }
}

// --- selecting ---------------------------------------------------------------

/// The container services of the project, as `(key, engine)`.
fn configured(project: &Project) -> Vec<(String, Engine)> {
    project
        .config
        .containers()
        .map(|(key, engine, _)| (key.to_string(), engine))
        .collect()
}

/// The service a command acts on: `name`, else the only one.
fn select(project: &Project, name: Option<&str>) -> Result<Service> {
    let all = configured(project);
    let key = match name {
        Some(name) => {
            if !all.iter().any(|(key, _)| key == name) {
                let known = if all.is_empty() {
                    "it has none (add `[services.NAME]` with `engine = \"valkey\"`, \
                     `\"redis\"` or `\"mysql\"`)"
                        .to_string()
                } else {
                    format!(
                        "its container services are {}",
                        all.iter()
                            .map(|(k, _)| k.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                bail!(
                    "{} has no container service '{name}'; {known}",
                    project.config_path.display()
                );
            }
            name.to_string()
        }
        None => match all.as_slice() {
            [] => bail!(
                "{} has no valkey, redis or mysql service; add `[services.cache]` with \
                 `engine = \"valkey\"`",
                project.config_path.display()
            ),
            [(key, _)] => key.clone(),
            many => bail!(
                "this project has {} container services ({}); name one: \
                 `devflow service NAME …`",
                many.len(),
                many.iter()
                    .map(|(k, _)| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
    };
    Service::open(project, &key)
}

/// The services a listing covers: `name`, else every container service.
fn select_many(project: &Project, name: Option<&str>) -> Result<Vec<Service>> {
    match name {
        Some(_) => Ok(vec![select(project, name)?]),
        None => {
            let all = configured(project);
            anyhow::ensure!(
                !all.is_empty(),
                "{} has no valkey, redis or mysql service; add `[services.cache]` with \
                 `engine = \"valkey\"`",
                project.config_path.display()
            );
            all.iter()
                .map(|(key, _)| Service::open(project, key))
                .collect()
        }
    }
}

// --- status and list --------------------------------------------------------

fn branch_rows(branches: &[BranchView], workspaces: &[(Uuid, String)]) -> Vec<Vec<String>> {
    branches
        .iter()
        .map(|b| {
            let owner = b
                .owner
                .and_then(|o| workspaces.iter().find(|(id, _)| *id == o))
                .map_or(b.name.clone(), |(_, name)| name.clone());
            vec![
                b.id.simple().to_string()[..8].to_string(),
                owner,
                output::snake(&b.state),
                b.container.clone(),
                b.endpoint.clone().unwrap_or_else(|| "—".into()),
                source_label(b),
                b.clone.map_or("—".to_string(), |m| m.to_string()),
                captured(
                    b.captured_at
                        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string()),
                    b.capture_last_save,
                ),
                seconds(b.source_downtime_ms),
            ]
        })
        .collect()
}

/// When the data was captured; a clone of a source with nothing new to save
/// says it is that source's last save.
fn captured(at: Option<String>, last_save: bool) -> String {
    at.map_or("—".to_string(), |when| {
        if last_save {
            format!("{when} (last save)")
        } else {
            when
        }
    })
}

/// Milliseconds as seconds with one decimal; `—` when there is none.
fn seconds(ms: Option<u64>) -> String {
    ms.map_or("—".to_string(), |ms| {
        format!("{:.1} s", ms as f64 / 1000.0)
    })
}

/// What the last finished clone, snapshot or reset did, in one line.
fn last_clone_line(last: &devflow_core::services::LastClone) -> String {
    let how = last.method.map_or("no copy".to_string(), |m| m.to_string());
    let source = match last.source_downtime_ms {
        Some(ms) => format!("source stopped {}", seconds(Some(ms))),
        None => "no source stopped".to_string(),
    };
    format!(
        "{} {} ({how}), {source}, {}",
        last.operation,
        &last.target.simple().to_string()[..8],
        last.finished_at.format("%Y-%m-%d %H:%M:%S UTC")
    )
}

fn source_label(branch: &BranchView) -> String {
    use catalog::SourceRef;
    match branch.source {
        SourceRef::Empty => "empty".into(),
        SourceRef::Branch { id } => format!("branch:{}", &id.simple().to_string()[..8]),
        SourceRef::Snapshot { id } => format!("snapshot:{}", &id.simple().to_string()[..8]),
    }
}

/// `(instance id, workspace name)` of every workspace with a receipt, to put
/// workspace names on branches.
fn workspace_names(project: &Project) -> Vec<(Uuid, String)> {
    devflow_core::worktree::list(project)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|w| {
            let receipt = devflow_core::receipt::Receipt::load(&w.git_dir).ok()??;
            Some((receipt.instance_id, w.name))
        })
        .collect()
}

fn show_status(
    ctx: &Ctx,
    command: &str,
    project: &Project,
    name: Option<&str>,
    probe: bool,
) -> Result<ExitCode> {
    let services = select_many(project, name)?;
    let statuses: Vec<ServiceStatus> = services
        .iter()
        .map(|svc| svc.status(probe))
        .collect::<Result<_>>()?;
    if ctx.json {
        output::emit(
            ctx,
            command,
            &json!({ "project": ProjectInfo::of(project), "services": statuses }),
        )?;
        return Ok(ExitCode::SUCCESS);
    }
    let names = workspace_names(project);
    for (index, status) in statuses.iter().enumerate() {
        if index > 0 {
            say("");
        }
        say(format!(
            "{} ({}, {})",
            status.key, status.engine, status.image
        ));
        let mut rows = vec![
            ("docker", docker_label(status)),
            ("cow", output::snake(&status.cow)),
            ("require_cow", status.require_cow.to_string()),
            ("snapshots", status.snapshots.to_string()),
        ];
        if let Some(note) = &status.cow_note {
            rows.push(("cow_note", note.clone()));
        }
        if let Some(last) = &status.last_clone {
            rows.push(("last clone", last_clone_line(last)));
        }
        if status.unfinished > 0 {
            rows.push((
                "unfinished",
                format!(
                    "{} operation(s); run `devflow service {} reconcile`",
                    status.unfinished, status.key
                ),
            ));
        }
        say(redact::text(&output::key_values(&rows)));
        if status.branches.is_empty() {
            say("no branches");
        } else {
            say(output::table(
                &[
                    "BRANCH",
                    "WORKSPACE",
                    "STATE",
                    "CONTAINER",
                    "ENDPOINT",
                    "SOURCE",
                    "CLONE",
                    "CAPTURED",
                    "SOURCE DOWN",
                ],
                &branch_rows(&status.branches, &names),
            ));
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn docker_label(status: &ServiceStatus) -> String {
    let short = status.docker.chars().take(12).collect::<String>();
    if status.orbstack {
        format!("{short} (OrbStack)")
    } else {
        short
    }
}

fn show_list(
    ctx: &Ctx,
    command: &str,
    project: &Project,
    name: Option<&str>,
    orphans: bool,
) -> Result<ExitCode> {
    let found = if orphans {
        Some(workspace::orphans(project)?)
    } else {
        None
    };
    let mut listing = Vec::new();
    // Orphans are about recorded data, so they do not need a configured service.
    if !orphans || !configured(project).is_empty() {
        for svc in select_many(project, name)? {
            listing.push((svc.key.clone(), svc.branch_views()?, svc.snapshot_views()?));
        }
    }
    if ctx.json {
        let services: Vec<_> = listing
            .iter()
            .map(|(key, branches, snapshots)| {
                json!({ "key": key, "branches": branches, "snapshots": snapshots })
            })
            .collect();
        let mut body = json!({ "project": ProjectInfo::of(project), "services": services });
        if let Some(orphans) = &found {
            body["orphans"] = serde_json::to_value(orphans)?;
        }
        output::emit(ctx, command, &body)?;
        return Ok(ExitCode::SUCCESS);
    }
    let names = workspace_names(project);
    if let Some(orphans) = &found {
        say_incomplete(orphans);
        if orphans.services.is_empty() && orphans.branches.is_empty() && orphans.is_complete() {
            say("No orphans.");
        }
        for key in &orphans.services {
            say(format!(
                "service '{key}' is no longer configured; its data is kept (remove it with \
                 `devflow service {key} forget`)"
            ));
        }
        for branch in &orphans.branches {
            say(format!(
                "{}: branch {} ({}): {}",
                branch.service, branch.branch, branch.name, branch.reason
            ));
        }
        return Ok(ExitCode::SUCCESS);
    }
    for (index, (key, branches, snapshots)) in listing.iter().enumerate() {
        if index > 0 {
            say("");
        }
        say(format!("{key}:"));
        if branches.is_empty() {
            say("  no branches");
        } else {
            say(output::table(
                &[
                    "BRANCH",
                    "WORKSPACE",
                    "STATE",
                    "CONTAINER",
                    "ENDPOINT",
                    "SOURCE",
                    "CLONE",
                    "CAPTURED",
                    "SOURCE DOWN",
                ],
                &branch_rows(branches, &names),
            ));
        }
        if !snapshots.is_empty() {
            say(snapshot_table(snapshots));
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// A scan that could not read every receipt cannot say who owns a branch:
/// say so instead of listing fewer orphans (or none).
fn say_incomplete(orphans: &workspace::Orphans) {
    for problem in &orphans.incomplete {
        say(redact::text(&format!(
            "workspace '{}': its receipt cannot be read ({}); who owns which branch is unknown \
             until it is repaired, so no branch is listed as an orphan",
            problem.workspace, problem.reason
        )));
    }
}

fn snapshot_table(snapshots: &[SnapshotView]) -> String {
    let rows: Vec<Vec<String>> = snapshots
        .iter()
        .map(|s| {
            vec![
                format!("{}@v{}", s.name, s.version),
                output::snake(&s.state),
                s.clone.map_or("—".to_string(), |m| m.to_string()),
                captured(
                    s.captured_at
                        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string()),
                    s.capture_last_save,
                ),
                s.created_at.format("%Y-%m-%d %H:%M").to_string(),
            ]
        })
        .collect();
    output::table(
        &["SNAPSHOT", "STATE", "CLONE", "CAPTURED", "CREATED (UTC)"],
        &rows,
    )
}

// --- up, down, url -------------------------------------------------------------

/// The branches a command acts on: `--all`, or the workspace's own.
fn targets(
    ctx: &Ctx,
    project: &Project,
    svc: &Service,
    args: &TargetArgs,
) -> Result<Vec<(String, Uuid)>> {
    if args.all {
        return Ok(svc
            .records()?
            .branches
            .iter()
            .filter(|b| b.state == RecordState::Ready)
            .map(|b| (b.name.clone(), b.id))
            .collect());
    }
    let workspace = workspace_arg(ctx, project, args.workspace.as_deref())?;
    let (_, record) = setup::service_branch(project, &workspace, &svc.key)?;
    Ok(vec![(workspace.name, record.id)])
}

fn start(
    ctx: &Ctx,
    command: &str,
    project: &Project,
    name: Option<&str>,
    args: TargetArgs,
) -> Result<ExitCode> {
    let svc = select(project, name)?;
    let mut started = Vec::new();
    for (workspace, id) in targets(ctx, project, &svc, &args)? {
        let endpoint = svc.up(id)?;
        started.push((workspace, id, endpoint));
    }
    if ctx.json {
        let items: Vec<_> = started
            .iter()
            .map(|(workspace, id, endpoint)| {
                json!({
                    "workspace": workspace,
                    "branch": id,
                    "endpoint": format!("127.0.0.1:{}", endpoint.port),
                    "container_endpoint": endpoint.container_endpoint,
                })
            })
            .collect();
        output::emit(
            ctx,
            command,
            &json!({ "service": svc.key, "started": items }),
        )?;
    } else if started.is_empty() {
        say("No branches.");
    } else {
        for (workspace, _, endpoint) in &started {
            say(format!(
                "{} Started {} ({workspace}) on 127.0.0.1:{}",
                Palette::stdout().green("✓"),
                svc.key,
                endpoint.port
            ));
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn stop(
    ctx: &Ctx,
    command: &str,
    project: &Project,
    name: Option<&str>,
    args: TargetArgs,
) -> Result<ExitCode> {
    let svc = select(project, name)?;
    let mut stopped = Vec::new();
    for (workspace, id) in targets(ctx, project, &svc, &args)? {
        let was_running = svc.down(id)?;
        stopped.push((workspace, id, was_running));
    }
    if ctx.json {
        let items: Vec<_> = stopped
            .iter()
            .map(|(workspace, id, was)| json!({ "workspace": workspace, "branch": id, "was_running": was }))
            .collect();
        output::emit(
            ctx,
            command,
            &json!({ "service": svc.key, "stopped": items }),
        )?;
    } else if stopped.is_empty() {
        say("No branches.");
    } else {
        for (workspace, _, was_running) in &stopped {
            say(format!(
                "{} {} ({workspace}) {}",
                Palette::stdout().green("✓"),
                svc.key,
                if *was_running {
                    "stopped; its data is kept"
                } else {
                    "was not running"
                }
            ));
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn url(
    ctx: &Ctx,
    command: &str,
    project: &Project,
    name: Option<&str>,
    arg: WorkspaceArg,
) -> Result<ExitCode> {
    let svc = select(project, name)?;
    let workspace = workspace_arg(ctx, project, arg.workspace.as_deref())?;
    let (_, record) = setup::service_branch(project, &workspace, &svc.key)?;
    let endpoint = svc.endpoint(&record)?;
    if ctx.json {
        // Handing out the credential is the point of this command.
        let body = json!({
            "workspace": workspace.name,
            "service": svc.key,
            "url": endpoint.url,
            "port": endpoint.port,
            "container_endpoint": endpoint.container_endpoint,
            "branch_id": record.id,
        });
        output::emit_unredacted(ctx, command, &body)?;
    } else {
        say(&endpoint.url);
    }
    Ok(ExitCode::SUCCESS)
}

// --- snapshots and reset ---------------------------------------------------------

fn snapshot(
    ctx: &Ctx,
    command: &str,
    project: &Project,
    name: Option<&str>,
    action: SnapshotCommand,
) -> Result<ExitCode> {
    // A malformed reference is refused before Docker is contacted.
    let delete = match &action {
        SnapshotCommand::Delete { snapshot } => Some(parse_snapshot_ref(snapshot)?),
        _ => None,
    };
    let svc = select(project, name)?;
    match action {
        SnapshotCommand::Create { name: label, from } => {
            let workspace = workspace_arg(ctx, project, from.as_deref())?;
            let record = setup::service_snapshot(project, &workspace, &svc.key, &label, None)?;
            if ctx.json {
                output::emit(
                    ctx,
                    command,
                    &json!({ "workspace": workspace.name, "service": svc.key, "snapshot": svc
                        .snapshot_views()?
                        .into_iter()
                        .find(|s| s.id == record.id) }),
                )?;
            } else {
                say(format!(
                    "{} Created snapshot {}@v{} of {} from {} ({})",
                    Palette::stdout().green("✓"),
                    record.name,
                    record.version,
                    svc.key,
                    workspace.name,
                    record.clone.map_or("empty".to_string(), |m| m.to_string())
                ));
            }
        }
        SnapshotCommand::List => {
            let snapshots = svc.snapshot_views()?;
            if ctx.json {
                output::emit(
                    ctx,
                    command,
                    &json!({ "service": svc.key, "snapshots": snapshots }),
                )?;
            } else if snapshots.is_empty() {
                say("No snapshots.");
            } else {
                say(snapshot_table(&snapshots));
            }
        }
        SnapshotCommand::Delete { .. } => {
            let (label, version) = delete.context("a delete names a snapshot version")?;
            svc.remove_snapshot(&label, version)?;
            if ctx.json {
                output::emit(
                    ctx,
                    command,
                    &json!({ "service": svc.key, "deleted": { "name": label, "version": version } }),
                )?;
            } else {
                say(format!("Deleted snapshot {label}@v{version}"));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `NAME@vN` (the version is required: deleting "the latest" is too easy to get wrong).
fn parse_snapshot_ref(text: &str) -> Result<(String, u32)> {
    let (name, version) = text
        .rsplit_once("@v")
        .filter(|(name, _)| !name.is_empty())
        .ok_or_else(|| anyhow!("expected NAME@vN (for example seed@v2), got '{text}'"))?;
    let version = version
        .parse::<u32>()
        .with_context(|| format!("'{version}' is not a snapshot version"))?;
    Ok((name.to_string(), version))
}

fn reset(
    ctx: &Ctx,
    command: &str,
    project: &Project,
    name: Option<&str>,
    args: ResetArgs,
) -> Result<ExitCode> {
    let svc = select(project, name)?;
    let workspace = workspace_arg(ctx, project, args.workspace.as_deref())?;
    let source = args
        .to
        .as_ref()
        .map_or_else(|| "its recorded source".to_string(), ToString::to_string);
    let record = setup::service_reset(project, &workspace, &svc.key, args.to, None)?;
    if ctx.json {
        let view = svc.branch_views()?.into_iter().find(|b| b.id == record.id);
        output::emit(
            ctx,
            command,
            &json!({ "workspace": workspace.name, "service": svc.key, "branch": view, "source": source }),
        )?;
    } else {
        say(format!(
            "{} Reset {} of {} from {source} ({})",
            Palette::stdout().green("✓"),
            svc.key,
            workspace.name,
            record.clone.map_or("empty".to_string(), |m| m.to_string())
        ));
    }
    Ok(ExitCode::SUCCESS)
}

// --- reconcile, drop, forget -------------------------------------------------------

fn reconcile(ctx: &Ctx, command: &str, project: &Project, name: Option<&str>) -> Result<ExitCode> {
    let mut reports = Vec::new();
    for svc in select_many(project, name)? {
        reports.push((svc.key.clone(), svc.reconcile()?));
    }
    let orphans = workspace::orphans(project)?;
    if ctx.json {
        let items: Vec<_> = reports
            .iter()
            .map(|(key, report)| json!({ "key": key, "report": report }))
            .collect();
        output::emit(
            ctx,
            command,
            &json!({ "services": items, "orphans": orphans }),
        )?;
        return Ok(ExitCode::SUCCESS);
    }
    let mut nothing = orphans.is_complete();
    say_incomplete(&orphans);
    for (key, report) in &reports {
        for action in &report.actions {
            nothing = false;
            say(redact::text(&format!("- {key}: {action}")));
        }
        for line in &report.in_progress {
            nothing = false;
            say(format!("- {key}: {line}"));
        }
        for orphan in &report.orphans {
            nothing = false;
            say(redact::text(&format!("- {key}: {orphan} (kept)")));
        }
        for problem in &report.problems {
            nothing = false;
            say(redact::text(&format!(
                "- {key}: needs an operator: {problem}"
            )));
        }
    }
    for key in &orphans.services {
        nothing = false;
        say(format!(
            "- service '{key}' is no longer configured; `devflow service {key} forget` removes its data"
        ));
    }
    for branch in &orphans.branches {
        nothing = false;
        say(format!(
            "- {}: branch {} ({}): {}",
            branch.service, branch.branch, branch.name, branch.reason
        ));
    }
    if nothing {
        say("Nothing to reconcile.");
    }
    Ok(ExitCode::SUCCESS)
}

fn drop_branch(
    ctx: &Ctx,
    command: &str,
    project: &Project,
    name: Option<&str>,
    id: devflow_core::ids::IdRef,
) -> Result<ExitCode> {
    let svc = select(project, name)?;
    let id = svc.resolve_branch(&id)?;
    svc.remove_branch(id)?;
    if ctx.json {
        output::emit(ctx, command, &json!({ "service": svc.key, "dropped": id }))?;
    } else {
        say(format!("Dropped branch {id} of {}", svc.key));
    }
    Ok(ExitCode::SUCCESS)
}

fn forget(ctx: &Ctx, command: &str, project: &Project, name: Option<&str>) -> Result<ExitCode> {
    let key = name.ok_or_else(|| {
        anyhow!("name the service to forget: `devflow service NAME forget` (see `devflow service list --orphans`)")
    })?;
    let removed = workspace::forget(project, key)?;
    if ctx.json {
        output::emit(ctx, command, &json!({ "service": key, "removed": removed }))?;
    } else if removed.is_empty() {
        say(format!("Forgot service '{key}' (nothing was recorded)."));
    } else {
        for line in &removed {
            say(format!("- removed {line}"));
        }
        note(format!("forgot service '{key}'"));
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_references_need_a_version() {
        assert_eq!(
            parse_snapshot_ref("seed@v2").unwrap(),
            ("seed".to_string(), 2)
        );
        assert_eq!(
            parse_snapshot_ref("a@vb@v10").unwrap(),
            ("a@vb".to_string(), 10)
        );
        assert!(parse_snapshot_ref("seed").is_err());
        assert!(parse_snapshot_ref("@v1").is_err());
        assert!(parse_snapshot_ref("seed@vx").is_err());
        assert!(parse_snapshot_ref("seed@v-1").is_err());
    }
}
