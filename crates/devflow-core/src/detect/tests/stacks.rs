//! Whole-repository scenarios: realistic stacks end to end.
use super::*;

const SENTINEL: &str = "VALUE_MUST_NOT_LEAK";

#[test]
fn bun_next_prisma_with_postgres_and_redis_compose() {
    let dir = fixture(&[
        (
            "package.json",
            r#"{
              "name": "web",
              "packageManager": "bun@1.1.30",
              "scripts": {
                "dev": "next dev",
                "build": "next build",
                "start": "next start",
                "worker": "bun run src/worker.ts",
                "db:migrate": "prisma migrate deploy",
                "postinstall": "prisma generate"
              },
              "dependencies": { "next": "15.0.0", "@prisma/client": "6.0.0", "pg": "8.11.0", "ioredis": "5.4.0" },
              "devDependencies": { "prisma": "6.0.0" }
            }"#,
        ),
        ("bun.lock", "{}"),
        ("next.config.ts", "export default {};"),
        (
            "prisma/schema.prisma",
            r#"generator client {
  provider = "prisma-client-js"
  output   = "../src/generated/prisma"
}

datasource db {
  provider = "postgresql"
  url      = env("DATABASE_URL")
  directUrl = env("DIRECT_URL")
}
"#,
        ),
        (
            "prisma/migrations/20240101000000_init/migration.sql",
            "select 1;",
        ),
        (
            "prisma/migrations/migration_lock.toml",
            "provider = \"postgresql\"",
        ),
        (
            "compose.yaml",
            r#"services:
  db:
    image: postgres:16-alpine
    container_name: web-db
    ports:
      - "5432:5432"
    environment:
      POSTGRES_USER: app
      POSTGRES_PASSWORD: VALUE_MUST_NOT_LEAK
      POSTGRES_DB: app
    volumes:
      - pgdata:/var/lib/postgresql/data
      - ./db/init:/docker-entrypoint-initdb.d:ro
  cache:
    image: redis:7
    ports:
      - 6379:6379
volumes:
  pgdata: {}
"#,
        ),
        (
            ".env.example",
            "DATABASE_URL=VALUE_MUST_NOT_LEAK\nDIRECT_URL=VALUE_MUST_NOT_LEAK\nREDIS_URL=x\nNEXTAUTH_SECRET=VALUE_MUST_NOT_LEAK\n",
        ),
        (".env", "DATABASE_URL=VALUE_MUST_NOT_LEAK\nPORT=3000\n"),
        (
            "mise.toml",
            r#"[tools]
node = "22"
bun = "latest"

[env]
_.file = ".devflow/env"

[tasks.dev]
run = "bun run dev"

[tasks."db:migrate"]
run = "bunx prisma migrate deploy"
"#,
        ),
        (".nvmrc", "22\n"),
        (
            "pitchfork.toml",
            "[daemons.web]\nrun = \"bun run dev\"\nmise = true\n\n[daemons.worker]\nrun = \"bun run worker\"\nmise = true\n",
        ),
        ("devflow.toml", "[project]\nname = \"web\"\n"),
        (".gitignore", "node_modules\n.env\n.next\nsrc/generated\n"),
        ("node_modules/.keep", ""),
        (".next/.keep", ""),
        ("src/generated/prisma/index.js", ""),
    ]);
    git_init(dir.path());
    let facts = scan_dir(&dir);

    // Package manager: the field wins, with its version.
    let js = facts.js_project().expect("root js project");
    assert_eq!(js.package_manager.kind, PackageManager::Bun);
    assert_eq!(js.package_manager.version.as_deref(), Some("1.1.30"));
    assert_eq!(
        js.package_manager.source,
        PackageManagerSource::PackageManagerField
    );
    assert_eq!(
        facts.js_install_command(),
        Some("bun install --frozen-lockfile")
    );
    let package = &js.packages[0];
    assert!(package.has_script("db:migrate"));
    assert_eq!(package.prisma_generate_scripts, vec!["postinstall"]);
    assert!(package.generates_prisma_on_install());

    // Framework and ORM.
    let next = framework(&facts, Framework::NextJs).expect("next");
    assert_eq!(next.path, ".");
    assert!(
        next.evidence
            .contains(&"package.json:dependencies.next".to_string())
    );
    assert!(next.evidence.contains(&"next.config.ts".to_string()));
    let prisma = migration(&facts, MigrationTool::Prisma).expect("prisma");
    assert!(prisma.has_migrations());
    assert_eq!(prisma.migrations_dirs, vec!["prisma/migrations"]);
    assert_eq!(prisma.generated_outputs, vec!["src/generated/prisma"]);
    assert_eq!(prisma.database, Some(Engine::Postgres));
    assert!(
        prisma
            .config_files
            .contains(&"prisma/schema.prisma".to_string())
    );
    assert!(
        facts
            .drivers
            .iter()
            .any(|d| d.engine == Engine::Postgres && d.dependency == "pg")
    );
    assert!(
        facts
            .drivers
            .iter()
            .any(|d| d.engine == Engine::Redis && d.dependency == "ioredis")
    );

    // Compose.
    let db = service(&facts, "db");
    assert_eq!(db.engine, Engine::Postgres);
    assert_eq!(db.major_version, Some(16));
    assert_eq!(db.container_name.as_deref(), Some("web-db"));
    assert_eq!(db.ports.len(), 1);
    assert_eq!(db.ports[0].host, Some(5432));
    assert_eq!(db.ports[0].container, 5432);
    assert_eq!(db.init_scripts, vec!["./db/init"]);
    assert_eq!(
        db.environment_keys,
        vec!["POSTGRES_DB", "POSTGRES_PASSWORD", "POSTGRES_USER"]
    );
    assert!(
        db.volumes
            .iter()
            .any(|v| v.source.as_deref() == Some("pgdata") && !v.bind)
    );
    let cache = service(&facts, "cache");
    assert_eq!(cache.engine, Engine::Redis);
    assert_eq!(cache.major_version, Some(7));
    assert_eq!(cache.ports[0].host, Some(6379));
    assert_eq!(facts.compose_services(Engine::Postgres).len(), 1);

    // .env: names only.
    assert!(facts.env.has_database_keys);
    let database_url = facts
        .env
        .keys
        .iter()
        .find(|k| k.name == "DATABASE_URL")
        .expect("DATABASE_URL");
    assert_eq!(database_url.category, EnvCategory::DatabaseUrl);
    assert_eq!(database_url.files, vec![".env", ".env.example"]);
    assert!(facts.env.has_key("DIRECT_URL"));
    assert!(facts.env.has_key("PORT"));
    assert!(
        !facts.env.has_key("NEXTAUTH_SECRET"),
        "only infrastructure names are kept"
    );

    // Processes.
    let dev = process(&facts, "dev", ProcessSource::PackageJson).expect("dev script");
    assert_eq!(dev.display.as_deref(), Some("bun run dev"));
    assert_eq!(dev.role, ProcessRole::Dev);
    let start = process(&facts, "start", ProcessSource::PackageJson).expect("start script");
    assert_eq!(start.role, ProcessRole::Serve);
    let worker = process(&facts, "worker", ProcessSource::PackageJson).expect("worker script");
    assert_eq!(worker.role, ProcessRole::Worker);
    assert!(process(&facts, "build", ProcessSource::PackageJson).is_none());
    assert!(process(&facts, "dev", ProcessSource::MiseTask).is_some());
    assert!(process(&facts, "web", ProcessSource::Pitchfork).is_some());

    // Copy candidates.
    let node_modules = artifact(&facts, "node_modules").expect("node_modules");
    assert_eq!(node_modules.kind, ArtifactKind::NodeModules);
    assert!(node_modules.is_copy_candidate());
    let env_file = artifact(&facts, ".env").expect(".env");
    assert!(env_file.is_copy_candidate());
    let example = artifact(&facts, ".env.example").expect(".env.example");
    assert_eq!(
        example.git_ignored,
        Some(false),
        "tracked-style files are not candidates"
    );
    assert!(!example.is_copy_candidate());
    let next_cache = artifact(&facts, ".next").expect(".next");
    assert_eq!(
        next_cache.recommendation,
        CopyRecommendation::NotRecommended
    );
    assert_eq!(next_cache.git_ignored, Some(true));
    let client = artifact(&facts, "src/generated/prisma").expect("prisma client");
    assert_eq!(client.kind, ArtifactKind::GeneratedCode);
    assert!(client.is_copy_candidate());

    // Wiring and toolchain.
    assert_eq!(facts.wiring.devflow_config.as_deref(), Some("devflow.toml"));
    let pitchfork = facts.wiring.pitchfork.as_ref().expect("pitchfork");
    assert_eq!(pitchfork.daemons.len(), 2);
    assert!(pitchfork.all_daemons_use_mise);
    assert!(facts.wiring.mise_loads_devflow_env);
    let mise = facts.toolchain.mise.as_ref().expect("mise");
    assert_eq!(mise.tasks, vec!["db:migrate", "dev"]);
    assert_eq!(mise.tools, vec!["bun", "node"]);
    assert!(facts.has_mise_task("dev"));
    assert_eq!(facts.toolchain.version_files[0].file, ".nvmrc");
    assert_eq!(
        facts.toolchain.version_files[0].version.as_deref(),
        Some("22")
    );

    // Nothing sensitive leaks into the serialized facts.
    let json = serde_json::to_string(&facts).expect("serialize");
    assert!(
        !json.contains(SENTINEL),
        "a value leaked into the facts: {json}"
    );
    assert!(facts.git.repository);
    assert!(facts.warnings.is_empty(), "{:?}", facts.warnings);
}

#[test]
fn django_with_uv_and_no_alembic() {
    let dir = fixture(&[
        (
            "manage.py",
            "#!/usr/bin/env python\nimport os\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'shop.settings')\n",
        ),
        (
            "pyproject.toml",
            r#"[project]
name = "shop"
requires-python = ">=3.12"
dependencies = ["Django>=5.0", "psycopg[binary]>=3.1", "redis"]

[dependency-groups]
dev = ["pytest"]
"#,
        ),
        ("uv.lock", "version = 1\n"),
        (".python-version", "3.12\n"),
        ("catalog/__init__.py", ""),
        ("catalog/migrations/__init__.py", ""),
        ("catalog/migrations/0001_initial.py", ""),
        ("shop/settings.py", ""),
        (".venv/pyvenv.cfg", "home = /usr/bin\n"),
        (".gitignore", ".venv\n"),
        (
            "compose.yaml",
            "services:\n  db:\n    image: postgis/postgis:16-3.4\n    ports: ['5433:5432']\n",
        ),
        (".env.example", "DATABASE_URL=x\nDEBUG=1\n"),
    ]);
    git_init(dir.path());
    let facts = scan_dir(&dir);

    assert_eq!(facts.python.len(), 1);
    assert_eq!(facts.python[0].tool, PythonTool::Uv);
    assert_eq!(facts.python[0].evidence, vec!["uv.lock"]);
    assert_eq!(facts.python_install_command(), Some("uv sync --frozen"));
    assert!(facts.js.is_empty());
    assert_eq!(facts.js_install_command(), None);

    let django = framework(&facts, Framework::Django).expect("django");
    assert!(django.evidence.contains(&"manage.py".to_string()));
    assert!(
        django
            .evidence
            .contains(&"pyproject.toml:django".to_string())
    );
    let django_migrations = migration(&facts, MigrationTool::Django).expect("django migrations");
    assert_eq!(
        django_migrations.migrations_dirs,
        vec!["catalog/migrations"]
    );
    assert!(
        !facts.has_migration_tool(MigrationTool::Alembic),
        "no alembic.ini and no alembic dependency"
    );
    assert!(
        facts
            .drivers
            .iter()
            .any(|d| d.engine == Engine::Postgres && d.dependency == "psycopg")
    );

    let runserver =
        process(&facts, "runserver", ProcessSource::DjangoRunserver).expect("runserver");
    assert_eq!(
        runserver.display.as_deref(),
        Some("uv run python manage.py runserver")
    );

    let db = service(&facts, "db");
    assert_eq!(db.engine, Engine::Postgres);
    assert_eq!(db.flavor.as_deref(), Some("postgis"));
    assert_eq!(db.major_version, Some(16));
    assert_eq!(db.ports[0].host, Some(5433));
    assert_eq!(db.ports[0].container, 5432);

    let venv = artifact(&facts, ".venv").expect(".venv");
    assert_eq!(venv.recommendation, CopyRecommendation::NotRecommended);
    assert!(venv.reason.contains("not relocatable"));
    assert_eq!(facts.toolchain.version_files[0].tool, "python");
}

#[test]
fn rails_with_mysql_compose_and_procfile_dev() {
    let dir = fixture(&[
        (
            "Gemfile",
            "source 'https://rubygems.org'\ngem 'rails'\ngem 'mysql2'\n",
        ),
        (
            "Gemfile.lock",
            "GEM\n  remote: https://rubygems.org/\n  specs:\n    actionpack (7.1.3)\n      rack (>= 2.2)\n    mysql2 (0.5.6)\n    rails (7.1.3)\n    sidekiq (7.2.0)\n\nPLATFORMS\n  arm64-darwin-23\n\nDEPENDENCIES\n  mysql2\n  rails (~> 7.1)\n  sidekiq\n",
        ),
        ("bin/rails", "#!/usr/bin/env ruby\n"),
        (
            "bin/dev",
            "#!/usr/bin/env sh\nexec foreman start -f Procfile.dev\n",
        ),
        ("config/application.rb", "module App; end\n"),
        ("config/master.key", "0000"),
        ("config/credentials/production.key", "1111"),
        ("config/credentials/production.yml.enc", "enc"),
        ("db/migrate/20240101000000_create_users.rb", ""),
        ("db/schema.rb", ""),
        ("app/assets/builds/application.css", ""),
        ("tmp/pids/server.pid", "1"),
        ("log/development.log", ""),
        ("vendor/bundle/ruby/3.3.0/.keep", ""),
        (
            "Procfile.dev",
            "web: bin/rails server -p 3000\ncss: bin/rails dartsass:watch\nworker: bundle exec sidekiq\n",
        ),
        (
            "compose.yml",
            "services:\n  mysql:\n    image: mysql:8.0\n    ports:\n      - \"127.0.0.1:3306:3306\"\n  redis:\n    image: redis:7.2-alpine\n",
        ),
        (".ruby-version", "3.3.0\n"),
        (
            ".gitignore",
            "/config/master.key\n/config/credentials/*.key\n/tmp/*\n/log/*\n/app/assets/builds/*\n/vendor/bundle\n",
        ),
    ]);
    git_init(dir.path());
    let facts = scan_dir(&dir);

    let rails = framework(&facts, Framework::Rails).expect("rails");
    assert!(rails.evidence.contains(&"bin/rails".to_string()));
    assert!(rails.evidence.contains(&"Gemfile.lock:rails".to_string()));
    let rails_migrations = migration(&facts, MigrationTool::Rails).expect("rails migrations");
    assert_eq!(rails_migrations.migrations_dirs, vec!["db/migrate"]);
    assert!(
        rails_migrations
            .config_files
            .contains(&"db/schema.rb".to_string())
    );
    assert!(
        facts
            .drivers
            .iter()
            .any(|d| d.engine == Engine::Mysql && d.dependency == "mysql2")
    );

    let mysql = service(&facts, "mysql");
    assert_eq!(mysql.engine, Engine::Mysql);
    assert_eq!(mysql.major_version, Some(8));
    assert_eq!(mysql.ports[0].host_ip.as_deref(), Some("127.0.0.1"));
    assert_eq!(service(&facts, "redis").major_version, Some(7));
    assert_eq!(facts.compose.as_ref().expect("compose").file, "compose.yml");

    // Procfile.dev entries and bin/dev.
    let web = process(&facts, "web", ProcessSource::Procfile).expect("web");
    assert_eq!(web.display.as_deref(), Some("Procfile.dev: web"));
    assert_eq!(web.role, ProcessRole::Serve);
    assert_eq!(
        process(&facts, "css", ProcessSource::Procfile).map(|p| p.role),
        Some(ProcessRole::Dev)
    );
    assert_eq!(
        process(&facts, "worker", ProcessSource::Procfile).map(|p| p.role),
        Some(ProcessRole::Worker)
    );
    let bin_dev = process(&facts, "dev", ProcessSource::RailsBinDev).expect("bin/dev");
    assert_eq!(bin_dev.display.as_deref(), Some("bin/dev"));

    // Candidates: keys and builds are recommended; tmp and log are present but not advised.
    for path in [
        "config/master.key",
        "config/credentials/production.key",
        "app/assets/builds",
        "vendor/bundle",
    ] {
        let a = artifact(&facts, path).unwrap_or_else(|| panic!("{path} missing"));
        assert!(a.is_copy_candidate(), "{path}: {a:?}");
    }
    assert!(artifact(&facts, "config/credentials/production.yml.enc").is_none());
    for path in ["tmp", "log"] {
        let a = artifact(&facts, path).unwrap_or_else(|| panic!("{path} missing"));
        assert_eq!(a.recommendation, CopyRecommendation::NotRecommended);
        assert!(!a.is_copy_candidate());
    }
    assert_eq!(facts.toolchain.version_files[0].tool, "ruby");
}

#[test]
fn laravel() {
    let dir = fixture(&[
        ("artisan", "#!/usr/bin/env php\n"),
        (
            "composer.json",
            r#"{
              "require": { "php": "^8.3", "laravel/framework": "^11.0", "ext-pdo_pgsql": "*" },
              "scripts": { "dev": "npx concurrently ...", "test": "phpunit" }
            }"#,
        ),
        ("composer.lock", "{}"),
        (
            "database/migrations/2024_01_01_000000_create_users_table.php",
            "",
        ),
        ("vendor/autoload.php", ""),
        ("bootstrap/cache/packages.php", ""),
        ("storage/logs/laravel.log", ""),
        ("public/build/manifest.json", "{}"),
        (
            "package.json",
            r#"{"scripts":{"dev":"vite","build":"vite build"},"devDependencies":{"vite":"5"}}"#,
        ),
        ("package-lock.json", "{}"),
        ("vite.config.js", "export default {};"),
        (
            ".env.example",
            "APP_KEY=\nDB_CONNECTION=pgsql\nDB_HOST=127.0.0.1\nDB_PORT=5432\nREDIS_HOST=127.0.0.1\n",
        ),
        (".env", "APP_KEY=VALUE_MUST_NOT_LEAK\nDB_HOST=x\n"),
        (
            ".gitignore",
            "/vendor\n/node_modules\n/bootstrap/cache/*\n/storage/*\n/public/build\n.env\n",
        ),
        ("node_modules/.keep", ""),
    ]);
    git_init(dir.path());
    let facts = scan_dir(&dir);

    let laravel = framework(&facts, Framework::Laravel).expect("laravel");
    assert!(laravel.evidence.contains(&"artisan".to_string()));
    assert!(
        laravel
            .evidence
            .contains(&"composer.json:require.laravel/framework".to_string())
    );
    let migrations = migration(&facts, MigrationTool::Laravel).expect("laravel migrations");
    assert_eq!(migrations.migrations_dirs, vec!["database/migrations"]);
    assert!(
        facts
            .drivers
            .iter()
            .any(|d| d.engine == Engine::Postgres && d.dependency == "ext-pdo_pgsql")
    );
    let dev = process(&facts, "dev", ProcessSource::Composer).expect("composer dev");
    assert_eq!(dev.display.as_deref(), Some("composer run dev"));

    // JavaScript side: npm (assumed from the lockfile) plus Vite.
    assert_eq!(
        facts.js_install_command(),
        Some("npm install --prefer-offline --no-audit --no-fund")
    );
    assert_eq!(
        facts.js[0].package_manager.source,
        PackageManagerSource::Lockfile
    );
    let vite = framework(&facts, Framework::Vite).expect("vite");
    assert!(vite.vite_based);

    assert!(facts.env.has_database_keys);
    assert!(facts.env.has_key("DB_CONNECTION"));
    assert!(facts.env.has_key("REDIS_HOST"));
    assert!(!facts.env.has_key("APP_KEY"));

    for path in ["vendor", "node_modules", "public/build", ".env"] {
        let a = artifact(&facts, path).unwrap_or_else(|| panic!("{path} missing"));
        assert!(a.is_copy_candidate(), "{path}: {a:?}");
    }
    for path in ["bootstrap/cache", "storage"] {
        let a = artifact(&facts, path).unwrap_or_else(|| panic!("{path} missing"));
        assert_eq!(
            a.recommendation,
            CopyRecommendation::NotRecommended,
            "{path}"
        );
    }
    assert!(
        !serde_json::to_string(&facts)
            .expect("json")
            .contains(SENTINEL)
    );
}

#[test]
fn pnpm_monorepo_with_turbo() {
    let dir = fixture(&[
        (
            "package.json",
            r#"{
              "name": "acme",
              "private": true,
              "packageManager": "pnpm@9.12.0+sha512.deadbeef",
              "scripts": { "dev": "turbo run dev", "build": "turbo run build" },
              "devDependencies": { "turbo": "2" }
            }"#,
        ),
        (
            "pnpm-workspace.yaml",
            "packages:\n  - 'apps/*'\n  - \"packages/*\"\n  - '!packages/legacy'\n",
        ),
        ("pnpm-lock.yaml", "lockfileVersion: '9.0'\n"),
        ("turbo.json", "{ \"tasks\": {} }"),
        (
            "apps/web/package.json",
            r#"{"name":"@acme/web","scripts":{"dev":"next dev","build":"next build"},"dependencies":{"next":"15","@acme/ui":"workspace:*"}}"#,
        ),
        (
            "apps/api/package.json",
            r#"{"name":"@acme/api","scripts":{"start:dev":"nest start --watch","worker":"node dist/worker.js"},"dependencies":{"@nestjs/core":"10","pg":"8"}}"#,
        ),
        ("apps/api/nest-cli.json", "{}"),
        (
            "packages/ui/package.json",
            r#"{"name":"@acme/ui","scripts":{"build":"tsc"}}"#,
        ),
        ("packages/legacy/package.json", r#"{"name":"@acme/legacy"}"#),
        (".gitignore", "node_modules\n"),
        ("node_modules/.pnpm/.keep", ""),
        ("apps/web/node_modules/.keep", ""),
        ("apps/api/node_modules/.keep", ""),
        ("packages/ui/node_modules/.keep", ""),
    ]);
    git_init(dir.path());
    let facts = scan_dir(&dir);

    // Members belong to the root project. `packages/legacy` is excluded by the
    // negated glob, so it is not a member: it shows up as a project of its own.
    assert_eq!(facts.js.len(), 2);
    assert_eq!(facts.js[1].path, "packages/legacy");
    assert_eq!(
        facts.js[1].package_manager.source,
        PackageManagerSource::Inherited
    );
    let js = &facts.js[0];
    assert_eq!(js.package_manager.kind, PackageManager::Pnpm);
    assert_eq!(js.package_manager.version.as_deref(), Some("9.12.0"));
    assert_eq!(
        facts.js_install_command(),
        Some("pnpm install --frozen-lockfile --prefer-offline")
    );
    let workspaces = js.workspaces.as_ref().expect("workspaces");
    assert_eq!(workspaces.source, WorkspaceSource::PnpmWorkspaceYaml);
    assert_eq!(
        workspaces.patterns,
        vec!["apps/*", "packages/*", "!packages/legacy"]
    );
    assert_eq!(js.monorepo_tools.len(), 1);
    assert_eq!(js.monorepo_tools[0].tool, MonorepoTool::Turbo);
    assert!(
        js.monorepo_tools[0]
            .evidence
            .contains(&"turbo.json".to_string())
    );

    let paths: Vec<&str> = js.packages.iter().map(|p| p.path.as_str()).collect();
    assert_eq!(paths, vec![".", "apps/api", "apps/web", "packages/ui"]);
    assert!(js.packages[1].workspace_member);
    assert!(!js.packages[0].workspace_member);

    let next = framework(&facts, Framework::NextJs).expect("next");
    assert_eq!(next.path, "apps/web");
    let nest = framework(&facts, Framework::NestJs).expect("nest");
    assert_eq!(nest.path, "apps/api");
    assert!(
        facts
            .drivers
            .iter()
            .any(|d| d.path == "apps/api" && d.engine == Engine::Postgres)
    );

    let web_dev = facts
        .processes
        .iter()
        .find(|p| p.cwd == "apps/web" && p.name == "dev")
        .expect("apps/web dev");
    assert_eq!(web_dev.display.as_deref(), Some("pnpm run dev"));
    let api_dev = process(&facts, "start:dev", ProcessSource::PackageJson).expect("nest dev");
    assert_eq!(api_dev.cwd, "apps/api");
    assert_eq!(api_dev.role, ProcessRole::Dev);

    for path in [
        "node_modules",
        "apps/web/node_modules",
        "apps/api/node_modules",
        "packages/ui/node_modules",
    ] {
        let a = artifact(&facts, path).unwrap_or_else(|| panic!("{path} missing"));
        assert!(a.is_copy_candidate(), "{path}");
    }
    assert!(artifact(&facts, "packages/legacy/node_modules").is_none());
}

#[test]
fn nothing_recognizable() {
    let dir = fixture(&[("README.md", "# hello\n"), ("notes/todo.txt", "ship it\n")]);
    let facts = scan_dir(&dir);

    assert!(facts.js.is_empty());
    assert!(facts.python.is_empty());
    assert!(facts.frameworks.is_empty());
    assert!(facts.migrations.is_empty());
    assert!(facts.drivers.is_empty());
    assert!(facts.compose.is_none());
    assert!(facts.env.files.is_empty());
    assert!(!facts.env.has_database_keys);
    assert!(facts.processes.is_empty());
    assert!(facts.artifacts.is_empty());
    assert_eq!(facts.toolchain, ToolchainFacts::default());
    assert_eq!(facts.wiring, WiringFacts::default());
    assert!(facts.warnings.is_empty());
    assert!(!facts.git.repository);
    assert_eq!(facts.js_install_command(), None);
    assert_eq!(facts.python_install_command(), None);
    assert!(!facts.has_framework(Framework::NextJs));
}

#[test]
fn empty_directory_and_missing_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let facts = scan(dir.path()).expect("scan");
    assert!(facts.frameworks.is_empty());

    let missing = dir.path().join("does-not-exist");
    let error = scan(&missing).expect_err("missing root");
    assert!(error.to_string().contains("cannot access"), "{error:#}");

    write(dir.path(), "file.txt", "x");
    let error = scan(&dir.path().join("file.txt")).expect_err("a file is not a root");
    assert!(error.to_string().contains("not a directory"), "{error:#}");
}

#[test]
fn polyglot_repository_with_nested_projects() {
    let dir = fixture(&[
        (
            "backend/pyproject.toml",
            "[project]\nname='api'\ndependencies=['fastapi','alembic','asyncpg']\n",
        ),
        ("backend/poetry.lock", ""),
        (
            "backend/alembic.ini",
            "[alembic]\nscript_location = migrations\n",
        ),
        ("backend/migrations/versions/abc_init.py", ""),
        (
            "frontend/package.json",
            r#"{"scripts":{"dev":"vite"},"devDependencies":{"vite":"5"}}"#,
        ),
        ("frontend/yarn.lock", "__metadata:\n  version: 8\n"),
        (
            "services/billing/go.mod",
            "module billing\n\nrequire (\n\tgithub.com/pressly/goose/v3 v3.20.0\n\tgithub.com/jackc/pgx/v5 v5.5.0 // indirect\n)\n",
        ),
        (
            "services/billing/db/migrations/00001_init.sql",
            "-- +goose Up\nselect 1;\n",
        ),
        (
            "worker/Cargo.toml",
            "[package]\nname='worker'\n\n[dependencies]\nsqlx = { version = '0.8', features = ['postgres', 'runtime-tokio'] }\n",
        ),
        ("worker/Cargo.lock", ""),
        ("worker/migrations/20240101_init.sql", "select 1;"),
        ("worker/.sqlx/query-abc.json", "{}"),
        ("docs/index.md", "# docs\n"),
    ]);
    let facts = scan_dir(&dir);

    assert_eq!(facts.python.len(), 1);
    assert_eq!(facts.python[0].path, "backend");
    assert_eq!(facts.python[0].tool, PythonTool::Poetry);
    assert!(facts.has_framework(Framework::FastApi));
    let alembic = migration(&facts, MigrationTool::Alembic).expect("alembic");
    assert_eq!(alembic.path, "backend");
    assert_eq!(alembic.migrations_dirs, vec!["backend/migrations/versions"]);

    assert_eq!(facts.js.len(), 1);
    assert_eq!(facts.js[0].path, "frontend");
    assert_eq!(facts.js[0].package_manager.kind, PackageManager::Yarn);
    assert!(
        facts.js[0].package_manager.yarn_berry,
        "__metadata marks berry"
    );
    assert_eq!(facts.js_install_command(), Some("yarn install --immutable"));
    assert!(framework(&facts, Framework::Vite).is_some_and(|f| f.path == "frontend"));

    let goose = migration(&facts, MigrationTool::Goose).expect("goose");
    assert_eq!(goose.path, "services/billing");
    assert_eq!(
        goose.migrations_dirs,
        vec!["services/billing/db/migrations"]
    );
    assert!(framework(&facts, Framework::Go).is_some_and(|f| f.path == "services/billing"));

    let sqlx = migration(&facts, MigrationTool::Sqlx).expect("sqlx");
    assert_eq!(sqlx.path, "worker");
    assert_eq!(sqlx.migrations_dirs, vec!["worker/migrations"]);
    assert_eq!(sqlx.offline_data.as_deref(), Some("worker/.sqlx"));
    assert_eq!(sqlx.database, Some(Engine::Postgres));
    assert!(framework(&facts, Framework::Rust).is_some_and(|f| f.path == "worker"));
}
