//! A stop that is asked for, journaled, and fenced before anything is
//! restarted.
//!
//! A `docker stop` is a client of the daemon, and it can outlive the
//! coordinator that started it: the coordinator is killed, the client is in a
//! process group of its own, and the request may reach the daemon late (a slow
//! engine, a remote context, a stalled process). Docker's own timeout starts
//! only once it has the request, and it keeps carrying the stop out after the
//! client has gone. So no amount of waiting proves that a stop is over: the
//! delivery can come after any deadline computed from the time it was asked
//! for, and a source that recovery found running and healthy, restarted or
//! accepted, would then be taken down by a stop nobody is waiting for.
//!
//! What proves it is identity. Docker applies a stop to the container id it was
//! issued for and to no other. The request is recorded before it is issued
//! ([`StopRequest`], with the container's id), and whatever restarts a source
//! an operation stopped asks one question: did the client return success? If
//! it did, the daemon is done and the container is used as it is. If it did
//! not (it was never seen to return, or it failed), the stop may still be on
//! its way, and the container is **fenced** rather than reused:
//!
//! 1. the decision is journaled ([`Fence`]) before anything is done;
//! 2. the container is stopped gracefully by this recovery (its client is the
//!    one that is watched, so the server saves its dataset or shuts InnoDB
//!    down cleanly) unless it is already stopped;
//! 3. it is removed by its recorded id, which must still carry the branch's
//!    labels, and its absence is confirmed;
//! 4. a new container is created from the managed profile on the branch's
//!    retained volume, name, port, labels and endpoint, started, recorded in the
//!    catalog and health-checked (`bring_up`), and its id is noted in the
//!    journal.
//!
//! A stop that arrives afterwards names a container that no longer exists. A
//! crash between any two steps is finished by the next run: each step is
//! journaled before it is taken, and each is idempotent (a container that is
//! already gone is not an error and is not looked for under its old id).
//!
//! # Ordinary stops
//!
//! The stop of `devflow service down` belongs to no operation, and it has the
//! same hazard: it times out, or its caller dies while Docker still holds the
//! request, and a later `up` starts the very container the late stop then takes
//! down. So it is journaled too, in the branch's own record
//! (`BranchRecord::stop`) with its request time, before it is issued; a client
//! that returns success confirms it and clears the record. Anything else leaves
//! it, and the next start of the branch (`Service::bring_up`, hence `up`, a
//! workspace's setup and every recovery's restart) or the next ordinary stop
//! settles it first with the same fence: stop by us, remove by the recorded id,
//! confirm, and (for a start) recreate on the same volume, name, port and
//! labels. A record whose container is already gone is cleared. Records are
//! cleared only by proof, never by elapsed time.
use super::Service;
use super::catalog::{Fence, Op, ServiceCatalog, StopRequest};
use super::docker::{self, ContainerInfo};
use anyhow::{Context, Result};
use chrono::Utc;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// How long a removed container's published port is waited for before the
/// replacement is created on it.
const PORT_RELEASE: Duration = Duration::from_secs(15);

/// Where a stop request is journaled: in the operation that asked for it, or,
/// for an ordinary stop that belongs to no operation, in the branch's record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StopSlot {
    Op(Uuid),
    Branch(Uuid),
}

impl StopSlot {
    /// The slot's request, if its owner still exists.
    fn request(self, service: &mut ServiceCatalog) -> Option<&mut Option<StopRequest>> {
        match self {
            StopSlot::Op(id) => service.op_mut(id).map(|op| &mut op.stop),
            StopSlot::Branch(id) => service.branch_mut(id).map(|branch| &mut branch.stop),
        }
    }
}

/// What settling a branch's ordinary stop did.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Settled {
    /// The container the unconfirmed stop named was stopped, removed by id and
    /// confirmed gone.
    pub fenced: bool,
    /// It was running when the settling began.
    pub was_running: bool,
}

/// Whether the container that exists now (`current`, by id) is the one `stop`
/// was issued for and the stop is not known to have finished. Only a client
/// that returned success proves the daemon is done with a request; everything
/// else (a client nobody saw return, a failed one, one that ran past its bound)
/// leaves it possibly on its way.
pub(super) fn needs_fence(stop: &StopRequest, current: Option<&ContainerInfo>) -> bool {
    stop.outcome != Some(true) && current.is_some_and(|info| info.id == stop.container)
}

impl Service {
    /// Stop `info`'s container for operation `op_id`: the request is journaled
    /// before it is issued, and its outcome after, so a coordinator that dies
    /// in between leaves a record that says a stop may still arrive.
    pub(super) fn stop_journaled(
        &self,
        op_id: Uuid,
        info: &ContainerInfo,
        timeout: Duration,
    ) -> Result<()> {
        self.stop_journaled_in(StopSlot::Op(op_id), info, timeout)
    }

    /// [`stop_journaled`](Self::stop_journaled) into any slot. An operation's
    /// request keeps its outcome (recovery reads it); a branch's ordinary
    /// request is cleared once its client returned success, and left with a
    /// failed outcome otherwise.
    pub(super) fn stop_journaled_in(
        &self,
        slot: StopSlot,
        info: &ContainerInfo,
        timeout: Duration,
    ) -> Result<()> {
        let request = StopRequest {
            container: info.id.clone(),
            run_started_at: info.started_at.clone(),
            requested_at: Utc::now(),
            timeout_secs: timeout.as_secs(),
            coordinator_pid: std::process::id(),
            outcome: None,
            fence: None,
        };
        self.update(|service| {
            *slot
                .request(service)
                .context("the journal entry vanished")? = Some(request);
            Ok(())
        })?;
        let stopped = docker::stop_container(&info.id, timeout);
        // The outcome is noted either way: a failure is not a stop that is
        // known not to be in flight (the daemon may still be carrying it out).
        let ok = stopped.is_ok();
        self.update(|service| {
            if let Some(request) = slot.request(service) {
                match slot {
                    // Confirmed: the daemon answered the client.
                    StopSlot::Branch(_) if ok => *request = None,
                    _ => {
                        if let Some(stop) = request.as_mut() {
                            stop.outcome = Some(ok);
                        }
                    }
                }
            }
            Ok(())
        })?;
        stopped
    }

    /// Settle the ordinary stop of `branch` that was asked for and never
    /// confirmed, before the branch is started or stopped again: the container
    /// it names, if it still exists, is fenced (see the module comment), and
    /// the record is cleared once nothing it names can be stopped any more.
    pub(super) fn settle_branch_stop(&self, branch: Uuid) -> Result<Settled> {
        let Some(stop) = self.require_branch(branch)?.stop else {
            return Ok(Settled::default());
        };
        // By the id the request names, not by what the catalog points at now:
        // a late stop is applied to that id and to no other.
        let named = docker::inspect_container(&stop.container)?;
        let was_running = named.as_ref().is_some_and(|info| info.running);
        let fenced = needs_fence(&stop, named.as_ref());
        if fenced {
            self.fence(branch, StopSlot::Branch(branch), &stop)?;
        }
        self.update(|service| {
            if let Some(record) = service.branch_mut(branch)
                && record
                    .stop
                    .as_ref()
                    .is_some_and(|s| s.container == stop.container)
            {
                record.stop = None;
            }
            Ok(())
        })?;
        Ok(Settled {
            fenced,
            was_running,
        })
    }

    /// Start the branch's container again after an operation stopped it, and
    /// make sure it stays up. When the operation's stop may still be on its way
    /// (it was never known to have returned), the container it names is fenced
    /// and replaced first (see the module comment); without such a stop this
    /// is a plain start and check.
    pub(super) fn restart_after_stop(&self, branch: Uuid, op: &Op) -> Result<()> {
        // The journal as it is now: a fence an earlier run began is seen.
        let stop = self.require_op(op.id)?.stop;
        let record = self.require_branch(branch)?;
        let current = self.find_container(&record)?;
        let outstanding = stop
            .as_ref()
            .filter(|stop| needs_fence(stop, current.as_ref()));
        if let Some(stop) = outstanding {
            self.fence(branch, StopSlot::Op(op.id), stop)?;
            if let Some(port) = record.port {
                self.wait_port_released(port);
            }
        }
        self.bring_up(branch, false)?;
        if outstanding.is_some() {
            let replacement = self.require_branch(branch)?.container_id;
            self.update(|service| {
                if let Some(fence) = service
                    .op_mut(op.id)
                    .and_then(|op| op.stop.as_mut())
                    .and_then(|stop| stop.fence.as_mut())
                {
                    fence.replacement = replacement;
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    /// Steps 1 to 3 of the fence: the decision is journaled, the container is
    /// stopped by us unless it already is, and it is removed by its recorded
    /// id. A container that no longer carries the branch's labels is not ours
    /// to touch: it stays, and the recovery stops with the reason.
    fn fence(&self, branch: Uuid, slot: StopSlot, stop: &StopRequest) -> Result<()> {
        self.update(|service| {
            if let Some(request) = slot.request(service).and_then(Option::as_mut)
                && request.container == stop.container
                && request.fence.is_none()
            {
                request.fence = Some(Fence {
                    decided_at: Utc::now(),
                    replacement: None,
                });
            }
            Ok(())
        })?;
        let ours = self.container_ownership(branch);
        if let Some(info) = docker::inspect_container(&stop.container)? {
            if let Some(mismatch) = ours.mismatch(&info.labels) {
                return Err(docker::NotOwned {
                    resource: format!("container {}", info.name),
                    mismatch: mismatch.to_string(),
                }
                .into());
            }
            if info.running {
                // Our own stop, with our own client to watch: a server that is
                // asked to stop saves its dataset (or shuts InnoDB down)
                // before the container is removed.
                docker::stop_container(&stop.container, Duration::from_secs(stop.timeout_secs))
                    .context("the container of an outstanding stop could not be stopped")?;
            }
        }
        docker::remove_owned_container(&stop.container, Some(&stop.container), &ours)
            .context("the container of an outstanding stop could not be removed")?;
        Ok(())
    }

    /// Wait (bounded) until nothing listens on `port`: the removed container's
    /// published port is released a moment after the container is gone.
    pub(super) fn wait_port_released(&self, port: u16) {
        let until = Instant::now() + PORT_RELEASE;
        while self.host_conflict(port).is_some() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn info(id: &str, running: bool) -> ContainerInfo {
        ContainerInfo {
            id: id.into(),
            name: "n".into(),
            running,
            status: if running { "running" } else { "exited" }.into(),
            exit_code: 0,
            started_at: "2026-10-11T10:00:00.1Z".into(),
            finished_at: String::new(),
            labels: BTreeMap::new(),
            image_id: String::new(),
            published: None,
            configured: None,
        }
    }

    fn request(outcome: Option<bool>) -> StopRequest {
        StopRequest {
            container: "abc".into(),
            run_started_at: "2026-10-11T10:00:00.1Z".into(),
            requested_at: Utc::now(),
            timeout_secs: 300,
            coordinator_pid: 1,
            outcome,
            fence: None,
        }
    }

    #[test]
    fn only_a_client_that_returned_success_proves_the_stop_is_over() {
        let current = info("abc", true);
        // Never seen to return, or failed, or past its bound: still on its way,
        // whatever the container looks like and however long ago it was asked.
        for outcome in [None, Some(false)] {
            let mut stop = request(outcome);
            stop.requested_at = Utc::now() - chrono::Duration::days(1);
            assert!(needs_fence(&stop, Some(&current)), "{outcome:?}");
            assert!(
                needs_fence(&stop, Some(&info("abc", false))),
                "a stopped container is not proof either: it could be started again and \
                 stopped by the late request"
            );
        }
        // The daemon answered the client: nothing is in flight.
        assert!(!needs_fence(&request(Some(true)), Some(&current)));
    }

    #[test]
    fn a_slot_is_the_operations_request_or_the_branchs_own() {
        use super::super::catalog::{BranchRecord, Op, OpKind};
        let mut service = ServiceCatalog::new(crate::config::Engine::Valkey);
        let op = Op::new(OpKind::Clone, Uuid::from_u128(1));
        let op_id = op.id;
        service.journal.push(op);
        let now = Utc::now();
        let branch: BranchRecord = serde_json::from_value(serde_json::json!({
            "id": Uuid::from_u128(2), "name": "b", "state": "ready",
            "container_name": "c", "volume": "v", "image": "i", "pin": "p",
            "password": "pw", // kingfisher:ignore
            "source": { "kind": "empty" }, "generation": 1,
            "created_at": now, "updated_at": now,
        }))
        .unwrap();
        service.branches.push(branch);
        let branch_id = Uuid::from_u128(2);

        *StopSlot::Op(op_id).request(&mut service).unwrap() = Some(request(None));
        assert!(service.op(op_id).unwrap().stop.is_some());
        assert!(service.branch(branch_id).unwrap().stop.is_none());
        *StopSlot::Branch(branch_id).request(&mut service).unwrap() = Some(request(Some(false)));
        assert_eq!(
            service
                .branch(branch_id)
                .unwrap()
                .stop
                .as_ref()
                .unwrap()
                .outcome,
            Some(false)
        );
        // An owner that vanished has no slot.
        assert!(
            StopSlot::Branch(Uuid::from_u128(9))
                .request(&mut service)
                .is_none()
        );
        assert!(
            StopSlot::Op(Uuid::from_u128(9))
                .request(&mut service)
                .is_none()
        );
    }

    #[test]
    fn a_container_other_than_the_one_stop_names_needs_no_fence() {
        // A late stop names its container by id: a replacement has another.
        let stop = request(None);
        assert!(!needs_fence(&stop, Some(&info("def", true))));
        assert!(!needs_fence(&stop, None));
    }
}
