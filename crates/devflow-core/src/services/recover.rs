//! Recovery: completing or rolling back every journaled step.
//!
//! A coordinator can die at any point, and its host lock dies with it while
//! a helper may live on in the Docker VM. So every operation that touches a
//! source or destination first reconciles the unfinished entries that name
//! them, under the same locks: it kills the recorded helper and confirms it
//! is gone, removes what the entry created by recorded identity (labels
//! corroborate), marks the destination aborted and never promotes it. What
//! [`catalog::recovery`] calls forward (a copy that finished, a removal that
//! was asked for) is finished instead.
use super::Service;
use super::catalog::{BranchRecord, Op, OpKind, RecordState, Recovery, ResetPlan, Step, recovery};
use super::docker;
use super::launch::VolumeOwner;
use super::locks;
use super::profile;
use crate::config::Engine;
use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::Serialize;
use std::time::Duration;
use uuid::Uuid;

/// How long a planned operation may wait for its coordinator to appear
/// before it is taken for abandoned (the caller died between journal and
/// spawn).
const START_GRACE: Duration = Duration::from_secs(120);
/// How many times the locks of an operation are taken, an unfinished operation
/// that touches them found and recovered, and the locks taken again.
const RECOVERY_ROUNDS: usize = 5;

/// The time one recovery allows its Docker calls in all (a fenced MySQL stop
/// alone may take 6 minutes): when it is used up the recovery stops with a
/// [`docker::QueryTimedOut`], the journal intact. Tests may set it with
/// `DEVFLOW_TEST_RECOVERY_BUDGET_SECS`.
const RECOVERY_BUDGET: Duration = Duration::from_secs(20 * 60);

fn recovery_budget() -> Duration {
    std::env::var("DEVFLOW_TEST_RECOVERY_BUDGET_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(RECOVERY_BUDGET, Duration::from_secs)
}

/// Why recovery runs.
pub(super) enum Cause<'a> {
    /// The entry was found unfinished and nobody holds its locks.
    Crash,
    /// The coordinator itself failed, with this message.
    Failed(&'a str),
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ReconcileReport {
    /// What was completed or rolled back.
    pub actions: Vec<String>,
    /// Operations a live coordinator is still running (left alone).
    pub in_progress: Vec<String>,
    /// Things an operator has to look at.
    pub problems: Vec<String>,
    /// Docker resources carrying this project's labels that the catalog does
    /// not know: reported, never removed.
    pub orphans: Vec<String>,
}

impl Service {
    /// Reconcile every unfinished operation of this service, then report
    /// what is left that the catalog does not know.
    pub fn reconcile(&self) -> Result<ReconcileReport> {
        let mut report = ReconcileReport::default();
        for op in self.records()?.unfinished().into_iter().cloned() {
            match self.recover_if_abandoned(&op) {
                Ok(Some(action)) => report.actions.push(action),
                Ok(None) => report.in_progress.push(format!(
                    "{:?} of {} is {:?} and still held by its coordinator",
                    op.kind, op.target, op.step
                )),
                Err(error) => report
                    .problems
                    .push(format!("{:?} of {}: {error:#}", op.kind, op.target)),
            }
        }
        // The helpers whose creation was asked for and not proven resolved,
        // of every operation that has ended (those just recovered included).
        match self.sweep_helpers(None) {
            Ok(swept) => {
                report.actions.extend(swept.removed.into_iter().map(|name| {
                    format!("removed helper {name}, which Docker created after its client was gone")
                }));
                report.actions.extend(swept.fenced.into_iter().map(|name| {
                    format!(
                        "fenced helper {name}: Docker may still create it, so its name is held \
                         by a tombstone container that `forget` removes"
                    )
                }));
            }
            Err(error) => report
                .problems
                .push(format!("cannot sweep helpers: {error:#}")),
        }
        match self.unrecorded() {
            Ok(found) => report.orphans.extend(found),
            Err(error) => report
                .problems
                .push(format!("cannot list resources: {error:#}")),
        }
        Ok(report)
    }

    /// Recover the unfinished operations that name any of `ids`, before
    /// something else is done with them. An operation a live coordinator is
    /// running is an error: the resource is busy.
    pub(crate) fn recover_touching(&self, ids: &[Uuid]) -> Result<Vec<String>> {
        let mut actions = Vec::new();
        for op in self
            .records()?
            .unfinished_touching(ids)
            .into_iter()
            .cloned()
            .collect::<Vec<_>>()
        {
            match self.recover_if_abandoned(&op)? {
                Some(action) => actions.push(action),
                None => {
                    return Err(self
                        .reconcile_problem(format!(
                            "{:?} of {} is in progress (step {:?}); wait for it, or finish or \
                             roll it back if it is stuck",
                            op.kind, op.target, op.step,
                        ))
                        .into_error());
                }
            }
        }
        Ok(actions)
    }

    /// Whether `op` was planned a moment ago and has no coordinator yet: its
    /// caller is still starting one.
    fn planned_recently(op: &Op) -> Result<bool> {
        Ok(op.coordinator.is_none()
            && Utc::now() - op.started_at < chrono::Duration::from_std(START_GRACE)?)
    }

    /// Take the locks of `ids` and return them only once no unfinished
    /// operation other than `own` that touches them is left behind by a dead
    /// coordinator. The check made before locking (`recover_touching`) is not
    /// enough: two requests can both pass it, one then takes the locks, stops a
    /// source and dies, and the other would run on what it left. So whoever
    /// holds the locks looks again; an abandoned operation found is recovered
    /// (it needs locks of its own, so these are released for that) and the locks
    /// are taken again, a bounded number of times. `None` when another holder
    /// kept a lock for `wait`.
    pub(super) fn acquire_recovered(
        &self,
        ids: &[Uuid],
        own: Option<Uuid>,
        wait: Duration,
    ) -> Result<Option<locks::Held>> {
        for _ in 0..RECOVERY_ROUNDS {
            let Some(held) = locks::acquire(&self.lock_dir(), &self.key, ids, wait)? else {
                return Ok(None);
            };
            let mut abandoned = Vec::new();
            for op in self.records()?.unfinished_touching(ids) {
                // Holding the lock of a resource the operation touches means
                // its coordinator is gone, unless it was only just planned.
                if Some(op.id) != own && !Self::planned_recently(op)? {
                    abandoned.push(op.clone());
                }
            }
            if abandoned.is_empty() {
                return Ok(Some(held));
            }
            drop(held);
            for op in &abandoned {
                self.recover_if_abandoned(op)?;
            }
        }
        Err(self
            .reconcile_problem(format!(
                "service '{}' keeps having unfinished operations that touch the same resources; \
                 finish or roll them back",
                self.key,
            ))
            .into_error())
    }

    /// Recover `op` if no coordinator holds its locks. `None`: someone does
    /// (or it was only just planned) and it is left alone.
    fn recover_if_abandoned(&self, op: &Op) -> Result<Option<String>> {
        if Self::planned_recently(op)? {
            return Ok(None);
        }
        let Some(held) =
            locks::acquire(&self.lock_dir(), &self.key, &op.resources(), Duration::ZERO)?
        else {
            return Ok(None);
        };
        // Re-read under the locks: it may have ended while we looked.
        let current = self.require_op(op.id)?;
        let action = if current.step.is_terminal() {
            format!("{:?} of {} had already ended", current.kind, current.target)
        } else {
            self.recover_op(op.id, Cause::Crash)?
        };
        drop(held);
        Ok(Some(action))
    }

    /// Complete or roll back operation `op_id`. The caller holds the locks of
    /// everything the operation touches.
    ///
    /// Every Docker call it makes is bounded by what is left of the recovery's
    /// time ([`recovery_budget`]) besides its own bound. When the daemon does
    /// not answer in time the error is a [`docker::QueryTimedOut`]; the journal
    /// entry is left as it was, and the next run settles it.
    pub(super) fn recover_op(&self, op_id: Uuid, cause: Cause<'_>) -> Result<String> {
        docker::within_deadline(recovery_budget(), || self.recover_op_bounded(op_id, cause))
    }

    fn recover_op_bounded(&self, op_id: Uuid, cause: Cause<'_>) -> Result<String> {
        let op = self.require_op(op_id)?;
        let mut plan = recovery(op.kind, op.step);
        let note = match &cause {
            Cause::Crash => "its coordinator died".to_string(),
            Cause::Failed(message) => (*message).to_string(),
        };
        // A copy or start that just failed is not repeated: a failure rolls
        // back. Removals were asked for and are finished either way. A MySQL
        // destination is never completed after a crash: its identity is set
        // by a bootstrap that is journaled but not resumable, so it is
        // rolled back and made again.
        if plan == Recovery::Forward
            && matches!(op.kind, OpKind::Clone | OpKind::Snapshot)
            && (matches!(cause, Cause::Failed(_)) || self.engine == Engine::Mysql)
        {
            plan = Recovery::Abort;
        }
        match plan {
            Recovery::None => Ok(format!("{:?} of {} had already ended", op.kind, op.target)),
            Recovery::Abort => {
                self.abort_op(&op, &note)?;
                Ok(format!(
                    "rolled back {:?} of {} at {:?}: {note}",
                    op.kind, op.target, op.step
                ))
            }
            Recovery::Forward => {
                // Only a copy that is still there is completed. Whatever
                // else is found (a volume removed or replaced meanwhile, a
                // dump that is gone) rolls the operation back: a completion
                // would start a server on data that is not the source's.
                if let Err(missing) = self.forward_precondition(&op) {
                    // A daemon that did not answer says nothing about the
                    // copy: the operation stays as it is for the next run,
                    // never rolled back on a guess.
                    if docker::timed_out(&missing) {
                        return Err(missing);
                    }
                    let note = format!("cannot be completed ({missing:#}); rolled back instead");
                    self.abort_op(&op, &note)?;
                    return Ok(format!(
                        "rolled back {:?} of {} at {:?}: {note}",
                        op.kind, op.target, op.step
                    ));
                }
                self.forward_op(&op)?;
                Ok(format!(
                    "completed {:?} of {} from {:?}",
                    op.kind, op.target, op.step
                ))
            }
        }
    }

    /// Kill the helpers the entry recorded (the copy helper and the one that
    /// reads a volume for a recovery), each by its id or, if the id was not
    /// saved yet, by the name chosen for it, and confirm they are gone. A
    /// container that does not carry this operation's helper labels is not
    /// ours: the recorded id of one is an error and it stays, a name that
    /// merely matches is skipped.
    pub(super) fn kill_helper(&self, op: &Op) -> Result<()> {
        let ours = self.helper_ownership(op.id);
        for (id, name, what) in [
            (&op.helper_id, &op.helper_name, "copy helper"),
            (&op.verify_id, &op.verify_name, "verify helper"),
        ] {
            let removed = match (id, name) {
                (Some(id), _) => docker::remove_owned_container(id, Some(id), &ours),
                (None, Some(name)) => {
                    // One attempt at a time: a sweep of the same attempt
                    // must not fence the name between the removal and the
                    // record being resolved.
                    let _held = self.hold_attempt(name)?;
                    // The name was journaled before Docker was asked for it,
                    // so a `docker create` of it may still be on its way (a
                    // client that was delayed when its process was killed):
                    // it is waited for, or ended, before the name is looked up.
                    self.settle_create(name)?;
                    match docker::remove_owned_container(name, None, &ours) {
                        Err(error) if error.is::<docker::NotOwned>() => continue,
                        // The container was made and is now removed and
                        // confirmed gone: that attempt is resolved. (Absent
                        // is not: the sweep below fences the name.)
                        Ok(docker::Removal::Removed) => {
                            self.resolve_attempt(name)?;
                            Ok(docker::Removal::Removed)
                        }
                        other => other,
                    }
                }
                (None, None) => continue,
            };
            removed.with_context(|| format!("the {what} could not be removed"))?;
        }
        // Every attempt of this operation that is not resolved (a create whose
        // client was ended may still be carried out by the daemon).
        self.sweep_helpers(Some(op.id))?;
        Ok(())
    }

    /// Remove a container the catalog names: by its recorded id, which must
    /// still carry this branch's labels, else by the name chosen for it (an
    /// interrupted creation that never saved the id), only when its labels
    /// say it was made for this branch. A container that merely has that name
    /// is someone else's and is left alone; a recorded id whose container
    /// carries other labels is an error, and the container stays.
    pub(super) fn remove_recorded_container(
        &self,
        branch: Uuid,
        id: Option<&str>,
        name: &str,
    ) -> Result<()> {
        let ours = self.container_ownership(branch);
        match id {
            Some(id) => docker::remove_owned_container(id, Some(id), &ours)
                .map(|_| ())
                .with_context(|| self.kept_hint(&format!("container {name}"))),
            None => match docker::remove_owned_container(name, None, &ours) {
                Err(error) if error.is::<docker::NotOwned>() => Ok(()),
                other => other.map(|_| ()),
            },
        }
    }

    /// Remove a volume the catalog names, only if it carries the project, the
    /// service and the identity of `owner` the catalog records it for. One
    /// that does not is kept and the operation stops with the reason, so the
    /// step is not counted as done and `reconcile` reports it until an
    /// operator settles it. `legacy` also accepts a snapshot volume that has
    /// no snapshot label (made by a pre-release build).
    pub(super) fn remove_recorded_volume(
        &self,
        name: &str,
        owner: VolumeOwner,
        legacy: bool,
    ) -> Result<()> {
        // A helper that mounts the volume (one that appeared after the
        // journal was read, or that Docker finished creating late) would make
        // Docker refuse: the helpers go first, and again while it says the
        // volume is in use.
        self.remove_volume_clear_of_helpers(name, &self.volume_ownership(owner, legacy))
            .map_err(|error| error.context(self.kept_hint(&format!("volume {name}"))))
    }

    fn kept_hint(&self, resource: &str) -> crate::remedy::Problem {
        self.reconcile_problem(format!(
            "{resource} of service '{key}' was kept and the operation is not finished; if it is \
             not devflow's, remove it yourself, then finish the operation",
            key = self.key
        ))
    }

    /// Roll an operation back: the helper is killed and confirmed gone, what
    /// the entry created is removed by recorded identity, the destination is
    /// marked aborted (never promoted) and the entry ends `Aborted`.
    fn abort_op(&self, op: &Op, why: &str) -> Result<()> {
        // The intent is recorded before anything is removed. Once it is, no
        // recovery completes the operation: a rollback that is interrupted
        // here is finished by the next one, and what it had already removed
        // is never made again (a completion would start a server on a volume
        // it recreated empty).
        let journaled;
        let op = if op.step == Step::RollingBack {
            op
        } else {
            let message = why.to_string();
            self.update(|service| {
                let entry = service
                    .op_mut(op.id)
                    .context("the journal entry vanished")?;
                entry.error = Some(message);
                entry.advance(Step::RollingBack)
            })?;
            journaled = self.require_op(op.id)?;
            &journaled
        };
        self.kill_helper(op)?;
        // The copier is gone, so a source this operation stopped (MySQL) can
        // serve again; this comes before anything else is undone.
        self.restore_stopped_source(op)?;
        match op.kind {
            OpKind::CreateRoot | OpKind::Clone => {
                if let Some(branch) = self.records()?.branch(op.target).cloned() {
                    // The bootstrap container (MySQL) goes first.
                    if let Some(name) = &op.boot_name {
                        self.remove_recorded_container(branch.id, op.boot_id.as_deref(), name)?;
                    }
                    self.remove_recorded_container(
                        branch.id,
                        branch.container_id.as_deref(),
                        &branch.container_name,
                    )?;
                    self.remove_recorded_volume(
                        &branch.volume,
                        VolumeOwner::Branch(branch.id),
                        false,
                    )?;
                }
            }
            OpKind::Snapshot => {
                if let Some(snapshot) = self.snapshot(op.target)? {
                    self.remove_recorded_volume(
                        &snapshot.volume,
                        VolumeOwner::Snapshot(snapshot.id),
                        false,
                    )?;
                }
            }
            OpKind::Reset => self.abort_reset(op)?,
            OpKind::Drop | OpKind::DeleteSnapshot => {
                bail!("a removal is finished, never rolled back")
            }
        }
        self.update(|service| {
            if let Some(branch) = service.branch_mut(op.target)
                && branch.state == RecordState::Creating
            {
                branch.state = RecordState::Aborted;
                branch.container_id = None;
                branch.updated_at = Utc::now();
            }
            if let Some(snapshot) = service.snapshot_mut(op.target)
                && snapshot.state == RecordState::Creating
            {
                snapshot.state = RecordState::Aborted;
            }
            if let Some(entry) = service.op_mut(op.id) {
                entry.advance(Step::Aborted)?;
            }
            Ok(())
        })
    }

    /// A reset's rollback: the original pair is untouched in the catalog; the
    /// replacement volume, the staging and final containers go, and the
    /// original container is started again if the cutover had stopped it.
    fn abort_reset(&self, op: &Op) -> Result<()> {
        let plan: &ResetPlan = op.reset.as_ref().context("a reset has no plan")?;
        let branch = self.require_branch(op.target)?;
        // The replacement's containers, by recorded id or chosen name.
        self.remove_recorded_container(branch.id, plan.staging_id.as_deref(), &plan.staging_name)?;
        self.remove_recorded_container(
            branch.id,
            plan.container_id.as_deref(),
            &plan.container_name,
        )?;
        self.remove_recorded_volume(&plan.volume, VolumeOwner::Branch(branch.id), false)?;
        if op.reached() == Step::Cutover && plan.old_was_running {
            // The old container was stopped for the cutover; bring it back.
            self.restart_after_stop(branch.id, op)
                .context("the original container could not be started again")?;
        }
        Ok(())
    }

    /// What completing `op` needs to find: the volume the copy produced,
    /// whole and ours, holding the dump that was recorded (exactly when the
    /// copy had just ended; a destination that has started may have saved
    /// again, so then any non-empty dump will do). Anything else is not a
    /// copy that can be completed.
    fn forward_precondition(&self, op: &Op) -> Result<()> {
        let (volume, owner) = match (op.kind, op.step) {
            (OpKind::Clone, Step::Copied | Step::Starting) => {
                let branch = self.require_branch(op.target)?;
                (branch.volume, VolumeOwner::Branch(op.target))
            }
            (OpKind::Snapshot, Step::Copied) => {
                let snapshot = self
                    .snapshot(op.target)?
                    .context("the snapshot record vanished")?;
                (snapshot.volume, VolumeOwner::Snapshot(op.target))
            }
            _ => return Ok(()),
        };
        self.require_source_volume(&volume, owner)?;
        let size = self
            .volume_file_size(op.id, &volume, profile::DUMP_FILE)?
            .with_context(|| format!("volume {volume} holds no {}", profile::DUMP_FILE))?;
        anyhow::ensure!(
            size > 0,
            "the {} in volume {volume} is empty",
            profile::DUMP_FILE
        );
        if op.step == Step::Copied {
            let recorded = op
                .copied_size
                .context("the size of the copy was not recorded")?;
            anyhow::ensure!(
                size == recorded,
                "the {} in volume {volume} is {size} bytes and the copy recorded {recorded}",
                profile::DUMP_FILE
            );
        }
        Ok(())
    }

    /// Finish an operation that was meant to end.
    fn forward_op(&self, op: &Op) -> Result<()> {
        self.kill_helper(op)?;
        match (op.kind, op.step) {
            (OpKind::Clone, Step::Copied | Step::Starting) => {
                if op.step == Step::Copied {
                    self.step(op.id, Step::Starting, |_| Ok(()))?;
                }
                self.bring_up(op.target, true)?;
                self.update(|service| {
                    let branch = service
                        .branch_mut(op.target)
                        .context("the new branch vanished")?;
                    branch.state = RecordState::Ready;
                    branch.updated_at = Utc::now();
                    service
                        .op_mut(op.id)
                        .context("the journal entry vanished")?
                        .advance(Step::Done)
                })
            }
            (OpKind::Snapshot, Step::Copied) => self.update(|service| {
                let snapshot = service
                    .snapshot_mut(op.target)
                    .context("the snapshot vanished")?;
                snapshot.state = RecordState::Ready;
                service
                    .op_mut(op.id)
                    .context("the journal entry vanished")?
                    .advance(Step::Done)
            }),
            (OpKind::Reset, Step::Cleanup) => self.finish_reset_cleanup(op.id),
            (OpKind::Drop, _) => self.finish_drop(op.id),
            (OpKind::DeleteSnapshot, _) => self.finish_delete_snapshot(op.id),
            (kind, step) => bail!("no way to complete {kind:?} at {step:?}"),
        }
    }

    // ---- removals ----

    /// Drop a branch: its container and volume by recorded identity, then its
    /// record. Journaled first, so an interruption is finished by the next
    /// reconcile. The caller holds the branch's lock.
    pub(crate) fn drop_branch(&self, id: Uuid) -> Result<()> {
        let mut op = Op::new(OpKind::Drop, id);
        op.coordinator = Some(super::plan::coordinator_ref());
        let op_id = op.id;
        self.update(|service| {
            let branch = service
                .branch_mut(id)
                .with_context(|| format!("service '{}' has no branch {id}", self.key))?;
            branch.state = RecordState::Deleting;
            service.journal.push(op);
            Ok(())
        })?;
        self.finish_drop(op_id)
    }

    pub(super) fn finish_drop(&self, op_id: Uuid) -> Result<()> {
        let op = self.require_op(op_id)?;
        if op.step == Step::Planned {
            self.step(op_id, Step::Removing, |_| Ok(()))?;
        }
        if let Some(branch) = self.records()?.branch(op.target).cloned() {
            self.remove_recorded_container(
                branch.id,
                branch.container_id.as_deref(),
                &branch.container_name,
            )?;
            self.remove_recorded_volume(&branch.volume, VolumeOwner::Branch(branch.id), false)?;
        }
        self.update(|service| {
            service.branches.retain(|b| b.id != op.target);
            service
                .op_mut(op_id)
                .context("the journal entry vanished")?
                .advance(Step::Done)
        })
    }

    /// Delete one snapshot version.
    pub(crate) fn delete_snapshot(&self, id: Uuid) -> Result<()> {
        let mut op = Op::new(OpKind::DeleteSnapshot, id);
        op.coordinator = Some(super::plan::coordinator_ref());
        let op_id = op.id;
        self.update(|service| {
            let snapshot = service
                .snapshot_mut(id)
                .with_context(|| format!("service '{}' has no snapshot {id}", self.key))?;
            snapshot.state = RecordState::Deleting;
            service.journal.push(op);
            Ok(())
        })?;
        self.finish_delete_snapshot(op_id)
    }

    fn finish_delete_snapshot(&self, op_id: Uuid) -> Result<()> {
        let op = self.require_op(op_id)?;
        if op.step == Step::Planned {
            self.step(op_id, Step::Removing, |_| Ok(()))?;
        }
        if let Some(snapshot) = self.snapshot(op.target)? {
            // A snapshot of a pre-release build has no snapshot label: it is
            // removed by the catalog record, the project and the service.
            self.remove_recorded_volume(
                &snapshot.volume,
                VolumeOwner::Snapshot(snapshot.id),
                true,
            )?;
        }
        self.update(|service| {
            service.snapshots.retain(|s| s.id != op.target);
            service
                .op_mut(op_id)
                .context("the journal entry vanished")?
                .advance(Step::Done)
        })
    }

    // ---- reports ----

    /// Containers and volumes that carry this project's label and are not in
    /// the catalog (as branches, snapshots, helpers or reset plans).
    fn unrecorded(&self) -> Result<Vec<String>> {
        let project = self.project.to_string();
        let records = self.catalog_all()?;
        let mut known_containers: Vec<String> = Vec::new();
        let mut known_volumes: Vec<String> = Vec::new();
        for service in records.services.values() {
            for branch in &service.branches {
                known_containers.extend(branch.container_id.clone());
                known_containers.push(branch.container_name.clone());
                known_volumes.push(branch.volume.clone());
            }
            for snapshot in &service.snapshots {
                known_volumes.push(snapshot.volume.clone());
            }
            // An attempt to create a helper is recorded until it is resolved.
            for attempt in &service.helpers {
                known_containers.push(attempt.name.clone());
                known_containers.extend(attempt.id.clone());
            }
            for op in service.journal.iter().filter(|op| !op.step.is_terminal()) {
                known_containers.extend(op.helper_id.clone());
                known_containers.extend(op.helper_name.clone());
                known_containers.extend(op.verify_id.clone());
                known_containers.extend(op.verify_name.clone());
                known_containers.extend(op.boot_id.clone());
                known_containers.extend(op.boot_name.clone());
                if let Some(plan) = &op.reset {
                    known_containers.extend(plan.staging_id.clone());
                    known_containers.extend(plan.container_id.clone());
                    known_containers.push(plan.staging_name.clone());
                    known_containers.push(plan.container_name.clone());
                    known_volumes.push(plan.volume.clone());
                    // After the cutover the catalog points at the new pair,
                    // and the old one is the plan's until its cleanup is done.
                    known_containers.extend(plan.old_container_id.clone());
                    known_volumes.extend(plan.old_volume.clone());
                }
            }
        }
        let mut found = Vec::new();
        for info in docker::list_project_containers(&project)? {
            if !known_containers.contains(&info.id) && !known_containers.contains(&info.name) {
                found.push(format!(
                    "container {} ({}) carries this project's label but is not in the catalog",
                    info.name,
                    &info.id[..12.min(info.id.len())]
                ));
            }
        }
        for volume in docker::list_project_volumes(&project)? {
            if !known_volumes.contains(&volume) {
                found.push(format!(
                    "volume {volume} carries this project's label but is not in the catalog"
                ));
            }
        }
        Ok(found)
    }

    fn catalog_all(&self) -> Result<super::catalog::Catalog> {
        super::catalog::Store::open(self.project, self.docker_id())?.load()
    }

    /// Record `branch` as unusable and aborted (used when a creation cannot
    /// be completed and nothing else exists).
    #[allow(dead_code)]
    pub(super) fn mark_aborted(&self, branch: &BranchRecord) -> Result<()> {
        self.update(|service| {
            if let Some(record) = service.branch_mut(branch.id) {
                record.state = RecordState::Aborted;
            }
            Ok(())
        })
    }
}
