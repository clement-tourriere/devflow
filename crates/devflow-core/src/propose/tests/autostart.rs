//! The `[pitchfork] autostart` proposal.
use super::*;
use crate::config::Config;

const DAEMON: &str = "[daemons.web]\nrun = \"bun run dev\"\nmise = true\n";

#[test]
fn a_pitchfork_file_gets_an_autostart_proposal_that_is_off_by_default() {
    let repo = Repo::new(&[("pitchfork.toml", DAEMON)]);
    let plan = repo.plan();
    let autostart = item(&plan, "pitchfork:autostart");
    assert_eq!(autostart.kind, ItemKind::Daemon);
    assert_eq!(autostart.confidence, Confidence::Medium);
    assert_eq!(autostart.patch.file, "devflow.toml");
    assert!(autostart.is_actionable());
    assert!(
        autostart
            .risks
            .iter()
            .any(|risk| risk.contains("`pitchfork start` for its daemons")),
        "{:?}",
        autostart.risks
    );
    assert!(autostart.evidence.iter().any(|e| e.contains("web")));
    // Medium confidence: never applied unless somebody accepts it.
    assert!(
        !defaults(&plan).contains(&"pitchfork:autostart".to_string()),
        "{:?}",
        defaults(&plan)
    );

    // Accepted, it writes the one key and the file still loads.
    let mut accepted = defaults(&plan);
    accepted.push("pitchfork:autostart".into());
    apply(repo.root(), &plan, &accepted).unwrap();
    let text = repo.read("devflow.toml");
    assert!(text.contains("[pitchfork]"), "{text}");
    assert!(text.contains("autostart = true"), "{text}");
    let (config, _) = Config::load(repo.root()).unwrap();
    assert!(config.pitchfork.autostart);
}

#[test]
fn proposed_daemons_alone_also_get_one() {
    let repo = Repo::new(&[
        (
            "package.json",
            r#"{"name":"app","scripts":{"dev":"vite"},"devDependencies":{"vite":"5"}}"#,
        ),
        ("bun.lock", "{}"),
        ("vite.config.ts", "export default {};"),
    ]);
    let plan = repo.plan();
    assert!(
        plan.items
            .iter()
            .any(|i| i.id.starts_with("daemon:") && i.is_actionable()),
        "{:?}",
        ids(&plan)
    );
    assert_eq!(
        item(&plan, "pitchfork:autostart").confidence,
        Confidence::Medium
    );
}

#[test]
fn nothing_is_proposed_without_pitchfork_or_daemons() {
    let repo = Repo::new(&[("README.md", "hello\n")]);
    let plan = repo.plan();
    assert!(
        plan.item("pitchfork:autostart").is_none(),
        "{:?}",
        ids(&plan)
    );
}

#[test]
fn an_existing_decision_is_left_alone_whichever_way_it_went() {
    for value in ["true", "false"] {
        let repo = Repo::new(&[
            ("pitchfork.toml", DAEMON),
            (
                "devflow.toml",
                &format!(
                    "[project]\nname = \"shop\"\nid = \"{ID}\"\n\n[pitchfork]\nautostart = {value}\n"
                ),
            ),
        ]);
        let plan = repo.plan();
        assert!(
            plan.item("pitchfork:autostart").is_none(),
            "{:?}",
            ids(&plan)
        );
        assert!(
            plan.already_configured
                .contains(&"pitchfork:autostart".to_string()),
            "{:?}",
            plan.already_configured
        );
    }
}

#[test]
fn daemons_that_start_on_their_own_are_named_in_the_risks() {
    let repo = Repo::new(&[(
        "pitchfork.toml",
        "[daemons.web]\nrun = \"bun run dev\"\n[daemons.sync]\nrun = \"bun run sync\"\ncron = { schedule = \"* * * * *\" }\n",
    )]);
    let plan = repo.plan();
    let autostart = item(&plan, "pitchfork:autostart");
    assert!(
        autostart
            .risks
            .iter()
            .any(|risk| risk.contains("sync (cron)") && risk.contains("starts nothing in bulk")),
        "{:?}",
        autostart.risks
    );
}

#[test]
fn a_plan_cannot_make_init_write_other_pitchfork_keys() {
    use crate::propose::{FilePatch, PatchOp};
    let repo = Repo::new(&[("pitchfork.toml", DAEMON)]);
    let mut plan = repo.plan();
    for item in &mut plan.items {
        if item.id == "pitchfork:autostart" {
            item.patch = FilePatch {
                file: "devflow.toml".into(),
                ops: vec![PatchOp::Set {
                    path: vec!["pitchfork".into(), "other".into()],
                    value: serde_json::json!(true),
                    comment: None,
                }],
            };
        }
    }
    let error = plan.validate().unwrap_err().to_string();
    assert!(error.contains("pitchfork.other"), "{error}");
}
