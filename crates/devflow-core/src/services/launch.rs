//! Starting and stopping a branch's container: the volume, the container,
//! the host port and the authenticated health check.
//!
//! The port follows the endpoint rules (D3): allocated once from
//! `hash(project, service key, branch id)`, journaled in the catalog before
//! Docker is asked to publish it, decided by Docker's own bind, persisted, and
//! never changed afterwards. A retained port that became unavailable is
//! reported with its cause; it is not replaced.
use super::catalog::{BranchRecord, RecordState, SourceRef};
use super::docker::{self, ContainerInfo, ContainerSpec, StartError};
use super::stop::StopSlot;
use super::{Endpoint, Service, mysql, profile};
use crate::config::Engine;
use crate::db::{AllocationError, BindFailure, allocate_port, ports_from_seed};
use anyhow::{Context, Result, bail};
use std::cell::RefCell;
use std::net::TcpListener;
use std::time::Duration;
use uuid::Uuid;

/// How long a started server may take to answer `AUTH` and `PING` (a big
/// dataset loads before it does).
const HEALTH_TIMEOUT: Duration = Duration::from_secs(60);
/// The same for MySQL, whose InnoDB may have a redo log to apply first.
const MYSQL_HEALTH_TIMEOUT: Duration = Duration::from_secs(180);
/// Grace for a stop: the server saves its dataset on `SIGTERM`.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
/// Grace for a MySQL stop outside a clone: a slow InnoDB shutdown purges and
/// merges everything first, and a container killed before it is done has to
/// recover when it starts again.
const MYSQL_STOP_TIMEOUT: Duration = Duration::from_secs(300);

/// What a container is created from.
pub(super) struct Launch {
    pub branch: Uuid,
    pub name: String,
    pub volume: String,
    pub pin: String,
    pub password: String,
    pub domain: Option<String>,
}

/// Whose data a volume holds: the identity its labels must repeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VolumeOwner {
    Branch(Uuid),
    Snapshot(Uuid),
}

impl VolumeOwner {
    fn label(self) -> (&'static str, String) {
        match self {
            VolumeOwner::Branch(id) => (docker::LABEL_BRANCH, id.to_string()),
            VolumeOwner::Snapshot(id) => (docker::LABEL_SNAPSHOT, id.to_string()),
        }
    }

    /// The labels the volume of this owner must carry. `legacy` accepts a
    /// snapshot volume that has no snapshot label (a pre-release build made
    /// it): for removing it only, never for copying from it.
    pub(super) fn ownership(self, project: &str, service: &str, legacy: bool) -> docker::Ownership {
        docker::Ownership {
            project: project.to_string(),
            service: service.to_string(),
            identity: self.label(),
            identity_may_be_missing: legacy && matches!(self, VolumeOwner::Snapshot(_)),
        }
    }
}

/// The labels a volume must carry before devflow uses it: this project, this
/// service and the branch or snapshot the catalog says it holds. The message
/// ends `what`, the use that is refused.
fn check_volume_labels(
    volume: &docker::VolumeInfo,
    project: &str,
    service: &str,
    owner: VolumeOwner,
    what: &str,
) -> Result<()> {
    let expected = owner.ownership(project, service, false);
    if let Some(mismatch) = expected.mismatch(&volume.labels) {
        bail!(
            "volume {} is recorded for service '{service}' but {mismatch}; devflow will not {what} it",
            volume.name
        );
    }
    Ok(())
}

impl Launch {
    pub(super) fn of(record: &BranchRecord) -> Launch {
        Launch {
            branch: record.id,
            name: record.container_name.clone(),
            volume: record.volume.clone(),
            pin: record.pin.clone(),
            password: record.password.clone(),
            domain: record.domain.clone(),
        }
    }
}

impl Service {
    /// The seeds of the port candidates of `branch`.
    pub fn candidate_ports(&self, branch: Uuid) -> Vec<u16> {
        ports_from_seed(&format!(
            "devflow.endpoint.v1\0{}\0{}\0{branch}",
            self.project, self.key
        ))
    }

    fn container_spec(&self, launch: &Launch, host_port: Option<u16>) -> Result<ContainerSpec> {
        let mut labels = self.labels(launch.branch);
        if let Some(domain) = &launch.domain {
            labels.push((docker::LABEL_ORBSTACK_DOMAINS.into(), domain.clone()));
        }
        let (container_port, data_dir, files, command) = match self.engine {
            Engine::Mysql => (
                mysql::CONTAINER_PORT,
                mysql::DATA_DIR,
                vec![(self.mysql_profile_file()?, mysql::PROFILE_MOUNT.to_string())],
                // The image's entrypoint finds the data directory in place,
                // so it neither initializes nor needs a password option.
                vec!["mysqld".to_string()],
            ),
            engine => (
                profile::CONTAINER_PORT,
                profile::DATA_DIR,
                Vec::new(),
                profile::command(engine, &launch.password),
            ),
        };
        Ok(ContainerSpec {
            name: launch.name.clone(),
            image: launch.pin.clone(),
            volume: launch.volume.clone(),
            data_dir: data_dir.to_string(),
            host_port,
            container_port,
            isolated: false,
            files,
            labels,
            command,
        })
    }

    /// The volume exists (created with the project, service and `owner`
    /// labels if it did not), and an existing one is the `owner`'s: a volume
    /// that merely has the name is never written into.
    pub(super) fn ensure_volume(&self, name: &str, owner: Option<VolumeOwner>) -> Result<()> {
        match docker::inspect_volume(name)? {
            Some(volume) => {
                match owner {
                    Some(owner) => self.require_owned_volume(&volume, owner, "write into")?,
                    None => self.corroborate_volume(&volume)?,
                }
                Ok(())
            }
            None => {
                let mut labels = vec![
                    (docker::LABEL_PROJECT.to_string(), self.project.to_string()),
                    (docker::LABEL_SERVICE.to_string(), self.key.clone()),
                ];
                if let Some(owner) = owner {
                    let (key, value) = owner.label();
                    labels.push((key.to_string(), value));
                }
                docker::create_volume(name, &labels)
            }
        }
    }

    /// The volume a copy reads from is the one the catalog records for
    /// `owner`: it exists (mounting a missing one would create it empty, and
    /// an empty dataset must never be taken for the source's) and carries this
    /// project's, this service's and the owner's labels. Another volume that
    /// merely has the name is refused.
    pub(super) fn require_source_volume(&self, name: &str, owner: VolumeOwner) -> Result<()> {
        let volume = docker::inspect_volume(name)?.with_context(|| {
            format!(
                "volume {name} of service '{}' no longer exists, so its data cannot be used; \
                 devflow will not substitute an empty one",
                self.key
            )
        })?;
        // A snapshot taken by a pre-release build has this project's and this
        // service's labels but no snapshot label. It is not migrated (the
        // feature never shipped), and it is refused in words that say what to
        // do, not as a label mismatch.
        if let VolumeOwner::Snapshot(id) = owner {
            let expected = self.volume_ownership(owner, false);
            if let Some(mismatch) = expected.mismatch(&volume.labels)
                && expected.lacks_identity(&mismatch)
            {
                let which = match self.snapshot(id) {
                    Ok(Some(snapshot)) => format!("{}@v{}", snapshot.name, snapshot.version),
                    _ => id.to_string(),
                };
                bail!(
                    "snapshot {which} (volume {name}) was made by a pre-release build without an \
                     ownership label (`{}`), so devflow will not copy from it. Delete the snapshot \
                     and take a new one",
                    docker::LABEL_SNAPSHOT,
                );
            }
        }
        self.require_owned_volume(&volume, owner, "copy from")
    }

    /// `volume` carries the project, service and `owner` labels the catalog
    /// records; otherwise devflow will not `what` it.
    pub(super) fn require_owned_volume(
        &self,
        volume: &docker::VolumeInfo,
        owner: VolumeOwner,
        what: &str,
    ) -> Result<()> {
        check_volume_labels(volume, &self.project.to_string(), &self.key, owner, what)
    }

    /// The labels the volume `owner` holds must carry, for removing it.
    /// `legacy` also accepts a snapshot volume without its snapshot label.
    pub(super) fn volume_ownership(&self, owner: VolumeOwner, legacy: bool) -> docker::Ownership {
        owner.ownership(&self.project.to_string(), &self.key, legacy)
    }

    /// The labels of the containers made for `branch`.
    pub(super) fn container_ownership(&self, branch: Uuid) -> docker::Ownership {
        docker::Ownership {
            project: self.project.to_string(),
            service: self.key.clone(),
            identity: (docker::LABEL_BRANCH, branch.to_string()),
            identity_may_be_missing: false,
        }
    }

    /// The labels of the copy and verify helpers of operation `op`.
    pub(super) fn helper_ownership(&self, op: Uuid) -> docker::Ownership {
        docker::Ownership {
            project: self.project.to_string(),
            service: self.key.clone(),
            identity: (docker::LABEL_HELPER, op.to_string()),
            identity_may_be_missing: false,
        }
    }

    /// A volume the catalog names must carry this project's label before
    /// devflow uses it, when no finer identity is known.
    pub(super) fn corroborate_volume(&self, volume: &docker::VolumeInfo) -> Result<()> {
        let project = self.project.to_string();
        anyhow::ensure!(
            volume.labels.get(docker::LABEL_PROJECT) == Some(&project),
            "volume {} is recorded for service '{}' but does not carry this project's label; \
             devflow will not use it",
            volume.name,
            self.key
        );
        Ok(())
    }

    /// A container the catalog names must carry the labels of this branch.
    pub(super) fn corroborate_container(&self, info: &ContainerInfo, branch: Uuid) -> Result<()> {
        let get = |key: &str| info.labels.get(key).map(String::as_str);
        anyhow::ensure!(
            get(docker::LABEL_PROJECT) == Some(self.project.to_string().as_str())
                && get(docker::LABEL_SERVICE) == Some(self.key.as_str())
                && get(docker::LABEL_BRANCH) == Some(branch.to_string().as_str()),
            "container {} is recorded for branch {branch} but its labels say otherwise; devflow \
             will not touch it",
            info.name
        );
        Ok(())
    }

    /// The branch's container as Docker has it: by recorded id, else by the
    /// name chosen for it (a creation interrupted before the id was saved),
    /// and only if its labels agree with the record.
    pub(super) fn find_container(&self, record: &BranchRecord) -> Result<Option<ContainerInfo>> {
        let by_id = match record.container_id.as_deref() {
            Some(id) => docker::inspect_container(id)?,
            None => None,
        };
        let info = match by_id {
            Some(info) => info,
            // The recorded id is gone (or was never saved): the name chosen
            // for the branch may still be its container, if the labels say so.
            // A container that only has the name is someone else's and is
            // refused, never adopted or removed.
            None => match docker::inspect_container(&record.container_name)? {
                Some(info) => info,
                None => return Ok(None),
            },
        };
        self.corroborate_container(&info, record.id)?;
        Ok(Some(info))
    }

    /// Something other than Docker already listens on `port`. OrbStack
    /// publishes a port even where a host process listens on it, and a
    /// client would then talk to that process.
    pub(super) fn host_conflict(&self, port: u16) -> Option<String> {
        match TcpListener::bind(("127.0.0.1", port)) {
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => Some(format!(
                "another process already listens on 127.0.0.1:{port} ({error})"
            )),
            _ => None,
        }
    }

    fn port_unavailable(&self, branch: Uuid, port: u16, cause: &str) -> anyhow::Error {
        anyhow::anyhow!(
            "host port {port} is not available for branch {branch} of service '{}': {cause}. \
             devflow keeps the port it allocated (recorded in {}): stop what uses it and run the \
             command again",
            self.key,
            self.catalog_path().display()
        )
    }

    /// Create and start a container for `launch`, on `retained` when the
    /// branch has a port, else on the first free candidate. `record_id`
    /// saves (or clears, with `None`) the container id in the catalog: the id
    /// is saved before the container starts. Returns the container id and
    /// the port Docker bound.
    pub(super) fn launch_container(
        &self,
        launch: &Launch,
        retained: Option<u16>,
        pending: Option<u16>,
        record_id: &mut dyn FnMut(Option<String>) -> Result<()>,
    ) -> Result<(String, u16)> {
        let created: RefCell<Option<String>> = RefCell::new(None);
        let record = RefCell::new(record_id);
        // One attempt on one port: a fresh container, because the published
        // port is fixed when a container is created.
        let attempt = |port: u16| -> Result<()> {
            if let Some(conflict) = self.host_conflict(port) {
                return Err(BindFailure(conflict).into());
            }
            let id = docker::create_container(&self.container_spec(launch, Some(port))?)?;
            (record.borrow_mut())(Some(id.clone()))?;
            *created.borrow_mut() = Some(id.clone());
            match docker::start_container(&id) {
                Ok(()) => Ok(()),
                Err(StartError::Bind(why)) => {
                    // Nothing was bound: remove the container, forget its id.
                    docker::remove_container(&id)?;
                    (record.borrow_mut())(None)?;
                    *created.borrow_mut() = None;
                    Err(BindFailure(why).into())
                }
                Err(StartError::Other(error)) => Err(error),
            }
        };
        if let Some(port) = retained {
            return match attempt(port) {
                Ok(()) => Ok((created.take().context("no container was created")?, port)),
                Err(error) => match error.downcast::<BindFailure>() {
                    Ok(failure) => Err(self.port_unavailable(launch.branch, port, &failure.0)),
                    Err(error) => Err(error),
                },
            };
        }
        let candidates = self.candidate_ports(launch.branch);
        let result = allocate_port(
            &candidates,
            pending,
            None,
            &mut |pending, port| {
                self.update(|service| {
                    if let Some(branch) = service.branch_mut(launch.branch) {
                        branch.pending_port = pending;
                        if port.is_some() {
                            branch.port = port;
                        }
                    }
                    Ok(())
                })
            },
            &mut |port| attempt(port),
        );
        match result {
            Ok(port) => Ok((created.take().context("no container was created")?, port)),
            Err(AllocationError::Exhausted(refused)) => bail!(
                "no host port was available for branch {} of service '{}'; tried {}. Free one of \
                 them and run the command again",
                launch.branch,
                self.key,
                refused.join("; ")
            ),
            Err(AllocationError::Other(error)) => Err(error),
        }
    }

    /// Make the branch's container exist, run and answer `AUTH`/`PING`:
    /// the container is created on the retained or newly allocated port if
    /// it is gone, and started if stopped. `creating` allows a branch that is
    /// not `ready` yet (the coordinator's own creation). Only an empty root
    /// has its volume made when missing: any other branch's volume holds data
    /// (copied, or lived in), and a missing one is data lost, never a reason
    /// to start an empty server in its place. The caller holds the branch's
    /// lock.
    pub(crate) fn bring_up(&self, id: Uuid, creating: bool) -> Result<Endpoint> {
        let record = self.require_branch(id)?;
        anyhow::ensure!(
            record.state == RecordState::Ready
                || (creating && record.state == RecordState::Creating),
            "branch {id} of service '{}' is {:?}, not ready",
            self.key,
            record.state
        );
        if creating && record.source == SourceRef::Empty {
            self.ensure_volume(&record.volume, Some(VolumeOwner::Branch(id)))?;
        } else {
            let volume = docker::inspect_volume(&record.volume)?.ok_or_else(|| {
                let problem = crate::remedy::Problem::new(format!(
                    "volume {} of branch {id} no longer exists; its data is gone. Drop the \
                     branch and set the workspace up again",
                    record.volume
                ));
                if self.root.as_os_str().is_empty() {
                    return problem.into_error();
                }
                problem
                    .with(crate::remedy::Remedy::DropServiceBranch {
                        root: self.root.clone(),
                        key: self.key.clone(),
                        branch: id,
                    })
                    .into_error()
            })?;
            // A container is started on this volume only if the catalog says
            // it holds this branch's data, not a namesake of the project.
            self.require_owned_volume(&volume, VolumeOwner::Branch(id), "start a server on")?;
        }
        // A stop of this branch that was asked for and never confirmed (an
        // ordinary `down` that timed out, or whose caller died) may still be
        // carried out by the daemon: the container it names is stopped, removed
        // by its id and confirmed gone, and the container below is created
        // again on the same volume, name, port and labels.
        let record = if self.settle_branch_stop(id)?.fenced {
            if let Some(port) = record.port {
                self.wait_port_released(port);
            }
            self.require_branch(id)?
        } else {
            record
        };
        let existing = self.find_container(&record)?;
        let record_id = |cid: Option<String>| {
            self.update(|service| {
                if let Some(branch) = service.branch_mut(id) {
                    branch.container_id = cid;
                }
                Ok(())
            })
        };
        let port = match existing {
            Some(info) if info.running => {
                if record.container_id.as_deref() != Some(info.id.as_str()) {
                    record_id(Some(info.id.clone()))?;
                }
                let published = info
                    .published
                    .context("the running container publishes no port")?;
                // The port is allocated once and kept: a container that
                // publishes another one is reported, not adopted.
                if let Some(recorded) = record.port
                    && recorded != published
                {
                    bail!(
                        "container {} publishes 127.0.0.1:{published} but branch {id} was \
                         allocated port {recorded}; devflow does not move a recorded port. \
                         Remove the container (`docker rm -f {}`) and run the command again to \
                         recreate it on {recorded}",
                        info.name,
                        info.name
                    );
                }
                published
            }
            Some(info) => {
                let port = record.port.or(info.configured);
                match port {
                    Some(port) => {
                        if record.container_id.as_deref() != Some(info.id.as_str()) {
                            record_id(Some(info.id.clone()))?;
                        }
                        // The same container, the same port: only start it.
                        if let Some(conflict) = self.host_conflict(port) {
                            return Err(self.port_unavailable(id, port, &conflict));
                        }
                        match docker::start_container(&info.id) {
                            Ok(()) => port,
                            Err(StartError::Bind(why)) => {
                                return Err(self.port_unavailable(id, port, &why));
                            }
                            Err(StartError::Other(error)) => return Err(error),
                        }
                    }
                    None => {
                        // A container that was never given a port.
                        docker::remove_container(&info.id)?;
                        record_id(None)?;
                        self.launch_new(&record, &record_id)?
                    }
                }
            }
            None => {
                if record.container_id.is_some() {
                    // The recorded container vanished (removed by hand): it
                    // is created again from the record, on the same port.
                    record_id(None)?;
                }
                self.launch_new(&record, &record_id)?
            }
        };
        let container = self
            .find_container(&self.require_branch(id)?)?
            .context("the container vanished after it started")?;
        if let Err(error) = self.wait_healthy(&container.id, &record, port, None) {
            let logs = docker::logs_tail(&container.id, 30);
            bail!("{error:#}\n{logs}");
        }
        self.update(|service| {
            if let Some(branch) = service.branch_mut(id) {
                branch.port = Some(port);
                branch.pending_port = None;
            }
            Ok(())
        })?;
        self.endpoint(&self.require_branch(id)?)
    }

    fn launch_new(
        &self,
        record: &BranchRecord,
        record_id: &dyn Fn(Option<String>) -> Result<()>,
    ) -> Result<u16> {
        let mut save = |cid: Option<String>| record_id(cid);
        let (_, port) = self.launch_container(
            &Launch::of(record),
            record.port,
            record.pending_port,
            &mut save,
        )?;
        Ok(port)
    }

    /// Wait until the container `container` of `record` answers, on `port`,
    /// as the engine's authenticated health check: `AUTH` and `PING`
    /// (Valkey, Redis), or a `SELECT 1` as the application account and the
    /// greeting on the published port (MySQL). `timeout` replaces the
    /// engine's default.
    pub(super) fn wait_healthy(
        &self,
        container: &str,
        record: &BranchRecord,
        port: u16,
        timeout: Option<Duration>,
    ) -> Result<()> {
        match self.engine {
            Engine::Mysql => mysql::health(
                container,
                port,
                &self.mysql_login(record)?,
                timeout.unwrap_or(MYSQL_HEALTH_TIMEOUT),
            ),
            _ => profile::health(port, &record.password, timeout.unwrap_or(HEALTH_TIMEOUT)),
        }
    }

    /// Stop the branch's container gracefully (the server saves its dataset;
    /// MySQL shuts InnoDB down, and the stop is verified). A container that
    /// is not running is left as it is. The stop is journaled in the branch's
    /// record before it is issued and cleared once it is confirmed; one that
    /// is not is fenced by the next start or stop (see `stop.rs`).
    pub(crate) fn stop_branch(&self, id: Uuid) -> Result<bool> {
        self.stop_branch_for(None, id)
    }

    /// [`stop_branch`](Self::stop_branch) as a step of operation `op`: the
    /// stop is journaled in the operation before it is issued, so that
    /// recovery can fence it if this process dies waiting for it.
    pub(crate) fn stop_branch_for(&self, op: Option<Uuid>, id: Uuid) -> Result<bool> {
        if op.is_none() {
            // An earlier ordinary stop that was never confirmed may still be
            // on its way to the container it names. It is fenced before
            // another is issued: the container is stopped by us, removed by
            // id and confirmed gone, which is what a stop asks for.
            let settled = self.settle_branch_stop(id)?;
            if settled.fenced {
                return Ok(settled.was_running);
            }
        }
        let record = self.require_branch(id)?;
        let Some(info) = self.find_container(&record)? else {
            return Ok(false);
        };
        if !info.running {
            return Ok(false);
        }
        let timeout = if self.engine == Engine::Mysql {
            MYSQL_STOP_TIMEOUT
        } else {
            STOP_TIMEOUT
        };
        match op {
            Some(op) => self.stop_journaled(op, &info, timeout)?,
            None => self.stop_journaled_in(StopSlot::Branch(id), &info, timeout)?,
        }
        if self.engine == Engine::Mysql {
            mysql::verify_stopped(&info.id)?;
        }
        Ok(true)
    }

    /// Whether the branch's container is running now.
    pub(crate) fn is_running(&self, record: &BranchRecord) -> Result<bool> {
        Ok(self
            .find_container(record)?
            .is_some_and(|info| info.running))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(labels: &[(&str, &str)]) -> docker::VolumeInfo {
        docker::VolumeInfo {
            name: "v".into(),
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn a_source_volume_must_repeat_the_catalog_identity() {
        let snapshot = Uuid::from_u128(5);
        let owner = VolumeOwner::Snapshot(snapshot);
        let full = volume(&[
            (docker::LABEL_PROJECT, "p"),
            (docker::LABEL_SERVICE, "cache"),
            (docker::LABEL_SNAPSHOT, &snapshot.to_string()),
        ]);
        assert!(check_volume_labels(&full, "p", "cache", owner, "copy from").is_ok());
        // An empty namesake that only carries a test label.
        let namesake = volume(&[(docker::LABEL_TEST, "x")]);
        assert!(check_volume_labels(&namesake, "p", "cache", owner, "copy from").is_err());
        // Another project's, another service's, another snapshot's.
        assert!(check_volume_labels(&full, "q", "cache", owner, "copy from").is_err());
        assert!(check_volume_labels(&full, "p", "queue", owner, "copy from").is_err());
        let other = VolumeOwner::Snapshot(Uuid::from_u128(6));
        assert!(check_volume_labels(&full, "p", "cache", other, "copy from").is_err());
        // A branch volume is not a snapshot volume.
        assert!(
            check_volume_labels(
                &full,
                "p",
                "cache",
                VolumeOwner::Branch(snapshot),
                "copy from"
            )
            .is_err()
        );
    }

    #[test]
    fn candidate_ports_follow_the_branch_identity() {
        let svc = crate::services::tests::service();
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        assert_eq!(svc.candidate_ports(a), svc.candidate_ports(a));
        assert_ne!(svc.candidate_ports(a)[0], svc.candidate_ports(b)[0]);
        assert_eq!(svc.candidate_ports(a).len(), 16);
        assert!(
            svc.candidate_ports(a)
                .iter()
                .all(|p| (20_000..40_000).contains(p))
        );
        // The service key is part of the identity.
        let mut other = svc.clone();
        other.key = "queue".into();
        assert_ne!(svc.candidate_ports(a)[0], other.candidate_ports(a)[0]);
    }
}
