//! `devflow.toml` schema, loading with the machine-local overlay, raw-text
//! editing with concurrent-edit detection, and 0.x detection.
use crate::diag::{Seg, key_label};
use crate::error::{ConfigShape, NotInitialized, ShapeKind};
use crate::hooks::{HookCommand, HookPhase};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Project configuration file names, in lookup order.
pub const CONFIG_FILES: [&str; 2] = ["devflow.toml", ".devflow.toml"];
/// Machine-local overlay, merged on top of the project file. Never committed.
pub const LOCAL_CONFIG_FILE: &str = "devflow.local.toml";
/// Files that only devflow 0.x used.
const LEGACY_FILES: [&str; 4] = [
    ".devflow.yml",
    ".devflow.yaml",
    ".devflow.local.yml",
    ".devflow.local.yaml",
];
pub const MIGRATION_GUIDE: &str =
    "https://github.com/clement-tourriere/devflow/blob/main/docs/MIGRATING.md";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub project: ProjectSection,
    #[serde(default)]
    pub worktree: WorktreeSection,
    /// Services by key (`[services.db]`). The key names the service in
    /// receipts and output; it is never inferred from anything else.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub services: BTreeMap<String, ServiceConfig>,
    #[serde(default)]
    pub hooks: HooksSection,
    /// Extra variables for `.devflow/env`: recipes built from devflow's own
    /// variables, and framework presets.
    #[serde(default, skip_serializing_if = "EnvSection::is_empty")]
    pub env: EnvSection,
    /// How devflow treats the project's pitchfork daemons.
    #[serde(default, skip_serializing_if = "PitchforkSection::is_default")]
    pub pitchfork: PitchforkSection,
}

/// `[pitchfork]`: devflow's part in starting a workspace's daemons.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct PitchforkSection {
    /// Start the workspace's daemons once it is `ready` after `setup` and
    /// `switch` (and when switching into a ready one). Chosen at onboarding;
    /// off by default. Starting is still gated by the approval of the
    /// workspace's pitchfork configuration, and never happens while a daemon
    /// starts on its own (`cron`, `watch`, `boot_start`, `auto`).
    pub autostart: bool,
}

impl PitchforkSection {
    fn is_default(&self) -> bool {
        *self == PitchforkSection::default()
    }
}

/// Longest service key.
pub const MAX_KEY_LEN: usize = 31;
/// Engines this build understands, for error messages.
const SUPPORTED_ENGINES: &str = "postgres, valkey, redis, mysql";

/// Image of a `valkey` service when `image` is not set.
pub const VALKEY_IMAGE: &str = "valkey/valkey:8-alpine";
/// Image of a `redis` service when `image` is not set.
pub const REDIS_IMAGE: &str = "redis:8-alpine";
/// Image of a `mysql` service when `image` is not set. The profile is
/// MySQL 8.4 (standalone InnoDB); other versions are refused when a branch
/// starts, not guessed at.
pub const MYSQL_IMAGE: &str = "mysql:8.4";
/// Variable a `valkey` or `redis` service exports its URL as when `env` is not set.
pub const DEFAULT_CACHE_ENV: &str = "REDIS_URL";
/// Variable a `mysql` service exports its URL as when `env` is not set and
/// the project has no postgres service (which owns `DATABASE_URL`).
pub const DEFAULT_MYSQL_ENV: &str = "DATABASE_URL";
/// The variable a `mysql` service exports its URL as when `env` is not set
/// and a postgres service exists.
pub const MYSQL_URL_ENV: &str = "MYSQL_URL";
/// The typed connection fields a `mysql` service exports besides its URL
/// (there is at most one mysql service, so the names carry no prefix).
pub const MYSQL_FIELD_ENVS: [&str; 5] = [
    "MYSQL_HOST",
    "MYSQL_PORT",
    "MYSQL_USER",
    "MYSQL_PASSWORD",
    "MYSQL_DATABASE",
];
/// The longest MySQL database name devflow generates or accepts.
const MAX_MYSQL_DATABASE_LEN: usize = 48;
/// Schemas a MySQL service's `database` cannot name: the server's own.
const MYSQL_SYSTEM_DATABASES: [&str; 4] =
    ["mysql", "sys", "information_schema", "performance_schema"];

/// Keys that only `engine = "postgres"` has.
const POSTGRES_ONLY_KEYS: [&str; 10] = [
    "mode",
    "admin_url",
    "app_host",
    "app_port",
    "bind",
    "port",
    "database",
    "extra_databases",
    "on_busy_source",
    "cow",
];

/// Names a container service cannot take as its key: `devflow service [NAME]
/// <subcommand>` reads a word in the NAME position as a subcommand first.
pub const SERVICE_SUBCOMMANDS: [&str; 10] = [
    "status",
    "up",
    "down",
    "url",
    "list",
    "snapshot",
    "reset",
    "reconcile",
    "forget",
    "help",
];

/// A data engine a service can run on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    Postgres,
    Valkey,
    Redis,
    Mysql,
}

impl Engine {
    pub const ALL: [Engine; 4] = [
        Engine::Postgres,
        Engine::Valkey,
        Engine::Redis,
        Engine::Mysql,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Engine::Postgres => "postgres",
            Engine::Valkey => "valkey",
            Engine::Redis => "redis",
            Engine::Mysql => "mysql",
        }
    }

    pub fn parse(name: &str) -> Option<Engine> {
        Self::ALL.into_iter().find(|engine| engine.as_str() == name)
    }

    /// A managed container engine devflow branches itself (`valkey`,
    /// `redis`, `mysql`), as opposed to the shared PostgreSQL server.
    pub fn is_container(self) -> bool {
        !matches!(self, Engine::Postgres)
    }

    /// The image a service of this engine runs when `image` is not set.
    pub fn default_image(self) -> Option<&'static str> {
        match self {
            Engine::Postgres => None,
            Engine::Valkey => Some(VALKEY_IMAGE),
            Engine::Redis => Some(REDIS_IMAGE),
            Engine::Mysql => Some(MYSQL_IMAGE),
        }
    }
}

impl std::fmt::Display for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One `[services.<key>]` table, selected by its `engine`.
///
/// Internally tagged: `engine = "postgres"` plus the engine's own keys, which
/// stay strict (`deny_unknown_fields` on the section).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "engine", rename_all = "snake_case")]
pub enum ServiceConfig {
    Postgres(PostgresSection),
    Valkey(ContainerSection),
    Redis(ContainerSection),
    Mysql(ContainerSection),
}

impl ServiceConfig {
    pub fn engine(&self) -> Engine {
        match self {
            ServiceConfig::Postgres(_) => Engine::Postgres,
            ServiceConfig::Valkey(_) => Engine::Valkey,
            ServiceConfig::Redis(_) => Engine::Redis,
            ServiceConfig::Mysql(_) => Engine::Mysql,
        }
    }

    pub fn as_postgres(&self) -> Option<&PostgresSection> {
        match self {
            ServiceConfig::Postgres(section) => Some(section),
            _ => None,
        }
    }

    pub fn as_postgres_mut(&mut self) -> Option<&mut PostgresSection> {
        match self {
            ServiceConfig::Postgres(section) => Some(section),
            _ => None,
        }
    }

    /// The section of a `valkey`, `redis` or `mysql` service.
    pub fn as_container(&self) -> Option<&ContainerSection> {
        match self {
            ServiceConfig::Valkey(section)
            | ServiceConfig::Redis(section)
            | ServiceConfig::Mysql(section) => Some(section),
            ServiceConfig::Postgres(_) => None,
        }
    }

    /// Stable identifier of this section's content: SHA-256 (hex) of its
    /// canonical JSON (fields in declaration order, unset options omitted).
    /// Receipts record it per binding to notice a service that was added or
    /// changed after a workspace was set up.
    ///
    /// Credentials in `admin_url` are masked first, so rotating a password
    /// changes nothing and the digest never depends on a secret. A section
    /// field added later must be skipped when unset (`skip_serializing_if`),
    /// or every existing generation would change with the upgrade.
    pub fn generation(&self) -> String {
        let mut section = self.clone();
        match &mut section {
            ServiceConfig::Postgres(pg) => {
                if let Some(url) = pg.admin_url.as_mut() {
                    *url = crate::redact::text(url);
                }
            }
            ServiceConfig::Valkey(_) | ServiceConfig::Redis(_) | ServiceConfig::Mysql(_) => {}
        }
        // Serializing plain data to JSON cannot fail.
        let json = serde_json::to_vec(&section).unwrap_or_default();
        hex(&Sha256::digest(json))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether `key` is a valid service key: `^[a-z][a-z0-9_-]{0,30}$`.
pub fn valid_service_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && key.len() <= MAX_KEY_LEN
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Longest scratch database name.
const MAX_EXTRA_LEN: usize = 21;

/// Names no scratch database can take: `main` is the branch's own database,
/// `postgres` and `devflow` are databases of the server, and `DEVFLOW_*`
/// variables belong to devflow.
const RESERVED_EXTRA_DATABASES: [&str; 3] = ["main", "postgres", "devflow"];

/// Whether `name` is a valid scratch database name (`extra_databases`):
/// `^[a-z][a-z0-9_]{0,20}$`, and not reserved (`main`, `postgres`, `devflow`,
/// `template*`). The name is only a label: the database is named by devflow.
pub fn valid_extra_database(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= MAX_EXTRA_LEN
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !RESERVED_EXTRA_DATABASES.contains(&name)
        && !name.starts_with("template")
}

/// The variable a scratch database is exported as: `test` is
/// `TEST_DATABASE_URL`, `shadow` is `SHADOW_DATABASE_URL`.
pub fn extra_database_env_name(name: &str) -> String {
    format!("{}_DATABASE_URL", name.to_ascii_uppercase())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProjectSection {
    pub name: String,
    /// Stable across machines; written by `devflow init` and committed.
    pub id: uuid::Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct WorktreeSection {
    /// Parent directory for new worktrees, relative to the main checkout.
    /// `{project}` and `{repo}` expand to the project name and the main
    /// checkout's directory name.
    pub dir: String,
    /// Paths cloned from the source worktree into new worktrees when present.
    pub copy: Vec<String>,
}

impl Default for WorktreeSection {
    fn default() -> Self {
        Self {
            dir: "../{repo}.worktrees".into(),
            copy: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PostgresMode {
    /// devflow runs a PostgreSQL container with `docker compose`.
    #[default]
    Managed,
    /// An existing PostgreSQL ≥ 18 server reached through `admin_url`.
    External,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BusySourcePolicy {
    /// Terminate sessions on the source database, then clone (bounded retries).
    #[default]
    Disconnect,
    /// Refuse to clone a source database that has sessions.
    Fail,
}

/// Whether cloned databases share blocks with their source.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CowCapability {
    /// Block sharing was verified on the server's data directory.
    Verified,
    /// Clones are full copies.
    Copy,
    /// Not verifiable from here (external servers).
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct PostgresSection {
    pub mode: PostgresMode,
    /// Managed mode: image to run; must be PostgreSQL ≥ 18.
    pub image: String,
    /// External mode: admin (provisioner) connection URL. `DEVFLOW_PG_ADMIN_URL` overrides it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admin_url: Option<String>,
    /// Host written into workspace URLs when it differs from the admin endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_host: Option<String>,
    /// Port written into workspace URLs when it differs from the admin endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_port: Option<u16>,
    /// Managed mode: address the container port is published on.
    pub bind: String,
    /// Managed mode: fixed host port; 0 lets Docker pick one.
    pub port: u16,
    /// Display name of the default workspace's database branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// Scratch databases every branch gets besides its own (`["test",
    /// "shadow"]`): empty, recorded, owned like the branch's data and
    /// reachable with the branch's login. Each is exported as
    /// `<NAME>_DATABASE_URL`. See [`valid_extra_database`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extra_databases: Vec<String>,
    /// Default: `disconnect` for a managed server bound to loopback, `fail` otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_busy_source: Option<BusySourcePolicy>,
    /// Refuse to create branches unless CoW is verified.
    pub require_cow: bool,
    /// External mode: operator declaration after checking the server's filesystem.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cow: Option<CowCapability>,
}

impl Default for PostgresSection {
    fn default() -> Self {
        Self {
            mode: PostgresMode::Managed,
            image: "postgres:18".into(),
            admin_url: None,
            app_host: None,
            app_port: None,
            bind: "127.0.0.1".into(),
            port: 0,
            database: None,
            extra_databases: Vec::new(),
            on_busy_source: None,
            require_cow: false,
            cow: None,
        }
    }
}

/// The keys of a `valkey`, `redis` or `mysql` service. Container engines are
/// managed only: devflow runs the containers, so there is no `mode`,
/// `admin_url` or port to configure (each branch's host port is allocated
/// once and kept).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct ContainerSection {
    /// Image to run, by tag or digest. Default: [`VALKEY_IMAGE`],
    /// [`REDIS_IMAGE`] or [`MYSQL_IMAGE`]. A branch pins the image digest it
    /// was created with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Variable the connection URL is exported as. Default:
    /// [`DEFAULT_CACHE_ENV`] for `valkey` and `redis`; for `mysql`
    /// [`DEFAULT_MYSQL_ENV`], or [`MYSQL_URL_ENV`] when the project also has a
    /// postgres service.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    /// `mysql` only: the database every branch of the service has (the schema
    /// its application account owns). Default: the project name as an
    /// identifier. Branches cloned from existing data keep the name that data
    /// has.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// Refuse to clone unless the filesystem shares blocks (reflink); without
    /// it a full copy is allowed and reported as `copy`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub require_cow: bool,
}

/// Whether `name` can be the database of a `mysql` service:
/// `^[a-z][a-z0-9_]{0,47}$`, and not one of the server's own schemas.
pub fn valid_mysql_database(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= MAX_MYSQL_DATABASE_LEN
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !MYSQL_SYSTEM_DATABASES.contains(&name)
}

/// The database a `mysql` service gets when `database` is not set: the
/// project name reduced to lowercase letters, digits and `_`, starting with a
/// letter.
pub fn default_mysql_database(project_name: &str) -> String {
    let mut name = String::new();
    for c in project_name.to_lowercase().chars() {
        let c = if c.is_ascii_lowercase() || c.is_ascii_digit() {
            c
        } else {
            '_'
        };
        if c != '_' || !name.ends_with('_') {
            name.push(c);
        }
    }
    let name = name.trim_matches('_');
    let mut name = if name.is_empty() {
        "app".to_string()
    } else if name.starts_with(|c: char| c.is_ascii_digit()) {
        format!("db_{name}")
    } else {
        name.to_string()
    };
    name.truncate(MAX_MYSQL_DATABASE_LEN);
    let name = name.trim_end_matches('_').to_string();
    if valid_mysql_database(&name) {
        name
    } else {
        // Only a project named after a system schema ("mysql") gets here.
        format!("{name}_db")
    }
}

impl ContainerSection {
    /// The image a service of `engine` runs.
    pub fn image(&self, engine: Engine) -> String {
        self.image
            .clone()
            .or_else(|| engine.default_image().map(str::to_string))
            .unwrap_or_default()
    }

    /// The variable the URL is exported as. `with_postgres` says whether the
    /// project also has a postgres service, which owns `DATABASE_URL`.
    pub fn env_name(&self, engine: Engine, with_postgres: bool) -> &str {
        self.env.as_deref().unwrap_or(match engine {
            Engine::Mysql if with_postgres => MYSQL_URL_ENV,
            Engine::Mysql => DEFAULT_MYSQL_ENV,
            _ => DEFAULT_CACHE_ENV,
        })
    }

    /// The database of a `mysql` service whose project is `project_name`.
    pub fn mysql_database(&self, project_name: &str) -> String {
        self.database
            .clone()
            .unwrap_or_else(|| default_mysql_database(project_name))
    }

    fn validate(&self, engine: Engine, key: &str) -> Result<()> {
        if let Some(image) = &self.image {
            anyhow::ensure!(
                !image.trim().is_empty()
                    && image == image.trim()
                    && !image.starts_with('-')
                    && !image.chars().any(|c| c.is_whitespace() || c.is_control()),
                "services.{key}.image must be an image reference (no spaces, not starting with '-')"
            );
        }
        if let Some(database) = &self.database {
            anyhow::ensure!(
                engine == Engine::Mysql,
                "services.{key}.database is only available for mysql services"
            );
            anyhow::ensure!(
                valid_mysql_database(database),
                "services.{key}.database: use a lowercase letter, then lowercase letters, digits \
                 or '_' ({MAX_MYSQL_DATABASE_LEN} characters at most), and not one of the \
                 server's own schemas (mysql, sys, information_schema, performance_schema)"
            );
        }
        if let Some(env) = &self.env {
            anyhow::ensure!(
                valid_env_name(env),
                "services.{key}.env: not a valid variable name (letters, digits and `_`, not \
                 starting with a digit)"
            );
            // A mysql service may take `DATABASE_URL` when no postgres
            // service does: whether that holds is `Config::validate`'s.
            let own_url = engine == Engine::Mysql && env == DEFAULT_MYSQL_ENV;
            anyhow::ensure!(
                own_url || !crate::env_file::is_reserved(env),
                "services.{key}.env: that variable is reserved, devflow sets DEVFLOW_*, PG*, \
                 DATABASE_URL, COMPOSE_PROJECT_NAME and the URLs of the scratch databases itself"
            );
            anyhow::ensure!(
                engine != Engine::Mysql || !MYSQL_FIELD_ENVS.contains(&env.as_str()),
                "services.{key}.env: {env} is one of the connection fields a mysql service \
                 exports next to its URL; choose another variable"
            );
        }
        anyhow::ensure!(
            !SERVICE_SUBCOMMANDS.contains(&key),
            "service key '{key}' is a `devflow service` subcommand; choose another key for a \
             container service"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct HooksSection {
    /// Run once in a new (or adopted) worktree, after payloads, database and env.
    pub setup: Vec<HookEntry>,
    /// Run on every `devflow switch` into an existing workspace.
    pub switch: Vec<HookEntry>,
    /// Run before removal; a failure blocks removal unless forced.
    pub teardown: Vec<HookEntry>,
    /// Per-command timeout (humantime, e.g. "15m"); an entry's own `timeout`
    /// overrides it.
    pub timeout: String,
}

impl Default for HooksSection {
    fn default() -> Self {
        Self {
            setup: Vec::new(),
            switch: Vec::new(),
            teardown: Vec::new(),
            timeout: "15m".into(),
        }
    }
}

/// A framework preset: a fixed set of variables rendered from the PostgreSQL
/// binding's connection parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvPreset {
    /// `DIRECT_URL` (the same URL). `SHADOW_DATABASE_URL` comes with a
    /// `shadow` scratch database (`extra_databases = ["shadow"]`): devflow
    /// exports it itself, with the other scratch URLs, so Prisma's
    /// `shadowDatabaseUrl = env("SHADOW_DATABASE_URL")` finds it whether or
    /// not this preset is on.
    Prisma,
    /// `DB_CONNECTION`, `DB_HOST`, `DB_PORT`, `DB_DATABASE`, `DB_USERNAME`, `DB_PASSWORD`.
    Laravel,
    /// `SPRING_DATASOURCE_URL` (JDBC), `_USERNAME`, `_PASSWORD`.
    Spring,
    /// `SQLALCHEMY_DATABASE_URI` (`postgresql+psycopg://`).
    Sqlalchemy,
    /// `DB_HOST`, `DB_PORT`, `DB_NAME`, `DB_USER`, `DB_PASSWORD`.
    DbParts,
}

impl EnvPreset {
    pub const ALL: [EnvPreset; 5] = [
        EnvPreset::Prisma,
        EnvPreset::Laravel,
        EnvPreset::Spring,
        EnvPreset::Sqlalchemy,
        EnvPreset::DbParts,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            EnvPreset::Prisma => "prisma",
            EnvPreset::Laravel => "laravel",
            EnvPreset::Spring => "spring",
            EnvPreset::Sqlalchemy => "sqlalchemy",
            EnvPreset::DbParts => "db_parts",
        }
    }

    pub fn parse(name: &str) -> Option<EnvPreset> {
        Self::ALL.into_iter().find(|preset| preset.as_str() == name)
    }

    /// The variables the preset can set (for conflict checks; a preset may
    /// leave one out when the connection has no value for it).
    pub fn keys(self) -> &'static [&'static str] {
        match self {
            EnvPreset::Prisma => &["DIRECT_URL"],
            EnvPreset::Laravel => &[
                "DB_CONNECTION",
                "DB_HOST",
                "DB_PORT",
                "DB_DATABASE",
                "DB_USERNAME",
                "DB_PASSWORD",
            ],
            EnvPreset::Spring => &[
                "SPRING_DATASOURCE_URL",
                "SPRING_DATASOURCE_USERNAME",
                "SPRING_DATASOURCE_PASSWORD",
            ],
            EnvPreset::Sqlalchemy => &["SQLALCHEMY_DATABASE_URI"],
            EnvPreset::DbParts => &["DB_HOST", "DB_PORT", "DB_NAME", "DB_USER", "DB_PASSWORD"],
        }
    }
}

impl std::fmt::Display for EnvPreset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `[env]`: `presets = ["prisma", …]` and any number of `NAME = "value"`
/// recipes. A value may reference devflow-generated variables as `${NAME}`
/// (one pass: no defaults, no nesting, never another recipe); see
/// [`crate::env_file::render`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvSection {
    /// Presets in the order written.
    pub presets: Vec<EnvPreset>,
    /// Recipe variables: name → value template.
    pub vars: BTreeMap<String, String>,
}

impl EnvSection {
    pub fn is_empty(&self) -> bool {
        self.presets.is_empty() && self.vars.is_empty()
    }

    /// Order-independent view: presets sorted, without repeats. Digests and
    /// generations are made from this, so reordering changes nothing.
    pub fn canonical(&self) -> (Vec<&'static str>, &BTreeMap<String, String>) {
        let mut presets: Vec<_> = self.presets.iter().map(|p| p.as_str()).collect();
        presets.sort_unstable();
        presets.dedup();
        (presets, &self.vars)
    }

    /// Warnings about `[env]` that is valid but probably not what was meant.
    ///
    /// `preset = "prisma"` (or `presets_`, `Presets`: any key one edit away
    /// from `presets`, other than `presets` itself) under `[env]` is a recipe
    /// that sets a variable of that name, not the preset. When its value
    /// names a known preset the intent is clear; the message says what to
    /// write. Only the preset's own name is ever repeated, never another value.
    pub fn hints(&self) -> Vec<String> {
        self.vars
            .iter()
            .filter(|(key, _)| near_presets(key))
            .filter_map(|(key, value)| {
                let preset = EnvPreset::ALL
                    .into_iter()
                    .find(|p| p.as_str().eq_ignore_ascii_case(value.trim()))?;
                Some(format!(
                    "[env] {key} = \"{preset}\" sets a variable named {key}, not the {preset} \
                     preset: did you mean `presets = [\"{preset}\"]`? \
                     (`devflow env preset add {preset}`)"
                ))
            })
            .collect()
    }

    /// Check names, references and conflicts; every error names its key.
    pub fn validate(&self) -> Result<()> {
        self.validate_with(&[])
    }

    /// [`EnvSection::validate`] for a project whose scratch databases are
    /// exported as `extra_vars` (see [`extra_database_env_name`]): recipes
    /// may reference those variables and cannot set them.
    pub fn validate_with(&self, extra_vars: &[String]) -> Result<()> {
        use crate::env_file::{Segment, is_generated, is_reserved, parse_template};
        // Errors name keys and rules, never a value: see `crate::diag`.
        for (key, template) in &self.vars {
            let label = key_label(key);
            anyhow::ensure!(
                valid_env_name(key),
                "env.{label}: not a valid variable name (letters, digits and `_`, not starting \
                 with a digit)"
            );
            anyhow::ensure!(
                !is_reserved(key) && !extra_vars.contains(key),
                "env.{label}: reserved, devflow sets DEVFLOW_*, PG*, DATABASE_URL, \
                 COMPOSE_PROJECT_NAME, the URLs of the scratch databases \
                 (`extra_databases`) and the URLs of the container services itself"
            );
            let segments =
                parse_template(template).map_err(|why| anyhow::anyhow!("env.{label}: {why}"))?;
            for segment in segments {
                let Segment::Var(name) = segment else {
                    continue;
                };
                anyhow::ensure!(
                    name != *key,
                    "env.{label}: refers to itself; a recipe can only use devflow-generated \
                     variables"
                );
                // A recipe's name is a key of this table and may be shown; any
                // other name is something the value said.
                anyhow::ensure!(
                    !self.vars.contains_key(&name),
                    "env.{label}: refers to the recipe env.{}; only devflow-generated variables \
                     can be referenced (one pass, no nesting)",
                    key_label(&name)
                );
                anyhow::ensure!(
                    is_generated(&name) || extra_vars.contains(&name),
                    "env.{label}: refers to a variable devflow does not generate. Recipes can \
                     use DATABASE_URL, the PG* variables, DEVFLOW_PROJECT, DEVFLOW_PROJECT_ROOT, \
                     DEVFLOW_WORKSPACE, DEVFLOW_WORKSPACE_PATH, DEVFLOW_SOURCE_PATH, \
                     DEVFLOW_WORKSPACE_ID, COMPOSE_PROJECT_NAME, <NAME>_DATABASE_URL for \
                     each of `extra_databases`, and the URL variable of each container service"
                );
            }
        }
        let mut owners: BTreeMap<&str, EnvPreset> = BTreeMap::new();
        for (index, preset) in self.presets.iter().enumerate() {
            anyhow::ensure!(
                !self.presets[..index].contains(preset),
                "env.presets[{index}]: \"{preset}\" is listed twice"
            );
            for key in preset.keys() {
                anyhow::ensure!(
                    !self.vars.contains_key(*key),
                    "env.{key}: also set by the \"{preset}\" preset; remove one of them"
                );
                if let Some(other) = owners.insert(key, *preset) {
                    bail!(
                        "env.presets: \"{other}\" and \"{preset}\" both set {key}; \
                         use one of them"
                    );
                }
            }
        }
        Ok(())
    }
}

/// A key one edit (insertion, deletion or substitution of a character, case
/// ignored) away from `presets`, but not `presets` itself.
fn near_presets(key: &str) -> bool {
    if key == "presets" {
        return false;
    }
    let target: Vec<char> = "presets".chars().collect();
    let key: Vec<char> = key.to_ascii_lowercase().chars().collect();
    if key == target {
        // Only the case differs: still not the key.
        return true;
    }
    match key.len().cmp(&target.len()) {
        std::cmp::Ordering::Equal => key.iter().zip(&target).filter(|(a, b)| a != b).count() == 1,
        std::cmp::Ordering::Less if key.len() + 1 == target.len() => {
            (0..target.len()).any(|skip| {
                target
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != skip)
                    .map(|(_, c)| c)
                    .eq(key.iter())
            })
        }
        std::cmp::Ordering::Greater if key.len() == target.len() + 1 => {
            (0..key.len()).any(|skip| {
                key.iter()
                    .enumerate()
                    .filter(|(i, _)| *i != skip)
                    .map(|(_, c)| c)
                    .eq(target.iter())
            })
        }
        _ => false,
    }
}

/// `^[A-Za-z_][A-Za-z0-9_]*$`
pub fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl Serialize for EnvSection {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        if !self.presets.is_empty() {
            map.serialize_entry("presets", &self.presets)?;
        }
        for (key, value) in &self.vars {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for EnvSection {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{MapAccess, Visitor};
        struct Env;
        impl<'de> Visitor<'de> for Env {
            type Value = EnvSection;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a table of `NAME = \"value\"` recipes and an optional `presets` list")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<EnvSection, A::Error> {
                let mut section = EnvSection::default();
                while let Some(key) = map.next_key::<String>()? {
                    if key == "presets" {
                        // Typed, so a wrong name or shape is reported by its
                        // path (`env.presets[1]`) and kind, never by value.
                        section.presets.extend(map.next_value::<Vec<EnvPreset>>()?);
                    } else {
                        section.vars.insert(key, map.next_value::<String>()?);
                    }
                }
                Ok(section)
            }
        }
        deserializer.deserialize_map(Env)
    }
}

/// One hook command: a plain string, or a table
/// `{ name = "migrate", run = "…", timeout = "5m" }` (`name` and `timeout`
/// optional). Both run with `sh -c`; there is no templating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookEntry {
    /// Shown in logs and output instead of (beside) the command.
    pub name: Option<String>,
    /// The shell command.
    pub run: String,
    /// Overrides `[hooks] timeout` for this command (humantime).
    pub timeout: Option<String>,
}

/// Longest hook name.
const MAX_HOOK_NAME_LEN: usize = 64;

/// Read a duration setting (humantime: "30s", "15m"). The error names the
/// setting (`at`) and not the text, and drops the parser's own message, which
/// quotes it.
fn parse_duration(text: &str, at: &str) -> Result<Duration> {
    humantime::parse_duration(text)
        .map_err(|_| anyhow::anyhow!("{at}: expected a duration like \"30s\" or \"15m\""))
}

impl HookEntry {
    /// An unnamed command with the section's timeout.
    pub fn new(run: impl Into<String>) -> Self {
        Self {
            name: None,
            run: run.into(),
            timeout: None,
        }
    }

    /// What identifies the entry to a person: `name: run` or just `run`.
    pub fn label(&self) -> String {
        match &self.name {
            Some(name) => format!("{name}: {}", self.run),
            None => self.run.clone(),
        }
    }
}

impl From<&str> for HookEntry {
    fn from(run: &str) -> Self {
        Self::new(run)
    }
}

impl From<String> for HookEntry {
    fn from(run: String) -> Self {
        Self::new(run)
    }
}

impl PartialEq<&str> for HookEntry {
    /// A plain command equals its string.
    fn eq(&self, other: &&str) -> bool {
        self.name.is_none() && self.timeout.is_none() && self.run == *other
    }
}

impl Serialize for HookEntry {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        if self.name.is_none() && self.timeout.is_none() {
            return serializer.serialize_str(&self.run);
        }
        let mut map = serializer.serialize_map(None)?;
        if let Some(name) = &self.name {
            map.serialize_entry("name", name)?;
        }
        map.serialize_entry("run", &self.run)?;
        if let Some(timeout) = &self.timeout {
            map.serialize_entry("timeout", timeout)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for HookEntry {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{Error, MapAccess, Visitor};
        struct Entry;
        impl<'de> Visitor<'de> for Entry {
            type Value = HookEntry;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(
                    "a command string, or a table with `run` and optionally `name` and `timeout`",
                )
            }

            fn visit_str<E: Error>(self, run: &str) -> Result<HookEntry, E> {
                Ok(HookEntry::new(run))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<HookEntry, A::Error> {
                let (mut name, mut run, mut timeout) = (None, None, None);
                while let Some(key) = map.next_key::<String>()? {
                    let slot = match key.as_str() {
                        "name" => &mut name,
                        "run" => &mut run,
                        "timeout" => &mut timeout,
                        other => {
                            return Err(crate::diag::own(
                                Some(other),
                                "unknown key (expected one of `name`, `run`, `timeout`)",
                            ));
                        }
                    };
                    if slot.replace(map.next_value::<String>()?).is_some() {
                        return Err(A::Error::duplicate_field(match key.as_str() {
                            "name" => "name",
                            "run" => "run",
                            _ => "timeout",
                        }));
                    }
                }
                Ok(HookEntry {
                    name,
                    run: run.ok_or_else(|| A::Error::missing_field("run"))?,
                    timeout,
                })
            }
        }
        deserializer.deserialize_any(Entry)
    }
}

impl PostgresSection {
    /// Check the fields that have a shape of their own (`key` names the
    /// service in errors).
    fn validate(&self, key: &str) -> Result<()> {
        let key = key_label(key);
        for (index, name) in self.extra_databases.iter().enumerate() {
            if let Some(first) = self.extra_databases[..index].iter().position(|n| n == name) {
                bail!(
                    "services.{key}.extra_databases[{index}]: the same name as entry {first}; \
                     list each scratch database once"
                );
            }
            anyhow::ensure!(
                valid_extra_database(name),
                "services.{key}.extra_databases[{index}]: not a valid name: use a lowercase \
                 letter, then lowercase letters, digits or '_' ({MAX_EXTRA_LEN} characters at \
                 most), and not main, postgres, devflow or template*"
            );
        }
        Ok(())
    }

    /// The variables the scratch databases are exported as, in the order
    /// written.
    pub fn extra_database_vars(&self) -> Vec<String> {
        self.extra_databases
            .iter()
            .map(|name| extra_database_env_name(name))
            .collect()
    }

    /// The effective policy for cloning a source database that has sessions.
    pub fn busy_policy(&self) -> BusySourcePolicy {
        if let Some(policy) = self.on_busy_source {
            return policy;
        }
        let loopback = self
            .bind
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
            || self.bind == "localhost";
        if self.mode == PostgresMode::Managed && loopback {
            BusySourcePolicy::Disconnect
        } else {
            BusySourcePolicy::Fail
        }
    }
}

impl HooksSection {
    pub fn is_empty(&self) -> bool {
        self.setup.is_empty() && self.switch.is_empty() && self.teardown.is_empty()
    }

    pub fn timeout(&self) -> Result<Duration> {
        parse_duration(&self.timeout, "hooks.timeout")
    }

    /// The entries of one phase.
    pub fn entries(&self, phase: HookPhase) -> &[HookEntry] {
        match phase {
            HookPhase::Setup => &self.setup,
            HookPhase::Switch => &self.switch,
            HookPhase::Teardown => &self.teardown,
        }
    }

    /// The timeout `entry` runs under: its own, else the section's. An error
    /// names the setting, not the entry (its command may hold a credential).
    pub fn timeout_of(&self, entry: &HookEntry) -> Result<Duration> {
        self.timeout_at(entry, "the `timeout` of a hook")
    }

    /// [`HooksSection::timeout_of`], with `at` naming the entry's `timeout`.
    fn timeout_at(&self, entry: &HookEntry, at: &str) -> Result<Duration> {
        match &entry.timeout {
            Some(text) => parse_duration(text, at),
            None => self.timeout(),
        }
    }

    /// The commands of `phase`, ready to run: names and effective timeouts.
    pub fn commands(&self, phase: HookPhase) -> Result<Vec<HookCommand>> {
        self.entries(phase)
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let at = format!("hooks.{}[{index}].timeout", phase.as_str());
                Ok(HookCommand {
                    name: entry.name.clone(),
                    run: entry.run.clone(),
                    timeout: self.timeout_at(entry, &at)?,
                })
            })
            .collect()
    }

    /// Check every entry, naming the phase and position of a bad one.
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.timeout()?.is_zero(),
            "hooks.timeout must be longer than zero"
        );
        for phase in [HookPhase::Setup, HookPhase::Switch, HookPhase::Teardown] {
            let mut names: Vec<&str> = Vec::new();
            for (index, entry) in self.entries(phase).iter().enumerate() {
                let at = format!("hooks.{}[{index}]", phase.as_str());
                anyhow::ensure!(
                    !entry.run.trim().is_empty(),
                    "{at}: `run` must not be empty"
                );
                if let Some(name) = &entry.name {
                    anyhow::ensure!(
                        !name.is_empty()
                            && name.chars().count() <= MAX_HOOK_NAME_LEN
                            && name == name.trim()
                            && !name.chars().any(char::is_control),
                        "{at}: `name` must be 1-{MAX_HOOK_NAME_LEN} characters, without control \
                         characters or surrounding spaces"
                    );
                    if let Some(first) = names.iter().position(|n| *n == name.as_str()) {
                        bail!(
                            "{at}: `name` is already used by hooks.{phase}[{first}]; names are \
                             unique within a phase",
                            phase = phase.as_str()
                        );
                    }
                    names.push(name);
                }
                let timeout = self.timeout_at(entry, &format!("{at}.timeout"))?;
                anyhow::ensure!(
                    !timeout.is_zero(),
                    "{at}: `timeout` must be longer than zero"
                );
            }
        }
        Ok(())
    }
}

impl Config {
    /// The project file in `root`, if any.
    pub fn find_file(root: &Path) -> Option<PathBuf> {
        CONFIG_FILES
            .iter()
            .map(|name| root.join(name))
            .find(|path| path.is_file())
    }

    /// Load the project file plus `devflow.local.toml` from `root`. Without a
    /// project file the error is [`NotInitialized`] (or the 0.x guidance when
    /// a 0.x file is found).
    pub fn load(root: &Path) -> Result<(Config, PathBuf)> {
        let path = match Self::find_file(root) {
            Some(path) => path,
            None => {
                if let Some(legacy) = legacy_file(root) {
                    return Err(shape_error(
                        ShapeKind::Legacy,
                        &legacy,
                        legacy_message(&legacy),
                    ));
                }
                return Err(not_initialized(root));
            }
        };
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let mut value = parse_table(&text, &path)?;
        // Each input is checked on its own before merging: an overlay table
        // replaces a base value of another shape (`[services.db]` over a 0.x
        // `[[services]]` array), which would erase the legacy shape the base
        // file is refused for. A partial overlay table of the current format
        // (no `engine` yet) passes; the merged result is validated below.
        check_shape(&value, &path)?;
        let local = root.join(LOCAL_CONFIG_FILE);
        if local.is_file() {
            let text = std::fs::read_to_string(&local)
                .with_context(|| format!("cannot read {}", local.display()))?;
            let overlay = parse_table(&text, &local)?;
            check_shape(&overlay, &local)?;
            merge(&mut value, overlay);
        }
        // An error about the merged document names both files: the value may
        // come from either.
        let label = if local.is_file() {
            merged_label(&path.display().to_string(), LOCAL_CONFIG_FILE)
        } else {
            path.display().to_string()
        };
        let config = Self::from_value(value, Path::new(&label))?;
        Ok((config, path))
    }

    /// The `[project]` id and name the project in `root` has, read with the
    /// layers [`Config::load`] merges (`devflow.local.toml` over the project
    /// file, key by key) but without judging the rest: a project whose
    /// configuration is refused (an earlier build's `[postgres]`, say) still
    /// has an identity. The error is [`NotInitialized`] without a project file.
    pub fn read_identity(root: &Path) -> Result<(uuid::Uuid, String)> {
        let path = Self::find_file(root).ok_or_else(|| not_initialized(root))?;
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let mut table = parse_table(&text, &path)?;
        let local = root.join(LOCAL_CONFIG_FILE);
        if local.is_file() {
            let text = std::fs::read_to_string(&local)
                .with_context(|| format!("cannot read {}", local.display()))?;
            merge(&mut table, parse_table(&text, &local)?);
        }
        let project = table.get("project").and_then(toml::Value::as_table);
        let id = project
            .and_then(|p| p.get("id"))
            .and_then(toml::Value::as_str)
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
            .filter(|id| !id.is_nil());
        let name = project
            .and_then(|p| p.get("name"))
            .and_then(toml::Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty());
        match (id, name) {
            (Some(id), Some(name)) => Ok((id, name.to_string())),
            _ => bail!("{} has no valid [project] id and name", path.display()),
        }
    }

    /// Parse a single document (no overlay); used to validate editor text.
    pub fn parse(text: &str) -> Result<Config> {
        let path = Path::new("devflow.toml");
        Self::from_value(parse_table(text, path)?, path)
    }

    /// Parse a project file's text with the text of `devflow.local.toml`
    /// merged on top, the way [`Config::load`] does for files on disk.
    /// `base_name` and `local_name` only label errors.
    pub fn parse_layers(
        base: &str,
        base_name: &str,
        local: Option<&str>,
        local_name: &str,
    ) -> Result<Config> {
        let base_path = Path::new(base_name);
        let mut value = parse_table(base, base_path)?;
        check_shape(&value, base_path)?;
        let mut label = base_name.to_string();
        if let Some(local) = local {
            let local_path = Path::new(local_name);
            let overlay = parse_table(local, local_path)?;
            check_shape(&overlay, local_path)?;
            merge(&mut value, overlay);
            label = merged_label(base_name, local_name);
        }
        Self::from_value(value, Path::new(&label))
    }

    fn from_value(mut value: toml::Table, path: &Path) -> Result<Config> {
        check_shape(&value, path)?;
        // Services are read one by one so an error names its table, and so an
        // unknown engine gets a message of its own instead of serde's.
        let services = take_services(&mut value, path)?;
        // A failure is described by key path and kind, never by value.
        let mut config: Config = crate::diag::read_toml(toml::Value::Table(value), path, &[])?;
        config.services = services;
        config
            .validate()
            .map_err(|e| anyhow::anyhow!("invalid configuration in {}: {e:#}", path.display()))?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.project.name.trim().is_empty(),
            "project.name must not be empty"
        );
        anyhow::ensure!(!self.project.id.is_nil(), "project.id must be set");
        for (index, entry) in self.worktree.copy.iter().enumerate() {
            validate_relative(entry).with_context(|| format!("invalid worktree.copy[{index}]"))?;
        }
        self.hooks.validate()?;
        let exports = self.exported_variables();
        let extra_vars: Vec<String> = exports.iter().map(|(name, _)| name.clone()).collect();
        self.env.validate_with(&extra_vars)?;
        let mut postgres: Option<&str> = None;
        let mut mysql: Option<&str> = None;
        for (key, service) in &self.services {
            anyhow::ensure!(
                valid_service_key(key),
                "service key '{}' is invalid: use a lowercase letter, then lowercase letters, \
                 digits, '_' or '-' (31 characters at most)",
                key_label(key)
            );
            match service {
                ServiceConfig::Postgres(pg) => {
                    pg.validate(key)?;
                    if let Some(first) = postgres.replace(key) {
                        bail!(
                            "at most one postgres service is supported, found '{}' and '{}'",
                            key_label(first),
                            key_label(key)
                        );
                    }
                    if pg.mode == PostgresMode::Managed {
                        anyhow::ensure!(
                            !pg.image.trim().is_empty(),
                            "services.{}.image must be set",
                            key_label(key)
                        );
                    }
                }
                ServiceConfig::Valkey(section) | ServiceConfig::Redis(section) => {
                    section.validate(service.engine(), key)?;
                }
                ServiceConfig::Mysql(section) => {
                    section.validate(Engine::Mysql, key)?;
                    // One mysql service: its typed fields (`MYSQL_HOST`, …)
                    // carry no service prefix, so two would overwrite each
                    // other. A prefix scheme can lift this later.
                    if let Some(first) = mysql.replace(key) {
                        bail!(
                            "at most one mysql service is supported for now, found '{}' and \
                             '{}' (they would both export MYSQL_HOST, MYSQL_PORT, …)",
                            key_label(first),
                            key_label(key)
                        );
                    }
                }
            }
        }
        // Two services must not export the same variable. The postgres
        // service owns `DATABASE_URL`, which `exported_variables` leaves out.
        let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
        if let Some(owner) = postgres {
            seen.insert("DATABASE_URL", owner);
        }
        for (name, owner) in &exports {
            if let Some(first) = seen.insert(name, owner) {
                bail!(
                    "services.{first} and services.{owner} both export the same variable \
                     {name}; set `env` on a container service to another variable name"
                );
            }
        }
        Ok(())
    }

    /// The variable container service `key` exports its URL as: its `env`, or
    /// the engine's default (see [`ContainerSection::env_name`]). `None` when
    /// `key` is not a container service.
    pub fn url_variable(&self, key: &str) -> Option<&str> {
        let service = self.services.get(key)?;
        Some(
            service
                .as_container()?
                .env_name(service.engine(), self.postgres().is_some()),
        )
    }

    /// The variables the services export beyond `DATABASE_URL` and `PG*`
    /// (which are PostgreSQL's alone): the scratch databases' URLs
    /// (`extra_databases`), the URL of each container service and the typed
    /// fields of a mysql service, each with the key of the service that
    /// exports it. Recipes may reference these and cannot set them.
    pub fn exported_variables(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (key, service) in &self.services {
            match service {
                ServiceConfig::Postgres(pg) => out.extend(
                    pg.extra_database_vars()
                        .into_iter()
                        .map(|name| (name, key.clone())),
                ),
                ServiceConfig::Valkey(_) | ServiceConfig::Redis(_) => {
                    out.extend(self.url_variable(key).map(|v| (v.to_string(), key.clone())));
                }
                ServiceConfig::Mysql(_) => {
                    out.extend(self.url_variable(key).map(|v| (v.to_string(), key.clone())));
                    out.extend(
                        MYSQL_FIELD_ENVS
                            .iter()
                            .map(|v| (v.to_string(), key.clone())),
                    );
                }
            }
        }
        out
    }

    /// The container services (`valkey`, `redis`, `mysql`): key, engine and
    /// section, by key.
    pub fn containers(&self) -> impl Iterator<Item = (&str, Engine, &ContainerSection)> {
        self.services.iter().filter_map(|(key, service)| {
            Some((key.as_str(), service.engine(), service.as_container()?))
        })
    }

    /// The project's PostgreSQL service: its key and section. There is at
    /// most one; `devflow db …` addresses it whatever its key.
    pub fn postgres(&self) -> Option<(&str, &PostgresSection)> {
        self.services
            .iter()
            .find_map(|(key, service)| Some((key.as_str(), service.as_postgres()?)))
    }

    /// Mutable access to the PostgreSQL service.
    pub fn postgres_mut(&mut self) -> Option<(&str, &mut PostgresSection)> {
        self.services
            .iter_mut()
            .find_map(|(key, service)| Some((key.as_str(), service.as_postgres_mut()?)))
    }

    /// Set (or replace) the PostgreSQL service under `key`.
    pub fn set_postgres(&mut self, key: &str, section: PostgresSection) {
        self.services
            .insert(key.to_string(), ServiceConfig::Postgres(section));
    }

    /// The generation of service `key` (see [`ServiceConfig::generation`]).
    pub fn service_generation(&self, key: &str) -> Option<String> {
        self.services.get(key).map(ServiceConfig::generation)
    }

    /// The generation of everything that decides what `.devflow/env` holds:
    /// the set of services (key, engine and content) and the `[env]` recipes
    /// and presets. Receipts record it when the env phase completes.
    pub fn env_generation(&self) -> String {
        let parts: Vec<(&str, Engine, String)> = self
            .services
            .iter()
            .map(|(key, service)| (key.as_str(), service.engine(), service.generation()))
            .collect();
        let json = serde_json::to_vec(&("env.v2", parts, self.env.canonical())).unwrap_or_default();
        hex(&Sha256::digest(json))
    }

    /// A minimal starting file, for callers that cannot plan (the desktop
    /// app's "new project"). `devflow init` proposes its content from what the
    /// repository contains instead; see [`crate::propose`].
    pub fn template(name: &str, id: uuid::Uuid, postgres: Option<PostgresMode>) -> String {
        let mut text = format!(
            r#"[project]
name = "{name}"
id = "{id}"

[worktree]
# Parent directory for new worktrees ({{repo}} = this checkout's directory name).
dir = "../{{repo}}.worktrees"
# Ignored paths cloned (copy-on-write when possible) from the source worktree.
copy = [".env"]
"#
        );
        match postgres {
            Some(PostgresMode::Managed) => text.push_str(
                r#"
[services.db]
engine = "postgres"
mode = "managed"
image = "postgres:18"
"#,
            ),
            Some(PostgresMode::External) => text.push_str(
                r#"
[services.db]
engine = "postgres"
mode = "external"
# Put the admin URL under [services.db] in devflow.local.toml, or in DEVFLOW_PG_ADMIN_URL, not here.
"#,
            ),
            None => {}
        }
        text.push_str(
            r#"
[hooks]
# Shell commands run in a new worktree with DATABASE_URL and DEVFLOW_* set.
setup = []
teardown = []
"#,
        );
        text
    }
}

/// Raw configuration text for editors: preserves formatting and comments,
/// validates before saving, and refuses to overwrite a newer edit.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigDocument {
    pub path: PathBuf,
    pub content: String,
}

impl ConfigDocument {
    pub fn open(root: &Path) -> Result<Self> {
        let path = Config::find_file(root).ok_or_else(|| not_initialized(root))?;
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        Ok(Self { path, content })
    }

    /// Validate `text` as a project file (without the local overlay).
    pub fn validate(text: &str) -> Result<Config> {
        Config::parse(text)
    }

    /// Save `text` if the file still holds the content this document was
    /// opened with. Returns the reopened document.
    pub fn save(&self, text: &str) -> Result<Self> {
        Self::validate(text)?;
        let current = std::fs::read_to_string(&self.path)
            .with_context(|| format!("cannot read {}", self.path.display()))?;
        if current != self.content {
            return Err(crate::error::ConfigChanged::new(self.path.clone()).into());
        }
        crate::fsutil::write_atomic(&self.path, text.as_bytes(), false)?;
        Ok(Self {
            path: self.path.clone(),
            content: text.to_string(),
        })
    }
}

/// How errors about a project file with its machine-local overlay merged in
/// name their source.
fn merged_label(base: &str, local: &str) -> String {
    format!("{base} (merged with {local})")
}

fn not_initialized(root: &Path) -> anyhow::Error {
    anyhow::Error::new(NotInitialized {
        root: root.to_path_buf(),
    })
}

/// Parse a TOML file. An error says what is wrong and where, never what the
/// file says: the parser's own rendering quotes the offending line, which may
/// hold a credential (`admin_url`, an `[env]` token). Its description is a
/// fixed text (see [`crate::diag::toml_syntax_error`]).
fn parse_table(text: &str, path: &Path) -> Result<toml::Table> {
    text.parse::<toml::Table>().map_err(|e| {
        crate::diag::toml_syntax_error(&path.display().to_string(), text, e.message(), e.span())
    })
}

/// Merge `overlay` into `base`: tables merge recursively, other values replace.
fn merge(base: &mut toml::Table, overlay: toml::Table) {
    for (key, value) in overlay {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(base)), toml::Value::Table(overlay)) => merge(base, overlay),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

fn legacy_file(root: &Path) -> Option<PathBuf> {
    LEGACY_FILES
        .iter()
        .map(|name| root.join(name))
        .find(|path| path.exists())
}

/// The 0.x constructs in `value`, recognised by their shape. 1.0 uses
/// `services` too, but as a table of tables keyed by service name; 0.x had an
/// array of tables (or entries that are not tables).
fn legacy_shapes(value: &toml::Table) -> Vec<String> {
    const KEYS: [&str; 6] = [
        "processes",
        "agent",
        "default_workspace",
        "git",
        "commit",
        "proxy",
    ];
    let mut found: Vec<String> = KEYS
        .iter()
        .filter(|key| value.contains_key(**key))
        .map(|key| (*key).to_string())
        .collect();
    match value.get("services") {
        Some(toml::Value::Table(services)) => found.extend(
            services
                .iter()
                .filter(|(_, entry)| !entry.is_table())
                .map(|(key, _)| format!("services.{} (not a table)", key_label(key))),
        ),
        Some(_) => found.push("services (an array, not a table of tables)".into()),
        None => {}
    }
    if let Some(hooks) = value.get("hooks").and_then(toml::Value::as_table) {
        found.extend(
            hooks
                .keys()
                .filter(|key| key.contains('-'))
                .map(|key| format!("hooks.{}", key_label(key))),
        );
    }
    found
}

/// The 1.0 constructs in `value` (including the pre-release `[postgres]`).
fn current_shapes(value: &toml::Table) -> Vec<String> {
    let table = |key: &str| value.get(key).and_then(toml::Value::as_table);
    let mut found = Vec::new();
    if table("postgres").is_some() {
        found.push("postgres".to_string());
    }
    if let Some(services) = table("services") {
        found.extend(
            services
                .iter()
                .filter(|(_, entry)| entry.is_table())
                .map(|(key, _)| format!("services.{}", key_label(key))),
        );
    }
    let has = |section: &str, keys: &[&str]| {
        table(section)
            .into_iter()
            .flat_map(|t| keys.iter().filter(|k| t.contains_key(**k)))
            .map(|k| format!("{section}.{k}"))
            .collect::<Vec<_>>()
    };
    found.extend(has("project", &["id"]));
    found.extend(has("worktree", &["dir", "copy"]));
    found.extend(has("hooks", &["setup", "switch", "teardown", "timeout"]));
    found.extend(has("pitchfork", &["autostart"]));
    found
}

/// Refuse 0.x shapes (with the migration link), mixtures of 0.x and 1.0
/// shapes (naming both sides) and the pre-release top-level `[postgres]`.
fn check_shape(value: &toml::Table, path: &Path) -> Result<()> {
    let legacy = legacy_shapes(value);
    if !legacy.is_empty() {
        let current = current_shapes(value);
        if current.is_empty() {
            return Err(shape_error(ShapeKind::Legacy, path, legacy_message(path)));
        }
        return Err(shape_error(
            ShapeKind::Mixed,
            path,
            format!(
                "{} mixes devflow 1.0 keys ({}) with 0.x keys ({}); keep one format. \
                 See {MIGRATION_GUIDE}",
                path.display(),
                current.join(", "),
                legacy.join(", ")
            ),
        ));
    }
    if value.contains_key("postgres") {
        return Err(shape_error(
            ShapeKind::PreRelease,
            path,
            format!(
                "{}: `[postgres]` is now `[services.db]` with `engine = \"postgres\"` \
                 (see docs/MIGRATING.md)",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn shape_error(kind: ShapeKind, path: &Path, message: String) -> anyhow::Error {
    anyhow::Error::new(ConfigShape {
        kind,
        path: path.to_path_buf(),
        message,
    })
}

/// Remove `services` from `value` and read each entry on its own. Keys, the
/// engine and the engine's fields are validated here; at-most-one rules are
/// [`Config::validate`]'s.
fn take_services(value: &mut toml::Table, path: &Path) -> Result<BTreeMap<String, ServiceConfig>> {
    let mut services = BTreeMap::new();
    let Some(entries) = value.remove("services") else {
        return Ok(services);
    };
    // `check_shape` has already refused every non-table shape.
    let toml::Value::Table(entries) = entries else {
        bail!("services must be a table of tables in {}", path.display());
    };
    for (key, entry) in entries {
        let label = key_label(&key);
        anyhow::ensure!(
            valid_service_key(&key),
            "service key '{label}' in {} is invalid: use a lowercase letter, then lowercase \
             letters, digits, '_' or '-' (31 characters at most)",
            path.display()
        );
        let toml::Value::Table(table) = entry else {
            bail!("services.{label} in {} must be a table", path.display());
        };
        match table.get("engine") {
            None => bail!(
                "services.{label} in {} has no `engine` (supported: {SUPPORTED_ENGINES})",
                path.display()
            ),
            Some(toml::Value::String(name)) => match Engine::parse(name) {
                Some(engine) if engine.is_container() => {
                    // Managed only: say so instead of listing serde's keys.
                    // (`database` is a postgres key and a mysql key.)
                    if let Some(option) = table.keys().find(|k| {
                        POSTGRES_ONLY_KEYS.contains(&k.as_str())
                            && !(engine == Engine::Mysql && k.as_str() == "database")
                    }) {
                        bail!(
                            "services.{label} in {}: `{option}` is not available for {engine} \
                             services, which devflow always runs itself (keys: {})",
                            path.display(),
                            if engine == Engine::Mysql {
                                "image, env, database, require_cow"
                            } else {
                                "image, env, require_cow"
                            }
                        );
                    }
                }
                Some(_) => {}
                // The unsupported name is a value: it is not repeated.
                None => bail!(
                    "services.{label} in {}: unsupported engine (supported: {SUPPORTED_ENGINES})",
                    path.display()
                ),
            },
            Some(other) => bail!(
                "services.{label} in {}: engine must be a string, not {}",
                path.display(),
                other.type_str()
            ),
        }
        let base = [Seg::key("services"), Seg::key(key.as_str())];
        let service: ServiceConfig = crate::diag::read_tagged(table, "engine", path, &base)?;
        services.insert(key, service);
    }
    Ok(services)
}

fn legacy_message(path: &Path) -> String {
    format!(
        "{} is a devflow 0.x configuration. devflow 1.0 uses a new format and never adopts 0.x \
         containers or data. See {MIGRATION_GUIDE} (keep devflow 0.8 to export and remove old resources).",
        path.display()
    )
}

/// Reject absolute paths, `..` and empty components.
pub(crate) fn validate_relative(entry: &str) -> Result<()> {
    let path = Path::new(entry);
    anyhow::ensure!(!entry.trim().is_empty(), "empty path");
    anyhow::ensure!(path.is_relative(), "must be relative");
    for component in path.components() {
        match component {
            std::path::Component::Normal(_) | std::path::Component::CurDir => {}
            _ => bail!("must stay inside the worktree"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diag::render_path;

    fn write(dir: &Path, name: &str, text: &str) {
        std::fs::write(dir.join(name), text).unwrap();
    }

    #[test]
    fn template_round_trips_and_local_overlay_merges() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4();
        write(
            dir.path(),
            "devflow.toml",
            &Config::template("shop", id, Some(PostgresMode::External)),
        );
        write(
            dir.path(),
            LOCAL_CONFIG_FILE,
            "[services.db]\nadmin_url = \"postgres://p@db/postgres\"\n",
        );
        let (config, path) = Config::load(dir.path()).unwrap();
        assert_eq!(path, dir.path().join("devflow.toml"));
        assert_eq!(config.project.id, id);
        assert_eq!(config.worktree.copy, vec![".env".to_string()]);
        let (key, pg) = config.postgres().unwrap();
        assert_eq!(key, "db");
        assert_eq!(pg.mode, PostgresMode::External);
        assert_eq!(pg.admin_url.as_deref(), Some("postgres://p@db/postgres"));
        assert_eq!(pg.busy_policy(), BusySourcePolicy::Fail);
        let managed = PostgresSection::default();
        assert_eq!(managed.busy_policy(), BusySourcePolicy::Disconnect);
        let remote = PostgresSection {
            bind: "100.64.0.2".into(),
            ..PostgresSection::default()
        };
        assert_eq!(remote.busy_policy(), BusySourcePolicy::Fail);
    }

    #[test]
    fn legacy_configs_are_refused_with_guidance() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".devflow.yml", "services: []\n");
        let error = Config::load(dir.path()).unwrap_err().to_string();
        assert!(error.contains("0.x"), "{error}");

        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            ".devflow.toml",
            "[hooks.post-create]\nmigrate = \"make migrate\"\n",
        );
        let error = Config::load(dir.path()).unwrap_err().to_string();
        assert!(error.contains(MIGRATION_GUIDE), "{error}");
    }

    #[test]
    fn unknown_keys_and_escaping_copy_paths_are_rejected() {
        let base = Config::template("shop", uuid::Uuid::new_v4(), None);
        assert!(Config::parse(&format!("{base}\n[extra]\nx = 1\n")).is_err());
        for bad in ["../secrets", "/etc/passwd", "a/../../b"] {
            let text = base.replace("copy = [\".env\"]", &format!("copy = [\"{bad}\"]"));
            assert!(Config::parse(&text).is_err(), "{bad} must be rejected");
        }
        assert!(Config::parse(&base).is_ok());
    }

    #[test]
    fn documents_refuse_stale_saves() {
        let dir = tempfile::tempdir().unwrap();
        let text = Config::template("shop", uuid::Uuid::new_v4(), None);
        write(dir.path(), "devflow.toml", &text);
        let doc = ConfigDocument::open(dir.path()).unwrap();
        let edited = format!("# comment\n{text}");
        let reopened = doc.save(&edited).unwrap();
        assert_eq!(reopened.content, edited);
        assert!(
            doc.save(&text).is_err(),
            "stale document must not overwrite"
        );
        assert!(reopened.save("not = [valid").is_err());
    }

    #[test]
    fn a_missing_project_file_is_a_typed_error_but_legacy_files_keep_their_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let error = Config::load(dir.path()).unwrap_err();
        let typed = error.downcast_ref::<NotInitialized>().expect("typed");
        assert_eq!(typed.root, dir.path());
        // The message says what is wrong; the way out is a remedy.
        assert!(!error.to_string().contains("devflow init"), "{error}");
        assert_eq!(
            crate::remedy::remedies_of(&error),
            vec![crate::remedy::Remedy::Init {
                root: dir.path().to_path_buf()
            }]
        );
        let error = ConfigDocument::open(dir.path()).unwrap_err();
        assert!(error.downcast_ref::<NotInitialized>().is_some());

        write(dir.path(), ".devflow.yml", "services: []\n");
        let error = Config::load(dir.path()).unwrap_err();
        assert!(error.downcast_ref::<NotInitialized>().is_none(), "{error}");
        assert!(error.to_string().contains("0.x"));
    }

    const HEAD: &str =
        "[project]\nname = \"shop\"\nid = \"0b3c9d2e-1111-4222-8333-444455556666\"\n";

    fn parse(extra: &str) -> Result<Config> {
        Config::parse(&format!("{HEAD}{extra}"))
    }

    fn message(extra: &str) -> String {
        format!("{:#}", parse(extra).unwrap_err())
    }

    #[test]
    fn services_are_a_table_of_tables_selected_by_engine() {
        let config = parse(
            "[services.db]\nengine = \"postgres\"\nmode = \"external\"\nport = 5433\n\
             require_cow = true\n",
        )
        .unwrap();
        let (key, pg) = config.postgres().unwrap();
        assert_eq!(key, "db");
        assert_eq!(
            (pg.mode, pg.port, pg.require_cow),
            (PostgresMode::External, 5433, true)
        );
        assert_eq!(config.services["db"].engine(), Engine::Postgres);
        // No services is fine, and so is any valid key.
        assert!(parse("").unwrap().postgres().is_none());
        let named = parse("[services.main-db_2]\nengine = \"postgres\"\n").unwrap();
        assert_eq!(named.postgres().unwrap().0, "main-db_2");
        // Section defaults are the same as before.
        assert_eq!(
            named.postgres().unwrap().1,
            &PostgresSection::default(),
            "an engine alone is a managed postgres:18"
        );
    }

    #[test]
    fn engine_fields_stay_strict_inside_the_tagged_enum() {
        let error = message("[services.db]\nengine = \"postgres\"\nbogus = 1\n");
        assert!(error.contains("services.db"), "{error}");
        assert!(error.contains("bogus"), "{error}");
        // A value of the wrong type is an error too.
        assert!(parse("[services.db]\nengine = \"postgres\"\nport = \"x\"\n").is_err());
        assert!(parse("[services.db]\nengine = \"postgres\"\nmode = \"cloud\"\n").is_err());
        // Unknown fields elsewhere stay errors.
        assert!(parse("[extra]\nx = 1\n").is_err());
        assert!(parse("[worktree]\nbogus = 1\n").is_err());
    }

    #[test]
    fn unknown_or_missing_engines_are_refused_by_name() {
        let error = message("[services.cache]\nengine = \"oraclehunter2\"\n");
        assert!(
            error.contains("unsupported engine (supported: postgres, valkey, redis, mysql)"),
            "{error}"
        );
        assert!(error.contains("services.cache"), "{error}");
        assert!(
            !error.contains("oraclehunter2"),
            "the value is not echoed: {error}"
        );
        let error = message("[services.cache]\nmode = \"managed\"\n");
        assert!(error.contains("no `engine`"), "{error}");
        let error = message("[services.cache]\nengine = 3\n");
        assert!(error.contains("engine must be a string"), "{error}");
    }

    #[test]
    fn service_keys_and_the_postgres_count_are_validated() {
        for key in ["Db", "1db", "_db", "d.b", "db space", &"a".repeat(32)] {
            let text = format!("[services.\"{key}\"]\nengine = \"postgres\"\n");
            let error = message(&text);
            assert!(error.contains("invalid"), "{key}: {error}");
        }
        assert!(
            parse(&format!(
                "[services.{}]\nengine = \"postgres\"\n",
                "a".repeat(31)
            ))
            .is_ok()
        );
        assert!(valid_service_key("a") && valid_service_key("a-b_c9"));
        assert!(!valid_service_key("") && !valid_service_key("-a") && !valid_service_key("aB"));
        let error =
            message("[services.a]\nengine = \"postgres\"\n[services.b]\nengine = \"postgres\"\n");
        assert!(error.contains("at most one postgres service"), "{error}");
        assert!(error.contains("'a' and 'b'"), "{error}");
        // Programmatic configurations are held to the same rules.
        let mut config = parse("").unwrap();
        config.set_postgres("Bad Key", PostgresSection::default());
        assert!(config.validate().is_err());
    }

    #[test]
    fn scratch_database_names_are_validated() {
        for good in ["test", "shadow", "t", "e2e_2", &"a".repeat(21)] {
            assert!(valid_extra_database(good), "{good}");
        }
        for bad in [
            "",
            "Test",
            "1test",
            "_test",
            "te-st",
            "te st",
            "te.st",
            "tést",
            &"a".repeat(22),
            "main",
            "postgres",
            "devflow",
            "template0",
            "template1",
            "template_x",
            "templates",
        ] {
            assert!(!valid_extra_database(bad), "{bad:?}");
        }
        assert_eq!(extra_database_env_name("test"), "TEST_DATABASE_URL");
        assert_eq!(extra_database_env_name("e2e_2"), "E2E_2_DATABASE_URL");
    }

    #[test]
    fn extra_databases_load_and_are_checked_per_service() {
        let config = parse(
            "[services.db]\nengine = \"postgres\"\nextra_databases = [\"test\", \"shadow\"]\n",
        )
        .unwrap();
        let (_, pg) = config.postgres().unwrap();
        assert_eq!(pg.extra_databases, ["test", "shadow"]);
        assert_eq!(
            pg.extra_database_vars(),
            ["TEST_DATABASE_URL", "SHADOW_DATABASE_URL"]
        );
        for (value, expected) in [
            ("[\"Test\"]", "extra_databases[0]: not a valid name"),
            ("[\"main\"]", "extra_databases[0]: not a valid name"),
            ("[\"template1\"]", "extra_databases[0]: not a valid name"),
            (
                "[\"ok\", \"test\", \"test\"]",
                "extra_databases[2]: the same name as entry 1",
            ),
            ("[\"ok\", \"a-b\"]", "extra_databases[1]: not a valid name"),
        ] {
            let error = message(&format!(
                "[services.db]\nengine = \"postgres\"\nextra_databases = {value}\n"
            ));
            assert!(
                error.contains(expected) && error.contains("services.db.extra_databases"),
                "{value}: {error}"
            );
            // The names are values: they are located by position only.
            for name in ["Test", "template1", "a-b"] {
                assert!(!error.contains(name), "{value}: {error}");
            }
        }
        // Not a list of strings.
        assert!(
            parse("[services.db]\nengine = \"postgres\"\nextra_databases = \"test\"\n").is_err()
        );
        assert!(parse("[services.db]\nengine = \"postgres\"\nextra_databases = [1]\n").is_err());
        // Programmatic configurations are held to the same rules.
        let mut config = parse("").unwrap();
        config.set_postgres(
            "db",
            PostgresSection {
                extra_databases: vec!["BAD".into()],
                ..PostgresSection::default()
            },
        );
        assert!(config.validate().is_err());
    }

    #[test]
    fn extra_databases_change_the_generation_only_when_there_are_some() {
        let plain = parse("[services.db]\nengine = \"postgres\"\n").unwrap();
        let none = parse("[services.db]\nengine = \"postgres\"\nextra_databases = []\n").unwrap();
        assert_eq!(
            plain.service_generation("db"),
            none.service_generation("db"),
            "an empty list is nothing: existing workspaces are not reopened by the upgrade"
        );
        let some =
            parse("[services.db]\nengine = \"postgres\"\nextra_databases = [\"test\"]\n").unwrap();
        let more = parse(
            "[services.db]\nengine = \"postgres\"\nextra_databases = [\"test\", \"shadow\"]\n",
        )
        .unwrap();
        let generations = [
            plain.service_generation("db"),
            some.service_generation("db"),
            more.service_generation("db"),
        ];
        assert_ne!(generations[0], generations[1]);
        assert_ne!(generations[1], generations[2]);
        assert_ne!(plain.env_generation(), some.env_generation());
    }

    #[test]
    fn recipes_may_use_the_scratch_urls_but_not_set_them() {
        let with = |env: &str| {
            format!(
                "[services.db]\nengine = \"postgres\"\nextra_databases = [\"test\", \"e2e\"]\n{env}"
            )
        };
        // Referencing an exported scratch URL is fine...
        assert!(
            parse(&with(
                "[env]\nDJANGO_TEST_URL = \"${TEST_DATABASE_URL}\"\nE = \"${E2E_DATABASE_URL}\"\n"
            ))
            .is_ok()
        );
        // ...but not one that is not exported: no `shadow` here.
        let error = message(&with("[env]\nX = \"${SHADOW_DATABASE_URL}\"\n"));
        assert!(
            error.contains("env.X") && error.contains("does not generate"),
            "{error}"
        );
        // Setting one is refused, for a configured name and for the two
        // reserved ones.
        for key in [
            "E2E_DATABASE_URL",
            "TEST_DATABASE_URL",
            "SHADOW_DATABASE_URL",
        ] {
            let error = message(&with(&format!("[env]\n{key} = \"x\"\n")));
            assert!(
                error.contains(&format!("env.{key}")) && error.contains("reserved"),
                "{key}: {error}"
            );
        }
    }

    #[test]
    fn the_old_top_level_postgres_table_points_to_the_new_schema() {
        let error = message("[postgres]\nmode = \"managed\"\n");
        assert!(
            error.contains(
                "`[postgres]` is now `[services.db]` with `engine = \"postgres\"` \
                 (see docs/MIGRATING.md)"
            ),
            "{error}"
        );
    }

    #[test]
    fn zero_point_x_shapes_are_recognised_by_shape_not_by_key_names() {
        // A 0.x file has no `[project] id`, which would make it a mixture.
        let legacy = |text: &str| {
            let error = format!("{:#}", Config::parse(text).unwrap_err());
            assert!(error.contains("0.x configuration"), "{text}: {error}");
            assert!(error.contains(MIGRATION_GUIDE), "{text}: {error}");
        };
        // `services` as an array of tables, or with entries that are not tables.
        legacy("[[services]]\nname = \"app-db\"\nservice_type = \"postgres\"\n");
        legacy("services = [\"db\"]\n");
        legacy("[services]\ndb = \"postgres\"\n");
        for key in [
            "processes",
            "agent",
            "default_workspace",
            "git",
            "commit",
            "proxy",
        ] {
            legacy(&format!("[{key}]\nx = 1\n"));
        }
        legacy("[hooks.post-create]\nmigrate = \"make migrate\"\n");
        // The names 1.0 uses are not legacy when their shape is the new one.
        assert!(parse("[services.db]\nengine = \"postgres\"\n").is_ok());
        assert!(parse("[hooks]\nsetup = [\"true\"]\n").is_ok());
    }

    #[test]
    fn mixing_one_point_oh_and_zero_point_x_keys_names_the_conflict() {
        let error = message(
            "[services.db]\nengine = \"postgres\"\n[processes]\nweb = \"npm start\"\n\
             [hooks.post-create]\nmigrate = \"x\"\n",
        );
        assert!(error.contains("mixes"), "{error}");
        assert!(error.contains("services.db"), "{error}");
        assert!(error.contains("processes"), "{error}");
        assert!(error.contains("hooks.post-create"), "{error}");
        // The pre-release [postgres] table next to 0.x keys is a mixture too.
        let error = message("[postgres]\nmode = \"managed\"\n[agent]\nx = 1\n");
        assert!(
            error.contains("mixes") && error.contains("postgres"),
            "{error}"
        );
    }

    #[test]
    fn the_local_overlay_merges_service_tables_key_by_key() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "devflow.toml",
            &format!("{HEAD}[services.db]\nengine = \"postgres\"\nmode = \"external\"\nport = 1\n"),
        );
        write(
            dir.path(),
            LOCAL_CONFIG_FILE,
            "[services.db]\nadmin_url = \"postgres://u:pw@h/postgres\"\nport = 2\n", // kingfisher:ignore
        );
        let (config, _) = Config::load(dir.path()).unwrap();
        let (_, pg) = config.postgres().unwrap();
        assert_eq!(
            pg.mode,
            PostgresMode::External,
            "the project file's keys stay"
        );
        assert_eq!(pg.port, 2, "the overlay's value wins");
        assert_eq!(pg.admin_url.as_deref(), Some("postgres://u:pw@h/postgres")); // kingfisher:ignore
        // An overlay table with no engine anywhere is refused, a legacy overlay too.
        write(
            dir.path(),
            LOCAL_CONFIG_FILE,
            "[services.extra]\nport = 3\n",
        );
        assert!(Config::load(dir.path()).is_err());
        write(dir.path(), LOCAL_CONFIG_FILE, "[postgres]\nport = 3\n");
        let error = Config::load(dir.path()).unwrap_err().to_string();
        assert!(
            error.contains("`[postgres]` is now `[services.db]`"),
            "{error}"
        );
    }

    #[test]
    fn hook_entries_are_strings_or_tables_and_string_lists_keep_working() {
        let config = parse(
            "[hooks]\ntimeout = \"10m\"\nsetup = [\n  \"bun install\",\n  \
             { name = \"migrate\", run = \"bun run db:migrate\", timeout = \"5m\" },\n  \
             { run = \"bun run seed\" },\n]\nswitch = [\"echo hi\"]\n\
             [[hooks.teardown]]\nname = \"clean\"\nrun = \"make clean\"\n",
        )
        .unwrap();
        let setup = &config.hooks.setup;
        assert_eq!(setup.len(), 3);
        assert_eq!(setup[0], "bun install", "a string is an unnamed command");
        assert_eq!(setup[1].name.as_deref(), Some("migrate"));
        assert_eq!(setup[1].run, "bun run db:migrate");
        assert_eq!(setup[1].timeout.as_deref(), Some("5m"));
        assert_eq!(
            (setup[2].name.clone(), setup[2].timeout.clone()),
            (None, None)
        );
        assert_eq!(config.hooks.switch, ["echo hi"]);
        assert_eq!(config.hooks.teardown[0].name.as_deref(), Some("clean"));
        // Effective timeouts: the entry's own, else `[hooks] timeout`.
        let commands = config.hooks.commands(HookPhase::Setup).unwrap();
        let timeouts: Vec<_> = commands.iter().map(|c| c.timeout.as_secs()).collect();
        assert_eq!(timeouts, [600, 300, 600]);
        assert_eq!(commands[1].name.as_deref(), Some("migrate"));
        // A plain list round-trips as plain strings, a table as a table.
        let json = serde_json::to_value(&config.hooks).unwrap();
        assert_eq!(json["setup"][0], "bun install");
        assert_eq!(json["setup"][1]["name"], "migrate");
        assert_eq!(
            json["setup"][2], "bun run seed",
            "no name or timeout: a string"
        );
        assert_eq!(json["switch"][0], "echo hi");
    }

    #[test]
    fn bad_hook_entries_are_refused_with_their_position() {
        let hooks = |entries: &str| message(&format!("[hooks]\nsetup = [{entries}]\n"));
        assert!(
            hooks("{ name = \"x\" }").contains("run"),
            "a table needs `run`"
        );
        assert!(hooks("{ run = \"x\", cmd = \"y\" }").contains("cmd"));
        assert!(hooks("{ run = 3 }").contains("string"));
        assert!(hooks("3").contains("command string"));
        let empty = hooks("\"ok\", { run = \"  \" }");
        assert!(
            empty.contains("hooks.setup[1]") && empty.contains("`run`"),
            "{empty}"
        );
        for name in ["\"\"", "\" padded \"", "\"two\\nlines\""] {
            let error = hooks(&format!("{{ name = {name}, run = \"x\" }}"));
            assert!(
                error.contains("hooks.setup[0]") && error.contains("`name`"),
                "{name}: {error}"
            );
        }
        let long = hooks(&format!("{{ name = \"{}\", run = \"x\" }}", "n".repeat(65)));
        assert!(long.contains("`name`"), "{long}");
        let twice = hooks("{ name = \"a\", run = \"x\" }, { name = \"a\", run = \"y\" }");
        assert!(
            twice.contains("hooks.setup[1]") && twice.contains("already used by hooks.setup[0]"),
            "{twice}"
        );
        // The same name in two phases is fine.
        parse("[hooks]\nsetup = [{ name = \"a\", run = \"x\" }]\nswitch = [{ name = \"a\", run = \"y\" }]\n")
            .unwrap();
        let slow = hooks("\"ok\", { run = \"x\", timeout = \"soon\" }");
        assert!(
            slow.contains("hooks.setup[1].timeout: expected a duration like \"30s\"")
                && !slow.contains("soon"),
            "{slow}"
        );
        let hook_timeout = message("[hooks]\ntimeout = \"later\"\n");
        assert!(
            hook_timeout.contains("hooks.timeout") && !hook_timeout.contains("later"),
            "{hook_timeout}"
        );
        let zero = hooks("{ run = \"x\", timeout = \"0s\" }");
        assert!(zero.contains("longer than zero"), "{zero}");
        assert!(message("[hooks]\ntimeout = \"0s\"\n").contains("longer than zero"));
    }

    #[test]
    fn env_recipes_and_presets_parse_and_feed_the_env_generation() {
        let config = parse(
            "[env]\npresets = [\"spring\", \"prisma\"]\nDB_HOST = \"${PGHOST}\"\n\
             API_URL = \"http://localhost:3000\"\nCOMPOSED = \"x-${DEVFLOW_WORKSPACE}-$${raw}\"\n",
        )
        .unwrap();
        assert_eq!(config.env.presets, [EnvPreset::Spring, EnvPreset::Prisma]);
        assert_eq!(config.env.vars.len(), 3);
        assert_eq!(config.env.vars["DB_HOST"], "${PGHOST}");
        assert!(!config.env.is_empty() && parse("").unwrap().env.is_empty());
        // Presets are a set: order does not change what is generated.
        let reordered = parse(
            "[env]\npresets = [\"prisma\", \"spring\"]\nDB_HOST = \"${PGHOST}\"\n\
             API_URL = \"http://localhost:3000\"\nCOMPOSED = \"x-${DEVFLOW_WORKSPACE}-$${raw}\"\n",
        )
        .unwrap();
        assert_eq!(config.env_generation(), reordered.env_generation());
        // Any change to what `.devflow/env` holds is a new generation.
        let changed = parse("[env]\nDB_HOST = \"${PGHOST}\"\n").unwrap();
        assert_ne!(config.env_generation(), changed.env_generation());
        assert_ne!(
            changed.env_generation(),
            parse("").unwrap().env_generation()
        );
        let other = parse("[env]\nDB_HOST = \"${PGPORT}\"\n").unwrap();
        assert_ne!(changed.env_generation(), other.env_generation());
        // It serializes back to the table it was read from.
        let json = serde_json::to_value(&config.env).unwrap();
        assert_eq!(json["presets"], serde_json::json!(["spring", "prisma"]));
        assert_eq!(json["DB_HOST"], "${PGHOST}");
        // The text round-trips through the document editor's validation.
        assert!(Config::parse(&format!("{HEAD}[env]\nA = \"${{DATABASE_URL}}\"\n")).is_ok());
    }

    #[test]
    fn env_errors_are_reported_with_their_key() {
        let env = |body: &str| message(&format!("[env]\n{body}\n"));
        // Names.
        for bad in ["\"1BAD\"", "\"A-B\"", "\"A B\"", "\"\"", "\"É\""] {
            let error = env(&format!("{bad} = \"x\""));
            assert!(
                error.contains("not a valid variable name"),
                "{bad}: {error}"
            );
        }
        // Generated and reserved names cannot be overridden.
        for reserved in [
            "DATABASE_URL",
            "PGHOST",
            "PGPASSWORD",
            "PGAPPNAME",
            "PGSERVICE",
            "DEVFLOW_WORKSPACE",
            "DEVFLOW_ANYTHING",
            "COMPOSE_PROJECT_NAME",
            "TEST_DATABASE_URL",
            "SHADOW_DATABASE_URL",
        ] {
            let error = env(&format!("{reserved} = \"x\""));
            assert!(
                error.contains(&format!("env.{reserved}")) && error.contains("reserved"),
                "{reserved}: {error}"
            );
        }
        // References: missing, not generated, other recipes, itself, loops.
        let error = env("A = \"${HOME}\"");
        assert!(
            error.contains("env.A")
                && error.contains("does not generate")
                && !error.contains("HOME}"),
            "a name the value made up is not repeated: {error}"
        );
        let error = env("A = \"${B}\"\nB = \"x\"");
        assert!(
            error.contains("env.A") && error.contains("recipe env.B"),
            "a recipe is a key of the table and is named: {error}"
        );
        let error = env("A = \"${A}\"");
        assert!(
            error.contains("env.A") && error.contains("refers to itself"),
            "{error}"
        );
        let error = env("A = \"${B}\"\nB = \"${A}\"");
        assert!(
            error.contains("refers to the recipe"),
            "a loop is a reference to a recipe: {error}"
        );
        // Only `${NAME}` is a reference.
        for bad in [
            "${PGHOST:-x}",
            "${PGHOST-x}",
            "${}",
            "${PGHOST",
            "${${PGHOST}}",
            "${1}",
        ] {
            let error = env(&format!("A = \"{bad}\""));
            assert!(
                error.contains("env.A: ")
                    && (error.contains("unsupported reference syntax")
                        || error.contains("is never closed")),
                "{bad}: {error}"
            );
            // The template is a value: no piece of it is repeated.
            assert!(!error.contains("PGHOST"), "{bad}: {error}");
        }
        // Shapes: values are strings, presets a list of known names.
        let error = env("A = 3");
        assert!(
            error.contains("env.A") && error.contains("expected a string"),
            "{error}"
        );
        let error = env("presets = \"prisma\"");
        assert!(error.contains("env.presets"), "{error}");
        let error = env("presets = [3]");
        assert!(error.contains("env.presets[0]"), "{error}");
        let error = env("presets = [\"prisma\", \"rails\"]");
        assert!(
            error.contains("env.presets[1]: unknown value")
                && error.contains("`prisma`, `laravel`, `spring`, `sqlalchemy`, `db_parts`")
                && !error.contains("rails"),
            "{error}"
        );
        // Conflicts: a preset twice, a recipe on a preset's key, two presets on one key.
        let error = env("presets = [\"prisma\", \"prisma\"]");
        assert!(
            error.contains("env.presets[1]: \"prisma\" is listed twice"),
            "{error}"
        );
        let error = env("presets = [\"laravel\"]\nDB_HOST = \"x\"");
        assert!(
            error.contains("env.DB_HOST") && error.contains("\"laravel\" preset"),
            "{error}"
        );
        let error = env("presets = [\"laravel\", \"db_parts\"]");
        assert!(
            error.contains("\"laravel\" and \"db_parts\" both set DB_HOST"),
            "{error}"
        );
        // Presets that do not overlap are fine together.
        assert!(parse("[env]\npresets = [\"prisma\", \"spring\", \"sqlalchemy\"]\n").is_ok());
        // Valid in the document editor too: errors surface before saving.
        assert!(Config::parse(&format!("{HEAD}[env]\nPGHOST = \"x\"\n")).is_err());
    }

    #[test]
    fn each_file_is_checked_for_legacy_shapes_before_the_overlay_is_merged() {
        let dir = tempfile::tempdir().unwrap();
        // A 0.x project file is refused alone...
        write(
            dir.path(),
            "devflow.toml",
            &format!("{HEAD}[[services]]\nname = \"app-db\"\nservice_type = \"postgres\"\n"),
        );
        let alone = Config::load(dir.path()).unwrap_err().to_string();
        assert!(
            alone.contains("mixes") && alone.contains("devflow.toml"),
            "{alone}"
        );
        // ...and an overlay that replaces the array with a current-format
        // table must not make it acceptable.
        write(
            dir.path(),
            LOCAL_CONFIG_FILE,
            "[services.db]\nengine = \"postgres\"\n",
        );
        let merged = Config::load(dir.path()).unwrap_err().to_string();
        assert!(
            merged.contains("mixes") && merged.contains("devflow.toml"),
            "{merged}"
        );
        // The same for a file that is purely 0.x.
        write(
            dir.path(),
            "devflow.toml",
            "[[services]]\nname = \"app-db\"\n",
        );
        let error = Config::load(dir.path()).unwrap_err().to_string();
        assert!(error.contains("0.x configuration"), "{error}");

        // A legacy overlay is refused by itself, naming the overlay.
        write(
            dir.path(),
            "devflow.toml",
            &format!("{HEAD}[services.db]\nengine = \"postgres\"\n"),
        );
        write(
            dir.path(),
            LOCAL_CONFIG_FILE,
            "[processes]\nweb = \"npm start\"\n",
        );
        let error = Config::load(dir.path()).unwrap_err().to_string();
        assert!(error.contains("devflow.local.toml"), "{error}");

        // Partial current-format overlay tables stay welcome.
        write(
            dir.path(),
            LOCAL_CONFIG_FILE,
            "[services.db]\nport = 5433\n",
        );
        let (config, _) = Config::load(dir.path()).unwrap();
        assert_eq!(config.postgres().unwrap().1.port, 5433);
    }

    #[test]
    fn generations_follow_content_and_never_a_password() {
        let one = parse("[services.db]\nengine = \"postgres\"\n").unwrap();
        let same = parse("[services.db]\nengine = \"postgres\"\nmode = \"managed\"\n").unwrap();
        let generation = one.service_generation("db").unwrap();
        assert_eq!(generation.len(), 64);
        assert_eq!(
            same.service_generation("db"),
            Some(generation.clone()),
            "spelling a default out changes nothing"
        );
        assert_eq!(one.service_generation("missing"), None);
        let changed =
            parse("[services.db]\nengine = \"postgres\"\nimage = \"postgres:19\"\n").unwrap();
        assert_ne!(changed.service_generation("db"), Some(generation.clone()));
        assert_ne!(changed.env_generation(), one.env_generation());
        assert_ne!(parse("").unwrap().env_generation(), one.env_generation());
        // Rotating the admin password is not a configuration change; a new host is.
        let url = |u: &str| {
            parse(&format!(
                "[services.db]\nengine = \"postgres\"\nmode = \"external\"\nadmin_url = \"{u}\"\n"
            ))
            .unwrap()
        };
        let a = url("postgres://admin:one@db.internal/postgres"); // kingfisher:ignore
        let b = url("postgres://admin:two@db.internal/postgres"); // kingfisher:ignore
        let c = url("postgres://admin:one@other.internal/postgres"); // kingfisher:ignore
        assert_eq!(a.service_generation("db"), b.service_generation("db"));
        assert_ne!(a.service_generation("db"), c.service_generation("db"));
        // The same key, engine and content give the same env generation anywhere.
        assert_eq!(one.env_generation(), same.env_generation());
    }

    #[test]
    fn the_template_writes_the_services_schema() {
        for mode in [PostgresMode::Managed, PostgresMode::External] {
            let text = Config::template("shop", uuid::Uuid::new_v4(), Some(mode));
            assert!(
                text.contains("[services.db]\nengine = \"postgres\""),
                "{text}"
            );
            assert!(!text.contains("\n[postgres]"), "{text}");
            let config = Config::parse(&text).unwrap();
            assert_eq!(config.postgres().unwrap().1.mode, mode);
        }
        let none = Config::template("shop", uuid::Uuid::new_v4(), None);
        assert!(Config::parse(&none).unwrap().services.is_empty());
    }

    #[test]
    fn configuration_errors_say_what_is_wrong_never_what_the_value_is() {
        // Values that look like credentials, in the shapes a wrong type or a
        // typo gives: none of them may come back in the error.
        for (extra, wanted) in [
            (
                "[env]\nAPI_TOKEN = 123456789\n",
                "env.API_TOKEN: wrong type of value: found an integer, expected a string",
            ),
            (
                "[env]\nAPI_TOKEN = [\"hunter2hunter2\"]\n",
                "env.API_TOKEN: wrong type of value: found an array, expected a string",
            ),
            (
                "[env]\nAPI_TOKEN = { a = \"hunter2hunter2\" }\n",
                "env.API_TOKEN: wrong type of value: found a table, expected a string",
            ),
            (
                "[env]\npresets = \"hunter2hunter2\"\n",
                "env.presets: wrong type of value: found a string, expected a sequence",
            ),
            (
                "[env]\npresets = [123456789]\n",
                "env.presets[0]: wrong type of value",
            ),
            (
                "[env]\npresets = [\"hunter2hunter2:123456789@host\"]\n",
                "env.presets[0]: unknown value (expected one of `prisma`",
            ),
            (
                "[env]\nAPI_TOKEN = hunter2hunter2\n",
                "invalid TOML in devflow.toml at line 5, column 13",
            ),
            (
                "[env]\nAPI_TOKEN = \"hunter2hunter2\n",
                "invalid TOML in devflow.toml at line 5",
            ),
            (
                "[services.db]\nengine = \"postgres\"\nadmin_url = postgres://u:hunter2hunter2@h/db\n", // kingfisher:ignore
                "invalid TOML in devflow.toml at line 6",
            ),
            (
                "[services.db]\nengine = \"postgres\"\nport = \"hunter2hunter2\"\n",
                "invalid configuration in devflow.toml: services.db.port: wrong type of value: \
                 found a string, expected u16",
            ),
            (
                "[services.db]\nengine = \"postgres\"\nadmin_url = 123456789\n",
                "services.db.admin_url: wrong type of value: found an integer, expected a string",
            ),
            (
                "[services.db]\nengine = \"postgres\"\nport = 123456789\n",
                "services.db.port: invalid value: found an integer, expected u16",
            ),
            (
                "[services.db]\nengine = \"postgres\"\nmode = \"hunter2hunter2\"\n",
                "services.db.mode: unknown value (expected `managed` or `external`)",
            ),
            (
                "[services.db]\nengine = \"postgres\"\nextra_databases = [123456789]\n",
                "services.db.extra_databases: wrong type of value",
            ),
            (
                "[hooks]\ntimeout = 123456789\n",
                "hooks.timeout: wrong type of value: found an integer, expected a string",
            ),
            (
                "[hooks]\nsetup = [{ run = 123456789 }]\n",
                "hooks.setup[0].run: wrong type of value: found an integer, expected a string",
            ),
            (
                "[hooks]\nsetup = [{ run = \"x\", hunter2hunter2 = 1 }]\n",
                "hooks.setup[0].hunter2hunter2: unknown key (expected one of `name`, `run`, \
                 `timeout`)",
            ),
            (
                "[worktree]\nbogus = 1\n",
                "worktree.bogus: unknown key (expected `dir` or `copy`)",
            ),
            (
                "[project]\nname = 123456789\n",
                "invalid TOML in devflow.toml",
            ),
        ] {
            let error = message(extra);
            assert!(error.contains(wanted), "{extra:?}: {error}");
            assert!(!error.contains("123456789"), "{extra:?} leaks: {error}");
            if !extra.contains("hunter2hunter2 = 1") {
                // (A key is a name, and is shown when it is plain.)
                assert!(!error.contains("hunter2"), "{extra:?} leaks: {error}");
            }
        }
        // The plain, one-line form of the error carries the reason too.
        let plain = parse("[env]\nAPI_TOKEN = 123456789\n")
            .unwrap_err()
            .to_string();
        assert!(
            plain.contains("env.API_TOKEN: wrong type of value"),
            "{plain}"
        );
        // A preset that is spelled like one is not repeated either.
        let error = message("[env]\npresets = [\"prisam\"]\n");
        assert!(error.contains("unknown value") && !error.contains("prisam"));
        // The overlay is read the same way.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "devflow.toml", HEAD);
        write(
            dir.path(),
            LOCAL_CONFIG_FILE,
            "[services.db]\nengine = \"postgres\"\nadmin_url = postgres://u:hunter2hunter2@h/db\n", // kingfisher:ignore
        );
        let error = format!("{:#}", Config::load(dir.path()).unwrap_err());
        assert!(
            error.contains("devflow.local.toml") && !error.contains("hunter2"),
            "{error}"
        );
    }

    /// Planted in values by the tests below; no message may contain it.
    const MARK: &str = "REVIEWSECRET";

    /// Every way the error can be shown.
    fn shown(error: &anyhow::Error) -> String {
        format!("{error}\n{error:#}\n{error:?}")
    }

    #[test]
    fn a_reference_with_a_default_is_not_echoed_secret_included() {
        let error = message(&format!("[env]\nAUTH = \"${{UNSET:-{MARK}}}\"\n"));
        assert!(
            error.contains("env.AUTH: unsupported reference syntax"),
            "{error}"
        );
        assert!(!error.contains(MARK) && !error.contains("UNSET"), "{error}");
        // The same through the other reference errors.
        for template in [
            format!("${{{MARK}"),
            format!("${{{MARK}-x}}"),
            format!("${{{MARK}}}"),
            format!("a ${{${{{MARK}}}}}"),
        ] {
            let error = message(&format!("[env]\nAUTH = {template:?}\n"));
            assert!(error.contains("env.AUTH: "), "{template}: {error}");
            assert!(!error.contains(MARK), "{template}: {error}");
        }
    }

    #[test]
    fn a_bad_hook_timeout_does_not_echo_the_command() {
        let error = message(&format!(
            "[hooks]\nsetup = [\"true\", \"true\", {{ run = \"tool --auth {MARK}\", timeout = \
             \"bad\" }}]\n"
        ));
        assert!(
            error.contains("hooks.setup[2].timeout: expected a duration like \"30s\""),
            "{error}"
        );
        assert!(!error.contains(MARK) && !error.contains("tool"), "{error}");
        // A name is not echoed either, with the timeout or on its own.
        let error = message(&format!(
            "[hooks]\nswitch = [{{ name = \"{MARK}\", run = \"x\", timeout = \"1 fortnight\" }}]\n"
        ));
        assert!(error.contains("hooks.switch[0].timeout"), "{error}");
        assert!(
            !error.contains(MARK) && !error.contains("fortnight"),
            "{error}"
        );
        let error = message(&format!(
            "[hooks]\nteardown = [{{ name = \"{MARK}\", run = \"x\" }}, {{ name = \"{MARK}\", \
             run = \"y\" }}]\n"
        ));
        assert!(
            error.contains("hooks.teardown[1]: `name` is already used by hooks.teardown[0]"),
            "{error}"
        );
        assert!(!error.contains(MARK), "{error}");
        // The section's own timeout.
        let error = message(&format!("[hooks]\ntimeout = \"{MARK}\"\n"));
        assert!(
            error.contains("hooks.timeout: expected a duration like \"30s\""),
            "{error}"
        );
        assert!(!error.contains(MARK), "{error}");
        // Asking a loaded configuration for the commands says where, not what.
        let mut config = parse("[hooks]\nsetup = [\"true\"]\n").unwrap();
        config.hooks.setup.push(HookEntry {
            name: None,
            run: format!("tool --auth {MARK}"),
            timeout: Some("bad".into()),
        });
        let error = config.hooks.commands(HookPhase::Setup).unwrap_err();
        assert!(
            shown(&error).contains("hooks.setup[1].timeout") && !shown(&error).contains(MARK),
            "{}",
            shown(&error)
        );
        let error = config.hooks.timeout_of(&config.hooks.setup[1]).unwrap_err();
        assert!(!shown(&error).contains(MARK), "{}", shown(&error));
    }

    #[test]
    fn a_value_that_looks_like_the_rest_of_a_message_cannot_get_it_echoed() {
        // The deserializer's message for an unknown variant is
        // "unknown variant `<value>`, expected `managed` or `external`"; a
        // sanitizer that looks for the first "`, expected " keeps the tail of
        // the value. Nothing here searches the message.
        for payload in [
            format!("bad`, expected {MARK}"),
            format!("`, expected one of `{MARK}`"),
            format!("{MARK}`, expected `managed`"),
            format!("bad\", expected {MARK}"),
        ] {
            let error = message(&format!(
                "[services.db]\nengine = \"postgres\"\nmode = {payload:?}\n"
            ));
            assert!(
                error
                    .contains("services.db.mode: unknown value (expected `managed` or `external`)"),
                "{payload}: {error}"
            );
            assert!(!error.contains(MARK), "{payload}: {error}");
            // The same value in the other enum fields and in a service of
            // another name.
            for field in ["on_busy_source", "cow"] {
                let error = message(&format!(
                    "[services.main-db]\nengine = \"postgres\"\n{field} = {payload:?}\n"
                ));
                assert!(
                    error.contains(&format!("services.main-db.{field}: unknown value")),
                    "{payload}: {error}"
                );
                assert!(!error.contains(MARK), "{payload}: {error}");
            }
        }
    }

    /// A document that sets every string-valued field and is valid.
    const EVERY_FIELD: &str = r#"
[project]
name = "shop"
id = "0b3c9d2e-1111-4222-8333-444455556666"

[worktree]
dir = "../{repo}.worktrees"
copy = [".env", "data"]

[services.db]
engine = "postgres"
mode = "external"
image = "postgres:18"
admin_url = "postgres://u@h/postgres"
app_host = "db.internal"
bind = "127.0.0.1"
database = "main"
extra_databases = ["test", "shadow"]
on_busy_source = "fail"
cow = "unknown"

[hooks]
timeout = "5m"
setup = ["echo a", { name = "n", run = "echo b", timeout = "1m" }]
switch = ["echo c"]
teardown = ["echo d"]

[env]
presets = ["prisma"]
API_URL = "http://localhost:3000/${DEVFLOW_WORKSPACE}"
"#;

    fn string_leaves(value: &toml::Value, path: &mut Vec<Seg>, out: &mut Vec<Vec<Seg>>) {
        match value {
            toml::Value::String(_) => out.push(path.clone()),
            toml::Value::Table(table) => {
                for (key, value) in table {
                    path.push(Seg::key(key.as_str()));
                    string_leaves(value, path, out);
                    path.pop();
                }
            }
            toml::Value::Array(items) => {
                for (index, value) in items.iter().enumerate() {
                    path.push(Seg::Index(index));
                    string_leaves(value, path, out);
                    path.pop();
                }
            }
            _ => {}
        }
    }

    fn slot<'a>(root: &'a mut toml::Value, path: &[Seg]) -> &'a mut toml::Value {
        let mut at = root;
        for seg in path {
            at = match (seg, at) {
                (Seg::Key(key), toml::Value::Table(table)) => table.get_mut(key).unwrap(),
                (Seg::Index(index), toml::Value::Array(items)) => &mut items[*index],
                _ => panic!("no such field"),
            };
        }
        at
    }

    #[test]
    fn no_string_value_reaches_an_error_whichever_field_it_is_in() {
        let base = toml::Value::Table(EVERY_FIELD.parse::<toml::Table>().unwrap());
        assert!(
            Config::parse(EVERY_FIELD).is_ok(),
            "the base document is valid"
        );
        let mut leaves = Vec::new();
        string_leaves(&base, &mut Vec::new(), &mut leaves);
        assert!(
            leaves.len() >= 25,
            "every string field is covered: {leaves:?}"
        );

        // Values shaped to survive a sanitizer that looks for delimiters, to
        // pass for a reference, or to pass for one of our own messages.
        let payloads = [
            MARK.to_string(),
            format!("bad`, expected {MARK}"),
            format!("${{UNSET:-{MARK}}}"),
            format!("${{{MARK}}}"),
            format!("{MARK}\n"),
            format!("\"{MARK}\""),
            format!("{MARK}, expected a string"),
            format!("../{MARK}"),
            format!("\u{1f}devflow\u{1f}{MARK}\u{1f}{MARK}"),
        ];
        // Some siblings that make the document invalid on their own: an
        // unknown key beside the field, and a timeout nobody can parse.
        let mut failed = 0;
        let mut tried = 0;
        for leaf in &leaves {
            for payload in &payloads {
                for variant in 0..3 {
                    let mut doc = base.clone();
                    *slot(&mut doc, leaf) = toml::Value::String(payload.clone());
                    match variant {
                        0 => {}
                        1 => {
                            // An unknown key in a table of the document.
                            let parent = &leaf[..leaf.len() - 1];
                            let target = match slot(&mut doc, parent) {
                                toml::Value::Table(_) => parent,
                                _ => &parent[..parent.len() - 1],
                            };
                            if let toml::Value::Table(table) = slot(&mut doc, target) {
                                table.insert("zz_unknown".into(), 1.into());
                            }
                        }
                        _ => {
                            let toml::Value::Table(hooks) = slot(&mut doc, &[Seg::key("hooks")])
                            else {
                                unreachable!()
                            };
                            hooks.insert(
                                "setup".into(),
                                toml::Value::Array(vec![
                                    toml::Value::try_from(HookEntry {
                                        name: Some(payload.clone()),
                                        run: format!("tool --auth {payload}"),
                                        timeout: Some("bad".into()),
                                    })
                                    .unwrap(),
                                ]),
                            );
                        }
                    }
                    let text = toml::to_string(&doc).unwrap();
                    tried += 1;
                    if let Err(error) = Config::parse(&text) {
                        failed += 1;
                        let shown = shown(&error);
                        assert!(
                            !shown.contains(MARK),
                            "{} <- {payload:?} (variant {variant}) leaks:\n{shown}\n{text}",
                            render_path(leaf),
                        );
                        assert!(!shown.is_empty());
                    }
                }
            }
        }
        // The sweep is not vacuous: most of the combinations are errors.
        assert!(
            failed * 2 > tried,
            "{failed} of {tried} documents were refused"
        );
    }

    #[test]
    fn syntax_errors_never_repeat_the_text() {
        // The parser's description is a fixed text; this holds it to that
        // for the places a value can sit.
        for text in [
            format!("a = \"{MARK}\n"),
            format!("a = '{MARK}\n"),
            format!("a = {MARK}\n"),
            format!("[{MARK}\n"),
            format!("[[{MARK}]\n"),
            format!("{MARK}\n"),
            format!("{MARK} = \n"),
            format!("\"{MARK}\" = \n"),
            format!("a = 2024-{MARK}\n"),
            format!("a = 1979-05-27T07:32:00Z{MARK}\n"),
            format!("a = 0x{MARK}\n"),
            format!("a = 1_{MARK}\n"),
            format!("a = \"\\{MARK}\"\n"),
            format!("a = \"\\u{MARK}\"\n"),
            format!("a = [{MARK}]\n"),
            format!("a = {{ {MARK} }}\n"),
            format!("a = {{ {MARK} = 1, {MARK} = 2 }}\n"),
            format!("{MARK} = 1\n{MARK} = 2\n"),
            format!("x = 1\nx.{MARK} = 2\n"),
            format!("[x]\n[x.{MARK}]\n[x]\n"),
            format!("a = \"\"\"{MARK}\n"),
            format!("a = '''{MARK}\n"),
            format!("a.{MARK}.b = 1\n[a]\n"),
            format!("a = 1 {MARK}\n"),
            format!("[a] {MARK}\n"),
        ] {
            let error = Config::parse(&text).unwrap_err();
            let shown = shown(&error);
            assert!(
                shown.contains("invalid TOML in devflow.toml"),
                "{text:?}\n{shown}"
            );
            assert!(!shown.contains(MARK), "{text:?}\n{shown}");
        }
    }

    #[test]
    fn files_on_disk_report_errors_the_same_way() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "devflow.toml",
            &format!("{HEAD}[env]\nAUTH = \"${{UNSET:-{MARK}}}\"\n"),
        );
        let error = Config::load(dir.path()).unwrap_err();
        let text = shown(&error);
        assert!(
            text.contains("devflow.toml") && text.contains("env.AUTH") && !text.contains(MARK),
            "{text}"
        );
        // A bad value in the machine-local file names that file.
        write(dir.path(), "devflow.toml", HEAD);
        write(
            dir.path(),
            LOCAL_CONFIG_FILE,
            &format!("[services.db]\nengine = \"postgres\"\nmode = \"bad`, expected {MARK}\"\n"),
        );
        let error = Config::load(dir.path()).unwrap_err();
        let text = shown(&error);
        assert!(
            text.contains("devflow.toml (merged with devflow.local.toml)")
                && text.contains("services.db.mode"),
            "{text}"
        );
        assert!(!text.contains(MARK), "{text}");
        // So does the document editor's validation.
        let error = ConfigDocument::validate(&format!(
            "{HEAD}[hooks]\nsetup = [{{ run = \"tool --auth {MARK}\", timeout = \"bad\" }}]\n"
        ))
        .unwrap_err();
        assert!(
            shown(&error).contains("hooks.setup[0].timeout") && !shown(&error).contains(MARK),
            "{}",
            shown(&error)
        );
    }

    #[test]
    fn container_services_default_their_image_and_variable() {
        let config = parse(
            "[services.cache]\nengine = \"valkey\"\n[services.queue]\nengine = \"redis\"\n\
             image = \"redis:7-alpine\"\nenv = \"QUEUE_URL\"\nrequire_cow = true\n",
        )
        .unwrap();
        let containers: Vec<_> = config.containers().collect();
        assert_eq!(containers.len(), 2);
        let (key, engine, cache) = containers[0];
        assert_eq!((key, engine), ("cache", Engine::Valkey));
        assert_eq!(cache.image(engine), "valkey/valkey:8-alpine");
        assert_eq!(cache.env_name(engine, false), "REDIS_URL");
        assert!(!cache.require_cow);
        let (key, engine, queue) = containers[1];
        assert_eq!((key, engine), ("queue", Engine::Redis));
        assert_eq!(queue.image(engine), "redis:7-alpine");
        assert_eq!(queue.env_name(engine, false), "QUEUE_URL");
        assert!(queue.require_cow);
        let plain = ContainerSection::default();
        assert_eq!(plain.image(Engine::Redis), "redis:8-alpine");
        assert!(config.postgres().is_none());
        assert!(Engine::Valkey.is_container() && !Engine::Postgres.is_container());
        assert_eq!(Engine::parse("valkey"), Some(Engine::Valkey));
        assert_eq!(Engine::parse("mysql"), Some(Engine::Mysql));
        assert_eq!(Engine::parse("mariadb"), None);
    }

    #[test]
    fn container_services_are_managed_only_and_strict() {
        for option in [
            "mode = \"external\"",
            "admin_url = \"redis://h\"",
            "port = 6379",
            "bind = \"127.0.0.1\"",
            "extra_databases = [\"test\"]",
        ] {
            let error = message(&format!(
                "[services.cache]\nengine = \"valkey\"\n{option}\n"
            ));
            assert!(
                error.contains("not available for valkey services") && error.contains("cache"),
                "{option}: {error}"
            );
        }
        let error = message("[services.cache]\nengine = \"valkey\"\nbogus = 1\n");
        assert!(
            error.contains("services.cache") && error.contains("bogus"),
            "{error}"
        );
        assert!(parse("[services.cache]\nengine = \"valkey\"\nrequire_cow = \"yes\"\n").is_err());
        assert!(parse("[services.cache]\nengine = \"valkey\"\nimage = 7\n").is_err());
        for image in ["", " redis", "-v /:/x", "a b", "redis\n"] {
            let text = format!(
                "[services.cache]\nengine = \"redis\"\nimage = {}\n",
                serde_json::to_string(image).unwrap()
            );
            let error = message(&text);
            assert!(error.contains("services.cache.image"), "{image:?}: {error}");
        }
        // Several container services and a postgres one live together.
        let config = parse(
            "[services.db]\nengine = \"postgres\"\n[services.cache]\nengine = \"valkey\"\n\
             [services.queue]\nengine = \"redis\"\nenv = \"QUEUE_URL\"\n",
        )
        .unwrap();
        assert_eq!(config.postgres().unwrap().0, "db");
        assert_eq!(config.containers().count(), 2);
    }

    #[test]
    fn a_mysql_service_defaults_its_image_database_and_variable() {
        let config = parse("[services.sql]\nengine = \"mysql\"\n").unwrap();
        let (key, engine, section) = config.containers().next().unwrap();
        assert_eq!((key, engine), ("sql", Engine::Mysql));
        assert_eq!(section.image(engine), "mysql:8.4");
        assert_eq!(section.mysql_database("Shop App!"), "shop_app");
        assert_eq!(config.url_variable("sql"), Some("DATABASE_URL"));
        // With a postgres service, DATABASE_URL is postgres's.
        let both =
            parse("[services.db]\nengine = \"postgres\"\n[services.sql]\nengine = \"mysql\"\n")
                .unwrap();
        assert_eq!(both.url_variable("sql"), Some("MYSQL_URL"));
        assert_eq!(both.url_variable("db"), None, "postgres is not a container");
        // Explicit choices win.
        let chosen = parse(
            "[services.sql]\nengine = \"mysql\"\nimage = \"mysql:8.4.3\"\nenv = \"SHOP_DB\"\n\
             database = \"shop\"\nrequire_cow = true\n",
        )
        .unwrap();
        let (_, _, section) = chosen.containers().next().unwrap();
        assert_eq!(chosen.url_variable("sql"), Some("SHOP_DB"));
        assert_eq!(section.mysql_database("anything"), "shop");
        assert_eq!(section.image(Engine::Mysql), "mysql:8.4.3");
        assert!(section.require_cow);
        // The typed fields are exported (and reserved) next to the URL.
        let exported: Vec<String> = chosen
            .exported_variables()
            .into_iter()
            .map(|(name, owner)| {
                assert_eq!(owner, "sql");
                name
            })
            .collect();
        assert_eq!(
            exported,
            [
                "SHOP_DB",
                "MYSQL_HOST",
                "MYSQL_PORT",
                "MYSQL_USER",
                "MYSQL_PASSWORD",
                "MYSQL_DATABASE"
            ]
        );
        // A change of database is a change of the service's generation.
        let other = parse("[services.sql]\nengine = \"mysql\"\ndatabase = \"other\"\n").unwrap();
        assert_ne!(
            config.service_generation("sql"),
            other.service_generation("sql")
        );
    }

    #[test]
    fn mysql_database_names_are_identifiers_devflow_can_quote_and_serve() {
        for name in ["shop", "a", "shop_2", &"a".repeat(48)] {
            assert!(valid_mysql_database(name), "{name}");
        }
        for name in [
            "",
            "Shop",
            "2shop",
            "_shop",
            "sh-op",
            "sh op",
            "sh`op",
            "mysql",
            "sys",
            "information_schema",
            "performance_schema",
            &"a".repeat(49),
        ] {
            assert!(!valid_mysql_database(name), "{name}");
            let error = message(&format!(
                "[services.sql]\nengine = \"mysql\"\ndatabase = {}\n",
                serde_json::to_string(name).unwrap()
            ));
            assert!(error.contains("services.sql.database"), "{name}: {error}");
        }
        // Whatever the project is called, the default is valid.
        for project in [
            "Shop App!",
            "123",
            "",
            "mysql",
            "a--b",
            "Ünï",
            "x".repeat(100).as_str(),
        ] {
            let name = default_mysql_database(project);
            assert!(valid_mysql_database(&name), "{project:?} -> {name}");
        }
        assert_eq!(default_mysql_database("my-shop.app"), "my_shop_app");
        assert_eq!(default_mysql_database("123"), "db_123");
        assert_eq!(default_mysql_database("mysql"), "mysql_db");
        // Only mysql has a database.
        for engine in ["valkey", "redis"] {
            let error = message(&format!(
                "[services.c]\nengine = \"{engine}\"\ndatabase = \"x\"\n"
            ));
            assert!(error.contains("not available for"), "{error}");
        }
    }

    #[test]
    fn mysql_variables_collide_by_name_and_name_both_services() {
        // DATABASE_URL belongs to postgres when there is one.
        let error = message(
            "[services.db]\nengine = \"postgres\"\n[services.sql]\nengine = \"mysql\"\n\
             env = \"DATABASE_URL\"\n",
        );
        assert!(
            error.contains(
                "services.db and services.sql both export the same variable DATABASE_URL"
            ) || error.contains(
                "services.sql and services.db both export the same variable DATABASE_URL"
            ),
            "{error}"
        );
        // Without postgres, mysql may take it, and nobody else may.
        assert!(parse("[services.sql]\nengine = \"mysql\"\nenv = \"DATABASE_URL\"\n").is_ok());
        // A cache cannot take DATABASE_URL: it is reserved for everyone else.
        let error = message("[services.c]\nengine = \"valkey\"\nenv = \"DATABASE_URL\"\n");
        assert!(error.contains("reserved"), "{error}");
        // The fields cannot be the URL's name, or another service's.
        let error = message("[services.sql]\nengine = \"mysql\"\nenv = \"MYSQL_HOST\"\n");
        assert!(error.contains("services.sql.env"), "{error}");
        let error = message(
            "[services.sql]\nengine = \"mysql\"\n[services.c]\nengine = \"valkey\"\n\
             env = \"MYSQL_PORT\"\n",
        );
        assert!(
            error.contains("both export the same variable MYSQL_PORT"),
            "{error}"
        );
        // Two mysql services are refused, naming both.
        let error = message(
            "[services.a]\nengine = \"mysql\"\n[services.b]\nengine = \"mysql\"\nenv = \"B_URL\"\n",
        );
        assert!(
            error.contains("at most one mysql service")
                && error.contains("'a'")
                && error.contains("'b'"),
            "{error}"
        );
        // Recipes may use the fields and cannot set them.
        assert!(
            parse(
                "[services.sql]\nengine = \"mysql\"\n[env]\nDB_HOST = \"${MYSQL_HOST}\"\n\
                 JDBC = \"jdbc:mysql://${MYSQL_HOST}:${MYSQL_PORT}/${MYSQL_DATABASE}\"\n"
            )
            .is_ok()
        );
        let error = message("[services.sql]\nengine = \"mysql\"\n[env]\nMYSQL_USER = \"root\"\n");
        assert!(error.contains("reserved"), "{error}");
        // Without a mysql service they are ordinary variables.
        assert!(parse("[env]\nMYSQL_USER = \"root\"\n").is_ok());
    }

    #[test]
    fn two_services_exporting_one_variable_are_a_configuration_error_naming_both() {
        let error = message(
            "[services.cache]\nengine = \"valkey\"\n[services.queue]\nengine = \"redis\"\n",
        );
        assert!(
            error.contains("services.cache and services.queue both export the same variable"),
            "{error}"
        );
        // A distinct variable resolves it.
        assert!(
            parse(
                "[services.cache]\nengine = \"valkey\"\n[services.queue]\nengine = \"redis\"\n\
                 env = \"QUEUE_URL\"\n"
            )
            .is_ok()
        );
        // A scratch database URL of the postgres service is an export too.
        let error = message(
            "[services.db]\nengine = \"postgres\"\nextra_databases = [\"cache\"]\n\
             [services.c]\nengine = \"valkey\"\nenv = \"CACHE_DATABASE_URL\"\n",
        );
        assert!(
            error.contains("services.c and services.db both export the same variable")
                || error.contains("services.db and services.c both export the same variable"),
            "{error}"
        );
        // Variables devflow sets itself are reserved.
        for name in [
            "DATABASE_URL",
            "PGHOST",
            "DEVFLOW_X",
            "COMPOSE_PROJECT_NAME",
            "TEST_DATABASE_URL",
        ] {
            let error = message(&format!(
                "[services.cache]\nengine = \"valkey\"\nenv = \"{name}\"\n"
            ));
            assert!(
                error.contains("services.cache.env") && error.contains("reserved"),
                "{name}: {error}"
            );
        }
        assert!(
            message("[services.cache]\nengine = \"valkey\"\nenv = \"1X\"\n")
                .contains("not a valid variable name")
        );
        // Recipes can refer to the URL and cannot set it.
        assert!(
            parse("[services.cache]\nengine = \"valkey\"\n[env]\nCACHE = \"${REDIS_URL}\"\n")
                .is_ok()
        );
        assert!(
            message("[services.cache]\nengine = \"valkey\"\n[env]\nREDIS_URL = \"redis://x\"\n")
                .contains("env.REDIS_URL")
        );
    }

    #[test]
    fn service_keys_that_are_subcommands_cannot_name_a_container_service() {
        for key in ["status", "list", "up", "snapshot", "forget"] {
            let error = message(&format!("[services.{key}]\nengine = \"valkey\"\n"));
            assert!(
                error.contains("`devflow service` subcommand"),
                "{key}: {error}"
            );
        }
        // A postgres service is not addressed with `devflow service`.
        assert!(parse("[services.status]\nengine = \"postgres\"\n").is_ok());
        assert!(parse("[services.cache]\nengine = \"valkey\"\n").is_ok());
    }

    #[test]
    fn container_generations_follow_the_section_and_start_from_the_engine() {
        let a = parse("[services.cache]\nengine = \"valkey\"\n").unwrap();
        let same = parse("[services.cache]\nengine = \"valkey\"\nrequire_cow = false\n").unwrap();
        assert_eq!(
            a.service_generation("cache"),
            same.service_generation("cache")
        );
        for changed in [
            "[services.cache]\nengine = \"valkey\"\nrequire_cow = true\n",
            "[services.cache]\nengine = \"valkey\"\nimage = \"valkey/valkey:9\"\n",
            "[services.cache]\nengine = \"valkey\"\nenv = \"CACHE_URL\"\n",
            "[services.cache]\nengine = \"redis\"\n",
        ] {
            let other = parse(changed).unwrap();
            assert_ne!(
                a.service_generation("cache"),
                other.service_generation("cache"),
                "{changed}"
            );
        }
        // The environment depends on the set of services and on their variable.
        assert_ne!(a.env_generation(), parse("").unwrap().env_generation());
        let renamed =
            parse("[services.cache]\nengine = \"valkey\"\nenv = \"CACHE_URL\"\n").unwrap();
        assert_ne!(a.env_generation(), renamed.env_generation());
    }

    #[test]
    fn a_preset_written_as_a_variable_is_hinted() {
        let head = "[project]\nname = \"shop\"\nid = \"0b3c9d2e-1111-4222-8333-444455556666\"\n";
        let hints = |env: &str| {
            Config::parse(&format!("{head}\n[env]\n{env}"))
                .unwrap()
                .env
                .hints()
        };
        // The three spellings people (and models) write, and a typo.
        for key in ["preset", "presets_", "Presets", "presetz", "prsets"] {
            let found = hints(&format!("{key} = \"prisma\"\n"));
            assert_eq!(found.len(), 1, "{key}: {found:?}");
            assert!(
                found[0].contains("did you mean `presets = [\"prisma\"]`?"),
                "{found:?}"
            );
            assert!(found[0].contains(key), "{found:?}");
        }
        // Names are matched without regard to case.
        assert_eq!(hints("preset = \"Laravel\"\n").len(), 1);
        // Nothing to say: the real key, a value that is no preset, other names.
        assert!(hints("presets = [\"prisma\"]\n").is_empty());
        assert!(hints("preset = \"hello\"\n").is_empty());
        assert!(
            hints("PRISMA = \"prisma\"\nDIRECT = \"prisma\"\nresult = \"prisma\"\n").is_empty()
        );
        assert!(hints("").is_empty());
        // The valid configuration still loads: it is a hint, not an error.
        assert!(
            Config::parse(&format!("{head}\n[env]\npreset = \"prisma\"\n"))
                .unwrap()
                .validate()
                .is_ok()
        );
    }
}
