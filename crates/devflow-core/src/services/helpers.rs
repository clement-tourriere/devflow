//! Helper containers: every attempt to create one is recorded, and every
//! cleanup of the service looks for the ones that are not resolved.
//!
//! A helper is a short-lived container that mounts a volume (the copy helper,
//! the one that reads a file out of a volume). Its `docker create` is a
//! client of the daemon, and ending the client (the process is killed, a bound
//! passes) does not cancel the request: the daemon can finish creating the
//! container after whoever asked has recovered, moved on and called the
//! volume free. The container then mounts the volume and pins it against
//! removal, and nothing journaled would find it.
//!
//! So the name of each attempt, unique to it, is persisted in the service's
//! list of helpers ([`HelperAttempt`]) before Docker is asked for it, apart
//! from the operation, and kept after the operation completes. Reconcile and
//! every later cleanup (a rollback, a drop, the removal of a volume) removes
//! the helpers of the list that exist, and retires an attempt only on evidence:
//!
//! - its container was made (the id came back), removed by id and confirmed
//!   gone: **resolved**, and dropped from the list; or
//! - its outcome is unknown (no id came back and no container has the name):
//!   **fenced**.
//!
//! Elapsed time is not evidence. A request the daemon holds can be delivered
//! after any deadline (a stalled client, a slow remote engine), and the late
//! helper would mount the volume and pin it, or, once the volume was removed,
//! recreate it empty under the same name. So an attempt of unknown outcome is
//! retired by occupying its exact name: a **tombstone** container is created
//! under it (from the helper image that is already present, never started,
//! with devflow labels that mark it as a tombstone, and no mounts). A create of
//! that name that reaches the daemon afterwards fails with a name conflict,
//! before any volume is touched. If the tombstone's own create conflicts
//! because the real helper has appeared meanwhile, the real one is removed by
//! its labels (it was never started: its client is gone) and the tombstone is
//! created again.
//!
//! Policy for tombstones: they are tiny (a created container, no process, no
//! mounts) and the attempt they fence can arrive at any time, so none is
//! removed because time has passed. The attempt stays listed with the
//! tombstone's id, every sweep checks the tombstone is still there (and makes
//! it again if someone removed it), and `forget` removes them last, after all
//! its other effects, right before the entry is deleted
//! ([`Service::remove_tombstones`]). Nothing else removes one: not a drop, a
//! volume removal or a reconcile.
//!
//! The helpers of an operation that has not ended are left to the operation
//! (its coordinator is using them, or its recovery owns them).
//!
//! Sweeps run in several processes at once (two reconciles, a volume removal,
//! a `forget`), so each attempt is handled under a lock of its own
//! ([`locks::acquire_attempt`]): inspect, remove, fence and the catalog
//! transition happen as one step, and the attempt is read again under the lock,
//! so a sweep that read it before another resolved it does nothing. A catalog
//! write for an attempt that is no longer listed is not a success: the
//! tombstone made for it is removed. A service being forgotten is left alone
//! by every sweep except `forget`'s own (no new tombstone may appear behind
//! the one that removes them), and `forget` is exclusive per service
//! ([`locks::acquire_forget`]): a second one waits, so it cannot make a
//! tombstone between the first one's removal of them and its deletion of the
//! entry. The entry is deleted only if the attempts still name the tombstones
//! that were removed (by container id, not by the attempt's name).
use super::catalog::{FencedAttempt, HelperAttempt, ServiceCatalog};
use super::docker::{self, HELPER_IMAGE, Removal, TombstoneCreate};
use super::{Service, locks};
use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use std::cell::RefCell;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// How long a `docker create` of a journaled helper name is waited for before
/// it is ended (a create takes seconds; one this late is stuck or delayed).
/// Tests may shorten it with `DEVFLOW_TEST_CREATE_WAIT_SECS`.
const CREATE_WAIT: Duration = Duration::from_secs(60);
/// How many times the tombstone of an attempt is created while the real helper
/// keeps appearing under the name (it is removed between the tries).
const FENCE_TRIES: usize = 3;
/// How many times a volume is removed, helpers swept again between the tries,
/// while Docker says it is in use.
const VOLUME_ATTEMPTS: usize = 4;
/// The pause between those tries.
const VOLUME_PAUSE: Duration = Duration::from_secs(1);
/// How long a sweep waits for another's hold on an attempt: longer than the
/// holder's own bounds (a create waited for, a container killed and confirmed).
const ATTEMPT_LOCK_WAIT: Duration = Duration::from_secs(180);

thread_local! {
    /// The `(project, service)` whose `forget` this thread is running.
    static FORGETTING: RefCell<Vec<(Uuid, String)>> = const { RefCell::new(Vec::new()) };
}

/// While alive, the sweeps of this thread are `forget`'s own.
pub(super) struct ForgetScope;

impl Drop for ForgetScope {
    fn drop(&mut self) {
        FORGETTING.with(|f| {
            f.borrow_mut().pop();
        });
    }
}

fn seconds_from_env(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(default, Duration::from_secs)
}

/// What a sweep did.
#[derive(Debug, Clone, Default)]
pub(super) struct Swept {
    /// Helpers that existed and were removed.
    pub removed: Vec<String>,
    /// Attempts of unknown outcome that were fenced by this sweep.
    pub fenced: Vec<String>,
}

/// What one attempt came to in a sweep.
enum Verdict {
    /// The container was found and removed, or its creation is over: resolved.
    Resolved { removed: Option<String> },
    /// The outcome is unknown and a tombstone (this id) holds the name.
    Fenced(String),
    /// Not resolved: the next cleanup looks again.
    Pending,
}

impl Service {
    /// A name for a new attempt of operation `op`: unique to the attempt.
    pub(super) fn helper_attempt_name(&self, op: Uuid, kind: &str) -> String {
        format!(
            "devflow-{}-{}-{kind}-{}-{}",
            self.project8(),
            self.key,
            super::short(&op, 12),
            super::short(&Uuid::new_v4(), 6)
        )
    }

    /// Record an attempt before Docker is asked to create it (inside the
    /// caller's own journal write, via [`Service::update`]).
    pub(super) fn register_attempt(
        service: &mut super::catalog::ServiceCatalog,
        op: Uuid,
        name: &str,
    ) {
        if !service.helpers.iter().any(|h| h.name == name) {
            service.helpers.push(HelperAttempt {
                name: name.to_string(),
                op,
                registered_at: Utc::now(),
                id: None,
                tombstone: None,
            });
        }
    }

    /// The attempt `name` got container `id`.
    pub(super) fn attempt_created(
        service: &mut super::catalog::ServiceCatalog,
        name: &str,
        id: &str,
    ) {
        if let Some(attempt) = service.helpers.iter_mut().find(|h| h.name == name) {
            attempt.id = Some(id.to_string());
        }
    }

    /// The attempt `name` is resolved: its container was removed and confirmed
    /// gone after `docker create` returned its id.
    pub(super) fn resolve_attempt(&self, name: &str) -> Result<()> {
        self.update(|service| {
            service.helpers.retain(|h| h.name != name);
            Ok(())
        })
    }

    /// Whether `name` is already recorded as an attempt.
    pub(super) fn attempt_recorded(&self, name: &str) -> Result<bool> {
        Ok(self.records()?.helpers.iter().any(|h| h.name == name))
    }

    /// Wait until no `docker create` of a container called `name` is running,
    /// and end it if it takes longer than a create should. A client that is
    /// killed with the process that started it can be delayed and still reach
    /// the daemon later. `true` when a client was seen at all.
    pub(super) fn settle_create(&self, name: &str) -> Result<bool> {
        let until = Instant::now() + seconds_from_env("DEVFLOW_TEST_CREATE_WAIT_SECS", CREATE_WAIT);
        let mut seen = false;
        loop {
            let clients = docker::create_clients(name)?;
            if clients.is_empty() {
                return Ok(seen);
            }
            seen = true;
            if Instant::now() >= until {
                docker::kill_stop_clients(&clients);
                let ended = Instant::now() + Duration::from_secs(10);
                while !docker::create_clients(name)?.is_empty() {
                    ensure!(
                        Instant::now() < ended,
                        "a `docker create` of {name} is still running after being killed"
                    );
                    std::thread::sleep(Duration::from_millis(200));
                }
                // A request that had reached the daemon is carried out there,
                // and may land after this: the attempt stays recorded.
                std::thread::sleep(Duration::from_millis(500));
                return Ok(seen);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// This thread is running `forget` for this service.
    pub(super) fn forget_scope(&self) -> ForgetScope {
        FORGETTING.with(|f| f.borrow_mut().push((self.project, self.key.clone())));
        ForgetScope
    }

    fn forgetting_here(&self) -> bool {
        FORGETTING.with(|f| {
            f.borrow()
                .iter()
                .any(|(project, key)| *project == self.project && *key == self.key)
        })
    }

    /// Take the lock of attempt `name`: the one place where its container is
    /// inspected and removed, its tombstone made and its record changed.
    pub(super) fn hold_attempt(&self, name: &str) -> Result<locks::Held> {
        locks::acquire_attempt(&self.lock_dir(), &self.key, name, ATTEMPT_LOCK_WAIT)?.with_context(
            || {
                format!(
                    "another devflow command has held helper attempt {name} for over {} seconds",
                    ATTEMPT_LOCK_WAIT.as_secs()
                )
            },
        )
    }

    /// Whether the sweep for `owned` handles `attempt`: an operation the
    /// caller is recovering, whatever its step; any other only once it ended
    /// (a live coordinator is using its helpers).
    fn eligible(records: &ServiceCatalog, attempt: &HelperAttempt, owned: Option<Uuid>) -> bool {
        Some(attempt.op) == owned
            || records
                .op(attempt.op)
                .is_none_or(|op| op.step.is_terminal())
    }

    /// Look for the helpers of the list that are not resolved and remove those
    /// that exist. `owned` is an operation the caller is recovering: its
    /// attempts are swept whatever its step; any other operation's only once
    /// it has ended (a live coordinator is using its helpers).
    pub(super) fn sweep_helpers(&self, owned: Option<Uuid>) -> Result<Swept> {
        let records = self.records()?;
        let names: Vec<String> = records
            .helpers
            .iter()
            .filter(|h| Self::eligible(&records, h, owned))
            .map(|h| h.name.clone())
            .collect();
        let mut swept = Swept::default();
        let mut failure: Option<anyhow::Error> = None;
        for name in names {
            if let Err(error) = self.sweep_one(&name, owned, &mut swept) {
                failure
                    .get_or_insert(error.context(format!("the helper {name} could not be swept")));
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(swept),
        }
    }

    /// One attempt of a sweep, under the attempt's lock. The list above is a
    /// snapshot: another process may have resolved the attempt, fenced it or
    /// forgotten the service since, so the attempt is read again here and
    /// acted on only as it is now.
    fn sweep_one(&self, name: &str, owned: Option<Uuid>, swept: &mut Swept) -> Result<()> {
        let _held = self.hold_attempt(name)?;
        let records = self.records()?;
        let Some(attempt) = records
            .helpers
            .iter()
            .find(|h| h.name == name && Self::eligible(&records, h, owned))
            .cloned()
        else {
            // Resolved (or taken over) by someone else while we waited.
            return Ok(());
        };
        // Fencing makes a container; none may appear behind a `forget` that
        // is removing them, except by that `forget`.
        let may_fence = records.forgetting.is_none() || self.forgetting_here();
        match self.sweep_attempt(&attempt, may_fence)? {
            Verdict::Resolved { removed } => {
                swept.removed.extend(removed);
                self.resolve_attempt(&attempt.name)?;
            }
            Verdict::Fenced(id) => {
                if attempt.tombstone.as_deref() != Some(id.as_str())
                    && self.record_tombstone(&attempt, &id)?
                {
                    swept.fenced.push(attempt.name.clone());
                }
            }
            Verdict::Pending => {}
        }
        Ok(())
    }

    fn sweep_attempt(&self, attempt: &HelperAttempt, may_fence: bool) -> Result<Verdict> {
        // 0. A fence that is still in place is all there is to do: the name is
        //    held, so nothing can be created under it.
        if let Some(id) = &attempt.tombstone
            && self.tombstone_holds(attempt, id)?
        {
            return Ok(Verdict::Fenced(id.clone()));
        }
        // 1. A create that is still on its way is waited for, or ended.
        self.settle_create(&attempt.name)?;
        // 2. The container, by the id Docker gave it or else by the name chosen
        //    for this attempt, only if it carries this operation's labels.
        let ours = self.helper_ownership(attempt.op);
        let target = attempt.id.as_deref().unwrap_or(&attempt.name);
        match docker::remove_owned_container(target, attempt.id.as_deref(), &ours) {
            Ok(Removal::Removed) => Ok(Verdict::Resolved {
                removed: Some(attempt.name.clone()),
            }),
            // The create returned an id and the container is gone: nothing can
            // still be on its way.
            Ok(Removal::Absent) if attempt.id.is_some() => Ok(Verdict::Resolved { removed: None }),
            // No id came back and nothing has the name: the daemon may still
            // hold the request. Occupy the name.
            Ok(Removal::Absent) if !may_fence => Ok(Verdict::Pending),
            Ok(Removal::Absent) => self.fence_name(attempt),
            // A namesake that is not this attempt's: the fence looks at it.
            Err(error) if attempt.id.is_none() && error.is::<docker::NotOwned>() && !may_fence => {
                Ok(Verdict::Pending)
            }
            Err(error) if attempt.id.is_none() && error.is::<docker::NotOwned>() => {
                self.fence_name(attempt)
            }
            Err(error) => Err(error),
        }
    }

    /// The labels a tombstone of an attempt of operation `op` carries.
    fn tombstone_labels(&self, op: Uuid) -> Vec<(String, String)> {
        vec![
            (docker::LABEL_PROJECT.into(), self.project.to_string()),
            (docker::LABEL_SERVICE.into(), self.key.clone()),
            (docker::LABEL_TOMBSTONE.into(), op.to_string()),
        ]
    }

    fn tombstone_ownership(&self, op: Uuid) -> docker::Ownership {
        docker::Ownership {
            project: self.project.to_string(),
            service: self.key.clone(),
            identity: (docker::LABEL_TOMBSTONE, op.to_string()),
            identity_may_be_missing: false,
        }
    }

    /// Whether container `id` is still the tombstone holding the attempt's name.
    fn tombstone_holds(&self, attempt: &HelperAttempt, id: &str) -> Result<bool> {
        Ok(docker::inspect_container(id)?.is_some_and(|info| {
            info.id == id
                && info.name == attempt.name
                && self
                    .tombstone_ownership(attempt.op)
                    .mismatch(&info.labels)
                    .is_none()
        }))
    }

    /// Fence an attempt whose outcome is unknown: create a tombstone under its
    /// exact name, so a create that reaches the daemon later fails with a name
    /// conflict. When the tombstone's own create conflicts the name is taken:
    /// by an earlier tombstone (adopted), or by the real helper, which was
    /// created in between (removed by its labels, then the tombstone is created
    /// again). A container that is neither is not ours to touch; the attempt
    /// stays unresolved.
    fn fence_name(&self, attempt: &HelperAttempt) -> Result<Verdict> {
        let helper = self.helper_ownership(attempt.op);
        let tombstone = self.tombstone_ownership(attempt.op);
        let labels = self.tombstone_labels(attempt.op);
        for _ in 0..FENCE_TRIES {
            let created = docker::create_tombstone(&attempt.name, &labels, HELPER_IMAGE)
                .with_context(|| {
                    format!(
                        "the name of helper attempt {} could not be fenced (the image \
                         {HELPER_IMAGE} must be present: it is never pulled for this)",
                        attempt.name
                    )
                })?;
            match created {
                TombstoneCreate::Created(id) => return Ok(Verdict::Fenced(id)),
                TombstoneCreate::NameInUse => {
                    let Some(info) = docker::inspect_container(&attempt.name)? else {
                        // Gone between the two calls: try again.
                        continue;
                    };
                    if tombstone.mismatch(&info.labels).is_none() {
                        return Ok(Verdict::Fenced(info.id));
                    }
                    if helper.mismatch(&info.labels).is_some() {
                        return Ok(Verdict::Pending);
                    }
                    // The real helper: it was created after the cleanup looked
                    // and its client is gone, so nothing runs it.
                    docker::remove_owned_container(&attempt.name, Some(&info.id), &helper)?;
                }
            }
        }
        bail!(
            "the name of helper attempt {} is still taken by a helper after {FENCE_TRIES} tries",
            attempt.name
        )
    }

    /// The attempt is fenced by the tombstone `id`. `false` when the attempt
    /// is no longer listed (it was resolved or the service forgotten while the
    /// tombstone was being made): nothing tracks the tombstone then, so it is
    /// removed here, by its id and labels.
    fn record_tombstone(&self, attempt: &HelperAttempt, id: &str) -> Result<bool> {
        let recorded = self.update(|service| {
            Ok(
                match service.helpers.iter_mut().find(|h| h.name == attempt.name) {
                    Some(listed) => {
                        listed.tombstone = Some(id.to_string());
                        true
                    }
                    None => false,
                },
            )
        })?;
        if !recorded {
            docker::remove_owned_container(id, Some(id), &self.tombstone_ownership(attempt.op))
                .with_context(|| {
                    format!(
                        "the tombstone of helper attempt {}, which is no longer listed, \
                         could not be removed",
                        attempt.name
                    )
                })?;
        }
        Ok(recorded)
    }

    /// Remove the tombstones of every fenced attempt, each by its recorded id
    /// (which must still carry the tombstone labels), confirm they are gone,
    /// and return each attempt with the id of the tombstone that was removed.
    /// Only `forget` calls this, last, right before the entry is deleted:
    /// after it nothing holds the names. The entry may go only while the
    /// attempts still name these very containers ([`FencedAttempt`]).
    pub(super) fn remove_tombstones(&self) -> Result<Vec<FencedAttempt>> {
        let mut gone = Vec::new();
        for listed in self.records()?.helpers {
            // Under the attempt's lock, and as it is now: a sweep that was
            // making this very tombstone has finished and recorded it.
            let _held = self.hold_attempt(&listed.name)?;
            let Some(attempt) = self
                .records()?
                .helpers
                .into_iter()
                .find(|h| h.name == listed.name)
            else {
                continue;
            };
            let Some(id) = &attempt.tombstone else {
                continue;
            };
            docker::remove_owned_container(id, Some(id), &self.tombstone_ownership(attempt.op))
                .with_context(|| {
                    format!(
                        "the tombstone of helper attempt {} could not be removed",
                        attempt.name
                    )
                })?;
            gone.push(FencedAttempt {
                name: attempt.name,
                tombstone: id.clone(),
            });
        }
        Ok(gone)
    }

    /// Remove the helper containers that still mount `volume`: they would make
    /// its removal fail (or, worse, keep it alive), and a helper created late
    /// by a client no journal entry names is found here by its labels. Only a
    /// container that carries this project's and service's labels and a helper
    /// label is removed.
    pub(super) fn remove_helpers_on(&self, volume: &str) -> Result<()> {
        let project = self.project.to_string();
        for info in docker::containers_mounting(volume)? {
            let label = |key: &str| info.labels.get(key).map(String::as_str);
            let ours = label(docker::LABEL_PROJECT) == Some(project.as_str())
                && label(docker::LABEL_SERVICE) == Some(self.key.as_str())
                && label(docker::LABEL_HELPER).is_some();
            if ours {
                docker::kill_and_confirm(&info.id, Duration::from_secs(20)).with_context(|| {
                    format!(
                        "the helper {} that mounts volume {volume} could not be removed",
                        info.name
                    )
                })?;
            }
        }
        Ok(())
    }

    /// Remove volume `name` (a branch's or snapshot's, checked against
    /// `ownership`) once no helper holds it: the list of helpers and the
    /// containers that mount it are swept first, and while Docker says the
    /// volume is in use (a helper was created late, in between) they are swept
    /// again and the removal retried, a bounded number of times.
    pub(super) fn remove_volume_clear_of_helpers(
        &self,
        name: &str,
        ownership: &docker::Ownership,
    ) -> Result<()> {
        retry_while_in_use(
            VOLUME_ATTEMPTS,
            VOLUME_PAUSE,
            || {
                self.sweep_helpers(None)?;
                self.remove_helpers_on(name)
            },
            || docker::remove_owned_volume(name, ownership).map(|_| ()),
        )
    }
}

/// `sweep`, then `remove`; while `remove` fails because the volume is in use,
/// `pause`, `sweep` again and retry, up to `attempts` times in all. Any other
/// failure ends it at once.
fn retry_while_in_use(
    attempts: usize,
    pause: Duration,
    mut sweep: impl FnMut() -> Result<()>,
    mut remove: impl FnMut() -> Result<()>,
) -> Result<()> {
    let mut last = None;
    for attempt in 0..attempts.max(1) {
        sweep()?;
        match remove() {
            Ok(()) => return Ok(()),
            Err(error) if error.is::<docker::VolumeInUse>() => {
                last = Some(error);
                if attempt + 1 < attempts {
                    std::thread::sleep(pause);
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(last.expect("at least one attempt was made"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn in_use() -> anyhow::Error {
        docker::VolumeInUse {
            volume: "v".into(),
            detail: "volume is in use".into(),
        }
        .into()
    }

    #[test]
    fn a_volume_in_use_is_swept_again_and_retried_until_it_is_free() {
        let sweeps = Cell::new(0);
        let removals = Cell::new(0);
        retry_while_in_use(
            4,
            Duration::ZERO,
            || {
                sweeps.set(sweeps.get() + 1);
                Ok(())
            },
            || {
                removals.set(removals.get() + 1);
                // A helper created late holds it for the first two tries.
                if removals.get() < 3 {
                    Err(in_use())
                } else {
                    Ok(())
                }
            },
        )
        .unwrap();
        assert_eq!(
            (sweeps.get(), removals.get()),
            (3, 3),
            "swept before each try"
        );
    }

    #[test]
    fn the_retries_are_bounded_and_another_failure_is_not_retried() {
        let removals = Cell::new(0);
        let error = retry_while_in_use(
            3,
            Duration::ZERO,
            || Ok(()),
            || {
                removals.set(removals.get() + 1);
                Err(in_use())
            },
        )
        .unwrap_err();
        assert_eq!(removals.get(), 3);
        assert!(error.is::<docker::VolumeInUse>());

        removals.set(0);
        let error = retry_while_in_use(
            3,
            Duration::ZERO,
            || Ok(()),
            || {
                removals.set(removals.get() + 1);
                anyhow::bail!("`docker volume rm` failed: something else")
            },
        )
        .unwrap_err();
        assert_eq!(removals.get(), 1, "only 'in use' is retried");
        assert!(!error.is::<docker::VolumeInUse>());

        // A sweep that fails ends it: the volume is not removed blind.
        let removed = Cell::new(false);
        assert!(
            retry_while_in_use(
                3,
                Duration::ZERO,
                || anyhow::bail!("cannot list"),
                || {
                    removed.set(true);
                    Ok(())
                }
            )
            .is_err()
        );
        assert!(!removed.get());
    }

    #[test]
    fn a_tombstone_is_never_taken_for_a_helper_nor_a_helper_for_a_tombstone() {
        let svc = crate::services::tests::service();
        let op = Uuid::from_u128(9);
        let tombstone: std::collections::BTreeMap<String, String> =
            svc.tombstone_labels(op).into_iter().collect();
        assert!(svc.tombstone_ownership(op).mismatch(&tombstone).is_none());
        // Removing "the helper by its labels" never removes the fence.
        assert!(svc.helper_ownership(op).mismatch(&tombstone).is_some());
        let helper: std::collections::BTreeMap<String, String> = [
            (docker::LABEL_PROJECT, svc.project.to_string()),
            (docker::LABEL_SERVICE, svc.key.clone()),
            (docker::LABEL_HELPER, op.to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        assert!(svc.helper_ownership(op).mismatch(&helper).is_none());
        assert!(svc.tombstone_ownership(op).mismatch(&helper).is_some());
        // Another operation's tombstone is not this attempt's.
        assert!(
            svc.tombstone_ownership(Uuid::from_u128(10))
                .mismatch(&tombstone)
                .is_some()
        );
    }

    #[test]
    fn only_the_thread_running_forget_is_the_forgetter() {
        let svc = crate::services::tests::service();
        assert!(!svc.forgetting_here());
        {
            let _scope = svc.forget_scope();
            assert!(svc.forgetting_here());
            // Another thread, even for the same service, is not.
            let other = svc.clone();
            assert!(
                !std::thread::spawn(move || other.forgetting_here())
                    .join()
                    .unwrap()
            );
            // Another service of the thread is not.
            let mut elsewhere = svc.clone();
            elsewhere.key = "queue".into();
            assert!(!elsewhere.forgetting_here());
        }
        assert!(!svc.forgetting_here(), "the scope ends with the forget");
    }

    #[test]
    fn attempts_are_recorded_once_with_names_unique_to_each() {
        let svc = crate::services::tests::service();
        let op = Uuid::from_u128(9);
        let a = svc.helper_attempt_name(op, "verify");
        let b = svc.helper_attempt_name(op, "verify");
        assert_ne!(a, b);
        assert!(a.contains("-verify-") && a.starts_with("devflow-"));
        let mut service = super::super::catalog::ServiceCatalog::new(crate::config::Engine::Valkey);
        Service::register_attempt(&mut service, op, &a);
        Service::register_attempt(&mut service, op, &a);
        Service::register_attempt(&mut service, op, &b);
        assert_eq!(service.helpers.len(), 2);
        Service::attempt_created(&mut service, &a, "abc");
        assert_eq!(service.helpers[0].id.as_deref(), Some("abc"));
        assert!(service.helpers[1].id.is_none());
    }
}
