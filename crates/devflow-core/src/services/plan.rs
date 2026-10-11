//! Journaling an operation, and handing it to a coordinator.
//!
//! The caller of a clone, snapshot or reset only plans it: the catalog gets
//! the destination record and the journal entry (names, port-to-be, the
//! credentials, whether a full copy is allowed) before any Docker resource
//! exists. Then it starts the coordinator (`devflow __clone`, its own
//! process group), waits for its handshake (it holds the source's lock
//! by then) and for its end, and reads the outcome from the catalog.
use super::catalog::{
    BranchRecord, Coordinator as CoordinatorRef, MysqlIdentity, Op, OpKind, RecordState, ResetPlan,
    SnapshotRecord, SourceRef, Step,
};
use super::{Service, coordinator_program, docker, profile};
use crate::config::Engine;
use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// How long the coordinator may take to hold its locks and say so.
const HANDSHAKE: Duration = Duration::from_secs(90);
/// How long past the copy deadline the caller waits for the coordinator.
const WAIT_SLACK: Duration = Duration::from_secs(900);
/// Default time a copy may take before its helper is killed.
const COPY_TIMEOUT_SECS: u64 = 600;

/// A branch to create.
#[derive(Debug, Clone)]
pub struct NewBranch {
    /// The identity to create it under, when the caller recorded one first
    /// (a workspace receipt's `creating`); a new one otherwise.
    pub id: Option<Uuid>,
    /// Display name (the workspace).
    pub name: String,
    /// The workspace instance that may reset and drop it.
    pub owner: Option<Uuid>,
    pub source: SourceRef,
}

impl Service {
    /// Seconds a copy may take: 10 minutes, or `DEVFLOW_TEST_COPY_TIMEOUT_SECS`
    /// (read here, journaled, so a coordinator uses what the planner decided).
    fn copy_timeout_secs() -> u64 {
        std::env::var("DEVFLOW_TEST_COPY_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|secs| *secs > 0)
            .unwrap_or(COPY_TIMEOUT_SECS)
    }

    /// Seconds a MySQL source may take to stop cleanly: 5 minutes, or
    /// `DEVFLOW_TEST_STOP_TIMEOUT_SECS` (0 included, which kills at once; read
    /// here and journaled, like the copy timeout).
    fn stop_timeout_secs() -> u64 {
        std::env::var("DEVFLOW_TEST_STOP_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(super::catalog::DEFAULT_STOP_TIMEOUT_SECS)
    }

    /// The MySQL schema and account a branch made from `source` has: the
    /// source's own (they are in its data), or, for an empty root, the
    /// configured database and the application account.
    fn mysql_identity_for(
        &self,
        source: SourceRef,
        records: &super::catalog::ServiceCatalog,
    ) -> Result<Option<MysqlIdentity>> {
        if self.engine != Engine::Mysql {
            return Ok(None);
        }
        let found = match source {
            SourceRef::Empty => Some(MysqlIdentity {
                database: self.section().mysql_database(&self.project_name),
                user: super::mysql::APP_USER.to_string(),
            }),
            SourceRef::Branch { id } => records.branch(id).and_then(|b| b.mysql.clone()),
            SourceRef::Snapshot { id } => records.snapshot(id).and_then(|s| s.mysql.clone()),
        };
        Ok(Some(found.with_context(|| {
            format!(
                "the source of service '{}' records no database and account; it was not made by \
                 a MySQL profile of devflow",
                self.key
            )
        })?))
    }

    /// The pinned image of a source, resolved before anything is journaled:
    /// the source's own for a branch or snapshot (the clone boots the version
    /// that wrote the data), the configured image's current digest for an
    /// empty root.
    fn pin_for(
        &self,
        source: SourceRef,
        records: &super::catalog::ServiceCatalog,
    ) -> Result<(String, String)> {
        match source {
            SourceRef::Empty => {
                let image = self.section().image(self.engine);
                let pin = docker::pin_image(&image)?;
                Ok((image, pin.reference))
            }
            SourceRef::Branch { id } => {
                let branch = records.branch(id).with_context(|| {
                    format!("service '{}' has no branch {id} to clone", self.key)
                })?;
                ensure!(
                    branch.state == RecordState::Ready,
                    "branch {id} of service '{}' is not ready ({:?})",
                    self.key,
                    branch.state
                );
                Ok((branch.image.clone(), branch.pin.clone()))
            }
            SourceRef::Snapshot { id } => {
                let snapshot = records.snapshot(id).with_context(|| {
                    format!("service '{}' has no snapshot {id} to clone", self.key)
                })?;
                ensure!(
                    snapshot.state == RecordState::Ready,
                    "snapshot {}@v{} of service '{}' is not ready ({:?})",
                    snapshot.name,
                    snapshot.version,
                    self.key,
                    snapshot.state
                );
                Ok((snapshot.image.clone(), snapshot.pin.clone()))
            }
        }
    }

    /// Create a branch: an empty root, or a clone of a branch or snapshot.
    /// Returns the ready record. A branch that already exists under `id` is
    /// finished or rolled back first, and returned if it ended up ready.
    pub fn create_branch(&self, request: NewBranch) -> Result<BranchRecord> {
        let id = request.id.unwrap_or_else(Uuid::new_v4);
        let mut touched = vec![id];
        touched.extend(request.source.id());
        self.recover_touching(&touched)?;
        let records = self.records()?;
        if let Some(existing) = records.branch(id) {
            ensure!(
                existing.state == RecordState::Ready,
                "branch {id} of service '{}' is {:?} and cannot be completed; create a new one",
                self.key,
                existing.state
            );
            return Ok(existing.clone());
        }
        let (image, pin) = self.pin_for(request.source, &records)?;
        let mysql = self.mysql_identity_for(request.source, &records)?;
        let generation = 1;
        let kind = match request.source {
            SourceRef::Empty => OpKind::CreateRoot,
            _ => OpKind::Clone,
        };
        let now = Utc::now();
        let record = BranchRecord {
            id,
            name: request.name,
            owner: request.owner,
            state: RecordState::Creating,
            container_name: self.container_name(id, generation),
            container_id: None,
            volume: self.volume_name(id, generation),
            image,
            pin,
            password: profile::new_password(),
            // Its own root login too, never the source's: both are set in
            // the clone before it listens anywhere.
            root_password: mysql.is_some().then(profile::new_password),
            mysql,
            port: None,
            pending_port: None,
            domain: self.orbstack().then(|| self.domain_name(id)),
            source: request.source,
            captured_at: None,
            capture_exact: true,
            capture_last_save: false,
            clone: None,
            allocated_bytes: None,
            downtime_ms: None,
            stop: None,
            generation,
            created_at: now,
            updated_at: now,
        };
        let mut op = Op::new(kind, id);
        op.source = Some(request.source);
        // The decision to allow a full copy is made here, before the source
        // is touched; the coordinator reads it back and never decides.
        op.copy_allowed = !self.section().require_cow;
        op.copy_timeout_secs = Self::copy_timeout_secs();
        op.stop_timeout_secs = Self::stop_timeout_secs();
        op.helper_name = Some(self.helper_name(op.id));
        op.boot_name = (self.engine == Engine::Mysql).then(|| self.boot_name(id, generation));
        let op_id = op.id;
        self.update_planning(|service| {
            ensure!(
                service.branch(id).is_none(),
                "branch {id} appeared meanwhile"
            );
            service.branches.push(record);
            service.journal.push(op);
            Ok(())
        })?;
        self.run_op(op_id)?;
        let done = self.require_branch(id)?;
        ensure!(
            done.state == RecordState::Ready,
            "branch {id} of service '{}' did not become ready",
            self.key
        );
        Ok(done)
    }

    /// Snapshot a branch as the next version of `name`.
    pub fn snapshot_branch(&self, branch: Uuid, name: &str) -> Result<SnapshotRecord> {
        crate::db::validate_snapshot_name(name)?;
        self.recover_touching(&[branch])?;
        let id = Uuid::new_v4();
        let mut op = Op::new(OpKind::Snapshot, id);
        op.source = Some(SourceRef::Branch { id: branch });
        op.copy_allowed = !self.section().require_cow;
        op.copy_timeout_secs = Self::copy_timeout_secs();
        op.stop_timeout_secs = Self::stop_timeout_secs();
        op.helper_name = Some(self.helper_name(op.id));
        let op_id = op.id;
        // The source is read, and the version taken, in the same locked
        // update that records the snapshot: two snapshots of one name cannot
        // be given the same number, and a number that was deleted is never
        // given again.
        self.update_planning(|service| {
            let source = service
                .branch(branch)
                .with_context(|| format!("service '{}' has no branch {branch}", self.key))?;
            ensure!(
                source.state == RecordState::Ready,
                "branch {branch} is not ready ({:?})",
                source.state
            );
            let (image, pin) = (source.image.clone(), source.pin.clone());
            let mysql = source.mysql.clone();
            let version = service.allocate_version(name);
            service.snapshots.push(SnapshotRecord {
                id,
                name: name.to_string(),
                version,
                state: RecordState::Creating,
                volume: self.snapshot_volume_name(id),
                source: SourceRef::Branch { id: branch },
                image,
                pin,
                mysql,
                captured_at: None,
                capture_exact: true,
                capture_last_save: false,
                clone: None,
                allocated_bytes: None,
                downtime_ms: None,
                created_at: Utc::now(),
            });
            service.journal.push(op);
            Ok(())
        })?;
        self.run_op(op_id)?;
        self.snapshot(id)?
            .filter(|s| s.state == RecordState::Ready)
            .with_context(|| format!("snapshot {name} did not become ready"))
    }

    /// Replace a branch's data with a fresh clone of `to`, through a staged
    /// replacement volume: started and checked first, then swapped in by a
    /// journaled cutover. The original stays until the replacement is proven.
    pub fn reset_branch(&self, branch: Uuid, to: SourceRef) -> Result<BranchRecord> {
        let mut touched = vec![branch];
        touched.extend(to.id());
        self.recover_touching(&touched)?;
        ensure!(
            to != SourceRef::Branch { id: branch },
            "a branch cannot be reset from itself"
        );
        let records = self.records()?;
        let current = records
            .branch(branch)
            .with_context(|| format!("service '{}' has no branch {branch}", self.key))?;
        ensure!(
            current.state == RecordState::Ready,
            "branch {branch} is not ready ({:?})",
            current.state
        );
        let (image, pin) = match to {
            // An empty reset keeps the image the branch runs.
            SourceRef::Empty => (current.image.clone(), current.pin.clone()),
            other => self.pin_for(other, &records)?,
        };
        // A reset keeps the branch's URL, so the database and account in the
        // data it receives must be the ones that URL names.
        if self.engine == Engine::Mysql && to != SourceRef::Empty {
            let incoming = self.mysql_identity_for(to, &records)?;
            ensure!(
                incoming == current.mysql,
                "the data to reset from has database '{}' and the branch's URL names '{}'; a \
                 reset keeps the URL, so reset from data with the same database or drop the \
                 branch and create it again",
                incoming.as_ref().map_or("?", |i| i.database.as_str()),
                current.mysql.as_ref().map_or("?", |i| i.database.as_str())
            );
        }
        let generation = current.generation + 1;
        let mut op = Op::new(OpKind::Reset, branch);
        // The op's id is part of the names: a rolled-back reset leaves no
        // volume or container that a later one could mistake for its own.
        let suffix = super::short(&op.id, 6);
        let container_name = format!("{}-{suffix}", self.container_name(branch, generation));
        op.source = Some(to);
        op.copy_allowed = !self.section().require_cow;
        op.copy_timeout_secs = Self::copy_timeout_secs();
        op.stop_timeout_secs = Self::stop_timeout_secs();
        op.helper_name = Some(self.helper_name(op.id));
        op.reset = Some(ResetPlan {
            volume: format!("{}-{suffix}", self.volume_name(branch, generation)),
            staging_name: format!("{container_name}-stage"),
            container_name,
            image,
            pin,
            staging_id: None,
            container_id: None,
            old_container_id: None,
            old_volume: None,
            old_was_running: false,
            captured_at: None,
            capture_exact: true,
            capture_last_save: false,
            clone: None,
            allocated_bytes: None,
            downtime_ms: None,
        });
        let op_id = op.id;
        self.update_planning(|service| {
            service.journal.push(op);
            Ok(())
        })?;
        self.run_op(op_id)?;
        self.require_branch(branch)
    }

    /// Start the coordinator for journaled operation `op_id` and wait for it.
    fn run_op(&self, op_id: Uuid) -> Result<()> {
        let program = coordinator_program()?;
        let log_path = self.op_log_path(op_id)?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("cannot open {}", log_path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&log_path, std::fs::Permissions::from_mode(0o600));
        }
        let mut command = Command::new(&program);
        command
            .arg("__clone")
            .arg(self.project.to_string())
            .arg(&self.key)
            .arg(op_id.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(log);
        // Its own process group: a ^C or a SIGHUP that ends the caller does
        // not end an operation half way through a clone.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("cannot start {}", program.display()))?;
        let stdout = child.stdout.take().context("no coordinator output")?;
        let (lines, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
                if lines.send(line).is_err() {
                    break;
                }
            }
        });

        // Handshake: the coordinator holds the locks when it says `ready`.
        match rx.recv_timeout(HANDSHAKE) {
            Ok(line) if line == "ready" => {}
            other => {
                let said = match other {
                    Ok(line) => line,
                    Err(_) => "did not answer".to_string(),
                };
                let _ = child.kill();
                let _ = child.wait();
                let detail = self.log_tail(&log_path);
                self.abort_unstarted(op_id, &said);
                bail!(
                    "the clone coordinator did not start ({said}){}",
                    if detail.is_empty() {
                        String::new()
                    } else {
                        format!(": {detail}")
                    }
                );
            }
        }

        let copy_timeout = self
            .records()?
            .op(op_id)
            .map_or(COPY_TIMEOUT_SECS, |op| op.copy_timeout_secs);
        let limit = Instant::now() + Duration::from_secs(copy_timeout) + WAIT_SLACK;
        let mut last = String::new();
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => last = line,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if Instant::now() >= limit {
                        bail!(
                            "the operation is still running after {}; it continues on \
                             its own. Check `devflow service {} reconcile` later",
                            crate::text::counted(copy_timeout + WAIT_SLACK.as_secs(), "second"),
                            self.key
                        );
                    }
                }
            }
        }
        let _ = child.wait();
        let records = self.records()?;
        let op = records
            .op(op_id)
            .context("the operation vanished from the journal")?;
        match op.step {
            Step::Done => Ok(()),
            _ => bail!(
                "{}",
                op.error
                    .clone()
                    .or_else(|| last.strip_prefix("error: ").map(str::to_string))
                    .unwrap_or_else(|| "the operation did not finish".to_string())
            ),
        }
    }

    fn op_log_path(&self, op_id: Uuid) -> Result<std::path::PathBuf> {
        let dir = self.catalog_path().with_extension("logs");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        Ok(dir.join(format!("{op_id}.log")))
    }

    fn log_tail(&self, path: &std::path::Path) -> String {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let lines: Vec<&str> = text.lines().rev().take(3).collect();
        crate::redact::text(&lines.into_iter().rev().collect::<Vec<_>>().join(" | "))
    }

    /// The coordinator never started, so nothing exists but the plan: end the
    /// operation and its destination record. A coordinator that did start is
    /// left to finish or to be recovered.
    fn abort_unstarted(&self, op_id: Uuid, why: &str) {
        let _ = self.update(|service| {
            let Some(op) = service.op(op_id).cloned() else {
                return Ok(());
            };
            if op.coordinator.is_some() || op.step.is_terminal() {
                return Ok(());
            }
            if let Some(op) = service.op_mut(op_id) {
                op.error = Some(format!("the coordinator did not start: {why}"));
                op.step = Step::Aborted;
                op.updated_at = Utc::now();
            }
            if let Some(branch) = service.branch_mut(op.target)
                && branch.state == RecordState::Creating
            {
                branch.state = RecordState::Aborted;
            }
            if let Some(snapshot) = service.snapshot_mut(op.target)
                && snapshot.state == RecordState::Creating
            {
                snapshot.state = RecordState::Aborted;
            }
            Ok(())
        });
    }
}

/// What a coordinator recorded about itself.
pub(super) fn coordinator_ref() -> CoordinatorRef {
    CoordinatorRef {
        pid: std::process::id(),
        started_at: Utc::now(),
    }
}
