//! Server, branch and snapshot operations (the table in the plan). Every
//! mutation holds the project lock, recovers pending work first, and records
//! its row before each external effect.
use super::admin::{
    Facts, Session, Target, bootstrap_leftovers, db_name, has_code, lock_lost, login_name,
    owner_name, random_password, url,
};
use super::meta::{self, BranchRow, FOREIGN_META_DB, META_DB, NewBranch, ResetIntent};
use super::reconcile;
use super::scratch;
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
        container_endpoint: None,
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
                    "the PostgreSQL container is stopped"
                } else {
                    "the managed PostgreSQL server has not been started"
                }
                .into(),
            );
            return Ok(());
        }
    }
    status.container_endpoint = server
        .managed
        .as_ref()
        .and_then(|managed| managed.container_endpoint());
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

/// How long a freshly started container gets to accept connections.
const READY_WITHIN: Duration = Duration::from_secs(60);
/// How long one probe step (connect, write, read) may take.
const PROBE_STEP: Duration = Duration::from_secs(3);

/// How long one readiness attempt (probe, connect, authentication and
/// `SELECT 1` all together) may take.
const ATTEMPT_WITHIN: Duration = Duration::from_secs(10);
/// How long the wait rests between two readiness attempts.
const READY_PAUSE: Duration = Duration::from_millis(500);

/// A freshly started container needs a moment before it accepts connections.
/// Each attempt first checks that the endpoint speaks the PostgreSQL protocol,
/// so a foreign process that accepts TCP and never answers cannot stall the
/// wait: it fails the attempt, and the wait ends at the deadline. The whole
/// attempt is bounded, not only its first steps: a listener that answers the
/// probe and then stalls the startup or the authentication fails the attempt
/// too. No attempt is given more time than the deadline leaves (and none
/// starts once it has passed), so the wait never outlasts [`READY_WITHIN`].
fn wait_ready(server: &Server) -> Result<()> {
    wait_ready_within(server, READY_WITHIN)
}

/// [`wait_ready`] with its own deadline. Finding the endpoint (a `docker
/// compose port` for a managed server) is part of the wait, not a step before
/// it: it gets what is left of `total`, and is killed and reaped when that runs out.
fn wait_ready_within(server: &Server, total: Duration) -> Result<()> {
    wait_until(&SystemClock, total, READY_PAUSE, &mut |remaining| {
        let started = Instant::now();
        // The lookup runs under what is left of the deadline: a Docker that
        // stalls in it is killed and reaped when the deadline comes, not waited for.
        let target = Target::resolve_within(server, remaining)?;
        // Looking the endpoint up took its share of what was left.
        let budget = ATTEMPT_WITHIN.min(remaining.saturating_sub(started.elapsed()));
        anyhow::ensure!(
            !budget.is_zero(),
            "the readiness deadline passed while looking for the server's address"
        );
        attempt_ready(target, budget)
    })
    .context("the PostgreSQL container started but does not accept connections")
}

/// One readiness attempt, over within `budget` whatever the other end does.
fn attempt_ready(target: Target, budget: Duration) -> Result<()> {
    // A step never outlasts the attempt, so the thread `within` abandons
    // ends with the attempt too.
    let step = PROBE_STEP.min(budget);
    within(budget, move || {
        target.probe(step)?;
        let mut client = target.connect("postgres")?;
        client.simple_query("SELECT 1")?;
        Ok(())
    })
}

/// Run `work` and give up on it after `budget`. The blocking client has no
/// read timeout once the TCP connection is up (startup, authentication and
/// queries wait for the peer for as long as it takes), so the work runs on
/// its own thread and the caller stops waiting. A thread stuck on a peer that
/// never answers ends when that peer closes the connection, and holds nothing
/// but its own socket until then.
fn within<T: Send + 'static>(
    budget: Duration,
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let (done, result) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(work());
    });
    match result.recv_timeout(budget) {
        Ok(outcome) => outcome,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            bail!(
                "the server did not answer within {} (it accepted the connection but never \
                 finished starting a session)",
                humantime::format_duration(Duration::from_millis(
                    budget.as_millis().try_into().unwrap_or(u64::MAX)
                ))
            )
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            bail!("the connection attempt ended without a result")
        }
    }
}

/// Time as [`wait_until`] sees it, so a test can stand in for the real one.
trait Clock {
    fn now(&self) -> Instant;
    fn sleep(&self, pause: Duration);
}

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, pause: Duration) {
        std::thread::sleep(pause);
    }
}

/// Run `attempt` until it succeeds or `total` has passed; the last error is
/// returned.
///
/// The bound is `total`, not `total` plus an attempt: `attempt` is told how
/// much time is left and must finish within it, no attempt starts when none
/// is, and the pause between attempts is cut to what is left.
fn wait_until(
    clock: &dyn Clock,
    total: Duration,
    pause: Duration,
    attempt: &mut dyn FnMut(Duration) -> Result<()>,
) -> Result<()> {
    let deadline = clock.now() + total;
    let mut last = None;
    loop {
        let remaining = deadline.saturating_duration_since(clock.now());
        if remaining.is_zero() {
            return Err(last.unwrap_or_else(|| anyhow::anyhow!("no time was left to try")));
        }
        match attempt(remaining) {
            Ok(()) => return Ok(()),
            Err(error) => last = Some(error),
        }
        let remaining = deadline.saturating_duration_since(clock.now());
        if !remaining.is_zero() {
            clock.sleep(pause.min(remaining));
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
        Err(recovery) => s.needs_reconcile(format!(
            "{error:#} (cleanup also failed: {recovery:#}; finish the interrupted work)"
        )),
    }
}

// ---- create ---------------------------------------------------------------

pub(super) fn create_branch(
    server: &Server,
    id: Uuid,
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
        let mut branch = row.info(None);
        let extra_urls = attach_extras(s, &mut branch, &host, port, &login, &password)?;
        Ok(BranchCredentials {
            branch,
            url: url(&host, port, &login, &password, &db),
            host,
            port,
            user: login,
            password,
            database: db,
            extra_urls,
            warnings: std::mem::take(&mut s.warnings),
        })
    })
}

/// login → database (closed) → ACLs → open → normalize → `CONNECT` last →
/// the scratch databases, built the same way → ready. A branch is `ready`
/// only with all of its configured scratch databases, so a creation that dies
/// anywhere is rolled back whole.
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
    s.grant_connect(db, login, owner)?;
    scratch::provision(s, id, login, 0)?;
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

// ---- scratch databases ------------------------------------------------------

/// The scratch databases recorded for `branch`, orphans marked against the
/// configuration.
fn extras_of(client: &mut Client, server: &Server, branch: Uuid) -> Result<Vec<ScratchInfo>> {
    let configured = &server.settings.extra_databases;
    Ok(meta::scratch_of_branch(client, server.project_id, branch)?
        .iter()
        .map(|row| row.info(configured))
        .collect())
}

/// Put the branch's scratch databases on `info` and return the URL of each
/// configured, `ready` one for the branch's login: the same credentials,
/// another database.
fn attach_extras(
    s: &mut Session,
    info: &mut BranchInfo,
    host: &str,
    port: u16,
    login: &str,
    password: &str,
) -> Result<BTreeMap<String, String>> {
    info.extras = extras_of(&mut s.admin, s.server, info.id)?;
    Ok(info
        .extras
        .iter()
        .filter(|extra| !extra.orphaned && extra.status == RecordStatus::Ready)
        .map(|extra| {
            (
                extra.name.clone(),
                url(host, port, login, password, &extra.db_name),
            )
        })
        .collect())
}

pub(super) fn ensure_scratch(server: &Server, id: Uuid) -> Result<Vec<ScratchInfo>> {
    let mut s = begin(server)?;
    guarded(&mut s, |s| {
        let row = ready_branch(s, id)?;
        scratch::provision(s, id, &row.role_name, row.generation)?;
        extras_of(&mut s.admin, server, id)
    })
}

pub(super) fn drop_orphans(server: &Server) -> Result<Vec<String>> {
    let mut s = begin(server)?;
    guarded(&mut s, |s| {
        // The database lock is held: the configuration is read again now, not
        // taken from the project the server was built from (a window keeps
        // that for as long as it is open). A name restored to
        // `extra_databases` since is no orphan, and a configuration that
        // cannot be read proves nothing.
        let listed = server.listed_extra_databases()?;
        scratch::drop_orphans(s, &listed)
    })
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
        // Journaled with the intent: recovery empties exactly these.
        let scratch_ids = scratch::selected_for_reset(s, id)?;
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
                scratch: &scratch_ids,
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
                Err(recovery) => s.needs_reconcile(format!(
                    "{error:#} (cleanup also failed: {recovery:#}; finish the interrupted work)"
                )),
            });
        }
        let row =
            meta::get_branch(&mut s.admin, project, id)?.context("the branch row vanished")?;
        let mut info = row.info(None);
        info.extras = extras_of(&mut s.admin, server, id)?;
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
        if row.op.is_some() {
            return Err(s.needs_reconcile(format!(
                "branch {} has an unrecovered reset ({}); resolve it before dropping",
                row.name,
                row.error
                    .as_deref()
                    .unwrap_or("finish the interrupted work")
            )));
        }
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
        // One login for the branch's database and its scratch databases: the
        // new password covers all of them.
        let password = random_password();
        s.set_password(&row.role_name, &password)?;
        let (host, port) = s.target.app_endpoint(&server.settings);
        let mut branch = row.info(None);
        let extra_urls = attach_extras(s, &mut branch, &host, port, &row.role_name, &password)?;
        Ok(BranchCredentials {
            url: url(&host, port, &row.role_name, &password, &row.db_name),
            branch,
            host,
            port,
            user: row.role_name.clone(),
            password,
            database: row.db_name.clone(),
            extra_urls,
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

/// The branches as callers see them: sizes (best effort) and scratch
/// databases (recorded, orphans marked against the configuration).
fn infos(server: &Server, client: &mut Client, rows: Vec<BranchRow>) -> Result<Vec<BranchInfo>> {
    let names: Vec<&str> = rows.iter().map(|r| r.db_name.as_str()).collect();
    let sizes = sizes(client, &names);
    let configured = &server.settings.extra_databases;
    let mut extras: HashMap<Uuid, Vec<ScratchInfo>> = HashMap::new();
    for row in meta::list_scratch(client, server.project_id)? {
        extras
            .entry(row.branch_id)
            .or_default()
            .push(row.info(configured));
    }
    Ok(rows
        .iter()
        .map(|r| {
            let mut info = r.info(sizes.get(&r.db_name).copied());
            info.extras = extras.remove(&r.id).unwrap_or_default();
            info
        })
        .collect())
}

pub(super) fn branch(server: &Server, id: Uuid) -> Result<Option<BranchInfo>> {
    let Some(mut client) = reader(server)? else {
        return Ok(None);
    };
    let Some(row) = meta::get_branch(&mut client, server.project_id, id)? else {
        return Ok(None);
    };
    Ok(infos(server, &mut client, vec![row])?.pop())
}

pub(super) fn branch_for_instance(
    server: &Server,
    instance_id: Uuid,
) -> Result<Option<BranchInfo>> {
    let Some(mut client) = reader(server)? else {
        return Ok(None);
    };
    let Some(row) = meta::latest_for_instance(&mut client, server.project_id, instance_id)? else {
        return Ok(None);
    };
    Ok(infos(server, &mut client, vec![row])?.pop())
}

pub(super) fn list_branches(server: &Server) -> Result<Vec<BranchInfo>> {
    let Some(mut client) = reader(server)? else {
        return Ok(Vec::new());
    };
    let rows = meta::list_branches(&mut client, server.project_id)?;
    infos(server, &mut client, rows)
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
                Err(recovery) => s.needs_reconcile(format!(
                    "{error:#} (cleanup also failed: {recovery:#}; finish the interrupted work)"
                )),
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
    fn the_readiness_wait_is_bounded_even_against_a_listener_that_never_answers() {
        use std::io::Read;
        // Accepts every connection and holds it open without a word.
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut sink = [0u8; 64];
                    while stream.read(&mut sink).is_ok_and(|n| n > 0) {}
                });
            }
        });
        let started = Instant::now();
        let mut attempts = 0;
        let error = wait_until(
            &SystemClock,
            Duration::from_millis(700),
            Duration::from_millis(50),
            &mut |remaining| {
                attempts += 1;
                super::super::admin::speaks_postgres(
                    "127.0.0.1",
                    port,
                    Duration::from_millis(200).min(remaining),
                )
            },
        )
        .unwrap_err();
        assert!(attempts >= 2, "it kept trying: {attempts}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(
            format!("{error:#}").contains("does not answer"),
            "{error:#}"
        );
    }

    #[test]
    fn a_listener_that_answers_the_probe_and_then_stalls_cannot_hold_the_wait() {
        use std::io::{Read, Write};
        // Answers the SSL request with `N` like a server would, reads the
        // startup message, and never says another word.
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut request = [0u8; 8];
                    let _ = stream.read_exact(&mut request);
                    let _ = stream.write_all(b"N");
                    let mut sink = [0u8; 256];
                    while stream.read(&mut sink).is_ok_and(|n| n > 0) {}
                });
            }
        });
        let started = Instant::now();
        let mut attempts = 0;
        let error = wait_until(
            &SystemClock,
            Duration::from_millis(800),
            Duration::from_millis(50),
            &mut |remaining| {
                attempts += 1;
                attempt_ready(
                    Target::for_endpoint("127.0.0.1", port, "postgres"),
                    Duration::from_millis(300).min(remaining),
                )
            },
        )
        .unwrap_err();
        assert!(attempts >= 2, "it kept trying: {attempts}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "bounded by the deadline plus one attempt: {:?}",
            started.elapsed()
        );
        assert!(
            format!("{error:#}").contains("did not answer within"),
            "{error:#}"
        );
    }

    #[test]
    fn work_that_finishes_or_fails_in_time_reports_its_own_result() {
        assert_eq!(within(Duration::from_secs(5), || Ok(7)).unwrap(), 7);
        let error = within(Duration::from_secs(5), || -> Result<()> { bail!("boom") })
            .unwrap_err()
            .to_string();
        assert_eq!(error, "boom");
        let error = within(Duration::from_secs(5), || -> Result<()> {
            panic!("worker panicked")
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("ended without a result"), "{error}");
    }

    #[test]
    fn the_readiness_wait_returns_as_soon_as_an_attempt_succeeds() {
        let mut attempts = 0;
        wait_until(
            &SystemClock,
            Duration::from_secs(30),
            Duration::from_millis(1),
            &mut |_| {
                attempts += 1;
                if attempts < 3 {
                    anyhow::bail!("not yet")
                }
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(attempts, 3);
    }

    /// Time that only moves when the test says so (or the wait sleeps).
    struct FakeClock {
        start: Instant,
        elapsed: std::cell::Cell<Duration>,
        sleeps: std::cell::RefCell<Vec<Duration>>,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                start: Instant::now(),
                elapsed: std::cell::Cell::new(Duration::ZERO),
                sleeps: std::cell::RefCell::new(Vec::new()),
            }
        }

        fn advance(&self, by: Duration) {
            self.elapsed.set(self.elapsed.get() + by);
        }

        fn elapsed(&self) -> Duration {
            self.elapsed.get()
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            self.start + self.elapsed.get()
        }

        fn sleep(&self, pause: Duration) {
            self.sleeps.borrow_mut().push(pause);
            self.advance(pause);
        }
    }

    #[test]
    fn the_readiness_budget_is_never_exceeded_and_attempts_get_only_what_is_left() {
        let clock = FakeClock::new();
        let total = Duration::from_secs(60);
        let mut budgets = Vec::new();
        // Every attempt takes all the time it is given, like `within` timing
        // out on a stalled listener.
        let error = wait_until(
            &clock,
            total,
            Duration::from_millis(300),
            &mut |remaining| {
                let budget = ATTEMPT_WITHIN.min(remaining);
                budgets.push(budget);
                clock.advance(budget);
                anyhow::bail!("not ready")
            },
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "not ready");
        assert!(
            clock.elapsed() <= total,
            "overran the budget: {:?}",
            clock.elapsed()
        );
        assert_eq!(clock.elapsed(), total, "and used all of it");
        assert!(
            budgets.iter().all(|budget| !budget.is_zero()),
            "{budgets:?}"
        );
        assert_eq!(budgets[0], ATTEMPT_WITHIN);
        // Attempts start 10.3s apart, so the sixth starts at 51.5s: it is
        // given the 8.5s that are left, not a full 10s.
        assert!(*budgets.last().unwrap() < ATTEMPT_WITHIN, "{budgets:?}");
    }

    #[test]
    fn an_attempt_starting_just_before_the_deadline_is_cut_short() {
        let clock = FakeClock::new();
        let mut seen = Vec::new();
        let result = wait_until(
            &clock,
            Duration::from_secs(60),
            Duration::from_millis(300),
            &mut |remaining| {
                seen.push(remaining);
                if seen.len() == 1 {
                    // The first attempt fails at 59.5s.
                    clock.advance(Duration::from_millis(59_500));
                    anyhow::bail!("not ready")
                }
                // The next one starts at 59.8s and takes all it is given.
                clock.advance(remaining);
                anyhow::bail!("still not ready")
            },
        );
        assert_eq!(result.unwrap_err().to_string(), "still not ready");
        assert_eq!(
            seen,
            vec![Duration::from_secs(60), Duration::from_millis(200)]
        );
        assert_eq!(*clock.sleeps.borrow(), vec![Duration::from_millis(300)]);
        assert_eq!(clock.elapsed(), Duration::from_secs(60));
    }

    #[test]
    fn no_attempt_starts_at_the_deadline_and_the_last_pause_is_cut() {
        let clock = FakeClock::new();
        let mut attempts = 0;
        let error = wait_until(
            &clock,
            Duration::from_millis(1_000),
            Duration::from_millis(450),
            &mut |remaining| {
                attempts += 1;
                assert!(remaining > Duration::ZERO);
                // Quick failures: 100ms each.
                clock.advance(Duration::from_millis(100));
                anyhow::bail!("refused")
            },
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "refused");
        // Attempt 0-100, pause to 550, attempt to 650, then the pause is cut
        // to the 350ms left and the wait is over: no third attempt.
        assert_eq!(attempts, 2);
        assert_eq!(
            *clock.sleeps.borrow(),
            vec![Duration::from_millis(450), Duration::from_millis(350)]
        );
        assert_eq!(clock.elapsed(), Duration::from_millis(1_000));
    }

    #[test]
    fn a_wait_without_time_makes_no_attempt() {
        let clock = FakeClock::new();
        let mut attempts = 0;
        let error = wait_until(
            &clock,
            Duration::ZERO,
            Duration::from_millis(10),
            &mut |_| {
                attempts += 1;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(attempts, 0);
        assert!(error.to_string().contains("no time"), "{error}");
        assert!(clock.sleeps.borrow().is_empty());
    }

    #[test]
    fn an_attempt_that_overruns_ends_the_wait_without_another_pause() {
        // A caller that ignores the budget still gets no sleep past the deadline.
        let clock = FakeClock::new();
        let error = wait_until(
            &clock,
            Duration::from_secs(1),
            Duration::from_millis(500),
            &mut |_| {
                clock.advance(Duration::from_secs(2));
                anyhow::bail!("slow")
            },
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "slow");
        assert!(clock.sleeps.borrow().is_empty());
    }

    #[test]
    fn a_short_attempt_budget_is_reported_in_milliseconds() {
        let error = within(Duration::from_millis(40), || {
            std::thread::sleep(Duration::from_millis(500));
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("within 40ms"), "{error}");
        let error = within(Duration::from_secs(1), || {
            std::thread::sleep(Duration::from_secs(3));
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("within 1s "), "{error}");
    }

    /// A Docker client that stalls in every command: it records the sleeper it
    /// started and waits for it.
    #[cfg(unix)]
    fn stalling_docker(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let pids = dir.join("pids");
        let docker = dir.join("docker");
        std::fs::write(
            &docker,
            format!(
                "#!/bin/sh\nsleep 60 &\necho $! >> '{}'\nwait\n",
                pids.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        (docker, pids)
    }

    #[cfg(unix)]
    fn server_with_docker(dir: &std::path::Path, docker: std::path::PathBuf) -> Server {
        let managed = super::super::managed::ManagedServer::for_test(dir.join("server"), docker);
        std::fs::create_dir_all(&managed.dir).unwrap();
        std::fs::write(managed.compose_file(), "services: {}\n").unwrap();
        Server {
            project_id: uuid::Uuid::nil(),
            project_name: "test".into(),
            settings: crate::config::PostgresSection::default(),
            managed: Some(managed),
            root: std::path::PathBuf::new(),
        }
    }

    #[cfg(unix)]
    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 delivers nothing; it only asks whether the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[cfg(unix)]
    #[test]
    fn a_docker_that_stalls_in_the_port_lookup_cannot_outlast_the_readiness_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let (docker, pids) = stalling_docker(dir.path());
        let server = server_with_docker(dir.path(), docker);
        let started = Instant::now();
        let error = wait_ready_within(&server, Duration::from_secs(3)).unwrap_err();
        // The deadline held (the stall lasts a minute), and the error says why.
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        let shown = format!("{error:#}");
        assert!(
            shown.contains("does not accept connections") && shown.contains("did not answer"),
            "{shown}"
        );
        // What it started was killed and reaped, not left behind.
        let recorded = std::fs::read_to_string(&pids).unwrap();
        assert!(!recorded.trim().is_empty(), "the stand-in ran");
        for pid in recorded
            .lines()
            .filter_map(|line| line.trim().parse::<i32>().ok())
        {
            let gone = (0..60).any(|_| {
                let gone = !alive(pid);
                if !gone {
                    std::thread::sleep(Duration::from_millis(50));
                }
                gone
            });
            assert!(gone, "process {pid} was left behind");
        }
    }

    #[cfg(unix)]
    #[test]
    fn every_endpoint_lookup_is_bounded_even_without_a_deadline_to_share() {
        let dir = tempfile::tempdir().unwrap();
        let (docker, _) = stalling_docker(dir.path());
        let server = server_with_docker(dir.path(), docker);
        let started = Instant::now();
        let error = Target::resolve_within(&server, Duration::from_millis(400))
            .err()
            .expect("the lookup gave up");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        assert!(
            format!("{error:#}").contains("did not answer within"),
            "{error:#}"
        );
    }

    #[test]
    fn snapshot_names_are_validated() {
        assert!(validate_snapshot_name("seed").is_ok());
        assert!(validate_snapshot_name("prod-copy_2026.10").is_ok());
        for bad in ["", "   ", "a@b", "prod copy", "tab\there", &"x".repeat(101)] {
            assert!(validate_snapshot_name(bad).is_err(), "{bad:?}");
        }
    }
}
