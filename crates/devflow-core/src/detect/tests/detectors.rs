//! Focused checks of individual detectors.
use super::*;

fn scan_files(files: &[(&str, &str)]) -> Facts {
    let dir = fixture(files);
    scan_dir(&dir)
}

// -- JavaScript package managers ----------------------------------------------------

#[test]
fn package_manager_resolution() {
    struct Case {
        name: &'static str,
        files: Vec<(&'static str, &'static str)>,
        kind: PackageManager,
        berry: bool,
        source: PackageManagerSource,
        command: &'static str,
    }
    let cases = [
        Case {
            name: "bun.lockb",
            files: vec![("package.json", "{}"), ("bun.lockb", "")],
            kind: PackageManager::Bun,
            berry: false,
            source: PackageManagerSource::Lockfile,
            command: "bun install --frozen-lockfile",
        },
        Case {
            name: "pnpm lockfile",
            files: vec![("package.json", "{}"), ("pnpm-lock.yaml", "")],
            kind: PackageManager::Pnpm,
            berry: false,
            source: PackageManagerSource::Lockfile,
            command: "pnpm install --frozen-lockfile --prefer-offline",
        },
        Case {
            name: "yarn classic",
            files: vec![
                ("package.json", "{}"),
                ("yarn.lock", "# yarn lockfile v1\n"),
            ],
            kind: PackageManager::Yarn,
            berry: false,
            source: PackageManagerSource::Lockfile,
            command: "yarn install --frozen-lockfile",
        },
        Case {
            name: "yarn berry by .yarnrc.yml",
            files: vec![
                ("package.json", "{}"),
                ("yarn.lock", "# yarn lockfile v1\n"),
                (".yarnrc.yml", "nodeLinker: node-modules\n"),
            ],
            kind: PackageManager::Yarn,
            berry: true,
            source: PackageManagerSource::Lockfile,
            command: "yarn install --immutable",
        },
        Case {
            name: "yarn berry by __metadata",
            files: vec![
                ("package.json", "{}"),
                ("yarn.lock", "__metadata:\n  version: 8\n"),
            ],
            kind: PackageManager::Yarn,
            berry: true,
            source: PackageManagerSource::Lockfile,
            command: "yarn install --immutable",
        },
        Case {
            name: "yarn berry by packageManager",
            files: vec![("package.json", r#"{"packageManager":"yarn@4.1.0"}"#)],
            kind: PackageManager::Yarn,
            berry: true,
            source: PackageManagerSource::PackageManagerField,
            command: "yarn install",
        },
        Case {
            name: "yarn classic by packageManager",
            files: vec![
                ("package.json", r#"{"packageManager":"yarn@1.22.22"}"#),
                (".yarnrc.yml", ""),
            ],
            kind: PackageManager::Yarn,
            berry: false,
            source: PackageManagerSource::PackageManagerField,
            command: "yarn install",
        },
        Case {
            name: "npm lockfile",
            files: vec![("package.json", "{}"), ("package-lock.json", "{}")],
            kind: PackageManager::Npm,
            berry: false,
            source: PackageManagerSource::Lockfile,
            command: "npm install --prefer-offline --no-audit --no-fund",
        },
        Case {
            name: "no signal",
            files: vec![("package.json", "{}")],
            kind: PackageManager::Npm,
            berry: false,
            source: PackageManagerSource::Assumed,
            command: "npm install --prefer-offline --no-audit --no-fund",
        },
        Case {
            name: "packageManager beats the lockfile",
            files: vec![
                ("package.json", r#"{"packageManager":"pnpm@9.1.0"}"#),
                ("package-lock.json", "{}"),
            ],
            kind: PackageManager::Pnpm,
            berry: false,
            source: PackageManagerSource::PackageManagerField,
            command: "pnpm install --prefer-offline",
        },
        Case {
            name: "devEngines",
            files: vec![(
                "package.json",
                r#"{"devEngines":{"packageManager":{"name":"bun","version":"1.x"}}}"#,
            )],
            kind: PackageManager::Bun,
            berry: false,
            source: PackageManagerSource::DevEngines,
            command: "bun install",
        },
        Case {
            name: "pnpm workspace file without a lockfile",
            files: vec![
                ("package.json", "{}"),
                ("pnpm-workspace.yaml", "packages: []\n"),
            ],
            kind: PackageManager::Pnpm,
            berry: false,
            source: PackageManagerSource::ConfigFile,
            command: "pnpm install --prefer-offline",
        },
        Case {
            name: "unknown packageManager falls back to the lockfile",
            files: vec![
                ("package.json", r#"{"packageManager":"deno@2.0.0"}"#),
                ("yarn.lock", ""),
            ],
            kind: PackageManager::Yarn,
            berry: false,
            source: PackageManagerSource::Lockfile,
            command: "yarn install --frozen-lockfile",
        },
    ];
    for case in cases {
        let facts = scan_files(&case.files);
        let pm = &facts.js[0].package_manager;
        assert_eq!(pm.kind, case.kind, "{}", case.name);
        assert_eq!(pm.yarn_berry, case.berry, "{}", case.name);
        assert_eq!(pm.source, case.source, "{}", case.name);
        assert_eq!(
            facts.js_install_command(),
            Some(case.command),
            "{}",
            case.name
        );
        assert!(
            !facts.js_install_command().unwrap_or("").contains("npm ci"),
            "{}: `npm ci` deletes cloned node_modules",
            case.name
        );
        assert!(!pm.evidence.is_empty(), "{}", case.name);
    }
}

#[test]
fn conflicting_lockfiles_warn_and_pick_by_priority() {
    let facts = scan_files(&[
        ("package.json", "{}"),
        ("bun.lock", ""),
        ("package-lock.json", "{}"),
    ]);

    assert_eq!(facts.js[0].package_manager.kind, PackageManager::Bun);
    assert!(
        facts
            .warnings
            .iter()
            .any(|w| w.message.contains("several lockfiles")),
        "{:?}",
        facts.warnings
    );
}

#[test]
fn workspaces_from_package_json_forms() {
    let facts = scan_files(&[
        (
            "package.json",
            r#"{"workspaces":{"packages":["libs/**","tools/cli"]}}"#,
        ),
        ("libs/a/package.json", "{}"),
        ("libs/group/b/package.json", "{}"),
        ("libs/group/b/c/d/package.json", "{}"),
        ("libs/no-manifest/readme.md", ""),
        ("tools/cli/package.json", "{}"),
        ("tools/other/package.json", "{}"),
        ("libs/node_modules/x/package.json", "{}"),
    ]);

    let js = &facts.js[0];
    assert_eq!(
        js.workspaces.as_ref().expect("workspaces").source,
        WorkspaceSource::PackageJson
    );
    let paths: Vec<&str> = js.packages.iter().map(|p| p.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![".", "libs/a", "libs/group/b", "tools/cli"],
        "`**` stops two levels below the base; node_modules is never entered"
    );
}

#[test]
fn nested_project_cap_is_enforced_with_a_warning() {
    let dir = tempfile::tempdir().expect("tempdir");
    for i in 0..320 {
        write(dir.path(), &format!("apps/a{i:03}/package.json"), "{}");
    }
    write(dir.path(), "package.json", r#"{"workspaces":["apps/*"]}"#);
    let facts = scan(dir.path()).expect("scan");

    assert!(facts.js[0].packages.len() <= 300);
    assert!(
        facts
            .warnings
            .iter()
            .any(|w| w.message.contains("only the first 300")),
        "{:?}",
        facts.warnings
    );
}

#[test]
fn package_scripts_report_names_not_bodies() {
    let facts = scan_files(&[(
        "package.json",
        r#"{"name":"x","scripts":{"dev":"SECRET_TOKEN=VALUE_MUST_NOT_LEAK next dev","prebuild":"prisma generate","prepare":"prisma generate && husky"}}"#,
    )]);

    let package = &facts.js[0].packages[0];
    assert_eq!(package.scripts, vec!["dev", "prebuild", "prepare"]);
    assert_eq!(package.prisma_generate_scripts, vec!["prebuild", "prepare"]);
    assert!(
        package.generates_prisma_on_install(),
        "`prepare` runs on install"
    );
    assert!(
        !serde_json::to_string(&facts)
            .expect("json")
            .contains("VALUE_MUST_NOT_LEAK")
    );
}

// -- frameworks ----------------------------------------------------------------------

#[test]
fn vite_based_frameworks() {
    let vite = scan_files(&[
        ("package.json", r#"{"devDependencies":{"vite":"5"}}"#),
        ("vite.config.ts", ""),
    ]);
    let fact = framework(&vite, Framework::Vite).expect("vite");
    assert!(fact.vite_based);
    assert!(fact.evidence.contains(&"vite.config.ts".to_string()));

    let router = scan_files(&[(
        "package.json",
        r#"{"dependencies":{"react-router":"7"},"devDependencies":{"@react-router/dev":"7","vite":"6"}}"#,
    )]);
    assert!(framework(&router, Framework::ReactRouter).is_some_and(|f| f.vite_based));
    assert!(
        framework(&router, Framework::Vite).is_none(),
        "the meta-framework replaces the plain Vite fact"
    );

    let remix_vite = scan_files(&[(
        "package.json",
        r#"{"dependencies":{"@remix-run/react":"2"},"devDependencies":{"@remix-run/dev":"2","vite":"5"}}"#,
    )]);
    assert!(framework(&remix_vite, Framework::Remix).is_some_and(|f| f.vite_based));
    assert!(framework(&remix_vite, Framework::Vite).is_none());

    let remix_classic = scan_files(&[(
        "package.json",
        r#"{"dependencies":{"@remix-run/react":"2"},"devDependencies":{"@remix-run/dev":"2"}}"#,
    )]);
    assert!(framework(&remix_classic, Framework::Remix).is_some_and(|f| !f.vite_based));

    let svelte = scan_files(&[(
        "package.json",
        r#"{"devDependencies":{"@sveltejs/kit":"2","vite":"5"}}"#,
    )]);
    assert!(framework(&svelte, Framework::SvelteKit).is_some_and(|f| f.vite_based));
    assert!(framework(&svelte, Framework::Vite).is_none());

    let astro = scan_files(&[
        ("package.json", r#"{"dependencies":{"astro":"4"}}"#),
        ("astro.config.mjs", ""),
    ]);
    assert!(framework(&astro, Framework::Astro).is_some_and(|f| f.vite_based));

    let nest = scan_files(&[("package.json", r#"{"dependencies":{"@nestjs/core":"10"}}"#)]);
    assert!(framework(&nest, Framework::NestJs).is_some_and(|f| !f.vite_based));
}

#[test]
fn python_frameworks_from_requirements_and_pyproject() {
    let pip = scan_files(&[
        (
            "requirements.txt",
            "# web\nfastapi==0.110.0\nFlask[async]>=3 ; python_version >= '3.9'\n-r other.txt\ngit+https://example.com/x.git\n",
        ),
        ("requirements-dev.txt", "pytest\n"),
    ]);
    assert!(pip.has_framework(Framework::FastApi));
    assert!(pip.has_framework(Framework::Flask));
    assert_eq!(pip.python[0].tool, PythonTool::Pip);
    assert_eq!(
        pip.python[0].requirements_files,
        vec!["requirements-dev.txt", "requirements.txt"]
    );
    assert_eq!(
        pip.python_install_command(),
        None,
        "pip needs an environment first"
    );

    let poetry = scan_files(&[(
        "pyproject.toml",
        "[tool.poetry]\nname='x'\n\n[tool.poetry.dependencies]\npython='^3.12'\nFastAPI='*'\n\n[tool.poetry.group.dev.dependencies]\nFlask='*'\n",
    )]);
    assert_eq!(poetry.python[0].tool, PythonTool::Poetry);
    assert_eq!(
        poetry.python_install_command(),
        Some("poetry install --no-interaction")
    );
    assert!(poetry.has_framework(Framework::FastApi));
    assert!(poetry.has_framework(Framework::Flask));

    let uv_section = scan_files(&[(
        "pyproject.toml",
        "[project]\nname='x'\n\n[tool.uv]\ndev-dependencies=['flask']\n",
    )]);
    assert_eq!(uv_section.python[0].tool, PythonTool::Uv);
    assert!(uv_section.has_framework(Framework::Flask));

    // A manage.py that is not Django's is not Django.
    let other = scan_files(&[("manage.py", "print('hello')\n")]);
    assert!(!other.has_framework(Framework::Django));
}

#[test]
fn jvm_elixir_go_and_rust() {
    let spring = scan_files(&[
        (
            "build.gradle.kts",
            "plugins { id(\"org.springframework.boot\") version \"3.3.0\" }\ndependencies { runtimeOnly(\"org.postgresql:postgresql\") }\n",
        ),
        ("gradlew", "#!/bin/sh\n"),
    ]);
    assert!(framework(&spring, Framework::SpringBoot).is_some());
    let boot = process(&spring, "bootRun", ProcessSource::SpringBoot).expect("bootRun");
    assert_eq!(boot.display.as_deref(), Some("./gradlew bootRun"));
    assert!(spring.drivers.iter().any(|d| d.engine == Engine::Postgres));

    let maven = scan_files(&[(
        "pom.xml",
        "<project><parent><artifactId>spring-boot-starter-parent</artifactId></parent><dependency><artifactId>flyway-core</artifactId></dependency></project>",
    )]);
    assert!(maven.has_framework(Framework::SpringBoot));
    assert!(maven.has_migration_tool(MigrationTool::Flyway));
    assert_eq!(
        process(&maven, "bootRun", ProcessSource::SpringBoot).and_then(|p| p.display.as_deref()),
        Some("mvn spring-boot:run")
    );

    let phoenix = scan_files(&[
        (
            "mix.exs",
            "defp deps do\n  [\n    {:phoenix, \"~> 1.7\"},\n    {:ecto_sql, \"~> 3.10\"},\n    {:postgrex, \">= 0.0.0\"}\n  ]\nend\n",
        ),
        ("priv/repo/migrations/20240101_create_users.exs", ""),
        ("deps/phoenix/mix.exs", "{:not_a_dep, 1}"),
        (".gitignore", "/deps\n"),
    ]);
    assert!(phoenix.has_framework(Framework::Phoenix));
    let ecto = migration(&phoenix, MigrationTool::Ecto).expect("ecto");
    assert_eq!(ecto.migrations_dirs, vec!["priv/repo/migrations"]);
    assert_eq!(ecto.database, Some(Engine::Postgres));
    assert_eq!(
        process(&phoenix, "phx.server", ProcessSource::Phoenix).and_then(|p| p.display.as_deref()),
        Some("mix phx.server")
    );
    assert!(artifact(&phoenix, "deps").is_some_and(|a| a.kind == ArtifactKind::ElixirDeps));

    let go = scan_files(&[
        (
            "go.mod",
            "module example.com/app\n\ngo 1.22\n\nrequire github.com/lib/pq v1.10.9\n",
        ),
        ("go.sum", ""),
        ("cmd/tool/go.mod", "module example.com/tool\n"),
    ]);
    let go_fact = framework(&go, Framework::Go).expect("go");
    assert_eq!(go_fact.evidence, vec!["go.mod", "go.sum"]);
    assert_eq!(
        go.frameworks
            .iter()
            .filter(|f| f.framework == Framework::Go)
            .count(),
        1,
        "a module below another is not a second project"
    );
    assert!(
        go.drivers
            .iter()
            .any(|d| d.engine == Engine::Postgres && d.dependency == "github.com/lib/pq")
    );

    let rust = scan_files(&[
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.dependencies]\nsqlx = { version = \"0.8\", features = [\"mysql\"] }\n",
        ),
        ("Cargo.lock", ""),
        (
            "crates/api/Cargo.toml",
            "[package]\nname='api'\n[dependencies]\ndiesel = { version = '2', features = ['postgres'] }\n",
        ),
        ("crates/core/Cargo.toml", "[package]\nname='core'\n"),
    ]);
    assert_eq!(
        rust.frameworks
            .iter()
            .filter(|f| f.framework == Framework::Rust)
            .count(),
        1,
        "workspace members belong to the root"
    );
    assert!(rust.has_migration_tool(MigrationTool::Sqlx));
    assert!(
        migration(&rust, MigrationTool::Diesel)
            .is_some_and(|m| m.path == "crates/api" && m.database == Some(Engine::Postgres))
    );
    assert!(
        rust.drivers
            .iter()
            .any(|d| d.engine == Engine::Mysql && d.dependency == "sqlx")
    );
}

// -- migration tools ------------------------------------------------------------------

#[test]
fn migration_tools_by_convention() {
    let drizzle = scan_files(&[
        (
            "package.json",
            r#"{"devDependencies":{"drizzle-kit":"0.30"}}"#,
        ),
        (
            "drizzle.config.ts",
            "import { defineConfig } from 'drizzle-kit';\nexport default defineConfig({ dialect: 'postgresql', out: './db/drizzle', schema: './src/schema.ts' });\n",
        ),
        ("db/drizzle/0000_init.sql", ""),
    ]);
    let fact = migration(&drizzle, MigrationTool::Drizzle).expect("drizzle");
    assert_eq!(fact.migrations_dirs, vec!["db/drizzle"]);
    assert_eq!(fact.database, Some(Engine::Postgres));
    assert_eq!(fact.config_files, vec!["drizzle.config.ts"]);

    let knex = scan_files(&[
        ("package.json", r#"{"dependencies":{"knex":"3"}}"#),
        (
            "knexfile.js",
            "module.exports = { migrations: { directory: './db/migrations' } };",
        ),
        ("db/migrations/001_init.js", ""),
    ]);
    assert_eq!(
        migration(&knex, MigrationTool::Knex)
            .expect("knex")
            .migrations_dirs,
        vec!["db/migrations"]
    );

    let typeorm = scan_files(&[
        ("package.json", r#"{"dependencies":{"typeorm":"0.3"}}"#),
        ("src/migrations/1700000000-init.ts", ""),
    ]);
    assert_eq!(
        migration(&typeorm, MigrationTool::TypeOrm)
            .expect("typeorm")
            .migrations_dirs,
        vec!["src/migrations"]
    );

    let sequelize = scan_files(&[
        (
            "package.json",
            r#"{"devDependencies":{"sequelize-cli":"6"}}"#,
        ),
        (".sequelizerc", "module.exports = {};"),
        ("migrations/20240101-create.js", ""),
    ]);
    let fact = migration(&sequelize, MigrationTool::Sequelize).expect("sequelize");
    assert_eq!(fact.migrations_dirs, vec!["migrations"]);
    assert_eq!(fact.config_files, vec![".sequelizerc"]);

    let alembic = scan_files(&[
        (
            "alembic.ini",
            "[alembic]\nscript_location = %(here)s/db_migrations\n",
        ),
        ("db_migrations/versions/abc_init.py", ""),
        ("requirements.txt", "alembic\nsqlalchemy\n"),
    ]);
    let fact = migration(&alembic, MigrationTool::Alembic).expect("alembic");
    assert_eq!(fact.migrations_dirs, vec!["db_migrations/versions"]);

    let flask_migrate = scan_files(&[
        ("requirements.txt", "Flask-Migrate\n"),
        ("migrations/alembic.ini", ""),
        ("migrations/versions/abc_init.py", ""),
    ]);
    assert_eq!(
        migration(&flask_migrate, MigrationTool::Alembic)
            .expect("flask-migrate")
            .migrations_dirs,
        vec!["migrations/versions"]
    );

    let diesel = scan_files(&[
        (
            "Cargo.toml",
            "[dependencies]\ndiesel = { version = '2', features = ['mysql'] }\n",
        ),
        (
            "diesel.toml",
            "[migrations_directory]\ndir = \"db/migrate\"\n",
        ),
        ("db/migrate/2024-01-01-000000_init/up.sql", ""),
        ("migrations/.keep", ""),
    ]);
    let fact = migration(&diesel, MigrationTool::Diesel).expect("diesel");
    assert_eq!(fact.migrations_dirs, vec!["db/migrate", "migrations"]);
    assert_eq!(fact.database, Some(Engine::Mysql));

    let goose = scan_files(&[(
        "db/migrations/00001_init.sql",
        "-- +goose Up\ncreate table t();\n",
    )]);
    assert_eq!(
        migration(&goose, MigrationTool::Goose)
            .expect("goose")
            .migrations_dirs,
        vec!["db/migrations"]
    );

    let golang_migrate = scan_files(&[
        ("migrations/000001_init.up.sql", "create table t();"),
        ("migrations/000001_init.down.sql", "drop table t;"),
    ]);
    assert!(migration(&golang_migrate, MigrationTool::GolangMigrate).is_some());
    assert!(!golang_migrate.has_migration_tool(MigrationTool::Goose));

    let atlas = scan_files(&[
        ("atlas.hcl", "env \"local\" {}\n"),
        ("migrations/atlas.sum", "h1:x\n"),
        ("migrations/20240101_init.sql", "create table t();"),
    ]);
    let fact = migration(&atlas, MigrationTool::Atlas).expect("atlas");
    assert_eq!(fact.migrations_dirs, vec!["migrations"]);
    assert_eq!(fact.config_files, vec!["atlas.hcl"]);

    let dbmate = scan_files(&[(
        "db/migrations/20240101_init.sql",
        "-- migrate:up\ncreate table t();\n-- migrate:down\ndrop table t;\n",
    )]);
    assert!(migration(&dbmate, MigrationTool::Dbmate).is_some());
    assert!(!dbmate.has_migration_tool(MigrationTool::Goose));

    let flyway = scan_files(&[
        (
            "flyway.conf",
            "flyway.url=jdbc:postgresql://localhost/app\n",
        ),
        ("db/migration/V1__init.sql", ""),
    ]);
    let fact = migration(&flyway, MigrationTool::Flyway).expect("flyway");
    assert_eq!(fact.migrations_dirs, vec!["db/migration"]);

    let liquibase = scan_files(&[
        (
            "liquibase.properties",
            "changeLogFile=db/changelog/db.changelog-master.xml\n",
        ),
        ("db/changelog/db.changelog-master.xml", ""),
    ]);
    assert_eq!(
        migration(&liquibase, MigrationTool::Liquibase)
            .expect("liquibase")
            .migrations_dirs,
        vec!["db/changelog"]
    );
}

#[test]
fn prisma_variants() {
    // prisma.config.ts pointing at a schema folder, with migrations beside it.
    let folder = scan_files(&[
        ("package.json", r#"{"dependencies":{"@prisma/client":"7"}}"#),
        (
            "prisma.config.ts",
            "import { defineConfig } from 'prisma/config';\nexport default defineConfig({ schema: 'db/schema' });\n",
        ),
        (
            "db/schema/base.prisma",
            "datasource db {\n  provider = \"mysql\"\n}\ngenerator client {\n  provider = \"prisma-client\"\n  output = \"../generated\"\n}\n",
        ),
        ("db/schema/user.prisma", "model User { id Int @id }\n"),
        ("db/migrations/20240101_init/migration.sql", ""),
    ]);
    let prisma = migration(&folder, MigrationTool::Prisma).expect("prisma");
    assert_eq!(prisma.database, Some(Engine::Mysql));
    assert_eq!(
        prisma.generated_outputs,
        vec!["db/generated"],
        "output is relative to the schema folder"
    );
    assert_eq!(prisma.migrations_dirs, vec!["db/migrations"]);
    assert!(
        prisma
            .config_files
            .contains(&"prisma.config.ts".to_string())
    );
    assert!(
        prisma
            .config_files
            .contains(&"db/schema/base.prisma".to_string())
    );

    // package.json#prisma.schema, no migrations yet.
    let custom = scan_files(&[
        (
            "package.json",
            r#"{"prisma":{"schema":"database/schema.prisma"}}"#,
        ),
        (
            "database/schema.prisma",
            "datasource db {\n  provider = \"sqlite\"\n}\n",
        ),
    ]);
    let prisma = migration(&custom, MigrationTool::Prisma).expect("prisma");
    assert!(!prisma.has_migrations());
    assert_eq!(prisma.database, Some(Engine::Sqlite));

    // Only a dependency: still reported, with no files.
    let dep_only = scan_files(&[("package.json", r#"{"devDependencies":{"prisma":"6"}}"#)]);
    assert!(migration(&dep_only, MigrationTool::Prisma).is_some_and(|p| p.config_files.is_empty()));
}

// -- compose ----------------------------------------------------------------------------

#[test]
fn compose_anchors_variables_and_port_forms() {
    let facts = scan_files(&[
        (
            "docker-compose.yml",
            r#"name: shop
x-common: &common
  restart: unless-stopped
  environment:
    TZ: UTC
services:
  a:
    <<: *common
    image: mongo:7
  b:
    <<: *common
    image: "clickhouse/clickhouse-server:24.3"
    ports:
      - target: 8123
        published: 8123
        host_ip: 127.0.0.1
      - "9000"
  c:
    image: ${PG_IMAGE:-pgvector/pgvector:pg17}
    ports: ["${PGPORT}:5432"]
    profiles: [dev]
  d:
    build: .
    ports: ["3000:3000"]
  e:
    image: minio/minio
    volumes:
      - type: bind
        source: ./init
        target: /docker-entrypoint-initdb.d
      - data:/data
  f:
    image: ${UNKNOWN}
  g:
    image: postgres
    volumes:
      - ./sql/schema.sql:/docker-entrypoint-initdb.d/10-schema.sql:ro
volumes:
  data:
"#,
        ),
        ("compose.override.yml", "services: {}"),
    ]);
    let compose = facts.compose.as_ref().expect("compose");

    assert_eq!(compose.file, "docker-compose.yml");
    assert_eq!(compose.project_name.as_deref(), Some("shop"));
    let names: Vec<&str> = compose.services.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["a", "b", "c", "d", "e", "f", "g"],
        "sorted by name"
    );

    let a = service(&facts, "a");
    assert_eq!(a.engine, Engine::Mongodb);
    assert_eq!(a.environment_keys, vec!["TZ"], "the merge key is resolved");

    let b = service(&facts, "b");
    assert_eq!(b.engine, Engine::Clickhouse);
    assert_eq!(b.ports[0].host, Some(8123));
    assert_eq!(b.ports[0].host_ip.as_deref(), Some("127.0.0.1"));
    assert_eq!((b.ports[1].host, b.ports[1].container), (None, 9000));

    let c = service(&facts, "c");
    assert_eq!(c.image.as_deref(), Some("pgvector/pgvector:pg17"));
    assert_eq!(
        (c.engine, c.flavor.as_deref(), c.major_version),
        (Engine::Postgres, Some("pgvector"), Some(17))
    );
    assert_eq!(c.ports[0].host, None);
    assert_eq!(c.ports[0].host_variable.as_deref(), Some("PGPORT"));
    assert_eq!(c.profiles, vec!["dev"]);

    let d = service(&facts, "d");
    assert!(d.build && d.image.is_none());
    assert_eq!(d.engine, Engine::Other);

    let e = service(&facts, "e");
    assert_eq!(e.init_scripts, vec!["./init"]);
    assert!(e.volumes.iter().any(|v| v.bind));
    assert!(
        e.volumes
            .iter()
            .any(|v| !v.bind && v.source.as_deref() == Some("data"))
    );

    let f = service(&facts, "f");
    assert_eq!(
        f.image.as_deref(),
        Some("${UNKNOWN}"),
        "unresolved images stay as written"
    );
    assert_eq!(f.engine, Engine::Other);

    assert_eq!(service(&facts, "g").init_scripts, vec!["./sql/schema.sql"]);
}

#[test]
fn compose_file_priority_and_include_warning() {
    let facts = scan_files(&[
        (
            "docker-compose.yaml",
            "services:\n  old:\n    image: redis\n",
        ),
        (
            "compose.yml",
            "include:\n  - other.yaml\nservices:\n  new:\n    image: valkey/valkey:8\n",
        ),
    ]);
    let compose = facts.compose.as_ref().expect("compose");

    assert_eq!(compose.file, "compose.yml");
    assert_eq!(compose.files, vec!["compose.yml", "docker-compose.yaml"]);
    assert_eq!(compose.services.len(), 1);
    assert_eq!(compose.services[0].engine, Engine::Valkey);
    assert!(
        facts.warnings.iter().any(|w| w.message.contains("include")),
        "{:?}",
        facts.warnings
    );
}

#[test]
fn compose_without_services_is_still_reported() {
    let facts = scan_files(&[("compose.yaml", "name: empty\n")]);
    let compose = facts.compose.as_ref().expect("compose");
    assert!(compose.services.is_empty());
    assert_eq!(compose.project_name.as_deref(), Some("empty"));
    assert!(facts.compose_services(Engine::Postgres).is_empty());
}

// -- mise, version files, wiring -----------------------------------------------------

#[test]
fn mise_configuration_forms() {
    let facts = scan_files(&[
        (
            ".mise.toml",
            r#"[tools]
node = "22"
python = { version = "3.12" }

[env]
_ = { file = [".env", { path = "{{config_root}}/.devflow/env", redact = true }] }

[tasks]
build = "make"
"db:migrate" = { run = "x" }

[tasks.start]
run = "y"
"#,
        ),
        (".tool-versions", "ruby 3.3.0 # comment\nnodejs 22\n"),
        ("mise-tasks/serve", "#!/bin/sh\n"),
        ("mise-tasks/db/reset", "#!/bin/sh\n"),
    ]);
    let mise = facts.toolchain.mise.as_ref().expect("mise");

    // In the order mise layers them: the lowest precedence first.
    assert_eq!(mise.config_files, vec![".tool-versions", ".mise.toml"]);
    assert_eq!(mise.tools, vec!["node", "nodejs", "python", "ruby"]);
    assert_eq!(
        mise.tasks,
        vec!["build", "db:migrate", "db:reset", "serve", "start"]
    );
    assert_eq!(mise.env_files, vec![".env", "{{config_root}}/.devflow/env"]);
    assert!(facts.wiring.mise_loads_devflow_env);
    assert!(
        facts
            .wiring
            .evidence
            .contains(&".mise.toml:env._.file".to_string())
    );
    assert_eq!(
        process(&facts, "serve", ProcessSource::MiseTask).map(|p| p.role),
        Some(ProcessRole::Serve)
    );
    assert_eq!(
        process(&facts, "start", ProcessSource::MiseTask).and_then(|p| p.display.as_deref()),
        Some("mise run start")
    );
    assert!(process(&facts, "build", ProcessSource::MiseTask).is_none());
}

#[test]
fn mise_that_does_not_load_the_devflow_env() {
    let facts = scan_files(&[(
        "mise.toml",
        "[env]\n_.file = \".env\"\n[tools]\nnode='22'\n",
    )]);

    assert!(!facts.wiring.mise_loads_devflow_env);
    assert_eq!(
        facts.toolchain.mise.as_ref().expect("mise").env_files,
        vec![".env"]
    );
}

#[test]
fn version_files() {
    let facts = scan_files(&[
        (".nvmrc", "# pinned\nlts/iron\n"),
        (".python-version", "3.12.4\n"),
        (
            "rust-toolchain.toml",
            "[toolchain]\nchannel = \"1.96.0\"\ncomponents = [\"clippy\"]\n",
        ),
        (".ruby-version", ""),
    ]);

    let by_file = |name: &str| {
        facts
            .toolchain
            .version_files
            .iter()
            .find(|v| v.file == name)
            .unwrap_or_else(|| panic!("{name} missing"))
    };
    assert_eq!(by_file(".nvmrc").version.as_deref(), Some("lts/iron"));
    assert_eq!(
        by_file(".python-version").version.as_deref(),
        Some("3.12.4")
    );
    assert_eq!(
        by_file("rust-toolchain.toml").version.as_deref(),
        Some("1.96.0")
    );
    assert_eq!(by_file(".ruby-version").version, None);
    assert!(facts.toolchain.mise.is_none());
}

#[test]
fn devflow_pitchfork_and_legacy_wiring() {
    let facts = scan_files(&[
        (".devflow.toml", "[project]\nname='x'\n"),
        ("devflow.local.toml", ""),
        (".devflow.yml", "legacy: true\n"),
        (
            "pitchfork.toml",
            "[daemons.web]\nrun = 'x'\nmise = true\n\n[daemons.db]\nrun = 'y'\n",
        ),
    ]);

    assert_eq!(
        facts.wiring.devflow_config.as_deref(),
        Some(".devflow.toml")
    );
    assert_eq!(
        facts.wiring.devflow_local_config.as_deref(),
        Some("devflow.local.toml")
    );
    assert_eq!(facts.wiring.legacy_devflow_files, vec![".devflow.yml"]);
    let pitchfork = facts.wiring.pitchfork.as_ref().expect("pitchfork");
    let names: Vec<(&str, bool)> = pitchfork
        .daemons
        .iter()
        .map(|d| (d.name.as_str(), d.mise))
        .collect();
    assert_eq!(names, vec![("db", false), ("web", true)]);
    assert!(!pitchfork.all_daemons_use_mise);
    assert!(!facts.wiring.mise_loads_devflow_env);
    assert!(process(&facts, "web", ProcessSource::Pitchfork).is_some_and(|p| p.display.is_none()));
}

// -- env files -------------------------------------------------------------------------

#[test]
fn env_files_in_projects_and_their_categories() {
    let facts = scan_files(&[
        ("package.json", r#"{"workspaces":["apps/*"]}"#),
        ("apps/web/package.json", "{}"),
        (
            ".env.sample",
            "S3_BUCKET=a\nMONGODB_URI=b\nCLICKHOUSE_HOST=c\nPGHOST=d\nMYSQL_ROOT_PASSWORD=e\nRABBITMQ_URL=f\nSMTP_HOST=g\nOPENSEARCH_URL=h\nSECRET=i\n",
        ),
        ("apps/web/.env.local", "DB_HOST=x\nDATABASE_URL=y\n"),
        (".envrc", "export FOO=bar\n"),
        (".env.local.bak", "A=1\n"),
        ("config/.env", "NOT_SCANNED=1\n"),
    ]);

    let paths: Vec<&str> = facts.env.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, vec![".env.sample", "apps/web/.env.local"]);
    assert_eq!(facts.env.files[0].key_count, 9);
    assert!(facts.env.files[0].has_database_keys);
    let category = |name: &str| {
        facts
            .env
            .keys
            .iter()
            .find(|k| k.name == name)
            .map(|k| k.category)
    };
    assert_eq!(category("S3_BUCKET"), Some(EnvCategory::ObjectStorage));
    assert_eq!(category("MONGODB_URI"), Some(EnvCategory::Mongo));
    assert_eq!(category("CLICKHOUSE_HOST"), Some(EnvCategory::Clickhouse));
    assert_eq!(category("PGHOST"), Some(EnvCategory::Postgres));
    assert_eq!(category("MYSQL_ROOT_PASSWORD"), Some(EnvCategory::Mysql));
    assert_eq!(category("RABBITMQ_URL"), Some(EnvCategory::Broker));
    assert_eq!(category("SMTP_HOST"), Some(EnvCategory::Mail));
    assert_eq!(category("OPENSEARCH_URL"), Some(EnvCategory::Search));
    assert_eq!(category("DB_HOST"), Some(EnvCategory::Database));
    assert_eq!(category("SECRET"), None);
    assert_eq!(category("FOO"), None);
    let names: Vec<&str> = facts.env.keys.iter().map(|k| k.name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted);
}

#[test]
fn env_without_database_keys() {
    let facts = scan_files(&[(".env", "PORT=3000\nSECRET_KEY_BASE=x\nFEATURE_FLAG=1\n")]);

    assert!(!facts.env.has_database_keys);
    assert!(!facts.env.files[0].has_database_keys);
    assert_eq!(facts.env.files[0].key_count, 3);
    assert_eq!(
        facts.env.keys.len(),
        1,
        "only PORT is infrastructure-looking"
    );
    assert_eq!(facts.env.keys[0].category, EnvCategory::Port);
}

// -- artifacts and Git -------------------------------------------------------------------

#[test]
fn without_git_nothing_is_a_copy_candidate() {
    let facts = scan_files(&[
        ("package.json", "{}"),
        ("node_modules/.keep", ""),
        (".env", "A=1\n"),
    ]);

    assert!(!facts.git.repository);
    let node_modules = artifact(&facts, "node_modules").expect("node_modules");
    assert_eq!(node_modules.git_ignored, None);
    assert!(!node_modules.is_copy_candidate());
    assert_eq!(artifact(&facts, ".env").expect(".env").git_ignored, None);
}

#[test]
fn tracked_files_are_not_ignored_even_when_a_rule_matches() {
    let dir = fixture(&[
        (".gitignore", ".env*\n"),
        (".env", "A=1\n"),
        (".env.local", "A=1\n"),
        ("package.json", "{}"),
    ]);
    let repo = git2::Repository::init(dir.path()).expect("init");
    let mut index = repo.index().expect("index");
    index
        .add_path(Path::new(".env"))
        .expect("add a force-tracked file");
    index.write().expect("write index");
    let facts = scan_dir(&dir);

    assert_eq!(
        artifact(&facts, ".env").expect(".env").git_ignored,
        Some(false)
    );
    assert_eq!(
        artifact(&facts, ".env.local")
            .expect(".env.local")
            .git_ignored,
        Some(true)
    );
}

#[test]
fn directories_with_ignored_contents_count_as_ignored() {
    let dir = fixture(&[
        ("Gemfile", "gem 'rails'\n"),
        ("bin/rails", ""),
        ("config/application.rb", ""),
        ("tmp/.keep", ""),
        ("log/.keep", ""),
        ("public/build/app.js", ""),
        (
            ".gitignore",
            "/tmp/*\n!/tmp/.keep\n/log/*\n!/log/.keep\n/public/build/\n",
        ),
    ]);
    git_init(dir.path());
    let facts = scan_dir(&dir);

    for path in ["tmp", "log", "public/build"] {
        assert_eq!(
            artifact(&facts, path)
                .unwrap_or_else(|| panic!("{path}"))
                .git_ignored,
            Some(true),
            "{path}"
        );
    }
}

#[test]
fn a_tracked_vendor_directory_is_not_ignored() {
    let dir = fixture(&[
        ("composer.json", "{}"),
        ("vendor/autoload.php", ""),
        (".gitignore", "/node_modules\n"),
    ]);
    git_init(dir.path());
    let facts = scan_dir(&dir);

    let vendor = artifact(&facts, "vendor").expect("vendor");
    assert_eq!(vendor.git_ignored, Some(false));
    assert!(!vendor.is_copy_candidate());
}

// -- serialization and helpers --------------------------------------------------------------

#[test]
fn facts_serialize_with_snake_case_names() {
    let facts = scan_files(&[
        (
            "package.json",
            r#"{"packageManager":"bun@1.1.0","dependencies":{"next":"15"}}"#,
        ),
        ("compose.yaml", "services:\n  db:\n    image: postgres:16\n"),
        (".env", "DATABASE_URL=x\n"),
    ]);
    let value = serde_json::to_value(&facts).expect("serialize");

    for key in [
        "root",
        "git",
        "toolchain",
        "js",
        "python",
        "frameworks",
        "migrations",
        "drivers",
        "compose",
        "env",
        "processes",
        "artifacts",
        "wiring",
        "warnings",
    ] {
        assert!(value.get(key).is_some(), "missing top-level key {key}");
    }
    assert_eq!(value["js"][0]["package_manager"]["kind"], "bun");
    assert_eq!(
        value["js"][0]["package_manager"]["source"],
        "package_manager_field"
    );
    assert_eq!(value["frameworks"][0]["framework"], "next_js");
    assert_eq!(value["compose"]["services"][0]["engine"], "postgres");
    assert_eq!(value["env"]["keys"][0]["category"], "database_url");
    assert_eq!(value["env"]["has_database_keys"], true);
}

#[test]
fn scanning_twice_gives_identical_facts() {
    let dir = fixture(&[
        (
            "package.json",
            r#"{"workspaces":["apps/*"],"packageManager":"pnpm@9"}"#,
        ),
        ("apps/a/package.json", r#"{"dependencies":{"next":"15"}}"#),
        ("apps/b/package.json", r#"{"dependencies":{"vite":"5"}}"#),
        (
            "compose.yaml",
            "services:\n  z:\n    image: redis\n  a:\n    image: postgres:17\n",
        ),
        (".env", "REDIS_URL=1\nDATABASE_URL=2\n"),
    ]);

    assert_eq!(scan_dir(&dir), scan_dir(&dir));
}

#[test]
fn the_scan_does_not_modify_the_repository() {
    let dir = fixture(&[
        ("package.json", "{}"),
        (".env", "A=1\n"),
        ("compose.yaml", "services: {}\n"),
    ]);
    git_init(dir.path());
    let snapshot = |root: &Path| {
        let mut entries: Vec<(String, u64)> = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(path) = stack.pop() {
            for entry in std::fs::read_dir(&path).expect("read_dir").flatten() {
                let meta = entry.metadata().expect("metadata");
                let name = entry
                    .path()
                    .strip_prefix(root)
                    .expect("prefix")
                    .display()
                    .to_string();
                if meta.is_dir() {
                    // Git may refresh its own bookkeeping; the working tree is what matters.
                    if name != ".git" {
                        stack.push(entry.path());
                    }
                } else {
                    entries.push((name, meta.len()));
                }
            }
        }
        entries.sort();
        entries
    };
    let before = snapshot(dir.path());
    let _ = scan_dir(&dir);

    assert_eq!(before, snapshot(dir.path()));
}
