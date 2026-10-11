use super::*;
use crate::config::ServiceConfig;
use crate::error::NotInitialized;
use crate::remedy::{Problem, Remedy};

const ID: &str = "3f2b1c7e-0b3c-4d2e-9a7a-8f6d5c4b3a21";

const PRE_RELEASE: &str = r#"# tt2 project
[project]
name = "tt2"
id = "3f2b1c7e-0b3c-4d2e-9a7a-8f6d5c4b3a21"

[worktree]
copy = [".env"]

# The database server.
[postgres]
# devflow runs it.
mode = "managed"
image = "postgres:18" # the major matters
extra_databases = ["test"]

[hooks]
setup = ["bun install"]
"#;

fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git2::Repository::init(dir.path()).unwrap();
    for (name, text) in files {
        std::fs::write(dir.path().join(name), text).unwrap();
    }
    dir
}

fn problem(error: anyhow::Error) -> Problem {
    error.downcast_ref::<Problem>().expect("a Problem").clone()
}

fn read(dir: &tempfile::TempDir, name: &str) -> String {
    std::fs::read_to_string(dir.path().join(name)).unwrap()
}

#[test]
fn the_postgres_table_becomes_a_service_in_place_with_its_comments() {
    let dir = project(&[("devflow.toml", PRE_RELEASE)]);
    let migration = plan(dir.path()).unwrap();
    assert_eq!(migration.service_key, "db");
    assert_eq!(migration.files.len(), 1);
    let file = &migration.files[0];
    assert_eq!(file.target, Target::Project);
    assert_eq!(
        file.after,
        r#"# tt2 project
[project]
name = "tt2"
id = "3f2b1c7e-0b3c-4d2e-9a7a-8f6d5c4b3a21"

[worktree]
copy = [".env"]

# The database server.
[services.db]
engine = "postgres"
# devflow runs it.
mode = "managed"
image = "postgres:18" # the major matters
extra_databases = ["test"]

[hooks]
setup = ["bun install"]
"#
    );
    // Planning writes nothing.
    assert_eq!(read(&dir, "devflow.toml"), PRE_RELEASE);
    // The preview shows the change.
    assert!(
        file.preview.added >= 2 && file.preview.removed == 1,
        "{:?}",
        file.preview
    );
    assert!(
        file.preview
            .diff
            .iter()
            .any(|l| l.kind == DiffKind::Removed && l.text == "[postgres]")
    );
}

#[test]
fn the_migrated_configuration_means_the_same_thing() {
    let dir = project(&[("devflow.toml", PRE_RELEASE)]);
    // What the old table said, read the way the earlier build read it.
    let old: toml::Table = PRE_RELEASE.parse().unwrap();
    let section: crate::config::PostgresSection = old["postgres"].clone().try_into().unwrap();

    migrate(dir.path()).unwrap();
    let (config, _) = Config::load(dir.path()).unwrap();
    let (key, now) = config.postgres().unwrap();
    assert_eq!(key, "db");
    assert_eq!(*now, section);
    assert_eq!(config.project.id.to_string(), ID);
    assert_eq!(config.project.name, "tt2");
    assert_eq!(config.hooks.setup.len(), 1);
    // A receipt stamped with the section's generation stays current.
    assert_eq!(
        config.service_generation("db").unwrap(),
        ServiceConfig::Postgres(section).generation()
    );
    // Nothing is left to migrate.
    let again = problem(plan(dir.path()).unwrap_err());
    assert!(
        again.message.contains("nothing to migrate"),
        "{}",
        again.message
    );
}

#[test]
fn the_local_file_is_migrated_with_the_project_file() {
    let dir = project(&[
        ("devflow.toml", PRE_RELEASE),
        ("devflow.local.toml", "[postgres]\nport = 5544\n"),
    ]);
    let migration = plan(dir.path()).unwrap();
    assert_eq!(migration.files.len(), 2);
    assert_eq!(migration.files[0].target, Target::Project);
    assert_eq!(migration.files[1].target, Target::Local);
    // The overlay does not repeat `engine`: the project file has it.
    assert_eq!(migration.files[1].after, "[services.db]\nport = 5544\n");
    apply(&migration).unwrap();
    let (config, _) = Config::load(dir.path()).unwrap();
    assert_eq!(config.postgres().unwrap().1.port, 5544);
}

#[test]
fn a_local_only_postgres_table_gets_the_engine() {
    let project_text = "[project]\nname = \"p\"\nid = \"3f2b1c7e-0b3c-4d2e-9a7a-8f6d5c4b3a21\"\n";
    let dir = project(&[
        ("devflow.toml", project_text),
        (
            "devflow.local.toml",
            "[postgres]\nmode = \"external\"\nadmin_url = \"postgres://u@h/db\"\n",
        ),
    ]);
    let migration = plan(dir.path()).unwrap();
    assert_eq!(migration.files.len(), 1);
    assert_eq!(migration.files[0].target, Target::Local);
    assert!(
        migration.files[0]
            .after
            .starts_with("[services.db]\nengine = \"postgres\"\n")
    );
    apply(&migration).unwrap();
    assert!(Config::load(dir.path()).unwrap().0.postgres().is_some());
    // The project file was not touched.
    assert_eq!(read(&dir, "devflow.toml"), project_text);
}

#[test]
fn a_local_table_overlays_the_service_the_project_file_already_names() {
    let project_text = format!(
        "[project]\nname = \"p\"\nid = \"{ID}\"\n\n[services.main]\nengine = \"postgres\"\n"
    );
    let dir = project(&[
        ("devflow.toml", &project_text),
        ("devflow.local.toml", "[postgres]\nport = 7\n"),
    ]);
    let migration = plan(dir.path()).unwrap();
    assert_eq!(migration.service_key, "main");
    assert_eq!(migration.files[0].after, "[services.main]\nport = 7\n");
    apply(&migration).unwrap();
    let (config, _) = Config::load(dir.path()).unwrap();
    assert_eq!(
        config.postgres().map(|(k, p)| (k, p.port)),
        Some(("main", 7))
    );
}

#[test]
fn other_ways_of_writing_the_table_are_moved_too() {
    for (written, expected) in [
        (
            "postgres = { mode = \"managed\", port = 9 }\n",
            "[services.db]",
        ),
        (
            "postgres.mode = \"managed\"\npostgres.port = 9\n",
            "[services.db]",
        ),
    ] {
        // Root keys must precede the first header.
        let text = format!("{written}\n[project]\nname = \"p\"\nid = \"{ID}\"\n");
        let dir = project(&[("devflow.toml", &text)]);
        let migration = plan(dir.path()).unwrap_or_else(|e| panic!("{written}: {e:#}"));
        assert!(
            migration.files[0].after.contains(expected),
            "{}",
            migration.files[0].after
        );
        apply(&migration).unwrap();
        let (config, _) = Config::load(dir.path()).unwrap();
        assert_eq!(config.postgres().unwrap().1.port, 9, "{written}");
    }
}

#[test]
fn services_next_to_the_table_stay_where_they_are() {
    let text = format!(
        "[project]\nname = \"p\"\nid = \"{ID}\"\n\n[services.cache]\nengine = \"valkey\"\n\n[postgres]\nport = 3\n"
    );
    let dir = project(&[("devflow.toml", &text)]);
    migrate(dir.path()).unwrap();
    assert_eq!(
        read(&dir, "devflow.toml"),
        format!(
            "[project]\nname = \"p\"\nid = \"{ID}\"\n\n[services.cache]\nengine = \"valkey\"\n\n[services.db]\nengine = \"postgres\"\nport = 3\n"
        )
    );
    let (config, _) = Config::load(dir.path()).unwrap();
    assert_eq!(config.services.len(), 2);
}

#[test]
fn a_file_with_the_table_and_a_postgres_service_is_refused_and_untouched() {
    let text = format!(
        "[project]\nname = \"p\"\nid = \"{ID}\"\n\n[postgres]\nport = 3\n\n[services.main]\nengine = \"postgres\"\n"
    );
    let dir = project(&[("devflow.toml", &text)]);
    let refused = problem(plan(dir.path()).unwrap_err());
    assert!(refused.message.contains("both"), "{}", refused.message);
    assert!(!refused.message.contains("devflow "), "{}", refused.message);
    assert!(
        refused
            .remedies
            .iter()
            .any(|r| matches!(r, Remedy::OpenFile { .. }))
    );
    assert_eq!(read(&dir, "devflow.toml"), text);
}

#[test]
fn a_service_that_already_has_the_key_is_refused() {
    let text = format!(
        "[project]\nname = \"p\"\nid = \"{ID}\"\n\n[services.db]\nengine = \"valkey\"\n\n[postgres]\nport = 3\n"
    );
    let dir = project(&[("devflow.toml", &text)]);
    let refused = problem(plan(dir.path()).unwrap_err());
    assert!(
        refused.message.contains("already exists"),
        "{}",
        refused.message
    );
    assert_eq!(read(&dir, "devflow.toml"), text);
}

#[test]
fn zero_x_configuration_is_not_rewritten() {
    let text = format!(
        "[project]\nname = \"p\"\nid = \"{ID}\"\n\n[postgres]\nport = 3\n\n[agent]\nx = 1\n"
    );
    let dir = project(&[("devflow.toml", &text)]);
    let refused = problem(plan(dir.path()).unwrap_err());
    assert!(
        refused
            .remedies
            .iter()
            .any(|r| matches!(r, Remedy::OpenGuide { .. })),
        "{refused:?}"
    );
    assert_eq!(read(&dir, "devflow.toml"), text);
}

#[test]
fn an_invalid_result_is_refused_before_anything_is_written() {
    // `port` must be a number: the old table was never valid either.
    let text = format!("[project]\nname = \"p\"\nid = \"{ID}\"\n\n[postgres]\nport = \"x\"\n");
    let dir = project(&[("devflow.toml", &text)]);
    let refused = problem(plan(dir.path()).unwrap_err());
    assert!(
        refused.message.contains("still not be valid"),
        "{}",
        refused.message
    );
    assert!(
        !refused.message.contains("\"x\""),
        "no values: {}",
        refused.message
    );
    assert_eq!(read(&dir, "devflow.toml"), text);
}

#[test]
fn nothing_to_migrate_and_no_project_are_reported() {
    let current = format!("[project]\nname = \"p\"\nid = \"{ID}\"\n");
    let dir = project(&[("devflow.toml", &current)]);
    assert!(
        problem(plan(dir.path()).unwrap_err())
            .message
            .contains("nothing to migrate")
    );

    let empty = project(&[]);
    let error = plan(empty.path()).unwrap_err();
    assert!(
        error.downcast_ref::<NotInitialized>().is_some(),
        "{error:#}"
    );
}

#[test]
fn a_file_that_changed_since_the_plan_is_not_overwritten() {
    let dir = project(&[("devflow.toml", PRE_RELEASE)]);
    let migration = plan(dir.path()).unwrap();
    let edited = format!("{PRE_RELEASE}\n# someone typed this\n");
    std::fs::write(dir.path().join("devflow.toml"), &edited).unwrap();
    let error = apply(&migration).unwrap_err();
    assert!(error.downcast_ref::<ConfigChanged>().is_some(), "{error:#}");
    assert_eq!(read(&dir, "devflow.toml"), edited);
}

#[cfg(unix)]
#[test]
fn a_symbolic_link_is_not_rewritten() {
    let dir = project(&[]);
    let real = dir.path().join("real.toml");
    std::fs::write(&real, PRE_RELEASE).unwrap();
    std::os::unix::fs::symlink(&real, dir.path().join("devflow.toml")).unwrap();
    let migration = plan(dir.path()).unwrap();
    let error = apply(&migration).unwrap_err();
    assert!(format!("{error:#}").contains("symbolic link"), "{error:#}");
    assert_eq!(std::fs::read_to_string(&real).unwrap(), PRE_RELEASE);
}
