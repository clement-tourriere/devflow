use super::*;
use std::time::Duration;

const DB: &str = "postgres://devflow:db-password-1@localhost:5432/branch_db"; // kingfisher:ignore
const MAIN: &str = "postgres://main:main-password-2@localhost:5432/main_db"; // kingfisher:ignore

/// A stand-in `mise`: it logs the subcommand it is asked for in `$LOG`;
/// `env --json` succeeds (the configuration is trusted) and `exec [flags] --
/// CMD…` drops everything up to `--`.
const EXEC: &str = "echo \"$1\" >> \"$LOG\"\n\
                    case \"$1\" in env) echo '{}'; exit 0;; \
                    exec) shift; while [ \"$1\" != -- ]; do shift; done; shift;; \
                    *) exit 64;; esac\n";

struct Fixture {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    vars: Vec<(String, String)>,
    scratch: std::path::PathBuf,
    log: std::path::PathBuf,
}

impl Fixture {
    /// A worktree with `.devflow/env` holding `DATABASE_URL` and `PGHOST`.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("worktree");
        std::fs::create_dir_all(&root).unwrap();
        let vars = vec![
            ("DATABASE_URL".to_string(), DB.to_string()),
            ("PGHOST".to_string(), "localhost".to_string()),
        ];
        env_file::write(&root, &vars).unwrap();
        let scratch = dir.path().join("seen");
        let log = dir.path().join("log");
        Self {
            _dir: dir,
            root,
            vars,
            scratch,
            log,
        }
    }

    /// The subcommands mise was asked for, in order.
    fn asked(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// A mise that runs the command after `--` with `body` (shell) applied
    /// first, as a mise whose configuration changes the environment would.
    fn mise(&self, body: &str) -> MiseCommand {
        let script = self.root.parent().unwrap().join("fake-mise.sh");
        std::fs::write(&script, format!("{EXEC}{body}\nexec \"$@\"\n")).unwrap();
        MiseCommand {
            program: "/bin/sh".into(),
            leading_args: vec![script.into_os_string()],
            envs: vec![
                ("SEEN".into(), self.scratch.clone().into_os_string()),
                ("LOG".into(), self.log.clone().into_os_string()),
            ],
            ..MiseCommand::default()
        }
    }

    fn check(&self, hooks: &[HookCommand], skip: bool, mise: &MiseCommand) -> Verdict {
        check(&self.root, hooks, &self.vars, skip, mise)
    }
}

fn hook(run: &str) -> HookCommand {
    HookCommand::new(run, Duration::from_secs(5))
}

fn named(name: &str, run: &str) -> HookCommand {
    HookCommand {
        name: Some(name.into()),
        ..hook(run)
    }
}

#[test]
fn a_mise_that_overrides_blocks_every_hook_and_names_the_key() {
    let f = Fixture::new();
    let mise = f.mise(&format!("DATABASE_URL='{MAIN}'; export DATABASE_URL"));
    let verdict = f.check(
        &[hook("echo hi"), hook("mise exec -- npm ci")],
        false,
        &mise,
    );
    let block = verdict.block.expect("blocked");
    assert!(
        block.contains("mise resolves DATABASE_URL from another source than .devflow/env"),
        "{block}"
    );
    assert!(block.contains("setup hook #2"), "{block}");
    assert!(block.contains("mise.local.toml"), "{block}");
    assert!(block.contains("devflow exec"), "{block}");
    assert!(block.contains("--skip-mise-check"), "{block}");
    assert!(!block.contains("main-password") && !block.contains("db-password"));
    assert!(!block.contains("npm ci"), "no command text: {block}");
}

#[test]
fn a_variable_mise_removes_is_named_as_removed() {
    // `[env] DATABASE_URL = false` without loading `.devflow/env`.
    let f = Fixture::new();
    let mise = f.mise("unset DATABASE_URL");
    let verdict = f.check(&[named("install", "mise x -- npm ci")], false, &mise);
    let block = verdict.block.expect("blocked");
    assert!(
        block.contains("mise removes DATABASE_URL") && block.contains("the setup hook 'install'"),
        "{block}"
    );
    assert!(block.contains("devflow exec"), "{block}");
    assert!(!block.contains("PGHOST"), "only the removed key: {block}");
}

#[test]
fn the_probe_gets_the_environment_the_hook_gets() {
    // A generated `MISE_ENV=production` selects other configuration for the
    // hook; the probe must run with it, as the hook does.
    let mut f = Fixture::new();
    f.vars.push(("MISE_ENV".into(), "production".into()));
    let mise = f.mise("echo \"$MISE_ENV $MISE_AUTO_INSTALL $MISE_EXEC_AUTO_INSTALL\" > \"$SEEN\"");
    let verdict = f.check(&[hook("mise exec -- npm ci")], false, &mise);
    assert_eq!(verdict, Verdict::default());
    assert_eq!(
        std::fs::read_to_string(&f.scratch).unwrap().trim(),
        "production 0 0",
        "MISE_ENV is injected; tools are not installed by the check"
    );
}

#[test]
fn the_escape_hatch_skips_the_check_and_says_so() {
    let f = Fixture::new();
    let mise = f.mise(&format!("DATABASE_URL='{MAIN}'; export DATABASE_URL"));
    let verdict = f.check(&[hook("mise exec -- npm ci")], true, &mise);
    assert_eq!(verdict.block, None);
    assert!(verdict.skipped);
    assert!(
        verdict
            .warnings
            .iter()
            .any(|w| w.contains("--skip-mise-check"))
    );
    // Nothing to skip when no hook goes through mise: nothing is recorded.
    let verdict = f.check(&[hook("echo hi")], true, &mise);
    assert_eq!(verdict, Verdict::default());
}

#[test]
fn a_delivering_mise_lets_the_hooks_run() {
    let f = Fixture::new();
    let mise = f.mise("true");
    assert_eq!(
        f.check(&[hook("mise exec -- npm ci")], false, &mise),
        Verdict::default()
    );
}

#[test]
fn hooks_that_do_not_run_mise_are_not_checked_at_all() {
    // Even a mise that would override everything is never asked, and a
    // worktree with mise configuration of any name does not change that.
    let f = Fixture::new();
    std::fs::create_dir_all(f.root.join(".config/mise/conf.d")).unwrap();
    std::fs::write(f.root.join(".config/mise/conf.d/a.toml"), "[tools]\n").unwrap();
    let asked = f.scratch.display().to_string();
    let mise = f.mise(&format!("touch '{asked}'"));
    let verdict = f.check(&[hook("npm ci"), hook("echo done")], false, &mise);
    assert_eq!(verdict, Verdict::default());
    assert!(!f.scratch.exists(), "mise was not run");
}

#[test]
fn a_hook_routed_through_devflow_exec_needs_no_check_of_the_variables() {
    let f = Fixture::new();
    // mise would override everything, but devflow exec sets the variables
    // after it: the hooks run, and mise is only asked whether it trusts the
    // configuration, never what the hook would receive.
    let mise = f.mise(&format!("DATABASE_URL='{MAIN}'; export DATABASE_URL"));
    let hooks = [
        hook("mise exec -- devflow exec -- npm ci"),
        hook("cd app && mise x -- devflow exec -- npm ci"),
        hook("npm test"),
    ];
    assert_eq!(f.check(&hooks, false, &mise), Verdict::default());
    assert_eq!(f.asked(), ["env"], "trust only, no command was run");
    // A hook that is not routed is asked about as well.
    std::fs::remove_file(&f.log).unwrap();
    let block = f
        .check(
            &[
                hook("mise exec -- devflow exec -- a"),
                hook("mise exec -- b"),
            ],
            false,
            &mise,
        )
        .block
        .expect("blocked");
    assert!(block.contains("setup hook #2"), "{block}");
    assert!(!block.contains("setup hook #1"), "{block}");
    assert_eq!(
        f.asked(),
        ["env", "exec"],
        "the trust check, then the variables"
    );
}

#[test]
fn a_mise_started_after_devflow_exec_or_beside_it_is_not_protected_by_it() {
    // mise overrides everything; only a hook that devflow exec wraps wholly,
    // inside mise, is safe from it.
    let f = Fixture::new();
    let mise = f.mise(&format!("DATABASE_URL='{MAIN}'; export DATABASE_URL"));
    for run in [
        // The mise inside applies its own [env] after devflow set the variables.
        "mise exec -- devflow exec -- mise exec -- npm run migrate",
        "mise exec -- devflow exec -- env A=1 mise exec -- npm run migrate",
        "mise exec -- devflow exec -- sh -c 'mise exec -- npm run migrate'",
        // ... behind a wrapper with options of its own, in a string for a shell.
        "mise exec -- devflow exec -- sh -c 'env -u UNUSED mise exec -- npm run migrate'",
        "mise exec -- devflow exec -- sh -c 'sudo -E mise exec -- npm run migrate'",
        "mise exec -- devflow exec -- sh -c 'timeout 5 mise exec -- npm run migrate'",
        "mise exec -c 'devflow exec -- eval \"mise exec -- npm run migrate\"'",
        // Without an outer mise, a wrapped command that starts one is the use.
        "devflow exec -- sh -c 'env -u UNUSED mise exec -- npm run migrate'",
        "devflow exec -- sh -c 'timeout 5 mise exec -- npm run migrate'",
        "sudo -E mise exec -- npm run migrate",
        "env -u UNUSED mise exec -- npm run migrate",
        // The command beside it runs under mise, outside the wrapper.
        "mise exec -c 'devflow exec -- true; npm run migrate'",
        "mise exec -- sh -c 'devflow exec -- true; mise exec -- npm run migrate'",
        "mise exec -- devflow exec -- true; mise exec -- npm run migrate",
        // A protected use does not vouch for a mise the scanner cannot classify.
        "mise exec -- devflow exec -- true; echo \"mise exec -- bun run migrate\" | sh",
        "mise exec -- devflow exec -- true && printf 'mise run migrate' | bash",
        "echo 'mise run migrate' | sh; mise exec -- devflow exec -- true",
    ] {
        let block = f
            .check(&[hook(run)], false, &mise)
            .block
            .unwrap_or_else(|| panic!("{run}: not blocked"));
        assert!(
            block.contains("mise resolves DATABASE_URL from another source"),
            "{run}: {block}"
        );
    }
    // The wrapped command alone is protected, and the check is not even asked.
    std::fs::remove_file(&f.log).ok();
    assert_eq!(
        f.check(
            &[hook("mise exec -- devflow exec -- npm run migrate")],
            false,
            &mise
        ),
        Verdict::default()
    );
    assert_eq!(f.asked(), ["env"], "trust only");
}

#[test]
fn a_hook_routed_through_devflow_exec_still_needs_mise_and_its_trust() {
    let f = Fixture::new();
    let missing = MiseCommand {
        program: f.root.join("no-such-mise").into_os_string(),
        ..MiseCommand::default()
    };
    let block = f
        .check(
            &[hook("mise exec -- devflow exec -- npm ci")],
            false,
            &missing,
        )
        .block
        .expect("blocked");
    assert!(block.contains("setup hook #1 runs it"), "{block}");
    // A configuration mise refuses stops it too, and nothing trusted it.
    let script = f.root.parent().unwrap().join("untrusted.sh");
    std::fs::write(
        &script,
        "echo 'mise ERROR Config files in /w/mise.toml are not trusted.' >&2\nexit 1\n",
    )
    .unwrap();
    let untrusted = MiseCommand {
        program: "/bin/sh".into(),
        leading_args: vec![script.into_os_string()],
        ..MiseCommand::default()
    };
    let block = f
        .check(
            &[hook("mise exec -- devflow exec -- npm ci")],
            false,
            &untrusted,
        )
        .block
        .expect("blocked");
    assert!(block.contains("mise trust /w/mise.toml"), "{block}");
    // A trust check that fails otherwise says nothing about hooks that need
    // none of mise's configuration.
    let broken = f.root.parent().unwrap().join("broken.sh");
    std::fs::write(&broken, "exit 3\n").unwrap();
    let broken = MiseCommand {
        program: "/bin/sh".into(),
        leading_args: vec![broken.into_os_string()],
        ..MiseCommand::default()
    };
    assert_eq!(
        f.check(
            &[hook("mise exec -- devflow exec -- npm ci")],
            false,
            &broken
        ),
        Verdict::default()
    );
}

#[test]
fn hooks_the_probe_cannot_stand_for_warn_by_name_and_do_not_block() {
    let f = Fixture::new();
    // This mise would override the variables, but these hooks are not blocked
    // because the check cannot tell what they get.
    let mise = f.mise(&format!("DATABASE_URL='{MAIN}'; export DATABASE_URL"));
    let hooks = [
        named("web", "cd app && mise exec -- npm ci"),
        hook("mise run migrate --token=s3cr3t-token-value"), // kingfisher:ignore
        hook("mise r migrate"),
        hook("mise tasks run migrate"),
        hook("mise -C app exec -- npm ci"),
    ];
    let verdict = f.check(&hooks, false, &mise);
    assert_eq!(verdict.block, None, "{verdict:?}");
    assert_eq!(verdict.warnings.len(), 5, "{:?}", verdict.warnings);
    let all = verdict.warnings.join("\n");
    assert!(
        all.contains("the setup hook 'web' changes directory"),
        "{all}"
    );
    assert!(all.contains("setup hook #2 runs a mise task"), "{all}");
    assert!(
        all.contains("setup hook #5 tells mise which directory"),
        "{all}"
    );
    assert!(
        all.contains("mise exec -- devflow exec -- COMMAND"),
        "{all}"
    );
    assert!(
        !all.contains("s3cr3t") && !all.contains("npm ci") && !all.contains("migrate"),
        "no command text: {all}"
    );
    assert!(!f.scratch.exists());
}

#[test]
fn a_representable_hook_still_blocks_next_to_one_that_cannot_be_checked() {
    let f = Fixture::new();
    let mise = f.mise(&format!("DATABASE_URL='{MAIN}'; export DATABASE_URL"));
    let verdict = f.check(
        &[hook("mise run migrate"), hook("mise exec -- npm ci")],
        false,
        &mise,
    );
    let block = verdict.block.expect("blocked");
    assert!(block.contains("setup hook #2"), "{block}");
    assert!(!block.contains("setup hook #1"), "{block}");
    assert_eq!(verdict.warnings.len(), 1);
}

#[test]
fn a_missing_mise_blocks_only_when_a_hook_runs_it() {
    let f = Fixture::new();
    let missing = MiseCommand {
        program: f.root.join("no-such-mise").into_os_string(),
        ..MiseCommand::default()
    };
    let plain = f.check(&[hook("npm ci"), hook("echo done")], false, &missing);
    assert_eq!(plain, Verdict::default());
    let blocked = f.check(
        &[hook("echo hi"), named("install", "mise exec -- npm ci")],
        false,
        &missing,
    );
    let block = blocked.block.expect("blocked");
    assert!(
        block.contains("the setup hook 'install' runs it"),
        "{block}"
    );
    let unnamed = f.check(&[hook("echo hi"), hook("mise exec -- x")], false, &missing);
    assert!(unnamed.block.expect("blocked").contains("setup hook #2"));
}

#[test]
fn nothing_is_checked_without_the_generated_file() {
    let f = Fixture::new();
    let mise = f.mise("unset DATABASE_URL");
    std::fs::remove_file(env_file::path(&f.root)).unwrap();
    assert_eq!(
        f.check(&[hook("mise exec -- x")], false, &mise),
        Verdict::default()
    );
}

#[test]
fn an_untrusted_configuration_is_never_trusted_for_the_user() {
    let f = Fixture::new();
    let script = f.root.parent().unwrap().join("untrusted.sh");
    std::fs::write(
        &script,
        "echo 'mise ERROR Config files in /w/mise.toml are not trusted.' >&2\nexit 1\n",
    )
    .unwrap();
    let mise = MiseCommand {
        program: "/bin/sh".into(),
        leading_args: vec![script.into_os_string()],
        ..MiseCommand::default()
    };
    let block = f
        .check(&[hook("mise exec -- x")], false, &mise)
        .block
        .expect("blocked");
    assert!(block.contains("mise trust /w/mise.toml"), "{block}");
    assert!(block.contains("devflow doctor"), "{block}");
    assert!(!block.contains("--skip-mise-check"), "{block}");
}

#[test]
fn a_check_that_fails_blocks_without_quoting_mise() {
    let f = Fixture::new();
    let script = f.root.parent().unwrap().join("broken.sh");
    std::fs::write(&script, "echo 'PASSWORD=stderr-secret-3' >&2\nexit 3\n").unwrap(); // kingfisher:ignore
    let mise = MiseCommand {
        program: "/bin/sh".into(),
        leading_args: vec![script.into_os_string()],
        ..MiseCommand::default()
    };
    let block = f
        .check(&[hook("mise exec -- x")], false, &mise)
        .block
        .expect("blocked");
    assert!(
        block.contains("code 3") && block.contains("--skip-mise-check"),
        "{block}"
    );
    assert!(!block.contains("stderr-secret"), "{block}");
}
