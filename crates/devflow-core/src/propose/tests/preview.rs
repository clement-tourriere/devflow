//! Previewing what an apply would write.
use super::stacks::{bun_next_prisma, initialized};
use super::*;
use crate::propose::{DiffKind, FileChange, inputs_changed};

fn snapshot(repo: &Repo) -> (String, String) {
    (
        repo.read("mise.toml"),
        if repo.exists("devflow.toml") {
            repo.read("devflow.toml")
        } else {
            Default::default()
        },
    )
}

#[test]
fn a_preview_shows_the_files_an_apply_would_write_and_writes_nothing() {
    let repo = bun_next_prisma();
    let plan = repo.plan();
    let accepted = defaults(&plan);
    let before = snapshot(&repo);
    let previews = preview(repo.root(), &plan, &accepted).unwrap();
    assert_eq!(snapshot(&repo), before, "nothing was written");
    assert!(!repo.root().join(".git/devflow/init-apply.json").exists());

    let devflow = previews.iter().find(|p| p.file == "devflow.toml").unwrap();
    assert_eq!(devflow.change, FileChange::Created);
    assert_eq!(devflow.removed, 0);
    assert!(devflow.added > 5, "{devflow:?}");
    assert!(devflow.diff.iter().all(|l| l.kind == DiffKind::Added));
    assert!(devflow.conflicts.is_empty());

    // It is the text an apply writes.
    apply(repo.root(), &plan, &accepted).unwrap();
    for shown in &previews {
        assert_eq!(repo.read(&shown.file), shown.after, "{}", shown.file);
    }
}

#[test]
fn additions_to_an_existing_file_are_lines_added_among_the_lines_kept() {
    let repo = initialized();
    let plan = repo.plan();
    let accepted = all(&plan);
    let previews = preview(repo.root(), &plan, &accepted).unwrap();
    let devflow = previews.iter().find(|p| p.file == "devflow.toml");
    let Some(devflow) = devflow else {
        // Everything the file could get is there already.
        return;
    };
    assert_eq!(devflow.change, FileChange::Modified);
    // Additions, except where an array is reflowed to hold an entry per line.
    assert!(devflow.added > devflow.removed, "{devflow:?}");
    assert!(devflow.diff.iter().any(|l| l.kind == DiffKind::Context));
}

#[test]
fn only_the_accepted_items_are_previewed() {
    let repo = bun_next_prisma();
    let plan = repo.plan();
    assert!(preview(repo.root(), &plan, &[]).unwrap().is_empty());
    let one = preview(repo.root(), &plan, &["project:init".to_string()]).unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].file, "devflow.toml");
    assert!(!one[0].after.contains("[hooks]"), "{}", one[0].after);
}

#[test]
fn stale_inputs_are_named_before_an_apply_is_refused() {
    let repo = bun_next_prisma();
    let plan = repo.plan();
    assert!(inputs_changed(repo.root(), &plan).unwrap().is_empty());
    // A file the plan could write appears.
    repo.write("devflow.toml", "# somebody else\n");
    let changed = inputs_changed(repo.root(), &plan).unwrap();
    assert_eq!(changed, ["devflow.toml (created)"]);
    assert!(apply(repo.root(), &plan, &defaults(&plan)).is_err());
    std::fs::remove_file(repo.root().join("devflow.toml")).unwrap();
    // A file detection reads changes.
    repo.write("package.json", "{\"name\": \"other\"}");
    let changed = inputs_changed(repo.root(), &plan).unwrap();
    assert_eq!(changed.len(), 1, "{changed:?}");
    assert!(changed[0].contains("detection reads"), "{changed:?}");
}
