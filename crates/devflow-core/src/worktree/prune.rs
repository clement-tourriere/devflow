//! Forgetting the Git registrations of worktrees whose directory is gone.
//!
//! A worktree on a drive that is unplugged looks exactly like one whose
//! directory was deleted, and a registration holds more than Git's own
//! bookkeeping: its administrative directory is where devflow keeps the
//! workspace's receipt, the record of the database and service branches the
//! workspace owns. Forgetting a registration therefore needs a person's
//! confirmation of an explicit list, and every item on it is checked again,
//! under the workspace's locks, right before it goes:
//!
//! - [`candidates`] lists what could be forgotten and, for each, why it counts
//!   as missing, whether it has a receipt, what data it is bound to and
//!   whether (and why) it would be refused;
//! - [`prune`] forgets only the [`PruneTarget`]s it is given, those that are
//!   still candidates, and refuses the rest with a [`PruneRefusal`].
//!
//! A registration that still owns data is never forgotten here: that would
//! leave its database or service branches recorded nowhere. The way on is
//! [`Remedy::RemoveWorkspace`], the normal removal, which drops them by
//! recorded identity.
use super::{Workspace, list};
use crate::project::Project;
use crate::receipt::Receipt;
use crate::remedy::Remedy;
use anyhow::{Context, Result};
use git2::WorktreePruneOptions;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a lock held by another devflow operation is waited for.
const LOCK_WAIT: Duration = Duration::from_millis(500);

/// Why a registration would not be forgotten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PruneRefusal {
    /// The folder the directory should be in is missing or unreadable too:
    /// most likely a drive that is not connected, which is not the same as a
    /// worktree that was deleted.
    ParentMissing { parent: PathBuf },
    /// Another devflow operation holds the workspace's lock.
    Busy,
    /// The receipt cannot be read, so what it owns is unknown.
    ReceiptUnreadable { reason: String },
    /// The receipt records data (or daemons) that forgetting the registration
    /// would leave behind unrecorded.
    OwnsData { owns: Vec<String> },
}

impl PruneRefusal {
    /// What is wrong, in plain words (no command line).
    pub fn message(&self) -> String {
        match self {
            PruneRefusal::ParentMissing { parent } => format!(
                "{} is missing or unreadable too, so the drive it is on may just be \
                 disconnected; connect it and look again",
                parent.display()
            ),
            PruneRefusal::Busy => "another devflow operation is using this workspace".into(),
            PruneRefusal::ReceiptUnreadable { reason } => {
                format!("its receipt cannot be read ({reason}), so what it owns is unknown")
            }
            PruneRefusal::OwnsData { owns } => format!(
                "it still owns {}; forgetting the registration would leave that unrecorded. \
                 Remove the workspace instead",
                owns.join(", ")
            ),
        }
    }

    /// What can be done about it, for the workspace `name` of the project at
    /// `root`.
    pub fn remedies(&self, root: &Path, name: &str) -> Vec<Remedy> {
        match self {
            PruneRefusal::OwnsData { .. } => vec![Remedy::RemoveWorkspace {
                root: root.to_path_buf(),
                name: name.to_string(),
            }],
            _ => Vec::new(),
        }
    }
}

/// A registration that could be forgotten, as the person is shown it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PruneCandidate {
    pub name: String,
    /// Git's name for the worktree (its administrative directory).
    pub id: String,
    /// Where the checkout was.
    pub path: PathBuf,
    /// Why it counts as missing.
    pub why: String,
    pub has_receipt: bool,
    /// What the receipt binds the workspace to: services (and a database from
    /// an older devflow), a branch being created, daemons devflow started.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub data: Vec<String>,
    /// Set when it would not be forgotten; `None` when it can be.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<PruneRefusal>,
    /// What can be done instead when it is refused (remove the workspace).
    #[serde(
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "crate::remedy::serialize_remedies"
    )]
    pub remedies: Vec<Remedy>,
}

impl PruneCandidate {
    /// What a caller hands to [`prune`] once the person agreed to this one.
    pub fn target(&self) -> PruneTarget {
        PruneTarget {
            id: self.id.clone(),
            path: self.path.clone(),
        }
    }
}

/// One registration the person agreed to forget: its Git name and where its
/// checkout was, both as [`candidates`] listed them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PruneTarget {
    pub id: String,
    pub path: PathBuf,
}

/// A target that was not forgotten, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PruneRefused {
    pub name: String,
    pub path: PathBuf,
    pub refusal: PruneRefusal,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PruneOutcome {
    /// The names of the workspaces whose registration was forgotten.
    pub pruned: Vec<String>,
    pub refused: Vec<PruneRefused>,
    /// Targets that are not candidates any more (the directory is back, the
    /// registration is gone or locked, a different checkout is there), by
    /// Git name. Nothing was done to them.
    pub changed: Vec<String>,
}

/// Git's name for `workspace`: the name of its administrative directory.
fn id_of(workspace: &Workspace) -> Option<String> {
    workspace
        .git_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

/// The linked worktrees Git could forget: registered, unlocked, directory gone.
fn missing(project: &Project) -> Result<Vec<(String, Workspace)>> {
    Ok(list(project)?
        .into_iter()
        .filter(|w| w.missing && !w.locked && !w.is_default)
        .filter_map(|w| Some((id_of(&w)?, w)))
        .collect())
}

fn why_missing(workspace: &Workspace) -> String {
    match workspace.path.symlink_metadata() {
        Err(_) => format!("its directory {} does not exist", workspace.path.display()),
        Ok(_) => format!(
            "{} is there but is not a readable checkout of this worktree",
            workspace.path.display()
        ),
    }
}

/// The folder of a missing checkout must be there and readable; otherwise it
/// is likely a disconnected drive.
fn parent_problem(workspace: &Workspace) -> Option<PruneRefusal> {
    let parent = workspace.path.parent()?;
    match std::fs::read_dir(parent) {
        Ok(_) => None,
        Err(_) => Some(PruneRefusal::ParentMissing {
            parent: parent.to_path_buf(),
        }),
    }
}

/// What the files say about `workspace`, without any lock: whether it has a
/// receipt, what that receipt owns, and the refusal that follows from them.
fn inspect(workspace: &Workspace) -> (bool, Vec<String>, Option<PruneRefusal>) {
    if let Some(refusal) = parent_problem(workspace) {
        let receipt = Receipt::load(&workspace.git_dir);
        let has = matches!(receipt, Ok(Some(_)));
        return (has, Vec::new(), Some(refusal));
    }
    match Receipt::load(&workspace.git_dir) {
        Ok(None) => (false, Vec::new(), None),
        Ok(Some(receipt)) => {
            let owns = owned_by(&receipt);
            let refusal = (!owns.is_empty()).then(|| PruneRefusal::OwnsData { owns: owns.clone() });
            (true, owns, refusal)
        }
        Err(error) => (
            // A file that is there but unreadable may well be a receipt.
            Receipt::path(&workspace.git_dir).exists(),
            Vec::new(),
            Some(PruneRefusal::ReceiptUnreadable {
                reason: format!("{error:#}"),
            }),
        ),
    }
}

/// What a receipt records that only the workspace's removal handles.
fn owned_by(receipt: &Receipt) -> Vec<String> {
    let mut owns = Vec::new();
    for (key, binding) in &receipt.services {
        if binding.branch_id.is_some() {
            owns.push(format!("the data of service '{key}'"));
        }
    }
    if receipt
        .unmapped
        .as_ref()
        .is_some_and(|data| data.branch_id.is_some())
    {
        owns.push("a database branch from an older devflow".to_string());
    }
    if let Some(creating) = &receipt.creating {
        owns.push(format!(
            "a branch of service '{}' that was being created",
            creating.service
        ));
    }
    if receipt.processes.is_some() {
        owns.push("daemons devflow started".to_string());
    }
    owns
}

/// The registrations whose directory is gone, with what each owns and whether
/// it would be forgotten. Reads only; nothing is locked or changed.
pub fn candidates(project: &Project) -> Result<Vec<PruneCandidate>> {
    Ok(missing(project)?
        .into_iter()
        .map(|(id, workspace)| {
            let (has_receipt, data, mut refusal) = inspect(&workspace);
            if refusal.is_none() && has_receipt && Receipt::is_locked(&workspace.git_dir) {
                refusal = Some(PruneRefusal::Busy);
            }
            let remedies = refusal
                .as_ref()
                .map(|r| r.remedies(&project.root, &workspace.name))
                .unwrap_or_default();
            PruneCandidate {
                why: why_missing(&workspace),
                remedies,
                name: workspace.name,
                id,
                path: workspace.path,
                has_receipt,
                data,
                refusal,
            }
        })
        .collect())
}

/// Forget the registrations of `confirmed` (what the person agreed to, from
/// [`candidates`]) and nothing else. Each is looked at again first: it must
/// still be a missing, unlocked worktree at the same place, its folder must be
/// readable, and under its process and receipt locks its receipt must be
/// readable and own nothing. A target that fails is reported, not forgotten;
/// its branch and everything else stay. A Git error is an error.
pub fn prune(project: &Project, confirmed: &[PruneTarget]) -> Result<PruneOutcome> {
    let repo = project.repo()?;
    let mut outcome = PruneOutcome::default();
    for target in confirmed {
        let current = missing(project)?
            .into_iter()
            .find(|(id, w)| *id == target.id && w.path == target.path);
        let Some((id, workspace)) = current else {
            outcome.changed.push(target.id.clone());
            continue;
        };
        let refused = |refusal| PruneRefused {
            name: workspace.name.clone(),
            path: workspace.path.clone(),
            refusal,
        };
        if let Some(refusal) = parent_problem(&workspace) {
            outcome.refused.push(refused(refusal));
            continue;
        }
        // Lock order: the process lock, then the receipt lock, as a removal
        // takes them. They are held until the registration is gone.
        let git_dir = &workspace.git_dir;
        if !git_dir.is_dir() {
            outcome.changed.push(target.id.clone());
            continue;
        }
        let Some(_process_lock) = Receipt::process_lock_waiting(git_dir, LOCK_WAIT)? else {
            outcome.refused.push(refused(PruneRefusal::Busy));
            continue;
        };
        let Some(_receipt_lock) = Receipt::lock_retrying(git_dir)? else {
            outcome.refused.push(refused(PruneRefusal::Busy));
            continue;
        };
        // Read under the locks: this is the receipt that decides.
        let (_, _, refusal) = inspect(&workspace);
        if let Some(refusal) = refusal {
            outcome.refused.push(refused(refusal));
            continue;
        }
        let registration = repo.find_worktree(&id)?;
        let mut options = WorktreePruneOptions::new();
        if !registration
            .is_prunable(Some(&mut options))
            .unwrap_or(false)
        {
            outcome.changed.push(target.id.clone());
            continue;
        }
        registration
            .prune(Some(&mut options))
            .with_context(|| format!("cannot prune the Git registration of {}", workspace.name))?;
        outcome.pruned.push(workspace.name.clone());
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{TestRepo, repo};
    use super::super::{create, list};
    use super::*;
    use crate::receipt::{CloneReport, ServiceBinding};
    use git2::BranchType;

    fn names(project: &Project) -> Vec<String> {
        list(project).unwrap().into_iter().map(|w| w.name).collect()
    }

    fn gone(t: &TestRepo, name: &str) -> Workspace {
        let ws = create(&t.project, name, None).unwrap();
        std::fs::remove_dir_all(&ws.path).unwrap();
        ws
    }

    fn bind(t: &TestRepo, name: &str, branch: Option<uuid::Uuid>) {
        let ws = list(&t.project)
            .unwrap()
            .into_iter()
            .find(|w| w.name == name)
            .unwrap();
        let mut receipt = Receipt::new(t.project.id(), name, None);
        if let Some(branch) = branch {
            receipt.services.insert(
                "cache".into(),
                ServiceBinding::container(
                    crate::config::Engine::Valkey,
                    branch,
                    "redis://127.0.0.1:1".into(),
                    None,
                    "empty".into(),
                    CloneReport {
                        method: None,
                        captured_at: None,
                        allocated_bytes: None,
                        source_downtime_ms: None,
                    },
                    None,
                ),
            );
        }
        receipt.save(&ws.git_dir).unwrap();
    }

    fn targets(candidates: &[PruneCandidate]) -> Vec<PruneTarget> {
        candidates.iter().map(PruneCandidate::target).collect()
    }

    #[test]
    fn only_confirmed_candidates_are_forgotten_and_a_present_or_locked_one_never() {
        let t = repo(None);
        let kept = create(&t.project, "kept", None).unwrap();
        gone(&t, "gone");
        gone(&t, "also-gone");
        let locked = gone(&t, "locked");
        t.project
            .repo()
            .unwrap()
            .find_worktree("locked")
            .unwrap()
            .lock(Some("keep"))
            .unwrap();
        let _ = locked;

        let listed = candidates(&t.project).unwrap();
        let listed_names: Vec<&str> = listed.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(listed_names, ["also-gone", "gone"], "{listed:?}");
        assert!(listed.iter().all(|c| c.refusal.is_none() && !c.has_receipt));
        assert!(listed[0].why.contains("does not exist"), "{listed:?}");

        // Only what was confirmed: `also-gone` stays registered.
        let only_gone: Vec<PruneTarget> = listed
            .iter()
            .filter(|c| c.name == "gone")
            .map(PruneCandidate::target)
            .collect();
        let outcome = prune(&t.project, &only_gone).unwrap();
        assert_eq!(outcome.pruned, ["gone"]);
        assert!(outcome.refused.is_empty() && outcome.changed.is_empty());
        let after = names(&t.project);
        assert!(after.contains(&"kept".to_string()), "{after:?}");
        assert!(after.contains(&"also-gone".to_string()), "{after:?}");
        assert!(after.contains(&"locked".to_string()), "a locked one stays");
        assert!(!after.contains(&"gone".to_string()), "{after:?}");
        assert!(kept.path.is_dir());
        // Its branch is not deleted.
        let repo = t.project.repo().unwrap();
        assert!(repo.find_branch("gone", BranchType::Local).is_ok());

        // A target that is not a candidate any more is skipped, not an error:
        // the same one again, a present checkout, a locked registration.
        let mut stale = only_gone.clone();
        stale.push(PruneTarget {
            id: "kept".into(),
            path: kept.path.clone(),
        });
        stale.push(PruneTarget {
            id: "locked".into(),
            path: locked.path.clone(),
        });
        let outcome = prune(&t.project, &stale).unwrap();
        assert!(outcome.pruned.is_empty() && outcome.refused.is_empty());
        assert_eq!(outcome.changed, ["gone", "kept", "locked"]);
        // A target at another place than the one shown is another worktree.
        let mut moved = targets(&candidates(&t.project).unwrap());
        moved[0].path = t.dir.path().join("elsewhere");
        let outcome = prune(&t.project, &moved).unwrap();
        assert!(outcome.pruned.is_empty());
        assert_eq!(outcome.changed.len(), 1);
        assert!(names(&t.project).contains(&"also-gone".to_string()));
    }

    #[test]
    fn a_registration_that_owns_data_is_refused_with_the_removal_as_the_way_on() {
        let t = repo(None);
        let ws = gone(&t, "owner");
        let branch = uuid::Uuid::new_v4();
        bind(&t, "owner", Some(branch));
        gone(&t, "plain");
        bind(&t, "plain", None); // a receipt that binds nothing

        let listed = candidates(&t.project).unwrap();
        let owner = listed.iter().find(|c| c.name == "owner").unwrap();
        assert!(owner.has_receipt);
        assert_eq!(owner.data, ["the data of service 'cache'"]);
        let Some(refusal @ PruneRefusal::OwnsData { .. }) = &owner.refusal else {
            panic!("{owner:?}");
        };
        assert!(!refusal.message().contains("devflow "), "{refusal:?}");
        assert_eq!(owner.remedies, refusal.remedies(&t.root, "owner"));
        assert_eq!(
            refusal.remedies(&t.root, &owner.name),
            [Remedy::RemoveWorkspace {
                root: t.root.clone(),
                name: "owner".into()
            }]
        );
        let plain = listed.iter().find(|c| c.name == "plain").unwrap();
        assert!(plain.has_receipt && plain.refusal.is_none(), "{plain:?}");

        // Even when the person confirmed both, the owner stays registered with
        // its receipt; the one that owns nothing goes.
        let outcome = prune(&t.project, &targets(&listed)).unwrap();
        assert_eq!(outcome.pruned, ["plain"]);
        assert_eq!(outcome.refused.len(), 1);
        assert_eq!(outcome.refused[0].name, "owner");
        assert!(matches!(
            outcome.refused[0].refusal,
            PruneRefusal::OwnsData { .. }
        ));
        assert!(names(&t.project).contains(&"owner".to_string()));
        assert!(Receipt::path(&ws.git_dir).is_file(), "the receipt is kept");

        // Daemons devflow started count too, and so does a receipt that
        // cannot be read.
        let mut receipt = Receipt::load(&ws.git_dir).unwrap().unwrap();
        receipt.services.clear();
        receipt.processes = Some(crate::receipt::ProcessRecord {
            started_at: chrono::Utc::now(),
            ids: vec!["ns/web".into()],
        });
        receipt.save(&ws.git_dir).unwrap();
        let listed = candidates(&t.project).unwrap();
        assert_eq!(listed[0].data, ["daemons devflow started"]);
        assert!(listed[0].refusal.is_some());
        std::fs::write(Receipt::path(&ws.git_dir), "{ not json").unwrap();
        let listed = candidates(&t.project).unwrap();
        assert!(matches!(
            listed[0].refusal,
            Some(PruneRefusal::ReceiptUnreadable { .. })
        ));
        let outcome = prune(&t.project, &targets(&listed)).unwrap();
        assert!(outcome.pruned.is_empty(), "{outcome:?}");
        assert!(names(&t.project).contains(&"owner".to_string()));
    }

    #[test]
    fn a_missing_folder_means_a_disconnected_drive_and_nothing_is_forgotten() {
        let t = repo(None);
        let ws = gone(&t, "away");
        // The folder the checkout was in goes too (a drive that is not there).
        let parent = ws.path.parent().unwrap().to_path_buf();
        std::fs::remove_dir_all(&parent).unwrap();
        let listed = candidates(&t.project).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].refusal,
            Some(PruneRefusal::ParentMissing {
                parent: parent.clone()
            })
        );
        let message = listed[0].refusal.as_ref().unwrap().message();
        assert!(message.contains("disconnected"), "{message}");
        let outcome = prune(&t.project, &targets(&listed)).unwrap();
        assert!(outcome.pruned.is_empty());
        assert_eq!(outcome.refused[0].name, "away");
        assert!(matches!(
            outcome.refused[0].refusal,
            PruneRefusal::ParentMissing { .. }
        ));
        assert!(names(&t.project).contains(&"away".to_string()));
        assert!(t.root.join(".git/worktrees/away").is_dir());

        // The drive comes back with the folder: the registration is forgotten.
        std::fs::create_dir_all(&parent).unwrap();
        let listed = candidates(&t.project).unwrap();
        assert!(listed[0].refusal.is_none(), "{listed:?}");
        assert_eq!(
            prune(&t.project, &targets(&listed)).unwrap().pruned,
            ["away"]
        );
    }

    #[test]
    fn a_workspace_another_operation_holds_is_busy_and_stays() {
        let t = repo(None);
        let ws = gone(&t, "busy");
        bind(&t, "busy", None);
        let listed = candidates(&t.project).unwrap();
        assert!(listed[0].refusal.is_none());
        let held = Receipt::try_lock(&ws.git_dir).unwrap().expect("free");
        let listed = candidates(&t.project).unwrap();
        assert_eq!(listed[0].refusal, Some(PruneRefusal::Busy));
        let outcome = prune(&t.project, &targets(&listed)).unwrap();
        assert!(outcome.pruned.is_empty());
        assert_eq!(outcome.refused[0].refusal, PruneRefusal::Busy);
        assert!(Receipt::path(&ws.git_dir).is_file());
        drop(held);
        assert_eq!(
            prune(&t.project, &targets(&candidates(&t.project).unwrap()))
                .unwrap()
                .pruned,
            ["busy"]
        );
    }

    #[test]
    fn a_directory_that_came_back_is_not_forgotten() {
        let t = repo(None);
        let ws = create(&t.project, "back", None).unwrap();
        let hidden = t.dir.path().join("hidden");
        std::fs::rename(&ws.path, &hidden).unwrap();
        let listed = candidates(&t.project).unwrap();
        assert_eq!(listed.len(), 1);
        // The drive is mounted again before the person confirmed.
        std::fs::rename(&hidden, &ws.path).unwrap();
        let outcome = prune(&t.project, &targets(&listed)).unwrap();
        assert!(outcome.pruned.is_empty());
        assert_eq!(outcome.changed, ["back"]);
        assert!(names(&t.project).contains(&"back".to_string()));
        assert!(ws.path.join(".git").exists());
    }
}
