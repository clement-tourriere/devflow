use super::*;
use crate::config::LOCAL_CONFIG_FILE;
use crate::error::ConfigChanged;
use std::path::PathBuf;

const HEAD: &str = "[project]\nname = \"shop\"\nid = \"0b3c9d2e-1111-4222-8333-444455556666\"\n";

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    /// A Git checkout whose `devflow.toml` is `HEAD` followed by `rest`.
    fn new(rest: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        git2::Repository::init(&root).unwrap();
        std::fs::write(root.join("devflow.toml"), format!("{HEAD}{rest}")).unwrap();
        Self { _dir: dir, root }
    }

    fn path(&self, target: Target) -> PathBuf {
        match target {
            Target::Project => self.root.join("devflow.toml"),
            Target::Local => self.root.join(LOCAL_CONFIG_FILE),
        }
    }

    fn text(&self, target: Target) -> String {
        std::fs::read_to_string(self.path(target)).unwrap_or_default()
    }

    fn write(&self, target: Target, text: &str) {
        std::fs::write(self.path(target), text).unwrap();
    }

    fn files(&self) -> ConfigFiles {
        ConfigFiles::read(&self.root).unwrap()
    }

    /// Apply `edit` the way a frontend does: look at the file, then edit it.
    fn edit(&self, target: Target, edit: Edit) -> Result<Applied> {
        let expected = self.files().file(target).hash.clone();
        apply(&self.root, target, expected.as_deref(), &edit)
    }

    fn ok(&self, target: Target, edit: Edit) -> Applied {
        self.edit(target, edit)
            .unwrap_or_else(|e| panic!("edit failed: {e:#}"))
    }
}

fn add(phase: HookPhase, entry: impl Into<HookEntry>) -> Edit {
    Edit::AddHook {
        phase,
        entry: entry.into(),
        at: Placement::End,
    }
}

fn named(name: &str, run: &str) -> HookEntry {
    HookEntry {
        name: Some(name.into()),
        run: run.into(),
        timeout: None,
    }
}

fn by_name(name: &str) -> HookRef {
    HookRef::Name(name.into())
}

fn runs(fixture: &Fixture, phase: HookPhase) -> Vec<String> {
    fixture
        .files()
        .config()
        .unwrap()
        .hooks
        .entries(phase)
        .iter()
        .map(|e| e.run.clone())
        .collect()
}

#[test]
fn a_plain_command_is_added_as_a_string_and_the_rest_of_the_file_is_untouched() {
    let original =
        "# how a task starts\n[hooks]\nsetup = [\"bun install\"] # deps\nteardown = []\n";
    let fixture = Fixture::new(&format!("\n{original}"));
    let applied = fixture.ok(Target::Project, add(HookPhase::Setup, "bun run codegen"));
    assert!(applied.changed);
    let text = fixture.text(Target::Project);
    assert!(text.contains("# how a task starts\n[hooks]\n"), "{text}");
    assert!(text.contains("\"bun install\""));
    assert!(text.contains("\"bun run codegen\""));
    assert!(text.contains("# deps"), "{text}");
    assert!(text.contains("teardown = []"));
    assert_eq!(
        runs(&fixture, HookPhase::Setup),
        ["bun install", "bun run codegen"]
    );
    // Written as a plain string, on one line, as the file had it.
    assert!(
        text.contains("setup = [\"bun install\", \"bun run codegen\"] # deps"),
        "{text}"
    );
    assert_eq!(applied.after, crate::propose::sha256_hex(&text));
}

#[test]
fn a_named_or_timed_command_is_a_table_and_a_multiline_list_stays_multiline() {
    let fixture = Fixture::new(
        "\n[hooks]\nsetup = [\n  # first, the dependencies\n  \"bun install\",\n  { name = \"gen\", run = \"bun run gen\" }, # codegen\n]\n",
    );
    fixture.ok(
        Target::Project,
        Edit::AddHook {
            phase: HookPhase::Setup,
            entry: HookEntry {
                name: Some("migrate".into()),
                run: "bunx prisma migrate deploy".into(),
                timeout: Some("5m".into()),
            },
            at: Placement::End,
        },
    );
    let text = fixture.text(Target::Project);
    assert!(text.contains("  # first, the dependencies\n  \"bun install\","));
    assert!(text.contains("{ name = \"gen\", run = \"bun run gen\" }, # codegen"));
    assert!(
        text.contains(
            "  { name = \"migrate\", run = \"bunx prisma migrate deploy\", timeout = \"5m\" },\n]"
        ),
        "{text}"
    );
    let hooks = fixture.files().hooks().unwrap();
    let rows = &hooks.setup.hooks;
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[2].name.as_deref(), Some("migrate"));
    assert_eq!(rows[2].timeout.as_deref(), Some("5m"));
    assert_eq!(rows[2].effective_timeout, "5m");
    assert_eq!(rows[0].effective_timeout, "15m");
}

#[test]
fn adding_a_command_creates_the_hooks_table_when_there_is_none() {
    let fixture = Fixture::new("");
    fixture.ok(
        Target::Project,
        add(HookPhase::Teardown, "docker compose down"),
    );
    let text = fixture.text(Target::Project);
    assert!(text.starts_with(HEAD), "{text}");
    assert!(
        text.contains("[hooks]\nteardown = [\"docker compose down\"]\n"),
        "{text}"
    );
}

#[test]
fn placements_put_the_command_where_asked() {
    let fixture = Fixture::new(
        "\n[hooks]\nsetup = [\n  { name = \"a\", run = \"one\" },\n  { name = \"b\", run = \"two\" },\n  { name = \"c\", run = \"three\" },\n]\n",
    );
    let put = |name: &str, at: Placement| {
        fixture.ok(
            Target::Project,
            Edit::AddHook {
                phase: HookPhase::Setup,
                entry: named(name, name),
                at,
            },
        );
        fixture
            .files()
            .hooks()
            .unwrap()
            .setup
            .hooks
            .iter()
            .map(|h| h.name.clone().unwrap())
            .collect::<Vec<_>>()
            .join("")
    };
    assert_eq!(put("s", Placement::Start), "sabc");
    assert_eq!(put("m", Placement::After(by_name("a"))), "sambc");
    assert_eq!(put("n", Placement::Before(by_name("c"))), "sambnc");
    assert_eq!(put("i", Placement::Index(1)), "siambnc");
    assert_eq!(put("e", Placement::After(HookRef::Index(0))), "seiambnc");
    assert_eq!(put("z", Placement::Index(99)), "seiambncz");
}

#[test]
fn an_entry_is_updated_in_place_and_goes_back_to_a_string_when_only_run_is_left() {
    let fixture = Fixture::new(
        "\n[hooks]\nsetup = [\n  \"bun install\", # deps\n  { name = \"gen\", run = \"bun run gen\", timeout = \"2m\" },\n]\n",
    );
    fixture.ok(
        Target::Project,
        Edit::UpdateHook {
            phase: HookPhase::Setup,
            hook: HookRef::Index(0),
            entry: named("install", "bun install --frozen-lockfile"),
        },
    );
    let text = fixture.text(Target::Project);
    assert!(
        text.contains(
            "  { name = \"install\", run = \"bun install --frozen-lockfile\" }, # deps\n"
        ),
        "{text}"
    );
    // Adding a timeout to a table that has none keeps the braces tidy.
    fixture.ok(
        Target::Project,
        Edit::UpdateHook {
            phase: HookPhase::Setup,
            hook: by_name("install"),
            entry: HookEntry {
                name: Some("install".into()),
                run: "bun install --frozen-lockfile".into(),
                timeout: Some("4m".into()),
            },
        },
    );
    let text = fixture.text(Target::Project);
    assert!(
        text.contains("{ name = \"install\", run = \"bun install --frozen-lockfile\", timeout = \"4m\" }, # deps"),
        "{text}"
    );
    fixture.ok(
        Target::Project,
        Edit::UpdateHook {
            phase: HookPhase::Setup,
            hook: by_name("gen"),
            entry: HookEntry::new("bun run generate"),
        },
    );
    let text = fixture.text(Target::Project);
    assert!(text.contains("  \"bun run generate\",\n]"), "{text}");
    assert_eq!(
        runs(&fixture, HookPhase::Setup),
        ["bun install --frozen-lockfile", "bun run generate"]
    );
}

#[test]
fn removing_an_entry_takes_its_own_comments_and_leaves_its_neighbours() {
    let fixture = Fixture::new(
        "\n[hooks]\nsetup = [\n  # install\n  \"bun install\", # fast\n  # generate\n  \"bun run gen\", # slow\n  \"bun run seed\",\n]\n",
    );
    fixture.ok(
        Target::Project,
        Edit::RemoveHook {
            phase: HookPhase::Setup,
            hook: HookRef::Index(1),
        },
    );
    let text = fixture.text(Target::Project);
    assert_eq!(
        text,
        format!(
            "{HEAD}\n[hooks]\nsetup = [\n  # install\n  \"bun install\", # fast\n  \"bun run seed\",\n]\n"
        )
    );
    // The first and the last go too.
    fixture.ok(
        Target::Project,
        Edit::RemoveHook {
            phase: HookPhase::Setup,
            hook: HookRef::Index(1),
        },
    );
    fixture.ok(
        Target::Project,
        Edit::RemoveHook {
            phase: HookPhase::Setup,
            hook: HookRef::Index(0),
        },
    );
    assert!(runs(&fixture, HookPhase::Setup).is_empty());
    assert!(fixture.text(Target::Project).contains("setup = []"));
}

#[test]
fn moving_an_entry_keeps_each_comment_with_its_entry() {
    let fixture = Fixture::new(
        "\n[hooks]\nsetup = [\n  # one\n  \"a\", # first\n  # two\n  \"b\", # second\n  # three\n  \"c\", # third\n]\n",
    );
    fixture.ok(
        Target::Project,
        Edit::MoveHook {
            phase: HookPhase::Setup,
            hook: HookRef::Index(2),
            to: Placement::Start,
        },
    );
    let text = fixture.text(Target::Project);
    assert_eq!(
        text,
        format!(
            "{HEAD}\n[hooks]\nsetup = [\n  # three\n  \"c\", # third\n  # one\n  \"a\", # first\n  # two\n  \"b\", # second\n]\n"
        )
    );
    fixture.ok(
        Target::Project,
        Edit::MoveHook {
            phase: HookPhase::Setup,
            hook: HookRef::Index(0),
            to: Placement::After(HookRef::Index(2)),
        },
    );
    assert_eq!(runs(&fixture, HookPhase::Setup), ["a", "b", "c"]);
    fixture.ok(
        Target::Project,
        Edit::MoveHook {
            phase: HookPhase::Setup,
            hook: HookRef::Index(2),
            to: Placement::Before(HookRef::Index(1)),
        },
    );
    assert_eq!(runs(&fixture, HookPhase::Setup), ["a", "c", "b"]);
    // Moving onto itself is refused; moving to where it is changes nothing.
    let error = fixture
        .edit(
            Target::Project,
            Edit::MoveHook {
                phase: HookPhase::Setup,
                hook: HookRef::Index(0),
                to: Placement::Before(HookRef::Index(0)),
            },
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("hooks.setup"), "{error:#}");
    let same = fixture.ok(
        Target::Project,
        Edit::MoveHook {
            phase: HookPhase::Setup,
            hook: HookRef::Index(0),
            to: Placement::Start,
        },
    );
    assert!(!same.changed);
}

#[test]
fn a_one_line_list_can_be_reordered() {
    let fixture = Fixture::new("\n[hooks]\nsetup = [\"a\", \"b\", \"c\"]\n");
    fixture.ok(
        Target::Project,
        Edit::MoveHook {
            phase: HookPhase::Setup,
            hook: HookRef::Index(2),
            to: Placement::Start,
        },
    );
    assert!(
        fixture
            .text(Target::Project)
            .contains("setup = [\"c\", \"a\", \"b\"]\n")
    );
}

#[test]
fn hooks_written_as_array_of_tables_are_edited_too() {
    let fixture = Fixture::new(
        "\n[[hooks.setup]]\nname = \"install\"\nrun = \"bun install\"\n\n# commands\n[[hooks.setup]]\nname = \"gen\"\nrun = \"bun run gen\" # codegen\n",
    );
    fixture.ok(
        Target::Project,
        Edit::UpdateHook {
            phase: HookPhase::Setup,
            hook: by_name("gen"),
            entry: HookEntry {
                name: Some("gen".into()),
                run: "bun run generate".into(),
                timeout: Some("3m".into()),
            },
        },
    );
    fixture.ok(
        Target::Project,
        add(HookPhase::Setup, named("seed", "bun run seed")),
    );
    fixture.ok(
        Target::Project,
        Edit::MoveHook {
            phase: HookPhase::Setup,
            hook: by_name("seed"),
            to: Placement::Start,
        },
    );
    fixture.ok(
        Target::Project,
        Edit::RemoveHook {
            phase: HookPhase::Setup,
            hook: by_name("install"),
        },
    );
    let text = fixture.text(Target::Project);
    assert!(text.contains("# commands"), "{text}");
    assert!(text.contains("# codegen"), "{text}");
    let rows = fixture.files().hooks().unwrap().setup.hooks;
    let names: Vec<_> = rows.iter().map(|r| r.name.clone().unwrap()).collect();
    assert_eq!(names, ["seed", "gen"], "{text}");
    assert_eq!(rows[1].run, "bun run generate");
    assert_eq!(rows[1].timeout.as_deref(), Some("3m"));
}

#[test]
fn the_hooks_timeout_is_set_changed_and_removed() {
    let fixture = Fixture::new(
        "\n[hooks]\n# how long a command may run\ntimeout = \"10m\" # generous\nsetup = []\n",
    );
    fixture.ok(Target::Project, Edit::SetHooksTimeout(Some("20m".into())));
    let text = fixture.text(Target::Project);
    assert!(
        text.contains("# how long a command may run\ntimeout = \"20m\" # generous\n"),
        "{text}"
    );
    assert_eq!(fixture.files().hooks().unwrap().timeout, "20m");
    fixture.ok(Target::Project, Edit::SetHooksTimeout(None));
    assert_eq!(fixture.files().hooks().unwrap().timeout, "15m");
    assert!(!fixture.text(Target::Project).contains("timeout"));
    // Added when the table has none.
    fixture.ok(Target::Project, Edit::SetHooksTimeout(Some("1h".into())));
    assert_eq!(fixture.files().hooks().unwrap().timeout, "1h");
}

#[test]
fn recipes_are_set_replaced_and_unset_with_their_comments() {
    let fixture = Fixture::new(
        "\n[services.db]\nengine = \"postgres\"\n\n[env]\n# for the ORM\nDIRECT_URL = \"${DATABASE_URL}\" # same url\n",
    );
    fixture.ok(
        Target::Project,
        Edit::SetEnv {
            key: "APP_DB".into(),
            value: "${DATABASE_URL}".into(),
        },
    );
    fixture.ok(
        Target::Project,
        Edit::SetEnv {
            key: "DIRECT_URL".into(),
            value: "${DATABASE_URL}?sslmode=disable".into(),
        },
    );
    let text = fixture.text(Target::Project);
    assert!(
        text.contains("# for the ORM\nDIRECT_URL = \"${DATABASE_URL}?sslmode=disable\" # same url\nAPP_DB = \"${DATABASE_URL}\"\n"),
        "{text}"
    );
    let env = fixture.files().env().unwrap();
    assert_eq!(env.recipes.len(), 2);
    assert!(env.recipes.iter().all(|r| r.source == Target::Project));
    fixture.ok(
        Target::Project,
        Edit::UnsetEnv {
            key: "APP_DB".into(),
        },
    );
    let text = fixture.text(Target::Project);
    assert!(!text.contains("APP_DB"), "{text}");
    assert!(text.contains("# for the ORM"));
    // Not set: refused, with the key path only.
    let error = fixture
        .edit(Target::Project, Edit::UnsetEnv { key: "NOPE".into() })
        .unwrap_err();
    assert!(format!("{error:#}").contains("env.NOPE"), "{error:#}");
}

#[test]
fn presets_are_added_removed_and_the_empty_list_disappears() {
    let fixture = Fixture::new(
        "\n[services.db]\nengine = \"postgres\"\n\n[env]\npresets = [\"prisma\"]\nFOO = \"bar\"\n",
    );
    fixture.ok(Target::Project, Edit::AddPreset(EnvPreset::Spring));
    assert!(
        fixture
            .text(Target::Project)
            .contains("presets = [\"prisma\", \"spring\"]\n")
    );
    // Adding it again changes nothing.
    let again = fixture.ok(Target::Project, Edit::AddPreset(EnvPreset::Spring));
    assert!(!again.changed);
    fixture.ok(Target::Project, Edit::RemovePreset(EnvPreset::Prisma));
    fixture.ok(Target::Project, Edit::RemovePreset(EnvPreset::Spring));
    let text = fixture.text(Target::Project);
    assert!(!text.contains("presets"), "{text}");
    assert!(text.contains("FOO = \"bar\""));
    let error = fixture
        .edit(Target::Project, Edit::RemovePreset(EnvPreset::Laravel))
        .unwrap_err();
    assert!(format!("{error:#}").contains("env.presets"), "{error:#}");
    // And from nothing at all.
    let bare = Fixture::new("");
    bare.ok(Target::Project, Edit::AddPreset(EnvPreset::DbParts));
    assert!(
        bare.text(Target::Project)
            .contains("[env]\npresets = [\"db_parts\"]\n")
    );
}

#[test]
fn a_file_that_changed_since_it_was_read_is_never_overwritten() {
    let fixture = Fixture::new("\n[hooks]\nsetup = [\"a\"]\n");
    let seen = fixture.files().project.hash.clone();
    // Somebody else edits the file after the caller looked at it.
    let theirs = format!("{HEAD}\n[hooks]\nsetup = [\"a\", \"theirs\"]\n");
    fixture.write(Target::Project, &theirs);
    let error = apply(
        &fixture.root,
        Target::Project,
        seen.as_deref(),
        &add(HookPhase::Setup, "mine"),
    )
    .unwrap_err();
    let changed = error.downcast_ref::<ConfigChanged>().expect("typed");
    assert_eq!(changed.path, fixture.path(Target::Project));
    assert_eq!(fixture.text(Target::Project), theirs);
    // A file that did not exist when it was read, and does now, conflicts too.
    let error = apply(
        &fixture.root,
        Target::Local,
        Some("deadbeef"),
        &add(HookPhase::Setup, "x"),
    )
    .unwrap_err();
    assert!(error.downcast_ref::<ConfigChanged>().is_some());
    fixture.write(Target::Local, "[hooks]\nteardown = [\"t\"]\n");
    let error = apply(
        &fixture.root,
        Target::Local,
        None,
        &add(HookPhase::Setup, "x"),
    )
    .unwrap_err();
    assert!(error.downcast_ref::<ConfigChanged>().is_some());
}

/// Files in the project directory, to show that a refused write leaves
/// nothing behind.
fn dir_names(fixture: &Fixture) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(&fixture.root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with(".g"))
        .collect();
    names.sort();
    names
}

#[test]
fn a_save_that_lands_after_validation_is_not_overwritten() {
    // Validating the edit and calculating the trust scope take time on a big
    // project; a save in that window used to be overwritten by an
    // unconditional write.
    let fixture = Fixture::new("\n[hooks]\nsetup = [\"a\"]\n");
    let theirs = format!("{HEAD}\n[hooks]\nsetup = [\"a\", \"theirs\"]\n");
    let save = {
        let path = fixture.path(Target::Project);
        let theirs = theirs.clone();
        move || std::fs::write(&path, &theirs).unwrap()
    };
    let _seams = seams::install(seams::Hooks {
        before_write: Some(Box::new(save)),
        ..seams::Hooks::default()
    });
    let names_before = dir_names(&fixture);
    let error = fixture
        .edit(Target::Project, add(HookPhase::Setup, "mine"))
        .unwrap_err();
    let changed = error.downcast_ref::<ConfigChanged>().expect("typed");
    assert_eq!(changed.path, fixture.path(Target::Project));
    assert_eq!(changed.kept, None);
    assert_eq!(fixture.text(Target::Project), theirs, "their save stands");
    assert_eq!(dir_names(&fixture), names_before, "no leftover files");
}

#[test]
fn a_save_between_staging_and_the_swap_is_not_overwritten() {
    // The last moment: the new content is staged, the check has passed, and
    // an editor saves by rename (a different inode, a different mode).
    let fixture = Fixture::new("\n[hooks]\nsetup = [\"a\"]\n");
    let theirs = format!("{HEAD}\n[hooks]\nsetup = [\"a\", \"theirs\"]\n");
    let save = {
        let path = fixture.path(Target::Project);
        let theirs = theirs.clone();
        move || {
            let scratch = path.with_extension("save");
            std::fs::write(&scratch, &theirs).unwrap();
            std::fs::rename(&scratch, &path).unwrap();
        }
    };
    let _seams = seams::install(seams::Hooks {
        after_staging: Some(Box::new(save)),
        ..seams::Hooks::default()
    });
    let names_before = dir_names(&fixture);
    let error = fixture
        .edit(Target::Project, add(HookPhase::Setup, "mine"))
        .unwrap_err();
    assert!(error.downcast_ref::<ConfigChanged>().is_some(), "{error:#}");
    assert_eq!(fixture.text(Target::Project), theirs);
    assert_eq!(dir_names(&fixture), names_before, "no leftover files");
}

#[test]
fn a_local_file_created_meanwhile_is_not_overwritten() {
    // The caller saw no local file; one appears before the write.
    let fixture = Fixture::new("");
    let theirs = "[hooks]\nteardown = [\"theirs\"]\n";
    let save = {
        let path = fixture.path(Target::Local);
        move || std::fs::write(&path, theirs).unwrap()
    };
    let _seams = seams::install(seams::Hooks {
        before_write: Some(Box::new(save)),
        ..seams::Hooks::default()
    });
    let error = fixture
        .edit(Target::Local, add(HookPhase::Setup, "mine"))
        .unwrap_err();
    let changed = error.downcast_ref::<ConfigChanged>().expect("typed");
    assert_eq!(changed.path, fixture.path(Target::Local));
    assert_eq!(fixture.text(Target::Local), theirs);
}

#[test]
fn a_displaced_save_that_could_not_be_put_back_is_named_in_the_error() {
    let fixture = Fixture::new("\n[hooks]\nsetup = [\"a\"]\n");
    let path = fixture.path(Target::Project);
    let first = format!("{HEAD}\n[hooks]\nsetup = [\"a\", \"first\"]\n");
    let second = format!("{HEAD}\n[hooks]\nsetup = [\"a\", \"second\"]\n");
    // One save lands before the swap, another while devflow's version is at
    // the target.
    let first_save = {
        let (path, first) = (path.clone(), first.clone());
        move || {
            let scratch = path.with_extension("save");
            std::fs::write(&scratch, &first).unwrap();
            std::fs::rename(&scratch, &path).unwrap();
        }
    };
    let second_save = {
        let (path, second) = (path.clone(), second.clone());
        move || std::fs::write(&path, &second).unwrap()
    };
    let _seams = seams::install(seams::Hooks {
        after_staging: Some(Box::new(first_save)),
        after_exchange: Some(Box::new(second_save)),
        ..seams::Hooks::default()
    });
    let error = fixture
        .edit(Target::Project, add(HookPhase::Setup, "mine"))
        .unwrap_err();
    let changed = error.downcast_ref::<ConfigChanged>().expect("typed");
    let kept = changed.kept.clone().expect("the later save is kept");
    assert!(
        kept.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("devflow.toml.devflow-conflict-"),
        "{kept:?}"
    );
    assert_eq!(std::fs::read_to_string(&kept).unwrap(), second);
    assert_eq!(fixture.text(Target::Project), first);
    let message = format!("{error:#}");
    assert!(message.contains(&kept.display().to_string()), "{message}");
}

#[test]
fn an_edit_with_nothing_in_the_way_still_writes_and_keeps_the_mode() {
    // The conditional write replaces the file like the unconditional one did.
    let fixture = Fixture::new("\n[hooks]\nsetup = [\"a\"]\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            fixture.path(Target::Project),
            std::fs::Permissions::from_mode(0o640),
        )
        .unwrap();
    }
    let names_before = dir_names(&fixture);
    fixture.ok(Target::Project, add(HookPhase::Setup, "b"));
    assert_eq!(runs(&fixture, HookPhase::Setup), ["a", "b"]);
    assert_eq!(dir_names(&fixture), names_before);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(fixture.path(Target::Project))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o640, "the mode carries over");
    }
}

#[test]
fn an_edit_that_would_make_the_configuration_invalid_writes_nothing_and_names_only_keys() {
    let fixture = Fixture::new(
        "\n[services.db]\nengine = \"postgres\"\n\n[env]\npresets = [\"prisma\"]\n\n[hooks]\nsetup = [{ name = \"gen\", run = \"first-secret-command\" }]\n",
    );
    let before = fixture.text(Target::Project);
    let refused = |edit: Edit| -> String {
        let error = fixture.edit(Target::Project, edit).unwrap_err();
        format!("{error:#}")
    };
    // A duplicate name.
    let message = refused(add(
        HookPhase::Setup,
        named("gen", "another-secret-command"),
    ));
    assert!(
        message.contains("hooks.setup[1]") && message.contains("is already used by"),
        "{message}"
    );
    assert!(
        !message.contains("gen\"") && !message.contains("another-secret-command"),
        "{message}"
    );
    // A timeout that does not parse is not repeated.
    let message = refused(add(
        HookPhase::Setup,
        HookEntry {
            name: None,
            run: "zzz-run".into(),
            timeout: Some("5 parsecs".into()),
        },
    ));
    assert!(message.contains("hooks.setup[1].timeout"), "{message}");
    assert!(
        !message.contains("parsecs") && !message.contains("zzz-run"),
        "{message}"
    );
    let message = refused(Edit::SetHooksTimeout(Some("soon-ish".into())));
    assert!(
        message.contains("hooks.timeout") && !message.contains("soon"),
        "{message}"
    );
    let message = refused(Edit::SetHooksTimeout(Some("0s".into())));
    assert!(message.contains("hooks.timeout"), "{message}");
    // An empty command.
    let message = refused(add(HookPhase::Setup, "   "));
    assert!(
        message.contains("hooks.setup[1]") && message.contains("`run`"),
        "{message}"
    );
    // A recipe that a preset already sets.
    let message = refused(Edit::SetEnv {
        key: "DIRECT_URL".into(),
        value: "${DATABASE_URL}".into(),
    });
    assert!(
        message.contains("env.DIRECT_URL") && message.contains("prisma"),
        "{message}"
    );
    // A reference to something devflow does not generate.
    let message = refused(Edit::SetEnv {
        key: "OTHER".into(),
        value: "${NOT_A_THING}".into(),
    });
    assert!(message.contains("env.OTHER"), "{message}");
    // A reserved name, a bad name, a malformed reference, the word "presets".
    for (key, value) in [
        ("DATABASE_URL", "x"),
        ("1BAD", "x"),
        ("OK", "${"),
        ("presets", "x"),
    ] {
        let message = refused(Edit::SetEnv {
            key: key.into(),
            value: value.into(),
        });
        assert!(message.contains("env"), "{message}");
    }
    assert_eq!(fixture.text(Target::Project), before, "nothing was written");
}

#[test]
fn presets_that_clash_are_refused() {
    let fixture =
        Fixture::new("\n[services.db]\nengine = \"postgres\"\n\n[env]\npresets = [\"laravel\"]\n");
    let error = fixture
        .edit(Target::Project, Edit::AddPreset(EnvPreset::DbParts))
        .unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("env.presets") && message.contains("DB_HOST"),
        "{message}"
    );
}

#[test]
fn a_configuration_that_is_invalid_already_is_not_edited() {
    let fixture = Fixture::new("\n[hooks]\nsetup = [\"a\", { name = \"x\" }]\n");
    let error = fixture
        .edit(Target::Project, add(HookPhase::Setup, "b"))
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("not valid as it is"),
        "{error:#}"
    );
}

#[test]
fn credentials_are_not_written_to_the_committed_file_but_may_go_to_the_local_one() {
    let fixture = Fixture::new("\n[services.db]\nengine = \"postgres\"\n");
    let literal = "postgres://app:hunter2hunter2@db.example.com/shop";
    for edit in [
        Edit::SetEnv {
            key: "OLD_DB".into(),
            value: literal.into(),
        },
        Edit::SetEnv {
            key: "API_TOKEN".into(),
            value: "abc123".into(),
        },
        add(HookPhase::Setup, format!("psql {literal} -f seed.sql")),
    ] {
        let error = fixture.edit(Target::Project, edit).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            !message.contains("hunter2") && !message.contains("abc123"),
            "{message}"
        );
        assert!(message.contains("--local"), "{message}");
    }
    assert!(!fixture.text(Target::Project).contains("hunter2"));
    // A reference is not a credential, even under a secret-looking name.
    fixture.ok(
        Target::Project,
        Edit::SetEnv {
            key: "DB_PASSWORD_URL".into(),
            value: "${DATABASE_URL}".into(),
        },
    );
    // The local file takes them.
    fixture.ok(
        Target::Local,
        Edit::SetEnv {
            key: "API_TOKEN".into(),
            value: "abc123".into(),
        },
    );
    assert!(
        fixture
            .text(Target::Local)
            .contains("API_TOKEN = \"abc123\"")
    );
}

/// A local file with credentials under names that say so, and under names
/// that do not.
const LOCAL_SECRETS: &str = "[env]\n\
    APP_PASSWORD = \"sentinel-password\"\n\
    STRIPE_KEY = \"sentinel-key\"\n\
    SENTRY_DSN = \"https://sentinel-dsn@o1.ingest.example/2\"\n\
    LEGACY_DB = \"postgres://app:sentinel-url@db.example.com/shop\"\n\
    MIXED_TOKEN = \"Bearer ${PGUSER}-sentinel-mixed\"\n\
    ALIASED_PASSWORD = \"${PGPASSWORD}\"\n\
    DIRECT_URL = \"${DATABASE_URL}\"\n\
    FEATURE_FLAG = \"on\"\n"; // kingfisher:ignore

#[test]
fn recipe_rows_carry_masked_text_and_the_real_template_only_through_reveal() {
    let fixture = Fixture::new("\n[services.db]\nengine = \"postgres\"\n");
    fixture.write(Target::Local, LOCAL_SECRETS);
    let files = fixture.files();
    let view = files.env().unwrap();
    let row = |key: &str| view.recipes.iter().find(|r| r.key == key).unwrap().clone();
    // A literal under a secret name is masked whole, whatever it looks like.
    for key in ["APP_PASSWORD", "STRIPE_KEY", "SENTRY_DSN", "MIXED_TOKEN"] {
        let recipe = row(key);
        assert_eq!(recipe.template, "***", "{key}");
        assert!(recipe.masked, "{key}");
        assert_eq!(recipe.source, Target::Local, "{key}");
        assert_ne!(recipe.reveal(), "***", "{key}");
    }
    assert_eq!(row("APP_PASSWORD").reveal(), "sentinel-password");
    // A credential in the text of an ordinary name: masked in place.
    let legacy = row("LEGACY_DB");
    assert_eq!(legacy.template, "postgres://app:***@db.example.com/shop");
    assert!(legacy.masked);
    // References and plain literals are what they say.
    for (key, shown) in [
        ("ALIASED_PASSWORD", "${PGPASSWORD}"),
        ("DIRECT_URL", "${DATABASE_URL}"),
        ("FEATURE_FLAG", "on"),
    ] {
        let recipe = row(key);
        assert_eq!(recipe.template, shown, "{key}");
        assert!(!recipe.masked, "{key}");
        assert_eq!(recipe.reveal(), shown, "{key}");
    }
    // Nothing that renders the view can show a credential: not serde, not Debug.
    let json = serde_json::to_string(&view).unwrap();
    let debug = format!("{view:?} {:?}", row("APP_PASSWORD"));
    for text in [json, debug] {
        assert!(!text.contains("sentinel"), "{text}");
    }
}

#[test]
fn the_files_and_the_result_of_an_edit_do_not_print_their_text() {
    let fixture = Fixture::new("\n[services.db]\nengine = \"postgres\"\n");
    fixture.write(Target::Local, LOCAL_SECRETS);
    assert!(!format!("{:?}", fixture.files()).contains("sentinel"));
    let applied = fixture.ok(
        Target::Local,
        Edit::SetEnv {
            key: "OTHER_PASSWORD".into(),
            value: "sentinel-other".into(),
        },
    );
    assert!(
        applied.text.contains("sentinel-other"),
        "the text is there to edit"
    );
    assert!(!format!("{applied:?}").contains("sentinel"));
}

#[test]
fn masking_is_decided_by_the_name_of_the_variable_and_by_the_text() {
    use super::{masked_template, masked_value};
    for (key, template, shown) in [
        ("APP_PASSWORD", "hunter2", "***"),
        ("APP_PASSWORD", "${PGPASSWORD}", "${PGPASSWORD}"),
        ("APP_PASSWORD", "", ""),
        ("APP_PASSWORD", "$${literal", "***"),
        ("APP_PASSWORD", "${broken", "***"),
        ("API_TOKEN", "t-${DEVFLOW_WORKSPACE}", "***"),
        ("DIRECT_URL", "${DATABASE_URL}", "${DATABASE_URL}"),
        ("DIRECT_URL", "postgres://u:pw@h/d", "postgres://u:***@h/d"), // kingfisher:ignore
        ("EXTRA", "host=h password=pw", "host=h password=***"),
        ("PORT", "5432", "5432"),
    ] {
        assert_eq!(masked_template(key, template), shown, "{key} = {template}");
    }
    let known = ["generated-pw"];
    for (key, value, shown) in [
        ("APP_PASSWORD", "hunter2", "***"),
        ("APP_PASSWORD", "", ""),
        ("SESSION_KEY", "abc", "***"),
        ("PROJECT_LABEL", "app-shop", "app-shop"),
        ("PROJECT_LABEL", "x-generated-pw-y", "x-***-y"),
        ("DIRECT_URL", "postgres://u:pw@h/d", "postgres://u:***@h/d"), // kingfisher:ignore
    ] {
        assert_eq!(masked_value(key, value, &known), shown, "{key} = {value}");
    }
}

#[test]
fn local_edits_create_the_local_file_and_copy_the_committed_list_first() {
    let fixture = Fixture::new(
        "\n[hooks]\nsetup = [\"bun install\", \"bun run gen\"]\nswitch = [\"bun install\"]\n",
    );
    assert!(!fixture.path(Target::Local).exists());
    let applied = fixture.ok(Target::Local, add(HookPhase::Setup, "echo mine"));
    assert_eq!(applied.target, Target::Local);
    assert_eq!(applied.notes.len(), 1);
    assert!(
        applied.notes[0].contains("copied from devflow.toml"),
        "{:?}",
        applied.notes
    );
    assert_eq!(
        fixture.text(Target::Local),
        "[hooks]\nsetup = [\"bun install\", \"bun run gen\", \"echo mine\"]\n"
    );
    // The committed file is untouched; the effective list is the local one.
    assert!(
        fixture
            .text(Target::Project)
            .contains("setup = [\"bun install\", \"bun run gen\"]")
    );
    assert_eq!(
        runs(&fixture, HookPhase::Setup),
        ["bun install", "bun run gen", "echo mine"]
    );
    let view = fixture.files().hooks().unwrap();
    assert_eq!(view.setup.source, Target::Local);
    assert_eq!(view.setup.shadowed.len(), 2);
    assert_eq!(view.switch.source, Target::Project);
    assert!(view.switch.shadowed.is_empty());
    // An edit of the committed file works on the committed list, an edit of
    // the local file on the one in effect.
    assert_eq!(view.setup.rows_for(Target::Project).len(), 2);
    assert_eq!(view.setup.rows_for(Target::Local).len(), 3);
    assert_eq!(view.switch.rows_for(Target::Project).len(), 1);
    assert_eq!(view.switch.rows_for(Target::Local).len(), 1);
    // A second local edit of the same phase copies nothing more.
    let applied = fixture.ok(
        Target::Local,
        Edit::RemoveHook {
            phase: HookPhase::Setup,
            hook: HookRef::Index(0),
        },
    );
    assert!(applied.notes.is_empty(), "{:?}", applied.notes);
    assert_eq!(
        runs(&fixture, HookPhase::Setup),
        ["bun run gen", "echo mine"]
    );
    // Editing the committed list while the local one overrides it says so.
    let applied = fixture.ok(Target::Project, add(HookPhase::Setup, "bun run seed"));
    assert!(
        applied.notes.iter().any(|n| n.contains("no effect")),
        "{:?}",
        applied.notes
    );
}

#[test]
fn local_recipes_win_and_a_local_file_cannot_remove_a_committed_one() {
    let fixture = Fixture::new(
        "\n[services.db]\nengine = \"postgres\"\n\n[env]\nA = \"${DATABASE_URL}\"\npresets = [\"prisma\"]\n",
    );
    fixture.ok(
        Target::Local,
        Edit::SetEnv {
            key: "A".into(),
            value: "${PGHOST}".into(),
        },
    );
    let env = fixture.files().env().unwrap();
    assert_eq!(env.recipes[0].template, "${PGHOST}");
    assert_eq!(env.recipes[0].source, Target::Local);
    // A local recipe can be unset; the committed one shows again.
    let unset = fixture.ok(Target::Local, Edit::UnsetEnv { key: "A".into() });
    assert!(unset.changed);
    assert!(
        unset
            .notes
            .iter()
            .any(|n| n.contains("env.A") && n.contains("applies on this machine again")),
        "{:?}",
        unset.notes
    );
    let error = fixture
        .edit(Target::Local, Edit::UnsetEnv { key: "A".into() })
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("env.A is set in devflow.toml"),
        "{error:#}"
    );
    // Presets are a list: the local list starts as a copy.
    let applied = fixture.ok(Target::Local, Edit::AddPreset(EnvPreset::Spring));
    assert!(applied.notes.iter().any(|n| n.contains("env.presets")));
    let env = fixture.files().env().unwrap();
    assert_eq!(env.presets_source, Target::Local);
    assert_eq!(
        env.presets.iter().map(|p| p.preset).collect::<Vec<_>>(),
        [EnvPreset::Prisma, EnvPreset::Spring]
    );
    assert_eq!(env.shadowed_presets, [EnvPreset::Prisma]);
}

fn presets_in_effect(fixture: &Fixture) -> Vec<EnvPreset> {
    fixture.files().config().unwrap().env.presets
}

#[test]
fn removing_the_last_local_preset_keeps_an_empty_list_so_the_committed_ones_stay_off() {
    let fixture = Fixture::new(
        "\n[services.db]\nengine = \"postgres\"\n\n[env]\npresets = [\"prisma\"]\nFOO = \"bar\"\n",
    );
    assert_eq!(presets_in_effect(&fixture), [EnvPreset::Prisma]);
    let applied = fixture.ok(Target::Local, Edit::RemovePreset(EnvPreset::Prisma));
    // The local file says "none": a file without the key inherits the list.
    let local = fixture.text(Target::Local);
    assert!(local.contains("presets = []"), "{local}");
    assert!(
        applied
            .notes
            .iter()
            .any(|n| n.contains("keeps env.presets = []")),
        "{:?}",
        applied.notes
    );
    assert_eq!(presets_in_effect(&fixture), [], "prisma is off here");
    let env = fixture.files().env().unwrap();
    assert!(env.presets.is_empty());
    assert_eq!(env.presets_source, Target::Local);
    assert_eq!(env.shadowed_presets, [EnvPreset::Prisma]);
    // The committed file is untouched and still has them for everyone else.
    assert!(
        fixture
            .text(Target::Project)
            .contains("presets = [\"prisma\"]")
    );
    // Removing it again is an error, not a second chance to lose the list.
    let error = fixture
        .edit(Target::Local, Edit::RemovePreset(EnvPreset::Prisma))
        .unwrap_err();
    assert!(format!("{error:#}").contains("env.presets"), "{error:#}");
    // The empty local list takes a preset and gives it back the same way.
    fixture.ok(Target::Local, Edit::AddPreset(EnvPreset::Spring));
    assert_eq!(presets_in_effect(&fixture), [EnvPreset::Spring]);
    fixture.ok(Target::Local, Edit::RemovePreset(EnvPreset::Spring));
    assert!(fixture.text(Target::Local).contains("presets = []"));
    assert_eq!(presets_in_effect(&fixture), []);
}

#[test]
fn a_local_list_of_its_own_also_keeps_the_empty_list_when_it_loses_its_last_preset() {
    let fixture =
        Fixture::new("\n[services.db]\nengine = \"postgres\"\n\n[env]\npresets = [\"prisma\"]\n");
    fixture.write(Target::Local, "[env]\npresets = [\"spring\"]\n");
    assert_eq!(presets_in_effect(&fixture), [EnvPreset::Spring]);
    fixture.ok(Target::Local, Edit::RemovePreset(EnvPreset::Spring));
    assert!(fixture.text(Target::Local).contains("presets = []"));
    assert_eq!(
        presets_in_effect(&fixture),
        [],
        "not back to the committed prisma"
    );
}

#[test]
fn an_empty_local_list_is_dropped_when_there_is_nothing_to_hide() {
    // No committed list: the key would only be noise, and would silently
    // override presets added to the committed file later.
    let fixture = Fixture::new("\n[services.db]\nengine = \"postgres\"\n");
    fixture.ok(Target::Local, Edit::AddPreset(EnvPreset::Spring));
    assert!(fixture.text(Target::Local).contains("presets"));
    let applied = fixture.ok(Target::Local, Edit::RemovePreset(EnvPreset::Spring));
    assert!(!fixture.text(Target::Local).contains("presets"));
    assert!(applied.notes.is_empty(), "{:?}", applied.notes);
    // The committed file drops its own empty list too.
    let committed =
        Fixture::new("\n[services.db]\nengine = \"postgres\"\n\n[env]\npresets = [\"prisma\"]\n");
    committed.ok(Target::Project, Edit::RemovePreset(EnvPreset::Prisma));
    assert!(!committed.text(Target::Project).contains("presets"));
    // A committed list that is empty hides nothing either.
    let empty = Fixture::new("\n[services.db]\nengine = \"postgres\"\n\n[env]\npresets = []\n");
    empty.ok(Target::Local, Edit::AddPreset(EnvPreset::Spring));
    empty.ok(Target::Local, Edit::RemovePreset(EnvPreset::Spring));
    assert!(!empty.text(Target::Local).contains("presets"));
}

#[test]
fn removing_the_last_local_hook_keeps_an_empty_list_so_the_committed_ones_stay_off() {
    // Hooks lists are replaced whole by a local list too; an emptied one
    // must stay in the file as `[]`, for each way a list can be written.
    for committed in [
        "\n[hooks]\nsetup = [\"a\"]\n",
        "\n[hooks]\nsetup = [{ name = \"a\", run = \"a\" }]\n",
        "\n[[hooks.setup]]\nname = \"a\"\nrun = \"a\"\n",
    ] {
        let fixture = Fixture::new(committed);
        assert_eq!(runs(&fixture, HookPhase::Setup), ["a"], "{committed}");
        fixture.ok(
            Target::Local,
            Edit::RemoveHook {
                phase: HookPhase::Setup,
                hook: HookRef::Index(0),
            },
        );
        let local = fixture.text(Target::Local);
        assert!(local.contains("setup = []"), "{committed}\n{local}");
        assert!(
            runs(&fixture, HookPhase::Setup).is_empty(),
            "back to the committed hook: {committed}"
        );
        let view = fixture.files().hooks().unwrap();
        assert_eq!(view.setup.source, Target::Local);
        assert_eq!(view.setup.shadowed.len(), 1);
        // The empty list takes a command and gives it up again.
        fixture.ok(Target::Local, add(HookPhase::Setup, "mine"));
        assert_eq!(runs(&fixture, HookPhase::Setup), ["mine"]);
        fixture.ok(
            Target::Local,
            Edit::RemoveHook {
                phase: HookPhase::Setup,
                hook: HookRef::Index(0),
            },
        );
        assert!(runs(&fixture, HookPhase::Setup).is_empty());
        assert!(fixture.text(Target::Local).contains("setup = []"));
    }
}

#[test]
fn every_local_hook_edit_starts_from_the_committed_list_and_keeps_the_phases_apart() {
    let fixture =
        Fixture::new("\n[hooks]\nsetup = [\"a\", \"b\"]\nswitch = [\"s\"]\nteardown = [\"t\"]\n");
    // Update, move and remove each seed the one phase they touch and leave
    // the others inheriting.
    fixture.ok(
        Target::Local,
        Edit::UpdateHook {
            phase: HookPhase::Setup,
            hook: HookRef::Index(0),
            entry: HookEntry::new("a2"),
        },
    );
    fixture.ok(
        Target::Local,
        Edit::MoveHook {
            phase: HookPhase::Switch,
            hook: HookRef::Index(0),
            to: Placement::End,
        },
    );
    let local = fixture.text(Target::Local);
    assert!(local.contains("setup = [\"a2\", \"b\"]"), "{local}");
    assert!(local.contains("switch = [\"s\"]"), "{local}");
    assert!(!local.contains("teardown"), "{local}");
    fixture.ok(
        Target::Local,
        Edit::RemoveHook {
            phase: HookPhase::Switch,
            hook: HookRef::Index(0),
        },
    );
    assert!(runs(&fixture, HookPhase::Switch).is_empty());
    assert_eq!(
        runs(&fixture, HookPhase::Teardown),
        ["t"],
        "still inherited"
    );
    assert_eq!(runs(&fixture, HookPhase::Setup), ["a2", "b"]);
}

#[test]
fn only_changes_to_hooks_and_env_alter_the_trust_scope() {
    let fixture = Fixture::new("\n[hooks]\nsetup = [\"a\"]\n");
    assert!(
        fixture
            .ok(Target::Project, add(HookPhase::Setup, "b"))
            .scope_changed
    );
    assert!(
        fixture
            .ok(Target::Local, add(HookPhase::Switch, "c"))
            .scope_changed
    );
    assert!(
        fixture
            .ok(Target::Project, Edit::SetHooksTimeout(Some("30m".into())))
            .scope_changed
    );
    // The same list in other words is the same scope.
    let same = fixture.ok(
        Target::Project,
        Edit::UpdateHook {
            phase: HookPhase::Setup,
            hook: HookRef::Index(0),
            entry: HookEntry {
                name: None,
                run: "a".into(),
                timeout: Some("30m".into()),
            },
        },
    );
    assert!(!same.scope_changed);
    let noop = fixture.ok(Target::Project, Edit::SetHooksTimeout(Some("30m".into())));
    assert!(!noop.changed && !noop.scope_changed);
}

#[test]
fn a_preview_writes_nothing() {
    let fixture = Fixture::new("\n[hooks]\nsetup = [\"a\"]\n");
    let before = fixture.text(Target::Project);
    let hash = fixture.files().project.hash;
    let preview = preview(
        &fixture.root,
        Target::Project,
        hash.as_deref(),
        &add(HookPhase::Setup, "b"),
    )
    .unwrap();
    assert!(preview.changed && preview.text.contains("\"b\""));
    assert_eq!(fixture.text(Target::Project), before);
}

#[test]
fn a_linked_file_is_not_rewritten() {
    let fixture = Fixture::new("");
    let real = fixture.root.join("real-local.toml");
    std::fs::write(&real, "[hooks]\nsetup = [\"x\"]\n").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&real, fixture.path(Target::Local)).unwrap();
        let error = fixture
            .edit(Target::Local, add(HookPhase::Setup, "y"))
            .unwrap_err();
        assert!(format!("{error:#}").contains("symbolic link"), "{error:#}");
        assert_eq!(
            std::fs::read_to_string(&real).unwrap(),
            "[hooks]\nsetup = [\"x\"]\n"
        );
    }
}

#[test]
fn a_project_without_a_file_is_not_initialized() {
    let dir = tempfile::tempdir().unwrap();
    git2::Repository::init(dir.path()).unwrap();
    let error = ConfigFiles::read(dir.path()).unwrap_err();
    assert!(
        error
            .downcast_ref::<crate::error::NotInitialized>()
            .is_some()
    );
}

#[test]
fn references_and_preset_texts_are_available_to_frontends() {
    let config = crate::Config::parse(&format!(
        "{HEAD}\n[services.db]\nengine = \"postgres\"\nextra_databases = [\"test\"]\n"
    ))
    .unwrap();
    let names = available_references(&config);
    for wanted in [
        "DATABASE_URL",
        "PGHOST",
        "COMPOSE_PROJECT_NAME",
        "TEST_DATABASE_URL",
    ] {
        assert!(names.iter().any(|n| n == wanted), "{wanted} in {names:?}");
    }
    for preset in EnvPreset::ALL {
        assert!(!preset_label(preset).is_empty() && !preset_description(preset).is_empty());
    }
}

#[test]
fn the_hooks_view_names_the_file_of_each_list_and_the_timeout() {
    let fixture = Fixture::new(
        "\n[hooks]\ntimeout = \"10m\"\nsetup = [\"a\", { name = \"b\", run = \"c\", timeout = \"1m\" }]\n",
    );
    fixture.write(
        Target::Local,
        "[hooks]\ntimeout = \"20m\"\nteardown = [\"t\"]\n",
    );
    let view = fixture.files().hooks().unwrap();
    assert_eq!(view.timeout, "20m");
    assert_eq!(view.timeout_source, Target::Local);
    assert_eq!(view.setup.source, Target::Project);
    assert_eq!(view.setup.hooks[0].effective_timeout, "20m");
    assert_eq!(view.setup.hooks[1].effective_timeout, "1m");
    assert_eq!(view.teardown.source, Target::Local);
    assert!(view.teardown.shadowed.is_empty());
    assert!(view.switch.hooks.is_empty());
    assert!(!view.is_empty());
    assert_eq!(view.phase(HookPhase::Teardown).hooks[0].run, "t");
}

#[test]
fn hook_references_resolve_a_name_before_a_position() {
    let hooks = vec![named("2", "x"), named("b", "y")];
    assert_eq!(
        HookRef::resolve_text("2", &hooks),
        HookRef::Name("2".into())
    );
    assert_eq!(HookRef::resolve_text("1", &hooks), HookRef::Index(0));
    assert_eq!(
        HookRef::resolve_text("b", &hooks),
        HookRef::Name("b".into())
    );
    assert_eq!(
        HookRef::resolve_text("0", &hooks),
        HookRef::Name("0".into())
    );
    assert_eq!(
        HookRef::resolve_text("zzz", &hooks),
        HookRef::Name("zzz".into())
    );
}

#[test]
fn a_missing_hook_is_reported_without_repeating_what_was_asked() {
    let fixture = Fixture::new("\n[hooks]\nsetup = [\"a\"]\n");
    let error = fixture
        .edit(
            Target::Project,
            Edit::RemoveHook {
                phase: HookPhase::Setup,
                hook: by_name("super-secret-name"),
            },
        )
        .unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("hooks.setup") && !message.contains("super-secret"),
        "{message}"
    );
    let error = fixture
        .edit(
            Target::Project,
            Edit::RemoveHook {
                phase: HookPhase::Teardown,
                hook: HookRef::Index(0),
            },
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("hooks.teardown"));
    let error = fixture
        .edit(
            Target::Project,
            Edit::UpdateHook {
                phase: HookPhase::Setup,
                hook: HookRef::Index(4),
                entry: HookEntry::new("x"),
            },
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("position 5"), "{error:#}");
}
