//! `devflow db ...`: the project's Postgres server, branches and snapshots.
use super::output::{self, Palette, note, say};
use super::{Ctx, ProjectInfo, system};
use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::{Args, Subcommand};
use devflow_core::Project;
use devflow_core::conn::{self, Params};
use devflow_core::db::{BranchCredentials, DataSource, Server, ServerStatus};
use devflow_core::receipt::{DataBinding, Receipt};
use devflow_core::redact;
use devflow_core::setup;
use devflow_core::worktree::{self, Workspace};
use serde_json::json;
use std::process::{Command, ExitCode};

#[derive(Subcommand)]
pub enum DbCommand {
    /// Show the server state, version and copy-on-write capability
    Status,
    /// Start the server (managed) or check it (external)
    Up,
    /// Stop the managed server; every branch of the project becomes unavailable
    Down,
    /// Print a workspace's database URL, including its password
    Url(WorkspaceArg),
    /// Open psql on a workspace's database
    Psql(WorkspaceArg),
    /// Replace a workspace's data with a fresh clone of its source (or of --to)
    Reset(ResetArgs),
    /// Finish or roll back operations interrupted by a crash
    Reconcile,
    /// Named, versioned, read-only copies of a workspace's data
    Snapshot {
        #[command(subcommand)]
        command: SnapshotCommand,
    },
    /// Database branches on the server (for tools that provision sandboxes)
    Branch {
        #[command(subcommand)]
        command: BranchCommand,
    },
}

#[derive(Args)]
pub struct WorkspaceArg {
    /// Workspace name (default: the workspace containing the current directory)
    pub workspace: Option<String>,
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

#[derive(Subcommand)]
pub enum BranchCommand {
    /// List the branches of this project
    List,
    /// Create a branch and print its credentials
    Create(BranchCreateArgs),
    /// Drop a branch and its role
    Delete {
        /// Branch id (see `devflow db branch list`)
        id: uuid::Uuid,
    },
}

#[derive(Args)]
pub struct BranchCreateArgs {
    /// Display name (default: generated)
    #[arg(long)]
    pub name: Option<String>,
    /// Data origin: a workspace, snapshot:NAME[@vN], branch:ID or empty (default: empty)
    #[arg(long, value_name = "SRC")]
    pub data_from: Option<DataSource>,
    /// Make retries idempotent: if a branch already exists for this instance,
    /// rotate its password and print the new credentials instead of creating
    /// another branch. Rotation invalidates any URL handed out earlier.
    #[arg(long, value_name = "UUID")]
    pub instance_id: Option<uuid::Uuid>,
}

impl DbCommand {
    /// Document name for `--json`; `None` for `db psql`, which is interactive.
    pub fn json_name(&self) -> Option<&'static str> {
        Some(match self {
            DbCommand::Status => "db status",
            DbCommand::Up => "db up",
            DbCommand::Down => "db down",
            DbCommand::Url(_) => "db url",
            DbCommand::Psql(_) => return None,
            DbCommand::Reset(_) => "db reset",
            DbCommand::Reconcile => "db reconcile",
            DbCommand::Snapshot { command } => match command {
                SnapshotCommand::Create { .. } => "db snapshot create",
                SnapshotCommand::List => "db snapshot list",
                SnapshotCommand::Delete { .. } => "db snapshot delete",
            },
            DbCommand::Branch { command } => match command {
                BranchCommand::List => "db branch list",
                BranchCommand::Create(_) => "db branch create",
                BranchCommand::Delete { .. } => "db branch delete",
            },
        })
    }
}

pub fn run(ctx: &Ctx, command: DbCommand) -> Result<ExitCode> {
    let name = command.json_name().unwrap_or("db psql");
    let project = ctx.project()?;
    match command {
        DbCommand::Status => {
            let status = server(&project)?.status();
            show_status(ctx, name, &project, &status)
        }
        DbCommand::Up => {
            let status = server(&project)?.up()?;
            show_status(ctx, name, &project, &status)
        }
        DbCommand::Down => {
            server(&project)?.down()?;
            if ctx.json {
                output::emit(
                    ctx,
                    name,
                    &json!({ "project": ProjectInfo::of(&project), "stopped": true }),
                )?;
            } else {
                say("Stopped the Postgres server.");
            }
            Ok(ExitCode::SUCCESS)
        }
        DbCommand::Url(arg) => url(ctx, name, &project, arg),
        DbCommand::Psql(arg) => psql(ctx, &project, arg),
        DbCommand::Reset(args) => reset(ctx, name, &project, args),
        DbCommand::Reconcile => {
            let report = server(&project)?.reconcile()?;
            if ctx.json {
                output::emit(ctx, name, &report)?;
            } else if report.actions.is_empty() && report.problems.is_empty() {
                say("Nothing to reconcile.");
            } else {
                report
                    .actions
                    .iter()
                    .for_each(|action| say(redact::text(&format!("- {action}"))));
                if !report.problems.is_empty() {
                    say("Needs an operator:");
                    report
                        .problems
                        .iter()
                        .for_each(|problem| say(redact::text(&format!("- {problem}"))));
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        DbCommand::Snapshot { command } => snapshot(ctx, name, &project, command),
        DbCommand::Branch { command } => branch(ctx, name, &project, command),
    }
}

fn server(project: &Project) -> Result<Server> {
    Server::for_project(project)?.ok_or_else(|| {
        anyhow!(
            "{} has no [postgres] section; add one to enable database branches",
            project.config_path.display()
        )
    })
}

// --- server ---------------------------------------------------------------

fn show_status(
    ctx: &Ctx,
    command: &str,
    project: &Project,
    status: &ServerStatus,
) -> Result<ExitCode> {
    if ctx.json {
        output::emit(
            ctx,
            command,
            &json!({ "project": ProjectInfo::of(project), "server": status }),
        )?;
        return Ok(ExitCode::SUCCESS);
    }
    let yes_no = |value: bool| if value { "yes" } else { "no" }.to_string();
    let mut rows = vec![
        ("mode", output::snake(&status.mode)),
        ("running", yes_no(status.running)),
        ("reachable", yes_no(status.reachable)),
    ];
    if let Some(endpoint) = &status.endpoint {
        rows.push(("endpoint", endpoint.clone()));
    }
    if let Some(version) = &status.server_version {
        rows.push(("version", version.clone()));
    }
    if let Some(method) = &status.file_copy_method {
        rows.push(("file_copy_method", method.clone()));
    }
    rows.push(("cow", output::snake(&status.cow)));
    say(redact::text(&output::key_values(&rows)));
    if let Some(problem) = &status.problem {
        note(format!(
            "{} {}",
            Palette::stderr().yellow("!"),
            redact::text(problem)
        ));
    }
    Ok(ExitCode::SUCCESS)
}

// --- workspace databases ----------------------------------------------------

fn workspace_arg(ctx: &Ctx, project: &Project, name: Option<&str>) -> Result<Workspace> {
    match name {
        Some(name) => worktree::find(project, name)?
            .ok_or_else(|| anyhow!("no workspace named '{name}'; see `devflow list`")),
        None => worktree::containing(project, &ctx.cwd)?.ok_or_else(|| {
            anyhow!(
                "not inside a workspace of {}; pass the workspace name",
                project.name()
            )
        }),
    }
}

/// The database a workspace is bound to, from its receipt (no server contact).
fn binding(project: &Project, workspace: &Workspace) -> Result<DataBinding> {
    let receipt = Receipt::load(&workspace.git_dir)?
        .filter(|receipt| receipt.project_id == project.id())
        .ok_or_else(|| {
            anyhow!(
                "workspace '{}' has not been set up; run `devflow setup` in {}",
                workspace.name,
                workspace.path.display()
            )
        })?;
    receipt.data.ok_or_else(|| {
        anyhow!(
            "workspace '{}' has no database; configure [postgres] and run `devflow setup`",
            workspace.name
        )
    })
}

fn url(ctx: &Ctx, command: &str, project: &Project, arg: WorkspaceArg) -> Result<ExitCode> {
    let workspace = workspace_arg(ctx, project, arg.workspace.as_deref())?;
    let data = binding(project, &workspace)?;
    if ctx.json {
        let body = json!({
            "workspace": workspace.name,
            "url": data.url,
            "database": data.database,
            "role": data.role,
            "branch_id": data.branch_id,
        });
        output::emit_unredacted(ctx, command, &body)?;
    } else {
        say(&data.url);
    }
    Ok(ExitCode::SUCCESS)
}

/// Open psql on the workspace's database.
///
/// No connection data goes on a command line: argv is readable by every local
/// user, while a process's environment is readable only by the same user (and
/// root), the same audience as a 0600 file. So psql gets no arguments at all
/// and its environment is built from the parsed URL ([`Params`], which reads
/// it the way libpq does), after removing every connection variable the
/// caller's shell exported: an inherited `PGPASSWORD` or `PGHOST` would
/// otherwise override or redirect the workspace's own.
///
/// devflow replaces itself with psql. Nothing has to outlive it (no file to
/// remove), so psql keeps its own handling of Ctrl-C and `SIGTERM`, and its
/// exit status is ours.
fn psql(ctx: &Ctx, project: &Project, arg: WorkspaceArg) -> Result<ExitCode> {
    let workspace = workspace_arg(ctx, project, arg.workspace.as_deref())?;
    let data = binding(project, &workspace)?;
    let params = Params::parse(&data.url)
        .context("the workspace's database URL cannot be used with psql")?;
    if let Some(psql) = system::which("psql") {
        let mut command = Command::new(psql);
        set_connection_env(&mut command, params.env());
        return system::exec(&mut command);
    }
    // No local client: use the one inside the managed container, but only
    // for a branch that server provisioned. A URL-only attachment may point at
    // any server; redirecting it to the container would open the wrong database.
    if data.branch_id.is_some()
        && let Some(server) = Server::for_project(project)?
        && let Some(managed) = server.managed()
    {
        ensure!(
            system::which("docker").is_some(),
            "neither psql nor docker was found on PATH"
        );
        let vars = container_env(&params);
        let mut command = Command::new("docker");
        command
            .args(["compose", "-p", &managed.compose_project, "-f"])
            .arg(managed.dir.join("compose.yaml"))
            .arg("exec");
        // `-e NAME` without a value forwards the docker client's own value, so
        // the values exist only in the environment of the client process.
        for (name, _) in &vars {
            command.args(["-e", name]);
        }
        command.args(["pg", "psql"]);
        set_connection_env(&mut command, vars);
        return system::exec(&mut command);
    }
    bail!("psql was not found on PATH; install the PostgreSQL client tools")
}

/// The connection environment of `command`: nothing inherited, `vars` set.
fn set_connection_env(command: &mut Command, vars: Vec<(&'static str, String)>) {
    for name in conn::libpq_env_vars() {
        command.env_remove(name);
    }
    command.envs(vars);
}

/// The connection variables for the psql inside the managed container, where
/// the server is at `127.0.0.1:5432` whatever address the host reaches it by.
fn container_env(params: &Params) -> Vec<(&'static str, String)> {
    let mut vars: Vec<_> = params
        .env()
        .into_iter()
        .filter(|(name, _)| !matches!(*name, "PGHOST" | "PGHOSTADDR" | "PGPORT"))
        .collect();
    vars.push(("PGHOST", "127.0.0.1".into()));
    if params.get("hostaddr").is_some() {
        vars.push(("PGHOSTADDR", "127.0.0.1".into()));
    }
    vars.push(("PGPORT", "5432".into()));
    vars
}

fn reset(ctx: &Ctx, command: &str, project: &Project, args: ResetArgs) -> Result<ExitCode> {
    let workspace = workspace_arg(ctx, project, args.workspace.as_deref())?;
    let source = args
        .to
        .as_ref()
        .map_or_else(|| "its recorded source".to_string(), ToString::to_string);
    let branch = setup::reset_data(project, &workspace, args.to)?;
    if ctx.json {
        let body = json!({ "workspace": workspace, "branch": branch, "source": source });
        output::emit(ctx, command, &body)?;
    } else {
        say(format!(
            "{} Reset {} ({}) from {source}",
            Palette::stdout().green("✓"),
            workspace.name,
            branch.db_name,
        ));
    }
    Ok(ExitCode::SUCCESS)
}

// --- snapshots ---------------------------------------------------------------

fn snapshot(
    ctx: &Ctx,
    command: &str,
    project: &Project,
    action: SnapshotCommand,
) -> Result<ExitCode> {
    match action {
        SnapshotCommand::Create { name, from } => {
            let workspace = workspace_arg(ctx, project, from.as_deref())?;
            let info = setup::snapshot(project, &workspace, &name)?;
            if ctx.json {
                output::emit(
                    ctx,
                    command,
                    &json!({ "workspace": workspace.name, "snapshot": info }),
                )?;
            } else {
                say(format!(
                    "{} Created snapshot {}@v{} from {}",
                    Palette::stdout().green("✓"),
                    info.name,
                    info.version,
                    workspace.name
                ));
            }
        }
        SnapshotCommand::List => {
            let snapshots = server(project)?.list_snapshots()?;
            if ctx.json {
                output::emit(ctx, command, &json!({ "snapshots": snapshots }))?;
            } else if snapshots.is_empty() {
                say("No snapshots.");
            } else {
                let rows: Vec<Vec<String>> = snapshots
                    .iter()
                    .map(|s| {
                        vec![
                            format!("{}@v{}", s.name, s.version),
                            output::snake(&s.status),
                            s.created_at.format("%Y-%m-%d %H:%M").to_string(),
                            s.db_name.clone(),
                        ]
                    })
                    .collect();
                say(output::table(
                    &["SNAPSHOT", "STATUS", "CREATED (UTC)", "DATABASE"],
                    &rows,
                ));
            }
        }
        SnapshotCommand::Delete { snapshot } => {
            let (name, version) = parse_snapshot_ref(&snapshot)?;
            server(project)?.delete_snapshot(&name, version)?;
            if ctx.json {
                output::emit(
                    ctx,
                    command,
                    &json!({ "deleted": { "name": name, "version": version } }),
                )?;
            } else {
                say(format!("Deleted snapshot {name}@v{version}"));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `NAME@vN` (the version is required: deleting "the latest" is too easy to get wrong).
fn parse_snapshot_ref(text: &str) -> Result<(String, i32)> {
    let (name, version) = text
        .rsplit_once("@v")
        .filter(|(name, _)| !name.is_empty())
        .ok_or_else(|| anyhow!("expected NAME@vN (for example seed@v2), got '{text}'"))?;
    let version = version
        .parse::<i32>()
        .with_context(|| format!("'{version}' is not a snapshot version"))?;
    Ok((name.to_string(), version))
}

// --- branches ------------------------------------------------------------------

fn branch(ctx: &Ctx, command: &str, project: &Project, action: BranchCommand) -> Result<ExitCode> {
    match action {
        BranchCommand::List => {
            let branches = server(project)?.list_branches()?;
            if ctx.json {
                output::emit(ctx, command, &json!({ "branches": branches }))?;
            } else if branches.is_empty() {
                say("No branches.");
            } else {
                let rows: Vec<Vec<String>> = branches
                    .iter()
                    .map(|b| {
                        vec![
                            b.id.to_string(),
                            b.name.clone(),
                            b.db_name.clone(),
                            b.source_kind.clone(),
                            output::snake(&b.status),
                            output::snake(&b.cow),
                            b.size_bytes.map_or("—".to_string(), output::bytes),
                        ]
                    })
                    .collect();
                say(output::table(
                    &["ID", "NAME", "DATABASE", "SOURCE", "STATUS", "COW", "SIZE"],
                    &rows,
                ));
            }
        }
        BranchCommand::Create(args) => branch_create(ctx, command, project, args)?,
        BranchCommand::Delete { id } => {
            server(project)?.drop_branch(id)?;
            if ctx.json {
                output::emit(ctx, command, &json!({ "deleted": id }))?;
            } else {
                say(format!("Dropped branch {id}"));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Creates a branch (or, with `--instance-id` and an existing branch, rotates
/// its credentials) and prints the credentials: handing them out is the point
/// of this command, so neither output mode redacts them.
fn branch_create(
    ctx: &Ctx,
    command: &str,
    project: &Project,
    args: BranchCreateArgs,
) -> Result<()> {
    let server = server(project)?;
    if !server.status().reachable {
        server.up()?;
    }
    let rotated = match args.instance_id {
        Some(id) => rotate_existing(&server, id)?,
        None => None,
    };
    let credentials = match rotated {
        Some(credentials) => credentials,
        None => {
            let source = args.data_from.unwrap_or(DataSource::Empty);
            let resolved = setup::resolve_source(project, &server, &source)?;
            let name = args.name.unwrap_or_else(generated_name);
            server.create_branch(&name, resolved, args.instance_id)?
        }
    };
    if ctx.json {
        return output::emit_unredacted(ctx, command, &credentials);
    }
    let rows = [
        ("branch", credentials.branch.id.to_string()),
        ("name", credentials.branch.name.clone()),
        ("host", credentials.host.clone()),
        ("port", credentials.port.to_string()),
        ("database", credentials.database.clone()),
        ("user", credentials.user.clone()),
        ("password", credentials.password.clone()),
        ("url", credentials.url.clone()),
    ];
    say(output::key_values(&rows));
    Ok(())
}

/// New credentials for the branch already created for `instance_id`, if any.
/// A half-created branch is rolled back by the rotation; when it is gone
/// afterwards the caller creates a fresh one.
fn rotate_existing(server: &Server, instance_id: uuid::Uuid) -> Result<Option<BranchCredentials>> {
    let Some(branch) = server.branch_for_instance(instance_id)? else {
        return Ok(None);
    };
    match server.rotate_credentials(branch.id) {
        Ok(credentials) => Ok(Some(credentials)),
        Err(error) if server.branch_for_instance(instance_id)?.is_none() => {
            note(format!(
                "devflow: discarded an unfinished branch ({}); creating a new one",
                redact::text(&format!("{error:#}"))
            ));
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn generated_name() -> String {
    format!("branch-{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_container_reaches_the_server_on_its_own_address() {
        let params = Params::parse(
            "postgres://app:pw@db.internal:54321/shop?hostaddr=192.0.2.7&sslmode=disable", // kingfisher:ignore
        )
        .unwrap();
        let vars = container_env(&params);
        let get = |name: &str| {
            vars.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("PGHOST"), Some("127.0.0.1"));
        assert_eq!(get("PGHOSTADDR"), Some("127.0.0.1"));
        assert_eq!(get("PGPORT"), Some("5432"));
        // Everything else is the workspace's own.
        assert_eq!(get("PGUSER"), Some("app"));
        assert_eq!(get("PGPASSWORD"), Some("pw"));
        assert_eq!(get("PGDATABASE"), Some("shop"));
        assert_eq!(get("PGSSLMODE"), Some("disable"));
        // No address is invented when the URL has none, and no name repeats.
        let vars = container_env(&Params::parse("postgres://u@h/d").unwrap());
        assert!(vars.iter().all(|(name, _)| *name != "PGHOSTADDR"));
        for (name, _) in &vars {
            assert_eq!(vars.iter().filter(|(n, _)| n == name).count(), 1, "{name}");
        }
    }

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
    }
}
