//! `devflow service NAME ...` for a `sqlite` service.
//!
//! A SQLite database is a file in each workspace: nothing runs, so there is
//! nothing to start, stop or branch in a container, and the commands that
//! manage containers (`up`, `down`) say so. What the others have is here:
//! `status` and `list` show where each workspace's file is and where its data
//! came from, `url` prints the `sqlite:` URL, `snapshot` keeps consistent
//! copies, `reset` replaces a workspace's file with a fresh copy of its
//! source, and `drop` removes it. The handlers parse arguments and render;
//! every decision is `devflow_core::setup`'s (the receipt, the locks) and
//! `devflow_core::sqlite`'s (the copy under SQLite's own locks).
use super::db::workspace_arg;
use super::output::{self, Palette, say};
use super::service::{
    ListArgs, ResetArgs, ServiceCommand, SnapshotCommand, StatusArgs, TargetArgs, WorkspaceArg,
    parse_snapshot_ref,
};
use super::{Ctx, ProjectInfo};
use anyhow::{Context, Result, bail};
use devflow_core::Project;
use devflow_core::config::SqliteSection;
use devflow_core::ids::IdRef;
use devflow_core::setup::{self, SqliteDatabase};
use devflow_core::sqlite::{self, Snapshot, Store};
use serde_json::json;
use std::process::ExitCode;

/// The sqlite service a `devflow service` command acts on, when it is one:
/// `name` names it, or `name` is absent and the project has no container
/// service and exactly one sqlite service. More than one without a name is an
/// error that lists them.
pub fn target(project: &Project, name: Option<&str>) -> Result<Option<String>> {
    if let Some(name) = name {
        return Ok(project
            .config
            .services
            .get(name)
            .filter(|service| service.as_sqlite().is_some())
            .map(|_| name.to_string()));
    }
    if project.config.containers().next().is_some() {
        return Ok(None);
    }
    let keys: Vec<&str> = project.config.sqlite_services().map(|(k, _)| k).collect();
    match keys.as_slice() {
        [] => Ok(None),
        [only] => Ok(Some((*only).to_string())),
        many => bail!(
            "this project has {} sqlite services ({}); name one: `devflow service NAME …`",
            many.len(),
            many.join(", ")
        ),
    }
}

pub fn run(
    ctx: &Ctx,
    command: &str,
    project: &Project,
    key: &str,
    action: ServiceCommand,
) -> Result<ExitCode> {
    match action {
        ServiceCommand::Status(args) => status(ctx, command, project, key, args),
        ServiceCommand::List(args) => list(ctx, command, project, key, args),
        ServiceCommand::Up(args) => nothing_to_run(ctx, command, key, "started", args),
        ServiceCommand::Down(args) => nothing_to_run(ctx, command, key, "stopped", args),
        ServiceCommand::Url(arg) => url(ctx, command, project, key, arg),
        ServiceCommand::Snapshot { command: action } => {
            snapshot(ctx, command, project, key, action)
        }
        ServiceCommand::Reset(args) => reset(ctx, command, project, key, args),
        ServiceCommand::Reconcile => reconcile(ctx, command, project, key),
        ServiceCommand::Drop { id, lost } => drop_database(ctx, command, project, key, id, lost),
        ServiceCommand::Forget => bail!(
            "service '{key}' is still configured: its snapshots are forgotten when the \
             configuration no longer has it"
        ),
    }
}

fn section(project: &Project, key: &str) -> Result<SqliteSection> {
    project
        .config
        .services
        .get(key)
        .and_then(|s| s.as_sqlite())
        .cloned()
        .with_context(|| {
            format!(
                "{} has no sqlite service '{key}'",
                project.config_path.display()
            )
        })
}

fn size(bytes: Option<u64>) -> String {
    bytes.map_or("—".to_string(), |bytes| {
        const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
        let mut value = bytes as f64;
        let mut unit = 0;
        while value >= 1024.0 && unit < UNITS.len() - 1 {
            value /= 1024.0;
            unit += 1;
        }
        if unit == 0 {
            format!("{bytes} B")
        } else {
            format!("{value:.1} {}", UNITS[unit])
        }
    })
}

fn database_rows(databases: &[SqliteDatabase]) -> Vec<Vec<String>> {
    databases
        .iter()
        .map(|d| {
            vec![
                d.id.map_or("—".to_string(), |id| {
                    id.simple().to_string()[..8].to_string()
                }),
                d.workspace.clone(),
                if d.file.exists {
                    size(d.file.bytes)
                } else {
                    "not created yet".to_string()
                },
                d.file.journal_mode.clone().unwrap_or_else(|| "—".into()),
                d.source.clone().unwrap_or_else(|| "—".into()),
                d.cloned.map_or("—".to_string(), |m| m.to_string()),
                match (d.source.is_some(), d.owned) {
                    (false, _) => "not set up".to_string(),
                    (true, true) => "devflow's".to_string(),
                    (true, false) => "yours".to_string(),
                },
            ]
        })
        .collect()
}

const DATABASE_HEADERS: [&str; 7] = [
    "ID",
    "WORKSPACE",
    "FILE",
    "MODE",
    "SOURCE",
    "CLONE",
    "OWNER",
];

fn snapshot_rows(snapshots: &[Snapshot]) -> Vec<Vec<String>> {
    snapshots
        .iter()
        .map(|s| {
            vec![
                s.label(),
                s.how.map_or("—".to_string(), |h| output::snake(&h)),
                size(s.bytes),
                s.source.clone(),
                s.created_at.format("%Y-%m-%d %H:%M").to_string(),
            ]
        })
        .collect()
}

const SNAPSHOT_HEADERS: [&str; 5] = ["SNAPSHOT", "TAKEN BY", "SIZE", "FROM", "CREATED (UTC)"];

fn status(
    ctx: &Ctx,
    command: &str,
    project: &Project,
    key: &str,
    _args: StatusArgs,
) -> Result<ExitCode> {
    let section = section(project, key)?;
    let databases = setup::sqlite_databases(project, key)?;
    let store = Store::open(project.id(), key)?;
    let snapshots = store.list()?;
    let unfinished = store.unfinished()?.len();
    if ctx.json {
        output::emit(
            ctx,
            command,
            &json!({
                "project": ProjectInfo::of(project),
                "services": [{
                    "key": key,
                    "engine": "sqlite",
                    "path": section.path,
                    "library": sqlite::library_version(),
                    "require_cow": section.require_cow,
                    "busy_wait_ms": section.busy_wait().as_millis() as u64,
                    "snapshots": snapshots.len(),
                    "unfinished": unfinished,
                    "databases": databases,
                }],
            }),
        )?;
        return Ok(ExitCode::SUCCESS);
    }
    say(format!("{key} (sqlite, {})", section.path));
    let mut rows = vec![
        (
            "library",
            format!("SQLite {} (bundled)", sqlite::library_version()),
        ),
        ("require_cow", section.require_cow.to_string()),
        (
            "busy_wait",
            format!("{} s", section.busy_wait().as_secs_f64()),
        ),
        ("snapshots", snapshots.len().to_string()),
    ];
    if unfinished > 0 {
        rows.push((
            "unfinished",
            format!(
                "{unfinished} snapshot operation(s); `devflow service {key} reconcile` settles them"
            ),
        ));
    }
    say(output::key_values(&rows));
    say(output::table(&DATABASE_HEADERS, &database_rows(&databases)));
    Ok(ExitCode::SUCCESS)
}

fn list(
    ctx: &Ctx,
    command: &str,
    project: &Project,
    key: &str,
    args: ListArgs,
) -> Result<ExitCode> {
    if args.orphans {
        // The files are the worktrees' own, and snapshots belong to a
        // configured service: nothing here can be orphaned.
        if ctx.json {
            output::emit(
                ctx,
                command,
                &json!({ "orphans": { "services": [], "branches": [] } }),
            )?;
        } else {
            say("No orphans.");
        }
        return Ok(ExitCode::SUCCESS);
    }
    let databases = setup::sqlite_databases(project, key)?;
    let snapshots = setup::sqlite_snapshots(project, key)?;
    if ctx.json {
        output::emit(
            ctx,
            command,
            &json!({
                "project": ProjectInfo::of(project),
                "services": [{ "key": key, "databases": databases, "snapshots": snapshots }],
            }),
        )?;
        return Ok(ExitCode::SUCCESS);
    }
    say(format!("{key}:"));
    if databases.is_empty() {
        say("  no workspaces");
    } else {
        say(output::table(&DATABASE_HEADERS, &database_rows(&databases)));
    }
    if !snapshots.is_empty() {
        say(output::table(&SNAPSHOT_HEADERS, &snapshot_rows(&snapshots)));
    }
    Ok(ExitCode::SUCCESS)
}

fn nothing_to_run(
    ctx: &Ctx,
    command: &str,
    key: &str,
    what: &str,
    _args: TargetArgs,
) -> Result<ExitCode> {
    if ctx.json {
        output::emit(
            ctx,
            command,
            &json!({ "service": key, "engine": "sqlite", what: [] }),
        )?;
    } else {
        say(format!(
            "{key} is a SQLite file: nothing is {what}. `devflow service {key} status` shows where each workspace's file is."
        ));
    }
    Ok(ExitCode::SUCCESS)
}

fn url(
    ctx: &Ctx,
    command: &str,
    project: &Project,
    key: &str,
    arg: WorkspaceArg,
) -> Result<ExitCode> {
    let section = section(project, key)?;
    let workspace = workspace_arg(ctx, project, arg.workspace.as_deref())?;
    let file = sqlite::file_in(&workspace.path, &section);
    let url = sqlite::url_of(&file);
    if ctx.json {
        output::emit(
            ctx,
            command,
            &json!({
                "workspace": workspace.name,
                "service": key,
                "url": url,
                "path": file,
                "exists": file.exists(),
            }),
        )?;
    } else {
        say(&url);
    }
    Ok(ExitCode::SUCCESS)
}

fn snapshot(
    ctx: &Ctx,
    command: &str,
    project: &Project,
    key: &str,
    action: SnapshotCommand,
) -> Result<ExitCode> {
    // A malformed reference is refused before anything is read.
    let delete = match &action {
        SnapshotCommand::Delete { snapshot } => Some(parse_snapshot_ref(snapshot)?),
        _ => None,
    };
    match action {
        SnapshotCommand::Create { name, from } => {
            let workspace = workspace_arg(ctx, project, from.as_deref())?;
            let made = setup::sqlite_snapshot(project, &workspace, key, &name, None)?;
            if ctx.json {
                output::emit(
                    ctx,
                    command,
                    &json!({ "workspace": workspace.name, "service": key, "snapshot": made }),
                )?;
            } else {
                say(format!(
                    "{} Created snapshot {} of {key} from {} ({}, {})",
                    Palette::stdout().green("✓"),
                    made.label(),
                    workspace.name,
                    made.how.map_or("copy".to_string(), |h| output::snake(&h)),
                    size(made.bytes)
                ));
            }
        }
        SnapshotCommand::List => {
            let snapshots = setup::sqlite_snapshots(project, key)?;
            if ctx.json {
                output::emit(
                    ctx,
                    command,
                    &json!({ "service": key, "snapshots": snapshots }),
                )?;
            } else if snapshots.is_empty() {
                say("No snapshots.");
            } else {
                say(output::table(&SNAPSHOT_HEADERS, &snapshot_rows(&snapshots)));
            }
        }
        SnapshotCommand::Delete { .. } => {
            let (name, version) = delete.context("a delete names a snapshot version")?;
            setup::sqlite_snapshot_delete(project, key, &name, version)?;
            if ctx.json {
                output::emit(
                    ctx,
                    command,
                    &json!({ "service": key, "deleted": { "name": name, "version": version } }),
                )?;
            } else {
                say(format!("Deleted snapshot {name}@v{version}"));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn reset(
    ctx: &Ctx,
    command: &str,
    project: &Project,
    key: &str,
    args: ResetArgs,
) -> Result<ExitCode> {
    let workspace = workspace_arg(ctx, project, args.workspace.as_deref())?;
    let done = setup::sqlite_reset(project, &workspace, key, args.to, None)?;
    if ctx.json {
        output::emit(
            ctx,
            command,
            &json!({ "workspace": workspace.name, "service": key, "reset": done }),
        )?;
    } else {
        say(format!(
            "{} Reset {key} of {} from {} ({})",
            Palette::stdout().green("✓"),
            workspace.name,
            done.source,
            done.method.map_or("empty".to_string(), |m| m.to_string())
        ));
    }
    Ok(ExitCode::SUCCESS)
}

fn reconcile(ctx: &Ctx, command: &str, project: &Project, key: &str) -> Result<ExitCode> {
    let report = setup::sqlite_reconcile(project, key)?;
    if ctx.json {
        output::emit(
            ctx,
            command,
            &json!({ "services": [{ "key": key, "report": report }], "orphans": {} }),
        )?;
        return Ok(ExitCode::SUCCESS);
    }
    for label in &report.abandoned {
        say(format!(
            "- {key}: removed the snapshot {label}, whose making was abandoned"
        ));
    }
    for label in &report.removed {
        say(format!("- {key}: finished removing the snapshot {label}"));
    }
    for label in &report.damaged {
        say(format!(
            "- {key}: the file of snapshot {label} is gone or has changed; its data is lost (delete the snapshot)"
        ));
    }
    if report.is_empty() {
        say("Nothing to reconcile.");
    }
    Ok(ExitCode::SUCCESS)
}

fn drop_database(
    ctx: &Ctx,
    command: &str,
    project: &Project,
    key: &str,
    id: IdRef,
    lost: bool,
) -> Result<ExitCode> {
    if lost {
        bail!("--lost is for container branches whose volume is gone; a SQLite database is a file");
    }
    // The ids `list` shows: the identity each workspace's binding records.
    let mut bound = Vec::new();
    for workspace in devflow_core::worktree::list(project)? {
        let Some(receipt) = setup::receipt(project, &workspace).ok().flatten() else {
            continue;
        };
        if let Some(branch) = receipt.services.get(key).and_then(|b| b.branch_id) {
            bound.push((workspace, branch));
        }
    }
    let id = id.resolve("database", bound.iter().map(|(_, id)| *id))?;
    let workspace = bound
        .into_iter()
        .find(|(_, branch)| *branch == id)
        .map(|(workspace, _)| workspace)
        .with_context(|| format!("no workspace has the database {id} of service '{key}'"))?;
    let removed = setup::sqlite_drop(project, &workspace, key, None)?;
    if ctx.json {
        output::emit(
            ctx,
            command,
            &json!({ "service": key, "dropped": id, "workspace": workspace.name, "removed": removed }),
        )?;
    } else {
        say(format!(
            "Dropped the database of {key} in {}{}",
            workspace.name,
            if removed { "" } else { " (there was no file)" }
        ));
    }
    Ok(ExitCode::SUCCESS)
}
