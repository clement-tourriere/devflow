//! Regression tests for the code-review findings about what the facts say:
//! credential-free process commands, install commands that can run,
//! workspace membership and mise wiring.
use super::*;

fn scan_files_here(files: &[(&str, &str)]) -> Facts {
    scan_dir(&fixture(files))
}

// -- 5. credentials in Procfile commands ------------------------------------------------

#[test]
fn finding_5_procfile_commands_are_never_copied_into_the_facts() {
    let repo = fixture(&[
        (
            "Procfile",
            concat!(
                "web: env DATABASE_URL=postgresql://alice:PROCFILE_SECRET@db.internal/app python app.py\n", // kingfisher:ignore
                "worker: PGPASSWORD=PROCFILE_PASSWORD bundle exec sidekiq\n", // kingfisher:ignore
            ),
        ),
        ("package.json", "{}"),
    ]);
    let facts = scan_dir(&repo);

    let web = process(&facts, "web", ProcessSource::Procfile).expect("web");
    // Not even a masked form of the command is kept (see round 2, finding 4).
    assert_eq!(web.display.as_deref(), Some("Procfile: web"));
    assert_eq!(
        web.launch,
        Some(ProcessLaunch::Procfile {
            file: "Procfile".into(),
            process: "web".into()
        })
    );
    assert_eq!(web.role, ProcessRole::Serve);
    assert_eq!(web.evidence, vec!["Procfile:web"]);

    let worker = process(&facts, "worker", ProcessSource::Procfile).expect("worker");
    assert_eq!(worker.display.as_deref(), Some("Procfile: worker"));
    assert_eq!(worker.role, ProcessRole::Worker);

    // Neither the serialized facts nor their Debug output carry the secrets.
    for text in [json(&facts), format!("{facts:?}")] {
        assert!(!text.contains("PROCFILE_SECRET"), "{text}");
        assert!(!text.contains("PROCFILE_PASSWORD"), "{text}");
    }
    // The pointer is serialized as data, not as a command.
    let value = serde_json::to_value(&facts).expect("value");
    let launch = &value["processes"][0]["launch"];
    assert_eq!(launch["kind"], "procfile");
    assert_eq!(launch["process"], "web");
}

// -- 9. frozen installs need a lockfile ---------------------------------------------------

#[test]
fn finding_9_frozen_flags_are_only_used_with_the_tools_own_lockfile() {
    struct Case {
        files: Vec<(&'static str, &'static str)>,
        command: &'static str,
        lockfile: Option<&'static str>,
    }
    let cases = [
        Case {
            files: vec![("package.json", r#"{"packageManager":"pnpm@9.1.0"}"#)],
            command: "pnpm install --prefer-offline",
            lockfile: None,
        },
        Case {
            files: vec![
                ("package.json", r#"{"packageManager":"pnpm@9.1.0"}"#),
                ("pnpm-lock.yaml", ""),
            ],
            command: "pnpm install --frozen-lockfile --prefer-offline",
            lockfile: Some("pnpm-lock.yaml"),
        },
        Case {
            // Another tool's lockfile cannot satisfy a frozen pnpm install.
            files: vec![
                ("package.json", r#"{"packageManager":"pnpm@9.1.0"}"#),
                ("package-lock.json", "{}"),
            ],
            command: "pnpm install --prefer-offline",
            lockfile: None,
        },
        Case {
            files: vec![
                ("package.json", "{}"),
                ("pnpm-workspace.yaml", "packages: ['apps/*']\n"),
            ],
            command: "pnpm install --prefer-offline",
            lockfile: None,
        },
        Case {
            files: vec![
                ("package.json", r#"{"packageManager":"bun@1.1.0"}"#),
                ("bun.lockb", ""),
            ],
            command: "bun install --frozen-lockfile",
            lockfile: Some("bun.lockb"),
        },
        Case {
            files: vec![("package.json", r#"{"packageManager":"bun@1.1.0"}"#)],
            command: "bun install",
            lockfile: None,
        },
        Case {
            files: vec![("package.json", r#"{"packageManager":"yarn@4.2.0"}"#)],
            command: "yarn install",
            lockfile: None,
        },
        Case {
            files: vec![
                ("package.json", r#"{"packageManager":"yarn@4.2.0"}"#),
                ("yarn.lock", "__metadata:\n"),
            ],
            command: "yarn install --immutable",
            lockfile: Some("yarn.lock"),
        },
        Case {
            files: vec![("package.json", r#"{"packageManager":"yarn@1.22.0"}"#)],
            command: "yarn install",
            lockfile: None,
        },
    ];
    for case in cases {
        let facts = scan_files_here(&case.files);
        let manager = &facts.js[0].package_manager;
        assert_eq!(
            facts.js_install_command(),
            Some(case.command),
            "{:?}",
            case.files
        );
        assert_eq!(
            manager.lockfile.as_deref(),
            case.lockfile,
            "{:?}",
            case.files
        );
    }
}

#[test]
fn finding_9_a_nested_project_needs_its_own_lockfile() {
    let facts = scan_files_here(&[
        ("package.json", r#"{"packageManager":"pnpm@9.1.0"}"#),
        ("pnpm-lock.yaml", ""),
        ("services/site/package.json", "{}"),
        ("services/blog/package.json", "{}"),
        ("services/blog/pnpm-lock.yaml", ""),
    ]);

    let commands: std::collections::BTreeMap<&str, &str> =
        facts.js_install_commands().into_iter().collect();
    assert_eq!(
        commands["."],
        "pnpm install --frozen-lockfile --prefer-offline"
    );
    // Inherits pnpm from the root, but the root's lockfile is not its own.
    assert_eq!(commands["services/site"], "pnpm install --prefer-offline");
    assert_eq!(
        commands["services/blog"],
        "pnpm install --frozen-lockfile --prefer-offline"
    );
}

#[test]
fn finding_9_uv_is_only_frozen_with_a_lock() {
    let without = scan_files_here(&[("pyproject.toml", "[tool.uv]\n"), ("uv.toml", "")]);
    assert_eq!(without.python_install_command(), Some("uv sync"));
    assert_eq!(without.python[0].lockfile, None);

    let with = scan_files_here(&[
        ("pyproject.toml", "[tool.uv]\n"),
        ("uv.lock", "version = 1\n"),
    ]);
    assert_eq!(with.python_install_command(), Some("uv sync --frozen"));
    assert_eq!(with.python[0].lockfile.as_deref(), Some("uv.lock"));
}

// -- 10. workspace declarations per package manager ----------------------------------------

#[test]
fn finding_10_pnpm_members_come_from_pnpm_workspace_yaml_only() {
    let facts = scan_files_here(&[
        (
            "package.json",
            r#"{"packageManager":"pnpm@9.1.0","workspaces":["apps/*"]}"#,
        ),
        ("pnpm-workspace.yaml", "packages:\n  - packages/*\n"),
        ("apps/a/package.json", "{}"),
        ("packages/b/package.json", "{}"),
    ]);

    let root = &facts.js[0];
    let members: Vec<&str> = root
        .packages
        .iter()
        .filter(|p| p.workspace_member)
        .map(|p| p.path.as_str())
        .collect();
    assert_eq!(members, vec!["packages/b"]);
    let workspaces = root.workspaces.as_ref().expect("workspaces");
    assert_eq!(workspaces.source, WorkspaceSource::PnpmWorkspaceYaml);
    assert_eq!(workspaces.patterns, vec!["packages/*"]);
    assert_eq!(workspaces.evidence, vec!["pnpm-workspace.yaml:packages"]);
    // `apps/a` is not a pnpm workspace member: a project of its own.
    assert!(facts.js.iter().any(|p| p.path == "apps/a"));
    assert!(has_warning(&facts, "pnpm reads pnpm-workspace.yaml"));
}

#[test]
fn finding_10_npm_yarn_and_bun_members_come_from_package_json_only() {
    for (pin, lock) in [
        (r#""packageManager":"npm@10.0.0""#, "package-lock.json"),
        (r#""packageManager":"yarn@1.22.0""#, "yarn.lock"),
        (r#""packageManager":"bun@1.1.0""#, "bun.lock"),
    ] {
        let manifest = format!(r#"{{{pin},"workspaces":["apps/*"]}}"#);
        let facts = scan_files_here(&[
            ("package.json", &manifest),
            (lock, ""),
            ("pnpm-workspace.yaml", "packages:\n  - packages/*\n"),
            ("apps/a/package.json", "{}"),
            ("packages/b/package.json", "{}"),
        ]);

        let root = &facts.js[0];
        let members: Vec<&str> = root
            .packages
            .iter()
            .filter(|p| p.workspace_member)
            .map(|p| p.path.as_str())
            .collect();
        assert_eq!(members, vec!["apps/a"], "{pin}");
        let workspaces = root.workspaces.as_ref().expect("workspaces");
        assert_eq!(workspaces.source, WorkspaceSource::PackageJson, "{pin}");
        assert_eq!(workspaces.patterns, vec!["apps/*"], "{pin}");
        assert!(
            facts
                .warnings
                .iter()
                .any(|w| w.path == "pnpm-workspace.yaml" && w.message.starts_with("ignored")),
            "{pin}"
        );
        // `packages/b` is not a member of this workspace: a project of its own.
        assert!(facts.js.iter().any(|p| p.path == "packages/b"), "{pin}");
    }
}

// -- 11. mise wiring ----------------------------------------------------------------------

#[test]
fn finding_11_only_workspace_anchored_env_files_count_as_wired() {
    let wired =
        |file: &str| scan_files_here(&[("mise.toml", &format!("[env]\n_.file = {file}\n"))]).wiring;

    for path in [
        r#"".devflow/env""#,
        r#""./.devflow/env""#,
        r#""{{config_root}}/.devflow/env""#,
        r#"[".env", { path = "{{config_root}}/.devflow/env", redact = true }]"#,
    ] {
        let wiring = wired(path);
        assert!(wiring.mise_loads_devflow_env, "{path}");
        assert!(wiring.mise_env_unresolved.is_empty(), "{path}");
    }

    // Another checkout's file, a home-relative or computed path.
    for path in [
        r#""/tmp/main/.devflow/env""#,
        r#""~/work/app/.devflow/env""#,
        r#""{{env.HOME}}/app/.devflow/env""#,
        r#""../main/.devflow/env""#,
    ] {
        let wiring = wired(path);
        assert!(!wiring.mise_loads_devflow_env, "{path}");
        assert_eq!(wiring.mise_env_unresolved.len(), 1, "{path}");
    }

    // A foreign path next to the real one does not take it back.
    let wiring = wired(r#"[".devflow/env", "/tmp/main/.devflow/env"]"#);
    assert!(wiring.mise_loads_devflow_env);
    assert_eq!(wiring.mise_env_unresolved, vec!["/tmp/main/.devflow/env"]);

    let unrelated = wired(r#"".env""#);
    assert!(!unrelated.mise_loads_devflow_env);
    assert!(unrelated.mise_env_unresolved.is_empty());
}

#[test]
fn finding_11_a_file_listed_after_devflow_env_wins_over_it() {
    let wired =
        |file: &str| scan_files_here(&[("mise.toml", &format!("[env]\n_.file = {file}\n"))]).wiring;
    // Last (or alone): it wins.
    for path in [
        r#"".devflow/env""#,
        r#"[".env", ".devflow/env"]"#,
        r#"[".env", { path = ".devflow/env" }]"#,
    ] {
        let wiring = wired(path);
        assert!(wiring.mise_loads_devflow_env, "{path}");
        assert!(wiring.mise_env_overridden_by.is_empty(), "{path}");
    }
    // Something after it replaces its variables.
    let wiring = wired(r#"[".devflow/env", ".env", "/tmp/main/.devflow/env"]"#);
    assert!(wiring.mise_loads_devflow_env);
    assert_eq!(
        override_paths(&wiring),
        vec![".env", "/tmp/main/.devflow/env"]
    );
    // A higher mise file counts as later, a lower one as earlier.
    let layered = |first: &str, second: &str| {
        scan_files_here(&[
            ("mise.toml", &format!("[env]\n_.file = {first}\n")),
            ("mise.local.toml", &format!("[env]\n_.file = {second}\n")),
        ])
        .wiring
    };
    let wiring = layered(r#"".devflow/env""#, r#"".env""#);
    assert_eq!(override_paths(&wiring), vec![".env"]);
    let wiring = layered(r#"".env""#, r#"".devflow/env""#);
    assert!(wiring.mise_env_overridden_by.is_empty());
    // Not loaded at all: nothing to be overridden.
    assert!(wired(r#"".env""#).mise_env_overridden_by.is_empty());
}

/// The paths of the entries that override `.devflow/env`.
fn override_paths(wiring: &WiringFacts) -> Vec<&str> {
    wiring
        .mise_env_overridden_by
        .iter()
        .map(|entry| entry.path.as_str())
        .collect()
}

/// Round 2, finding 1: the same file listed twice is applied twice. The old
/// list was deduplicated at its first position, so a trailing `.env` after
/// `.devflow/env` disappeared and the wiring looked fine.
#[test]
fn round_2_finding_1_a_repeated_entry_after_devflow_env_overrides_it() {
    let wired =
        |file: &str| scan_files_here(&[("mise.toml", &format!("[env]\n_.file = {file}\n"))]).wiring;
    let wiring = wired(r#"[".env", ".devflow/env", ".env"]"#);
    assert!(wiring.mise_loads_devflow_env);
    assert_eq!(override_paths(&wiring), vec![".env"]);
    assert_eq!(
        wiring.mise_env_overridden_by[0].describe(),
        "mise.toml ([env] _.file)"
    );
    // The list keeps every entry, repeated or not.
    let facts = scan_files_here(&[(
        "mise.toml",
        "[env]\n_.file = [\".env\", \".devflow/env\", \".env\"]\n",
    )]);
    let mise = facts.toolchain.mise.as_ref().expect("mise");
    assert_eq!(mise.env_files, vec![".env", ".devflow/env", ".env"]);
    // And the same file written twice in a row is not an override of itself.
    let wiring = wired(r#"[".env", ".devflow/env", ".devflow/env"]"#);
    assert!(wiring.mise_env_overridden_by.is_empty());
}

/// Layers are applied in mise's own order, which is not the order the files
/// were once listed in here: `.config/mise.toml` is below `mise.toml`,
/// `.mise.local.toml` above `mise.local.toml`, `mise.<env>.toml` between the
/// base files and the local ones.
#[test]
fn mise_layers_are_applied_in_mises_precedence_order() {
    let overridden = |lower: (&str, &str), higher: (&str, &str)| {
        let file = |entry: &str| format!("[env]\n_.file = \"{entry}\"\n");
        let wiring =
            scan_files_here(&[(lower.0, &file(lower.1)), (higher.0, &file(higher.1))]).wiring;
        wiring.mise_loads_devflow_env && !wiring.mise_env_overridden_by.is_empty()
    };
    // `.devflow/env` below `.env`: overridden. Above: not.
    for (low, high) in [
        (".config/mise.toml", "mise.toml"),
        ("mise.toml", ".mise.toml"),
        ("mise.toml", "mise.local.toml"),
        ("mise.local.toml", ".mise.local.toml"),
        (".config/mise/config.toml", ".config/mise.toml"),
        (".mise/config.toml", "mise/config.toml"),
        ("mise/config.toml", "mise.toml"),
        (".mise.toml", "mise.local.toml"),
        ("mise.toml", ".config/mise/config.local.toml"),
    ] {
        assert!(
            overridden((low, ".devflow/env"), (high, ".env")),
            "{high} is applied over {low}"
        );
        assert!(
            !overridden((high, ".devflow/env"), (low, ".env")),
            "{low} is applied under {high}"
        );
    }
}

/// `mise.<env>.toml` is read only when `MISE_ENV` selects it: it can override
/// `.devflow/env` but never load it, and a scan cannot tell whether it does.
#[test]
fn files_for_a_selected_mise_environment_are_reported_apart() {
    let file = |entry: &str| format!("[env]\n_.file = \"{entry}\"\n");
    let wiring = scan_files_here(&[
        ("mise.toml", &file(".devflow/env")),
        ("mise.production.toml", &file(".env.production")),
    ])
    .wiring;
    assert!(wiring.mise_loads_devflow_env);
    assert!(wiring.mise_env_overridden_by.is_empty());
    assert_eq!(wiring.mise_env_overridden_when_selected.len(), 1);
    let entry = &wiring.mise_env_overridden_when_selected[0];
    assert_eq!(
        (entry.file.as_str(), entry.env.as_deref()),
        ("mise.production.toml", Some("production"))
    );

    // Below a file that loads `.devflow/env` again, it overrides nothing.
    let wiring = scan_files_here(&[
        ("mise.toml", &file(".devflow/env")),
        ("mise.production.toml", &file(".env.production")),
        ("mise.local.toml", &file(".devflow/env")),
    ])
    .wiring;
    assert!(wiring.mise_env_overridden_by.is_empty());
    assert!(wiring.mise_env_overridden_when_selected.is_empty());

    // Loading it only for one environment is not wiring.
    let wiring = scan_files_here(&[("mise.dev.toml", &file(".devflow/env"))]).wiring;
    assert!(!wiring.mise_loads_devflow_env);
}

/// `[[env]]` tables and the order of `_.file` and `_.source` in one table.
#[test]
fn entries_keep_the_order_they_are_written_in() {
    let wired = |text: &str| scan_files_here(&[("mise.toml", text)]).wiring;
    let wiring = wired("[[env]]\n_.file = \".devflow/env\"\n\n[[env]]\n_.file = \".env\"\n");
    assert!(wiring.mise_loads_devflow_env);
    assert_eq!(override_paths(&wiring), vec![".env"]);

    let wiring = wired("[env]\n_.file = \".devflow/env\"\n_.source = \"load.sh\"\n");
    assert_eq!(override_paths(&wiring), vec!["load.sh"]);
    let wiring = wired("[env]\n_.source = \"load.sh\"\n_.file = \".devflow/env\"\n");
    assert!(wiring.mise_loads_devflow_env);
    assert!(wiring.mise_env_overridden_by.is_empty());
}

/// What mise applies over a file, for a file that need not exist yet.
#[test]
fn what_mise_applies_over_a_file_is_known_before_the_file_exists() {
    let facts = scan_files_here(&[
        (".config/mise.toml", "[env]\n_.file = \".env.low\"\n"),
        ("mise.local.toml", "[env]\n_.file = \".env\"\n"),
        ("mise.dev.toml", "[env]\n_.file = \".env.dev\"\n"),
    ]);
    let mise = facts.toolchain.mise.as_ref().expect("mise");
    // `mise.toml` does not exist: the proposal would create it.
    let (always, sometimes) = mise.entries_over("mise.toml");
    assert_eq!(always.len(), 1);
    assert_eq!(always[0].file, "mise.local.toml");
    assert_eq!(sometimes.len(), 1);
    assert_eq!(sometimes[0].file, "mise.dev.toml");
    // Nothing is above the highest file; nothing is known about a stranger.
    assert!(mise.entries_over(".mise.local.toml").0.is_empty());
    assert_eq!(mise.entries_over("package.json"), (Vec::new(), Vec::new()));
}

// -- 12. commands are argument vectors --------------------------------------------------------

#[test]
fn finding_12_script_and_task_names_are_single_arguments() {
    let repo = fixture(&[
        (
            "package.json",
            r#"{"packageManager":"npm@10.0.0","scripts":{
                "dev:web app":"x",
                "dev:$(touch pwned)":"x",
                "dev:a;b":"x",
                "dev:it's":"x",
                "dev:\u001b[31mred":"x",
                "dev":"x"
            }}"#,
        ),
        (
            "mise.toml",
            "[tasks.\"dev:web app\"]\nrun = 'x'\n[tasks.\"dev:x; touch pwned\"]\nrun = 'x'\n",
        ),
    ]);
    let facts = scan_dir(&repo);

    let script = |name: &str| {
        process(&facts, name, ProcessSource::PackageJson)
            .unwrap_or_else(|| panic!("{name} missing"))
    };
    assert_eq!(
        script("dev:web app").launch,
        Some(ProcessLaunch::Argv {
            argv: vec!["npm".into(), "run".into(), "dev:web app".into()]
        })
    );
    assert_eq!(
        script("dev:web app").display.as_deref(),
        Some("npm run 'dev:web app'")
    );
    assert_eq!(
        script("dev:$(touch pwned)").display.as_deref(),
        Some("npm run 'dev:$(touch pwned)'")
    );
    assert_eq!(
        script("dev:a;b")
            .launch
            .as_ref()
            .and_then(ProcessLaunch::shell_line)
            .as_deref(),
        Some("npm run 'dev:a;b'")
    );
    assert_eq!(
        script("dev:it's").display.as_deref(),
        Some("npm run 'dev:it'\\''s'")
    );
    assert_eq!(script("dev").display.as_deref(), Some("npm run dev"));
    // A name with a control character is not offered at all.
    assert!(
        !facts.processes.iter().any(|p| p.name.contains('\u{1b}')),
        "{:?}",
        facts.processes
    );
    assert!(has_warning(&facts, "unusable name"));

    let mise = process(&facts, "dev:x; touch pwned", ProcessSource::MiseTask).expect("task");
    assert_eq!(
        mise.launch,
        Some(ProcessLaunch::Argv {
            argv: vec!["mise".into(), "run".into(), "dev:x; touch pwned".into()]
        })
    );
    assert_eq!(
        mise.display.as_deref(),
        Some("mise run 'dev:x; touch pwned'")
    );
}

#[test]
fn finding_12_every_tool_built_process_has_an_argument_vector() {
    let repo = fixture(&[
        (
            "package.json",
            r#"{"scripts":{"dev:--prefix=/tmp":"x","dev":"x"}}"#,
        ),
        ("pitchfork.toml", "[daemons.web]\nrun = 'x'\n"),
    ]);
    let facts = scan_dir(&repo);

    // `dev:--prefix=/tmp` is a plain argument after `run`; a name that itself
    // begins with a dash is refused (see the unit tests).
    for p in &facts.processes {
        match p.source {
            ProcessSource::Pitchfork => assert!(p.launch.is_none() && p.display.is_none()),
            _ => assert!(
                matches!(p.launch, Some(ProcessLaunch::Argv { .. })),
                "{p:?}"
            ),
        }
    }
    assert!(process(&facts, "dev:--prefix=/tmp", ProcessSource::PackageJson).is_some());
}
