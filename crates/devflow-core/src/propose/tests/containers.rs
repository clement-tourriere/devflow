//! Compose services devflow branches itself: Valkey, Redis and MySQL 8.4.
use super::*;
use crate::config::{Config, Engine as Branch};
use serde_json::json;

/// A repository with `compose` as its compose file and `extra` files.
fn fixture(compose: &str, extra: &[(&str, &str)]) -> Repo {
    let mut files = vec![("compose.yaml", compose), ("README.md", "x\n")];
    files.extend_from_slice(extra);
    Repo::new(&files)
}

/// The compose file of one service called `name`.
fn one(name: &str, image: &str) -> String {
    format!("services:\n  {name}:\n    image: {image}\n")
}

fn service<'a>(plan: &'a Proposal, key: &str) -> &'a Item {
    item(plan, &format!("service:{key}"))
}

#[test]
fn each_valkey_and_redis_image_kind_becomes_a_service_item() {
    // (compose image, engine, image the item sets, a reason is recorded)
    let cases = [
        (
            "valkey/valkey:8-alpine",
            "valkey",
            "valkey/valkey:8-alpine",
            false,
        ),
        (
            "docker.io/valkey/valkey:8.0",
            "valkey",
            "docker.io/valkey/valkey:8.0",
            false,
        ),
        ("valkey:8", "valkey", "valkey:8", false),
        ("redis:7", "redis", "redis:7", false),
        ("redis", "redis", "redis", false),
        ("redis:7.2-alpine", "redis", "redis:7.2-alpine", false),
        (
            "docker.io/library/redis:8",
            "redis",
            "docker.io/library/redis:8",
            false,
        ),
        ("library/redis:7", "redis", "library/redis:7", false),
        // devflow's own image where the compose one is not a plain server it can run.
        ("redis:6.2", "redis", "redis:8-alpine", true),
        ("bitnami/redis:7.2", "redis", "redis:8-alpine", true),
        ("bitnami/valkey:8", "valkey", "valkey/valkey:8-alpine", true),
        ("ghcr.io/acme/redis:7", "redis", "redis:8-alpine", true),
        (
            "registry.example/acme/cache:valkey-8",
            "valkey",
            "valkey/valkey:8-alpine",
            true,
        ),
    ];
    for (image, engine, chosen, reason) in cases {
        let plan = fixture(&one("cache", image), &[]).plan();
        plan.validate().unwrap();
        let cache = service(&plan, "cache");
        assert_eq!(cache.kind, ItemKind::Service, "{image}");
        assert_eq!(cache.value["engine"], engine, "{image}");
        assert_eq!(cache.value["image"], chosen, "{image}");
        assert_eq!(cache.value["compose_service"], "cache", "{image}");
        assert!(cache.is_actionable(), "{image}");
        let noted = cache.risks.first().is_some_and(|r| r.contains(chosen));
        assert_eq!(noted, reason, "{image}: {:?}", cache.risks);
        // Confidence: recommended, except a repository devflow does not know.
        let expected = if image.contains("acme") {
            Confidence::Medium
        } else {
            Confidence::High
        };
        assert_eq!(cache.confidence, expected, "{image}");
        // devflow starts it when the item is accepted: compose has nothing
        // left to start. Declined, the compose hook starts it (see
        // `declining_a_service_item_keeps_its_compose_start`).
        for hook in ["hook:setup:compose-up", "hook:teardown:compose-down"] {
            assert_eq!(
                resolved_run(&plan, &["service:cache"], hook),
                None,
                "{image}: {hook}"
            );
        }
    }
}

#[test]
fn a_valkey_or_redis_service_is_checked_by_default_and_applies_to_a_valid_configuration() {
    let repo = fixture(&one("cache", "valkey/valkey:8-alpine"), &[]);
    let plan = repo.plan();
    assert!(defaults(&plan).contains(&"service:cache".to_string()));
    assert_eq!(
        service(&plan, "cache").value,
        json!({
            "key": "cache", "engine": "valkey", "image": "valkey/valkey:8-alpine",
            "env": "REDIS_URL", "compose_service": "cache",
        })
    );
    assert_eq!(
        service(&plan, "cache").depends_on,
        vec!["project:init".to_string()]
    );
    apply(repo.root(), &plan, &defaults(&plan)).unwrap();
    let (config, _) = Config::load(repo.root()).unwrap();
    let (key, engine, section) = config.containers().next().unwrap();
    assert_eq!((key, engine), ("cache", Branch::Valkey));
    assert_eq!(section.image, Some("valkey/valkey:8-alpine".to_string()));
    // The default variable is not written down.
    assert_eq!(section.env, None);
    assert_eq!(config.url_variable("cache"), Some("REDIS_URL"));
    // A second plan does not offer a duplicate. The only thing left is the
    // mise wiring a configured service now calls for.
    let second = repo.plan();
    assert_eq!(ids(&second), vec!["mise:env-file"]);
    assert!(
        second
            .warnings
            .iter()
            .any(|w| w.contains("already configured as services.cache")),
        "{:?}",
        second.warnings
    );
}

#[test]
fn the_url_variable_is_the_one_the_application_reads() {
    // CACHE_URL in a .env file: the service exports that and says so in the file.
    let repo = fixture(
        &one("cache", "redis:7"),
        &[(".env.example", "CACHE_URL=\nDATABASE_URL=\n")],
    );
    let plan = repo.plan();
    let cache = service(&plan, "cache");
    assert_eq!(cache.value["env"], "CACHE_URL");
    assert!(
        cache
            .evidence
            .contains(&".env.example:CACHE_URL".to_string()),
        "{:?}",
        cache.evidence
    );
    apply(repo.root(), &plan, &defaults(&plan)).unwrap();
    let (config, _) = Config::load(repo.root()).unwrap();
    assert_eq!(config.url_variable("cache"), Some("CACHE_URL"));
    assert!(repo.read("devflow.toml").contains("env = \"CACHE_URL\""));

    // The variable of an application service in the compose file counts too.
    let plan = repo_with_app("VALKEY_URL").plan();
    assert_eq!(service(&plan, "cache").value["env"], "VALKEY_URL");
    assert!(
        service(&plan, "cache")
            .evidence
            .contains(&"compose.yaml:services.app.environment.VALKEY_URL".to_string()),
        "{:?}",
        service(&plan, "cache").evidence
    );

    // Nothing names it: the default, and a risk that says so.
    let plan = fixture(&one("cache", "redis:7"), &[]).plan();
    let cache = service(&plan, "cache");
    assert_eq!(cache.value["env"], "REDIS_URL");
    assert!(
        cache
            .risks
            .iter()
            .any(|r| r.contains("exports it as REDIS_URL")),
        "{:?}",
        cache.risks
    );
}

fn repo_with_app(variable: &str) -> Repo {
    fixture(
        &format!(
            "services:\n  cache:\n    image: redis:7\n  app:\n    build: .\n    environment:\n      \
             - {variable}=redis://cache:6379\n"
        ),
        &[],
    )
}

#[test]
fn two_caches_never_export_the_same_variable() {
    let compose = "services:\n  cache:\n    image: redis:7\n  queue:\n    image: valkey/valkey:8\n";
    let repo = fixture(compose, &[(".env.example", "REDIS_URL=\n")]);
    let plan = repo.plan();
    assert_eq!(service(&plan, "cache").value["env"], "REDIS_URL");
    let queue = service(&plan, "queue");
    assert_eq!(queue.value["env"], "QUEUE_URL");
    assert!(
        queue.risks.iter().any(|r| r.contains("QUEUE_URL")),
        "{:?}",
        queue.risks
    );
    // The configuration it writes is valid: core refuses two services that share one.
    apply(repo.root(), &plan, &defaults(&plan)).unwrap();
    let (config, _) = Config::load(repo.root()).unwrap();
    assert_eq!(config.url_variable("cache"), Some("REDIS_URL"));
    assert_eq!(config.url_variable("queue"), Some("QUEUE_URL"));
}

#[test]
fn an_env_recipe_that_already_sets_the_default_variable_is_not_overwritten() {
    let repo = fixture(
        &one("cache", "redis:7"),
        &[(
            "devflow.toml",
            "[project]\nname = \"shop\"\nid = \"0b3c9d2e-7f4a-4c1e-9a52-6d1f3b8e2a40\"\n\n[env]\nREDIS_URL = \"redis://localhost:6379\"\n",
        )],
    );
    let plan = repo.plan();
    let cache = service(&plan, "cache");
    assert_ne!(cache.value["env"], "REDIS_URL");
    apply(repo.root(), &plan, &defaults(&plan)).unwrap();
    Config::load(repo.root()).unwrap();
}

#[test]
fn a_project_that_configures_its_cache_gets_no_duplicate() {
    let repo = fixture(
        "services:\n  redis:\n    image: redis:7\n  sessions:\n    image: redis:7\n",
        &[(
            "devflow.toml",
            "[project]\nname = \"shop\"\nid = \"0b3c9d2e-7f4a-4c1e-9a52-6d1f3b8e2a40\"\n\n[services.cache]\nengine = \"valkey\"\n",
        )],
    );
    let plan = repo.plan();
    // One configured service covers the first compose cache (matched in
    // order); the other is still offered, under another variable.
    assert!(plan.item("service:redis").is_none());
    let sessions = service(&plan, "sessions");
    assert_eq!(sessions.value["env"], "SESSIONS_URL");
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("`redis`") && w.contains("already configured as services.cache")),
        "{:?}",
        plan.warnings
    );
    // The configured service is devflow's; `sessions` is its own choice.
    assert_eq!(
        resolved_run(&plan, &["service:sessions"], "hook:setup:compose-up"),
        None
    );
    assert_eq!(
        resolved_run(&plan, &[], "hook:setup:compose-up").as_deref(),
        Some("docker compose up -d --wait sessions")
    );
}

#[test]
fn modules_and_topologies_are_not_proposed_and_say_which_kind_of_no() {
    for (image, kind) in [
        ("redis/redis-stack:latest", "not proposed automatically"),
        ("valkey/valkey-bundle:8", "not proposed automatically"),
        ("bitnami/redis-cluster:7", "unsupported"),
    ] {
        let plan = fixture(&one("cache", image), &[]).plan();
        assert!(plan.item("service:cache").is_none(), "{image}");
        let warning = plan
            .warnings
            .iter()
            .find(|w| w.contains(&format!("`cache` ({image})")))
            .unwrap_or_else(|| panic!("{image}: {:?}", plan.warnings));
        assert!(warning.contains(kind), "{image}: {warning}");
        assert!(
            warning.contains("README, \u{201c}Valkey and Redis branches\u{201d}"),
            "{warning}"
        );
        // Compose keeps starting it, and the hook says why devflow does not.
        let up = item(&plan, "hook:setup:compose-up");
        assert_eq!(up.value["run"], "docker compose up -d --wait cache");
        assert!(
            up.risks
                .iter()
                .any(|r| r.contains("`cache` is not branched") && r.contains(kind)),
            "{:?}",
            up.risks
        );
    }
    // A manual decision is told apart from an unsupported one.
    let plan = fixture(&one("cache", "redis/redis-stack"), &[]).plan();
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("To branch it anyway, configure it by hand")),
        "{:?}",
        plan.warnings
    );
    let plan = fixture(&one("cache", "bitnami/redis-cluster"), &[]).plan();
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("for what devflow supports")),
        "{:?}",
        plan.warnings
    );
}

#[test]
fn a_cache_behind_a_compose_profile_is_not_proposed() {
    let plan = fixture(
        "services:\n  cache:\n    image: redis:7\n    profiles: [extras]\n",
        &[],
    )
    .plan();
    assert!(plan.item("service:cache").is_none());
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("compose profiles (extras)")
                && w.contains("not proposed automatically")),
        "{:?}",
        plan.warnings
    );
}

#[test]
fn persistence_settings_and_existing_data_are_named_as_risks() {
    let plan = fixture(
        "services:\n  cache:\n    image: redis:7\n    environment:\n      REDIS_AOF_ENABLED: \"yes\"\n    volumes:\n      - ./redis.conf:/usr/local/etc/redis/redis.conf\n      - cachedata:/data\nvolumes:\n  cachedata:\n",
        &[],
    )
    .plan();
    let cache = service(&plan, "cache");
    let text = cache.risks.join("\n");
    assert!(text.contains("mounted configuration file"), "{text}");
    assert!(text.contains("AOF off") && text.contains("a source with AOF on is refused"));
    assert!(text.contains("volume is not carried over"), "{text}");
    assert!(
        text.contains("can stay in compose.yaml for other uses")
            && text.contains("use the URL devflow exports as REDIS_URL"),
        "{text}"
    );
}

// -- MySQL -------------------------------------------------------------------------

#[test]
fn mysql_8_4_is_proposed_as_a_preview_that_is_not_checked_by_default() {
    for image in [
        "mysql:8.4",
        "mysql:8.4.3",
        "docker.io/library/mysql:8.4-oracle",
    ] {
        let repo = fixture(&one("db", image), &[(".env.example", "DATABASE_URL=\n")]);
        let plan = repo.plan();
        plan.validate().unwrap();
        let sql = service(&plan, "mysql");
        assert_eq!(sql.confidence, Confidence::Medium, "{image}");
        assert!(sql.title.contains("preview"), "{}", sql.title);
        assert_eq!(
            sql.value,
            json!({
                "key": "mysql", "engine": "mysql", "image": image, "env": "DATABASE_URL",
                "compose_service": "db", "preview": true,
            }),
            "{image}"
        );
        assert!(!defaults(&plan).contains(&"service:mysql".to_string()));
        assert!(
            sql.risks.iter().any(|r| r.starts_with("Preview:")),
            "{:?}",
            sql.risks
        );
        assert_eq!(
            resolved_run(&plan, &["service:mysql"], "hook:setup:compose-up"),
            None,
            "{image}"
        );
        // Accepting it writes a configuration core loads.
        let accepted = vec!["project:init".to_string(), "service:mysql".to_string()];
        apply(repo.root(), &plan, &accepted).unwrap();
        let (config, _) = Config::load(repo.root()).unwrap();
        let (key, engine, section) = config.containers().next().unwrap();
        assert_eq!((key, engine), ("mysql", Branch::Mysql));
        assert_eq!(section.image.as_deref(), Some(image));
        assert_eq!(config.url_variable("mysql"), Some("DATABASE_URL"));
    }
}

#[test]
fn mysql_beside_postgres_exports_mysql_url_and_a_missing_variable_is_a_risk() {
    let plan = fixture(
        "services:\n  db:\n    image: postgres:18\n  mysql:\n    image: mysql:8.4\n",
        &[],
    )
    .plan();
    assert!(plan.item("service:db").is_some());
    let sql = service(&plan, "mysql");
    assert_eq!(sql.value["env"], "MYSQL_URL");
    assert!(
        sql.risks
            .iter()
            .any(|r| r.contains("exports it as MYSQL_URL")),
        "{:?}",
        sql.risks
    );
    // Both are valid together.
    let repo = repo_of(&plan);
    let accepted = vec![
        "project:init".to_string(),
        "service:db".to_string(),
        "service:mysql".to_string(),
    ];
    apply(repo.root(), &plan, &accepted).unwrap();
    let (config, _) = Config::load(repo.root()).unwrap();
    assert_eq!(config.url_variable("mysql"), Some("MYSQL_URL"));
}

/// The repository `plan` was made for: planning is deterministic, so a new
/// repository with the same files (the compose above) plans identically.
fn repo_of(_plan: &Proposal) -> Repo {
    fixture(
        "services:\n  db:\n    image: postgres:18\n  mysql:\n    image: mysql:8.4\n",
        &[],
    )
}

#[test]
fn other_mysql_versions_and_flavors_are_not_proposed_with_the_reason() {
    for (image, kind, reason) in [
        ("mysql:8.0", "unsupported", "MySQL 8.0"),
        ("mysql:5.7", "unsupported", "MySQL 5.7"),
        ("mysql:9.1", "unsupported", "MySQL 9.1"),
        ("percona/percona-server:8.4", "unsupported", "Percona"),
        (
            "mysql",
            "not proposed automatically",
            "does not say it is MySQL 8.4",
        ),
        (
            "mysql:latest",
            "not proposed automatically",
            "does not say it is MySQL 8.4",
        ),
        (
            "mysql:8",
            "not proposed automatically",
            "does not say it is MySQL 8.4",
        ),
    ] {
        let plan = fixture(&one("db", image), &[]).plan();
        assert!(plan.item("service:mysql").is_none(), "{image}");
        let warning = plan
            .warnings
            .iter()
            .find(|w| w.contains(&format!("`db` ({image})")))
            .unwrap_or_else(|| panic!("{image}: {:?}", plan.warnings));
        assert!(
            warning.contains(kind) && warning.contains(reason),
            "{warning}"
        );
        assert!(
            warning.contains("README, \u{201c}MySQL branches (preview)\u{201d}"),
            "{warning}"
        );
        // Compose keeps starting it.
        assert_eq!(
            item(&plan, "hook:setup:compose-up").value["run"],
            "docker compose up -d --wait db",
            "{image}"
        );
    }
}

#[test]
fn bitnami_mysql_8_4_gets_devflows_image_and_the_reason() {
    let plan = fixture(&one("db", "bitnami/mysql:8.4"), &[]).plan();
    let sql = service(&plan, "mysql");
    assert_eq!(sql.value["image"], "mysql:8.4");
    assert!(
        sql.risks
            .iter()
            .any(|r| r.contains("not the official MySQL image")),
        "{:?}",
        sql.risks
    );
}

#[test]
fn only_one_mysql_service_is_proposed_and_init_scripts_are_a_risk() {
    let plan = fixture(
        "services:\n  a:\n    image: mysql:8.4\n    volumes:\n      - ./init:/docker-entrypoint-initdb.d\n  b:\n    image: mysql:8.4\n",
        &[],
    )
    .plan();
    let sql = service(&plan, "mysql");
    assert_eq!(sql.value["compose_service"], "a");
    assert!(
        sql.risks
            .iter()
            .any(|r| r.contains("init scripts") && r.contains("./init")),
        "{:?}",
        sql.risks
    );
    assert!(
        sql.risks
            .iter()
            .any(|r| r.contains("At most one mysql service")),
        "{:?}",
        sql.risks
    );
    let warning = plan
        .warnings
        .iter()
        .find(|w| w.contains("`b` (mysql:8.4)"))
        .unwrap_or_else(|| panic!("{:?}", plan.warnings));
    assert!(
        warning.contains("at most one mysql service") && warning.contains("`a` is the one"),
        "{warning}"
    );
    assert_eq!(
        item(&plan, "hook:setup:compose-up").value["run"],
        "docker compose up -d --wait b"
    );
}

#[test]
fn a_configured_mysql_service_is_not_proposed_again() {
    let repo = fixture(
        &one("db", "mysql:8.4"),
        &[(
            "devflow.toml",
            "[project]\nname = \"shop\"\nid = \"0b3c9d2e-7f4a-4c1e-9a52-6d1f3b8e2a40\"\n\n[services.sql]\nengine = \"mysql\"\n",
        )],
    );
    let plan = repo.plan();
    // No second service; the configured one asks for the mise wiring.
    assert_eq!(ids(&plan), vec!["mise:env-file"]);
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("already configured as services.sql")),
        "{:?}",
        plan.warnings
    );
}

#[test]
fn the_plan_document_carries_the_new_items() {
    let plan = fixture(
        "services:\n  cache:\n    image: redis:7\n  sql:\n    image: mysql:8.4\n",
        &[],
    )
    .plan();
    let document = serde_json::to_value(&plan).unwrap();
    let items = document["items"].as_array().unwrap();
    let find = |id: &str| items.iter().find(|i| i["id"] == id).unwrap();
    let cache = find("service:cache");
    assert_eq!(cache["kind"], "service");
    assert_eq!(cache["confidence"], "high");
    assert_eq!(cache["patch"]["file"], "devflow.toml");
    let ops = cache["patch"]["ops"].as_array().unwrap();
    assert!(ops.len() >= 3, "{ops:?}");
    let sql = find("service:mysql");
    assert_eq!(sql["confidence"], "medium");
    assert_eq!(sql["value"]["preview"], true);
    // And it reads back and applies from the saved document.
    let back: Proposal = serde_json::from_value(document).unwrap();
    back.validate().unwrap();
    assert_eq!(back, plan);
}

// -- the workspace environment reaches daemons whatever the engine ---------------

/// A Vite project (`bun run dev`) with `compose`, and with or without mise.
fn vite_project(compose: &str, mise: bool) -> Repo {
    let mut files = vec![
        (
            "package.json",
            r#"{"name":"app","scripts":{"dev":"vite"},"devDependencies":{"vite":"5"}}"#,
        ),
        ("bun.lock", "{}"),
        ("vite.config.ts", "export default {};"),
        ("compose.yaml", compose),
    ];
    if mise {
        files.push(("mise.toml", "[tools]\nnode = \"22\"\nbun = \"1.2\"\n"));
    }
    Repo::new(&files)
}

#[test]
fn a_project_with_only_container_services_gets_the_workspace_environment() {
    let cases = [
        ("cache only", one("cache", "redis:7"), "service:cache"),
        ("mysql only", one("mysql", "mysql:8.4"), "service:mysql"),
        (
            "cache and postgres",
            "services:\n  db:\n    image: postgres:16\n  cache:\n    image: redis:7\n".to_string(),
            "service:cache",
        ),
    ];
    for (label, compose, service_id) in cases {
        for mise in [false, true] {
            let case = format!("{label}, mise={mise}");
            let plan = vite_project(&compose, mise).plan();
            plan.validate().unwrap();
            assert!(plan.item(service_id).is_some(), "{case}: {:?}", ids(&plan));
            // The daemon runs through `devflow exec`, which delivers
            // .devflow/env; mise only provides the tools.
            let web = item(&plan, "daemon:web");
            assert_eq!(
                web.value["run"], "devflow exec -- bun run dev --port $PORT --strictPort",
                "{case}"
            );
            assert_eq!(web.value["mise"], mise, "{case}");
            assert!(
                web.risks.iter().any(|r| r.contains("devflow exec")),
                "{case}: {:?}",
                web.risks
            );
            // And mise gets the wiring for shells and editors: created when
            // there is no mise.toml.
            let wiring = item(&plan, "mise:env-file");
            assert_eq!(wiring.value["creates_file"], !mise, "{case}");
            assert_eq!(
                wiring.confidence,
                if mise {
                    Confidence::High
                } else {
                    Confidence::Medium
                },
                "{case}"
            );
            assert!(
                wiring
                    .evidence
                    .iter()
                    .any(|e| e.contains("container service") || e.contains("DATABASE_URL")),
                "{case}: {:?}",
                wiring.evidence
            );
        }
    }
}

#[test]
fn container_services_in_the_configuration_count_as_well() {
    let repo = vite_project("services: {}\n", true);
    repo.write(
        "devflow.toml",
        "[project]\nname = \"shop\"\nid = \"0b3c9d2e-7f4a-4c1e-9a52-6d1f3b8e2a40\"\n\n[services.cache]\nengine = \"redis\"\n",
    );
    let plan = repo.plan();
    assert_eq!(
        item(&plan, "daemon:web").value["run"],
        "devflow exec -- bun run dev --port $PORT --strictPort"
    );
    assert!(plan.item("mise:env-file").is_some(), "{:?}", ids(&plan));
    // The same repository without the service is left as it was.
    repo.write(
        "devflow.toml",
        "[project]\nname = \"shop\"\nid = \"0b3c9d2e-7f4a-4c1e-9a52-6d1f3b8e2a40\"\n",
    );
    let plan = repo.plan();
    assert_eq!(
        item(&plan, "daemon:web").value["run"],
        "bun run dev --port $PORT --strictPort"
    );
    assert!(plan.item("mise:env-file").is_none(), "{:?}", ids(&plan));
}

#[test]
fn env_content_alone_gets_the_workspace_environment() {
    let repo = vite_project("services: {}\n", false);
    repo.write(
        "devflow.toml",
        "[project]\nname = \"shop\"\nid = \"0b3c9d2e-7f4a-4c1e-9a52-6d1f3b8e2a40\"\n\n[env]\nAPP_NAME = \"${DEVFLOW_WORKSPACE}\"\n",
    );
    let plan = repo.plan();
    assert_eq!(
        item(&plan, "daemon:web").value["run"],
        "devflow exec -- bun run dev --port $PORT --strictPort"
    );
    assert!(plan.item("mise:env-file").is_some(), "{:?}", ids(&plan));
}

#[test]
fn a_project_without_services_or_env_keeps_its_daemon_unwrapped() {
    let plan = vite_project("services: {}\n", true).plan();
    assert_eq!(
        item(&plan, "daemon:web").value["run"],
        "bun run dev --port $PORT --strictPort"
    );
    assert!(plan.item("mise:env-file").is_none(), "{:?}", ids(&plan));
}

// -- branch or compose is decided at selection ----------------------------------

const COMPOSE_HOOKS: [&str; 3] = [
    "hook:setup:compose-up",
    "hook:switch:compose-up",
    "hook:teardown:compose-down",
];

fn mysql_and_search() -> Repo {
    fixture(
        "services:\n  mysql:\n    image: mysql:8.4\n  search:\n    image: elasticsearch:8.13.0\n",
        &[],
    )
}

/// `project:init`, the compose hooks and `more`.
fn with_hooks(more: &[&str]) -> Vec<String> {
    let mut ids = vec!["project:init".to_string()];
    ids.extend(COMPOSE_HOOKS.iter().map(|s| s.to_string()));
    ids.extend(more.iter().map(|s| s.to_string()));
    ids
}

fn setup_runs(repo: &Repo) -> Vec<String> {
    let (config, _) = Config::load(repo.root()).unwrap();
    config.hooks.setup.iter().map(|h| h.run.clone()).collect()
}

#[test]
fn the_plan_shows_the_hook_for_the_services_devflow_always_leaves_to_compose() {
    let plan = mysql_and_search().plan();
    plan.validate().unwrap();
    let up = item(&plan, "hook:setup:compose-up");
    assert_eq!(up.value["run"], "docker compose up -d --wait search");
    // It says what it can start instead, and what decides it.
    let members = up.value["compose"]["members"].as_array().unwrap();
    assert_eq!(members.len(), 2, "{members:?}");
    assert_eq!(members[0]["service"], "mysql");
    assert_eq!(members[0]["branched_by"], "service:mysql");
    assert_eq!(members[1]["service"], "search");
    assert!(members[1].get("branched_by").is_none());
    assert!(
        up.risks
            .iter()
            .any(|r| r.contains("`mysql` is not started here while `service:mysql` is accepted")),
        "{:?}",
        up.risks
    );
    // And the service item says how to get it back.
    assert!(
        service(&plan, "mysql")
            .risks
            .iter()
            .any(|r| r.contains("--reject service:mysql")),
        "{:?}",
        service(&plan, "mysql").risks
    );
}

#[test]
fn declining_mysql_keeps_its_compose_start_in_the_hooks() {
    let repo = mysql_and_search();
    let plan = repo.plan();
    let digest = plan.digest();
    // MySQL (a preview, never a default) is left unchecked.
    let accepted = with_hooks(&[]);
    assert_eq!(
        resolved_run(&plan, &["project:init"], "hook:setup:compose-up").as_deref(),
        Some("docker compose up -d --wait mysql search")
    );
    let resolved = plan.resolved(&accepted);
    let up = item(&resolved, "hook:setup:compose-up");
    assert_eq!(up.value["run"], "docker compose up -d --wait mysql search");
    assert!(up.title.contains("mysql, search"), "{}", up.title);
    assert!(!up.risks.iter().any(|r| r.contains("is not started here")));
    assert!(up.evidence.iter().any(|e| e.contains("services.mysql")));
    assert_eq!(
        item(&resolved, "hook:switch:compose-up").value["run"],
        "docker compose up -d --wait mysql search"
    );
    assert!(
        item(&resolved, "hook:teardown:compose-down")
            .title
            .contains("mysql, search")
    );

    // The preview and the apply write what the selection says.
    let previews = preview(repo.root(), &plan, &accepted).unwrap();
    let file = previews
        .iter()
        .find(|p| p.file == "devflow.toml")
        .expect("devflow.toml");
    assert!(
        file.after
            .contains("docker compose up -d --wait mysql search"),
        "{}",
        file.after
    );
    let report = apply(repo.root(), &plan, &accepted).unwrap();
    assert!(!report.already_applied);
    assert_eq!(
        setup_runs(&repo),
        vec!["docker compose up -d --wait mysql search"]
    );
    let (config, _) = Config::load(repo.root()).unwrap();
    assert_eq!(config.containers().count(), 0, "MySQL is compose's");
    // Re-resolving never touched the plan: same digest, so a retry of the
    // same selection recognises the finished apply instead of a stale plan.
    assert_eq!(plan.digest(), digest);
    let again = apply(repo.root(), &plan, &accepted).unwrap();
    assert!(again.already_applied, "{again:?}");
}

#[test]
fn accepting_mysql_leaves_only_what_devflow_does_not_branch_to_compose() {
    let repo = mysql_and_search();
    let plan = repo.plan();
    let accepted = with_hooks(&["service:mysql"]);
    apply(repo.root(), &plan, &accepted).unwrap();
    assert_eq!(
        setup_runs(&repo),
        vec!["docker compose up -d --wait search"]
    );
    let (config, _) = Config::load(repo.root()).unwrap();
    assert_eq!(config.containers().count(), 1);
}

#[test]
fn a_rejected_cache_goes_back_to_compose_and_an_accepted_one_does_not() {
    let repo = fixture(
        "services:\n  cache:\n    image: redis:7\n  search:\n    image: elasticsearch:8.13.0\n",
        &[],
    );
    let plan = repo.plan();
    // The default selection: the cache is devflow's.
    let hooks: Vec<String> = COMPOSE_HOOKS.iter().map(|s| s.to_string()).collect();
    let selection = plan.select(&hooks, &[], true).unwrap();
    assert!(selection.accepted.contains(&"service:cache".to_string()));
    assert_eq!(
        plan.resolved(&selection.accepted)
            .item("hook:setup:compose-up")
            .unwrap()
            .value["run"],
        "docker compose up -d --wait search"
    );
    // `--reject service:cache`: compose starts it again.
    let selection = plan
        .select(&hooks, &["service:cache".to_string()], true)
        .unwrap();
    assert!(!selection.accepted.contains(&"service:cache".to_string()));
    apply(repo.root(), &plan, &selection.accepted).unwrap();
    assert_eq!(
        setup_runs(&repo),
        vec!["docker compose up -d --wait cache search"]
    );
    let (config, _) = Config::load(repo.root()).unwrap();
    assert_eq!(config.containers().count(), 0);
}

#[test]
fn a_project_with_only_a_cache_offers_compose_as_the_alternative() {
    let repo = fixture(&one("cache", "redis:7"), &[]);
    let plan = repo.plan();
    // Never a default: the cache is devflow's unless the person says otherwise.
    for hook in COMPOSE_HOOKS {
        let hook_item = item(&plan, hook);
        assert_eq!(hook_item.confidence, Confidence::Medium, "{hook}");
        assert!(!defaults(&plan).contains(&hook.to_string()), "{hook}");
    }
    assert_eq!(
        item(&plan, "hook:setup:compose-up").value["run"],
        "docker compose up -d --wait cache"
    );
    // Both accepted: nothing is left for compose, so the hooks drop out
    // instead of starting a second copy.
    let both = with_hooks(&["service:cache"]);
    let report = apply(repo.root(), &plan, &both).unwrap();
    for hook in COMPOSE_HOOKS {
        assert!(!report.applied.contains(&hook.to_string()), "{hook}");
        assert!(!report.manual.contains(&hook.to_string()), "{hook}");
    }
    assert!(report.applied.contains(&"service:cache".to_string()));
    assert!(setup_runs(&repo).is_empty());
}

#[test]
fn only_the_cache_rejected_the_hooks_start_it() {
    let repo = fixture(&one("cache", "redis:7"), &[]);
    let plan = repo.plan();
    apply(repo.root(), &plan, &with_hooks(&[])).unwrap();
    assert_eq!(setup_runs(&repo), vec!["docker compose up -d --wait cache"]);
}

#[test]
fn the_resolved_plan_survives_the_json_round_trip() {
    // `init --apply plan.json` reads the plan back from its document.
    let plan = mysql_and_search().plan();
    let back: Proposal = serde_json::from_value(serde_json::to_value(&plan).unwrap()).unwrap();
    assert_eq!(back, plan);
    let accepted = with_hooks(&[]);
    assert_eq!(back.resolved(&accepted), plan.resolved(&accepted));
}
