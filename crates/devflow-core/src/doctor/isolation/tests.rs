use super::*;
use crate::config::{Config, PostgresMode};
use crate::doctor::CheckStatus;
use crate::receipt::{DataBinding, Receipt, ServiceBinding};
use crate::worktree::testutil::{self, TestRepo};
use std::collections::BTreeMap;

const OWN: &str = "postgres://own:own-password-8@localhost/own_db"; // kingfisher:ignore
const MAIN: &str = "postgres://main:main-password-9@localhost/main_db"; // kingfisher:ignore

fn repo() -> TestRepo {
    testutil::repo(Some(&Config::template(
        "shop",
        uuid::Uuid::new_v4(),
        Some(PostgresMode::External),
    )))
}

/// The workspace's file, written as setup writes it.
fn write_env(t: &TestRepo) {
    let vars = vec![
        ("DATABASE_URL".to_string(), OWN.to_string()),
        ("DEVFLOW_WORKSPACE".to_string(), "main".to_string()),
    ];
    env_file::write_owned(
        &t.root,
        &vars,
        &env_owner::declared_names(&t.project.config),
        &BTreeSet::new(),
    )
    .unwrap();
}

/// A receipt binding a branch of the project's server to `database`.
fn bind(t: &TestRepo, database: &str) {
    let mut receipt = Receipt::new(t.project.id(), "main", None);
    receipt.services.insert(
        "db".into(),
        ServiceBinding::postgres(
            DataBinding {
                branch_id: Some(uuid::Uuid::new_v4()),
                database: database.into(),
                role: "role".into(),
                url: OWN.into(),
                source: "empty".into(),
                owned: true,
            },
            None,
        ),
    );
    let main = crate::worktree::list(&t.project).unwrap().remove(0);
    receipt.save(&main.git_dir).unwrap();
}

struct Fixture {
    environment: BTreeMap<String, String>,
    inherited: Inherited,
    reachable: bool,
    answer: Result<String, String>,
}

impl Fixture {
    fn new() -> Fixture {
        Fixture {
            environment: BTreeMap::new(),
            inherited: Inherited::default(),
            reachable: false,
            answer: Err("not asked".into()),
        }
    }

    fn set(mut self, name: &str, value: &str) -> Fixture {
        self.environment.insert(name.into(), value.into());
        self
    }

    fn run(&self, t: &TestRepo, cwd: &Path) -> Vec<Check> {
        let lookup = |name: &str| self.environment.get(name).cloned();
        let reachable = |_: &Project| self.reachable;
        let ask = |_: &str| self.answer.clone();
        checks_in(
            &t.project,
            cwd,
            &Process {
                inherited: self.inherited.clone(),
                lookup: &lookup,
                reachable: &reachable,
                ask: &ask,
            },
        )
    }
}

fn find<'a>(checks: &'a [Check], name: &str) -> &'a Check {
    checks
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("no {name} in {checks:?}"))
}

#[test]
fn nothing_is_said_outside_a_workspace_or_without_the_generated_file() {
    let t = repo();
    let fixture = Fixture::new().set("DATABASE_URL", MAIN);
    assert!(fixture.run(&t, &t.root).is_empty(), "no generated file yet");
    write_env(&t);
    assert!(
        fixture.run(&t, t.dir.path()).is_empty(),
        "not in a workspace"
    );
}

#[test]
fn an_environment_that_matches_or_sets_nothing_passes() {
    let t = repo();
    write_env(&t);
    let checks = Fixture::new().run(&t, &t.root);
    let check = find(&checks, "isolation");
    assert_eq!(check.status, CheckStatus::Ok, "{check:?}");
    // A shell that loaded this workspace's file.
    let checks = Fixture::new()
        .set("DATABASE_URL", OWN)
        .set("DEVFLOW_WORKSPACE", "main")
        .run(&t, &t.root);
    assert_eq!(find(&checks, "isolation").status, CheckStatus::Ok);
    // A variable that is not the workspace's to own is not looked at.
    let checks = Fixture::new().set("UNRELATED", "x").run(&t, &t.root);
    assert_eq!(find(&checks, "isolation").status, CheckStatus::Ok);
}

#[test]
fn another_workspaces_values_are_named_with_where_they_came_from_and_never_shown() {
    let t = repo();
    write_env(&t);
    let mut fixture = Fixture::new()
        .set("DATABASE_URL", MAIN)
        .set("PGHOST", "elsewhere")
        .set("DEVFLOW_WORKSPACE", "main")
        .set("UNRELATED", "x");
    fixture.inherited = Inherited {
        workspace_path: Some(t.dir.path().join("feature-a")),
        workspace_name: Some("feature-a".into()),
        ..Inherited::default()
    };
    let checks = fixture.run(&t, &t.root);
    let check = find(&checks, "isolation");
    assert_eq!(check.status, CheckStatus::Warn, "{check:?}");
    // PGHOST is not set by the file but is owned: the workspace has none.
    assert!(
        check.detail.contains("DATABASE_URL") && check.detail.contains("PGHOST"),
        "{check:?}"
    );
    assert!(
        check.detail.contains("workspace feature-a") && check.detail.contains("feature-a"),
        "{check:?}"
    );
    assert!(!check.detail.contains("UNRELATED"), "{check:?}");
    assert!(
        !check.detail.contains("main-password") && !check.detail.contains("elsewhere"),
        "{check:?}"
    );
    assert!(
        !check
            .hint
            .as_deref()
            .unwrap_or_default()
            .contains("`devflow"),
        "{check:?}"
    );
}

#[test]
fn an_older_file_without_the_list_is_reported_with_setup_as_the_remedy() {
    let t = repo();
    env_file::write(&t.root, &[("DATABASE_URL".to_string(), OWN.to_string())]).unwrap();
    let checks = Fixture::new().set("DATABASE_URL", MAIN).run(&t, &t.root);
    let check = find(&checks, "isolation");
    assert_eq!(check.status, CheckStatus::Warn, "{check:?}");
    assert!(check.detail.contains("earlier devflow"), "{check:?}");
    assert!(
        check
            .remedies
            .iter()
            .any(|r| matches!(r, Remedy::Setup { .. })),
        "{check:?}"
    );
    assert!(!check.detail.contains("main-password"), "{check:?}");
}

#[test]
fn an_environment_naming_a_workspace_that_is_gone_is_a_warning_with_the_override_as_remedy() {
    let t = repo();
    write_env(&t);
    let mut fixture = Fixture::new();
    fixture.inherited = Inherited {
        workspace_path: Some(t.dir.path().join("removed")),
        workspace_id: Some("an-id".into()),
        workspace_name: Some("removed".into()),
        ..Inherited::default()
    };
    let checks = fixture.run(&t, &t.root);
    let check = find(&checks, "isolation");
    assert_eq!(check.status, CheckStatus::Warn, "{check:?}");
    assert!(
        check.detail.contains("removed") && check.detail.contains("cannot be resolved"),
        "{check:?}"
    );
    assert_eq!(check.remedies, [Remedy::ClearInherited], "{check:?}");
    assert!(!check.detail.contains("an-id"), "{check:?}");
    // Told to go on, the check goes on too.
    fixture.inherited = fixture.inherited.clone().clearing_stale();
    let checks = fixture.run(&t, &t.root);
    assert_eq!(find(&checks, "isolation").status, CheckStatus::Ok);
}

#[test]
fn the_workspaces_url_is_asked_which_database_it_reaches() {
    let t = repo();
    write_env(&t);
    bind(&t, "own_db");
    let mut fixture = Fixture::new();
    // Not asked while the server does not answer.
    fixture.answer = Ok("own_db".into());
    let checks = fixture.run(&t, &t.root);
    assert!(!find(&checks, "isolation").detail.contains("answers"));
    fixture.reachable = true;
    let checks = fixture.run(&t, &t.root);
    let check = find(&checks, "isolation");
    assert_eq!(check.status, CheckStatus::Ok, "{check:?}");
    assert!(check.detail.contains("own_db"), "{check:?}");

    // Another database: a failure, with the workspace to set up again.
    fixture.answer = Ok("main_db".into());
    let checks = fixture.run(&t, &t.root);
    let check = find(&checks, "isolation");
    assert_eq!(check.status, CheckStatus::Fail, "{check:?}");
    assert!(
        check.detail.contains("main_db") && check.detail.contains("own_db"),
        "{check:?}"
    );
    assert!(
        check
            .remedies
            .iter()
            .any(|r| matches!(r, Remedy::Setup { .. })),
        "{check:?}"
    );

    // Unanswered: said, not a failure.
    fixture.answer = Err("the server did not accept the connection".into());
    let checks = fixture.run(&t, &t.root);
    let check = find(&checks, "isolation");
    assert_eq!(check.status, CheckStatus::Ok, "{check:?}");
    assert!(check.detail.contains("was not asked"), "{check:?}");

    // The worst of both is what is shown, with both said.
    fixture.answer = Ok("main_db".into());
    fixture
        .environment
        .insert("DATABASE_URL".into(), MAIN.into());
    let checks = fixture.run(&t, &t.root);
    let check = find(&checks, "isolation");
    assert_eq!(check.status, CheckStatus::Fail, "{check:?}");
    assert!(
        check.detail.contains("in the process environment"),
        "{check:?}"
    );
    for check in &checks {
        assert!(
            !check.detail.contains("password"),
            "{check:?} must not show a value"
        );
    }
}

#[test]
fn a_database_url_attached_by_hand_is_never_connected_to() {
    let t = repo();
    write_env(&t);
    let mut receipt = Receipt::new(t.project.id(), "main", None);
    receipt.services.insert(
        "db".into(),
        ServiceBinding::postgres(
            DataBinding {
                branch_id: None,
                database: "own_db".into(),
                role: "role".into(),
                url: OWN.into(),
                source: "url".into(),
                owned: false,
            },
            None,
        ),
    );
    let main = crate::worktree::list(&t.project).unwrap().remove(0);
    receipt.save(&main.git_dir).unwrap();
    let mut fixture = Fixture::new();
    fixture.reachable = true;
    fixture.answer = Ok("main_db".into());
    let checks = fixture.run(&t, &t.root);
    assert_eq!(find(&checks, "isolation").status, CheckStatus::Ok);
}

#[test]
fn a_copied_dotenv_that_disagrees_is_a_warning_by_file_and_name() {
    let t = repo();
    write_env(&t);
    std::fs::write(
        t.root.join(".env"),
        format!("DATABASE_URL={MAIN}\nPORT=3000\n"),
    )
    .unwrap();
    let checks = Fixture::new().run(&t, &t.root);
    let check = find(&checks, "dotenv");
    assert_eq!(check.status, CheckStatus::Warn, "{check:?}");
    assert!(
        check.detail.contains(".env") && check.detail.contains("DATABASE_URL"),
        "{check:?}"
    );
    assert!(!check.detail.contains("main-password"), "{check:?}");
    assert!(!check.detail.contains("PORT"), "{check:?}");
    assert!(
        check
            .remedies
            .iter()
            .any(|r| matches!(r, Remedy::OpenFile { path } if path.ends_with(".env"))),
        "{check:?}"
    );
    // Agreeing values say nothing.
    std::fs::write(t.root.join(".env"), format!("DATABASE_URL={OWN}\n")).unwrap();
    let checks = Fixture::new().run(&t, &t.root);
    assert!(checks.iter().all(|c| c.name != "dotenv"), "{checks:?}");
}

#[test]
fn current_database_gives_up_without_a_server_and_names_nothing() {
    // Nothing listens on port 1.
    let error = current_database("postgres://u:topsecret-1@127.0.0.1:1/db").unwrap_err(); // kingfisher:ignore
    assert!(!error.contains("topsecret"), "{error}");
    let error = current_database("not a url at all \u{0}").unwrap_err();
    assert!(!error.contains("not a url"), "{error}");
}
