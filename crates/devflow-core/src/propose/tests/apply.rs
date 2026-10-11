//! Applying proposals: validation before writes, input hashes, the journal.
use super::stacks::{bun_next_prisma, initialized};
use super::*;
use crate::config::Config;
use crate::propose::apply::{Probes, apply_with};
use crate::propose::inputs_changed;

const FILES: [&str; 3] = ["devflow.toml", "mise.toml", "pitchfork.toml"];

/// Fail after `n` files were written.
fn stop_after(n: usize) -> Probes<'static> {
    Probes {
        stop_after: Some(n),
        ..Probes::default()
    }
}

fn journal(repo: &Repo) -> serde_json::Value {
    let path = repo.root().join(".git/devflow/init-apply.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("journal")).expect("journal json")
}

fn snapshot(repo: &Repo) -> Vec<Option<String>> {
    FILES
        .iter()
        .map(|file| repo.exists(file).then(|| repo.read(file)))
        .collect()
}

#[test]
fn the_plan_survives_a_json_round_trip() {
    let repo = bun_next_prisma();
    let plan = repo.plan();
    let text = serde_json::to_string_pretty(&plan).unwrap();
    let back: Proposal = serde_json::from_str(&text).unwrap();
    assert_eq!(back, plan);
    assert_eq!(back.digest(), plan.digest());
    // The same plan applies the same files whichever copy is used.
    let accepted = defaults(&plan);
    apply(repo.root(), &back, &accepted).unwrap();
    assert!(repo.exists("devflow.toml"));
}

#[test]
fn dependencies_and_conflicts_are_checked_before_the_first_write() {
    let repo = bun_next_prisma();
    let plan = repo.plan();

    let error = apply(repo.root(), &plan, &["hook:setup:migrate".into()])
        .unwrap_err()
        .to_string();
    assert!(error.contains("nothing was written"), "{error}");
    assert!(
        error.contains("\"hook:setup:migrate\" needs \"service:db\""),
        "{error}"
    );
    assert!(error.contains("\"project:init\""), "{error}");

    let error = apply(repo.root(), &plan, &["nope".into()])
        .unwrap_err()
        .to_string();
    assert!(error.contains("no item \"nope\""), "{error}");

    // Items that exclude each other cannot both be applied.
    let mut exclusive = plan.clone();
    for item in &mut exclusive.items {
        if item.id == "daemon:web" {
            item.conflicts = vec!["daemon:worker".into()];
        }
    }
    let error = apply(
        repo.root(),
        &exclusive,
        &["daemon:web".into(), "daemon:worker".into()],
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("conflicts with \"daemon:worker\""),
        "{error}"
    );

    assert_eq!(
        snapshot(&repo),
        vec![None, Some(repo.read("mise.toml")), None]
    );
    assert!(!repo.root().join(".git/devflow/init-apply.json").exists());
}

#[test]
fn a_plan_from_elsewhere_is_refused_when_it_is_malformed() {
    let repo = bun_next_prisma();
    let plan = repo.plan();

    let mut newer = plan.clone();
    newer.proposal_version = 99;
    let error = apply(repo.root(), &newer, &[]).unwrap_err().to_string();
    assert!(error.contains("proposal_version 99"), "{error}");

    let mut other_schema = plan.clone();
    other_schema.schema_version = 2;
    assert!(apply(repo.root(), &other_schema, &[]).is_err());

    // A patch may only name the files devflow init edits.
    let mut escaping = plan.clone();
    escaping.items[0].patch.file = "../../.bashrc".into();
    let error = apply(repo.root(), &escaping, &[]).unwrap_err().to_string();
    assert!(error.contains("does not edit"), "{error}");

    // A dependency on an item that is not in the plan.
    let mut dangling = plan.clone();
    dangling.items[1].depends_on.push("ghost".into());
    assert!(
        apply(repo.root(), &dangling, &[])
            .unwrap_err()
            .to_string()
            .contains("ghost")
    );

    // Unknown fields are not silently ignored.
    let mut value = serde_json::to_value(&plan).unwrap();
    value["items"][0]["surprise"] = serde_json::json!(true);
    assert!(serde_json::from_value::<Proposal>(value).is_err());
}

#[test]
fn a_hand_edited_patch_cannot_overwrite_what_is_there() {
    let repo = initialized();
    let mut plan = repo.plan();
    let wiring = plan
        .items
        .iter_mut()
        .find(|i| i.id == "mise:env-file")
        .unwrap();
    // Someone removes the conflict and adds the operation anyway.
    wiring.conflicts.clear();
    wiring.patch.ops = vec![PatchOp::Set {
        path: vec!["env".into(), "_".into(), "file".into()],
        value: serde_json::json!(".devflow/env"),
        comment: None,
    }];
    let before = snapshot(&repo);
    let error = apply(repo.root(), &plan, &["mise:env-file".into()])
        .unwrap_err()
        .to_string();
    assert!(error.contains("conflicts with the plan"), "{error}");
    assert!(error.contains("env._.file"), "{error}");
    assert_eq!(snapshot(&repo), before);
}

#[test]
fn a_result_that_is_not_a_valid_configuration_is_refused_before_writing() {
    let repo = Repo::new(&[("README.md", "x\n")]);
    let mut plan = repo.plan();
    plan.items.push(Item {
        id: "copy:node_modules".into(),
        kind: ItemKind::Copy,
        title: "copy".into(),
        value: serde_json::json!({}),
        confidence: Confidence::High,
        evidence: vec![],
        risks: vec![],
        depends_on: vec![], // the missing dependency: no [project] would be written
        conflicts: vec![],
        patch: FilePatch {
            file: "devflow.toml".into(),
            ops: vec![PatchOp::Append {
                path: vec!["worktree".into(), "copy".into()],
                values: vec![serde_json::json!("node_modules")],
                front: false,
            }],
        },
    });
    let error = format!(
        "{:#}",
        apply(repo.root(), &plan, &["copy:node_modules".into()]).unwrap_err()
    );
    assert!(
        error.contains("would not be a valid configuration"),
        "{error}"
    );
    assert!(!repo.exists("devflow.toml"));
}

#[test]
fn a_changed_input_refuses_the_whole_plan_and_names_it() {
    let accepted = |plan: &Proposal| defaults(plan);

    // A configuration file that existed changed.
    let repo = bun_next_prisma();
    let plan = repo.plan();
    repo.write("mise.toml", "[tools]\nnode = \"20\"\n");
    let error = apply(repo.root(), &plan, &accepted(&plan))
        .unwrap_err()
        .to_string();
    assert!(error.contains("changed since the plan was made"), "{error}");
    assert!(error.contains("mise.toml"), "{error}");
    assert!(error.contains("nothing was written"), "{error}");
    assert!(!repo.exists("devflow.toml") && !repo.exists("pitchfork.toml"));

    // A file the plan saw as absent appeared.
    let repo = bun_next_prisma();
    let plan = repo.plan();
    repo.write("pitchfork.toml", "[daemons.api]\nrun = \"x\"\n");
    repo.write("devflow.local.toml", "[hooks]\nsetup = []\n");
    let error = apply(repo.root(), &plan, &accepted(&plan))
        .unwrap_err()
        .to_string();
    assert!(error.contains("pitchfork.toml (created)"), "{error}");
    assert!(error.contains("devflow.local.toml (created)"), "{error}");
    assert!(!repo.exists("devflow.toml"));

    // The detection inputs changed (a new script, a new lockfile, a new dependency).
    let repo = bun_next_prisma();
    let plan = repo.plan();
    repo.write(
        "package.json",
        r#"{"name":"shop","scripts":{"dev":"next dev","test":"vitest"},"dependencies":{"next":"15.1.0","@prisma/client":"6.1.0"},"devDependencies":{"prisma":"6.1.0"}}"#,
    );
    let error = apply(repo.root(), &plan, &accepted(&plan))
        .unwrap_err()
        .to_string();
    assert!(error.contains("detection reads"), "{error}");
    assert!(!repo.exists("devflow.toml"));
}

#[test]
fn a_plan_that_is_out_of_date_is_refused_with_a_typed_error_and_a_crash_is_not_one() {
    use crate::error::StalePlan;
    let is_stale = |error: &anyhow::Error| error.downcast_ref::<StalePlan>().is_some();

    // An input changed before the first write.
    let repo = bun_next_prisma();
    let plan = repo.plan();
    let accepted = defaults(&plan);
    repo.write("mise.toml", "[tools]\nnode = \"20\"\n");
    assert!(is_stale(&apply(repo.root(), &plan, &accepted).unwrap_err()));

    // A crash after one file is not stale, and the same plan resumes.
    let repo = bun_next_prisma();
    let plan = repo.plan();
    let crashed =
        crate::propose::apply_interrupted_after(repo.root(), &plan, &accepted, 1).unwrap_err();
    assert!(!is_stale(&crashed), "{crashed:#}");
    assert!(apply(repo.root(), &plan, &accepted).unwrap().resumed);

    // A file the interrupted apply still had to write was edited meanwhile.
    let repo = bun_next_prisma();
    let plan = repo.plan();
    crate::propose::apply_interrupted_after(repo.root(), &plan, &accepted, 1).unwrap_err();
    repo.write("mise.toml", "[tools]\nnode = \"20\"\n");
    let error = apply(repo.root(), &plan, &accepted).unwrap_err();
    assert!(is_stale(&error), "{error:#}");
}

#[test]
fn an_interrupted_apply_resumes_and_a_finished_one_is_a_no_op() {
    // The result of an uninterrupted apply, for comparison.
    let reference = bun_next_prisma();
    let plan = reference.plan();
    let accepted = defaults(&plan);
    apply(reference.root(), &plan, &accepted).unwrap();
    let expected = snapshot(&reference);

    let repo = bun_next_prisma();
    let plan = repo.plan();
    let before = snapshot(&repo);
    let error = apply_with(repo.root(), &plan, &accepted, &stop_after(1))
        .unwrap_err()
        .to_string();
    assert!(error.contains("interrupted"), "{error}");

    // The first file was written, the others were not, and the journal says so.
    let after = snapshot(&repo);
    assert!(after[0].is_some(), "devflow.toml written first");
    assert_eq!(after[1], before[1], "mise.toml untouched");
    assert_eq!(after[2], None, "pitchfork.toml not written");
    let journal = journal(&repo);
    assert_eq!(journal["state"], "applying");
    assert_eq!(journal["plan_digest"], plan.digest());
    let files = journal["files"].as_array().unwrap();
    assert_eq!(files.len(), 3);
    assert_eq!(files[0]["file"], "devflow.toml");
    assert_eq!(files[0]["written"], true);
    assert_eq!(files[1]["written"], false);
    assert_eq!(files[0]["before"], serde_json::Value::Null);
    assert_eq!(files[0]["after"], sha256_hex(after[0].as_ref().unwrap()));
    assert_eq!(files[1]["before"], sha256_hex(before[1].as_ref().unwrap()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(repo.root().join(".git/devflow/init-apply.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // A retry recognizes the first file and finishes the rest.
    let report = apply(repo.root(), &plan, &accepted).unwrap();
    assert!(report.resumed && !report.already_applied);
    let statuses: Vec<_> = report
        .files
        .iter()
        .map(|f| (f.file.as_str(), f.status))
        .collect();
    assert_eq!(
        statuses,
        vec![
            ("devflow.toml", FileStatus::AlreadyApplied),
            ("mise.toml", FileStatus::Modified),
            ("pitchfork.toml", FileStatus::Created),
        ]
    );
    assert_eq!(
        snapshot(&repo),
        expected,
        "same files as an uninterrupted apply"
    );
    assert_eq!(journal_state(&repo), "complete");

    // Applying again changes nothing.
    let again = apply(repo.root(), &plan, &accepted).unwrap();
    assert!(again.already_applied && !again.resumed);
    assert!(
        again
            .files
            .iter()
            .all(|f| f.status == FileStatus::AlreadyApplied)
    );
    assert_eq!(snapshot(&repo), expected);

    // Once the files are edited, the finished plan no longer applies.
    repo.write("pitchfork.toml", "[daemons.mine]\nrun = \"x\"\n");
    let error = apply(repo.root(), &plan, &accepted)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("already applied") && error.contains("pitchfork.toml"),
        "{error}"
    );
}

#[test]
fn a_retry_still_checks_the_facts_the_apply_does_not_change() {
    // Interrupted after one file, and after two: the files already written
    // change what detection reads back (wiring, mise, pitchfork), and a retry
    // must still finish when nothing else did.
    for written in [1, 2] {
        let repo = bun_next_prisma();
        let plan = repo.plan();
        let accepted = defaults(&plan);
        apply_with(repo.root(), &plan, &accepted, &stop_after(written)).unwrap_err();
        let report = apply(repo.root(), &plan, &accepted).unwrap();
        assert!(report.resumed, "after {written} file(s)");
        Config::load(repo.root()).unwrap();
    }

    // Also when the apply creates mise.toml and pitchfork.toml from nothing,
    // with every item accepted, whichever file it stops after.
    for written in [1, 2] {
        let repo = bun_next_prisma();
        std::fs::remove_file(repo.root().join("mise.toml")).unwrap();
        let plan = repo.plan();
        let accepted = all(&plan);
        apply_with(repo.root(), &plan, &accepted, &stop_after(written)).unwrap_err();
        let report = apply(repo.root(), &plan, &accepted).unwrap();
        assert!(report.resumed, "after {written} file(s)");
        assert!(repo.exists("mise.toml") && repo.exists("pitchfork.toml"));
    }

    // The reported case: the first file is written, then the package's `dev`
    // script goes away. The daemon the plan still holds would run a script
    // that is not there; the retry refuses instead of finishing the plan.
    type Change = dyn Fn(&Repo);
    let removals: [(&str, &Change); 3] = [
        ("the dev script", &|repo| {
            repo.write(
                "package.json",
                r#"{"name":"shop","private":true,"scripts":{"build":"next build","worker":"bun run src/worker.ts"},"dependencies":{"next":"15.1.0","react":"19.0.0","@prisma/client":"6.1.0","ioredis":"5.4.1"},"devDependencies":{"prisma":"6.1.0"}}"#,
            );
        }),
        ("the lockfile", &|repo| {
            std::fs::remove_file(repo.root().join("bun.lock")).unwrap();
            repo.write("package-lock.json", "{}\n");
        }),
        ("the compose file", &|repo| {
            repo.write("compose.yaml", "services:\n  cache:\n    image: redis:7\n");
        }),
    ];
    for written in [1, 2] {
        for (what, change) in &removals {
            let repo = bun_next_prisma();
            let plan = repo.plan();
            let accepted = defaults(&plan);
            apply_with(repo.root(), &plan, &accepted, &stop_after(written)).unwrap_err();
            let after_crash = snapshot(&repo);
            change(&repo);

            let error = apply(repo.root(), &plan, &accepted)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("unfinished") && error.contains("repository changed"),
                "{what}, {written} written: {error}"
            );
            assert_eq!(
                snapshot(&repo),
                after_crash,
                "{what}: nothing more was written"
            );
            assert_eq!(journal_state(&repo), "applying");
        }
    }

    // A finished apply stays a no-op whatever happens to the repository after.
    let repo = bun_next_prisma();
    let plan = repo.plan();
    let accepted = defaults(&plan);
    apply(repo.root(), &plan, &accepted).unwrap();
    repo.write("package.json", r#"{"name":"shop"}"#);
    assert!(
        apply(repo.root(), &plan, &accepted)
            .unwrap()
            .already_applied
    );
}

#[test]
fn a_file_edited_after_the_plan_was_read_is_never_overwritten() {
    // Edited between the apply's own checks and the replacement: the moment
    // the checks done up front cannot see.
    let edited = "[tools]\nbun = \"1.2\"\n\n[env]\nMY_TOKEN_FILE = \"~/.secrets\"\n";
    let repo = bun_next_prisma();
    let plan = repo.plan();
    let accepted = defaults(&plan);
    let before = snapshot(&repo);
    let probe = |file: &str| {
        if file == "mise.toml" {
            repo.write("mise.toml", edited);
        }
    };
    let error = apply_with(
        repo.root(),
        &plan,
        &accepted,
        &Probes {
            before_replace: Some(&probe),
            ..Probes::default()
        },
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("mise.toml was edited since the plan was made"),
        "{error}"
    );
    assert!(error.contains("Make a new plan"), "{error}");
    assert!(
        error.contains("devflow.toml") && error.contains("init-apply.json"),
        "the first file was already written, and the journal is named: {error}"
    );
    assert_eq!(repo.read("mise.toml"), edited, "the edit survived");
    assert!(repo.exists("devflow.toml"), "written before the edit");
    assert!(!repo.exists("pitchfork.toml"), "nothing after the refusal");
    assert_eq!(snapshot(&repo)[1].as_deref(), Some(edited));
    assert_ne!(before[1].as_deref(), Some(edited));

    // A retry of the same plan is refused too (the file is neither state).
    assert!(apply(repo.root(), &plan, &accepted).is_err());
    assert_eq!(repo.read("mise.toml"), edited);
    // Planning again works on what is there now, and keeps the edit.
    let again = repo.plan();
    apply(repo.root(), &again, &defaults(&again)).unwrap();
    let mise = repo.read("mise.toml");
    assert!(
        mise.contains("MY_TOKEN_FILE") && mise.contains("_.file = \".devflow/env\""),
        "{mise}"
    );

    // A file the plan saw as absent, created in the meantime, is not replaced
    // either, whichever file it is.
    for target in ["devflow.toml", "pitchfork.toml"] {
        let repo = bun_next_prisma();
        let plan = repo.plan();
        let accepted = defaults(&plan);
        let theirs = "# mine, written a moment ago\n";
        let probe = |file: &str| {
            if file == target {
                repo.write(target, theirs);
            }
        };
        let error = apply_with(
            repo.root(),
            &plan,
            &accepted,
            &Probes {
                before_replace: Some(&probe),
                ..Probes::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains(&format!("{target} was edited since the plan was made")),
            "{target}: {error}"
        );
        assert!(error.contains("was created"), "{target}: {error}");
        assert_eq!(repo.read(target), theirs);
    }

    // The same on a retry: the file the retry is about to replace.
    let repo = bun_next_prisma();
    let plan = repo.plan();
    let accepted = defaults(&plan);
    apply_with(repo.root(), &plan, &accepted, &stop_after(1)).unwrap_err();
    let probe = |file: &str| {
        if file == "mise.toml" {
            repo.write("mise.toml", edited);
        }
    };
    let error = apply_with(
        repo.root(),
        &plan,
        &accepted,
        &Probes {
            before_replace: Some(&probe),
            ..Probes::default()
        },
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("mise.toml was edited since the plan was made"),
        "{error}"
    );
    assert_eq!(repo.read("mise.toml"), edited);
    assert!(!repo.exists("pitchfork.toml"));
}

/// Round 2, finding 5: the check right before the replacement cannot see a
/// save that lands after it. The replacement is exchanged with the file and
/// the displaced content compared, so the last moment, after the new content
/// is staged and before it is swapped in, is covered too.
#[test]
fn a_save_between_staging_and_the_replacement_is_not_lost() {
    let edited = "[tools]\nbun = \"1.2\"\n\n[env]\nMY_TOKEN_FILE = \"~/.secrets\"\n";
    let stray = |repo: &Repo| {
        std::fs::read_dir(repo.root())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp") || n.ends_with(".bak"))
            .collect::<Vec<_>>()
    };

    // A fresh apply: an edit of a file the plan saw.
    let repo = bun_next_prisma();
    let plan = repo.plan();
    let accepted = defaults(&plan);
    let probe = |file: &str| {
        if file == "mise.toml" {
            repo.write("mise.toml", edited);
        }
    };
    let error = apply_with(
        repo.root(),
        &plan,
        &accepted,
        &Probes {
            after_staging: Some(&probe),
            ..Probes::default()
        },
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("mise.toml was edited since the plan was made")
            && error.contains("Make a new plan"),
        "{error}"
    );
    assert_eq!(repo.read("mise.toml"), edited, "the edit survived");
    assert!(stray(&repo).is_empty(), "{:?}", stray(&repo));
    assert!(!repo.exists("pitchfork.toml"), "nothing after the refusal");

    // A file the plan saw as absent, created in that moment.
    for target in ["devflow.toml", "pitchfork.toml"] {
        let repo = bun_next_prisma();
        let plan = repo.plan();
        let accepted = defaults(&plan);
        let theirs = "# mine, written a moment ago\n";
        let probe = |file: &str| {
            if file == target {
                repo.write(target, theirs);
            }
        };
        let error = apply_with(
            repo.root(),
            &plan,
            &accepted,
            &Probes {
                after_staging: Some(&probe),
                ..Probes::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains(&format!("{target} was edited since the plan was made")),
            "{target}: {error}"
        );
        assert_eq!(repo.read(target), theirs, "{target}");
        assert!(stray(&repo).is_empty(), "{:?}", stray(&repo));
    }

    // The same on a retry of an interrupted apply.
    let repo = bun_next_prisma();
    let plan = repo.plan();
    let accepted = defaults(&plan);
    apply_with(repo.root(), &plan, &accepted, &stop_after(1)).unwrap_err();
    let probe = |file: &str| {
        if file == "mise.toml" {
            repo.write("mise.toml", edited);
        }
    };
    let error = apply_with(
        repo.root(),
        &plan,
        &accepted,
        &Probes {
            after_staging: Some(&probe),
            ..Probes::default()
        },
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("mise.toml was edited since the plan was made"),
        "{error}"
    );
    assert_eq!(repo.read("mise.toml"), edited);
    assert!(stray(&repo).is_empty(), "{:?}", stray(&repo));
    assert!(!repo.exists("pitchfork.toml"));
}

#[cfg(unix)]
#[test]
fn applying_keeps_the_permissions_of_the_files_it_edits() {
    use std::os::unix::fs::PermissionsExt;
    let repo = bun_next_prisma();
    let mise = repo.root().join("mise.toml");
    std::fs::set_permissions(&mise, std::fs::Permissions::from_mode(0o600)).unwrap();
    let plan = repo.plan();
    apply(repo.root(), &plan, &defaults(&plan)).unwrap();
    assert!(repo.read("mise.toml").contains("_.file"));
    let mode = |file: &str| {
        std::fs::metadata(repo.root().join(file))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(
        mode("mise.toml"),
        0o600,
        "a token file does not become 0644"
    );
    assert_eq!(
        mode("devflow.toml") & 0o022,
        0,
        "new files are not group/world writable"
    );
}

fn journal_state(repo: &Repo) -> String {
    journal(repo)["state"].as_str().unwrap().to_string()
}

#[test]
fn a_retry_stops_when_a_file_changed_after_the_crash() {
    let repo = bun_next_prisma();
    let plan = repo.plan();
    let accepted = defaults(&plan);
    apply_with(repo.root(), &plan, &accepted, &stop_after(1)).unwrap_err();

    // Somebody edits the second file before the retry.
    repo.write("mise.toml", "[tools]\nnode = \"20\"\n");
    let error = apply(repo.root(), &plan, &accepted)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("unfinished") && error.contains("mise.toml"),
        "{error}"
    );
    assert!(!repo.exists("pitchfork.toml"), "nothing more was written");
    assert_eq!(journal_state(&repo), "applying");
}

#[test]
fn a_different_selection_cannot_continue_an_unfinished_apply() {
    let repo = bun_next_prisma();
    let plan = repo.plan();
    let accepted = defaults(&plan);
    apply_with(repo.root(), &plan, &accepted, &stop_after(1)).unwrap_err();

    let error = apply(repo.root(), &plan, &["project:init".into()])
        .unwrap_err()
        .to_string();
    assert!(error.contains("devflow.toml"), "{error}");
    assert!(error.contains("did not finish"), "{error}");
    // The same selection still resumes.
    apply(repo.root(), &plan, &accepted).unwrap();
    Config::load(repo.root()).unwrap();
}

#[test]
fn a_new_plan_after_a_finished_apply_works_on_the_new_state() {
    let repo = bun_next_prisma();
    let plan = repo.plan();
    apply(repo.root(), &plan, &defaults(&plan)).unwrap();

    // Redis was branched by the first apply. A MongoDB service joins the
    // compose file afterwards: devflow does not branch it, so compose hooks
    // are proposed for it alone.
    let compose = repo.read("compose.yaml");
    repo.write(
        "compose.yaml",
        &compose.replace(
            "volumes:\n  dbdata:",
            "  mongo:\n    image: mongo:7\nvolumes:\n  dbdata:",
        ),
    );

    // What is left are the medium items, proposed against the files just written.
    let second = repo.plan();
    assert!(second.item("project:init").is_none());
    assert!(second.item("service:redis").is_none());
    assert!(
        second
            .warnings
            .iter()
            .any(|w| w.contains("`redis`") && w.contains("already configured as services.redis")),
        "{:?}",
        second.warnings
    );
    for id in [
        "service:db",
        "hook:setup:install",
        "daemon:web",
        "mise:env-file",
    ] {
        assert!(second.already_configured.contains(&id.to_string()), "{id}");
    }
    let up = item(&second, "hook:setup:compose-up");
    assert!(up.is_actionable());
    assert_eq!(up.value["run"], "docker compose up -d --wait mongo");
    apply(repo.root(), &second, &all(&second)).unwrap();
    let (config, _) = Config::load(repo.root()).unwrap();
    assert_eq!(config.hooks.switch.len(), 1);
    assert_eq!(config.hooks.teardown.len(), 1);
    assert!(repo.read("pitchfork.toml").contains("[daemons.worker]"));
    // A third plan has nothing to add.
    let third = repo.plan();
    assert!(third.items.is_empty(), "{:?}", ids(&third));
}

#[test]
fn applying_writes_configuration_and_nothing_else() {
    let repo = bun_next_prisma();
    let plan = repo.plan();
    let before: Vec<String> = list(repo.root());
    apply(repo.root(), &plan, &all(&plan)).unwrap();
    let after = list(repo.root());
    let added: Vec<&String> = after.iter().filter(|p| !before.contains(p)).collect();
    assert_eq!(
        added,
        vec!["devflow.toml", "pitchfork.toml"],
        "plus the journal under .git, which is not listed"
    );
}

#[test]
fn a_plan_can_only_touch_the_sections_init_writes_and_carries_no_secrets() {
    let repo = Repo::new(&[("README.md", "x\n")]);
    let base = repo.plan();
    let url = format!("postgres://{}:{}@db.internal/app", "prov", "hunter2");
    let cases: Vec<(&str, PatchOp, &str)> = vec![
        (
            "devflow.toml",
            PatchOp::Set {
                path: vec!["services".into(), "db".into(), "admin_url".into()],
                value: serde_json::json!(url),
                comment: None,
            },
            "credential",
        ),
        (
            "devflow.toml",
            PatchOp::Set {
                path: vec!["services".into(), "db".into(), "password".into()],
                value: serde_json::json!("x"),
                comment: None,
            },
            "secret",
        ),
        (
            "devflow.toml",
            PatchOp::Append {
                path: vec!["hooks".into(), "setup".into()],
                values: vec![serde_json::json!({"name": "x", "run": format!("curl {url}")})],
                front: false,
            },
            "credential",
        ),
        (
            "devflow.toml",
            PatchOp::Table {
                path: vec!["agent".into()],
                comment: None,
                dotted: false,
            },
            "not something devflow init writes",
        ),
        (
            "pitchfork.toml",
            PatchOp::Set {
                path: vec!["env".into(), "X".into()],
                value: serde_json::json!("1"),
                comment: None,
            },
            "not something devflow init writes",
        ),
        (
            "mise.toml",
            PatchOp::Set {
                path: vec!["tasks".into(), "x".into(), "run".into()],
                value: serde_json::json!("rm -rf ~"),
                comment: None,
            },
            "not something devflow init writes",
        ),
        (
            "mise.toml",
            PatchOp::Set {
                path: vec!["env".into(), "FOO".into()],
                value: serde_json::json!("bar"),
                comment: None,
            },
            "not something devflow init writes",
        ),
        (
            ".mise.toml",
            PatchOp::Set {
                path: vec!["env".into(), "_".into(), "source".into()],
                value: serde_json::json!("./x.sh"),
                comment: None,
            },
            "not something devflow init writes",
        ),
        // The same keys written through a table value are the same keys.
        (
            "mise.toml",
            PatchOp::Set {
                path: vec!["env".into()],
                value: serde_json::json!({"_": {"source": "./bootstrap.sh"}}),
                comment: None,
            },
            "env._.source is not something devflow init writes",
        ),
        (
            "mise.toml",
            PatchOp::Set {
                path: vec!["env".into()],
                value: serde_json::json!({"FOO": "bar"}),
                comment: None,
            },
            "env.FOO is not something devflow init writes",
        ),
        (
            "mise.toml",
            PatchOp::Set {
                path: vec!["env".into(), "_".into()],
                value: serde_json::json!({"path": ["/tmp/bin"]}),
                comment: None,
            },
            "env._.path is not something devflow init writes",
        ),
        // The one thing mise may load is this workspace's file.
        (
            "mise.toml",
            PatchOp::Set {
                path: vec!["env".into(), "_".into(), "file".into()],
                value: serde_json::json!("/tmp/other.env"),
                comment: None,
            },
            "env._.file is not something devflow init writes",
        ),
        (
            "mise.toml",
            PatchOp::Set {
                path: vec!["env".into(), "_".into()],
                value: serde_json::json!({"file": [".devflow/env", "../x"]}),
                comment: None,
            },
            "env._.file is not something devflow init writes",
        ),
        (
            "mise.toml",
            PatchOp::Append {
                path: vec!["env".into(), "_".into(), "file".into()],
                values: vec![serde_json::json!("/tmp/other.env")],
                front: false,
            },
            "env._.file is not something devflow init writes",
        ),
        // Secret-named keys inside values.
        (
            "pitchfork.toml",
            PatchOp::Set {
                path: vec!["daemons".into(), "web".into(), "env".into()],
                value: serde_json::json!({"API_TOKEN": "abc"}),
                comment: None,
            },
            "daemons.web.env.API_TOKEN names a secret",
        ),
        (
            "devflow.toml",
            PatchOp::Set {
                path: vec!["env".into()],
                value: serde_json::json!({"nested": {"db_password": "x"}}),
                comment: None,
            },
            "env.nested.db_password names a secret",
        ),
        (
            "devflow.toml",
            PatchOp::Append {
                path: vec!["hooks".into(), "setup".into()],
                values: vec![serde_json::json!({"name": "x", "run": "true", "api_key": "k"})],
                front: false,
            },
            "hooks.setup.api_key names a secret",
        ),
        // Values TOML cannot hold are refused up front, not half-way through.
        (
            "devflow.toml",
            PatchOp::Set {
                path: vec!["project".into(), "name".into()],
                value: serde_json::Value::Null,
                comment: None,
            },
            "null",
        ),
        (
            "devflow.toml",
            PatchOp::Set {
                path: vec!["worktree".into()],
                value: serde_json::json!({"copy": [1.5]}),
                comment: None,
            },
            "only integers",
        ),
    ];
    for (file, op, why) in cases {
        let mut plan = base.clone();
        plan.items[0].patch = FilePatch {
            file: file.into(),
            ops: vec![op],
        };
        let error = apply(repo.root(), &plan, &["project:init".into()])
            .unwrap_err()
            .to_string();
        assert!(error.contains(why), "{why}: {error}");
    }
    assert!(!repo.exists("devflow.toml"));
    // What init itself writes passes the same check.
    let plan = bun_next_prisma().plan();
    plan.validate().unwrap();
}

#[test]
fn two_applies_at_once_take_turns() {
    let repo = bun_next_prisma();
    let plan = repo.plan();
    let accepted = defaults(&plan);
    let (a, b) = std::thread::scope(|scope| {
        let first = scope.spawn(|| apply(repo.root(), &plan, &accepted).unwrap());
        let second = scope.spawn(|| apply(repo.root(), &plan, &accepted).unwrap());
        (first.join().unwrap(), second.join().unwrap())
    });
    // One wrote the files; the other found them finished.
    assert_eq!(
        [a.already_applied, b.already_applied]
            .iter()
            .filter(|done| **done)
            .count(),
        1,
        "{a:?} {b:?}"
    );
    Config::load(repo.root()).unwrap();
}

fn list(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        if name != ".git" {
            found.push(name);
        }
    }
    found.sort();
    found
}

#[test]
fn a_retry_notices_a_mise_layer_that_appeared_changed_or_went_away_meanwhile() {
    // mise reads more than `mise.toml`: `.config/mise/conf.d/*.toml`, a
    // `config.local.toml`, `mise.<env>.toml`. One of them showing up between
    // the interruption and the retry can change what the wiring means (a
    // later `_.file` wins), so the retry refuses, whichever file it stopped
    // after, and writes nothing more.
    type Change = dyn Fn(&Repo);
    let later_env = "[env]\n_.file = \".env\"\n";
    let appearing: [(&str, &Change); 5] = [
        ("a local config under .config/mise", &|repo| {
            repo.write(".config/mise/config.local.toml", later_env);
        }),
        ("a conf.d file", &|repo| {
            repo.write(".config/mise/conf.d/a.toml", later_env);
        }),
        ("an environment-specific file", &|repo| {
            repo.write("mise.production.toml", later_env);
        }),
        (
            "mise.local.toml, which the plan also tracks by name",
            &|repo| {
                repo.write("mise.local.toml", later_env);
            },
        ),
        ("a layer that existed and is edited", &|repo| {
            repo.write(".config/mise/conf.d/b.toml", "[tools]\njq = \"latest\"\n");
        }),
    ];
    for written in [1, 2] {
        for (what, change) in &appearing {
            let repo = bun_next_prisma();
            // One layer is there when the plan is made (and, for the last
            // case, edited afterwards).
            repo.write(".config/mise/conf.d/b.toml", "[tools]\njq = \"1.7\"\n");
            let plan = repo.plan();
            let accepted = defaults(&plan);
            assert!(
                plan.input_hashes[".config/mise/conf.d/b.toml"].is_some(),
                "every layer detection finds is hashed: {:?}",
                plan.input_hashes.keys().collect::<Vec<_>>()
            );
            assert!(plan.input_hashes["mise-layers"].is_some());
            apply_with(repo.root(), &plan, &accepted, &stop_after(written)).unwrap_err();
            let after_crash = snapshot(&repo);
            change(&repo);

            let error = apply(repo.root(), &plan, &accepted)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("unfinished") && error.contains("files changed since"),
                "{what}, {written} written: {error}"
            );
            assert_eq!(
                snapshot(&repo),
                after_crash,
                "{what}: nothing more was written"
            );
            assert_eq!(journal_state(&repo), "applying");
        }
    }

    // A layer going away is a change as well, and so is one that appears when
    // the plan saw none.
    let repo = bun_next_prisma();
    repo.write(".config/mise/conf.d/b.toml", "[tools]\njq = \"1.7\"\n");
    let plan = repo.plan();
    let accepted = defaults(&plan);
    apply_with(repo.root(), &plan, &accepted, &stop_after(1)).unwrap_err();
    std::fs::remove_file(repo.root().join(".config/mise/conf.d/b.toml")).unwrap();
    let error = apply(repo.root(), &plan, &accepted)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("unfinished") && error.contains("conf.d/b.toml (deleted)"),
        "{error}"
    );
    assert!(error.contains("the mise configuration files"), "{error}");

    // The files the apply writes itself are not changes, nor are layers that
    // stay as they were: the retry finishes, also when it creates mise.toml.
    for creates_mise in [false, true] {
        let repo = bun_next_prisma();
        if creates_mise {
            std::fs::remove_file(repo.root().join("mise.toml")).unwrap();
        }
        repo.write(".config/mise/conf.d/b.toml", "[tools]\njq = \"1.7\"\n");
        let plan = repo.plan();
        let accepted = all(&plan);
        apply_with(repo.root(), &plan, &accepted, &stop_after(1)).unwrap_err();
        let report = apply(repo.root(), &plan, &accepted).unwrap();
        assert!(report.resumed, "creates_mise: {creates_mise}");
    }

    // The preview a frontend shows before an apply sees them too.
    let repo = bun_next_prisma();
    let plan = repo.plan();
    assert!(inputs_changed(repo.root(), &plan).unwrap().is_empty());
    repo.write(".config/mise/config.local.toml", later_env);
    let changed = inputs_changed(repo.root(), &plan).unwrap();
    assert!(
        changed
            .iter()
            .any(|c| c.contains("mise configuration files")),
        "{changed:?}"
    );
}
