//! Server, branch and snapshot operations (the table in the plan). Every
//! mutation holds the project lock, recovers pending work first, and records
//! its row before each external effect.
use super::admin::{
    Facts, Session, Target, bootstrap_leftovers, db_name, has_code, lock_lost, login_name,
    owner_name, random_password, url,
};
use super::meta::{self, BranchRow, FOREIGN_META_DB, META_DB, NewBranch, ResetIntent};
use super::reconcile;
use super::steps::in_use;
use super::*;
use crate::config::CowCapability;
use anyhow::{Context, bail};
use postgres::Client;
use postgres::error::SqlState;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Takeovers attempted while renaming a reset's databases.
const CUTOVER_ATTEMPTS: u32 = 5;

// ---- server ---------------------------------------------------------------

pub(super) fn status(server: &Server) -> ServerStatus {
    let mut status = ServerStatus {
        mode: server.settings.mode,
        running: server.managed.is_none(),
        reachable: false,
        endpoint: None,
        server_version: None,
        file_copy_method: None,
        cow: CowCapability::Unknown,
        cow_source: None,
        problem: None,
    };
    if let Err(error) = fill_status(server, &mut status) {
        status.problem.get_or_insert(format!("{error:#}"));
    }
    status
}

fn fill_status(server: &Server, status: &mut ServerStatus) -> Result<()> {
    if let Some(managed) = &server.managed {
        let state = managed.container_state()?;
        status.running = state.running;
        if !state.running {
            status.problem = Some(
                if state.exists {
                    "the PostgreSQL container is stopped; run `devflow db up`"
                } else {
                    "the managed PostgreSQL server has not been started; run `devflow db up`"
                }
                .into(),
            );
            return Ok(());
        }
    }
    let target = Target::resolve(server)?;
    status.endpoint = Some(format!("{}:{}", target.host, target.port));
    let mut admin = target.connect("postgres")?;
    status.reachable = true;
    let facts = Facts::read(&mut admin)?;
    status.server_version = Some(facts.version.clone());
    status.file_copy_method = facts.file_copy_method.clone();
    facts.check()?;
    (status.cow, status.cow_source) = recorded_capability(server, &facts);
    let mut problems = Vec::new();
    if !facts.clone_enabled() {
        problems.push("file_copy_method is not 'clone': branches are full copies".to_string());
    }
    problems.extend(bootstrap_leftovers(&mut admin).unwrap_or_default());
    if !problems.is_empty() {
        status.problem = Some(problems.join("; "));
    }
    Ok(())
}

/// The capability known without probing: the cached measurement (managed), the
/// declaration (external), or `copy` when the server does not clone at all.
fn recorded_capability(server: &Server, facts: &Facts) -> (CowCapability, Option<CowSource>) {
    if !facts.clone_enabled() {
        return (CowCapability::Copy, Some(CowSource::Measured));
    }
    match &server.managed {
        Some(managed) => match managed.cached_cow(&facts.postmaster_start) {
            Some(record) => (record.capability, Some(CowSource::Measured)),
            None => (CowCapability::Unknown, None),
        },
        None => match server.settings.cow {
            Some(declared) => (declared, Some(CowSource::Declared)),
            None => (CowCapability::Unknown, None),
        },
    }
}

/// Capability for a new branch; measures lazily if the server restarted since
/// the last probe.
fn capability(s: &mut Session) -> CowCapability {
    let (known, source) = recorded_capability(s.server, &s.facts);
    if let (Some(managed), None, true) = (&s.server.managed, source, s.facts.clone_enabled()) {
        return managed
            .measure_cow(&s.target, &mut s.admin, &s.facts.postmaster_start)
            .capability;
    }
    known
}

/// Capability of a clone from `src` (the server's, or `unknown` when the
/// source lives outside the verified filesystem), enforcing `postgres.require_cow`.
/// `refused` completes the error: what was left undone.
fn clone_capability(s: &mut Session, src: &Source, refused: &str) -> Result<CowCapability> {
    // The verification covers $PGDATA's filesystem only.
    let mut cow = capability(s);
    if cow == CowCapability::Verified
        && let Some(template) = &src.template
        && !s.on_default_tablespace(template)
    {
        cow = CowCapability::Unknown;
    }
    if s.server.settings.require_cow && cow != CowCapability::Verified {
        bail!(
            "postgres.require_cow is set, but copy-on-write is '{}' for this branch; {refused}",
            meta::cow_str(cow)
        );
    }
    Ok(cow)
}

pub(super) fn up(server: &Server) -> Result<ServerStatus> {
    if let Some(managed) = &server.managed {
        managed.start()?;
        wait_ready(server)?;
    }
    {
        let mut s = Session::open(server)?;
        ensure_project(&mut s)?;
        if let Some(managed) = &server.managed
            && s.facts.clone_enabled()
        {
            let cached = managed.cached_cow(&s.facts.postmaster_start);
            if cached.is_none_or(|r| r.capability == CowCapability::Unknown) {
                managed.measure_cow(&s.target, &mut s.admin, &s.facts.postmaster_start);
            }
        }
    }
    Ok(status(server))
}

/// A freshly started container needs a moment before it accepts connections.
fn wait_ready(server: &Server) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let attempt = Target::resolve(server)
            .and_then(|t| t.connect("postgres"))
            .and_then(|mut c| Ok(c.simple_query("SELECT 1").map(|_| ())?));
        match attempt {
            Ok(()) => return Ok(()),
            Err(e) if Instant::now() >= deadline => {
                return Err(e)
                    .context("the PostgreSQL container started but does not accept connections");
            }
            Err(_) => std::thread::sleep(Duration::from_millis(500)),
        }
    }
}

pub(super) fn down(server: &Server) -> Result<()> {
    match &server.managed {
        Some(managed) => managed.stop(),
        None => Ok(()),
    }
}

// ---- session setup --------------------------------------------------------

/// Run `body` on `s`. The lock session also carries every metadata read and
/// write, so a failure of any of them with the lock gone is reported as
/// [`LockLost`](super::admin::LockLost), whichever statement it came from.
fn guarded<T>(s: &mut Session, body: impl FnOnce(&mut Session) -> Result<T>) -> Result<T> {
    body(s).map_err(|error| s.lock_failure(error))
}

/// Open a session for a mutation: project row, owner role, pending recovery.
fn begin(server: &Server) -> Result<Session<'_>> {
    let mut s = Session::open(server)?;
    guarded(&mut s, |s| {
        ensure_project(s)?;
        let report = reconcile::run(s)?;
        s.warnings.extend(
            report
                .actions
                .into_iter()
                .map(|a| format!("recovered: {a}")),
        );
        s.warnings.extend(
            report
                .problems
                .into_iter()
                .map(|p| format!("needs attention: {p}")),
        );
        Ok(())
    })?;
    Ok(s)
}

/// The project row is written before the owner role exists, so a crash in
/// between is repaired by the next call; an existing role without a row is
/// never adopted.
pub(super) fn ensure_project(s: &mut Session) -> Result<()> {
    let project = s.project_id();
    let owner = owner_name(project);
    s.step()?;
    let known = s
        .admin
        .query_opt("SELECT 1 FROM projects WHERE id = $1", &[&project])?
        .is_some();
    if known {
        s.admin.execute(
            "UPDATE projects SET name = $2 WHERE id = $1",
            &[&project, &s.server.project_name],
        )?;
    } else {
        if s.role_exists(&owner)? {
            bail!("role {owner} already exists on this server but belongs to no devflow project");
        }
        s.admin
            .execute(
                "INSERT INTO projects (id, name, owner_role) VALUES ($1, $2, $3)",
                &[&project, &s.server.project_name, &owner],
            )
            .context(
                "cannot register the project (another project on this server has the same id prefix?)",
            )?;
    }
    s.ensure_owner_role(&owner)
}

// ---- sources --------------------------------------------------------------

struct Source {
    /// Database cloned from; `None` for an empty database.
    template: Option<String>,
    /// Login whose stragglers are reassigned to the owner (branch sources).
    login: Option<String>,
    kind: &'static str,
    id: Option<Uuid>,
}

fn resolve_source(s: &mut Session, source: ResolvedSource) -> Result<Source> {
    let project = s.project_id();
    Ok(match source {
        ResolvedSource::Empty => Source {
            template: None,
            login: None,
            kind: "empty",
            id: None,
        },
        ResolvedSource::Branch(id) => {
            let row = ready_branch(s, id)?;
            Source {
                template: Some(row.db_name),
                login: Some(row.role_name),
                kind: "branch",
                id: Some(id),
            }
        }
        ResolvedSource::Snapshot(id) => {
            let snapshot = meta::get_snapshot(&mut s.admin, project, id)?
                .with_context(|| format!("snapshot {id} does not exist"))?;
            anyhow::ensure!(
                snapshot.status == RecordStatus::Ready,
                "snapshot {}@v{} is not ready",
                snapshot.name,
                snapshot.version
            );
            Source {
                template: Some(snapshot.db_name),
                login: None,
                kind: "snapshot",
                id: Some(id),
            }
        }
    })
}

/// A reset re-clones the branch's recorded source, which may have been dropped
/// since: say so and point at the way out instead of a bare "does not exist".
fn resolve_reset_source(s: &mut Session, source: ResolvedSource) -> Result<Source> {
    let project = s.project_id();
    match source {
        ResolvedSource::Branch(id) if meta::get_branch(&mut s.admin, project, id)?.is_none() => {
            bail!("the source branch {id} no longer exists; choose another source with --to")
        }
        ResolvedSource::Snapshot(id)
            if meta::get_snapshot(&mut s.admin, project, id)?.is_none() =>
        {
            bail!("the source snapshot {id} no longer exists; choose another source with --to")
        }
        _ => resolve_source(s, source),
    }
}

fn ready_branch(s: &mut Session, id: Uuid) -> Result<BranchRow> {
    let project = s.project_id();
    let row = meta::get_branch(&mut s.admin, project, id)?
        .with_context(|| format!("branch {id} does not exist"))?;
    anyhow::ensure!(
        row.status == RecordStatus::Ready && row.op.is_none(),
        "branch {} is not ready (status {:?}{})",
        row.name,
        row.status,
        row.error
            .as_deref()
            .map(|e| format!(": {e}"))
            .unwrap_or_default()
    );
    Ok(row)
}

/// A failed step: stop at once if the lock is gone; otherwise record the
/// failure and run the same recovery `reconcile` would.
fn abandon(s: &mut Session, id: Uuid, error: anyhow::Error) -> anyhow::Error {
    let error = s.lock_failure(error);
    if lock_lost(&error) {
        return error;
    }
    let _ = meta::set_branch_status(
        &mut s.admin,
        id,
        RecordStatus::Creating,
        RecordStatus::Failed,
        Some(&format!("{error:#}")),
    );
    match reconcile::recover_branch(s, id) {
        Ok(_) => error,
        Err(recovery) => anyhow::anyhow!(
            "{error:#} (cleanup also failed: {recovery:#}; run `devflow db reconcile`)"
        ),
    }
}

// ---- create ---------------------------------------------------------------

pub(super) fn create_branch(
    server: &Server,
    name: &str,
    source: ResolvedSource,
    instance_id: Option<Uuid>,
) -> Result<BranchCredentials> {
    anyhow::ensure!(!name.trim().is_empty(), "branch name must not be empty");
    let mut s = begin(server)?;
    guarded(&mut s, |s| {
        let project = s.project_id();
        let src = resolve_source(s, source)?;

        let cow = clone_capability(s, &src, "no branch was created")?;

        let id = Uuid::new_v4();
        let db = db_name(project, id);
        let login = login_name(id);
        let owner = owner_name(project);
        let password = random_password();
        s.step()?;
        meta::insert_branch(
            &mut s.admin,
            project,
            &NewBranch {
                id,
                name,
                db_name: &db,
                role_name: &login,
                source_kind: src.kind,
                source_id: src.id,
                instance_id,
                cow,
            },
        )?;
        if let Err(error) = build_branch(s, id, &src, &db, &login, &owner, &password) {
            return Err(abandon(s, id, error));
        }
        let row =
            meta::get_branch(&mut s.admin, project, id)?.context("the new branch row vanished")?;
        let (host, port) = s.target.app_endpoint(&server.settings);
        Ok(BranchCredentials {
            branch: row.info(None),
            url: url(&host, port, &login, &password, &db),
            host,
            port,
            user: login,
            password,
            database: db,
            warnings: std::mem::take(&mut s.warnings),
        })
    })
}

/// login → database (closed) → ACLs → open → normalize → `CONNECT` last → ready.
fn build_branch(
    s: &mut Session,
    id: Uuid,
    src: &Source,
    db: &str,
    login: &str,
    owner: &str,
    password: &str,
) -> Result<()> {
    s.create_login(login, owner, password)?;
    create_database(s, src, db)?;
    s.restrict_database(db, owner)?;
    s.set_connections(db, true)?;
    s.normalize(db, owner, src.login.as_deref())?;
    s.grant_connect(db, login)?;
    s.step()?;
    meta::set_branch_status(
        &mut s.admin,
        id,
        RecordStatus::Creating,
        RecordStatus::Ready,
        None,
    )
}

fn create_database(s: &mut Session, src: &Source, db: &str) -> Result<()> {
    match &src.template {
        None => s.create_empty_database(db),
        Some(template) => s.clone_database(template, db),
    }
}

// ---- reset ----------------------------------------------------------------

pub(super) fn reset_branch(
    server: &Server,
    id: Uuid,
    source: ResolvedSource,
) -> Result<BranchInfo> {
    let mut s = begin(server)?;
    guarded(&mut s, |s| {
        let project = s.project_id();
        let row = ready_branch(s, id)?;
        anyhow::ensure!(
            source != ResolvedSource::Branch(id),
            "a branch cannot be reset to itself"
        );
        let src = resolve_reset_source(s, source)?;
        // Same capability and `require_cow` rules as creation, before anything is staged.
        let cow = clone_capability(s, &src, "the branch was not reset")?;
        let owner = owner_name(project);
        let replacement = db_name(project, Uuid::new_v4());
        let backup = db_name(project, Uuid::new_v4());
        s.step()?;
        meta::begin_reset(
            &mut s.admin,
            id,
            row.generation,
            &ResetIntent {
                replacement_db: &replacement,
                backup_db: &backup,
                source_kind: src.kind,
                source_id: src.id,
            },
        )?;
        if let Err(error) = run_reset(s, &row, &src, cow, &owner, &replacement, &backup) {
            let error = s.lock_failure(error);
            if lock_lost(&error) {
                return Err(error);
            }
            return Err(match reconcile::recover_branch(s, id) {
                Ok(_) => {
                    let _ = meta::set_branch_error(
                        &mut s.admin,
                        id,
                        Some(&format!("reset failed: {error:#}")),
                    );
                    error
                }
                Err(recovery) => anyhow::anyhow!(
                    "{error:#} (cleanup also failed: {recovery:#}; run `devflow db reconcile`)"
                ),
            });
        }
        let row =
            meta::get_branch(&mut s.admin, project, id)?.context("the branch row vanished")?;
        let mut info = row.info(None);
        info.warnings = std::mem::take(&mut s.warnings);
        Ok(info)
    })
}

/// staging (replacement cloned and normalized, closed) → cutover (close both,
/// terminate sessions, rename in one metadata transaction that also records the
/// new data's `cow`) → cleanup.
fn run_reset(
    s: &mut Session,
    row: &BranchRow,
    src: &Source,
    cow: CowCapability,
    owner: &str,
    replacement: &str,
    backup: &str,
) -> Result<()> {
    create_database(s, src, replacement)?;
    s.restrict_database(replacement, owner)?;
    s.set_connections(replacement, true)?;
    s.normalize(replacement, owner, src.login.as_deref())?;
    s.set_connections(replacement, false)?;

    s.step()?;
    meta::set_op_phase(&mut s.admin, row.id, row.generation, "staging", "cutover")?;
    let mut attempt = 0;
    let generation = loop {
        attempt += 1;
        s.set_connections(&row.db_name, false)?;
        let (_, stuck) = s.terminate_sessions(&row.db_name)?;
        match s.swap_databases(
            row.id,
            row.generation,
            &row.db_name,
            replacement,
            backup,
            cow,
        ) {
            Ok(generation) => break generation,
            Err(e) if in_use(&e) && attempt < CUTOVER_ATTEMPTS => {
                std::thread::sleep(Duration::from_millis(100 << (attempt - 1)));
            }
            Err(e) if in_use(&e) => {
                return Err(e).context(format!(
                    "cannot take over {} (sessions remain: {})",
                    row.db_name,
                    stuck.join(", ")
                ));
            }
            Err(e) => return Err(e),
        }
    };
    reconcile::finish_reset(s, row.id, generation, &row.db_name, &row.role_name, backup)
}

// ---- drop / rotate --------------------------------------------------------

pub(super) fn drop_branch(server: &Server, id: Uuid) -> Result<()> {
    let mut s = begin(server)?;
    guarded(&mut s, |s| {
        let project = s.project_id();
        let Some(row) = meta::get_branch(&mut s.admin, project, id)? else {
            return Ok(());
        };
        anyhow::ensure!(
            row.op.is_none(),
            "branch {} has an unrecovered reset ({}); resolve it before dropping",
            row.name,
            row.error.as_deref().unwrap_or("see `devflow db reconcile`")
        );
        s.step()?;
        meta::set_branch_status(
            &mut s.admin,
            id,
            row.status,
            RecordStatus::Deleting,
            row.error.as_deref(),
        )?;
        if let Err(error) = reconcile::finish_drop(s, &row) {
            let error = s.lock_failure(error);
            if !lock_lost(&error) {
                let _ = meta::set_branch_error(&mut s.admin, id, Some(&format!("{error:#}")));
            }
            return Err(error);
        }
        Ok(())
    })
}

pub(super) fn rotate_credentials(server: &Server, id: Uuid) -> Result<BranchCredentials> {
    let mut s = begin(server)?;
    guarded(&mut s, |s| {
        let project = s.project_id();
        let row = meta::get_branch(&mut s.admin, project, id)?.with_context(|| {
        format!(
            "branch {id} does not exist (an interrupted creation is rolled back during recovery)"
        )
    })?;
        anyhow::ensure!(
            row.status == RecordStatus::Ready && row.op.is_none(),
            "branch {} is not ready (status {:?})",
            row.name,
            row.status
        );
        let password = random_password();
        s.set_password(&row.role_name, &password)?;
        let (host, port) = s.target.app_endpoint(&server.settings);
        Ok(BranchCredentials {
            url: url(&host, port, &row.role_name, &password, &row.db_name),
            branch: row.info(None),
            host,
            port,
            user: row.role_name.clone(),
            password,
            database: row.db_name.clone(),
            warnings: std::mem::take(&mut s.warnings),
        })
    })
}

// ---- reads ----------------------------------------------------------------

/// A metadata connection for read-only calls. `None` when the server was never
/// bootstrapped, so reads never create anything.
fn reader(server: &Server) -> Result<Option<Client>> {
    let target = Target::resolve(server)?;
    let mut client = match target.connect(META_DB) {
        Ok(client) => client,
        Err(e) if has_code(&e, &SqlState::INVALID_CATALOG_NAME) => return Ok(None),
        Err(e) => return Err(e),
    };
    anyhow::ensure!(meta::is_marked(&mut client)?, "{FOREIGN_META_DB}");
    Ok(meta::schema_present(&mut client)?.then_some(client))
}

fn sizes(client: &mut Client, names: &[&str]) -> HashMap<String, i64> {
    client
        .query(
            "SELECT datname::text, pg_database_size(oid) FROM pg_database WHERE datname::text = ANY($1)",
            &[&names],
        )
        .map(|rows| rows.iter().map(|r| (r.get(0), r.get(1))).collect())
        .unwrap_or_default()
}

fn infos(client: &mut Client, rows: Vec<BranchRow>) -> Vec<BranchInfo> {
    let names: Vec<&str> = rows.iter().map(|r| r.db_name.as_str()).collect();
    let sizes = sizes(client, &names);
    rows.iter()
        .map(|r| r.info(sizes.get(&r.db_name).copied()))
        .collect()
}

pub(super) fn branch(server: &Server, id: Uuid) -> Result<Option<BranchInfo>> {
    let Some(mut client) = reader(server)? else {
        return Ok(None);
    };
    let row = meta::get_branch(&mut client, server.project_id, id)?;
    Ok(row.map(|r| infos(&mut client, vec![r]).remove(0)))
}

pub(super) fn branch_for_instance(
    server: &Server,
    instance_id: Uuid,
) -> Result<Option<BranchInfo>> {
    let Some(mut client) = reader(server)? else {
        return Ok(None);
    };
    let row = meta::latest_for_instance(&mut client, server.project_id, instance_id)?;
    Ok(row.map(|r| infos(&mut client, vec![r]).remove(0)))
}

pub(super) fn list_branches(server: &Server) -> Result<Vec<BranchInfo>> {
    let Some(mut client) = reader(server)? else {
        return Ok(Vec::new());
    };
    let rows = meta::list_branches(&mut client, server.project_id)?;
    Ok(infos(&mut client, rows))
}

// ---- snapshots ------------------------------------------------------------

/// Names appear in `snapshot:NAME@vN` on command lines, so `@` and whitespace
/// are reserved.
pub(crate) fn validate_snapshot_name(name: &str) -> Result<()> {
    anyhow::ensure!(
        !name.is_empty() && name.len() <= 100,
        "snapshot names must be 1-100 characters"
    );
    anyhow::ensure!(
        !name.contains('@') && !name.chars().any(|c| c.is_whitespace() || c.is_control()),
        "snapshot names cannot contain '@', spaces or control characters"
    );
    Ok(())
}

pub(super) fn create_snapshot(
    server: &Server,
    name: &str,
    from_branch: Uuid,
) -> Result<SnapshotInfo> {
    validate_snapshot_name(name)?;
    let mut s = begin(server)?;
    guarded(&mut s, |s| {
        let project = s.project_id();
        let src = resolve_source(s, ResolvedSource::Branch(from_branch))?;
        let id = Uuid::new_v4();
        let db = db_name(project, id);
        let owner = owner_name(project);
        s.step()?;
        meta::insert_snapshot(&mut s.admin, project, id, name, &db, from_branch)?;
        if let Err(error) = seal_snapshot(s, id, &src, &db, &owner) {
            let error = s.lock_failure(error);
            if lock_lost(&error) {
                return Err(error);
            }
            let _ = meta::set_snapshot_status(
                &mut s.admin,
                id,
                RecordStatus::Creating,
                RecordStatus::Failed,
            );
            return Err(match reconcile::recover_snapshot(s, id) {
                Ok(_) => error,
                Err(recovery) => anyhow::anyhow!(
                    "{error:#} (cleanup also failed: {recovery:#}; run `devflow db reconcile`)"
                ),
            });
        }
        let mut info =
            meta::get_snapshot(&mut s.admin, project, id)?.context("the snapshot row vanished")?;
        info.warnings = std::mem::take(&mut s.warnings);
        Ok(info)
    })
}

/// Clone, normalize, then close for good: snapshots are never templates and
/// never accept connections, so only the admin can clone them.
fn seal_snapshot(s: &mut Session, id: Uuid, src: &Source, db: &str, owner: &str) -> Result<()> {
    create_database(s, src, db)?;
    s.restrict_database(db, owner)?;
    s.set_connections(db, true)?;
    s.normalize(db, owner, src.login.as_deref())?;
    s.set_connections(db, false)?;
    s.step()?;
    meta::set_snapshot_status(
        &mut s.admin,
        id,
        RecordStatus::Creating,
        RecordStatus::Ready,
    )
}

pub(super) fn list_snapshots(server: &Server) -> Result<Vec<SnapshotInfo>> {
    match reader(server)? {
        Some(mut client) => meta::list_snapshots(&mut client, server.project_id),
        None => Ok(Vec::new()),
    }
}

pub(super) fn find_snapshot(
    server: &Server,
    name: &str,
    version: Option<i32>,
) -> Result<SnapshotInfo> {
    let found = match reader(server)? {
        Some(mut client) => {
            meta::find_ready_snapshot(&mut client, server.project_id, name, version)?
        }
        None => None,
    };
    found.ok_or_else(|| match version {
        Some(v) => anyhow::anyhow!("snapshot '{name}@v{v}' does not exist or is not ready"),
        None => anyhow::anyhow!("snapshot '{name}' does not exist or has no ready version"),
    })
}

pub(super) fn delete_snapshot(server: &Server, name: &str, version: i32) -> Result<()> {
    let mut s = begin(server)?;
    guarded(&mut s, |s| {
        let project = s.project_id();
        let snapshot = meta::get_snapshot_version(&mut s.admin, project, name, version)?
            .with_context(|| format!("snapshot '{name}@v{version}' does not exist"))?;
        s.step()?;
        meta::set_snapshot_status(
            &mut s.admin,
            snapshot.id,
            snapshot.status,
            RecordStatus::Deleting,
        )?;
        reconcile::finish_drop_snapshot(s, &snapshot)
    })
}

// ---- reconcile ------------------------------------------------------------

pub(super) fn reconcile(server: &Server) -> Result<ReconcileReport> {
    let mut s = Session::open(server)?;
    guarded(&mut s, |s| {
        ensure_project(s)?;
        reconcile::run(s)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_names_are_validated() {
        assert!(validate_snapshot_name("seed").is_ok());
        assert!(validate_snapshot_name("prod-copy_2026.10").is_ok());
        for bad in ["", "   ", "a@b", "prod copy", "tab\there", &"x".repeat(101)] {
            assert!(validate_snapshot_name(bad).is_err(), "{bad:?}");
        }
    }
}
