//! ORMs and schema-migration tools.
//!
//! Detection is by configuration files, conventional migration directories
//! and dependency names. Prisma schemas are parsed as text (never run) to
//! find the datasource provider and any generated-client `output` that lives
//! inside the repository.
use super::facts::{Engine, Framework, FrameworkFact, MigrationFact, MigrationTool};
use super::manifest::{Discovery, Project, parse_toml};
use super::push_unique;
use super::repo::{Repo, is_plain_relative, join};

/// Directories where SQL-file migration tools conventionally keep migrations.
const SQL_DIRS: [&str; 8] = [
    "db/migrations",
    "migrations",
    "sql/schema",
    "sql/migrations",
    "internal/db/migrations",
    "database/migrations",
    "db/migrate",
    "db/sql",
];

pub(super) fn detect(
    repo: &Repo,
    discovery: &Discovery,
    frameworks: &[FrameworkFact],
) -> Vec<MigrationFact> {
    let mut out = Vec::new();
    for project in &discovery.projects {
        let ctx = Ctx { repo, project };
        prisma(&ctx, &mut out);
        drizzle(&ctx, &mut out);
        js_migrators(&ctx, &mut out);
        alembic(&ctx, &mut out);
        let is_django = frameworks
            .iter()
            .any(|f| f.framework == Framework::Django && f.path == project.path);
        if is_django {
            django(&ctx, &mut out);
        }
        rails(&ctx, &mut out);
        laravel(&ctx, &mut out);
        ecto(&ctx, &mut out);
        sqlx(&ctx, &mut out);
        diesel(&ctx, &mut out);
        sql_file_tools(&ctx, &mut out);
        jvm_tools(&ctx, &mut out);
    }
    out
}

struct Ctx<'a> {
    repo: &'a Repo,
    project: &'a Project,
}

impl Ctx<'_> {
    fn new_fact(&self, tool: MigrationTool) -> MigrationFact {
        MigrationFact {
            tool,
            path: self.project.path.clone(),
            migrations_dirs: Vec::new(),
            config_files: Vec::new(),
            generated_outputs: Vec::new(),
            offline_data: None,
            database: None,
            evidence: Vec::new(),
        }
    }

    /// Path of `rel` (relative to the project) if it is a directory.
    fn dir(&self, rel: &str) -> Option<String> {
        let path = self.project.file(rel);
        self.repo.is_dir(&path).then_some(path)
    }

    /// The first of `names` that is a file in the project directory.
    fn first_file(&self, names: &[&str]) -> Option<String> {
        self.project
            .dir
            .first_file(names)
            .map(|n| self.project.file(n))
    }

    /// Config files present among `names`.
    fn files(&self, names: &[&str]) -> Vec<String> {
        names
            .iter()
            .filter(|n| self.project.has_file(n))
            .map(|n| self.project.file(n))
            .collect()
    }
}

/// Lexically resolve `written` against the repo-relative directory `base`.
/// Absolute paths and paths climbing out of the repository yield `None`.
fn resolve_inside(base: &str, written: &str) -> Option<String> {
    let written = written.trim();
    if written.is_empty()
        || written.starts_with('/')
        || written.contains('\\')
        || written.contains(':')
    {
        return None;
    }
    let mut parts: Vec<&str> = if base.is_empty() || base == "." {
        Vec::new()
    } else {
        base.split('/').collect()
    };
    for segment in written.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    (!joined.is_empty() && is_plain_relative(&joined)).then_some(joined)
}

/// The quoted string value following `key` as in `key: "value"`, `key = 'value'`
/// or `"key": "value"`. Good enough to read simple JS/TS/JSON/INI configs
/// without evaluating them.
pub(super) fn quoted_value(text: &str, key: &str) -> Option<String> {
    for (at, _) in text.match_indices(key) {
        let before = text[..at].chars().next_back();
        if before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.') {
            continue;
        }
        let mut rest = text[at + key.len()..].trim_start_matches(['"', '\'']);
        rest = rest.trim_start();
        let Some(after) = rest.strip_prefix([':', '=']) else {
            continue;
        };
        let after = after.trim_start();
        let Some(quote) = after
            .chars()
            .next()
            .filter(|c| matches!(c, '"' | '\'' | '`'))
        else {
            continue;
        };
        if let Some(end) = after[1..].find(quote) {
            return Some(after[1..1 + end].to_string());
        }
    }
    None
}

fn engine_from_dialect(value: &str) -> Option<Engine> {
    match value.to_ascii_lowercase().as_str() {
        "postgresql" | "postgres" | "pg" | "cockroachdb" => Some(Engine::Postgres),
        "mysql" | "mysql2" | "planetscale" => Some(Engine::Mysql),
        "mariadb" => Some(Engine::Mariadb),
        "sqlite" | "sqlite3" | "turso" | "libsql" => Some(Engine::Sqlite),
        "sqlserver" | "mssql" => Some(Engine::SqlServer),
        "mongodb" => Some(Engine::Mongodb),
        _ => None,
    }
}

// -- Prisma -------------------------------------------------------------------

#[derive(Default)]
struct PrismaSchema {
    provider: Option<String>,
    outputs: Vec<String>,
}

/// Read `datasource` providers and `generator` outputs from schema text.
fn parse_prisma_schema(text: &str, schema_dir: &str, outputs: &mut Vec<String>) -> PrismaSchema {
    #[derive(PartialEq)]
    enum Block {
        None,
        Datasource,
        Generator,
    }
    let mut block = Block::None;
    let mut parsed = PrismaSchema::default();
    for raw in text.lines() {
        let line = raw.split("//").next().unwrap_or("").trim();
        if block == Block::None {
            if line.starts_with("datasource ") && line.ends_with('{') {
                block = Block::Datasource;
            } else if line.starts_with("generator ") && line.ends_with('{') {
                block = Block::Generator;
            }
            continue;
        }
        if line.starts_with('}') {
            block = Block::None;
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        let Some(quoted) = value.strip_prefix('"').and_then(|v| v.split('"').next()) else {
            continue; // `env("X")` and other expressions are not literal values.
        };
        match (&block, key.trim()) {
            (Block::Datasource, "provider") => parsed.provider = Some(quoted.to_string()),
            (Block::Generator, "output") => {
                if let Some(path) = resolve_inside(schema_dir, quoted) {
                    push_unique(outputs, path.clone());
                    parsed.outputs.push(path);
                }
            }
            _ => {}
        }
    }
    parsed
}

fn prisma(ctx: &Ctx, out: &mut Vec<MigrationFact>) {
    let project = ctx.project;
    let config_files = ctx.files(&[
        "prisma.config.ts",
        "prisma.config.js",
        "prisma.config.mjs",
        "prisma.config.cjs",
        "prisma.config.mts",
        "prisma.config.cts",
    ]);

    // Candidate schema locations, most specific first.
    let mut candidates: Vec<String> = Vec::new();
    if let Some(path) = project
        .js
        .as_ref()
        .and_then(|m| m.prisma_schema.as_deref())
        .and_then(|p| resolve_inside(&project.path, p))
    {
        candidates.push(path);
    }
    for config in &config_files {
        if let Some(path) = ctx
            .repo
            .read_text(config)
            .and_then(|text| quoted_value(&text, "schema"))
            .and_then(|p| resolve_inside(&project.path, &p))
        {
            candidates.push(path);
        }
    }
    for rel in ["prisma/schema.prisma", "schema.prisma", "prisma/schema"] {
        candidates.push(project.file(rel));
    }

    let mut fact = ctx.new_fact(MigrationTool::Prisma);
    let mut schema_dir: Option<String> = None;
    let mut texts: Vec<(String, String)> = Vec::new();
    for candidate in candidates {
        if ctx.repo.is_file(&candidate) {
            if let Some(text) = ctx.repo.read_text(&candidate) {
                texts.push((candidate.clone(), text));
            }
            schema_dir = candidate.rsplit_once('/').map(|(d, _)| d.to_string());
            if schema_dir.is_none() {
                schema_dir = Some(project.path.clone());
            }
            fact.config_files.push(candidate);
            break;
        }
        if ctx.repo.is_dir(&candidate) {
            // Multi-file schema folder.
            let index = ctx.repo.list(&candidate);
            for name in index.files().filter(|n| n.ends_with(".prisma")).take(20) {
                let file = join(&candidate, name);
                if let Some(text) = ctx.repo.read_text(&file) {
                    texts.push((file.clone(), text));
                }
                fact.config_files.push(file);
            }
            if !fact.config_files.is_empty() {
                schema_dir = Some(candidate);
                break;
            }
        }
    }
    let dependency = ["prisma", "@prisma/client"]
        .iter()
        .find_map(|d| project.js_deps.evidence(d));
    if fact.config_files.is_empty() && config_files.is_empty() && dependency.is_none() {
        return;
    }

    for (file, text) in &texts {
        let dir = file.rsplit_once('/').map_or(".", |(d, _)| d);
        let parsed = parse_prisma_schema(text, dir, &mut fact.generated_outputs);
        if fact.database.is_none() {
            fact.database = parsed.provider.as_deref().and_then(engine_from_dialect);
        }
    }
    if let Some(dir) = &schema_dir {
        // `prisma/schema/*.prisma` keeps migrations next to the folder.
        let mut migrations = vec![join(dir, "migrations")];
        if let Some((parent, _)) = dir.rsplit_once('/') {
            migrations.push(join(parent, "migrations"));
        }
        for path in migrations {
            if ctx.repo.is_dir(&path) && !fact.migrations_dirs.contains(&path) {
                fact.migrations_dirs.push(path);
            }
        }
    }
    fact.evidence.extend(fact.config_files.clone());
    for config in config_files {
        fact.evidence.push(config.clone());
        fact.config_files.push(config);
    }
    fact.evidence.extend(fact.migrations_dirs.clone());
    fact.evidence.extend(dependency);
    out.push(fact);
}

// -- JavaScript ORMs ------------------------------------------------------------

fn drizzle(ctx: &Ctx, out: &mut Vec<MigrationFact>) {
    let configs = ctx.files(&[
        "drizzle.config.ts",
        "drizzle.config.js",
        "drizzle.config.mjs",
        "drizzle.config.cjs",
        "drizzle.config.mts",
        "drizzle.config.cts",
        "drizzle.config.json",
    ]);
    let dependency = ["drizzle-kit", "drizzle-orm"]
        .iter()
        .find_map(|d| ctx.project.js_deps.evidence(d));
    if configs.is_empty() && dependency.is_none() {
        return;
    }
    let mut fact = ctx.new_fact(MigrationTool::Drizzle);
    let mut out_dirs: Vec<String> = Vec::new();
    for config in &configs {
        let Some(text) = ctx.repo.read_text(config) else {
            continue;
        };
        if fact.database.is_none() {
            fact.database = quoted_value(&text, "dialect")
                .as_deref()
                .and_then(engine_from_dialect);
        }
        if let Some(path) =
            quoted_value(&text, "out").and_then(|p| resolve_inside(&ctx.project.path, &p))
        {
            out_dirs.push(path);
        }
    }
    out_dirs.push(ctx.project.file("drizzle"));
    for dir in out_dirs {
        if ctx.repo.is_dir(&dir) && !fact.migrations_dirs.contains(&dir) {
            fact.migrations_dirs.push(dir);
        }
    }
    fact.config_files = configs;
    fact.evidence.extend(fact.config_files.clone());
    fact.evidence.extend(fact.migrations_dirs.clone());
    fact.evidence.extend(dependency);
    out.push(fact);
}

/// Knex, TypeORM and Sequelize.
fn js_migrators(ctx: &Ctx, out: &mut Vec<MigrationFact>) {
    if ctx.project.js.is_none() {
        return;
    }
    let project = ctx.project;

    // Knex.
    let knexfile = ctx.first_file(&["knexfile.js", "knexfile.ts", "knexfile.cjs", "knexfile.mjs"]);
    let knex_dep = project.js_deps.evidence("knex");
    if knexfile.is_some() || knex_dep.is_some() {
        let mut fact = ctx.new_fact(MigrationTool::Knex);
        let mut dirs: Vec<String> = Vec::new();
        if let Some(file) = &knexfile
            && let Some(dir) = ctx
                .repo
                .read_text(file)
                .and_then(|t| quoted_value(&t, "directory"))
                .and_then(|p| resolve_inside(&project.path, &p))
        {
            dirs.push(dir);
        }
        for rel in [
            "migrations",
            "db/migrations",
            "src/migrations",
            "database/migrations",
        ] {
            dirs.push(project.file(rel));
        }
        fact.migrations_dirs = existing_dirs(ctx.repo, dirs);
        fact.config_files.extend(knexfile);
        fact.evidence.extend(fact.config_files.clone());
        fact.evidence.extend(knex_dep);
        out.push(fact);
    }

    // TypeORM.
    let ormconfig = ctx.first_file(&[
        "ormconfig.json",
        "ormconfig.js",
        "ormconfig.ts",
        "ormconfig.yml",
        "ormconfig.yaml",
        "data-source.ts",
    ]);
    let typeorm_dep = project.js_deps.evidence("typeorm");
    let datasource = ["src/data-source.ts", "src/data-source.js"]
        .iter()
        .find(|f| ctx.repo.is_file(&project.file(f)))
        .map(|f| project.file(f));
    if typeorm_dep.is_some() || ormconfig.is_some() {
        let mut fact = ctx.new_fact(MigrationTool::TypeOrm);
        fact.migrations_dirs = existing_dirs(
            ctx.repo,
            [
                "src/migrations",
                "migrations",
                "src/database/migrations",
                "src/db/migrations",
            ]
            .iter()
            .map(|r| project.file(r))
            .collect(),
        );
        fact.config_files.extend(ormconfig.or(datasource));
        fact.evidence.extend(fact.config_files.clone());
        fact.evidence.extend(typeorm_dep);
        out.push(fact);
    }

    // Sequelize.
    let sequelizerc = ctx.first_file(&[".sequelizerc"]);
    let sequelize_dep = ["sequelize", "sequelize-cli"]
        .iter()
        .find_map(|d| project.js_deps.evidence(d));
    if sequelizerc.is_some() || sequelize_dep.is_some() {
        let mut fact = ctx.new_fact(MigrationTool::Sequelize);
        let mut dirs = Vec::new();
        if let Some(rc) = &sequelizerc
            && let Some(dir) = ctx
                .repo
                .read_text(rc)
                .and_then(|t| quoted_value(&t, "migrations-path"))
                .and_then(|p| resolve_inside(&project.path, &p))
        {
            dirs.push(dir);
        }
        for rel in [
            "migrations",
            "db/migrations",
            "src/migrations",
            "database/migrations",
        ] {
            dirs.push(project.file(rel));
        }
        fact.migrations_dirs = existing_dirs(ctx.repo, dirs);
        fact.config_files.extend(sequelizerc);
        fact.evidence.extend(fact.config_files.clone());
        fact.evidence.extend(sequelize_dep);
        out.push(fact);
    }
}

/// The directories among `candidates` that exist, without repeats.
fn existing_dirs(repo: &Repo, candidates: Vec<String>) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for dir in candidates {
        if !found.contains(&dir) && repo.is_dir(&dir) {
            found.push(dir);
        }
    }
    found
}

// -- Python ---------------------------------------------------------------------

fn alembic(ctx: &Ctx, out: &mut Vec<MigrationFact>) {
    let project = ctx.project;
    let mut configs = ctx.files(&["alembic.ini"]);
    // Flask-Migrate keeps everything under `migrations/`.
    let flask_ini = project.file("migrations/alembic.ini");
    if project.has_dir("migrations") && ctx.repo.is_file(&flask_ini) {
        configs.push(flask_ini);
    }
    let env_py = project.has_dir("alembic") && ctx.repo.is_file(&project.file("alembic/env.py"));
    let dependency = ["alembic", "flask-migrate"]
        .iter()
        .find_map(|d| project.py.evidence(d));
    if configs.is_empty() && !env_py && dependency.is_none() {
        return;
    }
    let mut fact = ctx.new_fact(MigrationTool::Alembic);
    let mut dirs: Vec<String> = Vec::new();
    for config in &configs {
        let script_location = ctx.repo.read_text(config).and_then(|text| {
            text.lines().find_map(|line| {
                let (key, value) = line.split_once('=')?;
                (key.trim() == "script_location").then(|| value.trim().to_string())
            })
        });
        if let Some(location) = script_location {
            let location = location.replace("%(here)s", ".");
            let base = config
                .rsplit_once('/')
                .map_or(project.path.as_str(), |(d, _)| d);
            if let Some(dir) = resolve_inside(base, &location) {
                dirs.push(join(&dir, "versions"));
            }
        }
    }
    for rel in [
        "alembic/versions",
        "migrations/versions",
        "src/alembic/versions",
    ] {
        dirs.push(project.file(rel));
    }
    fact.migrations_dirs = existing_dirs(ctx.repo, dirs);
    fact.config_files = configs;
    if env_py {
        fact.config_files.push(project.file("alembic/env.py"));
    }
    fact.evidence.extend(fact.config_files.clone());
    fact.evidence.extend(fact.migrations_dirs.clone());
    fact.evidence.extend(dependency);
    out.push(fact);
}

fn django(ctx: &Ctx, out: &mut Vec<MigrationFact>) {
    const MAX_DIRS: usize = 12;
    let project = ctx.project;
    let mut dirs: Vec<String> = Vec::new();
    // `app/migrations`, plus `apps/*/migrations` and `src/*/migrations`.
    let mut parents: Vec<String> = vec![project.path.clone()];
    for container in ["apps", "src", "backend"] {
        if project.has_dir(container) {
            parents.push(project.file(container));
        }
    }
    'outer: for parent in parents {
        let index = ctx.repo.list(&parent);
        for name in index.dirs().filter(|n| !n.starts_with('.')).take(80) {
            let migrations = join(&join(&parent, name), "migrations");
            if migrations_package(ctx.repo, &migrations) {
                dirs.push(migrations);
                if dirs.len() >= MAX_DIRS {
                    break 'outer;
                }
            }
        }
    }
    if dirs.is_empty() {
        return;
    }
    let mut fact = ctx.new_fact(MigrationTool::Django);
    fact.config_files.push(project.file("manage.py"));
    fact.evidence.extend(dirs.clone());
    fact.migrations_dirs = dirs;
    out.push(fact);
}

/// A Python `migrations` package: an `__init__.py` or numbered migration files.
fn migrations_package(repo: &Repo, dir: &str) -> bool {
    if !repo.is_dir(dir) {
        return false;
    }
    let index = repo.list(dir);
    index.has_file("__init__.py")
        || index
            .files()
            .any(|n| n.ends_with(".py") && n.starts_with(|c: char| c.is_ascii_digit()))
}

// -- Rails, Laravel, Ecto -----------------------------------------------------------

fn rails(ctx: &Ctx, out: &mut Vec<MigrationFact>) {
    if !ctx.project.has_dir("db") {
        return;
    }
    let Some(dir) = ctx.dir("db/migrate") else {
        return;
    };
    let mut fact = ctx.new_fact(MigrationTool::Rails);
    fact.config_files = ctx.files(&["db/schema.rb", "db/structure.sql"]);
    for rel in ["db/schema.rb", "db/structure.sql", "config/database.yml"] {
        let path = ctx.project.file(rel);
        if ctx.repo.is_file(&path) {
            push_unique(&mut fact.config_files, path);
        }
    }
    fact.evidence.push(dir.clone());
    fact.evidence.extend(fact.config_files.clone());
    fact.migrations_dirs.push(dir);
    out.push(fact);
}

fn laravel(ctx: &Ctx, out: &mut Vec<MigrationFact>) {
    if !ctx.project.php.present() || !ctx.project.has_dir("database") {
        return;
    }
    let Some(dir) = ctx.dir("database/migrations") else {
        return;
    };
    let mut fact = ctx.new_fact(MigrationTool::Laravel);
    fact.evidence.push(dir.clone());
    fact.migrations_dirs.push(dir);
    out.push(fact);
}

fn ecto(ctx: &Ctx, out: &mut Vec<MigrationFact>) {
    let project = ctx.project;
    let dependency = ["ecto_sql", "ecto"]
        .iter()
        .find_map(|d| project.ex.evidence(d));
    let mut dirs = Vec::new();
    if project.has_dir("priv") {
        let index = ctx.repo.list(&project.file("priv"));
        for name in index.dirs().take(10) {
            let migrations = join(&project.file("priv"), &format!("{name}/migrations"));
            if ctx.repo.is_dir(&migrations) {
                dirs.push(migrations);
            }
        }
    }
    if dependency.is_none() && dirs.is_empty() {
        return;
    }
    let mut fact = ctx.new_fact(MigrationTool::Ecto);
    fact.evidence.extend(dirs.clone());
    fact.evidence.extend(dependency);
    fact.migrations_dirs = dirs;
    if let Some(dep) = project.ex.iter().find_map(|(name, _)| match name {
        "postgrex" => Some(Engine::Postgres),
        "myxql" => Some(Engine::Mysql),
        "ecto_sqlite3" | "exqlite" => Some(Engine::Sqlite),
        _ => None,
    }) {
        fact.database = Some(dep);
    }
    out.push(fact);
}

// -- Rust -----------------------------------------------------------------------------

fn engine_from_features(features: &[String]) -> Option<Engine> {
    features.iter().find_map(|f| {
        let f = f.to_ascii_lowercase();
        if f.contains("postgres") {
            Some(Engine::Postgres)
        } else if f.contains("mysql") {
            Some(Engine::Mysql)
        } else if f.contains("sqlite") {
            Some(Engine::Sqlite)
        } else {
            None
        }
    })
}

fn sqlx(ctx: &Ctx, out: &mut Vec<MigrationFact>) {
    let project = ctx.project;
    let dependency = project.rs.evidence("sqlx");
    let offline = ctx.dir(".sqlx");
    if dependency.is_none() && offline.is_none() {
        return;
    }
    let mut fact = ctx.new_fact(MigrationTool::Sqlx);
    fact.migrations_dirs.extend(ctx.dir("migrations"));
    fact.config_files = ctx.files(&["sqlx.toml"]);
    fact.database = engine_from_features(project.rs.features("sqlx"));
    fact.evidence.extend(fact.migrations_dirs.clone());
    fact.evidence.extend(offline.clone());
    fact.evidence.extend(dependency);
    fact.offline_data = offline;
    out.push(fact);
}

fn diesel(ctx: &Ctx, out: &mut Vec<MigrationFact>) {
    let project = ctx.project;
    let dependency = project.rs.evidence("diesel");
    let config = ctx.first_file(&["diesel.toml"]);
    if dependency.is_none() && config.is_none() {
        return;
    }
    let mut fact = ctx.new_fact(MigrationTool::Diesel);
    let mut dirs: Vec<String> = Vec::new();
    if let Some(file) = &config
        && let Some(table) = ctx
            .repo
            .read_text(file)
            .and_then(|text| parse_toml(ctx.repo, file, &text))
        && let Some(dir) = table
            .get("migrations_directory")
            .and_then(|m| m.get("dir"))
            .and_then(toml::Value::as_str)
            .and_then(|p| resolve_inside(&project.path, p))
    {
        dirs.push(dir);
    }
    dirs.push(project.file("migrations"));
    fact.migrations_dirs = existing_dirs(ctx.repo, dirs);
    fact.database = engine_from_features(project.rs.features("diesel"));
    fact.config_files.extend(config);
    fact.evidence.extend(fact.config_files.clone());
    fact.evidence.extend(fact.migrations_dirs.clone());
    fact.evidence.extend(dependency);
    out.push(fact);
}

// -- SQL-file tools: goose, golang-migrate, Atlas, dbmate -------------------------------

/// What the conventional SQL migration directories reveal.
#[derive(Default)]
struct SqlDirs {
    goose: Vec<String>,
    dbmate: Vec<String>,
    golang_migrate: Vec<String>,
    atlas: Vec<String>,
}

fn scan_sql_dirs(ctx: &Ctx) -> SqlDirs {
    let mut found = SqlDirs::default();
    for rel in SQL_DIRS {
        let Some(dir) = ctx.dir(rel) else { continue };
        let index = ctx.repo.list(&dir);
        if index.has_file("atlas.sum") {
            found.atlas.push(dir.clone());
        }
        if index.files().any(|n| n.ends_with(".up.sql")) {
            found.golang_migrate.push(dir.clone());
        }
        let first_sql = index.files().find(|n| n.ends_with(".sql"));
        if let Some(head) = first_sql.and_then(|n| ctx.repo.read_head(&join(&dir, n), 4096)) {
            if head.contains("+goose") {
                found.goose.push(dir.clone());
            }
            if head.contains("migrate:up") {
                found.dbmate.push(dir.clone());
            }
        }
    }
    found
}

fn sql_file_tools(ctx: &Ctx, out: &mut Vec<MigrationFact>) {
    let project = ctx.project;
    let dirs = scan_sql_dirs(ctx);

    let goose_dep = project
        .go
        .with_prefix("github.com/pressly/goose")
        .map(|(_, e)| e.to_string());
    if goose_dep.is_some() || !dirs.goose.is_empty() {
        let mut fact = ctx.new_fact(MigrationTool::Goose);
        fact.evidence.extend(dirs.goose.clone());
        fact.evidence.extend(goose_dep);
        fact.migrations_dirs = dirs.goose;
        out.push(fact);
    }

    let migrate_dep = project
        .go
        .with_prefix("github.com/golang-migrate/migrate")
        .map(|(_, e)| e.to_string());
    if migrate_dep.is_some() || !dirs.golang_migrate.is_empty() {
        let mut fact = ctx.new_fact(MigrationTool::GolangMigrate);
        fact.evidence.extend(dirs.golang_migrate.clone());
        fact.evidence.extend(migrate_dep);
        fact.migrations_dirs = dirs.golang_migrate;
        out.push(fact);
    }

    let atlas_config = ctx.first_file(&["atlas.hcl"]);
    let atlas_dep = project
        .go
        .with_prefix("ariga.io/atlas")
        .map(|(_, e)| e.to_string());
    if atlas_config.is_some() || atlas_dep.is_some() || !dirs.atlas.is_empty() {
        let mut fact = ctx.new_fact(MigrationTool::Atlas);
        fact.config_files.extend(atlas_config);
        fact.evidence.extend(fact.config_files.clone());
        fact.evidence.extend(dirs.atlas.clone());
        fact.evidence.extend(atlas_dep);
        fact.migrations_dirs = dirs.atlas;
        out.push(fact);
    }

    let dbmate_dep = project.js_deps.evidence("dbmate");
    if dbmate_dep.is_some() || !dirs.dbmate.is_empty() {
        let mut fact = ctx.new_fact(MigrationTool::Dbmate);
        fact.evidence.extend(dirs.dbmate.clone());
        fact.evidence.extend(dbmate_dep);
        fact.migrations_dirs = dirs.dbmate;
        out.push(fact);
    }
}

// -- Flyway and Liquibase ---------------------------------------------------------------

fn jvm_tools(ctx: &Ctx, out: &mut Vec<MigrationFact>) {
    let project = ctx.project;

    let flyway_config = ctx.files(&["flyway.conf", "flyway.toml", "conf/flyway.conf"]);
    let flyway_build = project.jvm.mentioning("flyway");
    let flyway_dirs = existing_dirs(
        ctx.repo,
        ["src/main/resources/db/migration", "db/migration"]
            .iter()
            .map(|r| project.file(r))
            .collect(),
    );
    if !flyway_config.is_empty() || !flyway_build.is_empty() {
        let mut fact = ctx.new_fact(MigrationTool::Flyway);
        fact.evidence.extend(flyway_config.clone());
        fact.evidence.extend(flyway_build);
        fact.evidence.extend(flyway_dirs.clone());
        fact.config_files = flyway_config;
        fact.migrations_dirs = flyway_dirs;
        out.push(fact);
    }

    let liquibase_config = ctx.files(&["liquibase.properties"]);
    let liquibase_build = project.jvm.mentioning("liquibase");
    let liquibase_dirs = existing_dirs(
        ctx.repo,
        ["src/main/resources/db/changelog", "db/changelog"]
            .iter()
            .map(|r| project.file(r))
            .collect(),
    );
    if !liquibase_config.is_empty() || !liquibase_build.is_empty() {
        let mut fact = ctx.new_fact(MigrationTool::Liquibase);
        fact.evidence.extend(liquibase_config.clone());
        fact.evidence.extend(liquibase_build);
        fact.evidence.extend(liquibase_dirs.clone());
        fact.config_files = liquibase_config;
        fact.migrations_dirs = liquibase_dirs;
        out.push(fact);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_values_in_configs() {
        assert_eq!(
            quoted_value(
                "export default { out: './drizzle', dialect: \"postgresql\" }",
                "out"
            ),
            Some("./drizzle".into())
        );
        assert_eq!(
            quoted_value("{ \"out\": \"db\" }", "out"),
            Some("db".into())
        );
        assert_eq!(quoted_value("without: 'x'", "out"), None);
        assert_eq!(quoted_value("out: process.env.OUT", "out"), None);
    }

    #[test]
    fn paths_stay_inside_the_repository() {
        assert_eq!(
            resolve_inside("prisma", "../generated/client"),
            Some("generated/client".into())
        );
        assert_eq!(
            resolve_inside("prisma", "./client"),
            Some("prisma/client".into())
        );
        assert_eq!(resolve_inside("prisma", "../../outside"), None);
        assert_eq!(resolve_inside(".", "../outside"), None);
        assert_eq!(resolve_inside("prisma", "/etc/passwd"), None);
        assert_eq!(
            resolve_inside("apps/web/prisma", "../../generated"),
            Some("apps/generated".into())
        );
        assert_eq!(
            resolve_inside("apps/web/prisma", "../../../generated"),
            Some("generated".into())
        );
        assert_eq!(resolve_inside("apps/web/prisma", "../../../../x"), None);
    }

    #[test]
    fn prisma_schema_blocks() {
        let schema = r#"
generator client {
  provider = "prisma-client-js"
  output   = "../src/generated/prisma"
}
generator docs {
  provider = "x"
  output   = env("DOCS_OUT")
}
datasource db {
  provider = "postgresql" // comment
  url      = env("DATABASE_URL")
}
"#;
        let mut outputs = Vec::new();
        let parsed = parse_prisma_schema(schema, "prisma", &mut outputs);
        assert_eq!(parsed.provider.as_deref(), Some("postgresql"));
        assert_eq!(outputs, vec!["src/generated/prisma".to_string()]);
    }
}
