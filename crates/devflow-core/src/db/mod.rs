//! Postgres branching on one PostgreSQL ≥ 18 server per project.
//!
//! A branch is a database cloned with `CREATE DATABASE … TEMPLATE …
//! STRATEGY FILE_COPY` while the server runs with `file_copy_method = clone`;
//! on a reflink filesystem the clone shares blocks with its source. Branch,
//! snapshot and role identities live in a `devflow` metadata database on the
//! server (see `meta`), never inferred from names. Mutations of one project
//! hold a session-level advisory lock on the admin connection that also runs
//! all of their cluster-level DDL and every metadata write, so losing the lock
//! aborts the statement in flight. The `devflow` database carries a comment
//! marking it as ours and is created as a marked staging database renamed into
//! place; a database of that name without the mark is refused, never adopted.
//!
//! Roles: the admin (provisioner) owns every devflow database and the metadata
//! database. One NOLOGIN owner role `dfo_<project8>` per project owns every
//! object inside every branch and snapshot database, so clones need no
//! ownership translation. Each branch has a LOGIN role `dfl_<id12>` that is a
//! member of the owner (`SET`, not `INHERIT`) and `SET role`s to it at login,
//! and holds `CONNECT` on its own databases only. The owner holds `CONNECT` on
//! the branch databases too (sessions act as it, and `pg_database_size` checks
//! the current user); that is safe only because no login inherits the owner,
//! which `reconcile` enforces first on every operation.
//! See docs/plans/2026-10-10-devflow-1.0.md for the exact steps.
//!
//! Scratch databases (`extra_databases`, see `scratch`): besides its own
//! database a branch has recorded, empty databases for tests and shadow
//! schemas, created with the same closed-creation protocol, owned by the
//! admin, open to the branch's login only. They are never cloned or
//! snapshotted, are emptied again when the branch is reset and are dropped
//! with it. Branch logins keep `NOCREATEDB`.
//!
//! Layout: `admin` (connections, names, lock), `steps` (single DDL steps),
//! `meta` (metadata schema and rows), `ops` (the public operations),
//! `scratch` (the scratch databases of a branch), `reconcile` (crash
//! recovery), `managed` (the Docker server and CoW probe).

mod admin;
mod managed;
mod meta;
mod ops;
mod reconcile;
mod scratch;
mod steps;

use crate::config::{CowCapability, PostgresMode};
use crate::project::Project;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub use managed::{ManagedServer, candidate_ports};
// Shared with the container services (`crate::services`): port allocation,
// bind-failure recognition, OrbStack detection and DNS-safe names.
pub(crate) use managed::{
    AllocationError, BindFailure, allocate_port, is_bind_failure, orbstack, ports_from_seed,
    sanitize_name, summarize_bind_failure,
};
pub(crate) use ops::validate_snapshot_name;

/// Where a new branch's data comes from, as requested by the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DataSource {
    /// An empty database.
    Empty,
    /// The branch currently bound to this workspace (looked up via its receipt).
    Workspace { name: String },
    /// A branch by id.
    Branch { id: uuid::Uuid },
    /// A branch by a unique prefix of its id (at least 8 hex characters, as
    /// listings show them), resolved against the branches that exist.
    BranchPrefix { prefix: String },
    /// A snapshot by name; latest version when `version` is `None`.
    Snapshot { name: String, version: Option<i32> },
}

impl std::str::FromStr for DataSource {
    type Err = anyhow::Error;

    /// `empty`, `snapshot:NAME[@vN]`, `branch:ID` (a UUID or a prefix of at
    /// least 8 hex characters of one), or a workspace name.
    fn from_str(text: &str) -> Result<Self> {
        if text == "empty" {
            return Ok(DataSource::Empty);
        }
        if let Some(rest) = text.strip_prefix("snapshot:") {
            let (name, version) = match rest.rsplit_once("@v") {
                Some((name, v)) => (name, Some(v.parse::<i32>()?)),
                None => (rest, None),
            };
            anyhow::ensure!(!name.is_empty(), "snapshot name is empty");
            return Ok(DataSource::Snapshot {
                name: name.to_string(),
                version,
            });
        }
        if let Some(id) = text.strip_prefix("branch:") {
            return Ok(match id.parse::<crate::ids::IdRef>()? {
                crate::ids::IdRef::Full(id) => DataSource::Branch { id },
                crate::ids::IdRef::Prefix(prefix) => DataSource::BranchPrefix { prefix },
            });
        }
        anyhow::ensure!(!text.is_empty(), "data source is empty");
        Ok(DataSource::Workspace {
            name: text.to_string(),
        })
    }
}

impl DataSource {
    /// The branch this names by id, among the ids of the branches that exist
    /// (`ids` is only read for a prefix); `None` for a source that is not a
    /// branch id. An ambiguous or unknown prefix is an error naming the
    /// candidates.
    pub fn branch_id(
        &self,
        ids: impl IntoIterator<Item = uuid::Uuid>,
    ) -> Result<Option<uuid::Uuid>> {
        match self {
            DataSource::Branch { id } => Ok(Some(*id)),
            DataSource::BranchPrefix { prefix } => crate::ids::IdRef::Prefix(prefix.clone())
                .resolve("branch", ids)
                .map(Some),
            _ => Ok(None),
        }
    }
}

impl std::fmt::Display for DataSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DataSource::Empty => write!(f, "empty"),
            DataSource::Workspace { name } => write!(f, "{name}"),
            DataSource::Branch { id } => write!(f, "branch:{id}"),
            DataSource::BranchPrefix { prefix } => write!(f, "branch:{prefix}"),
            DataSource::Snapshot {
                name,
                version: None,
            } => write!(f, "snapshot:{name}"),
            DataSource::Snapshot {
                name,
                version: Some(v),
            } => write!(f, "snapshot:{name}@v{v}"),
        }
    }
}

/// A source resolved to server identities (workspaces resolved by the caller).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedSource {
    Empty,
    Branch(uuid::Uuid),
    Snapshot(uuid::Uuid),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordStatus {
    Creating,
    Ready,
    Deleting,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchInfo {
    pub id: uuid::Uuid,
    /// Display name (usually the workspace name at creation).
    pub name: String,
    pub db_name: String,
    pub role_name: String,
    /// `empty`, `branch`, or `snapshot`.
    pub source_kind: String,
    pub source_id: Option<uuid::Uuid>,
    /// Workspace instance this branch was created for, if any.
    pub instance_id: Option<uuid::Uuid>,
    pub status: RecordStatus,
    pub cow: CowCapability,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    /// On-disk size reported by PostgreSQL (logical; shared blocks counted fully).
    pub size_bytes: Option<i64>,
    /// Bumped by every reset; the URL stays the same.
    pub generation: i32,
    /// The branch's scratch databases (`extra_databases`): names and
    /// databases, never credentials.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extras: Vec<ScratchInfo>,
    /// Notes from the operation that produced this value (reset only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// A scratch database of a branch (`extra_databases`): empty when created and
/// after every reset, reachable with the branch's login.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScratchInfo {
    pub id: uuid::Uuid,
    /// The configured label (`test`, `shadow`).
    pub name: String,
    /// The generated database name (`df_<project8>_<branch12>_<n>`).
    pub db_name: String,
    pub status: RecordStatus,
    /// The configuration no longer lists this name. The database is kept,
    /// reported by `reconcile` and `doctor`, and dropped only on request
    /// ([`Server::drop_orphans`]) or with its branch.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub orphaned: bool,
    /// Why the last attempt to build it failed, while it is `creating`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotInfo {
    pub id: uuid::Uuid,
    pub name: String,
    pub version: i32,
    pub db_name: String,
    pub source_branch_id: Option<uuid::Uuid>,
    pub status: RecordStatus,
    pub created_at: DateTime<Utc>,
    /// Notes from `create_snapshot` (for example aborted source sessions).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Connection details for one branch. `password` is only known right after
/// creation or rotation; devflow stores it in the worktree receipt and env.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchCredentials {
    pub branch: BranchInfo,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
    pub url: String,
    /// The URL of each configured, ready scratch database by name: the same
    /// login and password, another database.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_urls: BTreeMap<String, String>,
    /// Things the caller should surface: aborted source sessions, recovered
    /// interrupted operations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// How a [`CowCapability`] was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CowSource {
    /// devflow measured the clone path on the server's data filesystem.
    Measured,
    /// The operator declared it in `devflow.local.toml`.
    Declared,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerStatus {
    pub mode: PostgresMode,
    /// Managed: the container is running. External: always `true`.
    pub running: bool,
    pub reachable: bool,
    /// Admin endpoint without credentials (`host:port`): where the host
    /// reaches the server. A managed server keeps its host port across
    /// restarts.
    pub endpoint: Option<String>,
    /// Where other containers on the same Docker networks reach a managed
    /// server (`host:5432`), on OrbStack. `None` on other Docker hosts and for
    /// external servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_endpoint: Option<String>,
    pub server_version: Option<String>,
    /// Current `file_copy_method` setting (`clone` or `copy`).
    pub file_copy_method: Option<String>,
    pub cow: CowCapability,
    /// `None` while the capability is `unknown` without a declaration.
    pub cow_source: Option<CowSource>,
    /// Why the server is not usable, when it is not.
    pub problem: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReconcileReport {
    /// Human-readable actions taken (completed or rolled back records).
    pub actions: Vec<String>,
    /// Records that could not be recovered and need an operator (for example a
    /// prepared transaction blocking `DROP DATABASE`).
    pub problems: Vec<String>,
    /// Scratch databases whose name the configuration no longer lists: kept,
    /// never dropped implicitly (`devflow db reconcile --drop-orphans`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub orphans: Vec<String>,
}

/// The project's Postgres server, as configured by its `engine = "postgres"`
/// service in `[services]`.
pub struct Server {
    /// The project's root, for the remedies of the errors it raises.
    root: std::path::PathBuf,
    project_id: uuid::Uuid,
    project_name: String,
    settings: crate::config::PostgresSection,
    managed: Option<ManagedServer>,
}

impl Server {
    /// `None` when the project has no PostgreSQL service.
    pub fn for_project(project: &Project) -> Result<Option<Server>> {
        let Some((key, settings)) = project
            .config
            .postgres()
            .map(|(key, pg)| (key.to_string(), pg.clone()))
        else {
            return Ok(None);
        };
        let managed = match settings.mode {
            PostgresMode::Managed => Some(ManagedServer::new(project, &key, &settings)?),
            PostgresMode::External => None,
        };
        Ok(Some(Server {
            root: project.root.clone(),
            project_id: project.id(),
            project_name: project.name().to_string(),
            settings,
            managed,
        }))
    }

    pub fn mode(&self) -> PostgresMode {
        self.settings.mode
    }

    pub fn managed(&self) -> Option<&ManagedServer> {
        self.managed.as_ref()
    }

    /// Never fails: problems are reported in the status.
    pub fn status(&self) -> ServerStatus {
        ops::status(self)
    }

    /// Managed: start the container and wait until it accepts connections,
    /// then probe CoW. External: check reachability. Both ensure the metadata
    /// schema and the project row exist.
    pub fn up(&self) -> Result<ServerStatus> {
        ops::up(self)
    }

    /// Managed: stop the container (every branch of the project). External: no-op.
    pub fn down(&self) -> Result<()> {
        ops::down(self)
    }

    /// Create a branch from `source` (see the operation table in the plan).
    /// `instance_id` links it to a workspace instance. The branch gets an
    /// empty scratch database for each name in `extra_databases`, whatever
    /// its source: it is `ready` only with all of them, and a creation that
    /// fails anywhere is rolled back whole.
    pub fn create_branch(
        &self,
        name: &str,
        source: ResolvedSource,
        instance_id: Option<uuid::Uuid>,
    ) -> Result<BranchCredentials> {
        ops::create_branch(self, uuid::Uuid::new_v4(), name, source, instance_id)
    }

    /// [`Server::create_branch`] under an `id` the caller chose and recorded
    /// first, so that after a crash the branch is found by that identity
    /// (never by its name or instance). Fails if a branch with `id` exists.
    pub fn create_branch_with_id(
        &self,
        id: uuid::Uuid,
        name: &str,
        source: ResolvedSource,
        instance_id: Option<uuid::Uuid>,
    ) -> Result<BranchCredentials> {
        ops::create_branch(self, id, name, source, instance_id)
    }

    /// Replace the branch's data with a fresh clone of `source`, keeping its
    /// id, role and password (the URL stays valid). Callers pass the recorded
    /// `source_kind`/`source_id` by default; a source that no longer exists
    /// fails with a pointer to `--to`. `require_cow` is enforced like for a
    /// creation, and the branch's `cow` becomes that of the new data. The
    /// branch's scratch databases are emptied again (the last step of the
    /// reset, resumable like the rest); orphans are left alone.
    pub fn reset_branch(&self, id: uuid::Uuid, source: ResolvedSource) -> Result<BranchInfo> {
        ops::reset_branch(self, id, source)
    }

    /// Drop the scratch databases (orphans included), the database and its
    /// role, then the metadata row.
    pub fn drop_branch(&self, id: uuid::Uuid) -> Result<()> {
        ops::drop_branch(self, id)
    }

    /// Set a new random password for a `ready` branch role and return the URL.
    pub fn rotate_credentials(&self, id: uuid::Uuid) -> Result<BranchCredentials> {
        ops::rotate_credentials(self, id)
    }

    /// The branch an id as typed names: a whole UUID, or a unique prefix of at
    /// least 8 hex characters of one of the branches `db branch list` shows.
    pub fn resolve_branch(&self, id: &crate::ids::IdRef) -> Result<uuid::Uuid> {
        if let crate::ids::IdRef::Full(id) = id {
            return Ok(*id);
        }
        id.resolve("branch", self.list_branches()?.into_iter().map(|b| b.id))
    }

    pub fn branch(&self, id: uuid::Uuid) -> Result<Option<BranchInfo>> {
        ops::branch(self, id)
    }

    /// The latest branch (not being deleted) created for a workspace instance.
    /// `devflow db branch create --instance-id` uses it to hand out the
    /// credentials of an existing branch again. Setup does not: it recovers an
    /// interrupted creation by the branch id its receipt recorded for the
    /// service ([`Server::create_branch_with_id`]). A `creating` or `failed`
    /// result is rolled back by `rotate_credentials`, which then reports that
    /// the branch no longer exists.
    pub fn branch_for_instance(&self, instance_id: uuid::Uuid) -> Result<Option<BranchInfo>> {
        ops::branch_for_instance(self, instance_id)
    }

    /// Host and port written into workspace URLs (`app_host`/`app_port`, else
    /// the admin endpoint). A managed server on an ephemeral port gets a new
    /// port after a restart, so frontends refresh stored URLs from this.
    pub fn app_endpoint(&self) -> Result<(String, u16)> {
        let target = admin::Target::resolve(self)?;
        Ok(target.app_endpoint(&self.settings))
    }

    pub fn list_branches(&self) -> Result<Vec<BranchInfo>> {
        ops::list_branches(self)
    }

    /// Seal a new version of snapshot `name` from a branch.
    pub fn create_snapshot(&self, name: &str, from_branch: uuid::Uuid) -> Result<SnapshotInfo> {
        ops::create_snapshot(self, name, from_branch)
    }

    pub fn list_snapshots(&self) -> Result<Vec<SnapshotInfo>> {
        ops::list_snapshots(self)
    }

    /// Resolve `name` (latest version when `None`) to a ready snapshot.
    pub fn find_snapshot(&self, name: &str, version: Option<i32>) -> Result<SnapshotInfo> {
        ops::find_snapshot(self, name, version)
    }

    pub fn delete_snapshot(&self, name: &str, version: i32) -> Result<()> {
        ops::delete_snapshot(self, name, version)
    }

    /// Complete or roll back `creating`/`deleting` records left by crashes.
    pub fn reconcile(&self) -> Result<ReconcileReport> {
        ops::reconcile(self)
    }

    /// Give a `ready` branch every scratch database the configuration lists
    /// (`extra_databases`) that it does not have yet, empty, and return the
    /// branch's scratch databases, orphans included. Safe to repeat: existing
    /// ones are left alone, a half-made one is built again. New branches get
    /// theirs from [`Server::create_branch`]; this is for branches that
    /// exist before a name is added.
    pub fn ensure_scratch(&self, branch: uuid::Uuid) -> Result<Vec<ScratchInfo>> {
        ops::ensure_scratch(self, branch)
    }

    /// Drop the scratch databases whose name the configuration no longer
    /// lists, by recorded identity, and return a line for each. This is the
    /// one way to remove an orphan short of dropping its branch.
    pub fn drop_orphans(&self) -> Result<Vec<String>> {
        ops::drop_orphans(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_sources_parse_and_round_trip() {
        let id = uuid::Uuid::new_v4();
        let cases = [
            ("empty", DataSource::Empty),
            (
                "feature/login",
                DataSource::Workspace {
                    name: "feature/login".into(),
                },
            ),
            (
                "snapshot:seed",
                DataSource::Snapshot {
                    name: "seed".into(),
                    version: None,
                },
            ),
            (
                "snapshot:seed@v3",
                DataSource::Snapshot {
                    name: "seed".into(),
                    version: Some(3),
                },
            ),
        ];
        for (text, expected) in cases {
            let parsed: DataSource = text.parse().unwrap();
            assert_eq!(parsed, expected, "{text}");
            assert_eq!(parsed.to_string(), text);
        }
        let branch: DataSource = format!("branch:{id}").parse().unwrap();
        assert_eq!(branch, DataSource::Branch { id });
        assert_eq!(branch.to_string(), format!("branch:{id}"));
        // The short id a listing shows is a prefix, resolved later against
        // the branches that exist.
        let short: DataSource = "branch:105A6201".parse().unwrap();
        assert_eq!(
            short,
            DataSource::BranchPrefix {
                prefix: "105a6201".into()
            }
        );
        assert_eq!(short.to_string(), "branch:105a6201");
        let existing: uuid::Uuid = "105a6201-aaaa-4bbb-8ccc-0123456789ab".parse().unwrap();
        assert_eq!(short.branch_id([existing]).unwrap(), Some(existing));
        assert!(short.branch_id([]).is_err());
        assert_eq!(DataSource::Empty.branch_id([existing]).unwrap(), None);
    }

    #[test]
    fn malformed_data_sources_are_rejected() {
        for bad in [
            "",
            "snapshot:",
            "snapshot:@v2",
            "snapshot:x@vlatest",
            "branch:not-a-uuid",
            "branch:105a620",
            "branch:",
        ] {
            assert!(bad.parse::<DataSource>().is_err(), "{bad:?}");
        }
    }
}
