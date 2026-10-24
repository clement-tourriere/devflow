//! Serializable result types of a repository scan.
//!
//! A fact is here because something reads it: a proposal (`propose`), the
//! setup wizard's "what we found" chips, or the check that a plan's inputs did
//! not change (`apply`). Add a fact together with its reader, and remove it with
//! the last one.
//!
//! Every detected fact carries `evidence`: the files (and, after a colon, the
//! keys) that support it, relative to the scanned root, e.g. `bun.lock` or
//! `package.json:dependencies.next`. Paths are always `/`-separated and
//! relative to the scan root; `"."` is the root itself.
use serde::Serialize;

/// Everything [`super::scan`] learned about a repository.
///
/// The scan only reads files; nothing here was obtained by running project
/// code, and no `.env*` value is ever retained.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct Facts {
    /// Git status of the scanned directory.
    pub git: GitFacts,
    /// mise and language version files.
    pub toolchain: ToolchainFacts,
    /// JavaScript projects: the root one first, then nested ones.
    pub js: Vec<JsProject>,
    /// Python projects and their environment tool.
    pub python: Vec<PythonProject>,
    /// Application frameworks and languages.
    pub frameworks: Vec<FrameworkFact>,
    /// ORMs and schema-migration tools.
    pub migrations: Vec<MigrationFact>,
    /// Database client libraries the application depends on.
    pub drivers: Vec<DriverFact>,
    /// The compose file, when there is one.
    pub compose: Option<ComposeFacts>,
    /// `.env*` key names (never values).
    pub env: EnvFacts,
    /// Processes a developer runs while working.
    pub processes: Vec<ProcessFact>,
    /// Existing directories and files that may be worth cloning into a new worktree.
    pub artifacts: Vec<ArtifactFact>,
    /// Existing devflow, pitchfork and mise configuration.
    pub wiring: WiringFacts,
    /// Files that were skipped or could not be read; the scan itself still succeeded.
    pub warnings: Vec<ScanWarning>,
}

/// A file the scan skipped or could not fully use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ScanWarning {
    /// Path relative to the root (`"."` for scan-wide notes).
    pub path: String,
    /// Human-readable reason.
    pub message: String,
}

/// Whether the scanned directory is a Git checkout, which decides whether
/// ignore status can be determined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct GitFacts {
    /// The root is the top of a Git repository or worktree.
    pub repository: bool,
}

impl Facts {
    /// Whether `framework` was detected anywhere.
    pub fn has_framework(&self, framework: Framework) -> bool {
        self.frameworks.iter().any(|f| f.framework == framework)
    }

    /// Compose services running `engine`.
    pub fn compose_services(&self, engine: Engine) -> Vec<&ComposeService> {
        self.compose
            .iter()
            .flat_map(|c| &c.services)
            .filter(|s| s.engine == engine)
            .collect()
    }
}

// -- toolchain --------------------------------------------------------------

/// Toolchain management.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ToolchainFacts {
    /// mise configuration, if any.
    pub mise: Option<MiseFacts>,
    /// Language version files such as `.nvmrc`, by file name.
    pub version_files: Vec<String>,
}

/// mise configuration found at the root.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct MiseFacts {
    /// Configuration files read (`mise.toml`, `.mise.toml`, `.tool-versions`,
    /// `mise.local.toml`, `mise.<env>.toml`, ...), in the order mise layers
    /// them: the lowest precedence first, so a later file wins over an
    /// earlier one.
    pub config_files: Vec<String>,
    /// Pinned tool names, sorted.
    pub tools: Vec<String>,
    /// Task names (`[tasks.*]` and file tasks), sorted.
    pub tasks: Vec<String>,
    /// Every path listed under `[env] _.file` / `_.source`, as written, with
    /// the file and directive it comes from, in the order mise applies them
    /// (layer by layer, then as listed) with repeated entries kept: the same
    /// file listed twice is applied twice, and the later one wins.
    pub env_entries: Vec<MiseEnvEntry>,
    /// Supporting files and keys.
    pub evidence: Vec<String>,
}

/// One path of a mise `[env] _.file` / `_.source` list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct MiseEnvEntry {
    /// The mise configuration file that lists it (`mise.local.toml`).
    pub file: String,
    /// `file` or `source`.
    pub directive: String,
    /// The path as written.
    pub path: String,
    /// The mise environment (`MISE_ENV`) whose file lists it, for
    /// `mise.<env>.toml`; `None` for files mise always reads. An entry with an
    /// environment only applies when that environment is selected, which
    /// detection cannot know.
    pub env: Option<String>,
}

impl MiseEnvEntry {
    /// `mise.local.toml ([env] _.file)`: where the entry is, without its path.
    pub fn describe(&self) -> String {
        format!("{} ([env] _.{})", self.file, self.directive)
    }
}

impl MiseFacts {
    /// The entries mise applies *over* what the config file `file` lists: those
    /// in files of higher precedence, split into the ones that always apply
    /// and the ones that only apply when their `MISE_ENV` is selected. `file`
    /// need not exist (a file a proposal would create). Entries of `file`
    /// itself are not included: their order is the one they are listed in.
    pub fn entries_over(&self, file: &str) -> (Vec<&MiseEnvEntry>, Vec<&MiseEnvEntry>) {
        let Some(rank) = super::toolchain::layer_rank(file) else {
            return (Vec::new(), Vec::new());
        };
        let over = self.env_entries.iter().filter(|entry| {
            super::toolchain::layer_rank(&entry.file).is_some_and(|other| other > rank)
        });
        let (always, sometimes): (Vec<_>, Vec<_>) = over.partition(|entry| entry.env.is_none());
        (always, sometimes)
    }
}

// -- JavaScript -------------------------------------------------------------

/// A JavaScript package manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageManager {
    /// npm.
    Npm,
    /// pnpm.
    Pnpm,
    /// Yarn, classic or berry (see [`PackageManagerFact::yarn_berry`]).
    Yarn,
    /// Bun.
    Bun,
}

/// The package manager of one JavaScript project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PackageManagerFact {
    /// Which tool.
    pub kind: PackageManager,
    /// Yarn 2+ (berry); always false for other tools.
    pub yarn_berry: bool,
    /// No signal at all (no `packageManager` field, lockfile, tool configuration
    /// or enclosing project): npm is the default guess.
    pub assumed: bool,
    /// This tool's lockfile in the project directory (`pnpm-lock.yaml`,
    /// `apps/web/bun.lock`), when there is one. A lockfile of another tool
    /// does not count: it cannot satisfy a frozen install.
    pub lockfile: Option<String>,
    /// Supporting files and keys.
    pub evidence: Vec<String>,
}

impl PackageManagerFact {
    /// The idempotent dependency install command.
    ///
    /// Frozen-lockfile flags are only used when [`Self::lockfile`] exists:
    /// `pnpm install --frozen-lockfile`, `bun install --frozen-lockfile` and
    /// `yarn install --immutable` fail without one, so a project that has not
    /// been installed yet gets the plain install, which resolves and writes
    /// the lockfile.
    ///
    /// Deliberately never `npm ci`: it deletes an existing (for example
    /// copy-on-write cloned) `node_modules` before reinstalling.
    pub fn install_command(&self) -> &'static str {
        let locked = self.lockfile.is_some();
        match (self.kind, self.yarn_berry, locked) {
            (PackageManager::Bun, _, true) => "bun install --frozen-lockfile",
            (PackageManager::Bun, _, false) => "bun install",
            (PackageManager::Pnpm, _, true) => "pnpm install --frozen-lockfile --prefer-offline",
            (PackageManager::Pnpm, _, false) => "pnpm install --prefer-offline",
            (PackageManager::Yarn, true, true) => "yarn install --immutable",
            (PackageManager::Yarn, false, true) => "yarn install --frozen-lockfile",
            (PackageManager::Yarn, _, false) => "yarn install",
            (PackageManager::Npm, _, _) => "npm install --prefer-offline --no-audit --no-fund",
        }
    }

    /// The program and subcommand that run a package script, e.g.
    /// `["bun", "run"]`. Append the script name as its own argument.
    pub fn run_argv(&self) -> [&'static str; 2] {
        match self.kind {
            PackageManager::Bun => ["bun", "run"],
            PackageManager::Pnpm => ["pnpm", "run"],
            PackageManager::Yarn => ["yarn", "run"],
            PackageManager::Npm => ["npm", "run"],
        }
    }
}

/// One `package.json` and what matters about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct JsPackage {
    /// Directory relative to the scan root.
    pub path: String,
    /// Script names, sorted. Script bodies are not retained.
    pub scripts: Vec<String>,
    /// Scripts whose body mentions `prisma generate`.
    pub prisma_generate_scripts: Vec<String>,
    /// Whether this is a workspace member rather than the project root.
    pub workspace_member: bool,
    /// Supporting files and keys.
    pub evidence: Vec<String>,
}

impl JsPackage {
    /// Whether `prisma generate` runs as part of every dependency install.
    pub fn generates_prisma_on_install(&self) -> bool {
        self.prisma_generate_scripts.iter().any(|s| {
            matches!(
                s.as_str(),
                "preinstall" | "install" | "postinstall" | "prepare"
            )
        })
    }

    /// Whether a script with this exact name exists.
    pub fn has_script(&self, name: &str) -> bool {
        self.scripts.iter().any(|s| s == name)
    }
}

/// A directory with its own JavaScript dependency tree (one lockfile).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct JsProject {
    /// Directory relative to the scan root; `"."` for the root project.
    pub path: String,
    /// The package manager that installs this project.
    pub package_manager: PackageManagerFact,
    /// Declares workspaces (`workspaces` in `package.json`, or
    /// `pnpm-workspace.yaml`): its member packages follow in `packages`.
    pub workspaces: bool,
    /// The project's own package first (when it has a `package.json`), then workspace members.
    pub packages: Vec<JsPackage>,
    /// Supporting files and keys.
    pub evidence: Vec<String>,
}

// -- Python -----------------------------------------------------------------

/// A Python environment tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PythonTool {
    /// uv (`uv.lock`).
    Uv,
    /// Poetry (`poetry.lock`).
    Poetry,
    /// pip (`requirements*.txt`).
    Pip,
}

/// A Python project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PythonProject {
    /// Directory relative to the scan root.
    pub path: String,
    /// The tool that creates its environment.
    pub tool: PythonTool,
    /// The tool's lockfile in the project directory (`uv.lock`,
    /// `poetry.lock`), when there is one.
    pub lockfile: Option<String>,
    /// Supporting files and keys.
    pub evidence: Vec<String>,
}

impl PythonProject {
    /// The idempotent environment install command, when the tool has a safe one.
    ///
    /// `uv sync --frozen` fails without a `uv.lock`, so it is only used when
    /// [`Self::lockfile`] exists; otherwise `uv sync` resolves one.
    ///
    /// Environments are not relocatable, so a new worktree creates its own
    /// instead of cloning `.venv`.
    pub fn install_command(&self) -> Option<&'static str> {
        match (self.tool, self.lockfile.is_some()) {
            (PythonTool::Uv, true) => Some("uv sync --frozen"),
            (PythonTool::Uv, false) => Some("uv sync"),
            (PythonTool::Poetry, _) => Some("poetry install --no-interaction"),
            (PythonTool::Pip, _) => None,
        }
    }
}

// -- frameworks -------------------------------------------------------------

/// An application framework or language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Framework {
    /// Next.js.
    NextJs,
    /// Vite used directly (no Vite-based meta-framework).
    Vite,
    /// React Router in framework mode.
    ReactRouter,
    /// Remix.
    Remix,
    /// SvelteKit.
    SvelteKit,
    /// Astro.
    Astro,
    /// NestJS.
    NestJs,
    /// Django.
    Django,
    /// FastAPI.
    FastApi,
    /// Flask.
    Flask,
    /// Ruby on Rails.
    Rails,
    /// Laravel.
    Laravel,
    /// Phoenix.
    Phoenix,
    /// Spring Boot.
    SpringBoot,
    /// A Go module.
    Go,
    /// A Cargo package or workspace.
    Rust,
}

/// A framework found in one directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct FrameworkFact {
    /// Which framework.
    pub framework: Framework,
    /// Directory relative to the scan root.
    pub path: String,
    /// Built on Vite (Vite itself, React Router, Remix with Vite, SvelteKit, Astro).
    pub vite_based: bool,
    /// Supporting files and keys.
    pub evidence: Vec<String>,
}

// -- engines, migrations, drivers -------------------------------------------

/// A data or infrastructure service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    /// PostgreSQL, including PostGIS, pgvector and Timescale images.
    Postgres,
    /// MySQL.
    Mysql,
    /// MariaDB.
    Mariadb,
    /// Redis.
    Redis,
    /// Valkey.
    Valkey,
    /// Memcached.
    Memcached,
    /// MongoDB.
    Mongodb,
    /// ClickHouse.
    Clickhouse,
    /// MinIO (S3-compatible storage).
    Minio,
    /// RabbitMQ.
    Rabbitmq,
    /// Apache Kafka.
    Kafka,
    /// Redpanda.
    Redpanda,
    /// OpenSearch.
    Opensearch,
    /// Elasticsearch.
    Elasticsearch,
    /// Meilisearch.
    Meilisearch,
    /// Mailpit (SMTP sink).
    Mailpit,
    /// LocalStack (AWS emulation).
    Localstack,
    /// SQLite (a file, never a compose service).
    Sqlite,
    /// Microsoft SQL Server.
    SqlServer,
    /// Anything not recognized.
    Other,
}

/// An ORM or schema-migration tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationTool {
    /// Prisma.
    Prisma,
    /// Drizzle.
    Drizzle,
    /// Knex.
    Knex,
    /// TypeORM.
    TypeOrm,
    /// Sequelize.
    Sequelize,
    /// Alembic (including Flask-Migrate).
    Alembic,
    /// Django's own migrations.
    Django,
    /// Rails (`db/migrate`).
    Rails,
    /// Laravel (`database/migrations`).
    Laravel,
    /// Ecto (Elixir).
    Ecto,
    /// sqlx.
    Sqlx,
    /// Diesel.
    Diesel,
    /// goose.
    Goose,
    /// golang-migrate.
    GolangMigrate,
    /// Atlas.
    Atlas,
    /// dbmate.
    Dbmate,
    /// Flyway.
    Flyway,
    /// Liquibase.
    Liquibase,
}

/// A migration tool found in one directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct MigrationFact {
    /// Which tool.
    pub tool: MigrationTool,
    /// Directory relative to the scan root.
    pub path: String,
    /// Existing migration directories (empty when the tool is present but has no migrations yet).
    pub migrations_dirs: Vec<String>,
    /// Configuration or schema files.
    pub config_files: Vec<String>,
    /// Generated code directories inside the repository (the Prisma client `output`).
    pub generated_outputs: Vec<String>,
    /// The engine the tool is configured for, when it says so.
    pub database: Option<Engine>,
    /// Supporting files and keys.
    pub evidence: Vec<String>,
}

impl MigrationFact {
    /// Whether any migration directory exists.
    pub fn has_migrations(&self) -> bool {
        !self.migrations_dirs.is_empty()
    }
}

/// A database client dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct DriverFact {
    /// Engine the library talks to.
    pub engine: Engine,
    /// Supporting files and keys.
    pub evidence: Vec<String>,
}

// -- compose ----------------------------------------------------------------

/// A parsed compose file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ComposeFacts {
    /// The file that was parsed.
    pub file: String,
    /// Services, sorted by name.
    pub services: Vec<ComposeService>,
    /// Top-level `volumes:` definitions, sorted by key.
    pub volumes: Vec<ComposeVolume>,
    /// `volumes` lists every volume the project can have. False when a
    /// definition could not be read as data, or when part of the project lives
    /// elsewhere (`include:`, an override file): then a volume may be shared
    /// in a way these facts do not show.
    pub volumes_complete: bool,
}

/// A top-level compose volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ComposeVolume {
    /// The key under `volumes:` (what services mount by).
    pub name: String,
    /// `name:` is set: Docker uses that exact name, not one prefixed with
    /// `COMPOSE_PROJECT_NAME`, so every project that declares it shares it.
    pub explicit_name: bool,
    /// `external: true`: the volume exists outside Compose and is shared.
    pub external: bool,
}

impl ComposeVolume {
    /// The volume is the same Docker volume whatever `COMPOSE_PROJECT_NAME`
    /// says, so a checkout's other workspaces and the main checkout share it.
    pub fn shared(&self) -> bool {
        self.explicit_name || self.external
    }
}

/// One compose service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ComposeService {
    /// Service key.
    pub name: String,
    /// `image:` as written (after resolving `${VAR:-default}`).
    pub image: Option<String>,
    /// The service is built locally.
    pub build: bool,
    /// Engine guessed from the image.
    pub engine: Engine,
    /// Image variant: `postgis`, `pgvector`, `timescaledb`, `citus`, `supabase`.
    pub flavor: Option<String>,
    /// Image tag.
    pub tag: Option<String>,
    /// Major version taken from the tag (`postgres:16-alpine` is 16; `pg17` is 17).
    pub major_version: Option<u32>,
    /// Published ports.
    pub ports: Vec<PublishedPort>,
    /// `container_name`, which collides across copies of the project.
    pub container_name: Option<String>,
    /// Mounted volumes.
    pub volumes: Vec<VolumeMount>,
    /// Host sources mounted into `/docker-entrypoint-initdb.d`.
    pub init_scripts: Vec<String>,
    /// `environment` key names; values are not retained.
    pub environment_keys: Vec<String>,
    /// Compose profiles the service belongs to.
    pub profiles: Vec<String>,
    /// Supporting files and keys.
    pub evidence: Vec<String>,
}

/// A port published to the host (or only exposed with an ephemeral host port).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PublishedPort {
    /// Host port; `None` when Docker picks one or it comes from an unset variable.
    pub host: Option<u16>,
    /// Container port.
    pub container: u16,
}

/// A volume or bind mount.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct VolumeMount {
    /// Host path or named volume; `None` for anonymous volumes.
    pub source: Option<String>,
    /// Path inside the container.
    pub target: String,
    /// A host path (bind mount) rather than a named or anonymous volume.
    pub bind: bool,
}

// -- env --------------------------------------------------------------------

/// What kind of setting an environment variable name looks like: the kinds a
/// proposal acts on. Everything else is [`EnvCategory::Other`] and not reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvCategory {
    /// A connection URL: `DATABASE_URL`, `DIRECT_URL`, `POSTGRES_URL`, ...
    DatabaseUrl,
    /// `POSTGRES_*` and libpq `PG*` variables.
    Postgres,
    /// `MYSQL_*` and `MARIADB_*`.
    Mysql,
    /// Generic `DB_*`, `DATABASE_*` and `SPRING_DATASOURCE_*` parts.
    Database,
    /// `REDIS_*`, `VALKEY_*`, `CACHE_URL`.
    Redis,
    /// `PORT` and `*_PORT`: fixed ports collide between simultaneous worktrees.
    Port,
    /// Anything else. These names are not reported in [`EnvFacts::keys`].
    Other,
}

/// A variable name found in one or more `.env*` files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct EnvKey {
    /// The variable name.
    pub name: String,
    /// What it looks like.
    pub category: EnvCategory,
    /// Files that define it.
    pub files: Vec<String>,
}

/// `.env*` files and the variable names they define.
///
/// Only names are read; values are never stored.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct EnvFacts {
    /// The files read, relative to the root: the root's first, then project
    /// directories'; by name within a directory.
    pub files: Vec<String>,
    /// Names of the kinds a proposal acts on (everything except
    /// [`EnvCategory::Other`]), each with the files that define it, sorted by name.
    pub keys: Vec<EnvKey>,
}

impl EnvFacts {
    /// Whether any file defines `name`.
    pub fn has_key(&self, name: &str) -> bool {
        self.keys.iter().any(|k| k.name == name)
    }
}

// -- processes --------------------------------------------------------------

/// Where a process definition comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessSource {
    /// `Procfile` or `Procfile.dev`.
    Procfile,
    /// A `package.json` script.
    PackageJson,
    /// A mise task.
    MiseTask,
    /// A daemon already defined in `pitchfork.toml`.
    Pitchfork,
    /// Rails `bin/dev`.
    RailsBinDev,
    /// Django's development server.
    DjangoRunserver,
    /// Phoenix's server task.
    Phoenix,
    /// A Composer script (Laravel `composer run dev`).
    Composer,
    /// Spring Boot's Gradle/Maven run goal.
    SpringBoot,
}

/// What a process is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessRole {
    /// A development server or watcher.
    Dev,
    /// A server started the production way (`start`, `serve`, `web`).
    Serve,
    /// A background worker.
    Worker,
    /// A release-time command (Procfile `release`).
    Release,
    /// Anything else.
    Other,
}

/// How a process is started, without project-supplied command text.
///
/// This is the only field of a [`ProcessFact`] a proposal layer should build
/// commands from. It never carries script bodies, Procfile command lines or
/// anything else read from a repository that could embed a credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProcessLaunch {
    /// An exact argument vector, run without a shell. The program and
    /// subcommands are fixed by the scan; the only project-supplied element is
    /// a script or task *name*, passed as one argument. Use
    /// [`ProcessLaunch::shell_line`] to render it for a shell.
    Argv {
        /// Program first, then its arguments.
        argv: Vec<String>,
    },
    /// The command line of a `Procfile` entry. Its text is deliberately not
    /// copied into the facts (Procfile commands often inline credentials):
    /// read the line named `process` from `file` when it is needed.
    Procfile {
        /// Procfile path relative to the scan root.
        file: String,
        /// Process name, the part of the line before the colon.
        process: String,
    },
}

impl ProcessLaunch {
    /// The argument vector as a POSIX shell command line, every word quoted so
    /// that script names containing spaces or metacharacters stay one
    /// argument. `None` for [`ProcessLaunch::Procfile`].
    pub fn shell_line(&self) -> Option<String> {
        match self {
            ProcessLaunch::Argv { argv } => Some(
                argv.iter()
                    .map(|word| shell_quote(word))
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            ProcessLaunch::Procfile { .. } => None,
        }
    }
}

/// `word` quoted for a POSIX shell, unchanged when it is made only of
/// characters the shell never treats specially.
pub(crate) fn shell_quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '_' | '-' | '.' | '/' | ':' | '@' | '%' | '+' | '=' | ',')
        });
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// A process that is part of working on the project.
///
/// Only [`ProcessFact::launch`] is meant to be turned into a command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ProcessFact {
    /// Name (Procfile entry, script name, task name, daemon name).
    pub name: String,
    /// How to start the process. `None` when only the name is known (pitchfork
    /// daemons).
    pub launch: Option<ProcessLaunch>,
    /// Directory relative to the scan root.
    pub cwd: String,
    /// Where it was found.
    pub source: ProcessSource,
    /// What it is for.
    pub role: ProcessRole,
    /// Supporting files and keys.
    pub evidence: Vec<String>,
}

// -- artifacts --------------------------------------------------------------

/// What an existing path is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// `node_modules`.
    NodeModules,
    /// PHP `vendor` (Composer).
    ComposerVendor,
    /// Ruby `vendor/bundle`.
    BundlerVendor,
    /// Elixir `deps`.
    ElixirDeps,
    /// Compiled front-end assets (`app/assets/builds`, `public/build`).
    AssetBuilds,
    /// A secret key file (`config/master.key`, `config/credentials/*.key`).
    SecretKey,
    /// A `.env*` file.
    EnvFile,
    /// Generated code inside the repository (Prisma client output).
    GeneratedCode,
    /// A Python virtual environment.
    PythonVenv,
    /// `.next`.
    NextCache,
    /// Cargo's `target`.
    CargoTarget,
    /// `tmp`.
    Tmp,
    /// `log`.
    Log,
    /// Laravel `bootstrap/cache`.
    BootstrapCache,
    /// Laravel `storage`.
    Storage,
}

/// Whether cloning an existing path into a new worktree is advisable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyRecommendation {
    /// Expensive to rebuild and safe to clone.
    Recommended,
    /// Present, but cloning it is wrong or wasteful; see `reason`.
    NotRecommended,
}

/// An existing path that may be cloned into a new worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ArtifactFact {
    /// Path relative to the root.
    pub path: String,
    /// What it is.
    pub kind: ArtifactKind,
    /// Advice for the proposal layer.
    pub recommendation: CopyRecommendation,
    /// Why, in one sentence.
    pub reason: String,
    /// Matched by Git ignore rules and not tracked. Decided from the
    /// `.gitignore` files in the working tree and `$GIT_DIR/info/exclude`
    /// (the user's global excludes file is not read, so a path ignored only
    /// there reports `Some(false)`). `None` when the root is not a Git
    /// checkout, or when an input could not be read safely and completely
    /// (a FIFO or oversized `.gitignore`, an index that cannot be parsed).
    pub git_ignored: Option<bool>,
    /// Supporting files and keys.
    pub evidence: Vec<String>,
}

impl ArtifactFact {
    /// Recommended and known to be ignored: a plain candidate for `worktree.copy`.
    pub fn is_copy_candidate(&self) -> bool {
        self.recommendation == CopyRecommendation::Recommended && self.git_ignored == Some(true)
    }
}

// -- wiring -----------------------------------------------------------------

/// `pitchfork.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PitchforkFacts {
    /// The file read.
    pub file: String,
    /// Daemon names, sorted.
    pub daemons: Vec<String>,
    /// Supporting files and keys.
    pub evidence: Vec<String>,
}

/// Configuration devflow, pitchfork and mise already have in the repository.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct WiringFacts {
    /// `devflow.toml` or `.devflow.toml`.
    pub devflow_config: Option<String>,
    /// `devflow.local.toml`.
    pub devflow_local_config: Option<String>,
    /// devflow 0.x files (`.devflow.yml` and friends).
    pub legacy_devflow_files: Vec<String>,
    /// `pitchfork.toml`.
    pub pitchfork: Option<PitchforkFacts>,
    /// mise's `[env]` loads this workspace's `.devflow/env`: the path is
    /// written relative to the mise config (`.devflow/env`, `./.devflow/env`)
    /// or as `{{config_root}}/.devflow/env`.
    pub mise_loads_devflow_env: bool,
    /// `[env] _.file` entries that mention `.devflow/env` but are not anchored
    /// to this workspace (an absolute path to another checkout, a template
    /// other than `config_root`). They are reported as written and do not
    /// count as wired.
    pub mise_env_unresolved: Vec<String>,
    /// `[env] _.file` / `_.source` entries that come after the last time this
    /// workspace's `.devflow/env` is loaded, in the order mise applies them
    /// (lower config layers first, then list order; later entries win; a
    /// repeated entry counts every time it appears). Every one is taken to
    /// override: which variables a file sets is not read. Empty when
    /// `.devflow/env` is last or is not listed. Only entries of files mise
    /// always reads; see `mise_env_overridden_when_selected`.
    pub mise_env_overridden_by: Vec<MiseEnvEntry>,
    /// Entries after `.devflow/env` that sit in a `mise.<env>.toml` file: they
    /// override it only when that `MISE_ENV` is selected, which a scan of the
    /// repository cannot tell.
    pub mise_env_overridden_when_selected: Vec<MiseEnvEntry>,
}
