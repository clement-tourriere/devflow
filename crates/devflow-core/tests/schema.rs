//! The configuration and receipt contracts, against fixture files: shapes
//! devflow must accept, shapes it must refuse and how it words the refusal.
use devflow_core::config::{Config, Engine, MIGRATION_GUIDE, PostgresMode};

fn fixture(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/config")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn refusal(name: &str) -> String {
    match Config::parse(&fixture(name)) {
        Ok(config) => panic!("{name} must be refused, got {config:?}"),
        Err(error) => format!("{error:#}"),
    }
}

#[test]
fn the_current_shape_loads() {
    let config = Config::parse(&fixture("current.toml")).unwrap();
    let (key, pg) = config.postgres().expect("a postgres service");
    assert_eq!(key, "db");
    assert_eq!(pg.mode, PostgresMode::Managed);
    assert_eq!(pg.database.as_deref(), Some("shop"));
    assert_eq!(config.services["db"].engine(), Engine::Postgres);
    assert_eq!(config.hooks.setup, ["true"]);
}

#[test]
fn zero_point_x_files_are_refused_with_the_migration_guide() {
    for name in [
        "legacy-array-of-services.toml",
        "legacy-services-of-strings.toml",
    ] {
        let error = refusal(name);
        assert!(error.contains("0.x configuration"), "{name}: {error}");
        assert!(error.contains(MIGRATION_GUIDE), "{name}: {error}");
    }
}

#[test]
fn a_file_mixing_formats_names_the_conflicting_keys() {
    let error = refusal("mixed-current-and-legacy.toml");
    for key in [
        "mixes",
        "project.id",
        "services.db",
        "processes",
        "hooks.post-create",
    ] {
        assert!(error.contains(key), "{key}: {error}");
    }
}

#[test]
fn the_prerelease_postgres_table_says_what_to_write_instead() {
    let error = refusal("prerelease-postgres-table.toml");
    assert!(
        error.contains(
            "`[postgres]` is now `[services.db]` with `engine = \"postgres\"` \
             (see docs/MIGRATING.md)"
        ),
        "{error}"
    );
}

#[test]
fn malformed_services_are_refused_precisely() {
    let cases = [
        (
            "unsupported-engine.toml",
            "unsupported engine (supported: postgres, valkey, redis, mysql)",
        ),
        ("missing-engine.toml", "no `engine`"),
        ("unknown-field.toml", "immage"),
        ("bad-key.toml", "service key 'Main_DB'"),
        ("two-postgres.toml", "at most one postgres service"),
    ];
    for (name, expected) in cases {
        let error = refusal(name);
        assert!(error.contains(expected), "{name}: {error}");
    }
}

#[test]
fn the_shipped_examples_parse() {
    let examples = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    for name in ["devflow.toml", "devflow.external.toml"] {
        let text = std::fs::read_to_string(examples.join(name)).unwrap();
        let config = Config::parse(&text).unwrap_or_else(|e| panic!("{name}: {e:#}"));
        assert_eq!(config.postgres().map(|(key, _)| key), Some("db"), "{name}");
        if name == "devflow.toml" {
            // The named hook table and the [env] recipe the README points to.
            let named = config
                .hooks
                .setup
                .iter()
                .find(|h| h.name.is_some())
                .unwrap();
            assert_eq!(named.name.as_deref(), Some("migrate"));
            assert_eq!(named.timeout.as_deref(), Some("5m"));
            assert_eq!(config.env.vars["DIRECT_URL"], "${DATABASE_URL}");
        }
    }
}

#[test]
fn the_commented_container_service_of_the_example_is_valid_once_uncommented() {
    let examples = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let text = std::fs::read_to_string(examples.join("devflow.toml")).unwrap();
    let mut inside = false;
    let mut lines = Vec::new();
    for line in text.lines() {
        if line.starts_with("# [services.cache]") {
            inside = true;
        } else if inside && !line.starts_with("# ") {
            inside = false;
        }
        match line
            .strip_prefix("# ")
            .filter(|_| inside && !line.contains("Manage with"))
        {
            Some(rest) => lines.push(rest),
            None => lines.push(line),
        }
    }
    let config = Config::parse(&lines.join("\n")).unwrap();
    let (key, engine, section) = config.containers().next().expect("a container service");
    assert_eq!((key, engine), ("cache", Engine::Valkey));
    assert_eq!(section.env_name(engine, false), "REDIS_URL");
    assert_eq!(section.image(engine), "valkey/valkey:8-alpine");
    assert!(!section.require_cow);
}

#[test]
fn the_commented_mysql_service_of_the_example_is_valid_once_uncommented() {
    let examples = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let text = std::fs::read_to_string(examples.join("devflow.toml")).unwrap();
    let mut inside = false;
    let mut lines = Vec::new();
    for line in text.lines() {
        if line.starts_with("# [services.sql]") {
            inside = true;
        } else if inside && !line.starts_with("# ") {
            inside = false;
        }
        match line.strip_prefix("# ").filter(|_| inside) {
            Some(rest) => lines.push(rest),
            None => lines.push(line),
        }
    }
    let config = Config::parse(&lines.join("\n")).unwrap();
    let (key, engine, section) = config
        .containers()
        .find(|(key, ..)| *key == "sql")
        .expect("a mysql service");
    assert_eq!((key, engine), ("sql", Engine::Mysql));
    assert_eq!(section.image(engine), "mysql:8.4");
    assert_eq!(section.mysql_database("anything"), "shop");
    // The example has a postgres service, which keeps DATABASE_URL.
    assert_eq!(config.postgres().map(|(key, _)| key), Some("db"));
    assert_eq!(config.url_variable("sql"), Some("MYSQL_URL"));
}

#[test]
fn container_service_fixtures_are_accepted_and_refused_precisely() {
    let config = Config::parse(&fixture("container-services.toml")).unwrap();
    assert_eq!(config.containers().count(), 2);
    assert_eq!(config.postgres().map(|(key, _)| key), Some("db"));
    let cases = [
        (
            "container-services-same-variable.toml",
            "both export the same variable",
        ),
        (
            "container-services-postgres-option.toml",
            "`port` is not available for valkey services",
        ),
    ];
    for (name, expected) in cases {
        let error = refusal(name);
        assert!(error.contains(expected), "{name}: {error}");
    }
}
