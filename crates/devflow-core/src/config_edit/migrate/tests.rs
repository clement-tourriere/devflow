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

// -- review fixes ------------------------------------------------------------

fn shown(migration: &Migration) -> String {
    let mut out = String::new();
    for file in &migration.files {
        for line in &file.preview.diff {
            out.push_str(&line.text);
            out.push('\n');
        }
        out.push_str(&file.preview.after);
        out.push_str(&format!("{file:?}"));
    }
    out
}

#[test]
fn a_preview_never_shows_a_credential_of_the_context() {
    // The credentials sit right before the table, so the diff's context has them.
    let local = "\
[env]
KEEP = \"${PGHOST}\" # fine
CACHE_URL = \"postgres://app:urlpass-xyz@h/db\"
DB_PASS = 'hunter2-db'
SESSION_KEY = \"s3cr3t-session\"
[postgres]
port = 5544
";
    let project_text = format!("[project]\nname = \"p\"\nid = \"{ID}\"\n");
    let dir = project(&[
        ("devflow.toml", &project_text),
        ("devflow.local.toml", local),
    ]);
    let migration = plan(dir.path()).unwrap();
    let text = shown(&migration);
    for secret in ["s3cr3t-session", "hunter2-db", "urlpass-xyz"] {
        assert!(!text.contains(secret), "{secret} leaked:\n{text}");
    }
    // The change itself is still readable.
    let diff = &migration.files[0].preview.diff;
    assert!(
        diff.iter()
            .any(|l| l.kind == DiffKind::Removed && l.text == "[postgres]")
    );
    assert!(diff.iter().any(|l| l.text.contains("SESSION_KEY")));
    // What is applied is the real text.
    apply(&migration).unwrap();
    let written = read(&dir, "devflow.local.toml");
    assert!(written.contains("s3cr3t-session") && written.contains("hunter2-db"));
    assert!(written.contains("[services.db]"));
}

#[test]
fn a_service_of_another_engine_in_the_project_file_is_not_turned_into_postgres() {
    let project_text =
        format!("[project]\nname = \"p\"\nid = \"{ID}\"\n\n[services.db]\nengine = \"valkey\"\n");
    let local = "[postgres]\nmode = \"managed\"\n";
    let dir = project(&[
        ("devflow.toml", &project_text),
        ("devflow.local.toml", local),
    ]);
    let refused = problem(plan(dir.path()).unwrap_err());
    assert!(
        refused.message.contains("already exists") && refused.message.contains("valkey"),
        "{}",
        refused.message
    );
    assert_eq!(read(&dir, "devflow.toml"), project_text);
    assert_eq!(read(&dir, "devflow.local.toml"), local);

    // And the other way round: the local file's service wins the merge.
    let project_pg = format!("[project]\nname = \"p\"\nid = \"{ID}\"\n\n[postgres]\nport = 1\n");
    let local_other = "[services.db]\nengine = \"redis\"\n";
    let dir = project(&[
        ("devflow.toml", &project_pg),
        ("devflow.local.toml", local_other),
    ]);
    assert!(
        problem(plan(dir.path()).unwrap_err())
            .message
            .contains("redis")
    );
}

#[test]
fn a_local_file_created_after_the_plan_stops_the_apply() {
    let dir = project(&[("devflow.toml", PRE_RELEASE)]);
    let migration = plan(dir.path()).unwrap();
    assert_eq!(migration.files.len(), 1);
    let local = "[postgres]\nport = 5544\n";
    std::fs::write(dir.path().join("devflow.local.toml"), local).unwrap();
    let error = apply(&migration).unwrap_err();
    let changed = error
        .downcast_ref::<ConfigChanged>()
        .expect("ConfigChanged");
    assert!(changed.path.ends_with("devflow.local.toml"));
    assert_eq!(read(&dir, "devflow.toml"), PRE_RELEASE);
    assert_eq!(read(&dir, "devflow.local.toml"), local);
}

#[test]
fn a_different_project_file_after_the_plan_stops_the_apply() {
    let dir = project(&[(".devflow.toml", PRE_RELEASE)]);
    let migration = plan(dir.path()).unwrap();
    std::fs::write(dir.path().join("devflow.toml"), "[project]\nname = \"x\"\n").unwrap();
    let error = apply(&migration).unwrap_err();
    assert!(error.downcast_ref::<ConfigChanged>().is_some(), "{error:#}");
    assert_eq!(read(&dir, ".devflow.toml"), PRE_RELEASE);
}

#[test]
fn a_failed_second_write_puts_the_first_file_back() {
    let dir = project(&[
        ("devflow.toml", PRE_RELEASE),
        ("devflow.local.toml", "[postgres]\nport = 5544\n"),
    ]);
    let migration = plan(dir.path()).unwrap();
    assert_eq!(migration.files.len(), 2);
    // A save lands in the local file just before it is written.
    let local_path = dir.path().join("devflow.local.toml");
    let edited = "[postgres]\nport = 5544\n# edited meanwhile\n";
    let calls = std::cell::Cell::new(0);
    let _guard = super::super::seams::install(super::super::seams::Hooks {
        before_write: Some(Box::new({
            let local_path = local_path.clone();
            move || {
                calls.set(calls.get() + 1);
                if calls.get() == 2 {
                    std::fs::write(&local_path, edited).unwrap();
                }
            }
        })),
        ..Default::default()
    });
    let error = apply(&migration).unwrap_err();
    let report = format!("{error:#}");
    assert!(error.downcast_ref::<ConfigChanged>().is_some(), "{report}");
    assert!(
        report.contains("devflow.toml was put back") && report.contains("nothing was changed"),
        "{report}"
    );
    assert_eq!(read(&dir, "devflow.toml"), PRE_RELEASE);
    assert_eq!(read(&dir, "devflow.local.toml"), edited);
}

#[test]
fn a_first_file_changed_after_it_was_written_is_reported_not_overwritten() {
    let dir = project(&[
        ("devflow.toml", PRE_RELEASE),
        ("devflow.local.toml", "[postgres]\nport = 5544\n"),
    ]);
    let migration = plan(dir.path()).unwrap();
    let project_path = dir.path().join("devflow.toml");
    let local_path = dir.path().join("devflow.local.toml");
    let calls = std::cell::Cell::new(0);
    let _guard = super::super::seams::install(super::super::seams::Hooks {
        before_write: Some(Box::new(move || {
            calls.set(calls.get() + 1);
            if calls.get() == 2 {
                // Both files change: the second write fails and the first
                // cannot be put back without losing the newer text.
                std::fs::write(&project_path, "# newer\n").unwrap();
                std::fs::write(&local_path, "# newer too\n").unwrap();
            }
        })),
        ..Default::default()
    });
    let report = format!("{:#}", apply(&migration).unwrap_err());
    assert!(
        report.contains("devflow.toml was already migrated"),
        "{report}"
    );
    assert_eq!(read(&dir, "devflow.toml"), "# newer\n");
}

// -- review fixes, round 2 ---------------------------------------------------

const COMMENT_SECRETS: [&str; 9] = [
    "hunter2-decor",
    "hunter2-above",
    "urlpass-comment",
    "hunter2-after",
    "hunter2-array",
    "sk-live-array",
    "tok-inline",
    "hunter2-quoted",
    "trailing space",
];

#[test]
fn comments_never_carry_a_credential_into_a_preview() {
    let url = "postgres://app:urlpass-comment@h/db"; // kingfisher:ignore
    let local = "\
# SESSION_KEY = \"hunter2-above\"
# cache: URL_HERE
[worktree]
copy = [
  \"a\", # SESSION_KEY = 'hunter2-array'
  # stripeKey: sk-live-array
  \"b\",
]
[env]
KEEP = \"${PGHOST}\" # DB_PASS = hunter2-after
URL = \"http://x/#frag\" # not a secret
OTHER = \"1\" # api_token=tok-inline
# \"DB_PASS\" = \"hunter2-quoted\"
# password = trailing space secret
# DB_PASS = \"hunter2-decor\"
[postgres]
port = 5544
"
    .replace("URL_HERE", url);
    let project_text = format!("[project]\nname = \"p\"\nid = \"{ID}\"\n");
    let dir = project(&[
        ("devflow.toml", &project_text),
        ("devflow.local.toml", &local),
    ]);
    let migration = plan(dir.path()).unwrap();
    let text = shown(&migration);
    for secret in COMMENT_SECRETS {
        assert!(!text.contains(secret), "{secret} leaked:\n{text}");
    }
    // `preview.after`, the context lines and Debug, one by one.
    let file = &migration.files[0];
    for secret in COMMENT_SECRETS {
        assert!(!file.preview.after.contains(secret), "{secret} in after");
        assert!(
            !file.preview.diff.iter().any(|l| l.text.contains(secret)),
            "{secret} in the diff"
        );
        assert!(!format!("{file:?}").contains(secret), "{secret} in Debug");
    }
    // Comments stay readable where they hold nothing secret, and a `#` in a
    // string is not a comment.
    assert!(file.preview.after.contains("# not a secret"));
    assert!(file.preview.after.contains("\"http://x/#frag\""));
    assert!(file.preview.after.contains("# SESSION_KEY = ***"));
    // What is applied is the real text.
    apply(&migration).unwrap();
    let written = read(&dir, "devflow.local.toml");
    for secret in COMMENT_SECRETS {
        assert!(written.contains(secret), "{secret} lost from the file");
    }
}

#[test]
fn comments_in_a_multi_line_inline_table_and_other_layouts_are_masked_too() {
    // TOML 1.1 allows comments inside an inline table; the lexer does not
    // depend on how the document parses.
    let text = "x = {\n  a = 1, # DB_PASS = hunter2-inline\n  # SESSION_KEY: s3cr3t-inline\n  b = \"# not a comment\",\n}\n";
    let masked = masked_text(text);
    assert!(
        !masked.contains("hunter2-inline") && !masked.contains("s3cr3t-inline"),
        "{masked}"
    );
    assert!(masked.contains("b = \"# not a comment\""), "{masked}");
    // Line endings and multi-line strings.
    let text = "a = \"\"\"\n# DB_PASS = in-string\n\"\"\" # DB_PASS = hunter2-crlf\r\nb = 1\r\n";
    let masked = mask_comments(text);
    assert!(!masked.contains("hunter2-crlf"), "{masked}");
    assert!(masked.contains("# DB_PASS = in-string"), "{masked}");
    assert!(masked.contains("\r\nb = 1\r\n"), "{masked:?}");
}

#[test]
fn malformed_toml_is_an_error_not_nothing_to_migrate() {
    let text =
        format!("[project]\nname = \"p\"\nid = \"{ID}\"\n\n[postgres]\npassword = = \"hunter2\"\n");
    let dir = project(&[("devflow.toml", &text)]);
    for error in [
        needed(dir.path()).unwrap_err(),
        plan(dir.path()).unwrap_err(),
    ] {
        let refused = problem(error);
        assert!(
            refused.message.contains("invalid TOML"),
            "{}",
            refused.message
        );
        assert!(
            refused.message.contains("devflow.toml"),
            "{}",
            refused.message
        );
        assert!(!refused.message.contains("hunter2"), "{}", refused.message);
        assert!(
            refused
                .remedies
                .iter()
                .any(|r| matches!(r, Remedy::OpenFile { .. })),
            "{refused:?}"
        );
    }
    // The same in the local file.
    let current = format!("[project]\nname = \"p\"\nid = \"{ID}\"\n");
    let dir = project(&[
        ("devflow.toml", &current),
        ("devflow.local.toml", "[postgres\n"),
    ]);
    let refused = problem(needed(dir.path()).unwrap_err());
    assert!(
        refused.message.contains("devflow.local.toml"),
        "{}",
        refused.message
    );
    assert_eq!(read(&dir, "devflow.toml"), current);
}

#[test]
fn a_zero_x_or_invalid_configuration_without_the_table_is_refused_not_current() {
    let legacy = format!("[project]\nname = \"p\"\nid = \"{ID}\"\n\n[agent]\nx = 1\n");
    let dir = project(&[("devflow.toml", &legacy)]);
    for error in [
        needed(dir.path()).unwrap_err(),
        plan(dir.path()).unwrap_err(),
    ] {
        assert!(
            crate::remedy::remedies_of(&error)
                .iter()
                .any(|r| matches!(r, Remedy::OpenGuide { .. })),
            "{error:#}"
        );
    }
    let invalid = format!("[project]\nname = \"p\"\nid = \"{ID}\"\n\n[services.db]\nengine = 3\n");
    let dir = project(&[("devflow.toml", &invalid)]);
    assert!(needed(dir.path()).is_err());
    assert!(plan(dir.path()).is_err());
    assert_eq!(read(&dir, "devflow.toml"), invalid);
}

#[test]
fn a_zero_x_yaml_only_checkout_gets_the_guide_not_set_up() {
    let dir = project(&[(".devflow.yml", "services: []\n")]);
    for error in [
        needed(dir.path()).unwrap_err(),
        plan(dir.path()).unwrap_err(),
    ] {
        assert!(
            error.downcast_ref::<NotInitialized>().is_none(),
            "{error:#}"
        );
        assert!(
            crate::remedy::remedies_of(&error)
                .iter()
                .any(|r| matches!(r, Remedy::OpenGuide { .. })),
            "{error:#}"
        );
    }
}

#[test]
fn quotes_in_a_comment_never_end_a_masked_value_early() {
    for comment in [
        "# SESSION_KEY = \"\"\"canary-triple\"\"\"",
        "# SESSION_KEY = \"canary-\\\"escaped\"",
        "# SESSION_KEY = 'canary' and \"more-canary\"",
    ] {
        let masked = masked_comment(comment);
        assert!(!masked.contains("canary"), "{comment} -> {masked}");
        assert_eq!(masked, "# SESSION_KEY = ***", "{comment}");
    }
    let text = "# SESSION_KEY = \"\"\"canary-triple\"\"\"\n[postgres]\nport = 1\n";
    assert!(!masked_text(text).contains("canary"));
}

#[test]
fn a_valid_current_configuration_needs_nothing() {
    let current = format!("[project]\nname = \"p\"\nid = \"{ID}\"\n");
    let dir = project(&[("devflow.toml", &current)]);
    assert!(!needed(dir.path()).unwrap());
    let with_table = project(&[("devflow.toml", PRE_RELEASE)]);
    assert!(needed(with_table.path()).unwrap());
}

#[test]
fn a_replacement_that_went_in_before_its_refusal_is_put_back_too() {
    // Without an atomic exchange the local file is swapped in and the edit
    // that landed in place is found afterwards: the refusal comes after the
    // file was already migrated.
    let local_text = "[postgres]\nport = 5544\n";
    let dir = project(&[
        ("devflow.toml", PRE_RELEASE),
        ("devflow.local.toml", local_text),
    ]);
    let migration = plan(dir.path()).unwrap();
    assert_eq!(migration.files.len(), 2);
    let local_path = dir.path().join("devflow.local.toml");
    let edited = "[postgres]\nport = 5544\n# edited in place\n";
    let calls = std::cell::Cell::new(0);
    let guard = super::super::seams::install(super::super::seams::Hooks {
        no_exchange: true,
        after_backup: Some(Box::new({
            let local_path = local_path.clone();
            move || {
                calls.set(calls.get() + 1);
                if calls.get() == 2 {
                    // In place, so the inode stays the one that was linked.
                    std::fs::write(&local_path, edited).unwrap();
                }
            }
        })),
        ..Default::default()
    });
    let error = apply(&migration).unwrap_err();
    let report = format!("{error:#}");
    drop(guard);
    assert!(error.downcast_ref::<ConfigChanged>().is_some(), "{report}");
    // Both files are as they were; nothing is half migrated.
    assert_eq!(read(&dir, "devflow.toml"), PRE_RELEASE);
    assert_eq!(read(&dir, "devflow.local.toml"), local_text);
    assert!(
        report.contains("were put back") && report.contains("nothing was changed"),
        "{report}"
    );
    // The edit that would have been lost is kept, and the report names it.
    let kept: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.to_string_lossy().contains(".devflow-conflict-"))
        .collect();
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(std::fs::read_to_string(&kept[0]).unwrap(), edited);
    assert!(report.contains(&kept[0].display().to_string()), "{report}");
    // And the pair can be migrated again.
    assert_eq!(plan(dir.path()).unwrap().files.len(), 2);
}

#[test]
fn a_replacement_that_cannot_be_put_back_is_reported_with_every_kept_file() {
    // The local file's replacement went in, then the project file goes
    // before it is restored: the project file stays migrated and is named,
    // the local file is put back, and the kept version is reported.
    let dir = project(&[
        ("devflow.toml", PRE_RELEASE),
        ("devflow.local.toml", "[postgres]\nport = 5544\n"),
    ]);
    let migration = plan(dir.path()).unwrap();
    let project_path = dir.path().join("devflow.toml");
    let local_path = dir.path().join("devflow.local.toml");
    let calls = std::cell::Cell::new(0);
    let guard = super::super::seams::install(super::super::seams::Hooks {
        no_exchange: true,
        after_backup: Some(Box::new(move || {
            calls.set(calls.get() + 1);
            match calls.get() {
                // The local file is edited in place: its write is refused
                // after the replacement went in.
                2 => std::fs::write(&local_path, "# edit\n").unwrap(),
                // Restoring the local file: the project file goes meanwhile.
                3 => std::fs::remove_file(&project_path).unwrap(),
                _ => {}
            }
        })),
        ..Default::default()
    });
    let error = apply(&migration).unwrap_err();
    let report = format!("{error:#}");
    drop(guard);
    assert!(
        report.contains("devflow.toml was already migrated")
            && report.contains("devflow.local.toml was put back"),
        "{report}"
    );
    assert!(report.contains(".devflow-conflict-"), "{report}");
    assert_eq!(
        read(&dir, "devflow.local.toml"),
        "[postgres]\nport = 5544\n"
    );
}
