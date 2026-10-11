use super::*;

#[test]
fn commands_that_run_mise_are_recognized_by_their_command_word() {
    for (command, subs) in [
        ("mise exec -- npm install", vec!["exec"]),
        ("mise x -- npm ci", vec!["x"]),
        ("mise run db:migrate", vec!["run"]),
        ("mise install", vec!["install"]),
        ("mise", vec![""]),
        ("cd 'apps/web' && mise exec -- npm ci", vec!["exec"]),
        ("FOO=1 mise run x", vec!["run"]),
        ("sudo mise trust", vec!["trust"]),
        ("env A=b mise exec -- true", vec!["exec"]),
        ("/opt/homebrew/bin/mise run x", vec!["run"]),
        ("true; mise install || mise trust", vec!["install", "trust"]),
        ("echo hi | mise exec -- cat", vec!["exec"]),
        ("(mise run a)", vec!["run"]),
        ("if true; then mise run a; fi", vec!["run"]),
        ("sh -c 'mise exec -- npm ci'", vec!["exec"]),
        ("bash -lc \"cd x && mise run y\"", vec!["run"]),
        ("mise -q exec -- true", vec!["exec"]),
        ("echo $(mise exec -- pwd)", vec!["exec"]),
        // The value of a global flag is not the subcommand.
        ("mise -C app exec -- true", vec!["exec"]),
        ("mise --cd=app run x", vec!["run"]),
    ] {
        assert_eq!(mise_subcommands(command), subs, "{command}");
        assert!(command_uses_mise(command), "{command}");
    }
    for command in [
        "npm install",
        "echo mise",
        "git commit -m 'mise: update'",
        "echo \"mise exec\"",
        "promise exec",
        "./mise-wrapper.sh",
        "mise_env=1 npm ci",
        "# mise exec",
        "npx mise-tools",
        "",
    ] {
        assert!(!command_uses_mise(command), "{command}");
    }
    // A wrapper that runs a command does not hide it.
    assert!(command_uses_mise("xargs mise exec -- true"));
    // Documented limit: a command chosen at run time. (After `devflow exec --`
    // it makes the daemon not wrapped, see `command_is_wholly_devflow_exec`.)
    assert!(!command_uses_mise("$TOOL exec -- true"));
}

fn only(command: &str) -> MiseUse {
    let mut uses = mise_uses(command);
    assert_eq!(uses.len(), 1, "{command}: {uses:?}");
    uses.remove(0)
}

#[test]
fn devflow_exec_inside_mise_has_the_last_word() {
    for command in [
        "mise exec -- devflow exec -- npm ci",
        "mise x -- devflow exec -- npm ci",
        "cd 'apps/web' && mise exec -- devflow exec -- npm ci",
        "mise exec node@22 -- devflow exec -- npm ci",
        "mise -q exec -- /usr/local/bin/devflow exec -- npm ci",
        "mise exec -- env A=1 devflow -C x exec -- npm ci",
        "mise exec -- sh -c 'devflow exec -- npm ci'",
        "mise exec -c 'cd app && devflow exec -- npm ci'",
        "mise exec --command='devflow exec -- npm ci'",
        "MISE_ENV=ci mise exec -- devflow exec -- npm ci",
    ] {
        let found = only(command);
        assert!(found.through_devflow_exec, "{command}");
        assert_eq!(found.unfaithful, None, "{command}: protected, so faithful");
    }
}

#[test]
fn anything_else_leaves_mise_the_last_word() {
    for command in [
        "mise exec -- npm ci",
        // devflow exec outside mise: mise then applies its [env] over it.
        "devflow exec -- mise exec -- npm ci",
        // Something else runs next to it in the same shell.
        "mise exec -c 'devflow exec -- a && b'",
        "mise exec -- sh -c 'b; devflow exec -- a'",
        "mise exec -- sh -c 'npm ci'",
        "mise exec -- devflow doctor",
        "mise exec -- echo devflow exec",
    ] {
        let found = only(command);
        assert!(!found.through_devflow_exec, "{command}");
    }
}

#[test]
fn a_mise_after_devflow_exec_leaves_mise_the_last_word_again() {
    // `devflow exec` sets the variables, then the mise inside applies its own
    // `[env]` over them: the outer mise is not protected, and the inner one is
    // not either. No use of mise counts as protected, so the hook is one the
    // gate checks (a wrapper the scanner cannot read, `sudo -E mise`, still
    // makes the outer use unprotected).
    for command in [
        "mise exec -- devflow exec -- mise exec -- npm run migrate",
        "mise exec -- devflow exec -- env A=1 mise run migrate",
        "mise exec -- devflow exec -- sudo -E mise exec -- npm run migrate",
        "mise exec -- devflow exec -- sh -c 'mise exec -- npm run migrate'",
        "mise exec -c 'devflow exec -- mise exec -- npm run migrate'",
        "mise exec -- sh -c 'devflow exec -- true; devflow exec -- mise x -- npm run migrate'",
    ] {
        let uses = mise_uses(command);
        assert!(!uses.is_empty(), "{command}: {uses:?}");
        assert!(
            uses.iter().all(|found| !found.through_devflow_exec),
            "{command}: {uses:?}"
        );
        let hook = crate::hooks::HookCommand::new(command, std::time::Duration::from_secs(1));
        let classified = &classify_hooks(&[hook])[0];
        assert!(
            classified.uses_mise && classified.unprotected && classified.unrouted,
            "{command}: {classified:?}"
        );
    }
    // Something beside the wrapped command in the same shell string, too.
    for command in [
        "mise exec -c 'devflow exec -- true; npm run dev'",
        "mise exec -c 'devflow exec -- true || mise run dev'",
    ] {
        let found = &mise_uses(command)[0];
        assert!(!found.through_devflow_exec, "{command}");
    }
    // An empty command after `--` wraps nothing.
    assert!(!only("mise exec -- devflow exec --").through_devflow_exec);
}

#[test]
fn only_a_single_simple_command_is_wholly_devflow_exec() {
    for command in [
        "devflow exec -- npm run dev",
        "exec devflow exec -- npm run dev",
        "PORT=3000 devflow exec -- node server.js",
        "SERVER_PORT=$PORT devflow exec -- ./mvnw spring-boot:run",
        "FOO='a b' BAR=\"c\" exec /usr/local/bin/devflow exec -- ./run",
        "devflow -C app exec --workspace feature -- npm run dev",
        "devflow --json exec -- npm run dev",
        "devflow exec -- npm run dev > dev.log 2>&1",
        "devflow exec -- npm run dev 2>&1",
        "devflow exec -- npm run dev < /dev/null",
        // Operators and substitutions inside quotes belong to the argument.
        "devflow exec -- sh -c 'npm run a; npm run b'",
        "devflow exec -- sh -c \"npm run a && npm run b\"",
        "devflow exec -- node -e 'console.log(`$(x)`)'",
        "devflow exec -- echo \"a|b\" 'c&d' e\\;f",
        "devflow exec -- npm run dev --host \"$HOST\" --port ${PORT:-3000}",
        "devflow exec -- npm run \\\n    dev",
        "devflow exec -- echo 'a\nb'",
        "  devflow   exec  --  true  ",
    ] {
        assert!(command_is_wholly_devflow_exec(command), "{command:?}");
    }
    for command in [
        // Something else runs beside it, outside the wrapper.
        "devflow exec -- true; npm run dev",
        "devflow exec -- true && npm run dev",
        "devflow exec -- true || npm run dev",
        "devflow exec -- npm run dev | tee dev.log",
        "devflow exec -- npm run dev &",
        "devflow exec -- npm run dev & npm run worker",
        "devflow exec -- npm run dev &> dev.log",
        "devflow exec -- npm run dev\nnpm run worker",
        "npm run worker; devflow exec -- npm run dev",
        "npm run worker && devflow exec -- npm run dev",
        "cd app && devflow exec -- npm run dev",
        "(devflow exec -- npm run dev)",
        "{ devflow exec -- npm run dev; }",
        "if true; then devflow exec -- npm run dev; fi",
        "devflow exec -- npm run dev `date`",
        "devflow exec -- npm run dev $(date)",
        "devflow exec -- npm run dev \"$(date)\"",
        "devflow exec -- npm run dev \"`date`\"",
        "devflow exec -- npm run dev # && mise run x",
        "echo $(devflow exec -- npm run dev)",
        // Mise inside: it applies its own [env] after devflow set the variables.
        "devflow exec -- mise exec -- npm run dev",
        "devflow exec -- mise x -- npm run dev",
        "devflow exec -- mise run dev",
        "devflow exec -- /opt/homebrew/bin/mise run dev",
        "devflow exec -- env A=1 mise exec -- npm run dev",
        "devflow exec -- sudo -E mise exec -- npm run dev",
        "devflow exec -- timeout 5 mise run dev",
        "devflow exec -- xargs mise run",
        "devflow exec -- sh -c 'mise exec -- npm run dev'",
        "devflow exec -- bash -lc 'cd app && mise run dev'",
        "devflow exec -- devflow exec -- mise run dev",
        "FOO=1 exec devflow exec -- mise run dev",
        // Not `devflow exec`, or `devflow exec` without a command.
        "echo devflow exec",
        "devflow doctor",
        "devflow exec",
        "devflow exec --",
        "devflow exec -- ",
        "devflow exec npm run dev",
        "devflow exec stray -- npm run dev",
        "devflow exec --workspace -- npm run dev",
        "mise exec -- devflow exec -- npm run dev",
        "time devflow exec -- npm run dev",
        "exec exec devflow exec -- npm run dev",
        "npm run dev",
        "mise run dev",
        // Quotes that never close.
        "devflow exec -- sh -c 'npm run dev",
        "devflow exec -- echo \"x",
        "",
    ] {
        assert!(!command_is_wholly_devflow_exec(command), "{command:?}");
    }
}

#[test]
fn what_a_probe_at_the_root_cannot_stand_for_is_named() {
    let reason = |command: &str| only(command).unfaithful;
    assert_eq!(reason("mise exec -- npm ci"), None);
    assert_eq!(reason("mise install"), None);
    assert_eq!(
        reason("cd app && mise exec -- npm ci"),
        Some(Unfaithful::ChangesDirectory)
    );
    assert_eq!(reason("mise run migrate"), Some(Unfaithful::Task));
    assert_eq!(reason("mise r migrate"), Some(Unfaithful::Task));
    assert_eq!(reason("mise tasks run migrate"), Some(Unfaithful::Task));
    assert_eq!(reason("mise build"), Some(Unfaithful::Task));
    assert_eq!(reason("mise watch build"), Some(Unfaithful::Task));
    assert_eq!(
        reason("mise -C app exec -- npm ci"),
        Some(Unfaithful::Selects)
    );
    assert_eq!(
        reason("mise -E production exec -- npm ci"),
        Some(Unfaithful::Selects)
    );
    assert_eq!(
        reason("MISE_ENV=production mise exec -- npm ci"),
        Some(Unfaithful::Assigns)
    );
    // After a cd inside a shell string, too; before it, not.
    assert_eq!(
        reason("sh -c 'cd app && mise exec -- npm ci'"),
        Some(Unfaithful::ChangesDirectory)
    );
    assert_eq!(reason("mise exec -- npm ci && cd app"), None);
    // Managing tools starts no repository command.
    assert!(!only("mise install").runs_code);
    assert!(!only("mise trust").runs_code);
    assert!(!only("mise tasks ls").runs_code);
    assert!(only("mise exec -- true").runs_code);
    assert!(only("mise run x").runs_code);
    assert!(only("mise tasks run x").runs_code);
    for phrase in [
        Unfaithful::ChangesDirectory,
        Unfaithful::Selects,
        Unfaithful::Assigns,
        Unfaithful::Task,
    ] {
        assert!(!phrase.describe().is_empty());
    }
}

/// Commands that start mise behind something the scanner does not follow word
/// by word: a wrapper with options of its own, a string handed to a shell or to
/// `eval`, a path, a nested string. Run after `devflow exec --`, each hands the
/// program mise's `[env]` over what devflow set.
const HIDDEN_MISE: [&str; 28] = [
    // Wrappers with options, inside a string for a shell (the reproductions).
    "sh -c 'env -u UNUSED mise exec -- bun run dev'",
    "sh -c 'sudo -E mise exec -- bun run dev'",
    "sh -c 'timeout 5 mise exec -- bun run dev'",
    "bash -lc 'nice -n 5 mise run dev'",
    "zsh -c 'nohup mise run dev'",
    "sh -c 'xargs mise exec -- echo'",
    "sh -c 'command mise run dev'",
    "sh -c 'exec mise run dev'",
    "sh -c 'time mise run dev'",
    "sh -c \"env -i PATH=/usr/bin mise exec -- bun run dev\"",
    // The same, not in a string.
    "env -u UNUSED mise exec -- bun run dev",
    "sudo -E mise exec -- bun run dev",
    "timeout 5 mise run dev",
    "nice -n 5 mise run dev",
    "xargs -n1 mise run",
    // Strings that run strings.
    "eval 'mise run dev'",
    "eval mise run dev",
    "sh -c \"sh -c 'env -u X mise run dev'\"",
    "sh -c 'bash -c \"timeout 5 mise run dev\"'",
    // Paths, however they are spelled.
    "sh -c '/opt/homebrew/bin/mise run dev'",
    "sh -c '\"$HOME/.local/bin/mise\" run dev'",
    "sh -c '~/.local/bin/mise run dev'",
    "node -e 'require(\"child_process\").execSync(\"mise run x\")'",
    // Quotes in the middle of the word.
    "sh -c 'm\"\"ise run dev'",
    "sh -c \"\\mise run dev\"",
    "sh -c 'echo x && (mise run dev)'",
    "sh -c 'FOO=1 BAR=2 env -u X mise exec -- bun run dev'",
    "sh -c 'x=$(mise exec -- pwd) && bun run dev'",
];

#[test]
fn mise_hidden_behind_a_wrapper_or_a_string_is_not_wrapped_by_devflow_exec() {
    for hidden in HIDDEN_MISE {
        let run = format!("devflow exec -- {hidden}");
        assert!(!command_is_wholly_devflow_exec(&run), "{run}");
        // And in a daemon that is only `PORT=… exec devflow exec -- …`.
        let run = format!("PORT=3000 exec devflow exec -- {hidden}");
        assert!(!command_is_wholly_devflow_exec(&run), "{run}");
    }
    // Anything the scanner cannot follow is not vouched for either: the
    // command is chosen at run time.
    for run in [
        "devflow exec -- $TOOL run dev",
        "devflow exec -- ${RUNNER:-bun} run dev",
        "devflow exec -- sh -c \"$START\"",
        "devflow exec -- eval \"$START\"",
        "devflow exec -- sh -c 'FOO=1 $RUNNER run dev'",
    ] {
        assert!(!command_is_wholly_devflow_exec(run), "{run}");
    }
}

#[test]
fn a_plain_command_after_devflow_exec_stays_wrapped() {
    for run in [
        "devflow exec -- bun run dev",
        "devflow exec -- env -u UNUSED bun run dev",
        "devflow exec -- sh -c 'env -u UNUSED bun run dev'",
        "devflow exec -- sh -c 'sudo -E bun run dev'",
        "devflow exec -- timeout 5 bun test",
        "devflow exec -- nice -n 5 bun run dev",
        "devflow exec -- sh -c 'nohup bun run dev'",
        "devflow exec -- sh -c \"echo $HOME && bun run dev\"",
        "devflow exec -- bun run dev --host ${HOST:-0.0.0.0}",
        // Words that only start with, end with or contain "mise".
        "devflow exec -- npx mise-tools",
        "devflow exec -- node scripts/mise-sync.js",
        "devflow exec -- ./mise-wrapper.sh",
        "devflow exec -- cargo run --bin promise",
        "devflow exec -- env MISE_ENV=ci bun run dev",
        "devflow exec -- bun run dev --config mise.toml",
        "devflow exec -- sh -c 'echo premise'",
    ] {
        assert!(command_is_wholly_devflow_exec(run), "{run}");
    }
}

#[test]
fn a_hook_that_hides_mise_behind_a_wrapper_is_never_protected() {
    let hook = |run: &str| crate::hooks::HookCommand::new(run, std::time::Duration::from_secs(1));
    for hidden in HIDDEN_MISE {
        // `mise exec --` ahead of a `devflow exec` whose command starts another
        // mise: the outer use is not protected, the hook is one the gate checks.
        for run in [
            format!("mise exec -- devflow exec -- {hidden}"),
            format!(
                "mise x -c 'devflow exec -- {}'",
                hidden.replace('\'', "'\\''")
            ),
        ] {
            let uses = mise_uses(&run);
            assert!(!uses.is_empty(), "{run}");
            assert!(
                uses.iter().all(|found| !found.through_devflow_exec),
                "{run}: {uses:?}"
            );
            let classified = &classify_hooks(&[hook(&run)])[0];
            assert!(
                classified.uses_mise && classified.unprotected && classified.unrouted,
                "{run}: {classified:?}"
            );
        }
        // Without an outer mise, the one behind `devflow exec` still counts.
        let run = format!("devflow exec -- {hidden}");
        let classified = &classify_hooks(&[hook(&run)])[0];
        assert!(
            classified.uses_mise && classified.unprotected && classified.unrouted,
            "{run}: {classified:?}"
        );
    }
    // The wrapped command alone, with options of its own, is protected.
    for run in [
        "mise exec -- devflow exec -- bun run dev",
        "mise exec -- devflow exec -- env -u UNUSED bun run dev",
        "mise exec -- devflow exec -- sh -c 'timeout 5 bun test'",
        "mise exec -- sh -c 'devflow exec -- nice -n 5 bun test'",
    ] {
        let found = only(run);
        assert!(found.through_devflow_exec, "{run}");
        let classified = &classify_hooks(&[hook(run)])[0];
        assert!(
            classified.uses_mise && !classified.unprotected,
            "{run}: {classified:?}"
        );
    }
}

#[test]
fn a_wrapper_with_options_does_not_hide_a_hooks_own_use_of_mise() {
    for (command, sub) in [
        ("sudo -E mise exec -- npm ci", "exec"),
        ("env -u UNUSED mise run migrate", "run"),
        ("timeout 5 mise install", "install"),
        ("nice -n 5 mise exec -- npm ci", "exec"),
        ("nohup mise run dev", "run"),
        ("time -p mise run dev", "run"),
        ("command -p mise run dev", "run"),
        ("xargs -n1 mise trust", "trust"),
        ("eval 'mise run dev'", "run"),
        ("cd app && env -u X mise exec -- npm ci", "exec"),
        ("sh -c 'sudo -E mise trust'", "trust"),
    ] {
        assert_eq!(mise_subcommands(command), [sub], "{command}");
    }
    // A cd ahead of it is still seen.
    assert_eq!(
        only("cd app && env -u X mise exec -- npm ci").unfaithful,
        Some(Unfaithful::ChangesDirectory)
    );
    // Not mise.
    for command in [
        "env -u X npm ci",
        "sudo -E npm ci",
        "timeout 5 npm test",
        "nice -n 5 npm test",
        "xargs -n1 echo",
        "git commit -m 'mise: update'",
        "echo mise",
    ] {
        assert!(!command_uses_mise(command), "{command}");
    }
}

/// A protected use of mise next to a mention of mise that the scanner cannot
/// classify (a string piped to a shell, an argument, a path in a file name).
const PROTECTED_THEN_HIDDEN: [&str; 7] = [
    "mise exec -- devflow exec -- true; echo \"mise exec -- bun run migrate\" | sh",
    "mise exec -- devflow exec -- true && printf 'mise run migrate' | bash",
    "mise exec -- devflow exec -- true; xargs -n1 <<< 'mise exec -- bun run migrate'",
    "echo 'mise run migrate' | sh; mise exec -- devflow exec -- true",
    "mise exec -- devflow exec -- true; $HOME/.local/bin/mise run migrate",
    "mise exec -- devflow exec -- true; m\"i\"se run migrate",
    "mise exec -- devflow exec -- true; mise x -- devflow exec -- true; echo mise | sh",
];

#[test]
fn a_protected_use_never_vouches_for_another_mention_of_mise() {
    let hook = |run: &str| crate::hooks::HookCommand::new(run, std::time::Duration::from_secs(1));
    for run in PROTECTED_THEN_HIDDEN {
        let uses = mise_uses(run);
        assert!(
            uses.iter().any(|found| found.through_devflow_exec),
            "{run}: the first use is protected: {uses:?}"
        );
        assert!(
            uses.iter()
                .any(|found| !found.through_devflow_exec && found.runs_code),
            "{run}: the other mention is unprotected: {uses:?}"
        );
        let classified = &classify_hooks(&[hook(run)])[0];
        assert!(
            classified.uses_mise && classified.unprotected && classified.unrouted,
            "{run}: {classified:?}"
        );
    }
    // The protected uses alone stay protected, and a sibling that does not
    // mention mise as a word does not change that.
    for run in [
        "mise exec -- devflow exec -- true",
        "mise exec -- devflow exec -- true; echo done",
        "mise exec -- devflow exec -- true; mise x -- devflow exec -- bun test",
        "cd app && mise exec -- devflow exec -- npm ci; git commit -m 'update mise.toml'",
        "mise exec -- devflow exec -- true; ./mise-wrapper.sh",
    ] {
        let classified = &classify_hooks(&[hook(run)])[0];
        assert!(
            classified.uses_mise && !classified.unprotected,
            "{run}: {classified:?}"
        );
    }
}

#[test]
fn a_daemon_run_with_a_protected_and_an_unprotected_use_is_not_wrapped() {
    // The daemon rule is stricter than the hook rule: nothing after `devflow
    // exec --` may mention mise, so a protected use inside the string vouches
    // for nothing.
    for run in [
        "devflow exec -- sh -c 'mise exec -- devflow exec -- true; echo \"mise exec -- bun run migrate\" | sh'",
        "devflow exec -- sh -c 'mise exec -- devflow exec -- true; mise run migrate'",
        "devflow exec -- bash -c 'devflow exec -- true; echo mise | sh'",
        "mise exec -- devflow exec -- true; devflow exec -- bun run dev",
        "mise exec -- devflow exec -- true && echo \"mise exec -- bun run dev\" | sh",
    ] {
        assert!(!command_is_wholly_devflow_exec(run), "{run}");
    }
    assert!(command_is_wholly_devflow_exec(
        "devflow exec -- sh -c 'bun run migrate && bun run dev'"
    ));
}
