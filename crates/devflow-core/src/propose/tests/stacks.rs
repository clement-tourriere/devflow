//! Proposals for whole stacks.
use super::*;
use crate::config::{Config, PostgresMode};
use crate::detect;
use serde_json::json;

/// Bun + Next.js + Prisma, with compose running Postgres 16 and Redis, mise
/// pinning the toolchain, and an ignored `.env` whose values must never show up.
pub(super) fn bun_next_prisma() -> Repo {
    let repo = Repo::new(&[
        (
            "package.json",
            r#"{
  "name": "shop",
  "private": true,
  "scripts": {"dev": "next dev", "build": "next build", "worker": "bun run src/worker.ts"},
  "dependencies": {"next": "15.1.0", "react": "19.0.0", "@prisma/client": "6.1.0", "ioredis": "5.4.1"},
  "devDependencies": {"prisma": "6.1.0"}
}"#,
        ),
        ("bun.lock", "{}\n"),
        ("next.config.ts", "export default {};\n"),
        (
            "prisma/schema.prisma",
            "generator client {\n  provider = \"prisma-client-js\"\n}\ndatasource db {\n  provider = \"postgresql\"\n  url      = env(\"DATABASE_URL\")\n}\n",
        ),
        (
            "prisma/migrations/20240101000000_init/migration.sql",
            "CREATE TABLE t (id int);\n",
        ),
        (
            "compose.yaml",
            "services:\n  db:\n    image: postgres:16\n    ports:\n      - \"5432:5432\"\n    volumes:\n      - dbdata:/var/lib/postgresql/data\n  redis:\n    image: redis:7\n    container_name: shop-redis\n    ports:\n      - \"6379:6379\"\nvolumes:\n  dbdata:\n",
        ),
        (".env.example", "DATABASE_URL=\nREDIS_URL=\n"),
        (
            ".env",
            "DATABASE_URL=SENTINEL_VALUE_THAT_MUST_NOT_LEAK\nREDIS_URL=SENTINEL_VALUE_THAT_MUST_NOT_LEAK\n",
        ),
        (".gitignore", ".env\nnode_modules\n.next\n"),
        ("mise.toml", "[tools]\nnode = \"22\"\nbun = \"1.2\"\n"),
    ]);
    repo.mkdir("node_modules/.bin");
    repo.write("node_modules/.bin/.keep", "");
    repo.mkdir(".next");
    repo.write(".next/BUILD_ID", "x");
    repo
}

#[test]
fn bun_next_prisma_with_compose_postgres_and_redis() {
    let repo = bun_next_prisma();
    let plan = repo.plan();
    assert_eq!(plan.proposal_version, PROPOSAL_VERSION);
    assert_eq!(plan.schema_version, SCHEMA_VERSION);
    plan.validate().unwrap();

    // No value, and no path of an ignored cache, leaks into the plan.
    let json = serde_json::to_string(&plan).unwrap();
    assert!(!json.contains("SENTINEL"), "{json}");
    for denied in [".next", ".venv", "\"target\"", "copy:tmp", "copy:log"] {
        assert!(!json.contains(denied), "{denied} was proposed");
    }

    // Project and files cloned.
    let project = item(&plan, "project:init");
    assert_eq!(project.confidence, Confidence::High);
    assert_eq!(project.value["name"], "shop");
    assert_eq!(project.value["id"], ID);
    assert_eq!(
        item(&plan, "copy:node_modules").confidence,
        Confidence::High
    );
    let env = item(&plan, "copy:.env");
    assert!(env.risks.iter().any(|r| r.contains("secrets")));

    // The database: compose runs 16, devflow's server needs 18.
    let db = item(&plan, "service:db");
    assert_eq!(db.confidence, Confidence::High);
    assert_eq!(db.value["image"], "postgres:18");
    assert!(
        db.evidence
            .contains(&"compose.yaml:services.db.image".to_string())
    );
    assert!(
        db.risks.iter().any(|r| r.contains("PostgreSQL 16")),
        "{:?}",
        db.risks
    );
    assert!(
        db.risks.iter().any(|r| r.contains("cloned .env")),
        "a copied .env keeps its DATABASE_URL: {:?}",
        db.risks
    );

    // Hooks go through mise, and mise comes first. Each repository command
    // runs under `devflow exec`, which sets the workspace's variables after
    // mise has set its own: nothing depends on how mise is configured.
    let mise = item(&plan, "hook:setup:mise");
    assert!(matches!(
        &mise.patch.ops[0],
        PatchOp::Append { front: true, .. }
    ));
    assert_eq!(
        mise.value["entries"],
        json!([{"name": "mise-install", "run": "mise install"}]),
        "`mise trust` is not proposed: setup stops on an untrusted worktree before any hook"
    );
    assert_eq!(
        run(&plan, "hook:setup:install"),
        "mise exec -- devflow exec -- bun install --frozen-lockfile"
    );
    assert_eq!(
        run(&plan, "hook:setup:codegen"),
        "mise exec -- devflow exec -- bunx prisma generate"
    );
    assert_eq!(
        run(&plan, "hook:setup:migrate"),
        "mise exec -- devflow exec -- bunx prisma migrate deploy"
    );
    assert_eq!(
        item(&plan, "hook:setup:migrate").depends_on,
        vec!["project:init", "service:db"],
        "devflow exec has the last word, so it does not wait for mise's wiring"
    );
    assert!(!json.contains("npm ci"));

    // Redis is branched by devflow: a service item (recommended, ticked by
    // default) and no compose hook, since nothing is left for compose to
    // start (the compose postgres is replaced by devflow's server too).
    let cache = item(&plan, "service:redis");
    assert_eq!(cache.confidence, Confidence::High);
    assert_eq!(
        cache.value,
        json!({
            "key": "redis", "engine": "redis", "image": "redis:7", "env": "REDIS_URL",
            "compose_service": "redis",
        })
    );
    assert!(
        cache
            .evidence
            .contains(&"compose.yaml:services.redis.image".to_string())
            && cache
                .evidence
                .contains(&".env.example:REDIS_URL".to_string()),
        "{:?}",
        cache.evidence
    );
    assert!(
        cache
            .risks
            .iter()
            .any(|r| r.contains("cloned .env") && r.contains("REDIS_URL")),
        "a copied .env keeps its REDIS_URL: {:?}",
        cache.risks
    );
    assert!(
        cache.risks.iter().any(|r| r.contains("AOF off")),
        "{:?}",
        cache.risks
    );
    assert!(
        cache
            .risks
            .iter()
            .any(|r| r.contains("can stay in compose.yaml for other uses")),
        "{:?}",
        cache.risks
    );
    assert!(defaults(&plan).contains(&"service:redis".to_string()));
    for hook in [
        "hook:setup:compose-up",
        "hook:switch:compose-up",
        "hook:teardown:compose-down",
    ] {
        // Redis is devflow's once `service:redis` is accepted (it is by
        // default): compose has nothing left to start.
        assert_eq!(
            resolved_run(&plan, &["service:redis"], hook),
            None,
            "{hook}: nothing is left for compose"
        );
    }

    // Presets, mise wiring and daemons.
    assert_eq!(
        item(&plan, "env:preset:prisma").confidence,
        Confidence::High
    );
    assert_eq!(item(&plan, "mise:env-file").patch.file, "mise.toml");
    let web = item(&plan, "daemon:web");
    assert_eq!(web.confidence, Confidence::High);
    assert_eq!(web.value["run"], "devflow exec -- bun run dev");
    assert_eq!(web.value["port"], 3000);
    assert_eq!(web.value["mise"], true, "mise still provides the tools");
    assert_eq!(item(&plan, "daemon:worker").confidence, Confidence::Medium);

    // Inputs are hashed, the detection facts included.
    assert!(plan.input_hashes[DETECTION_KEY].is_some());
    assert_eq!(plan.input_hashes["devflow.toml"], None);
    assert!(plan.input_hashes["mise.toml"].is_some());
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("keeps the compose service `db`")),
        "{:?}",
        plan.warnings
    );
}

#[test]
fn defaults_apply_the_high_items_and_the_files_load() {
    let repo = bun_next_prisma();
    let plan = repo.plan();
    let selection = plan.select(&[], &[], true).unwrap();
    assert!(selection.dropped.is_empty(), "{:?}", selection.dropped);
    for id in [
        "project:init",
        "copy:node_modules",
        "copy:.env",
        "service:db",
        "hook:setup:mise",
        "hook:setup:install",
        "hook:setup:codegen",
        "hook:setup:migrate",
        "env:preset:prisma",
        "mise:env-file",
        "daemon:web",
    ] {
        assert!(selection.accepted.iter().any(|a| a == id), "{id}");
    }
    for id in [
        "hook:setup:compose-up",
        "hook:switch:compose-up",
        "hook:teardown:compose-down",
        "daemon:worker",
    ] {
        assert!(!selection.accepted.iter().any(|a| a == id), "{id}");
    }

    let report = apply(repo.root(), &plan, &selection.accepted).unwrap();
    assert!(!report.already_applied && !report.resumed);
    let (config, path) = Config::load(repo.root()).unwrap();
    assert_eq!(path, repo.root().join("devflow.toml"));
    assert_eq!(config.project.name, "shop");
    assert_eq!(config.project.id.to_string(), ID);
    assert_eq!(config.worktree.copy, vec!["node_modules", ".env"]);
    let (key, pg) = config.postgres().unwrap();
    assert_eq!(
        (key, pg.mode, pg.image.as_str()),
        ("db", PostgresMode::Managed, "postgres:18")
    );
    let names: Vec<_> = config
        .hooks
        .setup
        .iter()
        .map(|h| h.name.clone().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["mise-install", "install", "prisma-generate", "migrate"]
    );
    for hook in &config.hooks.setup[1..] {
        assert!(
            hook.run.starts_with("mise exec -- devflow exec -- "),
            "{}",
            hook.run
        );
    }
    assert_eq!(config.env.presets, vec![crate::config::EnvPreset::Prisma]);
    assert!(config.hooks.switch.is_empty() && config.hooks.teardown.is_empty());

    let pitchfork = repo.read("pitchfork.toml");
    assert!(pitchfork.contains("[daemons.web]"), "{pitchfork}");
    assert!(
        pitchfork.contains("run = \"devflow exec -- bun run dev\"")
            && pitchfork.contains("mise = true"),
        "{pitchfork}"
    );
    assert!(!pitchfork.contains("worker"), "{pitchfork}");
    let mise = repo.read("mise.toml");
    assert!(
        mise.starts_with("[tools]\nnode = \"22\"\nbun = \"1.2\"\n"),
        "{mise}"
    );
    assert!(mise.contains("_.file = \".devflow/env\""), "{mise}");
    // The wiring facts see it as wired now.
    let facts = detect::scan(repo.root()).unwrap();
    assert!(facts.wiring.mise_loads_devflow_env);
    // Nothing else was created: no tools installed, no `.devflow` directory.
    assert!(!repo.exists(".devflow"));
}

/// Django + uv, compose running PostGIS 16.
pub(super) fn django_uv() -> Repo {
    let repo = Repo::new(&[
        (
            "manage.py",
            "#!/usr/bin/env python\nimport os\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'shop.settings')\n",
        ),
        (
            "pyproject.toml",
            r#"[project]
name = "shop"
requires-python = ">=3.12"
dependencies = ["Django>=5.0", "psycopg[binary]>=3.1"]
"#,
        ),
        ("uv.lock", "version = 1\n"),
        ("catalog/__init__.py", ""),
        ("catalog/migrations/__init__.py", ""),
        ("catalog/migrations/0001_initial.py", ""),
        ("shop/settings.py", ""),
        (".gitignore", ".venv\n.env\n"),
        (
            "compose.yaml",
            "services:\n  db:\n    image: postgis/postgis:16-3.4\n    volumes:\n      - ./initdb:/docker-entrypoint-initdb.d\n",
        ),
        (".env.example", "DATABASE_URL=\nDEBUG=\n"),
        (".env", "DATABASE_URL=SENTINEL_VALUE_THAT_MUST_NOT_LEAK\n"),
    ]);
    repo.mkdir(".venv");
    repo.write(".venv/pyvenv.cfg", "home = /usr/bin\n");
    repo
}

#[test]
fn django_with_uv_and_postgis() {
    let repo = django_uv();
    let plan = repo.plan();
    let json = serde_json::to_string(&plan).unwrap();
    assert!(!json.contains("SENTINEL"));
    assert!(
        plan.item("copy:.venv").is_none(),
        "the virtualenv is never cloned"
    );
    assert!(!json.contains("\"path\":\".venv\""), "{json}");

    let db = item(&plan, "service:db");
    assert_eq!(db.confidence, Confidence::High);
    assert_eq!(db.value["image"], "postgis/postgis:18-3.6");
    assert!(db.risks.iter().any(|r| r.contains("PostgreSQL 16")));
    assert!(
        db.risks
            .iter()
            .any(|r| r.contains("init scripts") && r.contains("initdb")),
        "{:?}",
        db.risks
    );
    assert!(
        db.risks
            .iter()
            .any(|r| r.contains("postgis/postgis:18-3.6") && r.contains("tag exists")),
        "{:?}",
        db.risks
    );

    // No mise here: plain commands through uv.
    assert_eq!(run(&plan, "hook:setup:install:python"), "uv sync --frozen");
    assert_eq!(
        run(&plan, "hook:setup:migrate"),
        "uv run python manage.py migrate --noinput"
    );
    assert!(plan.item("hook:setup:mise").is_none());
    assert!(
        plan.item("hook:setup:install").is_none(),
        "no JavaScript here"
    );

    // Django reads DATABASE_URL in this project: no preset.
    assert!(!ids(&plan).iter().any(|id| id.starts_with("env:preset")));

    let web = item(&plan, "daemon:web");
    assert_eq!(
        web.value["run"],
        "devflow exec -- uv run python manage.py runserver 127.0.0.1:$PORT"
    );
    assert_eq!(web.value["port"], 8000);
    // The repository does not use mise, so the daemon does not either: it
    // gets the workspace's variables from `devflow exec`, not from mise.
    assert_eq!(web.value["mise"], false);
    assert!(web.depends_on.is_empty(), "{:?}", web.depends_on);

    // mise would be created for shells and editors, at medium confidence;
    // nothing waits for it.
    let wiring = item(&plan, "mise:env-file");
    assert_eq!(wiring.confidence, Confidence::Medium);
    assert_eq!(wiring.value["creates_file"], true);
    assert!(wiring.risks.iter().any(|r| r.contains("Needs mise")));
    assert_eq!(item(&plan, "copy:.env").confidence, Confidence::High);
}

#[test]
fn django_without_a_url_gets_the_db_parts_preset() {
    let repo = Repo::new(&[
        ("manage.py", "import django\n"),
        (
            "pyproject.toml",
            "[project]\nname = \"shop\"\ndependencies = [\"Django>=5.0\", \"psycopg>=3\"]\n",
        ),
        ("uv.lock", "version = 1\n"),
        ("catalog/migrations/__init__.py", ""),
        (
            ".env.example",
            "DB_HOST=\nDB_NAME=\nDB_USER=\nDB_PASSWORD=\n",
        ),
    ]);
    let plan = repo.plan();
    let db = item(&plan, "service:db");
    assert_eq!(
        db.confidence,
        Confidence::High,
        "a driver and database variables"
    );
    let preset = item(&plan, "env:preset:db_parts");
    assert_eq!(preset.confidence, Confidence::Medium);
    assert_eq!(preset.depends_on, vec!["project:init", "service:db"]);
    assert!(preset.evidence.iter().any(|e| e.ends_with(":DB_HOST")));
}

#[test]
fn a_driver_alone_is_a_medium_guess() {
    let repo = Repo::new(&[
        (
            "package.json",
            r#"{"name":"x","dependencies":{"pg":"8.0.0"}}"#,
        ),
        ("package-lock.json", "{}"),
    ]);
    let plan = repo.plan();
    let db = item(&plan, "service:db");
    assert_eq!(db.confidence, Confidence::Medium);
    assert!(db.risks.iter().any(|r| r.contains("DATABASE_URL")));
    assert_eq!(
        run(&plan, "hook:setup:install"),
        "npm install --prefer-offline --no-audit --no-fund",
        "never npm ci"
    );
    // Only the project is applied by default.
    let defaults = defaults(&plan);
    assert!(!defaults.contains(&"service:db".to_string()));
}

/// Rails on MySQL with a Procfile.dev and a compose file for MySQL and Redis.
pub(super) fn rails_mysql() -> Repo {
    Repo::new(&[
        (
            "Gemfile",
            "source 'https://rubygems.org'\ngem 'rails'\ngem 'mysql2'\n",
        ),
        (
            "Gemfile.lock",
            "GEM\n  remote: https://rubygems.org/\n  specs:\n    rails (7.1.3)\n    mysql2 (0.5.6)\n\nDEPENDENCIES\n  mysql2\n  rails (~> 7.1)\n",
        ),
        ("bin/rails", "#!/usr/bin/env ruby\n"),
        (
            "bin/dev",
            "#!/usr/bin/env sh\nexec foreman start -f Procfile.dev\n",
        ),
        ("config/application.rb", "module App; end\n"),
        ("config/master.key", "SENTINEL_VALUE_THAT_MUST_NOT_LEAK"),
        ("db/migrate/20240101000000_create_users.rb", ""),
        ("db/schema.rb", ""),
        ("tmp/pids/server.pid", "1"),
        ("log/development.log", ""),
        (
            "Procfile.dev",
            "web: bin/rails server -p 3000\ncss: bin/rails dartsass:watch\nworker: bundle exec sidekiq\n",
        ),
        (
            "compose.yml",
            "services:\n  mysql:\n    image: mysql:8.0\n    container_name: shop-mysql\n    ports:\n      - \"127.0.0.1:3306:3306\"\n    volumes:\n      - mysqldata:/var/lib/mysql\n  redis:\n    image: redis:7.2-alpine\n  mailer:\n    image: acme/mailer:1\nvolumes:\n  mysqldata:\n",
        ),
        (".gitignore", "/config/master.key\n/tmp/*\n/log/*\n"),
    ])
}

#[test]
fn rails_with_mysql_compose_and_procfile_dev() {
    let repo = rails_mysql();
    let plan = repo.plan();
    let json = serde_json::to_string(&plan).unwrap();
    assert!(!json.contains("SENTINEL"));
    assert!(!json.contains("foreman start"), "no Procfile text: {json}");

    // No PostgreSQL service and no migration hook (the migration hooks are
    // for PostgreSQL); the compose MySQL runs 8.0, which devflow's MySQL 8.4
    // profile does not.
    assert!(plan.item("service:db").is_none());
    assert!(plan.item("service:mysql").is_none());
    assert!(plan.item("hook:setup:migrate").is_none());
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("Rails migrations") && w.contains("no PostgreSQL")),
        "{:?}",
        plan.warnings
    );

    // Redis is devflow's; MySQL 8.0 starts from compose, isolated by the
    // project name, and the plan says why devflow does not branch it.
    assert_eq!(
        item(&plan, "service:redis").value["image"],
        "redis:7.2-alpine"
    );
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("`mysql` (mysql:8.0)")
                && w.contains("unsupported")
                && w.contains("MySQL 8.4 only")
                && w.contains("MySQL branches (preview)")),
        "{:?}",
        plan.warnings
    );
    let up = item(&plan, "hook:setup:compose-up");
    assert_eq!(up.confidence, Confidence::Medium);
    assert_eq!(
        run(&plan, "hook:setup:compose-up"),
        "docker compose up -d --wait mysql"
    );
    assert!(
        up.risks
            .iter()
            .any(|r| r.contains("`mysql` is not branched: unsupported")),
        "{:?}",
        up.risks
    );
    assert!(
        up.risks
            .iter()
            .any(|r| r.contains("container_name: shop-mysql"))
    );
    assert!(up.risks.iter().any(|r| r.contains("fixed host port 3306")));
    assert!(
        up.risks
            .iter()
            .any(|r| r.contains("named volume `mysqldata`"))
    );
    assert!(
        plan.warnings.iter().any(|w| w.contains("acme/mailer")),
        "an unrecognized image is left out and said so: {:?}",
        plan.warnings
    );

    // Rails installs its gems; secrets are cloned, scratch directories are not.
    assert_eq!(run(&plan, "hook:setup:install:ruby"), "bundle install");
    assert_eq!(
        item(&plan, "copy:config/master.key").confidence,
        Confidence::High
    );
    assert!(plan.item("copy:tmp").is_none() && plan.item("copy:log").is_none());

    // bin/dev runs every Procfile.dev process: one daemon, no placeholders.
    let web = item(&plan, "daemon:web");
    assert_eq!(web.value["run"], "devflow exec -- bin/dev");
    assert_eq!(web.value["port"], 3000);
    assert!(plan.item("daemon:css").is_none() && plan.item("daemon:worker").is_none());

    // The environment still needs wiring for COMPOSE_PROJECT_NAME in shells.
    let wiring = item(&plan, "mise:env-file");
    assert!(
        wiring
            .evidence
            .iter()
            .any(|e| e.contains("COMPOSE_PROJECT_NAME"))
    );
    assert!(
        !web.depends_on.contains(&"mise:env-file".to_string()),
        "{:?}",
        web.depends_on
    );
}

#[test]
fn procfile_entries_without_bin_dev_are_notes_to_fill_in() {
    let repo = Repo::new(&[
        ("Gemfile", "gem 'sinatra'\n"),
        (
            "Procfile",
            "web: bundle exec puma -C config/puma.rb --token SENTINEL_VALUE_THAT_MUST_NOT_LEAK\nrelease: bin/migrate\n",
        ),
    ]);
    let plan = repo.plan();
    let json = serde_json::to_string(&plan).unwrap();
    assert!(!json.contains("SENTINEL"), "{json}");
    let web = item(&plan, "daemon:web");
    assert_eq!(web.confidence, Confidence::Low);
    assert!(!web.is_actionable());
    assert_eq!(web.value["run"], json!(null));
    assert!(web.risks.iter().any(|r| r.contains("not copied")));
    assert!(plan.item("daemon:release").is_none());
    assert!(!defaults(&plan).contains(&"daemon:web".to_string()));
    // Accepting a note applies nothing and is reported as manual work.
    let report = apply(
        repo.root(),
        &plan,
        &["project:init".into(), "daemon:web".into()],
    )
    .unwrap();
    assert_eq!(report.manual, vec!["daemon:web"]);
    assert!(!repo.exists("pitchfork.toml"));
}

#[test]
fn an_empty_repository_gets_only_its_project_file() {
    let repo = Repo::new(&[("README.md", "# shop\n")]);
    let plan = repo.plan();
    assert_eq!(ids(&plan), vec!["project:init"]);
    assert!(plan.already_configured.is_empty());
    assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    let report = apply(repo.root(), &plan, &defaults(&plan)).unwrap();
    assert_eq!(report.applied, vec!["project:init"]);
    assert_eq!(
        repo.read("devflow.toml"),
        format!(
            "# Project identity. Keep the id stable and committed: it ties trust, the\n\
             # registry and the database server to this project across machines.\n\
             [project]\nname = \"shop\"\nid = \"{ID}\"\n\n\
             [worktree]\n\
             # Where new worktrees go ({{repo}} is this checkout's directory name).\n\
             dir = \"../{{repo}}.worktrees\"\n"
        )
    );
    let (config, _) = Config::load(repo.root()).unwrap();
    assert!(config.services.is_empty());
    // A second plan has nothing left to propose.
    let again = repo.plan();
    assert!(again.items.is_empty(), "{:?}", ids(&again));
}

#[test]
fn an_explicit_postgres_choice_is_not_downgraded_by_thin_evidence() {
    // A PostgreSQL driver and nothing else: no .env key, no compose service.
    let repo = Repo::new(&[
        (
            "package.json",
            r#"{"name":"api","dependencies":{"pg":"8.11.0"}}"#,
        ),
        ("package-lock.json", "{}\n"),
    ]);
    let plan = repo.plan();
    let guess = item(&plan, "service:db");
    assert_eq!(guess.confidence, Confidence::Medium, "a guess asks first");
    assert!(
        !defaults(&plan).contains(&"service:db".to_string()),
        "detection alone does not add the service by default"
    );

    for (choice, mode) in [
        (PostgresChoice::Managed, "managed"),
        (PostgresChoice::External, "external"),
    ] {
        let mut options = repo.options();
        options.postgres = Some(choice);
        let plan = propose(repo.root(), &options).unwrap();
        let db = item(&plan, "service:db");
        assert_eq!(db.confidence, Confidence::High, "--postgres {mode}");
        assert_eq!(db.value["mode"], mode);
        assert!(
            db.risks.iter().any(|r| r.contains("DATABASE_URL")),
            "the thin evidence is still said: {:?}",
            db.risks
        );
        // `--yes --postgres …` applies the default selection: the service is in it.
        let selected = defaults(&plan);
        assert!(selected.contains(&"service:db".to_string()), "{selected:?}");
        apply(repo.root(), &plan, &selected).unwrap();
        let (config, _) = Config::load(repo.root()).unwrap();
        assert!(config.postgres().is_some(), "--postgres {mode}");
        std::fs::remove_file(repo.root().join("devflow.toml")).unwrap();
        let _ = std::fs::remove_file(repo.root().join(".git/devflow/init-apply.json"));
    }
}

#[test]
fn name_and_postgres_overrides() {
    let repo = Repo::new(&[("README.md", "x\n")]);
    let mut options = repo.options();
    options.name = Some("Acme Shop".into());
    options.postgres = Some(PostgresChoice::Managed);
    let plan = plan_with(
        repo.root(),
        &detect::scan(repo.root()).unwrap(),
        &ExistingFiles::read(repo.root()).unwrap(),
        &options,
    )
    .unwrap();
    assert_eq!(item(&plan, "project:init").value["name"], "Acme Shop");
    let db = item(&plan, "service:db");
    assert_eq!(db.confidence, Confidence::High);
    assert_eq!(db.evidence[0], "--postgres managed");

    options.postgres = Some(PostgresChoice::External);
    let plan = plan_with(
        repo.root(),
        &detect::scan(repo.root()).unwrap(),
        &ExistingFiles::read(repo.root()).unwrap(),
        &options,
    )
    .unwrap();
    assert_eq!(item(&plan, "service:db").value["mode"], "external");
    apply(repo.root(), &plan, &all(&plan)).unwrap();
    let (config, _) = Config::load(repo.root()).unwrap();
    assert_eq!(config.project.name, "Acme Shop");
    assert_eq!(config.postgres().unwrap().1.mode, PostgresMode::External);

    // `--postgres none` removes the service and what depends on it.
    let repo = bun_next_prisma();
    let mut options = repo.options();
    options.postgres = Some(PostgresChoice::None);
    let plan = propose(repo.root(), &options).unwrap();
    assert!(plan.item("service:db").is_none());
    assert!(plan.item("hook:setup:migrate").is_none());
    assert!(plan.item("env:preset:prisma").is_none());
    assert_eq!(
        run(&plan, "hook:setup:compose-up"),
        "docker compose up -d --wait db",
        "without devflow's server, compose keeps starting the compose database (Redis is devflow's)"
    );
    assert!(plan.item("service:redis").is_some());
}

/// A repository that already has a configuration of its own.
pub(super) fn initialized() -> Repo {
    let repo = bun_next_prisma();
    repo.write(
        "devflow.toml",
        r#"# Mine.
[project]
name = "shop"
id = "11111111-2222-4333-8444-555555555555"

[worktree]
dir = "../{repo}.worktrees"
copy = [".env"]

[services.main]
engine = "postgres"
mode = "managed"
image = "postgres:17"

[hooks]
setup = [
  { name = "install", run = "make deps" },   # my install
  { name = "migrate", run = "make migrate" },
]

[env]
DIRECT_URL = "${DATABASE_URL}"
"#,
    );
    repo.write(
        "mise.toml",
        "[tools]\nbun = \"1.2\"\n\n[env]\n_.file = \".env\"\n",
    );
    repo.write(
        "pitchfork.toml",
        "[daemons.web]\nrun = \"mise exec -- bun run dev\"\n",
    );
    repo
}

#[test]
fn an_initialized_repository_reports_conflicts_and_never_overwrites() {
    let repo = initialized();
    let before = ["devflow.toml", "mise.toml", "pitchfork.toml"].map(|f| repo.read(f));
    let plan = repo.plan();

    // Nothing the files already do is proposed again.
    for id in [
        "copy:.env",
        "service:main",
        "hook:setup:migrate",
        "daemon:web",
    ] {
        assert!(
            plan.already_configured.contains(&id.to_string()),
            "{id} should be recognized as configured: {:?}",
            plan.already_configured
        );
    }
    assert!(plan.item("project:init").is_none());
    assert!(
        plan.item("service:db").is_none(),
        "one PostgreSQL service is enough"
    );

    // Differences are reported, not applied.
    let install = item(&plan, "hook:setup:install");
    assert!(!install.is_actionable());
    assert!(
        install.conflicts[0].contains("already has an entry named \"install\""),
        "{:?}",
        install.conflicts
    );
    let preset = item(&plan, "env:preset:prisma");
    assert!(!preset.is_actionable());
    assert!(
        preset.conflicts[0].contains("DIRECT_URL"),
        "{:?}",
        preset.conflicts
    );
    let wiring = item(&plan, "mise:env-file");
    assert!(!wiring.is_actionable());
    assert!(
        wiring.conflicts[0].contains("env._.file"),
        "{:?}",
        wiring.conflicts
    );
    assert!(wiring.risks.iter().any(|r| r.contains("main checkout")));

    // Additions are proposed against the existing files.
    assert!(item(&plan, "copy:node_modules").is_actionable());
    assert!(item(&plan, "daemon:worker").is_actionable());
    assert!(item(&plan, "hook:setup:mise").is_actionable());

    // The default selection leaves out every blocked item.
    let selection = plan.select(&[], &[], true).unwrap();
    for blocked in ["hook:setup:install", "env:preset:prisma", "mise:env-file"] {
        assert!(
            !selection.accepted.contains(&blocked.to_string()),
            "{blocked}"
        );
    }
    let report = apply(repo.root(), &plan, &selection.accepted).unwrap();
    assert!(report.files.iter().any(|f| f.file == "devflow.toml"));

    // The original text is all still there, in order, with the additions around it.
    let after = repo.read("devflow.toml");
    assert!(after.starts_with("# Mine.\n[project]\n"), "{after}");
    assert!(
        after.contains("{ name = \"install\", run = \"make deps\" },   # my install"),
        "{after}"
    );
    assert!(after.contains("DIRECT_URL = \"${DATABASE_URL}\""));
    assert!(after.contains("image = \"postgres:17\""));
    assert!(!after.contains("postgres:18"));
    let mise = repo.read("mise.toml");
    assert_eq!(mise, before[1], "the conflicting mise.toml is untouched");
    let pitchfork = repo.read("pitchfork.toml");
    assert!(pitchfork.starts_with(&before[2]), "{pitchfork}");
    let (config, _) = Config::load(repo.root()).unwrap();
    assert_eq!(config.worktree.copy, vec![".env", "node_modules"]);
    assert_eq!(config.hooks.setup[0].run, "mise install", "mise runs first");
    assert_eq!(
        config.hooks.setup[1].run, "make deps",
        "the project's own hook is kept"
    );

    // A conflicting item cannot be applied even when asked for by name.
    let error = apply(repo.root(), &plan, &["mise:env-file".into()])
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("mise:env-file") && error.contains("env._.file"),
        "{error}"
    );
}

/// `bun_next_prisma` with the `[env]` section of its `mise.toml` replaced.
fn with_mise_env(env: &str) -> Repo {
    let repo = bun_next_prisma();
    repo.write(
        "mise.toml",
        &format!("[tools]\nnode = \"22\"\nbun = \"1.2\"\n\n{env}"),
    );
    repo
}

/// Everything that runs repository commands through mise and needs the
/// workspace's variables: installs and code generation too, since lifecycle
/// scripts (`postinstall`, `prepare`) and generators can reach the database.
/// All of it runs under `devflow exec`, which sets the variables after mise.
const MISE_USERS: [&str; 4] = [
    "hook:setup:install",
    "hook:setup:codegen",
    "hook:setup:migrate",
    "daemon:web",
];

const SHELLS_WARNING: &str = "shells in the worktree won't see the branch's variables through \
     mise; commands devflow runs are unaffected";

#[test]
fn a_blocked_wiring_blocks_nothing_that_goes_through_devflow_exec() {
    // The reported case: mise loads the main checkout's `.env`, so the wiring
    // item is blocked. A migration run through `mise exec` would get the main
    // database from mise, which is why the plan runs it under `devflow exec`,
    // where devflow sets the workspace's variables after mise: the daemon
    // too. The blocked wiring only costs the shells and editors.
    let repo = with_mise_env("[env]\n_.file = \".env\"\n");
    let plan = repo.plan();
    let wiring = item(&plan, "mise:env-file");
    assert!(!wiring.is_actionable());
    assert!(
        wiring.conflicts[0].contains("env._.file"),
        "{:?}",
        wiring.conflicts
    );
    assert!(
        wiring.risks.iter().any(|r| r == SHELLS_WARNING),
        "{:?}",
        wiring.risks
    );
    assert!(
        plan.warnings.iter().any(|w| w.contains(SHELLS_WARNING)),
        "{:?}",
        plan.warnings
    );
    assert_eq!(
        run(&plan, "hook:setup:migrate"),
        "mise exec -- devflow exec -- bunx prisma migrate deploy"
    );
    for id in MISE_USERS {
        let user = item(&plan, id);
        assert!(
            !user.depends_on.contains(&"mise:env-file".to_string()),
            "{id}: {:?}",
            user.depends_on
        );
    }

    // Selected by default, and applied.
    let selection = plan.select(&[], &[], true).unwrap();
    assert!(selection.dropped.is_empty(), "{:?}", selection.dropped);
    for id in MISE_USERS {
        assert!(selection.accepted.contains(&id.to_string()), "{id}");
    }
    assert!(!selection.accepted.contains(&"mise:env-file".to_string()));
    let report = apply(repo.root(), &plan, &selection.accepted).unwrap();
    assert!(report.applied.contains(&"service:db".to_string()));
    let written = repo.read("devflow.toml");
    assert!(
        written.contains("mise exec -- devflow exec -- bunx prisma migrate deploy"),
        "{written}"
    );
    let daemons = repo.read("pitchfork.toml");
    assert!(
        daemons.contains("run = \"devflow exec -- bun run dev\""),
        "{daemons}"
    );

    // The blocked item itself is still refused, and nothing was written for it.
    let repo = with_mise_env("[env]\n_.file = \".env\"\n");
    let plan = repo.plan();
    let before = repo.read("mise.toml");
    let error = apply(repo.root(), &plan, &["mise:env-file".into()])
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("\"mise:env-file\" cannot be applied") && error.contains("env._.file"),
        "{error}"
    );
    assert!(!repo.exists("devflow.toml") && !repo.exists("pitchfork.toml"));
    assert_eq!(repo.read("mise.toml"), before);
}

#[test]
fn an_env_file_that_wins_over_devflow_env_blocks_the_wiring_too() {
    // `.devflow/env` is loaded, but `.env` comes after it, in the same list or
    // in a higher-precedence mise file, and mise lets later entries win.
    let losing = [
        (
            "a later entry of the list",
            "[env]\n_.file = [\".devflow/env\", \".env\"]\n",
            None,
        ),
        (
            "a later mise file",
            "[env]\n_.file = \".devflow/env\"\n",
            Some(("mise.local.toml", "[env]\n_.file = \".env\"\n")),
        ),
        (
            "a script after it",
            "[env]\n_.file = \".devflow/env\"\n_.source = \"./env.sh\"\n",
            None,
        ),
    ];
    for (label, env, extra) in losing {
        let repo = with_mise_env(env);
        if let Some((file, text)) = extra {
            repo.write(file, text);
        }
        let plan = repo.plan();
        let wiring = item(&plan, "mise:env-file");
        assert!(!wiring.is_actionable(), "{label}");
        assert!(
            wiring.conflicts[0].contains("after .devflow/env"),
            "{label}: {:?}",
            wiring.conflicts
        );
        // Blocked for shells only: the hooks and the daemon go ahead.
        assert!(
            plan.warnings.iter().any(|w| w.contains(SHELLS_WARNING)),
            "{label}: {:?}",
            plan.warnings
        );
        let selection = plan.select(&[], &[], true).unwrap();
        for id in MISE_USERS {
            assert!(
                selection.accepted.contains(&id.to_string()),
                "{label}: {id}"
            );
        }
    }

    // Loaded last, it wins: nothing to propose and nothing to wait for.
    let winning = [
        (
            "the last entry",
            "[env]\n_.file = [\".env\", \".devflow/env\"]\n",
            None,
        ),
        (
            "a higher mise file",
            "[env]\n_.file = \".env\"\n",
            Some(("mise.local.toml", "[env]\n_.file = \".devflow/env\"\n")),
        ),
        ("alone", "[env]\n_.file = \".devflow/env\"\n", None),
    ];
    for (label, env, extra) in winning {
        let repo = with_mise_env(env);
        if let Some((file, text)) = extra {
            repo.write(file, text);
        }
        let plan = repo.plan();
        assert!(plan.item("mise:env-file").is_none(), "{label}");
        assert!(
            plan.already_configured
                .contains(&"mise:env-file".to_string()),
            "{label}"
        );
        for id in MISE_USERS {
            assert!(
                !item(&plan, id)
                    .depends_on
                    .contains(&"mise:env-file".to_string()),
                "{label}: {id}"
            );
        }
        let selection = plan.select(&[], &[], true).unwrap();
        for id in MISE_USERS {
            assert!(
                selection.accepted.contains(&id.to_string()),
                "{label}: {id}"
            );
        }
    }
}

/// What the blocked wiring does: it is reported with the reason and a warning
/// about shells, and everything that goes through `devflow exec` is still
/// selected and applies.
fn assert_wiring_is_blocked_and_blocks_nothing(repo: &Repo, label: &str, naming: &[&str]) {
    let plan = repo.plan();
    let wiring = item(&plan, "mise:env-file");
    assert!(!wiring.is_actionable(), "{label}");
    for name in naming {
        assert!(
            wiring.conflicts[0].contains(name),
            "{label}: the reason names {name}: {:?}",
            wiring.conflicts
        );
    }
    assert!(
        !wiring.conflicts[0].contains("SENTINEL"),
        "{label}: no value: {:?}",
        wiring.conflicts
    );
    assert!(
        wiring.risks.iter().any(|r| r == SHELLS_WARNING),
        "{label}: {:?}",
        wiring.risks
    );
    let selection = plan.select(&[], &[], true).unwrap();
    for id in MISE_USERS {
        assert!(
            selection.accepted.contains(&id.to_string()),
            "{label}: {id}"
        );
    }
    let accepted = plan.with_dependencies(&["hook:setup:install".to_string()]);
    assert!(!accepted.contains(&"mise:env-file".to_string()), "{label}");
    apply(repo.root(), &plan, &accepted).unwrap_or_else(|error| panic!("{label}: {error:#}"));
}

/// Round 2, finding 1: `_.file = [".env", ".devflow/env", ".env"]` is
/// overridden by its last entry. The list used to be deduplicated at the first
/// position of each path, which hid it.
#[test]
fn round_2_finding_1_a_repeated_env_file_after_devflow_env_blocks_the_wiring() {
    let repo = with_mise_env("[env]\n_.file = [\".env\", \".devflow/env\", \".env\"]\n");
    assert_wiring_is_blocked_and_blocks_nothing(
        &repo,
        "repeated entry",
        &["mise.toml ([env] _.file)"],
    );
}

/// Round 2, finding 2: the wiring is added to `mise.toml`, and a
/// `mise.local.toml` that lists `.env` is read after it, so the proposal would
/// apply cleanly and be overridden the moment it is used. It used to be
/// checked only when `.devflow/env` was already in the file.
#[test]
fn round_2_finding_2_a_later_layer_blocks_a_newly_proposed_wiring() {
    for file in [
        "mise.local.toml",
        ".mise.local.toml",
        ".mise.toml",
        ".config/mise/config.local.toml",
    ] {
        // The wiring would go to an existing `mise.toml`...
        let repo = bun_next_prisma();
        repo.write(file, "[env]\n_.file = \".env\"\n");
        assert_wiring_is_blocked_and_blocks_nothing(
            &repo,
            file,
            &[file, "applied after mise.toml"],
        );

        // ...or create it (a `.mise.toml` alone would be the file to edit).
        if file == ".mise.toml" {
            continue;
        }
        let repo = bun_next_prisma();
        std::fs::remove_file(repo.root().join("mise.toml")).unwrap();
        repo.write(file, "[env]\n_.file = \".env\"\n");
        let plan = repo.plan();
        let wiring = item(&plan, "mise:env-file");
        assert!(!wiring.is_actionable(), "{file}: creating mise.toml");
        assert!(wiring.conflicts[0].contains(file), "{:?}", wiring.conflicts);
    }

    // A layer below the file the wiring goes to, or a file that lists nothing,
    // blocks nothing.
    for (file, text) in [
        (".config/mise.toml", "[env]\n_.file = \".env\"\n"),
        (".config/mise/config.toml", "[env]\n_.file = \".env\"\n"),
        ("mise.local.toml", "[tools]\nnode = \"22\"\n"),
        ("mise.local.toml", "[env]\nGREETING = \"hi\"\n"),
    ] {
        let repo = bun_next_prisma();
        repo.write(file, text);
        let plan = repo.plan();
        let wiring = item(&plan, "mise:env-file");
        assert!(wiring.is_actionable(), "{file}: {:?}", wiring.conflicts);
    }

    // A file for one `MISE_ENV` may or may not be read: a warning, not a block.
    let repo = bun_next_prisma();
    repo.write(
        "mise.production.toml",
        "[env]\n_.file = \".env.production\"\n",
    );
    let plan = repo.plan();
    let wiring = item(&plan, "mise:env-file");
    assert!(wiring.is_actionable());
    let note = "mise.production.toml ([env] _.file) (when MISE_ENV=production)";
    assert!(
        wiring.risks.iter().any(|r| r.contains(note)),
        "{:?}",
        wiring.risks
    );
    assert!(plan.warnings.iter().any(|w| w.contains(note)));
}

/// Round 2, finding 3: `mise exec -- bun install` runs `postinstall` scripts
/// with mise's environment, and those can migrate a database. Everything the
/// plan runs through mise goes through `devflow exec`, which sets the
/// workspace's variables after mise, so none of it depends on the wiring.
#[test]
fn round_2_finding_3_everything_run_through_mise_goes_through_devflow_exec() {
    let repo = bun_next_prisma();
    let plan = repo.plan();
    for id in [
        "hook:setup:install",
        "hook:setup:codegen",
        "hook:setup:migrate",
    ] {
        assert!(
            run(&plan, id).starts_with("mise exec -- devflow exec -- "),
            "{id}: {}",
            run(&plan, id)
        );
        assert!(
            !item(&plan, id)
                .depends_on
                .contains(&"mise:env-file".to_string()),
            "{id}: {:?}",
            item(&plan, id).depends_on
        );
    }
    // `mise install` runs no repository code, and `mise trust` is not proposed.
    assert!(
        !item(&plan, "hook:setup:mise")
            .depends_on
            .contains(&"mise:env-file".to_string())
    );
    let json = serde_json::to_string(&plan).unwrap();
    assert!(!json.contains("mise trust"), "{json}");
    // Deselecting the wiring takes nothing with it.
    let selection = plan.select(&[], &["mise:env-file".into()], true).unwrap();
    assert!(selection.dropped.is_empty(), "{:?}", selection.dropped);
    for id in MISE_USERS {
        assert!(selection.accepted.contains(&id.to_string()), "{id}");
    }

    // Without mise pinning tools, nothing runs through mise, and the daemon
    // has no `mise = true`; it is still wrapped.
    let repo = bun_next_prisma();
    repo.write("mise.toml", "[env]\nGREETING = \"hi\"\n");
    let plan = repo.plan();
    assert!(!run(&plan, "hook:setup:install").contains("mise"));
    assert!(
        !item(&plan, "hook:setup:install")
            .depends_on
            .contains(&"mise:env-file".to_string())
    );
}

/// What `devflow exec` cannot wrap: a mise task. Its own `env` is applied
/// after anything set before it, so it only gets the workspace's database
/// when mise loads `.devflow/env`, and it keeps depending on the wiring.
#[test]
fn a_mise_task_still_waits_for_the_wiring_because_devflow_exec_cannot_wrap_it() {
    let repo = bun_next_prisma();
    repo.write(
        "mise.toml",
        "[tools]\nnode = \"22\"\nbun = \"1.2\"\n\n[tasks.migrate]\nrun = \"bunx prisma migrate deploy\"\n",
    );
    let plan = repo.plan();
    assert_eq!(run(&plan, "hook:setup:migrate"), "mise run migrate");
    let migrate = item(&plan, "hook:setup:migrate");
    assert!(
        migrate.depends_on.contains(&"mise:env-file".to_string()),
        "{:?}",
        migrate.depends_on
    );
    assert!(
        migrate
            .risks
            .iter()
            .any(|r| r.contains("mise task") && r.contains("devflow exec")),
        "{:?}",
        migrate.risks
    );
    // The others do not.
    for id in ["hook:setup:install", "hook:setup:codegen", "daemon:web"] {
        assert!(
            !item(&plan, id)
                .depends_on
                .contains(&"mise:env-file".to_string()),
            "{id}"
        );
    }
    // Without the wiring the task is dropped; the rest stays.
    let selection = plan.select(&[], &["mise:env-file".into()], true).unwrap();
    assert!(
        selection
            .dropped
            .iter()
            .any(|d| d.id == "hook:setup:migrate" && d.reason.contains("mise:env-file")),
        "{:?}",
        selection.dropped
    );
    assert_eq!(selection.dropped.len(), 1, "{:?}", selection.dropped);
    assert!(
        selection
            .accepted
            .contains(&"hook:setup:install".to_string())
    );
    // And a blocked wiring blocks it, with the reason.
    let repo = bun_next_prisma();
    repo.write(
        "mise.toml",
        "[tools]\nbun = \"1.2\"\n\n[env]\n_.file = \".env\"\n\n[tasks.migrate]\nrun = \"x\"\n",
    );
    let plan = repo.plan();
    let selection = plan.select(&[], &[], true).unwrap();
    assert!(
        selection
            .dropped
            .iter()
            .any(|d| d.id == "hook:setup:migrate" && d.reason.contains("cannot be applied")),
        "{:?}",
        selection.dropped
    );
}

#[test]
fn deselecting_the_wiring_deselects_nothing_but_a_mise_task() {
    let repo = bun_next_prisma();
    let plan = repo.plan();
    assert!(item(&plan, "mise:env-file").is_actionable());
    let selection = plan.select(&[], &[], true).unwrap();
    for id in MISE_USERS {
        assert!(selection.accepted.contains(&id.to_string()), "{id}");
        assert!(
            !item(&plan, id)
                .depends_on
                .contains(&"mise:env-file".to_string()),
            "{id}"
        );
    }
    let selection = plan.select(&[], &["mise:env-file".into()], true).unwrap();
    for id in MISE_USERS {
        assert!(selection.accepted.contains(&id.to_string()), "{id}");
    }
    assert!(selection.dropped.is_empty(), "{:?}", selection.dropped);
    // Accepted without the wiring: apply goes ahead.
    let report = apply(
        repo.root(),
        &plan,
        &[
            "project:init".into(),
            "service:db".into(),
            "hook:setup:migrate".into(),
        ],
    )
    .unwrap();
    assert!(report.applied.contains(&"hook:setup:migrate".to_string()));

    // A mise file devflow cannot edit blocks the wiring and nothing else.
    let repo = bun_next_prisma();
    repo.write("mise.toml", "[tools\nnode = ");
    let plan = repo.plan();
    let wiring = item(&plan, "mise:env-file");
    assert!(!wiring.is_actionable());
    assert!(
        wiring.conflicts[0].contains("not valid TOML"),
        "{:?}",
        wiring.conflicts
    );
    assert!(wiring.risks.iter().any(|r| r == SHELLS_WARNING));
    assert!(
        plan.select(&[], &[], true)
            .unwrap()
            .accepted
            .contains(&"daemon:web".to_string())
    );
}

#[test]
fn selection_defaults_rejections_and_dependencies() {
    let repo = bun_next_prisma();
    let plan = repo.plan();

    // Rejecting the service drops what needs it, with the reason.
    let selection = plan.select(&[], &["service:db".into()], true).unwrap();
    assert!(!selection.accepted.contains(&"service:db".to_string()));
    for dependent in ["hook:setup:migrate", "env:preset:prisma"] {
        assert!(
            !selection.accepted.contains(&dependent.to_string()),
            "{dependent}"
        );
        assert!(
            selection
                .dropped
                .iter()
                .any(|d| d.id == dependent && d.reason.contains("service:db")),
            "{:?}",
            selection.dropped
        );
    }

    // Accepting a medium item adds it, and rejecting the wiring changes nothing for it.
    let selection = plan
        .select(&["daemon:worker".into()], &["mise:env-file".into()], true)
        .unwrap();
    assert!(selection.accepted.contains(&"daemon:worker".to_string()));
    assert!(!selection.accepted.contains(&"mise:env-file".to_string()));

    // Without defaults, only what was asked for.
    let selection = plan.select(&["project:init".into()], &[], false).unwrap();
    assert_eq!(selection.accepted, vec!["project:init"]);

    // Closure over dependencies, in plan order.
    let closure = plan.with_dependencies(&["hook:setup:migrate".into()]);
    assert_eq!(
        closure,
        vec!["project:init", "service:db", "hook:setup:migrate"]
    );

    // Unknown and contradictory ids are errors.
    assert!(plan.select(&["nope".into()], &[], true).is_err());
    assert!(
        plan.select(&["project:init".into()], &["project:init".into()], true)
            .is_err()
    );
}
