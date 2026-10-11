//! Container services: copy-on-write branches of Valkey, Redis and MySQL.
//!
//! A branch is a container of its own, with its own volume, port and
//! password. Branches are cloned by copying the source's data (`dump.rdb`
//! for Valkey and Redis, the whole data directory for MySQL) into a new
//! volume with a reflink inside the Docker VM, so a branch takes almost no
//! space until it diverges.
//!
//! - [`catalog`] is the authority: a versioned, locked JSON file per project
//!   bound to the Docker daemon, holding the branches, the snapshots and the
//!   operation journal. Labels only corroborate it.
//! - A clone, snapshot or reset is journaled first and then run by a
//!   *coordinator*, a separate process (`devflow __clone`, its own process
//!   group) that holds the source's lock until the end, so a caller that
//!   dies does not leave a half-done operation behind, and a coordinator
//!   that dies is recovered from the journal by the next operation
//!   ([`Service::reconcile`] and every start, stop and drop).
//! - [`profile`] is what makes Valkey/Redis clonable: AOF off, RDB on, a
//!   `BGSAVE` completed and verified before the file is copied (or, when
//!   nothing changed since the last save, that save's file as it is). [`mysql`] is
//!   MySQL 8.4's: standalone InnoDB, a source shut down cleanly (verified)
//!   for the copy and started again, and a clone whose identity is set while
//!   it has no network.
//! - [`probe`] measures, per Docker daemon, whether volumes really share
//!   blocks (`verified`, `copy` or `unknown`).
//! - [`workspace`] ties branches to worktrees (receipts) for the pipeline
//!   and the CLI.
//!
//! See docs/plans/2026-10-10-devflow-dx.md (D2, D3).
pub mod catalog;
pub mod docker;
mod effects;
mod exec;
mod helpers;
mod launch;
mod locks;
pub mod mysql;
mod ops;
mod plan;
pub mod probe;
pub mod profile;
mod recover;
mod resp;
mod stop;
pub mod workspace;

use crate::config::{ContainerSection, CowCapability, Engine};
use crate::project::Project;
use anyhow::{Context, Result, bail};
use catalog::{
    BranchRecord, CloneMethod, RecordState, ServiceCatalog, SnapshotRecord, SourceRef, Store,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

pub use effects::{Effects, Operation, StoppedSource, clone_stops_source, effects};
pub use exec::coordinate;
pub use ops::Adopted;
pub use plan::NewBranch;
pub use recover::ReconcileReport;

/// One configured (or recorded) container service on one Docker daemon.
#[derive(Debug, Clone)]
pub struct Service {
    pub project: Uuid,
    project_name: String,
    pub key: String,
    pub engine: Engine,
    section: ContainerSection,
    store: Store,
    orbstack: bool,
    /// The project's root, for the remedies of the errors this service
    /// raises; empty for a service made from the catalog alone (a coordinator
    /// that has no project), whose errors then carry none.
    root: std::path::PathBuf,
}

/// Where a running branch is reached.
#[derive(Debug, Clone, Serialize)]
pub struct Endpoint {
    pub port: u16,
    /// `redis://:<password>@127.0.0.1:<port>/0` (Valkey, Redis) or
    /// `mysql://app:<password>@127.0.0.1:<port>/<database>`: the host endpoint.
    pub url: String,
    /// `host:6379` (`host:3306`) for containers on the same Docker networks
    /// (OrbStack).
    pub container_endpoint: Option<String>,
}

/// A branch as listings show it: everything but the password.
#[derive(Debug, Clone, Serialize)]
pub struct BranchView {
    pub id: Uuid,
    pub name: String,
    pub owner: Option<Uuid>,
    pub state: RecordState,
    /// What Docker says now: `running`, `exited`, `missing`, …
    pub container: String,
    pub container_id: Option<String>,
    pub container_name: String,
    pub volume: String,
    pub image: String,
    pub pin: String,
    pub port: Option<u16>,
    pub endpoint: Option<String>,
    pub container_endpoint: Option<String>,
    pub source: SourceRef,
    pub captured_at: Option<DateTime<Utc>>,
    pub capture_exact: bool,
    /// The capture is the source's last save, cloned without a new one.
    pub capture_last_save: bool,
    pub clone: Option<CloneMethod>,
    pub allocated_bytes: Option<i64>,
    /// MySQL: the database the branch has (no credentials).
    pub database: Option<String>,
    /// Milliseconds the source was unavailable while this branch was cloned
    /// from it (MySQL).
    pub source_downtime_ms: Option<u64>,
    pub generation: u32,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotView {
    pub id: Uuid,
    pub name: String,
    pub version: u32,
    pub state: RecordState,
    pub volume: String,
    pub source: SourceRef,
    pub image: String,
    pub pin: String,
    pub captured_at: Option<DateTime<Utc>>,
    pub capture_exact: bool,
    /// The capture is the source's last save, cloned without a new one.
    pub capture_last_save: bool,
    pub clone: Option<CloneMethod>,
    pub allocated_bytes: Option<i64>,
    /// Milliseconds the source was unavailable while it was snapshotted.
    pub source_downtime_ms: Option<u64>,
    pub created_at: DateTime<Utc>,
}

/// The last clone, snapshot or reset a service finished: how it was made and
/// how long its source was unavailable.
#[derive(Debug, Clone, Serialize)]
pub struct LastClone {
    /// `clone`, `snapshot` or `reset`.
    pub operation: String,
    pub target: Uuid,
    pub finished_at: DateTime<Utc>,
    pub method: Option<CloneMethod>,
    /// Milliseconds the source was stopped (MySQL); `None` when no source was
    /// stopped (it kept serving, or the source was a snapshot).
    pub source_downtime_ms: Option<u64>,
}

/// `devflow service status`.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceStatus {
    pub key: String,
    pub engine: Engine,
    pub image: String,
    pub docker: String,
    pub orbstack: bool,
    /// `verified`, `copy` or `unknown`, from the cached probe.
    pub cow: CowCapability,
    pub cow_note: Option<String>,
    pub cow_measured_at: Option<DateTime<Utc>>,
    pub require_cow: bool,
    pub branches: Vec<BranchView>,
    pub snapshots: usize,
    /// Journal entries that have not ended (a clone in progress or interrupted).
    pub unfinished: usize,
    /// The last finished clone, snapshot or reset.
    pub last_clone: Option<LastClone>,
}

impl Service {
    /// The container service `key` of `project`'s configuration.
    pub fn open(project: &Project, key: &str) -> Result<Service> {
        let (_, engine, section) = project
            .config
            .containers()
            .find(|(k, _, _)| *k == key)
            .with_context(|| {
                format!(
                    "{} has no container service '{key}' (valkey, redis or mysql)",
                    project.config_path.display()
                )
            })?;
        Service::new(
            project.id(),
            project.name(),
            key,
            engine,
            section.clone(),
            &docker::daemon_id()?,
        )
        .map(|service| service.in_project(project))
    }

    /// The service a receipt binding names, whether or not the configuration
    /// still has it (removing a service never deletes its data, so its
    /// branches can still be listed and dropped).
    pub fn recorded(project: &Project, key: &str, engine: Engine) -> Result<Service> {
        let section = project
            .config
            .containers()
            .find(|(k, _, _)| *k == key)
            .map(|(_, _, section)| section.clone())
            .unwrap_or_default();
        Service::new(
            project.id(),
            project.name(),
            key,
            engine,
            section,
            &docker::daemon_id()?,
        )
        .map(|service| service.in_project(project))
    }

    /// The service of a recorded key, for a coordinator: the catalog knows
    /// its engine; nothing else is read from the configuration.
    pub(crate) fn for_catalog(project: Uuid, key: &str) -> Result<Service> {
        let daemon = docker::daemon_id()?;
        let store = Store::open(project, &daemon)?;
        let engine = store
            .load()?
            .services
            .get(key)
            .map(|s| s.engine)
            .with_context(|| format!("the catalog has no service '{key}'"))?;
        Service::new(
            project,
            "",
            key,
            engine,
            ContainerSection::default(),
            &daemon,
        )
    }

    /// Remember the project's root, so that the errors raised carry remedies.
    fn in_project(mut self, project: &Project) -> Service {
        self.root = project.root.clone();
        self
    }

    /// A problem with the remedy "finish or roll back this service's
    /// interrupted operations".
    pub(crate) fn reconcile_problem(&self, message: impl Into<String>) -> crate::remedy::Problem {
        let problem = crate::remedy::Problem::new(message);
        if self.root.as_os_str().is_empty() {
            return problem;
        }
        problem.with(crate::remedy::Remedy::ReconcileService {
            root: self.root.clone(),
            key: self.key.clone(),
        })
    }

    /// A problem with the remedy "start this service" (the branch `branch`,
    /// when it is about one).
    pub(crate) fn start_problem(
        &self,
        message: impl Into<String>,
        branch: Option<Uuid>,
    ) -> crate::remedy::Problem {
        let problem = crate::remedy::Problem::new(message);
        if self.root.as_os_str().is_empty() {
            return problem;
        }
        problem.with(crate::remedy::Remedy::StartService {
            root: self.root.clone(),
            key: self.key.clone(),
            branch,
        })
    }

    fn new(
        project: Uuid,
        project_name: &str,
        key: &str,
        engine: Engine,
        section: ContainerSection,
        daemon: &str,
    ) -> Result<Service> {
        anyhow::ensure!(engine.is_container(), "{engine} is not a container engine");
        Ok(Service {
            project,
            project_name: project_name.to_string(),
            key: key.to_string(),
            engine,
            section,
            store: Store::open(project, daemon)?,
            orbstack: crate::db::orbstack(),
            root: std::path::PathBuf::new(),
        })
    }

    pub fn section(&self) -> &ContainerSection {
        &self.section
    }

    pub fn docker_id(&self) -> &str {
        self.store.docker()
    }

    pub fn catalog_path(&self) -> &std::path::Path {
        self.store.path()
    }

    /// This service's records (empty before the first branch).
    pub fn records(&self) -> Result<ServiceCatalog> {
        let catalog = self.store.load()?;
        match catalog.services.get(&self.key) {
            Some(service) => {
                anyhow::ensure!(
                    service.engine == self.engine,
                    "service '{}' was created as {} and is configured as {}; their data is not \
                     interchangeable (use another key)",
                    self.key,
                    service.engine,
                    self.engine
                );
                Ok(service.clone())
            }
            None => Ok(ServiceCatalog::new(self.engine)),
        }
    }

    /// The last clone, snapshot or reset this service finished, if any.
    pub fn last_clone(&self) -> Option<LastClone> {
        last_clone(&self.records().ok()?)
    }

    pub fn branch(&self, id: Uuid) -> Result<Option<BranchRecord>> {
        Ok(self.records()?.branch(id).cloned())
    }

    /// The branch an id as typed names: a whole UUID, or a unique prefix of at
    /// least 8 hex characters of one of the branches `list` shows. An unknown
    /// or ambiguous prefix is an error naming the candidates.
    pub fn resolve_branch(&self, id: &crate::ids::IdRef) -> Result<Uuid> {
        let records = self.records()?;
        id.resolve(
            "branch",
            records
                .branches
                .iter()
                .filter(|b| b.state != RecordState::Aborted)
                .map(|b| b.id),
        )
        .with_context(|| format!("service '{}'", self.key))
    }

    /// The branch record, or an error saying the catalog does not know it.
    pub(crate) fn require_branch(&self, id: Uuid) -> Result<BranchRecord> {
        self.branch(id)?
            .with_context(|| format!("the catalog of service '{}' has no branch {id}", self.key))
    }

    pub fn snapshot(&self, id: Uuid) -> Result<Option<SnapshotRecord>> {
        Ok(self.records()?.snapshot(id).cloned())
    }

    /// A ready snapshot by name (the latest version when `version` is `None`).
    pub fn find_snapshot(&self, name: &str, version: Option<u32>) -> Result<SnapshotRecord> {
        self.records()?
            .find_snapshot(name, version)
            .cloned()
            .with_context(|| match version {
                Some(v) => format!("service '{}' has no snapshot {name}@v{v}", self.key),
                None => format!("service '{}' has no snapshot named '{name}'", self.key),
            })
    }

    /// The URL and endpoints of a branch (it need not be running).
    pub fn endpoint(&self, record: &BranchRecord) -> Result<Endpoint> {
        let port = record.port.ok_or_else(|| {
            self.start_problem(
                format!("branch {} has no port yet; start it first", record.id),
                Some(record.id),
            )
            .into_error()
        })?;
        let (url, container_port) = match (self.engine, &record.mysql) {
            (Engine::Mysql, Some(identity)) => (
                mysql::url(
                    &identity.user,
                    &record.password,
                    "127.0.0.1",
                    port,
                    &identity.database,
                ),
                mysql::CONTAINER_PORT,
            ),
            (Engine::Mysql, None) => bail!(
                "branch {} records no database and account, so it has no URL",
                record.id
            ),
            _ => (
                profile::url(&record.password, "127.0.0.1", port),
                profile::CONTAINER_PORT,
            ),
        };
        Ok(Endpoint {
            port,
            url,
            container_endpoint: record
                .domain
                .as_ref()
                .map(|d| format!("{d}:{container_port}")),
        })
    }

    /// Branches with their live container state (a Docker query each).
    pub fn branch_views(&self) -> Result<Vec<BranchView>> {
        let records = self.records()?;
        let mut views = Vec::new();
        for branch in records
            .branches
            .iter()
            .filter(|b| b.state != RecordState::Aborted)
        {
            views.push(self.view(branch));
        }
        Ok(views)
    }

    fn view(&self, branch: &BranchRecord) -> BranchView {
        let container = match &branch.container_id {
            Some(id) => match docker::inspect_container(id) {
                Ok(Some(info)) => info.status,
                Ok(None) => "missing".to_string(),
                Err(_) => "unknown".to_string(),
            },
            None => "none".to_string(),
        };
        let endpoint = self.endpoint(branch).ok();
        BranchView {
            id: branch.id,
            name: branch.name.clone(),
            owner: branch.owner,
            state: branch.state,
            container,
            container_id: branch.container_id.clone(),
            container_name: branch.container_name.clone(),
            volume: branch.volume.clone(),
            image: branch.image.clone(),
            pin: branch.pin.clone(),
            port: branch.port,
            endpoint: endpoint.as_ref().map(|e| format!("127.0.0.1:{}", e.port)),
            container_endpoint: endpoint.and_then(|e| e.container_endpoint),
            source: branch.source,
            captured_at: branch.captured_at,
            capture_exact: branch.capture_exact,
            capture_last_save: branch.capture_last_save,
            clone: branch.clone,
            allocated_bytes: branch.allocated_bytes,
            database: branch.mysql.as_ref().map(|m| m.database.clone()),
            source_downtime_ms: branch.downtime_ms,
            generation: branch.generation,
            created_at: branch.created_at,
        }
    }

    pub fn snapshot_views(&self) -> Result<Vec<SnapshotView>> {
        Ok(self
            .records()?
            .snapshots
            .iter()
            .filter(|s| s.state != RecordState::Aborted)
            .map(|s| SnapshotView {
                id: s.id,
                name: s.name.clone(),
                version: s.version,
                state: s.state,
                volume: s.volume.clone(),
                source: s.source,
                image: s.image.clone(),
                pin: s.pin.clone(),
                captured_at: s.captured_at,
                capture_exact: s.capture_exact,
                capture_last_save: s.capture_last_save,
                clone: s.clone,
                allocated_bytes: s.allocated_bytes,
                source_downtime_ms: s.downtime_ms,
                created_at: s.created_at,
            })
            .collect())
    }

    /// The service's state for `devflow service status` and `doctor`. The
    /// copy-on-write result is the cached probe of this Docker daemon,
    /// measured now if there is none (`refresh` measures again).
    pub fn status(&self, refresh: bool) -> Result<ServiceStatus> {
        let probe = probe::capability(self.docker_id(), refresh);
        self.status_with(probe.capability, probe.note, Some(probe.measured_at))
    }

    /// [`Service::status`] for a caller that asks often (a window that
    /// refreshes): it never measures. The copy-on-write result is the cached
    /// probe of this Docker daemon; with none, it is `unknown`, with a note
    /// that it was not measured, and `cow_measured_at` is `None`. Measuring
    /// is a request of its own (`status(true)`).
    pub fn status_cached(&self) -> Result<ServiceStatus> {
        match probe::cached(self.docker_id()) {
            Some(found) => self.status_with(found.capability, found.note, Some(found.measured_at)),
            None => self.status_with(
                CowCapability::Unknown,
                Some("not measured on this Docker engine yet".to_string()),
                None,
            ),
        }
    }

    fn status_with(
        &self,
        cow: CowCapability,
        cow_note: Option<String>,
        cow_measured_at: Option<DateTime<Utc>>,
    ) -> Result<ServiceStatus> {
        let records = self.records()?;
        Ok(ServiceStatus {
            key: self.key.clone(),
            engine: self.engine,
            image: self.section.image(self.engine),
            docker: self.docker_id().to_string(),
            orbstack: self.orbstack,
            cow,
            cow_note,
            cow_measured_at,
            require_cow: self.section.require_cow,
            branches: self.branch_views()?,
            snapshots: records
                .snapshots
                .iter()
                .filter(|s| s.state == RecordState::Ready)
                .count(),
            unfinished: records.unfinished().len(),
            last_clone: last_clone(&records),
        })
    }

    // ---- names (chosen before the resources exist) ----

    fn project8(&self) -> String {
        short(&self.project, 8)
    }

    pub fn volume_name(&self, branch: Uuid, generation: u32) -> String {
        format!(
            "devflow-{}-{}-{}-g{generation}",
            self.project8(),
            self.key,
            short(&branch, 12)
        )
    }

    pub(crate) fn snapshot_volume_name(&self, snapshot: Uuid) -> String {
        format!(
            "devflow-{}-{}-s{}",
            self.project8(),
            self.key,
            short(&snapshot, 12)
        )
    }

    pub(crate) fn container_name(&self, branch: Uuid, generation: u32) -> String {
        format!(
            "devflow-{}-{}-{}-g{generation}",
            self.project8(),
            self.key,
            short(&branch, 12)
        )
    }

    /// The name of the destination's bootstrap container (MySQL), chosen
    /// before the container exists.
    pub(crate) fn boot_name(&self, branch: Uuid, generation: u32) -> String {
        format!("{}-boot", self.container_name(branch, generation))
    }

    pub(crate) fn helper_name(&self, op: Uuid) -> String {
        format!(
            "devflow-{}-{}-copy-{}",
            self.project8(),
            self.key,
            short(&op, 12)
        )
    }

    /// `<branch8>.<key>.<project>-<project8>.devflow.orb.local`: the identity
    /// of the project is part of it, so two projects with the same name, or
    /// two branches with the same prefix of different services, never share a
    /// name. Underscores of the key become hyphens (host names have none).
    pub(crate) fn domain_name(&self, branch: Uuid) -> String {
        let key = self.key.replace('_', "-");
        format!(
            "{}.{}.{}-{}.devflow.orb.local",
            short(&branch, 8),
            key.trim_end_matches('-'),
            crate::db::sanitize_name(&self.project_name),
            self.project8()
        )
    }

    pub(crate) fn labels(&self, branch: Uuid) -> Vec<(String, String)> {
        vec![
            (docker::LABEL_PROJECT.into(), self.project.to_string()),
            (docker::LABEL_SERVICE.into(), self.key.clone()),
            (docker::LABEL_BRANCH.into(), branch.to_string()),
            (docker::LABEL_ENGINE.into(), self.engine.as_str().into()),
            (docker::LABEL_VERSION.into(), "1".into()),
        ]
    }

    pub(crate) fn lock_dir(&self) -> std::path::PathBuf {
        self.store.lock_dir()
    }

    pub(crate) fn orbstack(&self) -> bool {
        self.orbstack
    }

    /// Run `change` on this service's records under the catalog lock.
    pub(crate) fn update<T>(
        &self,
        change: impl FnOnce(&mut ServiceCatalog) -> Result<T>,
    ) -> Result<T> {
        self.store
            .update(|catalog| change(catalog.service_mut(&self.key, self.engine)?))
    }
}

impl Service {
    /// [`Service::update`] for a planner: the write that journals a new branch,
    /// snapshot or reset. Refused ([`catalog::Forgetting`]) while `forget` has
    /// marked the service, checked under the same catalog lock that the write
    /// is made under, so nothing can be recorded behind a `forget`'s back.
    pub(crate) fn update_planning<T>(
        &self,
        change: impl FnOnce(&mut ServiceCatalog) -> Result<T>,
    ) -> Result<T> {
        self.update(|service| {
            if service.forgetting.is_some() {
                return Err(catalog::Forgetting {
                    key: self.key.clone(),
                    root: self.root.clone(),
                }
                .into());
            }
            change(service)
        })
    }
}

/// The finished clone, snapshot or reset that ended last.
fn last_clone(records: &ServiceCatalog) -> Option<LastClone> {
    use catalog::{OpKind, Step};
    let op = records
        .journal
        .iter()
        .filter(|op| {
            op.step == Step::Done
                && matches!(op.kind, OpKind::Clone | OpKind::Snapshot | OpKind::Reset)
        })
        .max_by_key(|op| op.updated_at)?;
    let (operation, method) = match op.kind {
        OpKind::Clone => ("clone", records.branch(op.target).and_then(|b| b.clone)),
        OpKind::Snapshot => (
            "snapshot",
            records.snapshot(op.target).and_then(|s| s.clone),
        ),
        _ => ("reset", records.branch(op.target).and_then(|b| b.clone)),
    };
    Some(LastClone {
        operation: operation.to_string(),
        target: op.target,
        finished_at: op.updated_at,
        method,
        source_downtime_ms: op.downtime_ms,
    })
}

/// The first `n` hex digits of an id.
fn short(id: &Uuid, n: usize) -> String {
    id.simple().to_string()[..n].to_string()
}

/// The coordinator's executable: [`crate::paths::devflow_program`].
pub(crate) fn coordinator_program() -> Result<std::path::PathBuf> {
    crate::paths::devflow_program()
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn service() -> Service {
        Service {
            project: "0b3c9d2e-1111-4222-8333-444455556666".parse().unwrap(),
            project_name: "Shop App!".into(),
            key: "my_cache".into(),
            engine: Engine::Valkey,
            section: ContainerSection::default(),
            store: Store::open(Uuid::nil(), "daemon").unwrap(),
            orbstack: true,
            root: std::path::PathBuf::new(),
        }
    }

    #[test]
    fn a_branch_without_a_port_offers_to_start_it() {
        let mut svc = service();
        svc.root = std::path::PathBuf::from("/shop");
        let branch = BranchRecord::for_test(Uuid::nil());
        let error = svc.endpoint(&branch).unwrap_err();
        assert!(!error.to_string().contains("`devflow"), "{error}");
        assert_eq!(
            crate::remedy::remedies_of(&error),
            [crate::remedy::Remedy::StartService {
                root: "/shop".into(),
                key: "my_cache".into(),
                branch: Some(Uuid::nil()),
            }]
        );
        // A service made from the catalog alone knows no project: no remedy.
        assert!(crate::remedy::remedies_of(&service().endpoint(&branch).unwrap_err()).is_empty());
    }

    #[test]
    fn names_are_chosen_from_identities_and_never_collide_across_generations() {
        let svc = service();
        let branch: Uuid = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".parse().unwrap();
        assert_eq!(
            svc.volume_name(branch, 1),
            "devflow-0b3c9d2e-my_cache-aaaaaaaabbbb-g1"
        );
        assert_ne!(svc.volume_name(branch, 1), svc.volume_name(branch, 2));
        assert_eq!(
            svc.container_name(branch, 2),
            "devflow-0b3c9d2e-my_cache-aaaaaaaabbbb-g2"
        );
        let other: Uuid = "aaaaaaaa-bbbb-5ccc-8ddd-eeeeeeeeeeee".parse().unwrap();
        // The first 12 hex digits could collide in principle; the full
        // identity is in the labels and the catalog, never in the name alone.
        assert_eq!(svc.volume_name(other, 1), svc.volume_name(branch, 1));
        let snapshot = svc.snapshot_volume_name(branch);
        assert!(snapshot.contains("-s") && snapshot != svc.volume_name(branch, 1));
        assert!(svc.helper_name(branch).contains("-copy-"));
    }

    #[test]
    fn orbstack_names_carry_the_project_identity_and_are_host_names() {
        let svc = service();
        let branch: Uuid = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".parse().unwrap();
        assert_eq!(
            svc.domain_name(branch),
            "aaaaaaaa.my-cache.shop-app-0b3c9d2e.devflow.orb.local"
        );
        // Another project (same name, another id) never shares it.
        let mut other = service();
        other.project = "ffffffff-1111-4222-8333-444455556666".parse().unwrap();
        assert_ne!(svc.domain_name(branch), other.domain_name(branch));
        // Another branch of the same service differs in its first label.
        let next: Uuid = "bbbbbbbb-bbbb-4ccc-8ddd-eeeeeeeeeeee".parse().unwrap();
        assert_ne!(svc.domain_name(branch), svc.domain_name(next));
        for label in svc.domain_name(branch).split('.') {
            assert!(label.len() <= 63 && !label.contains('_'), "{label}");
        }
    }

    #[test]
    fn containers_are_labelled_with_the_project_service_and_branch() {
        let svc = service();
        let branch = Uuid::from_u128(5);
        let labels = svc.labels(branch);
        let get = |k: &str| {
            labels
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get(docker::LABEL_PROJECT), Some(svc.project.to_string()));
        assert_eq!(get(docker::LABEL_SERVICE).as_deref(), Some("my_cache"));
        assert_eq!(get(docker::LABEL_BRANCH), Some(branch.to_string()));
        assert_eq!(get(docker::LABEL_ENGINE).as_deref(), Some("valkey"));
    }
}
