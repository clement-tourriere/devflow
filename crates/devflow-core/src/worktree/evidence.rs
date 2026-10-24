//! Advisory evidence that the work of a workspace has been merged, for a
//! workspace whose branch may be gone: `list --wide`'s evidence ([`crate::wide`]:
//! the branch's tip is in the default branch's history, has the same tree, has no
//! changes of its own, or one commit of the default branch carries the same
//! patch), judged against the same base (the default branch's upstream, the local
//! default branch without one) from the workspace's branch while it exists, else
//! the commit it was last seen on.
//!
//! This is a hint for a person deciding what to clean up and never a reason to
//! delete anything. A history that cannot be read, or a branch and a commit that
//! are both gone, is `unknown`, not `not merged`.
use crate::project::Project;
use crate::wide::{self, Merged};
use anyhow::{Context, Result};
use git2::{BranchType, Oid};
use serde::Serialize;

/// What the history says about a branch against the base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Merge {
    /// Merged, or looks so; `how` says on what evidence (a squash is a hint).
    Merged { how: Merged },
    /// Commits of the branch the base does not have.
    NotMerged { ahead: usize },
    /// The history could not be read, or the branch is gone.
    Unknown { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MergeEvidence {
    /// What the branch was compared with (`origin/main`, `main`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(flatten)]
    pub merge: Merge,
}

impl MergeEvidence {
    fn unknown(base: Option<String>, reason: impl Into<String>) -> Self {
        Self {
            base,
            merge: Merge::Unknown {
                reason: reason.into(),
            },
        }
    }

    /// A short phrase for a table: `merged into origin/main`, `looks
    /// squash-merged into origin/main (a hint)`, `3 commits not merged`.
    pub fn summary(&self) -> String {
        let base = self.base.as_deref().unwrap_or("the default branch");
        match &self.merge {
            Merge::Merged {
                how: Merged::Ancestor,
            } => format!("merged into {base}"),
            Merge::Merged {
                how: Merged::LooksSquashed,
            } => format!("looks squash-merged into {base} (a hint)"),
            Merge::Merged {
                how: Merged::SameTree,
            } => format!("same content as {base}"),
            Merge::Merged {
                how: Merged::NoChanges,
            } => "no changes of its own".to_string(),
            Merge::NotMerged { ahead: 1 } => "1 commit not merged".to_string(),
            Merge::NotMerged { ahead } => format!("{ahead} commits not merged"),
            Merge::Unknown { .. } => "merge state unknown".to_string(),
        }
    }
}

/// The merge evidence of the work of a workspace: its `branch` while that still
/// exists, else the `head` commit (full id) it was last seen on.
pub fn merge_evidence(
    project: &Project,
    branch: Option<&str>,
    head: Option<&str>,
) -> MergeEvidence {
    match read(project, branch, head) {
        Ok(evidence) => evidence,
        Err(e) => MergeEvidence::unknown(None, format!("{e:#}")),
    }
}

fn read(project: &Project, branch: Option<&str>, head: Option<&str>) -> Result<MergeEvidence> {
    let repo = project.repo()?;
    let Some(base) = wide::default_base(project, &repo) else {
        return Ok(MergeEvidence::unknown(
            None,
            "the default workspace is not on a branch",
        ));
    };
    let name = Some(base.name.clone());
    let tip = branch
        .and_then(|name| repo.find_branch(name, BranchType::Local).ok())
        .and_then(|found| found.get().peel_to_commit().ok().map(|c| c.id()))
        .or_else(|| {
            head.and_then(|id| Oid::from_str(id).ok())
                .filter(|id| repo.find_commit(*id).is_ok())
        });
    let Some(tip) = tip else {
        return Ok(MergeEvidence::unknown(
            name,
            "the branch and the commit it was last on are gone",
        ));
    };
    let merge = match wide::merge_evidence(&repo, tip, base.tip) {
        Some(how) => Merge::Merged { how },
        None => {
            let (ahead, _) = repo
                .graph_ahead_behind(tip, base.tip)
                .context("cannot count the commits of the branch")?;
            Merge::NotMerged { ahead }
        }
    };
    Ok(MergeEvidence { base: name, merge })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::testutil::{commit_all, repo};
    use git2::Repository;

    fn write(root: &std::path::Path, name: &str, text: &str) {
        std::fs::write(root.join(name), text).unwrap();
    }

    fn branch_at(repo: &Repository, name: &str, id: Oid) {
        repo.branch(name, &repo.find_commit(id).unwrap(), true)
            .unwrap();
    }

    fn checkout(repo: &Repository, name: &str) {
        let object = repo.revparse_single(&format!("refs/heads/{name}")).unwrap();
        repo.checkout_tree(&object, Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        repo.set_head(&format!("refs/heads/{name}")).unwrap();
    }

    fn merged(how: Merged) -> Merge {
        Merge::Merged { how }
    }

    #[test]
    fn a_merged_branch_is_merged_and_an_unmerged_one_counts_its_commits() {
        let t = repo(None);
        let git = t.project.repo().unwrap();
        let start = git.head().unwrap().peel_to_commit().unwrap().id();
        branch_at(&git, "feature", start);
        checkout(&git, "feature");
        write(&t.root, "a.txt", "a\n");
        commit_all(&git, "add a");
        write(&t.root, "b.txt", "b\n");
        commit_all(&git, "add b");
        checkout(&git, "main");

        let evidence = merge_evidence(&t.project, Some("feature"), None);
        assert_eq!(evidence.base.as_deref(), Some("main"));
        assert_eq!(evidence.merge, Merge::NotMerged { ahead: 2 });
        assert_eq!(evidence.summary(), "2 commits not merged");

        // Fast-forward main to the branch.
        let tip = git
            .find_branch("feature", BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        git.reference("refs/heads/main", tip.id(), true, "fast-forward")
            .unwrap();
        checkout(&git, "main");
        let evidence = merge_evidence(&t.project, Some("feature"), None);
        assert_eq!(evidence.merge, merged(Merged::Ancestor));
        assert_eq!(evidence.summary(), "merged into main");
    }

    #[test]
    fn a_squash_merge_is_only_a_hint_and_needs_the_same_change() {
        let t = repo(None);
        let git = t.project.repo().unwrap();
        let start = git.head().unwrap().peel_to_commit().unwrap().id();
        branch_at(&git, "feature", start);
        checkout(&git, "feature");
        write(&t.root, "a.txt", "a\n");
        commit_all(&git, "add a");
        write(&t.root, "b.txt", "b\n");
        commit_all(&git, "add b");
        checkout(&git, "main");

        // Main moves on with an unrelated commit, then takes the whole change
        // of the branch as one commit (a squash merge).
        write(&t.root, "other.txt", "other\n");
        commit_all(&git, "unrelated");
        write(&t.root, "a.txt", "a\n");
        write(&t.root, "b.txt", "b\n");
        commit_all(&git, "feature (#1)");

        let evidence = merge_evidence(&t.project, Some("feature"), None);
        assert_eq!(evidence.merge, merged(Merged::LooksSquashed));
        assert!(evidence.summary().contains("a hint"));

        // A change that is not the branch's is not claimed to be.
        branch_at(&git, "partial", start);
        checkout(&git, "partial");
        write(&t.root, "c.txt", "c\n");
        commit_all(&git, "add c");
        checkout(&git, "main");
        assert_eq!(
            merge_evidence(&t.project, Some("partial"), None).merge,
            Merge::NotMerged { ahead: 1 }
        );
    }

    #[test]
    fn a_deleted_branch_is_judged_by_the_commit_it_was_last_on_and_unknown_without_one() {
        let t = repo(None);
        let git = t.project.repo().unwrap();
        let initial = git.head().unwrap().peel_to_commit().unwrap().id();
        let evidence = merge_evidence(&t.project, Some("gone"), Some(&initial.to_string()));
        assert_eq!(evidence.merge, merged(Merged::Ancestor));
        let evidence = merge_evidence(&t.project, Some("gone"), None);
        assert!(
            matches!(evidence.merge, Merge::Unknown { .. }),
            "{evidence:?}"
        );
        let evidence = merge_evidence(
            &t.project,
            None,
            Some("0123456789012345678901234567890123456789"),
        );
        assert!(
            matches!(evidence.merge, Merge::Unknown { .. }),
            "{evidence:?}"
        );
    }

    #[test]
    fn the_upstream_of_the_default_branch_is_the_base_when_it_has_one() {
        let t = repo(None);
        let git = t.project.repo().unwrap();
        let start = git.head().unwrap().peel_to_commit().unwrap().id();
        branch_at(&git, "feature", start);
        checkout(&git, "feature");
        write(&t.root, "a.txt", "a\n");
        let tip = commit_all(&git, "add a");
        checkout(&git, "main");
        // `origin/main` has the branch's commit; the local main does not.
        git.reference("refs/remotes/origin/main", tip, true, "test")
            .unwrap();
        git.remote("origin", "https://example.invalid/shop.git")
            .unwrap();
        let mut config = git.config().unwrap();
        config.set_str("branch.main.remote", "origin").unwrap();
        config
            .set_str("branch.main.merge", "refs/heads/main")
            .unwrap();
        let evidence = merge_evidence(&t.project, Some("feature"), None);
        assert_eq!(evidence.base.as_deref(), Some("origin/main"));
        assert_eq!(evidence.merge, merged(Merged::Ancestor));
    }
}
