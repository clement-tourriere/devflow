//! `devflow list --wide`: what the default listing leaves out because finding
//! it costs more than reading Git and receipts.
//!
//! Per workspace: whether the checkout has uncommitted changes, how far its
//! branch is from the default branch's upstream, advisory evidence that the
//! branch was merged, when it was last entered, how its container services
//! stand (Docker is asked, once per container, under a short bound), and its
//! daemons with their ports and URLs (pitchfork is asked once). Nothing here
//! changes anything, and the default listing never calls it.
use crate::pitchfork::Pitchfork;
use crate::processes::{self, DaemonState};
use crate::project::Project;
use crate::services::workspace as container_services;
use crate::setup::{self, WorkspaceStatus};
use crate::worktree::{self, Workspace};
use anyhow::Result;
use git2::{BranchType, Oid, Repository};
use serde::Serialize;

/// How far a branch is from the branch the project's default workspace tracks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AheadBehind {
    /// The ref compared against: the default branch's upstream
    /// (`origin/main`), or the local default branch when it has none.
    pub against: String,
    pub ahead: usize,
    pub behind: usize,
}

/// Why a branch looks merged. Advisory: nothing acts on it, a person decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Merged {
    /// The branch's tip is in the default branch's history: merged, or never
    /// given a commit of its own (Git cannot tell the two apart).
    Ancestor,
    /// The branch's changes are one commit of the default branch's history
    /// with the same patch id, as a squash merge leaves them: a hint, not a
    /// proof (the branch may have changed since in ways the patch id ignores).
    LooksSquashed,
    /// The branch's tree is the default branch's tree.
    SameTree,
    /// The branch has no changes of its own.
    NoChanges,
}

impl Merged {
    pub fn describe(self) -> &'static str {
        match self {
            Merged::Ancestor => "in the default branch",
            Merged::LooksSquashed => "looks squash-merged",
            Merged::SameTree => "same content as the default branch",
            Merged::NoChanges => "no changes of its own",
        }
    }
}

/// A daemon as the listing shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DaemonView {
    pub name: String,
    pub state: String,
    pub ports: Vec<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl From<&DaemonState> for DaemonView {
    fn from(daemon: &DaemonState) -> Self {
        Self {
            name: daemon.name.clone(),
            state: daemon.state.clone(),
            ports: daemon.ports.clone(),
            url: daemon.url.clone(),
        }
    }
}

/// The extra columns of one workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Wide {
    /// Uncommitted changes; `None` when they could not be read.
    pub dirty: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ahead_behind: Option<AheadBehind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged: Option<Merged>,
    /// When the workspace was last used: entered (`devflow switch`, the app) or
    /// ran a command (`devflow exec`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_active_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The workspace's own port (`DEVFLOW_PORT`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_port: Option<u16>,
    /// When it was suspended (`devflow suspend`); cleared by entering it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suspended_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Kept out of `suspend --idle`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    pub services: Vec<container_services::View>,
    /// Why the containers are not known (the workspace's record cannot be read).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub services_error: Option<String>,
    pub daemons: Vec<DaemonView>,
    /// Why `daemons` is not the whole truth (pitchfork could not be asked, the
    /// record of what devflow started cannot be read, …); absent when what
    /// runs was observed. Shown as unknown, never as "none".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemons_unobserved: Option<String>,
}

/// A listed workspace and its wide columns: the default listing's document
/// with a `wide` object added.
#[derive(Debug, Clone, Serialize)]
pub struct WideStatus {
    #[serde(flatten)]
    pub status: WorkspaceStatus,
    pub wide: Wide,
}

/// [`setup::list`] with the wide columns, asking Docker and pitchfork.
pub fn list(project: &Project) -> Result<Vec<WideStatus>> {
    list_with(project, Pitchfork::locate().as_ref())
}

/// [`list`] with a given pitchfork (or none).
pub fn list_with(project: &Project, pitchfork: Option<&Pitchfork>) -> Result<Vec<WideStatus>> {
    let items = setup::list(project)?;
    let workspaces: Vec<Workspace> = items.iter().map(|i| i.workspace.clone()).collect();
    let services = container_services::views(project, &workspaces);
    let probed = processes::probe(pitchfork);
    let snapshot = match (&probed, pitchfork) {
        (Ok(_), Some(pitchfork)) => processes::Snapshot::read(pitchfork, &project.root),
        _ => processes::Snapshot::default(),
    };
    let repo = project.repo().ok();
    let base = repo.as_ref().and_then(|repo| default_base(project, repo));
    Ok(items
        .into_iter()
        .zip(services)
        .map(|(status, services)| {
            let workspace = &status.workspace;
            let (daemons, daemons_unobserved) =
                if workspace.missing || !processes::may_have_daemons(project, workspace) {
                    (Vec::new(), None)
                } else {
                    let seen = processes::status_from(&probed, &snapshot, project, workspace);
                    let unobserved = seen.observation.summary();
                    let views = seen
                        .daemons
                        .iter()
                        .filter(|daemon| daemon.configured || daemon.id.is_some())
                        .map(|daemon| {
                            let mut view = DaemonView::from(daemon);
                            // "Not started" is a statement about pitchfork's
                            // answer; without one it is unknown.
                            if unobserved.is_some() && daemon.is_not_started() {
                                view.state = "unknown".to_string();
                            }
                            view
                        })
                        .collect();
                    (views, unobserved)
                };
            let (ahead_behind, merged) = match (&repo, &base) {
                (Some(repo), Some(base)) if !workspace.is_default && !workspace.missing => {
                    compare(repo, workspace, base)
                }
                _ => (None, None),
            };
            let wide = Wide {
                dirty: worktree::is_dirty(workspace).ok(),
                ahead_behind,
                merged,
                last_active_at: status
                    .receipt
                    .as_ref()
                    .and_then(|r| crate::suspend::last_used(workspace, r)),
                app_port: status.receipt.as_ref().and_then(|r| r.app_port),
                services: services.services,
                services_error: services.error,
                suspended_at: status.receipt.as_ref().and_then(|r| r.suspended_at),
                pinned: status.receipt.as_ref().is_some_and(|r| r.pinned),
                daemons,
                daemons_unobserved,
            };
            WideStatus { status, wide }
        })
        .collect())
}

/// What branches are compared against.
pub(crate) struct Base {
    pub(crate) name: String,
    pub(crate) tip: Oid,
}

/// The default branch's upstream (`origin/main`); the local default branch
/// when it tracks nothing. `None` when there is no default branch to speak of
/// (a detached HEAD, no commits).
pub(crate) fn default_base(project: &Project, repo: &Repository) -> Option<Base> {
    let default = worktree::list(project)
        .ok()?
        .into_iter()
        .find(|w| w.is_default)?
        .branch?;
    let local = repo.find_branch(&default, BranchType::Local).ok()?;
    if let Ok(upstream) = local.upstream()
        && let (Ok(Some(name)), Some(tip)) = (upstream.name(), upstream.get().target())
    {
        return Some(Base {
            name: name.to_string(),
            tip,
        });
    }
    Some(Base {
        name: default,
        tip: local.get().target()?,
    })
}

/// How far the workspace's branch is from `base`, and whether it looks merged.
fn compare(
    repo: &Repository,
    workspace: &Workspace,
    base: &Base,
) -> (Option<AheadBehind>, Option<Merged>) {
    let Some(branch) = &workspace.branch else {
        return (None, None);
    };
    let Some(tip) = repo
        .find_branch(branch, BranchType::Local)
        .ok()
        .and_then(|b| b.get().target())
    else {
        return (None, None);
    };
    let ahead_behind = repo
        .graph_ahead_behind(tip, base.tip)
        .ok()
        .map(|(ahead, behind)| AheadBehind {
            against: base.name.clone(),
            ahead,
            behind,
        });
    (ahead_behind, merge_evidence(repo, tip, base.tip))
}

/// The most commits of the default branch's history a squash is looked for in.
const SQUASH_WINDOW: usize = 500;

/// Whether `tip` looks merged into `base`. Advisory, see [`Merged`].
pub fn merge_evidence(repo: &Repository, tip: Oid, base: Oid) -> Option<Merged> {
    if tip == base || repo.graph_descendant_of(base, tip).ok()? {
        return Some(Merged::Ancestor);
    }
    let tip_commit = repo.find_commit(tip).ok()?;
    let base_commit = repo.find_commit(base).ok()?;
    if tip_commit.tree_id() == base_commit.tree_id() {
        return Some(Merged::SameTree);
    }
    let fork = repo.merge_base(tip, base).ok()?;
    let fork_tree = repo.find_commit(fork).ok()?.tree().ok()?;
    let own = repo
        .diff_tree_to_tree(Some(&fork_tree), Some(&tip_commit.tree().ok()?), None)
        .ok()?;
    if own.deltas().len() == 0 {
        return Some(Merged::NoChanges);
    }
    let wanted = own.patchid(None).ok()?;
    // Each commit the default branch gained since the fork, as a patch: a
    // squash merge is one of them with the branch's whole change.
    let mut walk = repo.revwalk().ok()?;
    walk.push(base).ok()?;
    walk.hide(fork).ok()?;
    for id in walk.flatten().take(SQUASH_WINDOW) {
        let commit = repo.find_commit(id).ok()?;
        if commit.parent_count() != 1 {
            continue;
        }
        let parent = commit.parent(0).ok()?;
        let diff = repo
            .diff_tree_to_tree(Some(&parent.tree().ok()?), Some(&commit.tree().ok()?), None)
            .ok()?;
        if diff.patchid(None).ok() == Some(wanted) {
            return Some(Merged::LooksSquashed);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig() -> git2::Signature<'static> {
        git2::Signature::now("Test", "test@example.com").unwrap()
    }

    /// Commit `files` on top of `parent` (or as a root), returning the commit.
    fn commit(repo: &Repository, parent: Option<Oid>, files: &[(&str, &str)]) -> Oid {
        let mut tree = match parent {
            Some(parent) => repo.find_commit(parent).unwrap().tree().unwrap(),
            None => {
                let empty = repo.treebuilder(None).unwrap().write().unwrap();
                repo.find_tree(empty).unwrap()
            }
        };
        for (name, content) in files {
            let blob = repo.blob(content.as_bytes()).unwrap();
            let mut builder = repo.treebuilder(Some(&tree)).unwrap();
            builder.insert(name, blob, 0o100644).unwrap();
            tree = repo.find_tree(builder.write().unwrap()).unwrap();
        }
        let parents: Vec<_> = parent
            .iter()
            .map(|id| repo.find_commit(*id).unwrap())
            .collect();
        repo.commit(
            None,
            &sig(),
            &sig(),
            "c",
            &tree,
            &parents.iter().collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[test]
    fn merge_evidence_tells_ancestors_squashes_and_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let root = commit(&repo, None, &[("a.txt", "one\n")]);

        // A branch two commits ahead of the root.
        let feature_1 = commit(&repo, Some(root), &[("b.txt", "feature\n")]);
        let feature = commit(&repo, Some(feature_1), &[("c.txt", "more\n")]);

        // Not merged: the default branch is still the root.
        assert_eq!(merge_evidence(&repo, feature, root), None);
        // Merged: the default branch contains the tip (a fast-forward).
        assert_eq!(
            merge_evidence(&repo, feature_1, feature),
            Some(Merged::Ancestor)
        );
        assert_eq!(merge_evidence(&repo, root, root), Some(Merged::Ancestor));

        // Squash-merged: one new commit on the default branch with the
        // branch's whole change, after the default branch moved on.
        let other = commit(&repo, Some(root), &[("d.txt", "other work\n")]);
        let squash = commit(
            &repo,
            Some(other),
            &[("b.txt", "feature\n"), ("c.txt", "more\n")],
        );
        assert_eq!(
            merge_evidence(&repo, feature, squash),
            Some(Merged::LooksSquashed)
        );
        // The same default branch without that commit: not merged.
        assert_eq!(merge_evidence(&repo, feature, other), None);

        // The same content reached another way.
        let same = commit(
            &repo,
            Some(root),
            &[("b.txt", "feature\n"), ("c.txt", "more\n")],
        );
        assert_eq!(merge_evidence(&repo, feature, same), Some(Merged::SameTree));

        // A branch that changes nothing.
        let nothing = commit(&repo, Some(root), &[("a.txt", "one\n")]);
        assert_eq!(
            merge_evidence(&repo, nothing, other),
            Some(Merged::NoChanges)
        );
    }
}
