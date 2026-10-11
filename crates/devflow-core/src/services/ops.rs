//! The operations that run in the caller: start, stop, drop and forget.
//!
//! Each reconciles the unfinished journal entries that name its resources
//! first (killing a dead coordinator's helper, rolling back its destination),
//! then takes the resource's lock for the length of the change, so it cannot
//! interleave with a clone that reads the same branch.
use super::catalog::{BranchRecord, Handled, RecordState, SnapshotRecord};
use super::{Endpoint, Service, locks};
use anyhow::{Context, Result};
use std::time::Duration;
use uuid::Uuid;

/// How long a command waits for a resource another command is changing.
const LOCK_WAIT: Duration = Duration::from_secs(30);
/// How long a `forget` waits for another `forget` of the same service.
const FORGET_WAIT: Duration = Duration::from_secs(30);

/// What an earlier, interrupted creation left under its recorded identity.
#[derive(Debug, Clone)]
pub enum Adopted {
    /// The branch was completed and is the owner's: use it.
    Ready(Box<BranchRecord>),
    /// Nothing was recorded: create it under the same identity.
    Gone,
    /// It was rolled back (or belongs to someone else): start over with a new
    /// identity.
    Unusable,
}

impl Service {
    /// Settle the creation an earlier run began under `id`: unfinished
    /// operations on it are recovered first.
    pub fn adopt(&self, id: Uuid, owner: Uuid) -> Result<Adopted> {
        self.recover_touching(&[id])?;
        Ok(match self.branch(id)? {
            None => Adopted::Gone,
            Some(record) if record.state == RecordState::Ready && record.owner == Some(owner) => {
                Adopted::Ready(Box::new(record))
            }
            Some(_) => Adopted::Unusable,
        })
    }

    fn hold(&self, ids: &[Uuid]) -> Result<locks::Held> {
        let held = self.acquire_recovered(ids, None, LOCK_WAIT)?;
        held.ok_or_else(|| {
            self.reconcile_problem(format!(
                "service '{}' is busy: another devflow command is changing it. Wait for it, or \
                 finish or roll back its operations if it is stuck",
                self.key
            ))
            .into_error()
        })
    }

    /// Start the branch's container (creating it again if it was removed)
    /// and wait until it answers `AUTH` and `PING`.
    pub fn up(&self, id: Uuid) -> Result<Endpoint> {
        self.recover_touching(&[id])?;
        let _held = self.hold(&[id])?;
        self.bring_up(id, false)
    }

    /// Stop the branch's container gracefully; the data stays in its volume.
    /// `true` when it was running.
    pub fn down(&self, id: Uuid) -> Result<bool> {
        self.recover_touching(&[id])?;
        let _held = self.hold(&[id])?;
        self.stop_branch(id)
    }

    /// Remove a branch, by recorded identity: its container and its volume.
    pub fn remove_branch(&self, id: Uuid) -> Result<()> {
        self.recover_touching(&[id])?;
        let _held = self.hold(&[id])?;
        let record = self.require_branch(id)?;
        anyhow::ensure!(
            record.state != RecordState::Creating,
            "branch {id} is still being created"
        );
        self.drop_branch(id)
    }

    /// [`Service::remove_branch`] for a caller that must decide on the branch
    /// as it is *now*: the branch's resource lock is taken first, its record is
    /// read again under it, and `check` sees that record before anything is
    /// removed; the lock is kept through the removal. A change that was
    /// made meanwhile (a new volume for the branch) is therefore seen by
    /// `check`, or waits for the removal; it can never be undone by it.
    pub fn remove_branch_if(
        &self,
        id: Uuid,
        check: impl FnOnce(&BranchRecord) -> Result<()>,
    ) -> Result<()> {
        self.recover_touching(&[id])?;
        let _held = self.hold(&[id])?;
        let record = self.require_branch(id)?;
        anyhow::ensure!(
            record.state != RecordState::Creating,
            "branch {id} is still being created"
        );
        check(&record)?;
        self.drop_branch(id)
    }

    /// Remove one snapshot version.
    pub fn remove_snapshot(&self, name: &str, version: u32) -> Result<SnapshotRecord> {
        let snapshot = self.find_snapshot(name, Some(version))?;
        self.recover_touching(&[snapshot.id])?;
        let _held = self.hold(&[snapshot.id])?;
        self.delete_snapshot(snapshot.id)?;
        Ok(snapshot)
    }

    /// Remove one snapshot by its recorded identity (never looked up by a name
    /// again): it must be a ready snapshot of this service.
    pub fn remove_snapshot_by_id(&self, id: Uuid) -> Result<SnapshotRecord> {
        let snapshot = self
            .snapshot(id)?
            .filter(|s| s.state == RecordState::Ready)
            .with_context(|| format!("service '{}' has no ready snapshot {id}", self.key))?;
        self.recover_touching(&[id])?;
        let _held = self.hold(&[id])?;
        self.delete_snapshot(id)?;
        Ok(snapshot)
    }

    /// Remove everything a service that is no longer configured left behind:
    /// every branch (container and volume) and snapshot by recorded identity,
    /// then the entry itself. Refuses while an operation is running.
    pub fn forget(&self) -> Result<Vec<String>> {
        self.forget_checked(|| Ok(()))
    }

    /// [`Service::forget`] that calls `check` once the service's resources are
    /// held and the catalog is locked, right before the first removal. A
    /// refusal there leaves everything as it was.
    ///
    /// A `forgetting` marker is persisted first, and planners refuse to
    /// journal into a marked service (checked under the catalog lock), so the
    /// records read next are the last there will be. The entry is deleted only
    /// after it is read again under that lock and found to hold nothing that
    /// was not handled ([`catalog::ForgetRefused`] otherwise, nothing deleted).
    /// The marker is cleared when the forget ends in an error; a forget that
    /// is killed leaves it, and running `forget` again finishes the job.
    ///
    /// A forget is exclusive per service, from before the marker is set until
    /// the entry is deleted or the marker cleared: a second one waits for the
    /// first ([`FORGET_WAIT`]) and is refused as busy when it takes longer.
    /// Without that, one forget could remove the tombstones and release their
    /// locks, another one make a tombstone again, and the first delete the
    /// entry under it, leaving a container nothing tracks.
    pub fn forget_checked(&self, check: impl FnOnce() -> Result<()>) -> Result<Vec<String>> {
        self.forget_exclusive(FORGET_WAIT, check)
    }

    fn forget_exclusive(
        &self,
        wait: Duration,
        check: impl FnOnce() -> Result<()>,
    ) -> Result<Vec<String>> {
        let _exclusive =
            locks::acquire_forget(&self.lock_dir(), &self.key, wait)?.with_context(|| {
                format!(
                    "service '{}' is busy: it is already being forgotten. Wait for that to \
                     finish, then do this again if the service is still listed",
                    self.key
                )
            })?;
        // Every other sweep leaves unknown attempts to this one while the
        // marker is set, so no tombstone appears behind the removal of them.
        let _forgetting = self.forget_scope();
        self.store.mark_forgetting(&self.key, true)?;
        let result = self.forget_marked(check);
        if result.is_err() {
            // Still exclusive: this cannot clear another forget's marker.
            let _ = self.store.mark_forgetting(&self.key, false);
        } else {
            let _ = self.prune_attempt_locks();
        }
        result
    }

    /// Remove the lock files of this service's resolved helper attempts, once
    /// the entry is gone (see [`locks::prune_attempts`] for why that is safe).
    /// Under the catalog's lock, with the attempts listed now kept: an entry
    /// created since lists its own.
    fn prune_attempt_locks(&self) -> Result<()> {
        self.store.with_lock(|| {
            let keep: Vec<String> = self
                .store
                .load()?
                .services
                .get(&self.key)
                .map(|entry| entry.helpers.iter().map(|h| h.name.clone()).collect())
                .unwrap_or_default();
            locks::prune_attempts(&self.lock_dir(), &self.key, &keep).map(|_| ())
        })
    }

    fn forget_marked(&self, check: impl FnOnce() -> Result<()>) -> Result<Vec<String>> {
        let records = self.records()?;
        let ids: Vec<Uuid> = records
            .branches
            .iter()
            .map(|b| b.id)
            .chain(records.snapshots.iter().map(|s| s.id))
            .collect();
        self.recover_touching(&ids)?;
        let _held = self.hold(&ids)?;
        self.store.with_lock(check)?;
        let mut removed = Vec::new();
        for branch in &records.branches {
            if branch.state == RecordState::Aborted {
                continue;
            }
            self.drop_branch(branch.id)?;
            removed.push(format!("branch {} ({})", branch.id, branch.name));
        }
        for snapshot in &records.snapshots {
            if snapshot.state == RecordState::Aborted {
                continue;
            }
            self.delete_snapshot(snapshot.id)?;
            removed.push(format!("snapshot {}@v{}", snapshot.name, snapshot.version));
        }
        // Helpers whose creation was asked for are looked for one last time:
        // a late one would mount the volumes that are gone and make them again.
        self.sweep_helpers(None)?;
        // The tombstones that fence the attempts of unknown outcome go last (no
        // other forget runs, and the marker keeps new ones from appearing),
        // after everything else forget did: once they are gone nothing holds
        // those names. A forget interrupted or refused after this point finds
        // the attempts still listed, and the next sweep fences them again.
        let fenced = self.remove_tombstones()?;
        // Read again under the catalog lock: what appeared since the records
        // were read (the marker keeps planners out, but a coordinator that
        // was already running, or an operation another process journaled
        // before the marker, would have) is refused, not erased.
        self.store
            .forget_service(
                &self.key,
                &Handled {
                    branches: records.branches.iter().map(|b| b.id).collect(),
                    snapshots: records.snapshots.iter().map(|s| s.id).collect(),
                    fenced,
                },
            )
            .map_err(
                |error| match error.downcast::<super::catalog::ForgetRefused>() {
                    // The store does not know the project; the service does.
                    Ok(refused) => super::catalog::ForgetRefused {
                        root: self.root.clone(),
                        ..refused
                    }
                    .into(),
                    Err(error) => error,
                },
            )?;
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Engine;
    use crate::services::catalog::Store;

    /// A service of an empty store in `dir`, with an (empty) entry.
    fn service_in(dir: &std::path::Path) -> Service {
        let mut svc = crate::services::tests::service();
        svc.store = Store::in_dir(dir, svc.project, "daemon");
        svc.store
            .update(|c| c.service_mut(&svc.key, Engine::Valkey).map(|_| ()))
            .unwrap();
        svc
    }

    fn forgetting(svc: &Service) -> bool {
        svc.store.load().unwrap().services[&svc.key]
            .forgetting
            .is_some()
    }

    #[test]
    fn a_second_forget_is_busy_and_leaves_the_first_ones_marker_alone() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service_in(dir.path());
        // The first forget: exclusive, marker set.
        let first = locks::acquire_forget(&svc.lock_dir(), &svc.key, Duration::ZERO)
            .unwrap()
            .expect("free");
        svc.store.mark_forgetting(&svc.key, true).unwrap();

        let ran = std::cell::Cell::new(false);
        let error = svc
            .forget_exclusive(Duration::from_millis(100), || {
                ran.set(true);
                Ok(())
            })
            .unwrap_err();
        assert!(error.to_string().contains("is busy"), "{error}");
        assert!(!ran.get(), "nothing of the second forget ran");
        // It neither took the first one's place nor cleared its marker.
        assert!(forgetting(&svc));
        drop(first);
    }

    #[test]
    fn a_second_forget_waits_for_the_first_and_finds_the_entry_gone() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service_in(dir.path());
        let first = locks::acquire_forget(&svc.lock_dir(), &svc.key, Duration::ZERO)
            .unwrap()
            .expect("free");
        let second = svc.clone();
        let waiter =
            std::thread::spawn(move || second.forget_exclusive(Duration::from_secs(10), || Ok(())));
        std::thread::sleep(Duration::from_millis(200));
        assert!(!waiter.is_finished(), "it waits while the first one runs");
        // The first one finishes: the entry is deleted, the lock released.
        svc.store
            .forget_service(&svc.key, &Default::default())
            .unwrap();
        drop(first);
        let removed = waiter.join().unwrap().unwrap();
        assert!(removed.is_empty());
        assert!(!svc.store.load().unwrap().services.contains_key(&svc.key));
    }

    #[test]
    fn a_failed_forget_clears_its_marker_and_a_finished_one_prunes_resolved_attempt_locks() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service_in(dir.path());
        let lock_dir = svc.lock_dir();
        let attempt = |service: &str, name: &str| {
            drop(locks::acquire_attempt(&lock_dir, service, name, Duration::ZERO).unwrap());
        };
        attempt(&svc.key, "devflow-resolved");
        attempt("other_cache", "devflow-resolved");

        // A refusal by `check` leaves everything, locks included, and clears
        // the marker it set.
        let error = svc
            .forget_checked(|| anyhow::bail!("refused by the caller"))
            .unwrap_err();
        assert!(error.to_string().contains("refused"), "{error}");
        assert!(!forgetting(&svc));
        assert!(
            lock_dir
                .join("my_cache.attempt.devflow-resolved.lock")
                .exists()
        );
        // The forget lock was released with it.
        assert!(
            locks::acquire_forget(&lock_dir, &svc.key, Duration::ZERO)
                .unwrap()
                .is_some()
        );

        // A forget that deletes the entry removes the resolved attempts' lock
        // files of this service, and only those.
        svc.forget().unwrap();
        assert!(!svc.store.load().unwrap().services.contains_key(&svc.key));
        assert!(
            !lock_dir
                .join("my_cache.attempt.devflow-resolved.lock")
                .exists()
        );
        assert!(
            lock_dir
                .join("other_cache.attempt.devflow-resolved.lock")
                .exists()
        );
    }

    /// A branch record of `svc` that names `volume`.
    fn seed_branch(svc: &Service, volume: &str) -> Uuid {
        let id = Uuid::new_v4();
        let mut record = BranchRecord::for_test(id);
        record.volume = volume.into();
        svc.store
            .update(|c| {
                c.service_mut(&svc.key, Engine::Valkey)?
                    .branches
                    .push(record);
                Ok(())
            })
            .unwrap();
        id
    }

    #[test]
    fn a_guarded_removal_checks_the_record_while_it_holds_the_branch_lock() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service_in(dir.path());
        let id = seed_branch(&svc, "v1");
        let held_during_check = std::cell::Cell::new(false);
        let error = svc
            .remove_branch_if(id, |record| {
                assert_eq!(record.volume, "v1");
                // Anyone else wanting the branch (a reset, a start) has to wait.
                held_during_check.set(
                    locks::acquire(&svc.lock_dir(), &svc.key, &[id], Duration::ZERO)
                        .unwrap()
                        .is_none(),
                );
                anyhow::bail!("not this one")
            })
            .unwrap_err();
        assert!(error.to_string().contains("not this one"), "{error}");
        assert!(
            held_during_check.get(),
            "the lock was taken before the check"
        );
        // A refusal removes nothing and frees the lock.
        assert!(svc.branch(id).unwrap().is_some());
        assert!(
            locks::acquire(&svc.lock_dir(), &svc.key, &[id], Duration::ZERO)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn a_volume_replaced_while_the_cleanup_waited_is_seen_and_the_replacement_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service_in(dir.path());
        // The cleanup was offered for "v1", whose volume is missing.
        let id = seed_branch(&svc, "v1");
        // A reset is replacing it: it holds the branch's lock, and records the
        // new volume before it lets go.
        let reset = locks::acquire(&svc.lock_dir(), &svc.key, &[id], Duration::ZERO)
            .unwrap()
            .expect("free");
        let cleanup = svc.clone();
        let key = svc.key.clone();
        let waiter = std::thread::spawn(move || {
            cleanup.remove_branch_if(id, |record| {
                crate::services::workspace::confirm_lost(&key, id, Some(record), "v1", |_| {
                    Ok(false)
                })
            })
        });
        std::thread::sleep(Duration::from_millis(200));
        assert!(!waiter.is_finished(), "the cleanup waits for the reset");
        svc.store
            .update(|c| {
                c.service_mut(&svc.key, Engine::Valkey)?
                    .branch_mut(id)
                    .unwrap()
                    .volume = "v2".into();
                Ok(())
            })
            .unwrap();
        drop(reset);
        let error = waiter.join().unwrap().unwrap_err().to_string();
        assert!(
            error.contains("different volume") && error.contains("Nothing was dropped"),
            "{error}"
        );
        // The healthy replacement is untouched.
        let kept = svc.branch(id).unwrap().expect("still recorded");
        assert_eq!(kept.volume, "v2");
        assert_eq!(kept.state, RecordState::Ready);
    }
}
