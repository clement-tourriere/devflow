use super::*;
use crate::doctor::CheckStatus;
use crate::worktree::testutil::{self, TestRepo};

const OWN: &str = "postgres://own:own-password-8@localhost/own"; // kingfisher:ignore
const MAIN: &str = "postgres://main:main-password-9@localhost/main"; // kingfisher:ignore

/// A stand-in `mise`: `exec -- CMD` runs CMD after `exec_body` (shell) changed
/// the environment, as a mise configuration would; `env --json` prints
/// `env_json`; anything else fails.
fn mise(t: &TestRepo, exec_body: &str, env_json: &str) -> MiseCommand {
    let script = t.dir.path().join("fake-mise.sh");
    std::fs::write(
        &script,
        format!(
            "case \"$1\" in\n\
             exec) shift; while [ \"$1\" != -- ]; do shift; done; shift\n{exec_body}\nexec \"$@\";;\n\
             env) echo '{env_json}';;\n\
             *) exit 64;;\nesac\n"
        ),
    )
    .unwrap();
    MiseCommand {
        program: "/bin/sh".into(),
        leading_args: vec![script.into_os_string()],
        ..MiseCommand::default()
    }
}

fn with_env_file(t: &TestRepo) {
    env_file::write(&t.root, &[("DATABASE_URL".to_string(), OWN.to_string())]).unwrap();
}

fn project_with_setup(t: &TestRepo, hooks: &[&str]) -> Project {
    let mut project = t.project.clone();
    project.config.hooks.setup = hooks.iter().map(|h| (*h).into()).collect();
    project
}

fn status(checks: &[Check], name: &str) -> CheckStatus {
    checks
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("no {name} in {checks:?}"))
        .status
}

fn shown(checks: &[Check]) -> String {
    format!("{checks:?}")
}

const OVERRIDE: &str = "DATABASE_URL='postgres://main:main-password-9@localhost/main'; \
                        export DATABASE_URL"; // kingfisher:ignore

#[test]
fn nothing_is_said_outside_a_workspace_or_without_mise_or_the_generated_file() {
    let t = testutil::repo(None);
    let project = project_with_setup(&t, &["npm ci"]);
    let command = mise(&t, OVERRIDE, "{}");
    // No generated file yet.
    assert!(checks_in(&project, &t.root, &command).is_empty());
    with_env_file(&t);
    // No mise configuration and no hook that runs mise.
    assert!(checks_in(&project, &t.root, &command).is_empty());
    // Not in a workspace.
    let hooked = project_with_setup(&t, &["mise exec -- npm ci"]);
    assert!(checks_in(&hooked, t.dir.path(), &command).is_empty());
}

#[test]
fn a_hook_that_runs_mise_and_an_overriding_mise_fail_by_key_and_hook() {
    let t = testutil::repo(None);
    with_env_file(&t);
    let project = project_with_setup(&t, &["npm ci", "mise exec -- npm test"]);
    let checks = checks_in(&project, &t.root, &mise(&t, OVERRIDE, "{}"));
    let check = checks.iter().find(|c| c.name == "mise-env").unwrap();
    assert_eq!(check.status, CheckStatus::Fail, "{check:?}");
    assert!(check.detail.contains("DATABASE_URL"), "{check:?}");
    assert!(
        check
            .detail
            .contains("not routed through `devflow exec`: setup hook #2"),
        "{check:?}"
    );
    let hint = check.hint.as_deref().unwrap();
    assert!(
        hint.contains("devflow exec") && hint.contains("--skip-mise-check"),
        "{hint}"
    );
    let text = shown(&checks);
    assert!(
        !text.contains("password") && !text.contains(MAIN) && !text.contains(OWN),
        "{text}"
    );
}

#[test]
fn a_removed_variable_is_reported_as_removed() {
    let t = testutil::repo(None);
    with_env_file(&t);
    let project = project_with_setup(&t, &["mise x -- npm test"]);
    let checks = checks_in(&project, &t.root, &mise(&t, "unset DATABASE_URL", "{}"));
    let check = checks.iter().find(|c| c.name == "mise-env").unwrap();
    assert_eq!(check.status, CheckStatus::Fail, "{check:?}");
    assert!(
        check.detail.contains("mise removes DATABASE_URL"),
        "{check:?}"
    );
}

#[test]
fn hooks_routed_through_devflow_exec_are_not_listed_and_a_clean_mise_is_ok() {
    let t = testutil::repo(None);
    with_env_file(&t);
    std::fs::write(t.root.join("mise.toml"), "[tools]\n").unwrap();
    let project = project_with_setup(
        &t,
        &[
            "mise exec -- devflow exec -- npm ci",
            "cd app && mise x -- devflow exec -- b",
        ],
    );
    let checks = checks_in(&project, &t.root, &mise(&t, "true", "{}"));
    let check = checks.iter().find(|c| c.name == "mise-env").unwrap();
    assert_eq!(check.status, CheckStatus::Ok, "{check:?}");
    assert!(!check.detail.contains("not routed"), "{check:?}");
    // Even an overriding mise is not a failure for them: devflow exec wins.
    let checks = checks_in(&project, &t.root, &mise(&t, OVERRIDE, "{}"));
    assert_eq!(
        status(&checks, "mise-env"),
        CheckStatus::Warn,
        "the override is worth knowing about, but no hook is refused for it"
    );
}

#[test]
fn a_hook_the_check_cannot_stand_for_is_listed_and_warned_about() {
    let t = testutil::repo(None);
    with_env_file(&t);
    let project = project_with_setup(&t, &["cd app && mise exec -- npm ci", "mise run migrate"]);
    let checks = checks_in(&project, &t.root, &mise(&t, "true", "{}"));
    let check = checks.iter().find(|c| c.name == "mise-env").unwrap();
    assert_eq!(check.status, CheckStatus::Warn, "{check:?}");
    assert!(
        check
            .detail
            .contains("not routed through `devflow exec`: setup hook #1, setup hook #2"),
        "{check:?}"
    );
    assert!(check.hint.as_deref().unwrap().contains("devflow exec"));
}

#[test]
fn a_worktree_with_mise_configuration_of_any_name_is_checked_without_such_a_hook() {
    // `.config/mise/conf.d/a.toml` is on no short list of names.
    let t = testutil::repo(None);
    with_env_file(&t);
    std::fs::create_dir_all(t.root.join(".config/mise/conf.d")).unwrap();
    std::fs::write(t.root.join(".config/mise/conf.d/a.toml"), "[tools]\n").unwrap();
    let project = project_with_setup(&t, &["npm ci"]);
    let checks = checks_in(&project, &t.root, &mise(&t, OVERRIDE, "{}"));
    assert_eq!(
        status(&checks, "mise-env"),
        CheckStatus::Warn,
        "no hook is refused, but commands through mise there get another database"
    );
    let checks = checks_in(&project, &t.root, &mise(&t, "true", "{}"));
    assert_eq!(status(&checks, "mise-env"), CheckStatus::Ok);
}

#[test]
fn what_shells_get_is_a_separate_warning() {
    let t = testutil::repo(None);
    with_env_file(&t);
    std::fs::write(t.root.join("mise.toml"), "[tools]\n").unwrap();
    let project = project_with_setup(&t, &["mise exec -- devflow exec -- npm ci"]);
    // Commands devflow runs are fine; mise on its own loads nothing.
    let checks = checks_in(&project, &t.root, &mise(&t, "true", "{}"));
    assert_eq!(status(&checks, "mise-env"), CheckStatus::Ok);
    let wiring = checks.iter().find(|c| c.name == "mise-wiring").unwrap();
    assert_eq!(wiring.status, CheckStatus::Warn, "{wiring:?}");
    assert!(
        wiring.detail.contains(
            "shells in the worktree won't see the branch's variables through mise; commands \
             devflow runs are unaffected"
        ),
        "{wiring:?}"
    );
    // mise loads it.
    let json = format!(r#"{{"DATABASE_URL": "{OWN}"}}"#);
    let checks = checks_in(&project, &t.root, &mise(&t, "true", &json));
    assert_eq!(status(&checks, "mise-wiring"), CheckStatus::Ok);
    // mise resolves another database for shells.
    let json = format!(r#"{{"DATABASE_URL": "{MAIN}"}}"#);
    let checks = checks_in(&project, &t.root, &mise(&t, "true", &json));
    let wiring = checks.iter().find(|c| c.name == "mise-wiring").unwrap();
    assert_eq!(wiring.status, CheckStatus::Warn);
    assert!(wiring.detail.contains("DATABASE_URL"), "{wiring:?}");
    let text = shown(&checks);
    assert!(!text.contains("password") && !text.contains(MAIN), "{text}");
}

#[test]
fn an_untrusted_configuration_gets_an_offer_that_is_text_only() {
    let t = testutil::repo(None);
    with_env_file(&t);
    let project = project_with_setup(&t, &["mise exec -- npm ci"]);
    let script = t.dir.path().join("untrusted.sh");
    let config = t.root.join("mise.toml");
    std::fs::write(
        &script,
        format!(
            "echo 'mise ERROR Config files in {} are not trusted.' >&2\nexit 1\n",
            config.display()
        ),
    )
    .unwrap();
    let command = MiseCommand {
        program: "/bin/sh".into(),
        leading_args: vec![script.into_os_string()],
        ..MiseCommand::default()
    };
    let checks = checks_in(&project, &t.root, &command);
    let check = checks.iter().find(|c| c.name == "mise-env").unwrap();
    assert_eq!(check.status, CheckStatus::Warn, "{check:?}");
    let detail = &check.detail;
    // This one file...
    assert!(
        detail.contains(&format!("`mise trust {}`", config.display())),
        "{detail}"
    );
    // ...or the directory worktrees are made in, with what that implies.
    let base = crate::worktree::path_for(&project, "probe");
    let base = base.parent().unwrap().display().to_string();
    assert!(
        detail.contains(&format!("`mise settings add trusted_config_paths {base}`")),
        "{detail}"
    );
    assert!(
        detail.contains("every mise configuration file that appears under it"),
        "{detail}"
    );
    assert!(detail.contains("devflow runs neither"), "{detail}");
    assert!(!config.exists(), "nothing was written or trusted");
}

#[test]
fn a_missing_mise_is_a_warning() {
    let t = testutil::repo(None);
    with_env_file(&t);
    let project = project_with_setup(&t, &["mise exec -- npm ci"]);
    let missing = MiseCommand {
        program: t.dir.path().join("no-such-mise").into_os_string(),
        ..MiseCommand::default()
    };
    let checks = checks_in(&project, &t.root, &missing);
    assert_eq!(status(&checks, "mise-env"), CheckStatus::Warn, "{checks:?}");
}
