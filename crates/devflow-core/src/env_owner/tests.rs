use super::*;
use std::collections::BTreeMap;

const OWN: &str = "postgres://own:own-password-8@localhost:5432/own"; // kingfisher:ignore
const OTHER: &str = "postgres://main:main-password-9@localhost:5432/main"; // kingfisher:ignore

/// A temporary directory whose path is canonical: a source is never read
/// through a symlink, and the system's temporary directory can sit behind one.
struct Tmp {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl Tmp {
    fn path(&self) -> &Path {
        &self.path
    }
}

fn tempdir() -> Tmp {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap();
    Tmp { _dir: dir, path }
}

fn config(text: &str) -> Config {
    Config::parse(text).unwrap_or_else(|error| panic!("{error:#}\n{text}"))
}

fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The environment of a workspace that owns `names`, against `inherited`.
fn resolved(
    vars: Vec<(String, String)>,
    names: &BTreeSet<String>,
    inherited: &Inherited,
) -> ChildEnv {
    ChildEnv::resolve(vars, names, inherited).unwrap()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|n| n.to_string()).collect()
}

/// What a process started with `env` gets when it inherits exactly `inherited`.
#[cfg(unix)]
fn seen(env: &ChildEnv, inherited: &[(&str, &str)]) -> BTreeMap<String, String> {
    let mut command = Command::new("/usr/bin/env");
    command
        .arg("-0")
        .env_clear()
        .envs(inherited.iter().copied());
    env.apply(&mut command);
    let output = command.output().unwrap();
    String::from_utf8(output.stdout)
        .unwrap()
        .split('\0')
        .filter_map(|entry| entry.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// A workspace root with `.devflow/env` recording `id` and `names`.
fn workspace(dir: &Path, id: &str, names: &[(&str, &str)]) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let mut all = vec![("DEVFLOW_WORKSPACE_ID", id)];
    all.extend_from_slice(names);
    env_file::write(dir, &vars(&all)).unwrap();
    dir.to_path_buf()
}

fn inherited_from(path: &Path, id: &str) -> Inherited {
    Inherited {
        workspace_path: Some(path.to_path_buf()),
        workspace_id: Some(id.into()),
        workspace_name: Some("feature-a".into()),
        keep: BTreeSet::new(),
        clear_stale: false,
    }
}

const FULL: &str = r#"
[project]
name = "shop"
id = "0b8fb0a2-4b3e-4a39-a5f5-0f2f3cf2a6a1"

[services.db]
engine = "postgres"
extra_databases = ["test", "shadow"]

[services.cache]
engine = "valkey"

[env]
presets = ["prisma"]
NODE_ENV = "development"
APP_DB = "${DATABASE_URL}"
"#;

#[test]
fn the_declared_names_cover_every_category_whether_or_not_they_are_bound() {
    let names = declared_names(&config(FULL));
    for identity in IDENTITY {
        assert!(names.contains(identity), "{identity}");
    }
    for name in [
        "DATABASE_URL",
        "PGHOST",
        "PGPASSWORD",
        "PGSERVICE",
        "PGSSLROOTCERT",
    ] {
        assert!(names.contains(name), "{name}");
    }
    for name in ["TEST_DATABASE_URL", "SHADOW_DATABASE_URL", "REDIS_URL"] {
        assert!(names.contains(name), "{name}");
    }
    for name in ["DIRECT_URL", "NODE_ENV", "APP_DB"] {
        assert!(names.contains(name), "{name}");
    }
    // Nothing that controls devflow itself, or that is not a name at all.
    assert!(names.iter().all(|n| !is_execution_control(n)));
    assert!(names.iter().all(|n| valid_env_name(n)));
}

#[test]
fn a_project_without_a_database_declares_neither_its_url_nor_the_libpq_family() {
    let names = declared_names(&config(
        "[project]\nname = \"p\"\nid = \"0b8fb0a2-4b3e-4a39-a5f5-0f2f3cf2a6a1\"\n",
    ));
    assert_eq!(names, IDENTITY.iter().map(|n| n.to_string()).collect());
}

#[test]
fn a_mysql_service_declares_its_url_and_typed_fields() {
    let names = declared_names(&config(
        "[project]\nname = \"p\"\nid = \"0b8fb0a2-4b3e-4a39-a5f5-0f2f3cf2a6a1\"\n\n\
         [services.sql]\nengine = \"mysql\"\n",
    ));
    for name in [
        "DATABASE_URL",
        "MYSQL_HOST",
        "MYSQL_PORT",
        "MYSQL_USER",
        "MYSQL_PASSWORD",
    ] {
        assert!(names.contains(name), "{name}");
    }
    assert!(!names.contains("PGHOST"));
}

#[test]
fn preset_keys_are_exactly_the_variables_the_presets_set() {
    let url = "postgres://user:secret-word@db.example:5433/app?sslmode=require"; // kingfisher:ignore
    for preset in crate::config::EnvPreset::ALL {
        let rendered = env_file::render(
            &crate::config::EnvSection {
                presets: vec![preset],
                vars: Default::default(),
            },
            &[],
            Some(url),
        );
        let set: BTreeSet<&str> = rendered.vars.iter().map(|(k, _)| k.as_str()).collect();
        let declared: BTreeSet<&str> = preset.keys().iter().copied().collect();
        assert_eq!(set, declared, "{preset}");
    }
}

#[cfg(unix)]
#[test]
fn a_no_db_workspace_takes_the_callers_database_out_of_the_environment() {
    // The reproduced leak: the workspace has no database, the caller's shell
    // carries another workspace's.
    let declared = declared_names(&config(FULL));
    let env = resolved(
        vars(&[("DEVFLOW_WORKSPACE", "feature-b")]),
        &declared,
        &Inherited::default(),
    );
    let inherited = [
        ("DATABASE_URL", OTHER),
        ("PGHOST", "elsewhere"),
        ("PGSERVICE", "main"),
        ("REDIS_URL", "redis://elsewhere"),
        ("NODE_ENV", "production"),
        ("DEVFLOW_WORKSPACE", "feature-a"),
        ("UNRELATED", "kept"),
    ];
    let got = seen(&env, &inherited);
    for name in [
        "DATABASE_URL",
        "PGHOST",
        "PGSERVICE",
        "REDIS_URL",
        "NODE_ENV",
    ] {
        assert!(!got.contains_key(name), "{name} leaked: {got:?}");
    }
    assert_eq!(got["DEVFLOW_WORKSPACE"], "feature-b");
    assert_eq!(got["UNRELATED"], "kept");
}

#[cfg(unix)]
#[test]
fn the_workspaces_own_values_win_over_the_inherited_ones() {
    let declared = declared_names(&config(FULL));
    let env = resolved(
        vars(&[("DATABASE_URL", OWN), ("PGSSLROOTCERT", "/ca.pem")]),
        &declared,
        &Inherited::default(),
    );
    let got = seen(
        &env,
        &[
            ("DATABASE_URL", OTHER),
            ("PGSSLROOTCERT", "/other.pem"),
            ("PGSERVICE", "main"),
        ],
    );
    assert_eq!(got["DATABASE_URL"], OWN);
    assert_eq!(got["PGSSLROOTCERT"], "/ca.pem");
    assert!(!got.contains_key("PGSERVICE"));
}

#[cfg(unix)]
#[test]
fn a_removed_recipe_and_another_projects_names_are_cleared_through_the_source_set() {
    let dir = tempdir();
    // Workspace A's file: a recipe the target no longer declares, and names of
    // another project altogether.
    let source = workspace(
        &dir.path().join("a"),
        "a-id",
        &[
            ("OLD_RECIPE", "gone"),
            ("MONGO_URL", "mongodb://elsewhere"),
            ("DATABASE_URL", OTHER),
        ],
    );
    let declared = set(&["DEVFLOW_WORKSPACE", "DEVFLOW_WORKSPACE_ID"]);
    let env = resolved(
        vars(&[("DEVFLOW_WORKSPACE", "b")]),
        &declared,
        &inherited_from(&source, "a-id"),
    );
    assert_eq!(env.source().unwrap().path, source);
    let got = seen(
        &env,
        &[
            ("OLD_RECIPE", "gone"),
            ("MONGO_URL", "mongodb://elsewhere"),
            ("DATABASE_URL", OTHER),
            ("DEVFLOW_WORKSPACE_ID", "a-id"),
            ("UNRELATED", "kept"),
        ],
    );
    for name in [
        "OLD_RECIPE",
        "MONGO_URL",
        "DATABASE_URL",
        "DEVFLOW_WORKSPACE_ID",
    ] {
        assert!(!got.contains_key(name), "{name}: {got:?}");
    }
    assert_eq!(got["UNRELATED"], "kept");
    // Without the source, the caller's values of names this workspace never
    // declared stay: it cannot know they are not the user's own.
    let alone = resolved(
        vars(&[("DEVFLOW_WORKSPACE", "b")]),
        &declared,
        &Inherited::default(),
    );
    assert_eq!(
        seen(&alone, &[("OLD_RECIPE", "gone")])["OLD_RECIPE"],
        "gone"
    );
}

#[cfg(unix)]
#[test]
fn execution_controls_and_process_names_are_never_cleared() {
    let dir = tempdir();
    // A source file (even a hand-written one) that lists them.
    let source = workspace(
        &dir.path().join("a"),
        "a-id",
        &[
            ("DEVFLOW_SETUP_ACTIVE", "1"),
            ("DEVFLOW_PHASE", "setup"),
            ("DEVFLOW_TRUST_HOOKS", "1"),
            ("DEVFLOW_NON_INTERACTIVE", "1"),
            ("DEVFLOW_CD_FILE", "/tmp/cd"),
            ("DEVFLOW_BIN", "/bin/devflow"),
            ("DEVFLOW_CONFIG_DIR", "/c"),
            ("DEVFLOW_DATA_DIR", "/d"),
            ("DEVFLOW_INSTANCE_ID", "x"),
            ("DEVFLOW_INHERIT", "A"),
            ("DEVFLOW_ANYTHING", "else"),
            ("PATH", "/usr/bin"),
            ("HOME", "/home/x"),
            ("NOT A NAME", "x"),
        ],
    );
    let env = resolved(
        vars(&[("DEVFLOW_WORKSPACE", "b")]),
        &declared_names(&config(FULL)),
        &inherited_from(&source, "a-id"),
    );
    for name in [
        "DEVFLOW_SETUP_ACTIVE",
        "DEVFLOW_PHASE",
        "DEVFLOW_TRUST_HOOKS",
        "DEVFLOW_NON_INTERACTIVE",
        "DEVFLOW_CD_FILE",
        "DEVFLOW_BIN",
        "DEVFLOW_CONFIG_DIR",
        "DEVFLOW_DATA_DIR",
        "DEVFLOW_INSTANCE_ID",
        "DEVFLOW_INHERIT",
        "DEVFLOW_ANYTHING",
        "PATH",
        "HOME",
    ] {
        assert!(!env.owned().contains(name), "{name} must not be owned");
        assert!(
            is_execution_control(name) || PROCESS.contains(&name),
            "{name}"
        );
    }
    let got = seen(
        &env,
        &[
            ("DEVFLOW_SETUP_ACTIVE", "1"),
            ("DEVFLOW_PHASE", "setup"),
            ("DEVFLOW_INHERIT", "A"),
            ("PATH", "/usr/bin"),
        ],
    );
    assert_eq!(got["DEVFLOW_SETUP_ACTIVE"], "1");
    assert_eq!(got["DEVFLOW_PHASE"], "setup");
    assert_eq!(got["DEVFLOW_INHERIT"], "A");
    assert_eq!(got["PATH"], "/usr/bin");
}

#[cfg(unix)]
#[test]
fn inherit_keeps_the_callers_value_unless_the_workspace_sets_one() {
    let declared = declared_names(&config(FULL));
    let inherited = [
        ("DATABASE_URL", "postgres://ci/db"),
        ("REDIS_URL", "redis://ci"),
    ];
    // Named explicitly (`--inherit`).
    let env = resolved(
        vars(&[("DEVFLOW_WORKSPACE", "b")]),
        &declared,
        &Inherited::default(),
    )
    .keeping(["DATABASE_URL".to_string()]);
    let got = seen(&env, &inherited);
    assert_eq!(got["DATABASE_URL"], "postgres://ci/db");
    assert!(!got.contains_key("REDIS_URL"));
    // Named in `DEVFLOW_INHERIT`.
    let env = resolved(
        vars(&[("DEVFLOW_WORKSPACE", "b")]),
        &declared,
        &Inherited {
            keep: inherit_list("REDIS_URL, DATABASE_URL,not a name,,"),
            ..Inherited::default()
        },
    );
    let got = seen(&env, &inherited);
    assert_eq!(got["DATABASE_URL"], "postgres://ci/db");
    assert_eq!(got["REDIS_URL"], "redis://ci");
    // A value the workspace sets wins over the caller's, named or not
    // (`setup --database-url` is such a value).
    let env = resolved(
        vars(&[("DATABASE_URL", OWN)]),
        &declared,
        &Inherited::default(),
    )
    .keeping(["DATABASE_URL".to_string()]);
    assert_eq!(seen(&env, &inherited)["DATABASE_URL"], OWN);
}

#[test]
fn the_inherit_list_takes_plain_names_only() {
    assert_eq!(inherit_list(""), set(&[]));
    assert_eq!(inherit_list("A,B_2 , _c"), set(&["A", "B_2", "_c"]));
    assert_eq!(inherit_list("A=1,$(x),a-b,,B"), set(&["B"]));
}

#[cfg(unix)]
#[test]
fn a_source_that_cannot_be_resolved_refuses_and_names_the_stale_workspace() {
    use std::os::unix::fs::symlink;
    let dir = tempdir();
    let declared = set(&["DEVFLOW_WORKSPACE"]);
    let attempt = |inherited: &Inherited| {
        ChildEnv::resolve(vars(&[("DEVFLOW_WORKSPACE", "b")]), &declared, inherited)
    };
    let real = workspace(&dir.path().join("real"), "a-id", &[("LEAKED", "x")]);
    // The genuine one resolves, and only decides what to clear.
    let env = attempt(&inherited_from(&real, "a-id")).unwrap();
    assert!(env.owned().contains("LEAKED"));
    assert_eq!(env.source().unwrap().path, real);
    // No workspace named at all is not a stale one.
    assert!(attempt(&Inherited::default()).unwrap().source().is_none());

    let stale = |inherited: &Inherited| attempt(inherited).unwrap_err();
    // Another instance (the id differs), or none.
    assert_eq!(
        stale(&inherited_from(&real, "forged")).reason,
        StaleReason::OtherInstance
    );
    assert_eq!(stale(&inherited_from(&real, "")).reason, StaleReason::NoId);
    // A path that is not absolute, an id with no path, a directory that is
    // gone, a directory with no file.
    assert_eq!(
        stale(&inherited_from(Path::new("real"), "a-id")).reason,
        StaleReason::NoPath
    );
    assert_eq!(
        stale(&Inherited {
            workspace_id: Some("a-id".into()),
            ..Inherited::default()
        })
        .reason,
        StaleReason::NoPath
    );
    assert_eq!(
        stale(&inherited_from(&dir.path().join("none"), "a-id")).reason,
        StaleReason::Gone
    );
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    assert_eq!(
        stale(&inherited_from(&empty, "a-id")).reason,
        StaleReason::NoFile
    );

    // `.devflow/env` is a link to a file that does record the id.
    let linked = dir.path().join("linked");
    std::fs::create_dir_all(linked.join(".devflow")).unwrap();
    symlink(env_file::path(&real), env_file::path(&linked)).unwrap();
    assert_eq!(
        stale(&inherited_from(&linked, "a-id")).reason,
        StaleReason::NotPlain
    );

    // `.devflow` is a link to a directory that holds one.
    let parent = dir.path().join("parent");
    std::fs::create_dir_all(&parent).unwrap();
    symlink(real.join(".devflow"), parent.join(".devflow")).unwrap();
    assert_eq!(
        stale(&inherited_from(&parent, "a-id")).reason,
        StaleReason::NotPlain
    );

    // The refusal names the workspace and where it was, never a value, and
    // carries its remedy.
    let refusal = stale(&inherited_from(&linked, "a-id"));
    let text = refusal.to_string();
    assert!(
        text.contains("feature-a") && text.contains(&linked.display().to_string()),
        "{text}"
    );
    assert!(!text.contains("a-id"), "{text}");
    let error = refusal.into_error();
    assert_eq!(
        crate::remedy::remedies_of(&error),
        [crate::remedy::Remedy::ClearInherited]
    );
    assert!(!error.to_string().contains("`devflow"), "{error}");
}

#[cfg(unix)]
#[test]
fn the_override_goes_on_and_clears_the_target_set_and_every_name_devflow_can_generate() {
    let dir = tempdir();
    let declared = set(&["DEVFLOW_WORKSPACE", "ONLY_HERE"]);
    let gone = inherited_from(&dir.path().join("gone"), "a-id");
    assert!(ChildEnv::resolve(vars(&[]), &declared, &gone).is_err());
    let env = resolved(
        vars(&[("DEVFLOW_WORKSPACE", "b")]),
        &declared,
        &gone.clone().clearing_stale(),
    );
    assert!(env.source().is_none());
    // The target's own names, and what devflow could have generated for any
    // workspace: a database of another project's among them.
    for name in [
        "ONLY_HERE",
        "DATABASE_URL",
        "PGSERVICE",
        "MYSQL_HOST",
        "REDIS_URL",
        "SHADOW_DATABASE_URL",
        "DIRECT_URL",
        "DEVFLOW_WORKSPACE_ID",
        "COMPOSE_PROJECT_NAME",
    ] {
        assert!(env.owned().contains(name), "{name}");
    }
    assert!(!env.owned().contains("UNRELATED"));
    assert!(env.owned().iter().all(|n| !is_execution_control(n)));
    let got = seen(
        &env,
        &[
            ("DATABASE_URL", OTHER),
            ("DEVFLOW_SETUP_ACTIVE", "1"),
            ("UNRELATED", "kept"),
            ("PATH", "/usr/bin"),
        ],
    );
    assert!(!got.contains_key("DATABASE_URL"));
    assert_eq!(got["DEVFLOW_SETUP_ACTIVE"], "1");
    assert_eq!(got["UNRELATED"], "kept");
    // Naming a variable to keep still keeps it.
    let kept = resolved(
        vars(&[("DEVFLOW_WORKSPACE", "b")]),
        &declared,
        &gone.clearing_stale().keeping(["DATABASE_URL".to_string()]),
    );
    assert_eq!(seen(&kept, &[("DATABASE_URL", "ci")])["DATABASE_URL"], "ci");
}

#[test]
fn a_file_without_the_versioned_manifest_is_refused_not_guessed() {
    let dir = tempdir();
    // Written by an earlier devflow: no list at all.
    env_file::write(dir.path(), &vars(&[("DATABASE_URL", OWN), ("EXTRA", "1")])).unwrap();
    let contents = env_file::read_contents(dir.path()).unwrap();
    assert_eq!(contents.declared, None);
    assert!(env_file::lacks_manifest(dir.path()));
    assert!(matches!(
        ChildEnv::from_contents_in(contents, &Inherited::default()),
        Err(NotReady::NoManifest)
    ));
    let error = ChildEnv::from_file(dir.path()).unwrap_err();
    assert_eq!(
        crate::remedy::remedies_of(&error),
        [crate::remedy::Remedy::Setup {
            workspace: dir.path().to_path_buf()
        }]
    );
    assert!(!error.to_string().contains("`devflow"), "{error}");
    assert!(!error.to_string().contains("main-password"), "{error}");
    // The first form of the marker, and a version this devflow does not know,
    // are not manifests either.
    for marker in [
        "# devflow-owns: DATABASE_URL",
        "# devflow-owns v2: DATABASE_URL",
    ] {
        std::fs::write(
            env_file::path(dir.path()),
            format!("# Generated\n{marker}\nDATABASE_URL='x'\n"),
        )
        .unwrap();
        assert!(env_file::lacks_manifest(dir.path()), "{marker}");
        assert!(ChildEnv::from_file(dir.path()).is_err(), "{marker}");
    }
    // A missing file has nothing to write again.
    std::fs::remove_file(env_file::path(dir.path())).unwrap();
    assert!(!env_file::lacks_manifest(dir.path()));
}

#[test]
fn the_manifest_round_trips_through_the_file_with_the_former_names() {
    let dir = tempdir();
    let declared = declared_names(&config(FULL));
    let former = set(&["OLD_RECIPE", "OLD_SERVICE_URL"]);
    env_file::write_owned(
        dir.path(),
        &vars(&[("DEVFLOW_WORKSPACE", "w")]),
        &declared,
        &former,
    )
    .unwrap();
    let text = std::fs::read_to_string(env_file::path(dir.path())).unwrap();
    assert!(text.contains("# devflow-owns v1: "), "{text}");
    assert!(
        text.contains("# devflow-formerly v1: OLD_RECIPE OLD_SERVICE_URL"),
        "{text}"
    );
    assert!(!text.contains("secret"), "names only: {text}");
    let contents = env_file::read_contents(dir.path()).unwrap();
    assert_eq!(contents.declared.as_ref(), Some(&declared));
    assert_eq!(contents.former, former);
    assert_eq!(contents.vars, vars(&[("DEVFLOW_WORKSPACE", "w")]));
    // `read` still shows the variables alone, and the owned set follows the
    // file: the former names are owned too.
    assert_eq!(env_file::read(dir.path()).unwrap(), contents.vars);
    assert!(!env_file::lacks_manifest(dir.path()));
    let env = ChildEnv::from_file(dir.path()).unwrap();
    assert!(env.owned().contains("DATABASE_URL"));
    assert!(env.owned().contains("OLD_RECIPE"));
    assert!(env.owned().contains("OLD_SERVICE_URL"));
}

#[cfg(unix)]
#[test]
fn a_former_name_is_cleared_after_its_source_file_was_written_again() {
    // Workspace A once exported OLD_RECIPE; the recipe was removed and the
    // file written again, so the file no longer names it as a variable. The
    // shell still holds it.
    let dir = tempdir();
    let a = dir.path().join("a");
    std::fs::create_dir_all(&a).unwrap();
    env_file::write_owned(
        &a,
        &vars(&[("DEVFLOW_WORKSPACE_ID", "a-id"), ("DEVFLOW_WORKSPACE", "a")]),
        &set(&["DEVFLOW_WORKSPACE", "DEVFLOW_WORKSPACE_ID"]),
        &set(&["OLD_RECIPE"]),
    )
    .unwrap();
    // B never exported it: A's file, which the environment names, says it
    // once did.
    let b = dir.path().join("b");
    std::fs::create_dir_all(&b).unwrap();
    env_file::write_owned(
        &b,
        &vars(&[("DEVFLOW_WORKSPACE", "b")]),
        &set(&["DEVFLOW_WORKSPACE"]),
        &set(&[]),
    )
    .unwrap();
    let contents = env_file::read_contents(&b).unwrap();
    let env = ChildEnv::from_contents_in(contents, &inherited_from(&a, "a-id")).unwrap();
    assert!(env.source().unwrap().names.contains("OLD_RECIPE"));
    let inherited = [
        ("OLD_RECIPE", "from a"),
        ("DEVFLOW_WORKSPACE_ID", "a-id"),
        ("UNRELATED", "kept"),
    ];
    let got = seen(&env, &inherited);
    assert!(!got.contains_key("OLD_RECIPE"), "{got:?}");
    assert_eq!(got["UNRELATED"], "kept");
    // Without that source, B's own list of former names does the same.
    env_file::write_owned(
        &b,
        &vars(&[("DEVFLOW_WORKSPACE", "b")]),
        &set(&["DEVFLOW_WORKSPACE"]),
        &set(&["OLD_RECIPE"]),
    )
    .unwrap();
    let env = ChildEnv::from_file(&b).unwrap();
    assert!(!seen(&env, &inherited).contains_key("OLD_RECIPE"));
    // A workspace that never exported it leaves the name alone.
    let other = ChildEnv::standalone(
        vars(&[("DEVFLOW_WORKSPACE", "c")]),
        &set(&["DEVFLOW_WORKSPACE"]),
    );
    assert_eq!(
        seen(&other, &[("OLD_RECIPE", "mine")])["OLD_RECIPE"],
        "mine"
    );
}

#[test]
fn absent_names_are_the_owned_ones_without_a_value_and_not_kept() {
    let env = ChildEnv::standalone(
        vars(&[("DATABASE_URL", OWN)]),
        &set(&["DATABASE_URL", "PGHOST", "REDIS_URL", "OLD_RECIPE"]),
    )
    .keeping(["REDIS_URL".to_string()]);
    let mut absent: Vec<&str> = env.absent().collect();
    absent.sort();
    assert_eq!(absent, ["OLD_RECIPE", "PGHOST"]);
}

#[test]
fn leaks_name_the_owned_variables_that_differ_and_where_they_came_from() {
    let dir = tempdir();
    let root = dir.path().join("b");
    let other = dir.path().join("a");
    let env = resolved(
        vars(&[("DATABASE_URL", OWN), ("DEVFLOW_WORKSPACE", "b")]),
        &set(&["DATABASE_URL", "REDIS_URL", "DEVFLOW_WORKSPACE"]),
        &Inherited::default(),
    );
    let process: BTreeMap<&str, &str> = [
        ("DATABASE_URL", OTHER),
        ("REDIS_URL", "redis://elsewhere"),
        ("DEVFLOW_WORKSPACE", "b"),
        ("UNRELATED", "x"),
    ]
    .into_iter()
    .collect();
    let lookup = |name: &str| process.get(name).map(|v| v.to_string());
    let inherited = Inherited {
        workspace_path: Some(other.clone()),
        workspace_name: Some("a".into()),
        ..Inherited::default()
    };
    let leak = leaks(&env, &root, lookup, &inherited);
    assert_eq!(leak.names, ["DATABASE_URL", "REDIS_URL"]);
    assert_eq!(leak.from, Some((other, Some("a".to_string()))));
    // The workspace's own values, or nothing, are no leak.
    let own: BTreeMap<&str, &str> = [("DATABASE_URL", OWN)].into_iter().collect();
    let leak = leaks(
        &env,
        &root,
        |name| own.get(name).map(|v| v.to_string()),
        &Inherited {
            workspace_path: Some(root.clone()),
            ..Inherited::default()
        },
    );
    assert_eq!(leak, Leak::default());
    // A name the caller keeps on purpose is no leak either.
    let keeping = env.clone().keeping(["REDIS_URL".to_string()]);
    let leak = leaks(&keeping, &root, lookup, &Inherited::default());
    assert_eq!(leak.names, ["DATABASE_URL"]);
}

fn dotenv_config() -> Config {
    let mut config = config(FULL);
    config.worktree.copy = vec![
        ".env".into(),
        "apps/web/.env.local".into(),
        "notes.env".into(),
        "data".into(),
        ".env.link".into(),
    ];
    config
}

#[test]
fn a_copied_dotenv_that_sets_an_owned_name_to_another_value_is_reported_by_name() {
    let dir = tempdir();
    let root = dir.path();
    std::fs::create_dir_all(root.join("apps/web")).unwrap();
    std::fs::write(
        root.join(".env"),
        format!("# comment\nDATABASE_URL={OTHER}\nexport REDIS_URL=\"redis://elsewhere\"\nPORT=3000\nNODE_ENV='development' # same\nbad name=1\n"),
    )
    .unwrap();
    std::fs::write(
        root.join("apps/web/.env.local"),
        format!("DATABASE_URL={OWN}\nAPP_DB=anything\n"),
    )
    .unwrap();
    // Not a dotenv file by name: never read.
    std::fs::write(root.join("notes.env"), "DATABASE_URL=x\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.join(".env"), root.join(".env.link")).unwrap();
    let env = resolved(
        vars(&[("DATABASE_URL", OWN), ("NODE_ENV", "development")]),
        &declared_names(&config(FULL)),
        &Inherited::default(),
    );
    let found = dotenv_conflicts(&dotenv_config(), root, &env);
    assert_eq!(
        found,
        [
            DotenvConflict {
                file: ".env".into(),
                names: vec!["DATABASE_URL".into(), "REDIS_URL".into()],
            },
            // `DATABASE_URL` agrees there; `APP_DB` is declared and unset here.
            DotenvConflict {
                file: "apps/web/.env.local".into(),
                names: vec!["APP_DB".into()],
            },
        ]
    );
    let text: String = found.iter().map(describe_conflict).collect();
    assert!(
        text.contains(".env") && text.contains("DATABASE_URL"),
        "{text}"
    );
    assert!(
        !text.contains("main-password") && !text.contains("elsewhere"),
        "{text}"
    );
}

#[test]
fn only_dotenv_names_count() {
    for yes in [".env", ".env.local", "apps/web/.env", "a/.env.production"] {
        assert!(is_dotenv(yes), "{yes}");
    }
    for no in [
        "env",
        "notes.env",
        ".envrc",
        "data",
        ".environment",
        "a/.env/x",
    ] {
        assert!(!is_dotenv(no), "{no}");
    }
}

#[cfg(unix)]
#[test]
fn a_symlink_anywhere_on_the_way_to_the_source_refuses_it() {
    use std::os::unix::fs::symlink;
    let dir = tempdir();
    let real = dir.path().join("real");
    let ws = workspace(&real.join("ws"), "a-id", &[("LEAKED", "x")]);
    let attempt = |path: &Path| {
        ChildEnv::resolve(
            vars(&[("DEVFLOW_WORKSPACE", "b")]),
            &set(&["DEVFLOW_WORKSPACE"]),
            &inherited_from(path, "a-id"),
        )
    };
    assert!(attempt(&ws).is_ok());

    // An ancestor of the worktree is a link: `alias/ws` reaches the same file.
    symlink(&real, dir.path().join("alias")).unwrap();
    let through_alias = dir.path().join("alias/ws");
    assert!(through_alias.join(".devflow/env").is_file());
    assert_eq!(
        attempt(&through_alias).unwrap_err().reason,
        StaleReason::NotPlain
    );
    // So is the worktree itself, and a `..` on the way.
    symlink(&ws, dir.path().join("ws-link")).unwrap();
    assert_eq!(
        attempt(&dir.path().join("ws-link")).unwrap_err().reason,
        StaleReason::NotPlain
    );
    assert_eq!(
        attempt(&real.join("../real/ws")).unwrap_err().reason,
        StaleReason::NotPlain
    );
    // A file where a directory should be is not a worktree.
    assert_eq!(
        attempt(&ws.join(".devflow/env/x")).unwrap_err().reason,
        StaleReason::Gone
    );
    // The names are not taken from it either.
    assert!(previous_names(&through_alias).is_empty());
    assert!(!previous_names(&ws).is_empty());
}

#[test]
fn a_kept_name_is_compared_once_the_workspace_sets_a_value_for_it() {
    let root = Path::new("/w/b");
    let process: BTreeMap<&str, &str> = [("DATABASE_URL", OTHER), ("REDIS_URL", "redis://ci")]
        .into_iter()
        .collect();
    let lookup = |name: &str| process.get(name).map(|v| v.to_string());
    // `DATABASE_URL` is kept and the workspace sets its own: the inherited
    // value differs from what a command gets. `REDIS_URL` is kept and the
    // workspace sets none: the caller's value is what it asked for.
    let env = ChildEnv::standalone(
        vars(&[("DATABASE_URL", OWN)]),
        &set(&["DATABASE_URL", "REDIS_URL"]),
    )
    .keeping(["DATABASE_URL".to_string(), "REDIS_URL".to_string()]);
    let leak = leaks(&env, root, lookup, &Inherited::default());
    assert_eq!(leak.names, ["DATABASE_URL"]);
}

#[test]
fn previous_names_are_the_variables_and_the_former_names_of_a_plain_file() {
    let dir = tempdir();
    assert!(previous_names(&dir.path().join("none")).is_empty());
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    env_file::write_owned(
        &ws,
        &vars(&[
            ("APP_DB", "x"),
            ("DEVFLOW_SETUP_ACTIVE", "1"),
            ("PATH", "/bin"),
        ]),
        &set(&["APP_DB"]),
        &set(&["OLD_RECIPE", "bad-name"]),
    )
    .unwrap();
    // Execution controls and process names are never anyone's to clear.
    assert_eq!(previous_names(&ws), set(&["APP_DB", "OLD_RECIPE"]));
}
