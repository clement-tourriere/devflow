//! Other stacks and awkward inputs.
use super::stacks::bun_next_prisma;
use super::*;

#[test]
fn laravel_with_postgres_gets_its_preset_and_artisan_migrations() {
    let repo = Repo::new(&[
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
        (
            "package.json",
            r#"{"scripts":{"dev":"vite","build":"vite build"},"devDependencies":{"vite":"5"}}"#,
        ),
        ("package-lock.json", "{}"),
        ("vite.config.js", "export default {};"),
        (
            ".env.example",
            "APP_KEY=\nDB_CONNECTION=pgsql\nDB_HOST=127.0.0.1\nDB_PORT=5432\nDB_DATABASE=\n",
        ),
        (".gitignore", "/vendor\n/node_modules\n.env\n"),
    ]);
    repo.mkdir("vendor");
    repo.write("vendor/autoload.php", "");
    let plan = repo.plan();
    assert_eq!(item(&plan, "service:db").confidence, Confidence::High);
    assert_eq!(
        run(&plan, "hook:setup:migrate"),
        "php artisan migrate --force --no-interaction"
    );
    assert_eq!(
        run(&plan, "hook:setup:install:php"),
        "composer install --no-interaction"
    );
    assert_eq!(
        item(&plan, "env:preset:laravel").confidence,
        Confidence::High
    );
    assert!(
        plan.item("env:preset:db_parts").is_none(),
        "Laravel owns the DB_* variables"
    );
    assert!(plan.item("copy:vendor").is_some());
    // `composer run dev` already starts Vite: one daemon, from the framework.
    let web = item(&plan, "daemon:web");
    assert_eq!(web.value["run"], "devflow exec -- composer run dev");
    assert!(web.value["port"].is_null());
    assert_eq!(web.confidence, Confidence::Medium);
    assert!(web.risks.iter().any(|r| r.contains("does not read PORT")));
}

#[test]
fn a_workspace_root_script_replaces_the_scripts_of_its_members() {
    let repo = Repo::new(&[
        (
            "package.json",
            r#"{
              "name": "acme",
              "private": true,
              "packageManager": "pnpm@9.12.0",
              "scripts": { "dev": "turbo run dev", "build": "turbo run build" },
              "devDependencies": { "turbo": "2" }
            }"#,
        ),
        ("pnpm-workspace.yaml", "packages:\n  - 'apps/*'\n"),
        ("pnpm-lock.yaml", "lockfileVersion: '9.0'\n"),
        ("turbo.json", "{ \"tasks\": {} }"),
        (
            "apps/web/package.json",
            r#"{"name":"@acme/web","scripts":{"dev":"next dev"},"dependencies":{"next":"15"}}"#,
        ),
        (
            "apps/api/package.json",
            r#"{"name":"@acme/api","scripts":{"start:dev":"nest start --watch","worker":"node dist/worker.js"},"dependencies":{"@nestjs/core":"10"}}"#,
        ),
        (".gitignore", "node_modules\n"),
    ]);
    repo.mkdir("node_modules/.pnpm");
    repo.mkdir("apps/web/node_modules");
    repo.write("node_modules/.pnpm/.keep", "");
    repo.write("apps/web/node_modules/.keep", "");
    let plan = repo.plan();
    assert_eq!(
        run(&plan, "hook:setup:install"),
        "pnpm install --frozen-lockfile --prefer-offline"
    );
    assert!(plan.item("copy:node_modules").is_some());
    assert!(plan.item("copy:apps/web/node_modules").is_some());
    let dev = item(&plan, "daemon:dev");
    assert_eq!(dev.value["run"], "pnpm run dev");
    assert_eq!(dev.confidence, Confidence::Medium);
    assert!(
        dev.risks.iter().any(|r| r.contains("whole workspace")),
        "{:?}",
        dev.risks
    );
    let daemons: Vec<_> = plan
        .items
        .iter()
        .filter(|i| i.kind == ItemKind::Daemon && i.id.starts_with("daemon:"))
        .collect();
    assert_eq!(daemons.len(), 1, "{:?}", ids(&plan));
}

#[test]
fn nested_applications_get_their_directory_and_their_own_names() {
    let repo = Repo::new(&[
        (
            "web/package.json",
            r#"{"name":"web","scripts":{"dev":"vite","worker":"node w.js"},"devDependencies":{"vite":"5"}}"#,
        ),
        ("web/bun.lock", "{}"),
        ("web/vite.config.ts", "export default {};"),
        (
            "api/manage.py",
            "import os\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'api.settings')\n# django\n",
        ),
        (
            "api/pyproject.toml",
            "[project]\nname = \"api\"\ndependencies = [\"django\"]\n",
        ),
        ("api/uv.lock", "version = 1\n"),
        ("api/polls/migrations/__init__.py", ""),
    ]);
    let plan = repo.plan();
    assert_eq!(
        run(&plan, "hook:setup:install:web"),
        "cd web && bun install --frozen-lockfile"
    );
    assert_eq!(
        run(&plan, "hook:setup:install:python:api"),
        "cd api && uv sync --frozen"
    );
    let web = item(&plan, "daemon:web");
    assert_eq!(web.value["dir"], "web");
    assert_eq!(web.value["run"], "bun run dev --port $PORT --strictPort");
    assert_eq!(web.value["port"], 5173);
    assert_eq!(item(&plan, "daemon:web-worker").value["dir"], "web");
}

#[test]
fn legacy_files_are_mentioned_and_left_alone() {
    let repo = Repo::new(&[(".devflow.yml", "services: []\n")]);
    let plan = repo.plan();
    assert_eq!(ids(&plan), vec!["project:init"]);
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains(".devflow.yml") && w.contains("0.x") && w.contains("MIGRATING")),
        "{:?}",
        plan.warnings
    );
    apply(repo.root(), &plan, &defaults(&plan)).unwrap();
    assert_eq!(repo.read(".devflow.yml"), "services: []\n");
}

#[test]
fn an_invalid_configuration_stops_planning() {
    let repo = Repo::new(&[("devflow.toml", "[project]\nname = \"x\"\n")]);
    let error = format!("{:#}", propose(repo.root(), &repo.options()).unwrap_err());
    assert!(error.contains("cannot plan"), "{error}");

    let repo = Repo::new(&[(".devflow.toml", "[hooks.post-create]\nmigrate = \"make\"\n")]);
    let error = format!("{:#}", propose(repo.root(), &repo.options()).unwrap_err());
    assert!(error.contains("MIGRATING"), "{error}");
}

#[test]
fn a_pitchfork_file_that_is_not_toml_only_costs_its_own_items() {
    let repo = bun_next_prisma();
    repo.write("pitchfork.toml", "this is = not [ toml\n");
    let plan = repo.plan();
    assert!(plan.item("daemon:web").is_none());
    assert!(
        plan.item("service:db").is_some(),
        "the rest of the plan stands"
    );
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("pitchfork.toml") && w.contains("daemon:web")),
        "{:?}",
        plan.warnings
    );
}

#[test]
fn a_local_overlay_that_replaces_hooks_is_flagged() {
    let repo = bun_next_prisma();
    repo.write("devflow.toml", &{
        let plan = repo.plan();
        apply(repo.root(), &plan, &defaults(&plan)).unwrap();
        repo.read("devflow.toml")
    });
    repo.write("devflow.local.toml", "[hooks]\nsetup = [\"echo local\"]\n");
    let plan = repo.plan();
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("devflow.local.toml") && w.contains("replaces")),
        "{:?}",
        plan.warnings
    );
    assert!(plan.input_hashes["devflow.local.toml"].is_some());
}

// -- compose volumes a project name cannot isolate ---------------------------

/// A repository whose compose file starts MongoDB (which devflow does not
/// branch) with a volume, plus `volumes`
/// at the top level of the file and the `extra` files.
fn compose_with_volumes(volumes: &str, extra: &[(&str, &str)]) -> Repo {
    let compose = format!(
        "services:\n  mongo:\n    image: mongo:7\n    volumes:\n      - cache:/data/db\n{volumes}"
    );
    let mut files = vec![("compose.yaml", compose.as_str()), ("README.md", "x\n")];
    files.extend_from_slice(extra);
    Repo::new(&files)
}

fn teardown(plan: &Proposal) -> &Item {
    item(plan, "hook:teardown:compose-down")
}

#[test]
fn a_volume_with_an_explicit_name_is_shared_and_never_deleted_by_the_teardown() {
    // The reported case: `name:` pins the volume to one Docker name, so the
    // workspace's COMPOSE_PROJECT_NAME does not give it a copy of its own.
    let repo = compose_with_volumes(
        "volumes:\n  cache:\n    name: shop-cache\n  scratch:\n",
        &[],
    );
    let plan = repo.plan();
    let down = teardown(&plan);
    assert_eq!(
        run(&plan, "hook:teardown:compose-down"),
        "docker compose down --remove-orphans || true"
    );
    let text = format!("{:?} {:?}", down.title, down.risks);
    assert!(!text.contains("delete their volumes"), "{text}");
    assert!(
        down.risks
            .iter()
            .any(|r| r.contains("`cache`") && r.contains("explicit name") && r.contains("down -v")),
        "the shared volume is named: {:?}",
        down.risks
    );
    assert!(
        !down.risks.iter().any(|r| r.contains("`scratch`")),
        "an ordinary volume is not blamed: {:?}",
        down.risks
    );
    // Nothing claims isolation, in the teardown or in the start hook.
    for id in ["hook:setup:compose-up", "hook:teardown:compose-down"] {
        for risk in &item(&plan, id).risks {
            assert!(
                !risk.contains("own empty copy") && !risk.contains("starts with empty data"),
                "{id}: {risk}"
            );
        }
    }
    assert!(
        item(&plan, "hook:setup:compose-up")
            .risks
            .iter()
            .any(|r| r.contains("`mongo` keeps data in the volume `cache`")
                && r.contains("main checkout")),
        "{:?}",
        item(&plan, "hook:setup:compose-up").risks
    );
    // And applying it writes the plain form.
    apply(repo.root(), &plan, &all(&plan)).unwrap();
    let written = repo.read("devflow.toml");
    assert!(
        written.contains("docker compose down --remove-orphans || true")
            && !written.contains("down -v"),
        "{written}"
    );
}

#[test]
fn an_external_volume_is_shared_too() {
    for definition in [
        "volumes:\n  cache:\n    external: true\n",
        "volumes:\n  cache:\n    external:\n      name: legacy-cache\n",
    ] {
        let plan = compose_with_volumes(definition, &[]).plan();
        assert_eq!(
            run(&plan, "hook:teardown:compose-down"),
            "docker compose down --remove-orphans || true",
            "{definition}"
        );
        assert!(
            teardown(&plan)
                .risks
                .iter()
                .any(|r| r.contains("`cache` (is external)")),
            "{:?}",
            teardown(&plan).risks
        );
    }
}

#[test]
fn volumes_that_cannot_be_read_confidently_get_the_non_destructive_teardown() {
    let safe = "docker compose down --remove-orphans || true";
    for (label, repo) in [
        (
            "include",
            compose_with_volumes(
                "volumes:\n  cache:\ninclude:\n  - other.yaml\n",
                &[("other.yaml", "volumes:\n  more:\n    name: shared\n")],
            ),
        ),
        (
            "override file",
            compose_with_volumes(
                "volumes:\n  cache:\n",
                &[(
                    "compose.override.yaml",
                    "volumes:\n  cache:\n    name: shop-cache\n",
                )],
            ),
        ),
        (
            "volumes that is not a mapping",
            compose_with_volumes("volumes: [cache]\n", &[]),
        ),
        (
            "a definition that is not a mapping",
            compose_with_volumes("volumes:\n  cache: nope\n", &[]),
        ),
        (
            "external that is not a boolean",
            compose_with_volumes("volumes:\n  cache:\n    external: maybe\n", &[]),
        ),
    ] {
        let plan = repo.plan();
        assert_eq!(run(&plan, "hook:teardown:compose-down"), safe, "{label}");
        let risks = teardown(&plan).risks.join(" ");
        assert!(risks.contains("could not read"), "{label}: {risks}");
        assert!(!risks.contains("never to the main checkout"), "{label}");
    }

    // A compose file that does not parse yields no compose hooks at all.
    let repo = Repo::new(&[
        ("compose.yaml", "services: [unclosed\n  - x: {\n"),
        ("README.md", "x\n"),
    ]);
    let plan = repo.plan();
    assert!(plan.item("hook:teardown:compose-down").is_none());
    assert!(plan.item("hook:setup:compose-up").is_none());
}

/// Round 2, finding 4: an override that is a symlink leaving the repository is
/// still merged by `docker compose`, and it can rename a volume to one the
/// main checkout uses. Detection drops such a link from its listing, so the
/// override has to be noticed from the directory entry itself.
#[cfg(unix)]
#[test]
fn an_override_the_scan_cannot_follow_still_prevents_the_destructive_teardown() {
    use std::os::unix::fs::symlink;
    let safe = "docker compose down --remove-orphans || true";
    let outside = tempfile::tempdir().unwrap();
    let external = outside.path().join("override.yaml");
    std::fs::write(&external, "volumes:\n  data:\n    name: shared-main-data\n").unwrap();

    type Setup = Box<dyn Fn(&Repo)>;
    let cases: Vec<(&str, Setup)> = vec![
        (
            "absolute link to a file outside the repository",
            Box::new({
                let external = external.clone();
                move |repo: &Repo| {
                    symlink(&external, repo.root().join("compose.override.yaml")).unwrap();
                }
            }),
        ),
        (
            "relative link out of the repository",
            Box::new(|repo: &Repo| {
                symlink(
                    "../../elsewhere.yaml",
                    repo.root().join("docker-compose.override.yml"),
                )
                .unwrap();
            }),
        ),
        (
            "dangling link",
            Box::new(|repo: &Repo| {
                symlink("nowhere.yaml", repo.root().join("compose.override.yml")).unwrap();
            }),
        ),
        (
            "override that is a directory",
            Box::new(|repo: &Repo| repo.mkdir("compose.override.yaml")),
        ),
        (
            "link to a file inside the repository",
            Box::new(|repo: &Repo| {
                repo.write("ci/override.yaml", "volumes:\n  cache:\n");
                symlink(
                    "ci/override.yaml",
                    repo.root().join("compose.override.yaml"),
                )
                .unwrap();
            }),
        ),
    ];
    for (label, setup) in cases {
        let repo = compose_with_volumes("volumes:\n  cache:\n", &[]);
        setup(&repo);
        let plan = repo.plan();
        assert_eq!(run(&plan, "hook:teardown:compose-down"), safe, "{label}");
        let risks = teardown(&plan).risks.join(" ");
        assert!(risks.contains("could not read"), "{label}: {risks}");
        assert!(!risks.contains("never to the main checkout"), "{label}");
        let facts = crate::detect::scan(repo.root()).unwrap();
        assert!(
            !facts.compose.as_ref().unwrap().volumes_complete,
            "{label}: volumes cannot be complete"
        );
    }

    // The main file itself: the first compose file name is a link the scan
    // cannot follow while a later one is readable. Docker would use the first.
    let repo = Repo::new(&[
        (
            "docker-compose.yml",
            "services:\n  mongo:\n    image: mongo:7\n    volumes:\n      - cache:/data/db\nvolumes:\n  cache:\n",
        ),
        ("README.md", "x\n"),
    ]);
    symlink(&external, repo.root().join("compose.yaml")).unwrap();
    let plan = repo.plan();
    assert_eq!(run(&plan, "hook:teardown:compose-down"), safe);

    // Without any override the isolating teardown stays.
    let plan = compose_with_volumes("volumes:\n  cache:\n", &[]).plan();
    assert_eq!(
        run(&plan, "hook:teardown:compose-down"),
        "docker compose down -v --remove-orphans || true"
    );
}

#[test]
fn project_scoped_volumes_keep_the_isolating_teardown() {
    // No `name:` and not external: Docker prefixes the workspace's project
    // name, so the volume is the workspace's own and `down -v` is safe.
    for volumes in ["volumes:\n  cache:\n", "volumes:\n  cache: {}\n", ""] {
        let plan = compose_with_volumes(volumes, &[]).plan();
        assert_eq!(
            run(&plan, "hook:teardown:compose-down"),
            "docker compose down -v --remove-orphans || true",
            "{volumes:?}"
        );
        assert!(
            teardown(&plan)
                .risks
                .iter()
                .any(|r| r.contains("never to the main checkout's project")),
            "{:?}",
            teardown(&plan).risks
        );
    }
}
