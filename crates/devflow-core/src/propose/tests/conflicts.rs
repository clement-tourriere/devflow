//! A conflict names the key or the entry, never what the existing file holds
//! there: a hook command with a password inline, an env file path, a project
//! name. Nothing the existing configuration says may reach a proposal.
use super::stacks::bun_next_prisma;
use super::*;

/// Stands for a credential in every position an existing file could hold one.
const MARKER: &str = "hunter2-MARKER-8c1f"; // kingfisher:ignore

/// Everything a conflict can be made of, each with the marker in a different
/// position: a hook command (two spellings), the project name, an `[env]`
/// recipe, `_.file` entries (a plain one, a foreign checkout's and one after
/// `.devflow/env`), a daemon command and the local overlay.
fn planted() -> Repo {
    let repo = bun_next_prisma();
    repo.write(
        "devflow.toml",
        &format!(
            r#"[project]
name = "shop-{MARKER}"
id = "11111111-2222-4333-8444-555555555555"

[worktree]
dir = "../{{repo}}.worktrees"
copy = [".env"]

[services.main]
engine = "postgres"
mode = "managed"
image = "postgres:17"

[hooks]
setup = [
  {{ name = "install", run = "make deps --password {MARKER}" }},
  {{ name = "prisma-generate", run = "psql postgresql://app:{MARKER}@db/app -c 'select 1'" }},
]

[env]
DIRECT_URL = "postgres://app:{MARKER}@db.internal/app"
"#
        ),
    );
    repo.write(
        "mise.toml",
        &format!(
            "[tools]\nbun = \"1.2\"\n\n[env]\n_.file = [\"/home/{MARKER}/.devflow/env\", \".devflow/env\", \"/srv/{MARKER}/.env\"]\n"
        ),
    );
    repo.write(
        "pitchfork.toml",
        &format!("[daemons.web]\nrun = \"bun run dev --token {MARKER}\"\n"),
    );
    repo.write(
        "devflow.local.toml",
        &format!("[hooks]\nsetup = [\"echo {MARKER}\"]\n"),
    );
    repo
}

/// Every string the proposal and its renderings hold.
fn documents(plan: &Proposal) -> Vec<(&'static str, String)> {
    vec![
        ("Debug", format!("{plan:?}")),
        ("JSON", serde_json::to_string_pretty(plan).unwrap()),
    ]
}

#[test]
fn a_conflict_never_repeats_what_the_existing_files_hold() {
    let repo = planted();
    let mut options = repo.options();
    options.name = Some("other".into());
    let plan = propose(repo.root(), &options).unwrap();

    // The positions really do produce conflicts, each naming its key.
    let conflicts = |id: &str| item(&plan, id).conflicts.join(" | ");
    assert!(
        conflicts("project:name").contains("project.name"),
        "{}",
        conflicts("project:name")
    );
    let install = conflicts("hook:setup:install");
    assert!(
        install.contains("hooks.setup") && install.contains("\"install\""),
        "{install}"
    );
    assert!(
        conflicts("hook:setup:codegen").contains("hooks.setup")
            || plan
                .already_configured
                .contains(&"hook:setup:codegen".to_string()),
        "{:?}",
        plan.items.iter().map(|i| &i.id).collect::<Vec<_>>()
    );
    assert!(
        conflicts("env:preset:prisma").contains("DIRECT_URL"),
        "{}",
        conflicts("env:preset:prisma")
    );
    assert!(
        conflicts("mise:env-file").contains("after .devflow/env"),
        "{}",
        conflicts("mise:env-file")
    );
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("devflow.local.toml")),
        "{:?}",
        plan.warnings
    );

    for (kind, text) in documents(&plan) {
        assert!(!text.contains(MARKER), "{kind} holds the marker:\n{text}");
        assert!(!text.contains("hunter2"), "{kind} holds part of it");
    }
}

#[test]
fn a_file_that_does_not_parse_is_described_by_position_not_by_content() {
    // The parser's own message quotes the line it chokes on.
    let invalid = format!("[daemons.web\nrun = \"x\"\npassword = \"{MARKER}\" oops\n");
    for (file, other) in [
        ("pitchfork.toml", "mise.toml"),
        ("mise.toml", "pitchfork.toml"),
    ] {
        let repo = bun_next_prisma();
        repo.write(file, &invalid);
        let _ = other;
        let plan = repo.plan();
        let warning = plan
            .warnings
            .iter()
            .find(|w| w.contains(file) && w.contains("not valid TOML"))
            .unwrap_or_else(|| panic!("{file}: {:?}", plan.warnings));
        assert!(warning.contains("line 1"), "{warning}");
        for (kind, text) in documents(&plan) {
            assert!(!text.contains(MARKER), "{file}: {kind}:\n{text}");
        }
    }
}

#[test]
fn applying_a_plan_whose_operation_meets_a_conflict_does_not_repeat_the_file_either() {
    let repo = planted();
    let mut plan = repo.plan();
    // Someone drops the conflicts from the blocked items and adds the operations.
    for item in &mut plan.items {
        match item.id.as_str() {
            "hook:setup:install" => {
                item.conflicts.clear();
                item.patch.ops = vec![PatchOp::Append {
                    path: vec!["hooks".into(), "setup".into()],
                    values: vec![serde_json::json!({"name": "install", "run": "bun install"})],
                    front: false,
                }];
            }
            "env:preset:prisma" | "mise:env-file" => {
                item.conflicts.clear();
                item.patch.ops = vec![PatchOp::Set {
                    path: if item.id == "mise:env-file" {
                        vec!["env".into(), "_".into(), "file".into()]
                    } else {
                        vec!["env".into(), "DIRECT_URL".into()]
                    },
                    value: serde_json::json!(if item.id == "mise:env-file" {
                        ".devflow/env"
                    } else {
                        "${DATABASE_URL}"
                    }),
                    comment: None,
                }];
            }
            _ => {}
        }
    }
    for id in ["hook:setup:install", "env:preset:prisma", "mise:env-file"] {
        let error = format!(
            "{:#}",
            apply(repo.root(), &plan, &[id.to_string()]).unwrap_err()
        );
        assert!(error.contains("conflicts with the plan"), "{id}: {error}");
        assert!(!error.contains(MARKER), "{id}: {error}");
        assert!(!error.contains("hunter2"), "{id}: {error}");
    }
}
