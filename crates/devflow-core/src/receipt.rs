//! Per-worktree setup receipt, stored in the worktree's own Git directory
//! (`git rev-parse --git-dir`/devflow/receipt.json) so it is unique per
//! worktree and disappears with `git worktree remove`/`prune`.
//!
//! Every pipeline phase is recorded as `started` before its effect and
//! `done`/`skipped`/`failed` after it; re-running setup resumes at the first
//! phase that is not done.
//!
//! Version 2 binds data per service (`services.<key>`). A version 1 receipt
//! (one `data` binding, a `database` phase) is read as is and migrated in
//! place by [`Receipt::migrate_in_place`] under the receipt lock, with no
//! provisioning effect: every field survives, `data` moves to the project's
//! sole PostgreSQL service, or to `unmapped` when the configuration has none.
//!
//! Two locks live in the worktree's Git directory, `devflow/setup.lock` (the
//! receipt lock: setup, removal, and every write of the receipt) and
//! `devflow/process.lock` (the process lock: one operation at a time on the
//! workspace's pitchfork daemons, held from the record of intent through the
//! effect to the verification). The order is fixed: **process lock first, then
//! receipt lock**, never the other way round. A writer of the receipt's
//! `processes` record has to hold the process lock ([`Receipt::update_processes`]
//! takes it as an argument), and takes the receipt lock briefly itself.
use crate::config::{Config, Engine};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const VERSION: u32 = 2;

/// Detail recorded when `--no-db` skips the services phase.
pub(crate) const SKIP_NO_DB: &str = "--no-db";
/// Detail recorded when `--no-hooks` skips the hooks phase.
pub(crate) const SKIP_NO_HOOKS: &str = "--no-hooks";
/// Detail recorded when the implicit setup of the default workspace skips hooks.
pub(crate) const SKIP_DEFAULT_HOOKS: &str =
    "not run when the default workspace is initialized implicitly";

/// A phase's recorded detail in words for a person: the markers of a skip
/// someone asked for are the flag names older receipts keep, which no
/// graphical frontend shows as they are.
pub fn plain_detail(detail: &str) -> &str {
    match detail {
        SKIP_NO_DB => "no database was asked for",
        SKIP_NO_HOOKS => "no hooks were asked for",
        other => other,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Materialize,
    Payload,
    /// Binds the configured services (version 1 receipts call it `database`).
    #[serde(alias = "database")]
    Services,
    Env,
    Hooks,
}

impl Phase {
    pub const ALL: [Phase; 5] = [
        Phase::Materialize,
        Phase::Payload,
        Phase::Services,
        Phase::Env,
        Phase::Hooks,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Materialize => "materialize",
            Phase::Payload => "payload",
            Phase::Services => "services",
            Phase::Env => "env",
            Phase::Hooks => "hooks",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseState {
    #[default]
    Pending,
    Started,
    Done,
    /// Deliberately not run (no database configured, hooks untrusted, …).
    /// `required_skip` in the record says whether readiness is affected.
    Skipped,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseRecord {
    pub state: PhaseState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// A skip that leaves the workspace `needs_attention` (e.g. untrusted hooks).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required_skip: bool,
    /// A skip someone asked for (`--no-db`, `--no-hooks`) or that is part of
    /// the workspace's design. A configuration change never reopens it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deliberate: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    /// When the attempt that ended in this state began: the time of the
    /// `started` record it replaced. Absent for a phase recorded without one
    /// (a reopened phase, a receipt from before durations were kept).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
}

impl PhaseRecord {
    /// How long the attempt took, when both ends were recorded.
    pub fn duration(&self) -> Option<std::time::Duration> {
        if self.state == PhaseState::Started {
            return None;
        }
        self.updated_at?
            .signed_duration_since(self.started_at?)
            .to_std()
            .ok()
    }
}

/// What the payload phase did with one `worktree.copy` path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PayloadOutcome {
    /// Every file shares blocks with the source (the operating system cloned it).
    Cloned,
    /// Some files were cloned, some copied.
    Mixed,
    /// Bytes were copied: no copy-on-write between these locations.
    Copied,
    /// The destination already had the path; it was left alone.
    AlreadyPresent,
    /// The source worktree has no such path.
    Missing,
}

impl PayloadOutcome {
    fn of(result: &crate::cow::EntryResult) -> Self {
        use crate::cow::{EntryResult, Method};
        match result {
            EntryResult::Done {
                method: Method::Cloned,
            } => Self::Cloned,
            EntryResult::Done {
                method: Method::Mixed,
            } => Self::Mixed,
            EntryResult::Done {
                method: Method::Copied,
            } => Self::Copied,
            EntryResult::AlreadyPresent => Self::AlreadyPresent,
            EntryResult::Missing => Self::Missing,
        }
    }

    /// The path was written by this workspace's own payload phase.
    pub fn was_written(self) -> bool {
        matches!(self, Self::Cloned | Self::Mixed | Self::Copied)
    }
}

/// One `worktree.copy` path and what happened to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayloadRecord {
    pub path: String,
    pub outcome: PayloadOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    InProgress,
    Ready,
    NeedsAttention,
}

/// A database this worktree is attached to: the version 1 receipt's `data`,
/// and the part of a [`ServiceBinding`] that says where the database is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataBinding {
    /// Server-side branch id; `None` when attached with only a URL
    /// (`devflow setup --database-url`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<uuid::Uuid>,
    pub database: String,
    pub role: String,
    /// Full application URL including the branch password.
    pub url: String,
    /// What the branch was created from (`empty`, `workspace:NAME`, `snapshot:NAME@vN`, `url`).
    pub source: String,
    /// This instance created the branch, so it may reset or drop it.
    /// `false` for URL-only attachments and copied (forked) receipts.
    #[serde(default)]
    pub owned: bool,
}

/// A scratch database a binding lists (`extra_databases`): the generated name
/// of the database on the server. No credentials: it is reached with the
/// binding's own login, so its URL is the binding's with this database
/// ([`ServiceBinding::extra_url`]) and follows a rotated password or a moved
/// endpoint with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtraBinding {
    pub database: String,
}

/// How a container service's data was cloned, as reported to the user
/// ("services: cache reflink"): never assumed from the filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloneReport {
    /// `reflink` or `copy`; `None` for an empty root (nothing was cloned).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<crate::services::catalog::CloneMethod>,
    /// When the data was captured (the `BGSAVE` fork of the source).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<DateTime<Utc>>,
    /// Blocks the clone took in the Docker VM, estimated from `df`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocated_bytes: Option<i64>,
    /// Milliseconds the source was unavailable while it was cloned: a MySQL
    /// source is shut down cleanly for the copy and started again. `None`
    /// when the source kept serving (Valkey and Redis) or nothing was cloned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_downtime_ms: Option<u64>,
}

/// What a worktree is bound to for one configured service
/// (`receipt.services.<key>`). PostgreSQL bindings name a database and a
/// role on the shared server; container engines (`valkey`, `redis`) name a
/// branch of the project's catalog and leave those two empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceBinding {
    pub engine: Engine,
    /// Server-side branch id (the catalog's, for container engines); `None`
    /// for a URL-only attachment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<uuid::Uuid>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub database: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub role: String,
    /// Full application URL including the branch password.
    pub url: String,
    /// Container engines: `host:port` other containers on the same Docker
    /// networks reach the branch at (OrbStack names); the host endpoint is `url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Container engines: how the data was cloned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloned: Option<CloneReport>,
    /// What the branch was created from (`empty`, `workspace:NAME`, `snapshot:NAME@vN`, `url`).
    pub source: String,
    /// This instance created the branch, so it may reset or drop it.
    #[serde(default)]
    pub owned: bool,
    /// The configuration generation of the service when this binding was
    /// applied ([`crate::config::ServiceConfig::generation`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied: Option<String>,
    /// The configuration no longer has this service and the removal was
    /// applied (the environment was rewritten without it). The binding and
    /// its provenance stay; this only stops the removal from reading as
    /// pending drift forever. Cleared when the service is configured again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub orphaned: bool,
    /// The scratch databases of the branch that the configuration lists and
    /// the server has ready, by configured name. Their identities are
    /// recorded on the server; this is what `.devflow/env` exports.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extras: BTreeMap<String, ExtraBinding>,
}

impl ServiceBinding {
    /// A PostgreSQL binding.
    pub fn postgres(data: DataBinding, applied: Option<String>) -> Self {
        Self {
            engine: Engine::Postgres,
            branch_id: data.branch_id,
            database: data.database,
            role: data.role,
            url: data.url,
            endpoint: None,
            cloned: None,
            source: data.source,
            owned: data.owned,
            applied,
            orphaned: false,
            extras: BTreeMap::new(),
        }
    }

    /// A container-engine binding (`valkey`, `redis`, `mysql`): a branch of the
    /// project's catalog, reached at `url` on the host and `endpoint` from
    /// other containers.
    pub fn container(
        engine: Engine,
        branch_id: uuid::Uuid,
        url: String,
        endpoint: Option<String>,
        source: String,
        cloned: CloneReport,
        applied: Option<String>,
    ) -> Self {
        Self {
            engine,
            branch_id: Some(branch_id),
            database: String::new(),
            role: String::new(),
            url,
            endpoint,
            cloned: Some(cloned),
            source,
            owned: true,
            applied,
            orphaned: false,
            extras: BTreeMap::new(),
        }
    }

    /// The URL of scratch database `name`: this binding's URL (login,
    /// password, endpoint, options) with the scratch database in place of the
    /// branch's own. `None` when the binding lists no such database.
    pub fn extra_url(&self, name: &str) -> Option<String> {
        crate::conn::url_with_database(&self.url, &self.extras.get(name)?.database)
    }

    /// Whether the binding is a bare URL: no server-side identity to reset or drop.
    pub fn is_url_only(&self) -> bool {
        self.branch_id.is_none()
    }
}

/// How the current configuration differs from the one a receipt was applied
/// under; see [`Receipt::drift`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Drift {
    /// Configured services with no binding.
    pub added: Vec<String>,
    /// Configured services bound under another generation of their section.
    pub changed: Vec<String>,
    /// Services that were bound under a configuration and are no longer in
    /// it, whose removal has not been applied yet
    /// ([`Receipt::acknowledge_removals`]). Their bindings stay in the receipt
    /// (nothing is deleted).
    pub removed: Vec<String>,
    /// `.devflow/env` was written under another env generation.
    pub env: bool,
}

impl Drift {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty() && !self.env
    }

    /// A short sentence fragment for phase details.
    fn describe(&self) -> String {
        let quoted = |keys: &[String]| {
            keys.iter()
                .map(|k| format!("'{k}'"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut parts = Vec::new();
        if !self.added.is_empty() {
            parts.push(format!("service {} added", quoted(&self.added)));
        }
        if !self.changed.is_empty() {
            parts.push(format!("service {} changed", quoted(&self.changed)));
        }
        if !self.removed.is_empty() {
            parts.push(format!("service {} removed", quoted(&self.removed)));
        }
        if parts.is_empty() {
            parts.push("environment settings changed".into());
        }
        parts.join(", ")
    }
}

/// [`Config::env_generation`] with no service in it: the `[env]` recipes and
/// presets alone.
fn env_generation_without_services(config: &Config) -> String {
    use sha2::{Digest, Sha256};
    let json =
        serde_json::to_vec(&("env.v2.no-services", config.env.canonical())).unwrap_or_default();
    Sha256::digest(json)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A branch creation that a service began and whose credentials are not
/// recorded yet. It is written before the branch exists, with the identity the
/// branch will have, so a crash between the two is recovered by that identity
/// and for that service only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingBranch {
    /// The service key the creation was begun for.
    pub service: String,
    /// The branch id the server was asked to create.
    pub branch_id: uuid::Uuid,
}

/// Daemons devflow started for a workspace and has not seen stop.
///
/// Recorded before devflow asks pitchfork to start daemons (`up`, autostart,
/// the app's start and restart), and cleared after a stop whose result was
/// read back and showed nothing of the workspace running. When pitchfork's
/// supervisor is down this is what tells "devflow never started anything
/// here" from "daemons devflow started may still be running": a supervisor
/// that crashed leaves its daemons running and cannot be asked about them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessRecord {
    /// When devflow last started daemons of this workspace.
    pub started_at: DateTime<Utc>,
    /// The `namespace/name` ids it asked pitchfork to start, sorted and unique.
    pub ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub version: u32,
    pub instance_id: uuid::Uuid,
    pub project_id: uuid::Uuid,
    /// Workspace (branch) name at creation.
    pub workspace: String,
    /// Workspace the code (and payloads) came from; `None` for adopted checkouts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_source: Option<String>,
    /// Requested data source, recorded before provisioning so a retry uses the same one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_source: Option<String>,
    /// The identity `data_source` resolved to (`empty`, `branch:<id>`,
    /// `snapshot:<id>`), recorded together with it: a retry clones exactly this
    /// source even if the workspace or snapshot name now means something else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_source_id: Option<String>,
    /// The data source each container service chose, by service key, as the
    /// label of what it resolved to (`empty`, `workspace:NAME`,
    /// `snapshot:NAME@vN`, `branch:ID`). Pinned per service before the branch
    /// is made: a retry uses it whatever `--data-from` says now, and a source
    /// that is missing fails that service instead of being replaced.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub service_sources: BTreeMap<String, String>,
    /// The catalog identity each of those labels resolved to (`empty`,
    /// `branch:<id>`, `snapshot:<id>`), recorded with the label. A retry
    /// clones exactly this source and never looks the label up again: a
    /// workspace or snapshot name may mean something else by then, and a
    /// source that is gone fails the service.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub service_source_ids: BTreeMap<String, String>,
    /// Bindings by service key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub services: BTreeMap<String, ServiceBinding>,
    /// The branch creation in flight, if any; see [`PendingBranch`]. Cleared
    /// when the binding is recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creating: Option<PendingBranch>,
    /// A version 1 binding that could not be mapped to a service: kept,
    /// reported by `list` and `doctor`, never guessed. (Version 1 files call
    /// it `data`; until [`Receipt::migrate`] runs, that is what this holds.)
    #[serde(default, alias = "data", skip_serializing_if = "Option::is_none")]
    pub unmapped: Option<DataBinding>,
    /// The environment generation ([`Config::env_generation`]) `.devflow/env`
    /// was last written under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_generation: Option<String>,
    #[serde(default)]
    pub phases: BTreeMap<String, PhaseRecord>,
    /// What the payload phase did with each `worktree.copy` path (how it was
    /// cloned), kept across retries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub payload: Vec<PayloadRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks_digest: Option<String>,
    /// When the setup hooks last ran with `--skip-mise-check`: nobody checked
    /// that mise hands them this workspace's variables. Cleared when the
    /// hooks phase runs again with the check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mise_check_skipped_at: Option<DateTime<Utc>>,
    /// The daemons devflow started and has not seen stop; see [`ProcessRecord`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processes: Option<ProcessRecord>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Receipt {
    pub fn new(project_id: uuid::Uuid, workspace: &str, code_source: Option<String>) -> Self {
        let now = Utc::now();
        Self {
            version: VERSION,
            // Always a fresh identity: `DEVFLOW_INSTANCE_ID` is applied by an
            // explicit `setup::setup` alone, never by whatever creates a receipt.
            instance_id: uuid::Uuid::new_v4(),
            project_id,
            workspace: workspace.to_string(),
            code_source,
            data_source: None,
            data_source_id: None,
            service_sources: BTreeMap::new(),
            service_source_ids: BTreeMap::new(),
            services: BTreeMap::new(),
            creating: None,
            unmapped: None,
            env_generation: None,
            phases: Default::default(),
            payload: Vec::new(),
            hooks_digest: None,
            mise_check_skipped_at: None,
            processes: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// `df-<project8>-<instance8>`: the first eight hex characters of the
    /// project id and of this workspace's instance id. Unique per workspace
    /// and a valid Docker Compose project name.
    pub fn slug(&self) -> String {
        let head = |id: &uuid::Uuid| id.simple().to_string()[..8].to_string();
        format!("df-{}-{}", head(&self.project_id), head(&self.instance_id))
    }

    pub fn phase(&self, phase: Phase) -> PhaseRecord {
        self.phases.get(phase.as_str()).cloned().unwrap_or_default()
    }

    pub fn set_phase(&mut self, phase: Phase, state: PhaseState, detail: Option<String>) {
        let now = Utc::now();
        // The attempt began when the `started` record it replaces was written.
        let started_at = self
            .phases
            .get(phase.as_str())
            .filter(|previous| previous.state == PhaseState::Started)
            .and_then(|previous| previous.updated_at)
            .filter(|_| state != PhaseState::Started);
        self.phases.insert(
            phase.as_str().to_string(),
            PhaseRecord {
                state,
                detail,
                required_skip: false,
                deliberate: false,
                updated_at: Some(now),
                started_at,
            },
        );
        self.updated_at = now;
    }

    /// Record what the payload phase did. A path that was already there
    /// (a retry) keeps the record of how an earlier attempt wrote it.
    pub fn record_payload(&mut self, entries: &[crate::cow::EntryReport]) {
        let previous = std::mem::take(&mut self.payload);
        self.payload = entries
            .iter()
            .map(|entry| {
                let outcome = PayloadOutcome::of(&entry.result);
                let kept = previous
                    .iter()
                    .find(|p| p.path == entry.path && p.outcome.was_written());
                match (outcome, kept) {
                    (PayloadOutcome::AlreadyPresent, Some(kept)) => kept.clone(),
                    _ => PayloadRecord {
                        path: entry.path.clone(),
                        outcome,
                    },
                }
            })
            .collect();
    }

    pub fn skip_required(&mut self, phase: Phase, detail: String) {
        self.set_phase(phase, PhaseState::Skipped, Some(detail));
        if let Some(record) = self.phases.get_mut(phase.as_str()) {
            record.required_skip = true;
        }
    }

    /// Record a skip someone asked for; see [`PhaseRecord::deliberate`].
    pub fn skip_deliberate(&mut self, phase: Phase, detail: String) {
        self.set_phase(phase, PhaseState::Skipped, Some(detail));
        if let Some(record) = self.phases.get_mut(phase.as_str()) {
            record.deliberate = true;
        }
    }

    pub fn status(&self) -> Status {
        let records: Vec<_> = Phase::ALL.iter().map(|p| self.phase(*p)).collect();
        if records.iter().any(|r| {
            r.state == PhaseState::Failed || (r.state == PhaseState::Skipped && r.required_skip)
        }) {
            Status::NeedsAttention
        } else if records
            .iter()
            .all(|r| matches!(r.state, PhaseState::Done | PhaseState::Skipped))
        {
            Status::Ready
        } else {
            Status::InProgress
        }
    }

    /// The PostgreSQL binding of the project `config` describes: the binding
    /// under its service's key. A project without a PostgreSQL service has
    /// none, except for a URL-only attachment (`devflow setup --database-url`)
    /// made while no service was configured, when there is exactly one: it
    /// names no server resource, so nothing is guessed. Bindings of services
    /// the configuration no longer has (an attachment made under a configured
    /// service included), and [`Receipt::unmapped`], are never returned.
    pub fn postgres(&self, config: &Config) -> Option<(&str, &ServiceBinding)> {
        match config.postgres() {
            Some((key, _)) => self
                .services
                .get_key_value(key)
                .filter(|(_, binding)| binding.engine == Engine::Postgres)
                .map(|(key, binding)| (key.as_str(), binding)),
            None => {
                // Only an attachment made with no service configured
                // (nothing was applied under a service's section). A URL
                // attached under a configured service stays that service's:
                // removing the service must not turn it into an export.
                let mut attached = self.services.iter().filter(|(_, b)| {
                    b.engine == Engine::Postgres && b.is_url_only() && b.applied.is_none()
                });
                let only = attached.next()?;
                attached
                    .next()
                    .is_none()
                    .then_some((only.0.as_str(), only.1))
            }
        }
    }

    /// How `config` differs from the configuration this receipt's services
    /// and environment were applied under. When the services phase was
    /// skipped on request (`--no-db`), nothing derived from services applies
    /// to the workspace and no service drifts; the environment still does,
    /// because `.devflow/env` holds the `[env]` recipes and
    /// `COMPOSE_PROJECT_NAME` whatever the services are.
    pub fn drift(&self, config: &Config) -> Drift {
        let skipped = self.services_skipped_on_request();
        let mut drift = Drift {
            // No recorded generation means `.devflow/env` was never written by
            // this receipt: the env phase runs anyway, and there is nothing to
            // compare. Without services, a change to the services alone is
            // not a change to the file.
            env: self.env_generation.as_deref().is_some_and(|written| {
                written != config.env_generation()
                    && !(skipped && written == env_generation_without_services(config))
            }),
            ..Drift::default()
        };
        if skipped {
            return drift;
        }
        for (key, service) in &config.services {
            let current = service.generation();
            match self.services.get(key) {
                None => drift.added.push(key.clone()),
                // A binding whose removal was applied belongs to a service
                // that is configured again: it is applied afresh.
                Some(binding)
                    if binding.orphaned || binding.applied.as_deref() != Some(current.as_str()) =>
                {
                    drift.changed.push(key.clone());
                }
                Some(_) => {}
            }
        }
        drift.removed = self
            .services
            .iter()
            .filter(|(key, binding)| {
                binding.applied.is_some()
                    && !binding.orphaned
                    && !config.services.contains_key(key.as_str())
            })
            .map(|(key, _)| key.clone())
            .collect();
        drift
    }

    /// The services phase was skipped on request (`--no-db`).
    fn services_skipped_on_request(&self) -> bool {
        let services = self.phase(Phase::Services);
        services.state == PhaseState::Skipped && services.deliberate
    }

    /// Record that `.devflow/env` was just written for `config`. A workspace
    /// whose services were skipped on request has no service in its file, so
    /// the generation it records leaves them out.
    pub fn stamp_env_generation(&mut self, config: &Config) {
        self.env_generation = Some(if self.services_skipped_on_request() {
            env_generation_without_services(config)
        } else {
            config.env_generation()
        });
    }

    /// Reopen what a changed configuration invalidates: the `services` phase
    /// when a service was added or changed, and always `env` and `hooks`
    /// (hooks run at least once, as ever), each only if it is complete.
    /// Required skips, failures and unfinished phases already run again, and a
    /// skip someone asked for (`--no-db`, `--no-hooks`) is kept. The reopened
    /// phases are required skips, so the workspace is `needs_attention` until
    /// `devflow setup` or the next `switch` completes them. Returns whether
    /// anything changed; calling it again changes nothing.
    pub fn reopen(&mut self, config: &Config) -> bool {
        let drift = self.drift(config);
        if drift.is_empty() {
            return false;
        }
        let mut changed = false;
        if !drift.added.is_empty() || !drift.changed.is_empty() {
            changed |= self.reopen_phase(
                Phase::Services,
                format!(
                    "configuration changed since this workspace was set up ({}); \
                     setup has to run again",
                    drift.describe()
                ),
            );
        }
        changed |= self.reopen_phase(
            Phase::Env,
            format!(
                "configuration changed since .devflow/env was written ({}); \
                 setup has to run again",
                drift.describe()
            ),
        );
        changed |= self.reopen_phase(
            Phase::Hooks,
            "setup hooks run again after a configuration change".into(),
        );
        changed
    }

    /// Reopen what a branch that no longer exists invalidates: the `services`
    /// phase (a new branch has to be provisioned) and, with it, `env` and
    /// `hooks`, each only if it is complete. `lost` are the keys of the
    /// services whose bound branch is gone from the catalog (for example
    /// dropped because its volume was lost). A skip someone asked for is
    /// kept; calling it again changes nothing. Returns whether anything
    /// changed.
    pub fn reopen_lost_branches(&mut self, lost: &[String]) -> bool {
        if lost.is_empty() {
            return false;
        }
        let which = lost.join(", ");
        let mut changed = self.reopen_phase(
            Phase::Services,
            format!(
                "the branch bound for {which} no longer exists; setup has to run again to \
                 give the workspace a new one"
            ),
        );
        changed |= self.reopen_phase(
            Phase::Env,
            format!(
                ".devflow/env still points at the branch of {which}, which no longer exists; \
                 setup has to run again"
            ),
        );
        changed |= self.reopen_phase(
            Phase::Hooks,
            "setup hooks run again after the workspace got a new branch".into(),
        );
        changed
    }

    /// Record that the removal of every service `config` no longer has was
    /// applied: the environment has just been written without them. Their
    /// bindings (and provenance) stay in the receipt, marked
    /// [`ServiceBinding::orphaned`], so the removal stops counting as drift
    /// and the next read of a workspace that completed setup is `ready`
    /// again. Returns whether anything changed.
    pub fn acknowledge_removals(&mut self, config: &Config) -> bool {
        let mut changed = false;
        for (key, binding) in &mut self.services {
            if binding.applied.is_some()
                && !binding.orphaned
                && !config.services.contains_key(key.as_str())
            {
                binding.orphaned = true;
                changed = true;
            }
        }
        changed
    }

    fn reopen_phase(&mut self, phase: Phase, detail: String) -> bool {
        let complete = self.phase_is_complete(phase);
        if complete {
            self.skip_required(phase, detail);
        }
        complete
    }

    /// Done, or skipped without anyone having asked for it or needing it.
    fn phase_is_complete(&self, phase: Phase) -> bool {
        let record = self.phase(phase);
        match record.state {
            PhaseState::Done => true,
            PhaseState::Skipped => !record.required_skip && !record.deliberate,
            _ => false,
        }
    }

    /// [`Receipt::reopen_phase`] that leaves the record's timestamp alone: for
    /// a migration, which changes outcomes and nothing else.
    fn reopen_in_place(&mut self, phase: Phase, detail: &str) {
        if !self.phase_is_complete(phase) {
            return;
        }
        if let Some(record) = self.phases.get_mut(phase.as_str()) {
            record.state = PhaseState::Skipped;
            record.required_skip = true;
            record.detail = Some(detail.to_string());
        }
    }

    /// The first PostgreSQL binding, whatever its key and the configuration:
    /// for listings that have no configuration at hand. Anything that acts on
    /// the database uses [`Receipt::postgres`].
    pub fn first_postgres(&self) -> Option<&ServiceBinding> {
        self.services
            .values()
            .find(|binding| binding.engine == Engine::Postgres)
    }

    pub fn path(git_dir: &Path) -> PathBuf {
        git_dir.join("devflow").join("receipt.json")
    }

    /// Read the receipt as stored, whatever its version. A version 1 file
    /// comes back unmigrated (`version` 1): its `data` binding is in
    /// [`Receipt::unmapped`] and its `database` phase is already renamed, but
    /// nothing is bound to a service until [`Receipt::migrate`] runs with the
    /// configuration. Use [`Receipt::load_for`] when a configuration is at hand.
    pub fn load(git_dir: &Path) -> Result<Option<Receipt>> {
        let path = Self::path(git_dir);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                // The receipt holds database URLs: a malformed one is described
                // by position and key path, never by what it says.
                let mut receipt: Receipt = crate::diag::json_from_str(
                    &text,
                    &format!("invalid receipt {}", path.display()),
                )?;
                ensure!(
                    receipt.version <= VERSION,
                    "{} was written by a newer devflow",
                    path.display()
                );
                if receipt.version < VERSION {
                    receipt.rename_v1_phase();
                }
                Ok(Some(receipt))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    /// [`Receipt::load`], migrated in memory against `config` when the file
    /// is a version 1 receipt. Nothing is written.
    pub fn load_for(git_dir: &Path, config: &Config) -> Result<Option<Receipt>> {
        let mut receipt = Self::load(git_dir)?;
        if let Some(receipt) = receipt.as_mut() {
            receipt.migrate(config);
        }
        Ok(receipt)
    }

    /// Migrate the file in `git_dir` to the current version, under the
    /// receipt lock the caller holds, and return the result. A current
    /// receipt is returned untouched. There is no provisioning effect.
    pub fn migrate_in_place(
        git_dir: &Path,
        config: &Config,
        lock: &ReceiptLock,
    ) -> Result<Option<Receipt>> {
        ensure!(
            lock.git_dir == git_dir,
            "the receipt lock held is not the one of {}",
            git_dir.display()
        );
        let Some(mut receipt) = Self::load(git_dir)? else {
            return Ok(None);
        };
        if receipt.version < VERSION {
            receipt.migrate(config);
            receipt.save(git_dir)?;
        }
        Ok(Some(receipt))
    }

    /// Bring a version 1 receipt to the current version, in memory; a current
    /// receipt is left alone. Every field is kept:
    ///
    /// - `data` moves to `services.<key>` when `config` has a PostgreSQL
    ///   service, stamped with that service's current generation (the receipt
    ///   cannot say which configuration applied it, so the next change is the
    ///   first one noticed); otherwise it stays in [`Receipt::unmapped`];
    /// - the `database` phase becomes `services`; skips that were deliberate
    ///   (`--no-db`, `--no-hooks`, the default workspace's implicit setup)
    ///   are marked as such;
    /// - a completed env phase is reopened, with the hooks phase that follows
    ///   it, rather than stamped with the current env generation: the file on
    ///   disk was written by version 1, without `COMPOSE_PROJECT_NAME` or the
    ///   `[env]` recipes, and the next setup writes them. (When the binding
    ///   stayed `unmapped` the file also exported a database the current rules
    ///   no longer export; the next setup rewrites it without it.)
    pub fn migrate(&mut self, config: &Config) {
        if self.version >= VERSION {
            return;
        }
        self.rename_v1_phase();
        for record in self.phases.values_mut() {
            let skipped = record.state == PhaseState::Skipped && !record.required_skip;
            if skipped
                && matches!(
                    record.detail.as_deref(),
                    Some(SKIP_NO_DB | SKIP_NO_HOOKS | SKIP_DEFAULT_HOOKS)
                )
            {
                record.deliberate = true;
            }
        }
        if let Some(data) = self.unmapped.take() {
            match config.postgres() {
                Some((key, _)) => {
                    let applied = config.service_generation(key);
                    self.services
                        .insert(key.to_string(), ServiceBinding::postgres(data, applied));
                }
                None => self.unmapped = Some(data),
            }
        }
        if self.phase(Phase::Env).state == PhaseState::Done {
            // `.devflow/env` is the file version 1 wrote. No generation is
            // stamped on it: nothing was written under it, and the current
            // rules add `COMPOSE_PROJECT_NAME` and the `[env]` recipes (and
            // stop exporting a binding nothing maps). Reopening the phase
            // makes the next setup write the file and record the generation;
            // the hooks that follow it run again, as after any change to the
            // environment. Only the phases' outcomes change: timestamps and
            // every other field are kept, like in the rest of the migration.
            let env = if self.unmapped.is_some() {
                "this workspace's database is no longer exported (the configuration has no \
                 postgres service to take it); setup has to run again to rewrite .devflow/env"
            } else {
                "this workspace was set up by a previous devflow version; setup has to run \
                 again to rewrite .devflow/env"
            };
            self.reopen_in_place(Phase::Env, env);
            self.reopen_in_place(
                Phase::Hooks,
                "setup hooks run again after the environment is rewritten",
            );
        }
        self.version = VERSION;
    }

    /// Version 1 named the services phase `database`.
    fn rename_v1_phase(&mut self) {
        if let Some(record) = self.phases.remove("database") {
            self.phases.entry("services".to_string()).or_insert(record);
        }
    }

    /// Write the receipt (0600). Only a current-version receipt is written: a
    /// version 1 file read without a configuration must be migrated first.
    pub fn save(&self, git_dir: &Path) -> Result<()> {
        ensure!(
            self.version == VERSION,
            "refusing to write a version {} receipt; migrate it first",
            self.version
        );
        let text = serde_json::to_vec_pretty(self)?;
        crate::fsutil::write_atomic(&Self::path(git_dir), &text, true)
    }

    /// Exclusive lock for a setup run on this worktree. Returns `None` when
    /// another process holds it.
    pub fn try_lock(git_dir: &Path) -> Result<Option<ReceiptLock>> {
        let dir = git_dir.join("devflow");
        std::fs::create_dir_all(&dir)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("setup.lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(ReceiptLock {
                _file: file,
                git_dir: git_dir.to_path_buf(),
            })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
        }
    }

    /// The process lock of this worktree, or `None` when another operation on
    /// the workspace's daemons holds it. See the module documentation for the
    /// lock order.
    pub fn try_process_lock(git_dir: &Path) -> Result<Option<ProcessLock>> {
        let dir = git_dir.join("devflow");
        std::fs::create_dir_all(&dir)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("process.lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(ProcessLock {
                _file: file,
                git_dir: git_dir.to_path_buf(),
            })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
        }
    }

    /// [`Receipt::try_process_lock`], waited for up to `wait`: an operation
    /// that is just finishing serialises with the next one, a long one (a
    /// start waiting for pitchfork) makes it give up.
    pub fn process_lock_waiting(
        git_dir: &Path,
        wait: std::time::Duration,
    ) -> Result<Option<ProcessLock>> {
        let step = std::time::Duration::from_millis(25);
        let deadline = std::time::Instant::now() + wait;
        loop {
            if let Some(lock) = Self::try_process_lock(git_dir)? {
                return Ok(Some(lock));
            }
            if std::time::Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(step);
        }
    }

    /// [`Receipt::try_lock`], retried briefly: a lock is released when the last
    /// duplicate of its file descriptor closes, and a process spawned by another
    /// thread can hold a duplicate for a few milliseconds.
    pub fn lock_retrying(git_dir: &Path) -> Result<Option<ReceiptLock>> {
        for attempt in 0..10 {
            if let Some(lock) = Self::try_lock(git_dir)? {
                return Ok(Some(lock));
            }
            if attempt < 9 {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
        Ok(None)
    }

    /// Change [`Receipt::processes`] in the receipt stored in `git_dir`, under
    /// the receipt lock (taken briefly; a setup run holding it is an error the
    /// caller reports, [`crate::error::Busy`] for `workspace`), by a caller that
    /// holds the workspace's process lock. A version 1
    /// file is migrated first, as every writer does. Nothing else changes, not
    /// even `updated_at`. `Ok(false)` when there is no receipt.
    pub fn update_processes(
        git_dir: &Path,
        workspace: &str,
        config: &Config,
        process_lock: &ProcessLock,
        change: impl FnOnce(&mut Option<ProcessRecord>),
    ) -> Result<bool> {
        ensure!(
            process_lock.git_dir == git_dir,
            "the process lock held is not the one of {}",
            git_dir.display()
        );
        let lock = Self::lock_retrying(git_dir)?
            .ok_or_else(|| anyhow::Error::new(crate::error::Busy::setup(workspace)))?;
        let Some(mut receipt) = Self::migrate_in_place(git_dir, config, &lock)? else {
            return Ok(false);
        };
        change(&mut receipt.processes);
        receipt.save(git_dir)?;
        Ok(true)
    }

    /// Whether a setup run currently holds the lock (for status display).
    /// Retried like [`Receipt::lock_retrying`]: a process another thread is
    /// spawning can hold a duplicate of a just-released lock's descriptor for
    /// a few milliseconds, which would show a finished workspace as busy.
    pub fn is_locked(git_dir: &Path) -> bool {
        matches!(Self::lock_retrying(git_dir), Ok(None))
    }

    pub fn logs_dir(git_dir: &Path) -> PathBuf {
        git_dir.join("devflow").join("logs")
    }
}

/// Held for the duration of a setup run; released on drop.
pub struct ReceiptLock {
    _file: std::fs::File,
    git_dir: PathBuf,
}

/// Held for the duration of one operation on a workspace's pitchfork daemons
/// (start, stop, restart, removal, replacing the instance identity); released
/// on drop. It is the proof that nobody else is starting, stopping or
/// replacing the namespace the record in [`Receipt::processes`] is about.
pub struct ProcessLock {
    _file: std::fs::File,
    git_dir: PathBuf,
}

impl ProcessLock {
    /// The Git directory of the worktree it is for.
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }
}

/// `DEVFLOW_INSTANCE_ID` lets an orchestrator give a forked VM a known
/// identity. Only [`crate::setup::setup`] (the explicit `devflow setup`) reads
/// it; the generated `.devflow/env` exports the informational
/// `DEVFLOW_WORKSPACE_ID` instead, so sourcing it cannot set this.
pub(crate) fn instance_id_override() -> Option<uuid::Uuid> {
    std::env::var("DEVFLOW_INSTANCE_ID")
        .ok()
        .and_then(|v| v.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_postgres(key: &str) -> Config {
        let mut config = Config::default();
        config.set_postgres(key, Default::default());
        config
    }

    fn binding(branch: Option<uuid::Uuid>) -> ServiceBinding {
        ServiceBinding {
            engine: Engine::Postgres,
            branch_id: branch,
            database: "df_a_b".into(),
            role: "dfl_b".into(),
            url: "postgres://dfl_b:pw@127.0.0.1:20001/df_a_b".into(), // kingfisher:ignore
            endpoint: None,
            cloned: None,
            source: "empty".into(),
            owned: branch.is_some(),
            applied: None,
            orphaned: false,
            extras: BTreeMap::new(),
        }
    }

    #[test]
    fn a_phase_remembers_when_its_attempt_began_and_how_long_it_took() {
        let mut receipt = Receipt::new(uuid::Uuid::new_v4(), "feature", None);
        receipt.set_phase(Phase::Payload, PhaseState::Started, None);
        let started = receipt.phase(Phase::Payload);
        assert_eq!(started.started_at, None);
        assert_eq!(started.duration(), None, "still running");
        std::thread::sleep(std::time::Duration::from_millis(15));
        receipt.set_phase(Phase::Payload, PhaseState::Done, None);
        let done = receipt.phase(Phase::Payload);
        assert_eq!(done.started_at, started.updated_at);
        assert!(done.duration().is_some_and(|d| d.as_millis() >= 10));
        // A phase recorded without a start has no duration, and a skip that
        // follows a start has one like any other end.
        receipt.set_phase(Phase::Env, PhaseState::Done, None);
        assert_eq!(receipt.phase(Phase::Env).duration(), None);
        receipt.set_phase(Phase::Hooks, PhaseState::Started, None);
        receipt.skip_required(Phase::Hooks, "untrusted".into());
        assert!(receipt.phase(Phase::Hooks).duration().is_some());
        // And it survives the file.
        let json = serde_json::to_string(&receipt).unwrap();
        let back: Receipt = serde_json::from_str(&json).unwrap();
        assert_eq!(back.phase(Phase::Payload), done);
        // A receipt written before durations were kept still reads.
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value["phases"]["payload"]
            .as_object_mut()
            .unwrap()
            .remove("started_at");
        let old: Receipt = serde_json::from_value(value).unwrap();
        assert_eq!(old.phase(Phase::Payload).duration(), None);
    }

    #[test]
    fn the_payload_record_keeps_how_a_path_was_written_across_retries() {
        use crate::cow::{EntryReport, EntryResult, Method};
        let entry = |path: &str, result| EntryReport {
            path: path.into(),
            result,
        };
        let mut receipt = Receipt::new(uuid::Uuid::new_v4(), "feature", None);
        receipt.record_payload(&[
            entry(
                ".env",
                EntryResult::Done {
                    method: Method::Copied,
                },
            ),
            entry(
                "node_modules",
                EntryResult::Done {
                    method: Method::Cloned,
                },
            ),
            entry("dist", EntryResult::Missing),
        ]);
        // A retry finds both there; what the first attempt did is kept.
        receipt.record_payload(&[
            entry(".env", EntryResult::AlreadyPresent),
            entry("node_modules", EntryResult::AlreadyPresent),
            entry("dist", EntryResult::AlreadyPresent),
        ]);
        let outcome = |path: &str| {
            receipt
                .payload
                .iter()
                .find(|p| p.path == path)
                .map(|p| p.outcome)
        };
        assert_eq!(outcome(".env"), Some(PayloadOutcome::Copied));
        assert_eq!(outcome("node_modules"), Some(PayloadOutcome::Cloned));
        // Nothing was written for a path that was only ever missing.
        assert_eq!(outcome("dist"), Some(PayloadOutcome::AlreadyPresent));
        let json = serde_json::to_string(&receipt).unwrap();
        assert!(json.contains("\"outcome\":\"cloned\""), "{json}");
        let empty = Receipt::new(uuid::Uuid::new_v4(), "x", None);
        assert!(!serde_json::to_string(&empty).unwrap().contains("payload"));
    }

    #[test]
    fn container_bindings_carry_no_database_fields_and_postgres_files_still_read() {
        let id = uuid::Uuid::new_v4();
        let binding = ServiceBinding::container(
            Engine::Valkey,
            id,
            "redis://:pw@127.0.0.1:23456/0".into(), // kingfisher:ignore
            Some("abcd1234.cache.shop-0b3c9d2e.devflow.orb.local:6379".into()),
            "workspace:main".into(),
            CloneReport {
                method: Some(crate::services::catalog::CloneMethod::Reflink),
                captured_at: None,
                allocated_bytes: Some(32768),
                source_downtime_ms: None,
            },
            Some("gen".into()),
        );
        let json = serde_json::to_value(&binding).unwrap();
        assert_eq!(json["engine"], "valkey");
        assert_eq!(json["branch_id"], id.to_string());
        assert_eq!(json["cloned"]["method"], "reflink");
        assert!(json.get("database").is_none() && json.get("role").is_none());
        assert!(json["owned"].as_bool().unwrap());
        let back: ServiceBinding = serde_json::from_value(json).unwrap();
        assert_eq!(back, binding);
        // A PostgreSQL binding written before the container fields existed
        // reads as it did, with none of them.
        let old = serde_json::json!({
            "engine": "postgres",
            "branch_id": id,
            "database": "df_a_b",
            "role": "dfl_b",
            "url": "postgres://dfl_b:pw@127.0.0.1:20001/df_a_b", // kingfisher:ignore
            "source": "empty",
            "owned": true,
        });
        let read: ServiceBinding = serde_json::from_value(old).unwrap();
        assert_eq!(
            (read.database.as_str(), read.role.as_str()),
            ("df_a_b", "dfl_b")
        );
        assert!(read.endpoint.is_none() && read.cloned.is_none());
        // And writes back without the new fields.
        let written = serde_json::to_value(&read).unwrap();
        assert!(written.get("endpoint").is_none() && written.get("cloned").is_none());
    }

    #[test]
    fn service_sources_are_kept_per_service_and_absent_in_old_receipts() {
        let mut receipt = Receipt::new(uuid::Uuid::new_v4(), "w", None);
        assert!(
            serde_json::to_value(&receipt)
                .unwrap()
                .get("service_sources")
                .is_none()
        );
        receipt
            .service_sources
            .insert("cache".into(), "snapshot:seed@v2".into());
        let json = serde_json::to_string(&receipt).unwrap();
        let back: Receipt = serde_json::from_str(&json).unwrap();
        assert_eq!(back.service_sources["cache"], "snapshot:seed@v2");
        // The identity a label resolved to is kept beside it, and is absent
        // from receipts written before it existed.
        assert!(
            serde_json::to_value(&receipt)
                .unwrap()
                .get("service_source_ids")
                .is_none()
        );
        receipt.service_source_ids.insert(
            "cache".into(),
            "snapshot:00000000-0000-4000-8000-000000000001".into(),
        );
        let json = serde_json::to_string(&receipt).unwrap();
        let back: Receipt = serde_json::from_str(&json).unwrap();
        assert_eq!(back.service_source_ids, receipt.service_source_ids);
    }

    #[test]
    fn extras_are_recorded_without_credentials_and_follow_the_binding_url() {
        let mut binding = binding(Some(uuid::Uuid::new_v4()));
        assert_eq!(binding.extra_url("test"), None);
        binding.extras.insert(
            "test".into(),
            ExtraBinding {
                database: "df_a_b_1".into(),
            },
        );
        assert_eq!(
            binding.extra_url("test").as_deref(),
            Some("postgres://dfl_b:pw@127.0.0.1:20001/df_a_b_1") // kingfisher:ignore
        );
        assert_eq!(binding.extra_url("shadow"), None);
        // A rotated password and a moved endpoint reach the extras with the URL.
        binding.url = "postgres://dfl_b:new@127.0.0.1:20002/df_a_b".into(); // kingfisher:ignore
        assert_eq!(
            binding.extra_url("test").as_deref(),
            Some("postgres://dfl_b:new@127.0.0.1:20002/df_a_b_1") // kingfisher:ignore
        );
        // The file carries names and databases, never a second copy of a secret.
        let json = serde_json::to_string(&binding).unwrap();
        assert!(
            json.contains(r#""extras":{"test":{"database":"df_a_b_1"}}"#),
            "{json}"
        );
        let read: ServiceBinding = serde_json::from_str(&json).unwrap();
        assert_eq!(read, binding);
        // Bindings without extras are written as before and read back.
        let plain = self::binding(None);
        let json = serde_json::to_string(&plain).unwrap();
        assert!(!json.contains("extras"), "{json}");
        assert_eq!(
            serde_json::from_str::<ServiceBinding>(&json).unwrap(),
            plain
        );
    }

    #[test]
    fn the_slug_names_project_and_instance_by_their_first_eight_hex_characters() {
        let project: uuid::Uuid = "0b3c9d2e-1111-4222-8333-444455556666".parse().unwrap();
        let mut receipt = Receipt::new(project, "w", None);
        receipt.instance_id = "a1b2c3d4-e5f6-4789-8abc-def012345678".parse().unwrap();
        assert_eq!(receipt.slug(), "df-0b3c9d2e-a1b2c3d4");
        // A fork has another instance id, hence another name.
        receipt.instance_id = uuid::Uuid::new_v4();
        assert_ne!(receipt.slug(), "df-0b3c9d2e-a1b2c3d4");
        assert!(receipt.slug().starts_with("df-0b3c9d2e-"));
    }

    #[test]
    fn status_follows_phase_states() {
        let mut receipt = Receipt::new(uuid::Uuid::new_v4(), "feature", None);
        assert_eq!(receipt.status(), Status::InProgress);
        for phase in [Phase::Materialize, Phase::Payload, Phase::Env] {
            receipt.set_phase(phase, PhaseState::Done, None);
        }
        receipt.set_phase(
            Phase::Services,
            PhaseState::Skipped,
            Some("no postgres service".into()),
        );
        receipt.skip_required(Phase::Hooks, "hooks are not trusted".into());
        assert_eq!(receipt.status(), Status::NeedsAttention);
        receipt.set_phase(Phase::Hooks, PhaseState::Done, None);
        assert_eq!(receipt.status(), Status::Ready);
    }

    #[test]
    fn saves_privately_and_locks_exclusively() {
        let dir = tempfile::tempdir().unwrap();
        let receipt = Receipt::new(uuid::Uuid::new_v4(), "feature", Some("main".into()));
        receipt.save(dir.path()).unwrap();
        assert_eq!(Receipt::load(dir.path()).unwrap(), Some(receipt));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(Receipt::path(dir.path()))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let lock = Receipt::try_lock(dir.path()).unwrap();
        assert!(lock.is_some());
        assert!(Receipt::try_lock(dir.path()).unwrap().is_none());
        drop(lock);
        assert!(Receipt::try_lock(dir.path()).unwrap().is_some());
    }

    #[test]
    fn the_services_phase_serializes_as_services_and_reads_database() {
        assert_eq!(
            serde_json::to_string(&Phase::Services).unwrap(),
            "\"services\""
        );
        let old: Phase = serde_json::from_str("\"database\"").unwrap();
        assert_eq!(old, Phase::Services);
        assert_eq!(Phase::Services.as_str(), "services");
    }

    /// A version 1 receipt file with a binding and the old phase name.
    fn write_v1(dir: &Path, with_data: bool) -> Receipt {
        let mut receipt = Receipt::new(uuid::Uuid::new_v4(), "feature", Some("main".into()));
        receipt.set_phase(Phase::Materialize, PhaseState::Done, None);
        receipt.set_phase(Phase::Services, PhaseState::Done, Some("branch".into()));
        receipt.set_phase(Phase::Env, PhaseState::Done, None);
        let mut json = serde_json::to_value(&receipt).unwrap();
        let object = json.as_object_mut().unwrap();
        object.insert("version".into(), 1.into());
        let phases = object["phases"].as_object_mut().unwrap();
        let record = phases.remove("services").unwrap();
        phases.insert("database".into(), record);
        if with_data {
            let data = binding(Some(uuid::Uuid::new_v4()));
            object.insert(
                "data".into(),
                serde_json::json!({
                    "branch_id": data.branch_id, "database": data.database,
                    "role": data.role, "url": data.url, "source": data.source,
                    "owned": true,
                }),
            );
        }
        std::fs::create_dir_all(dir.join("devflow")).unwrap();
        std::fs::write(Receipt::path(dir), serde_json::to_vec(&json).unwrap()).unwrap();
        receipt
    }

    #[test]
    fn a_malformed_receipt_is_described_without_its_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = Receipt::path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for (text, wanted) in [
            (
                r#"{"version": "REVIEWSECRET"}"#,
                "version: wrong type of value at line 1",
            ),
            (r#"{"version": 2, "x": "REVIEWSECRET"#, "ends unexpectedly"),
            ("REVIEWSECRET", "not valid JSON at line 1"),
            (
                r#"{"version": 2} REVIEWSECRET"#,
                "instance_id: required key is missing",
            ),
        ] {
            std::fs::write(&path, text).unwrap();
            let error = Receipt::load(dir.path()).unwrap_err();
            let shown = format!("{error} | {error:#} | {error:?}");
            assert!(shown.contains(wanted), "{text}\n{shown}");
            assert!(shown.contains("invalid receipt"), "{shown}");
            assert!(!shown.contains("REVIEWSECRET"), "{text}\n{shown}");
        }
    }

    #[test]
    fn a_version_one_file_loads_unmigrated_and_is_never_saved_back_as_is() {
        let dir = tempfile::tempdir().unwrap();
        write_v1(dir.path(), true);
        let loaded = Receipt::load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.version, 1);
        assert!(
            loaded.unmapped.is_some(),
            "without a config, data is unmapped"
        );
        assert!(loaded.services.is_empty());
        assert_eq!(loaded.phase(Phase::Services).state, PhaseState::Done);
        assert!(loaded.save(dir.path()).is_err(), "must be migrated first");
    }

    #[test]
    fn migration_maps_data_to_the_sole_postgres_service_or_keeps_it_unmapped() {
        let dir = tempfile::tempdir().unwrap();
        write_v1(dir.path(), true);
        let config = config_with_postgres("main-db");
        let mapped = Receipt::load_for(dir.path(), &config).unwrap().unwrap();
        assert_eq!(mapped.version, VERSION);
        assert!(mapped.unmapped.is_none());
        let service = &mapped.services["main-db"];
        assert_eq!(service.engine, Engine::Postgres);
        assert!(service.owned && service.branch_id.is_some());
        assert_eq!(service.applied, config.service_generation("main-db"));
        // The file on disk was written by version 1: the generation is not
        // stamped on content that was never written; the phase is reopened so
        // the next setup writes (and stamps) it.
        assert_eq!(mapped.env_generation, None);
        let env = mapped.phase(Phase::Env);
        assert!(
            env.state == PhaseState::Skipped && env.required_skip,
            "{env:?}"
        );
        assert!(env.detail.unwrap().contains("rewrite .devflow/env"));
        assert_eq!(mapped.status(), Status::NeedsAttention);
        // The file is untouched by an in-memory migration.
        assert_eq!(Receipt::load(dir.path()).unwrap().unwrap().version, 1);

        // No postgres service: the binding is kept, not guessed.
        let none = Receipt::load_for(dir.path(), &Config::default())
            .unwrap()
            .unwrap();
        assert!(none.services.is_empty());
        assert_eq!(
            none.unmapped.as_ref().map(|d| d.database.as_str()),
            Some("df_a_b")
        );
        assert!(none.postgres(&Config::default()).is_none());
        // The environment that version 1 wrote exports that binding, which is
        // no longer exported: it is rewritten by the next setup, and until
        // then the workspace is not ready (the generation is not stamped).
        let env = none.phase(Phase::Env);
        assert!(
            env.state == PhaseState::Skipped && env.required_skip,
            "{env:?}"
        );
        assert!(env.detail.unwrap().contains("rewrite .devflow/env"));
        assert_eq!(none.env_generation, None);
        assert_eq!(none.status(), Status::NeedsAttention);
    }

    #[test]
    fn migrating_in_place_needs_the_lock_and_writes_the_current_version() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        write_v1(dir.path(), true);
        let config = config_with_postgres("db");
        let wrong = Receipt::try_lock(other.path()).unwrap().unwrap();
        assert!(Receipt::migrate_in_place(dir.path(), &config, &wrong).is_err());
        let lock = Receipt::try_lock(dir.path()).unwrap().unwrap();
        let migrated = Receipt::migrate_in_place(dir.path(), &config, &lock)
            .unwrap()
            .unwrap();
        assert_eq!(Receipt::load(dir.path()).unwrap().unwrap(), migrated);
        assert_eq!(migrated.version, VERSION);
        // Idempotent.
        let again = Receipt::migrate_in_place(dir.path(), &config, &lock)
            .unwrap()
            .unwrap();
        assert_eq!(again, migrated);
        let empty = tempfile::tempdir().unwrap();
        let lock = Receipt::try_lock(empty.path()).unwrap().unwrap();
        assert!(
            Receipt::migrate_in_place(empty.path(), &config, &lock)
                .unwrap()
                .is_none()
        );
    }

    fn finished(config: &Config) -> Receipt {
        let mut receipt = Receipt::new(uuid::Uuid::new_v4(), "w", None);
        for phase in Phase::ALL {
            receipt.set_phase(phase, PhaseState::Done, None);
        }
        for (key, service) in &config.services {
            let mut service_binding = binding(Some(uuid::Uuid::new_v4()));
            service_binding.applied = Some(service.generation());
            receipt.services.insert(key.clone(), service_binding);
        }
        receipt.env_generation = Some(config.env_generation());
        receipt
    }

    /// The reasons a phase is reopened for are read in a frontend: words, no
    /// command line (the remedy that fixes it travels as data).
    #[test]
    fn the_reasons_for_reopening_a_phase_say_nothing_of_the_cli() {
        let say_nothing_of_the_cli = |receipt: &Receipt| {
            for phase in Phase::ALL {
                if let Some(detail) = receipt.phase(phase).detail {
                    assert!(
                        !detail.contains("`devflow") && !detail.contains("run devflow"),
                        "{phase:?}: {detail}"
                    );
                }
            }
        };
        let after = config_with_postgres("db");
        let mut changed = finished(&Config::default());
        assert!(changed.reopen(&after));
        say_nothing_of_the_cli(&changed);
        let reopened = changed.phase(Phase::Services).detail.unwrap();
        assert!(reopened.contains("setup has to run again"), "{reopened}");

        let dir = tempfile::tempdir().unwrap();
        write_v1(dir.path(), true);
        for config in [config_with_postgres("main-db"), Config::default()] {
            let migrated = Receipt::load_for(dir.path(), &config).unwrap().unwrap();
            say_nothing_of_the_cli(&migrated);
        }
    }

    #[test]
    fn a_skip_someone_asked_for_is_worded_for_a_person() {
        assert_eq!(plain_detail(SKIP_NO_DB), "no database was asked for");
        assert_eq!(plain_detail(SKIP_NO_HOOKS), "no hooks were asked for");
        assert_eq!(plain_detail("created /tmp/x"), "created /tmp/x");
    }

    #[test]
    fn an_unchanged_configuration_reopens_nothing() {
        let config = config_with_postgres("db");
        let mut receipt = finished(&config);
        assert_eq!(receipt.drift(&config), Drift::default());
        assert!(!receipt.reopen(&config));
        assert_eq!(receipt.status(), Status::Ready);
    }

    #[test]
    fn an_added_service_reopens_services_env_and_hooks_once() {
        let before = Config::default();
        let mut receipt = finished(&before);
        receipt.set_phase(
            Phase::Services,
            PhaseState::Skipped,
            Some("no services configured".into()),
        );
        let after = config_with_postgres("db");
        assert_eq!(receipt.drift(&after).added, ["db"]);
        assert!(receipt.reopen(&after));
        for phase in [Phase::Services, Phase::Env, Phase::Hooks] {
            let record = receipt.phase(phase);
            assert!(
                record.state == PhaseState::Skipped && record.required_skip,
                "{phase:?}: {record:?}"
            );
        }
        assert!(
            receipt
                .phase(Phase::Services)
                .detail
                .unwrap()
                .contains("service 'db' added")
        );
        assert_eq!(receipt.phase(Phase::Payload).state, PhaseState::Done);
        assert_eq!(receipt.status(), Status::NeedsAttention);
        // Idempotent: the reopened phases are not reopened again.
        let once = receipt.clone();
        assert!(!receipt.reopen(&after));
        assert_eq!(receipt, once);
    }

    #[test]
    fn a_changed_section_is_noticed_through_its_generation() {
        let config = config_with_postgres("db");
        let mut receipt = finished(&config);
        let mut other = config.clone();
        other.postgres_mut().unwrap().1.require_cow = true;
        let drift = receipt.drift(&other);
        assert_eq!(drift.changed, ["db"]);
        assert!(drift.added.is_empty() && drift.env);
        assert!(receipt.reopen(&other));
        assert!(
            receipt
                .phase(Phase::Services)
                .detail
                .unwrap()
                .contains("service 'db' changed")
        );
        // A binding that never recorded a generation (an attachment made
        // before the service existed) is behind a service added since.
        let mut attached = finished(&config);
        attached.services.get_mut("db").unwrap().applied = None;
        assert_eq!(attached.drift(&config).changed, ["db"]);
    }

    #[test]
    fn a_removed_service_keeps_its_binding_and_reopens_only_env_and_hooks() {
        let config = config_with_postgres("db");
        let mut receipt = finished(&config);
        let after = Config::default();
        let drift = receipt.drift(&after);
        assert_eq!(drift.removed, ["db"]);
        assert!(receipt.reopen(&after));
        assert_eq!(receipt.phase(Phase::Services).state, PhaseState::Done);
        assert!(receipt.phase(Phase::Env).required_skip);
        assert!(receipt.phase(Phase::Hooks).required_skip);
        assert!(receipt.services.contains_key("db"), "nothing is deleted");
        // Setup rewrites the environment and reruns the hooks, then records
        // that the removal was applied: no drift is left, and the binding and
        // its provenance are still there.
        let provenance = receipt.services["db"].clone();
        receipt.set_phase(Phase::Env, PhaseState::Done, None);
        receipt.set_phase(Phase::Hooks, PhaseState::Done, None);
        receipt.env_generation = Some(after.env_generation());
        assert!(receipt.acknowledge_removals(&after));
        assert!(!receipt.acknowledge_removals(&after), "idempotent");
        assert_eq!(receipt.drift(&after), Drift::default());
        assert!(!receipt.reopen(&after));
        assert_eq!(receipt.status(), Status::Ready);
        let kept = &receipt.services["db"];
        assert!(kept.orphaned);
        assert_eq!(
            (
                kept.branch_id,
                &kept.database,
                &kept.role,
                &kept.url,
                &kept.source,
                kept.owned
            ),
            (
                provenance.branch_id,
                &provenance.database,
                &provenance.role,
                &provenance.url,
                &provenance.source,
                provenance.owned
            )
        );
        // The orphan is not exported, and a later removal of another service
        // is still noticed.
        assert!(receipt.postgres(&after).is_none());
        // The service coming back (same section) is applied afresh.
        assert_eq!(receipt.drift(&config).changed, ["db"]);
        // A URL-only attachment made with no service was never configured.
        let mut attached = Receipt::new(uuid::Uuid::new_v4(), "w", None);
        attached.services.insert("db".into(), binding(None));
        assert!(attached.drift(&after).removed.is_empty());
    }

    #[test]
    fn deliberate_skips_and_unfinished_phases_are_left_alone() {
        let config = config_with_postgres("db");
        // `--no-db`: nothing derived from services applies at all.
        let mut no_db = Receipt::new(uuid::Uuid::new_v4(), "w", None);
        for phase in [Phase::Materialize, Phase::Payload, Phase::Env, Phase::Hooks] {
            no_db.set_phase(phase, PhaseState::Done, None);
        }
        no_db.skip_deliberate(Phase::Services, "--no-db".into());
        no_db.stamp_env_generation(&Config::default());
        assert_eq!(no_db.drift(&config), Drift::default());
        assert!(!no_db.reopen(&config));
        assert_eq!(no_db.status(), Status::Ready);
        // `--no-hooks`: services and env reopen, the hooks skip stays.
        let mut no_hooks = finished(&Config::default());
        no_hooks.skip_deliberate(Phase::Hooks, "--no-hooks".into());
        assert!(no_hooks.reopen(&config));
        assert!(no_hooks.phase(Phase::Env).required_skip);
        let hooks = no_hooks.phase(Phase::Hooks);
        assert!(hooks.deliberate && !hooks.required_skip);
        // Phases that did not finish, and a receipt that never wrote its
        // environment, are not touched: they run anyway.
        let mut partial = Receipt::new(uuid::Uuid::new_v4(), "w", None);
        partial.set_phase(Phase::Services, PhaseState::Failed, Some("boom".into()));
        assert!(!partial.reopen(&config));
        assert_eq!(
            partial.phase(Phase::Services).detail.as_deref(),
            Some("boom")
        );
        assert!(!partial.drift(&config).env);
    }

    #[test]
    fn a_workspace_without_services_still_picks_up_environment_changes() {
        let plain = Config::default();
        let mut with_env = Config::default();
        with_env
            .env
            .vars
            .insert("API_URL".into(), "http://x".into());
        let mut no_db = Receipt::new(uuid::Uuid::new_v4(), "w", None);
        for phase in [Phase::Materialize, Phase::Payload, Phase::Env, Phase::Hooks] {
            no_db.set_phase(phase, PhaseState::Done, None);
        }
        no_db.skip_deliberate(Phase::Services, "--no-db".into());
        no_db.stamp_env_generation(&plain);

        // An approved `[env]` change: the services skip is kept, the environment
        // and the hooks that follow it reopen.
        let drift = no_db.drift(&with_env);
        assert!(drift.env && drift.added.is_empty() && drift.changed.is_empty());
        assert!(no_db.reopen(&with_env));
        assert!(no_db.phase(Phase::Env).required_skip);
        assert!(no_db.phase(Phase::Hooks).required_skip);
        let services = no_db.phase(Phase::Services);
        assert!(
            services.deliberate && !services.required_skip,
            "{services:?}"
        );
        assert_eq!(no_db.status(), Status::NeedsAttention);
        // Writing the file again stamps the new generation: ready, no drift.
        no_db.set_phase(Phase::Env, PhaseState::Done, None);
        no_db.set_phase(Phase::Hooks, PhaseState::Done, None);
        no_db.stamp_env_generation(&with_env);
        assert_eq!(no_db.drift(&with_env), Drift::default());
        assert_eq!(no_db.status(), Status::Ready);

        // A service coming or changing is not a change to a file with no
        // service in it.
        let mut serviced = with_env.clone();
        serviced.set_postgres("db", Default::default());
        assert_eq!(no_db.drift(&serviced), Drift::default());
        assert!(!no_db.reopen(&serviced));
        assert_eq!(no_db.status(), Status::Ready);

        // A receipt written before this stamp (the full generation) still
        // compares equal while the configuration is the same.
        no_db.env_generation = Some(serviced.env_generation());
        assert_eq!(no_db.drift(&serviced), Drift::default());
    }

    #[test]
    fn a_migrated_ready_workspace_reopens_env_and_hooks_instead_of_stamping_them() {
        let dir = tempfile::tempdir().unwrap();
        let mut v1 = write_v1(dir.path(), true);
        v1.set_phase(Phase::Hooks, PhaseState::Done, None);
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(Receipt::path(dir.path())).unwrap()).unwrap();
        json["phases"]["hooks"] = serde_json::to_value(v1.phase(Phase::Hooks)).unwrap();
        std::fs::write(
            Receipt::path(dir.path()),
            serde_json::to_vec(&json).unwrap(),
        )
        .unwrap();
        let config = config_with_postgres("db");
        let lock = Receipt::try_lock(dir.path()).unwrap().unwrap();
        let mut migrated = Receipt::migrate_in_place(dir.path(), &config, &lock)
            .unwrap()
            .unwrap();
        assert_eq!(
            migrated.env_generation, None,
            "nothing was written under it"
        );
        assert!(migrated.phase(Phase::Env).required_skip);
        assert!(migrated.phase(Phase::Hooks).required_skip);
        assert_eq!(migrated.status(), Status::NeedsAttention);
        // Reopening again changes nothing, and the next setup completes it.
        assert!(!migrated.reopen(&config));
        migrated.set_phase(Phase::Payload, PhaseState::Done, None);
        migrated.set_phase(Phase::Env, PhaseState::Done, None);
        migrated.stamp_env_generation(&config);
        migrated.set_phase(Phase::Hooks, PhaseState::Done, None);
        assert_eq!(migrated.status(), Status::Ready);
        assert_eq!(migrated.drift(&config), Drift::default());
        // A deliberate `--no-hooks` skip survives the migration.
        let other = tempfile::tempdir().unwrap();
        write_v1(other.path(), true);
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(Receipt::path(other.path())).unwrap()).unwrap();
        json["phases"]["hooks"] = serde_json::json!({"state": "skipped", "detail": SKIP_NO_HOOKS});
        std::fs::write(
            Receipt::path(other.path()),
            serde_json::to_vec(&json).unwrap(),
        )
        .unwrap();
        let kept = Receipt::load_for(other.path(), &config).unwrap().unwrap();
        let hooks = kept.phase(Phase::Hooks);
        assert!(hooks.deliberate && !hooks.required_skip, "{hooks:?}");
        assert!(kept.phase(Phase::Env).required_skip);
    }

    #[test]
    fn postgres_lookup_is_by_key_and_never_guesses() {
        let branch = Some(uuid::Uuid::new_v4());
        let mut receipt = Receipt::new(uuid::Uuid::new_v4(), "w", None);
        receipt.services.insert("db".into(), binding(branch));
        let config = config_with_postgres("db");
        assert_eq!(receipt.postgres(&config).unwrap().0, "db");
        // Another key: the old binding is not the new service's.
        assert!(receipt.postgres(&config_with_postgres("main")).is_none());
        // No service in the configuration: a bound branch is not offered...
        assert!(receipt.postgres(&Config::default()).is_none());
        // ...but a lone URL-only attachment made with no service configured is.
        let mut attached = Receipt::new(uuid::Uuid::new_v4(), "w", None);
        attached.services.insert("db".into(), binding(None));
        assert_eq!(attached.postgres(&Config::default()).unwrap().0, "db");
        attached.services.insert("other".into(), binding(None));
        assert!(attached.postgres(&Config::default()).is_none(), "ambiguous");
    }

    #[test]
    fn a_url_attached_under_a_configured_service_is_not_exported_after_its_removal() {
        // Attached while `services.db` existed: the binding records the
        // generation it was applied under.
        let config = config_with_postgres("db");
        let mut receipt = Receipt::new(uuid::Uuid::new_v4(), "w", None);
        let mut attached = binding(None);
        attached.applied = config.service_generation("db");
        receipt.services.insert("db".into(), attached);
        assert_eq!(receipt.postgres(&config).unwrap().0, "db");
        // The service is removed: the fallback must not offer the old
        // attachment, or `.devflow/env` would keep exporting it.
        assert!(receipt.postgres(&Config::default()).is_none());
        assert_eq!(receipt.drift(&Config::default()).removed, ["db"]);
    }
    #[test]
    fn the_daemons_devflow_started_are_recorded_under_the_lock_and_nothing_else_changes() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path();
        let config = Config::default();
        let process = Receipt::try_process_lock(git_dir).unwrap().unwrap();
        // The process lock is exclusive and independent of the receipt lock.
        assert!(Receipt::try_process_lock(git_dir).unwrap().is_none());
        assert!(
            Receipt::process_lock_waiting(git_dir, std::time::Duration::from_millis(60))
                .unwrap()
                .is_none()
        );
        // No receipt: nothing to record in.
        assert!(!Receipt::update_processes(git_dir, "w", &config, &process, |_| {}).unwrap());
        let receipt = Receipt::new(uuid::Uuid::new_v4(), "w", None);
        receipt.save(git_dir).unwrap();
        // A receipt without the field (every older one) reads as none, and
        // does not write one.
        assert!(
            !std::fs::read_to_string(Receipt::path(git_dir))
                .unwrap()
                .contains("processes")
        );
        let record = ProcessRecord {
            started_at: Utc::now(),
            ids: vec!["df-a-b/web".into()],
        };
        let wanted = record.clone();
        assert!(
            Receipt::update_processes(git_dir, "w", &config, &process, |slot| *slot = Some(wanted))
                .unwrap()
        );
        let loaded = Receipt::load(git_dir).unwrap().unwrap();
        assert_eq!(loaded.processes, Some(record));
        assert_eq!(
            Receipt {
                processes: None,
                ..loaded
            },
            receipt,
            "only the record changed, not even updated_at"
        );
        // Cleared again.
        assert!(
            Receipt::update_processes(git_dir, "w", &config, &process, |slot| *slot = None)
                .unwrap()
        );
        assert_eq!(Receipt::load(git_dir).unwrap().unwrap().processes, None);
        // A setup run holding the lock is an error, not a lost update.
        let held = Receipt::try_lock(git_dir).unwrap().unwrap();
        let error = Receipt::update_processes(git_dir, "w", &config, &process, |_| {}).unwrap_err();
        assert!(
            error.downcast_ref::<crate::error::Busy>().is_some(),
            "{error:#}"
        );
        drop(held);
        // The other way round: a setup run does not hold the process lock.
        let setup = Receipt::try_lock(git_dir).unwrap().unwrap();
        drop(process);
        assert!(Receipt::try_process_lock(git_dir).unwrap().is_some());
        drop(setup);
        // A lock of another worktree is no proof for this one.
        let other = tempfile::tempdir().unwrap();
        let foreign = Receipt::try_process_lock(other.path()).unwrap().unwrap();
        assert!(Receipt::update_processes(git_dir, "w", &config, &foreign, |_| {}).is_err());
    }
}
