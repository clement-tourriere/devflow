//! The coordinator: runs one journaled operation from its first step to its
//! end, holding the locks of everything the operation touches.
//!
//! `devflow __clone <project> <service> <op>` is this function. It is started
//! by whoever planned the operation, takes the source's and the destination's
//! locks itself (so no other clone or reset of them can start, even if the
//! caller dies), records its pid and says `ready` for the caller's handshake.
//! It never trusts its caller for a decision: whether a full copy is allowed,
//! the destination names and the credentials were journaled before it started.
//!
//! The clone protocol for Valkey and Redis:
//!
//! 1. quiesce the source: `BGSAVE`, then wait until `INFO persistence` says
//!    no save is running, the last one was `ok`, `rdb_saves` advanced and
//!    `run_id` is unchanged (a source that is stopped has its last dump). A
//!    running source that reports no change since its last save, no save
//!    running, that save `ok` and has a `dump.rdb` is not saved again: the
//!    existing file is the dataset, and is cloned with the capture recorded as
//!    the last save (see `profile::reusable_last_save`);
//! 2. copy `dump.rdb` into the destination volume inside the Docker VM, with
//!    GNU `cp -a --reflink=always` in a helper container whose identity is
//!    recorded before it starts, under a deadline;
//! 3. a reflink that fails aborts the operation when copies are not allowed,
//!    and otherwise a full copy is made and reported as `copy`;
//! 4. start the destination (its own new `requirepass`, its own port) and
//!    check it with `AUTH` and `PING` before it is `ready`.
//!
//! The source is never paused and never stopped.
//!
//! The clone protocol for MySQL differs in the one thing the engine forces:
//!
//! 1. quiesce the source by stopping it cleanly (`docker stop` with a generous
//!    timeout), then verify the stop positively: exit code 0 and InnoDB's own
//!    shutdown report in the log since it started. A stop that ended in
//!    `SIGKILL` aborts the clone and the source is started again;
//! 2. copy the whole data directory inside the VM, as above;
//! 3. start the source again and check it (journaled as `Restoring`); the
//!    time between the stop and its first answer is the source's downtime;
//! 4. bring the destination up through a bootstrap with no network that
//!    sets its identity, then start it on its final endpoint.
//!
//! A coordinator that dies between 1 and 3 leaves a source that is stopped;
//! the next operation on it (recovery) starts it again before anything else.
use super::Service;
use super::catalog::{CloneMethod, Op, OpKind, RecordState, SourceRef, Step};
use super::docker::{self, HELPER_IMAGE, HelperOutcome, HelperSpec};
use super::launch::VolumeOwner;
use super::mysql;
use super::plan::coordinator_ref;
use super::profile::{self, Quiesced};
use crate::config::Engine;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use std::io::Write;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// How long the source may take to complete its `BGSAVE`.
const QUIESCE_TIMEOUT: Duration = Duration::from_secs(300);
/// How long the coordinator waits for the locks of its operation.
const LOCK_WAIT: Duration = Duration::from_secs(60);

fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Run operation `op_id` of service `key`: the body of `devflow __clone`.
pub fn coordinate(project: Uuid, key: &str, op_id: Uuid) -> Result<()> {
    let svc = Service::for_catalog(project, key)?;
    let op = svc
        .records()?
        .op(op_id)
        .cloned()
        .with_context(|| format!("the journal has no operation {op_id}"))?;
    anyhow::ensure!(
        !op.step.is_terminal(),
        "operation {op_id} has already ended"
    );
    // The locks are taken only when nothing that a dead coordinator left
    // unfinished touches them: it is recovered first (this operation was
    // planned before that coordinator died, so the caller's own check could not
    // have seen it).
    let Some(held) = svc.acquire_recovered(&op.resources(), Some(op_id), LOCK_WAIT)? else {
        say("busy");
        bail!("another operation holds the source or destination of {op_id}");
    };
    svc.update(|service| {
        if let Some(op) = service.op_mut(op_id) {
            op.coordinator = Some(coordinator_ref());
        }
        Ok(())
    })?;
    say("ready");
    let result = svc.execute(op_id);
    let outcome = match result {
        Ok(()) => {
            say("done");
            Ok(())
        }
        Err(error) => {
            let message = crate::redact::text(&format!("{error:#}"));
            // A failure rolls the operation back by the same rules as a
            // crash would, then reports why.
            let note = match svc.recover_op(op_id, super::recover::Cause::Failed(&message)) {
                Ok(_) => String::new(),
                Err(e) => format!(" (and rolling back failed: {e:#})"),
            };
            say(&format!("error: {message}{note}"));
            Err(error)
        }
    };
    drop(held);
    outcome
}

/// What a finished copy produced.
#[derive(Debug, Clone, Default)]
pub(super) struct CopyReport {
    pub method: Option<CloneMethod>,
    pub allocated: Option<i64>,
    /// Modification time of the copied file.
    pub mtime: Option<DateTime<Utc>>,
    /// Size in bytes of the copied file.
    pub size: Option<u64>,
    /// The source had no dump to copy.
    pub empty: bool,
}

/// What the helper script printed: `result=ok method=reflink size=… mtime=…`.
fn parse_copy(stdout: &str) -> Option<CopyReport> {
    let line = stdout.lines().find(|l| l.starts_with("result="))?;
    let field = |name: &str| {
        line.split_whitespace()
            .find_map(|part| part.strip_prefix(&format!("{name}=")))
    };
    match field("result")? {
        "empty" => Some(CopyReport {
            empty: true,
            ..CopyReport::default()
        }),
        "ok" => {
            let kib = |name: &str| field(name)?.parse::<i64>().ok();
            let allocated = match (kib("before"), kib("after")) {
                (Some(before), Some(after)) => Some(((before - after) * 1024).max(0)),
                _ => None,
            };
            Some(CopyReport {
                method: match field("method")? {
                    "reflink" => Some(CloneMethod::Reflink),
                    "copy" => Some(CloneMethod::Copy),
                    _ => None,
                },
                allocated,
                mtime: field("mtime")
                    .and_then(|t| t.parse::<i64>().ok())
                    .and_then(|secs| DateTime::from_timestamp(secs, 0)),
                size: field("size").and_then(|s| s.parse().ok()),
                empty: false,
            })
        }
        _ => None,
    }
}

/// The script of the copy helper. `ALLOW_COPY` is the journaled decision;
/// `DELAY` only exists for tests that need a copy that takes a while.
const COPY_SCRIPT: &str = r#"set -eu
f=dump.rdb
free() { df -Pk /dst | awk 'NR==2 {print $4}'; }
before=$(free)
if [ ! -s "/src/$f" ]; then echo "result=empty"; exit 0; fi
size=$(stat -c %s "/src/$f")
mtime=$(stat -c %Y "/src/$f")
if [ -n "${DELAY:-}" ]; then sleep "$DELAY"; fi
cloned=no
if [ -n "${NO_REFLINK:-}" ]; then
  echo "reflink disabled for this run (test hook): Operation not supported" > /tmp/cp.err
elif cp -a --reflink=always "/src/$f" "/dst/$f" 2>/tmp/cp.err; then
  cloned=yes
fi
if [ "$cloned" = yes ]; then
  method=reflink
elif [ "${ALLOW_COPY:-0}" = 1 ]; then
  rm -f "/dst/$f"
  cp -a "/src/$f" "/dst/$f"
  method=copy
else
  echo "result=refused error=$(tr '\n' ' ' < /tmp/cp.err)"
  exit 3
fi
sync
after=$(free)
echo "result=ok method=$method size=$size mtime=$mtime before=$before after=$after"
"#;

impl Service {
    pub(super) fn require_op(&self, op_id: Uuid) -> Result<Op> {
        self.records()?
            .op(op_id)
            .cloned()
            .with_context(|| format!("the journal has no operation {op_id}"))
    }

    /// Record the next step (and apply `also` to the catalog in the same
    /// write). The step is recorded before its effect.
    pub(super) fn step(
        &self,
        op_id: Uuid,
        to: Step,
        also: impl FnOnce(&mut super::catalog::ServiceCatalog) -> Result<()>,
    ) -> Result<()> {
        self.update(|service| {
            also(service)?;
            service
                .op_mut(op_id)
                .with_context(|| format!("the journal has no operation {op_id}"))?
                .advance(to)
        })
    }

    /// Run the operation's steps.
    pub(super) fn execute(&self, op_id: Uuid) -> Result<()> {
        let op = self.require_op(op_id)?;
        match op.kind {
            OpKind::CreateRoot => self.exec_root(&op),
            OpKind::Clone => self.exec_clone(&op),
            OpKind::Snapshot => self.exec_snapshot(&op),
            OpKind::Reset => self.exec_reset(&op),
            OpKind::Drop | OpKind::DeleteSnapshot => {
                bail!("{:?} runs in the caller, not in a coordinator", op.kind)
            }
        }
    }

    fn exec_root(&self, op: &Op) -> Result<()> {
        let id = op.target;
        self.step(op.id, Step::Starting, |_| Ok(()))?;
        if self.engine == Engine::Mysql {
            self.mysql_bootstrap_branch(op, true)?;
        }
        self.bring_up(id, true)?;
        self.update(|service| {
            let branch = service.branch_mut(id).context("the new branch vanished")?;
            branch.state = RecordState::Ready;
            branch.updated_at = Utc::now();
            service
                .op_mut(op.id)
                .context("the journal entry vanished")?
                .advance(Step::Done)
        })
    }

    fn exec_clone(&self, op: &Op) -> Result<()> {
        let id = op.target;
        let branch = self.require_branch(id)?;
        let source = op.source.context("a clone has no source")?;
        let (copied, captured) =
            self.copy_from(op, source, &branch.volume, VolumeOwner::Branch(id))?;
        self.step(op.id, Step::Copied, |service| {
            let record = service.branch_mut(id).context("the new branch vanished")?;
            record.captured_at = captured.captured_at;
            record.capture_exact = captured.exact;
            record.capture_last_save = captured.last_save;
            record.clone = copied.method;
            record.allocated_bytes = copied.allocated;
            if let Some(entry) = service.op_mut(op.id) {
                entry.copied_size = copied.size;
            }
            Ok(())
        })?;
        // A source that was stopped for the copy is serving again before
        // anything else happens; how long it was away is reported.
        let downtime = self.restore_source(op, &captured)?;
        self.step(op.id, Step::Starting, |service| {
            if let Some(ms) = downtime {
                service
                    .branch_mut(id)
                    .context("the new branch vanished")?
                    .downtime_ms = Some(ms);
                if let Some(entry) = service.op_mut(op.id) {
                    entry.downtime_ms = Some(ms);
                }
            }
            Ok(())
        })?;
        if self.engine == Engine::Mysql {
            self.mysql_bootstrap_branch(op, false)?;
        }
        self.bring_up(id, true)?;
        self.update(|service| {
            let record = service.branch_mut(id).context("the new branch vanished")?;
            record.state = RecordState::Ready;
            record.updated_at = Utc::now();
            service
                .op_mut(op.id)
                .context("the journal entry vanished")?
                .advance(Step::Done)
        })
    }

    fn exec_snapshot(&self, op: &Op) -> Result<()> {
        let id = op.target;
        let snapshot = self.snapshot(id)?.context("the new snapshot vanished")?;
        let source = op.source.context("a snapshot has no source")?;
        let (copied, captured) =
            self.copy_from(op, source, &snapshot.volume, VolumeOwner::Snapshot(id))?;
        self.step(op.id, Step::Copied, |service| {
            let record = service.snapshot_mut(id).context("the snapshot vanished")?;
            record.captured_at = captured.captured_at;
            record.capture_exact = captured.exact;
            record.capture_last_save = captured.last_save;
            record.clone = copied.method;
            record.allocated_bytes = copied.allocated;
            if let Some(entry) = service.op_mut(op.id) {
                entry.copied_size = copied.size;
            }
            Ok(())
        })?;
        let downtime = self.restore_source(op, &captured)?;
        self.update(|service| {
            let record = service.snapshot_mut(id).context("the snapshot vanished")?;
            record.state = RecordState::Ready;
            record.downtime_ms = downtime;
            if let Some(entry) = service.op_mut(op.id) {
                entry.downtime_ms = downtime;
            }
            service
                .op_mut(op.id)
                .context("the journal entry vanished")?
                .advance(Step::Done)
        })
    }

    /// Quiesce `source` (when it is a running branch), create the
    /// destination volume and copy the dump into it. Records the steps.
    /// Returns what the copy did and what was captured.
    fn copy_from(
        &self,
        op: &Op,
        source: SourceRef,
        dest_volume: &str,
        dest: VolumeOwner,
    ) -> Result<(CopyReport, Capture)> {
        match source {
            SourceRef::Empty => bail!("an empty source has nothing to copy"),
            SourceRef::Snapshot { id } => {
                let snapshot = self
                    .snapshot(id)?
                    .with_context(|| format!("snapshot {id} vanished"))?;
                anyhow::ensure!(
                    snapshot.state == RecordState::Ready,
                    "snapshot {id} is not ready"
                );
                // A snapshot is already a point in time, and no server runs
                // on it: there is nothing to quiesce, and a clone of it is
                // that same point.
                self.step(op.id, Step::Quiescing, |_| Ok(()))?;
                self.step(op.id, Step::Quiesced, |_| Ok(()))?;
                let report = self.copy_dump(
                    op,
                    (&snapshot.volume, VolumeOwner::Snapshot(id)),
                    (dest_volume, dest),
                )?;
                let capture = Capture {
                    captured_at: snapshot.captured_at,
                    exact: snapshot.capture_exact,
                    last_save: snapshot.capture_last_save,
                    stopped: None,
                };
                Ok((report, capture))
            }
            SourceRef::Branch { id } => {
                let branch = self.require_branch(id)?;
                anyhow::ensure!(
                    branch.state == RecordState::Ready,
                    "branch {id} is not ready ({:?})",
                    branch.state
                );
                self.step(op.id, Step::Quiescing, |_| Ok(()))?;
                // A running source is saved without being paused; a stopped
                // one has its last dump, written when it stopped.
                let running = self.is_running(&branch)?;
                self.update(|service| {
                    if let Some(entry) = service.op_mut(op.id) {
                        entry.source_running = Some(running);
                    }
                    Ok(())
                })?;
                if self.engine == Engine::Mysql {
                    // The source is shut down cleanly (or found so), copied
                    // whole, and started again by `restore_source`.
                    let (stopped, captured_at) =
                        self.mysql_quiesce(op.id, &branch, running, op.stop_timeout_secs)?;
                    self.step(op.id, Step::Quiesced, |_| Ok(()))?;
                    let report = self.copy_dump(
                        op,
                        (&branch.volume, VolumeOwner::Branch(id)),
                        (dest_volume, dest),
                    )?;
                    let capture = Capture {
                        captured_at: Some(captured_at),
                        exact: true,
                        last_save: false,
                        stopped,
                    };
                    return Ok((report, capture));
                }
                let quiesced = if running {
                    let port = branch
                        .port
                        .context("the running source has no recorded port")?;
                    profile::check_layout(port, &branch.password)?;
                    // A source with nothing new to save is cloned from the
                    // file it already has: a fresh save would write a second
                    // copy of the dataset while the first stays shared with
                    // earlier clones. The file must exist; any doubt saves.
                    let reusable = match profile::last_save(port, &branch.password)? {
                        Some(last)
                            if self
                                .volume_file_size(op.id, &branch.volume, profile::DUMP_FILE)?
                                .is_some_and(|size| size > 0) =>
                        {
                            Some(last)
                        }
                        _ => None,
                    };
                    match reusable {
                        Some(last) => Some(last),
                        None => Some(profile::quiesce(port, &branch.password, QUIESCE_TIMEOUT)?),
                    }
                } else {
                    None
                };
                self.step(op.id, Step::Quiesced, |_| Ok(()))?;
                let report = self.copy_dump(
                    op,
                    (&branch.volume, VolumeOwner::Branch(id)),
                    (dest_volume, dest),
                )?;
                let capture = self.capture_of(&branch, quiesced.as_ref(), &report);
                Ok((report, capture))
            }
        }
    }

    /// When the copied data was captured, and whether that is exact.
    fn capture_of(
        &self,
        source: &super::catalog::BranchRecord,
        quiesced: Option<&Quiesced>,
        report: &CopyReport,
    ) -> Capture {
        match quiesced {
            Some(quiesced) => {
                // If the server saved again while the file was copied, the
                // file may be that later save: newer than `captured_at`.
                let saves_now = source.port.and_then(|port| {
                    let mut conn = super::resp::Conn::connect(port, Duration::from_secs(3)).ok()?;
                    conn.auth(&source.password).ok()?;
                    conn.info("persistence")
                        .ok()?
                        .get("rdb_saves")?
                        .parse::<u64>()
                        .ok()
                });
                Capture {
                    captured_at: Some(quiesced.captured_at),
                    exact: saves_now.is_none_or(|now| now == quiesced.saves),
                    last_save: quiesced.last_save,
                    stopped: None,
                }
            }
            // A stopped source: its last dump, as old as the file.
            None => Capture {
                captured_at: report.mtime,
                exact: true,
                last_save: false,
                stopped: None,
            },
        }
    }

    /// Create the destination volume and copy `dump.rdb` into it with the
    /// helper. The helper's identity is recorded before it starts; the
    /// deadline kills it and confirms it is gone.
    fn copy_dump(
        &self,
        op: &Op,
        (src_volume, src_owner): (&str, VolumeOwner),
        (dest_volume, dest_owner): (&str, VolumeOwner),
    ) -> Result<CopyReport> {
        // The source is the catalog's, whole and ours, before anything is
        // mounted: a missing volume would be created empty by the mount and
        // an empty namesake would pass for the data.
        self.require_source_volume(src_volume, src_owner)?;
        self.ensure_volume(dest_volume, Some(dest_owner))?;
        // The attempt's name is unique to it: the one planned with the
        // operation the first time, a new one if that was already used. It is
        // recorded, in the list of helpers that outlives the operation, before
        // Docker is asked for the container.
        let planned = op
            .helper_name
            .clone()
            .context("no helper name was journaled")?;
        let name = if self.attempt_recorded(&planned)? {
            self.helper_attempt_name(op.id, "copy")
        } else {
            planned
        };
        self.update(|service| {
            Service::register_attempt(service, op.id, &name);
            if let Some(entry) = service.op_mut(op.id) {
                entry.helper_name = Some(name.clone());
            }
            Ok(())
        })?;
        let mut env = vec![(
            "ALLOW_COPY".to_string(),
            if op.copy_allowed { "1" } else { "0" }.to_string(),
        )];
        // Test hooks, for storage that cannot be made to misbehave otherwise:
        // a copy that takes a while, and a filesystem that refuses reflinks.
        if let Ok(delay) = std::env::var("DEVFLOW_TEST_COPY_DELAY_SECS") {
            env.push(("DELAY".to_string(), delay));
        }
        if std::env::var_os("DEVFLOW_TEST_NO_REFLINK").is_some() {
            env.push(("NO_REFLINK".to_string(), "1".to_string()));
        }
        // A MySQL data directory that becomes a branch loses what makes it
        // the source (server UUID, key pairs); a snapshot is a plain copy.
        let (script, what) = if self.engine == Engine::Mysql {
            if matches!(dest_owner, VolumeOwner::Branch(_)) {
                env.push(("FIXUP".to_string(), "1".to_string()));
            }
            (mysql::COPY_SCRIPT, "MySQL data directory")
        } else {
            (COPY_SCRIPT, "dump.rdb")
        };
        let helper = docker::create_helper(&HelperSpec {
            name: name.clone(),
            image: HELPER_IMAGE.into(),
            mounts: vec![
                (src_volume.to_string(), "/src".into(), true),
                (dest_volume.to_string(), "/dst".into(), false),
            ],
            env,
            labels: vec![
                (docker::LABEL_PROJECT.into(), self.project.to_string()),
                (docker::LABEL_SERVICE.into(), self.key.clone()),
                (docker::LABEL_HELPER.into(), op.id.to_string()),
            ],
            script: script.into(),
        })?;
        // Recorded before it runs: whoever recovers this operation kills it.
        self.step(op.id, Step::Copying, |service| {
            Service::attempt_created(service, &name, &helper);
            if let Some(entry) = service.op_mut(op.id) {
                entry.helper_id = Some(helper.clone());
            }
            Ok(())
        })?;
        let deadline = Instant::now() + Duration::from_secs(op.copy_timeout_secs);
        let outcome = docker::run_helper(&helper, deadline);
        let gone = docker::kill_and_confirm(&helper, Duration::from_secs(20));
        // Created, removed by id and confirmed gone: nothing is on its way.
        if gone.is_ok() {
            self.resolve_attempt(&name)?;
        }
        match outcome? {
            HelperOutcome::TimedOut => {
                gone.context("the copy helper is still present after being killed")?;
                bail!(
                    "the copy took longer than {}; the helper was killed and confirmed \
                     gone, and the destination is not used",
                    crate::text::counted(op.copy_timeout_secs, "second")
                )
            }
            HelperOutcome::Exited { code: 0, stdout } => {
                gone?;
                self.update(|service| {
                    if let Some(entry) = service.op_mut(op.id) {
                        entry.helper_id = None;
                    }
                    Ok(())
                })?;
                let report = parse_copy(&stdout).with_context(|| {
                    format!("unreadable output from the copy helper: {}", stdout.trim())
                })?;
                // A dump that is missing is data that is missing, never an empty
                // dataset: the catalog has no source that is recorded as empty
                // (an empty reset copies nothing).
                anyhow::ensure!(
                    !report.empty,
                    "the source volume {src_volume} holds no {what}; devflow will not treat \
                     missing data as an empty dataset (start the source with `devflow service \
                     {} up` so it saves, or reset to empty on purpose)",
                    self.key
                );
                Ok(report)
            }
            HelperOutcome::Exited { code, stdout } => {
                gone?;
                if stdout.contains("result=refused") {
                    bail!(
                        "the Docker volume storage cannot share blocks ({}), and a full copy \
                         is not allowed (`require_cow = true` on service '{}')",
                        stdout
                            .lines()
                            .find_map(|l| l.split_once(" error=").map(|(_, e)| e.trim()))
                            .unwrap_or("reflink refused"),
                        self.key
                    );
                }
                bail!(
                    "the copy helper exited with status {code}: {}",
                    crate::redact::text(stdout.trim())
                )
            }
        }
    }

    /// The size of `file` in `volume`, read inside the Docker VM by a short
    /// read-only helper of operation `op_id`; `None` when the volume has no
    /// such file. The helper is journaled like the copy helper: its name
    /// before Docker creates it, its id right after, both cleared once it is
    /// confirmed gone. A recovery killed in between leaves a helper that
    /// mounts the volume, and the next recovery (or a drop or reset, which
    /// recover first) finds it by what was recorded and removes it.
    pub(super) fn volume_file_size(
        &self,
        op_id: Uuid,
        volume: &str,
        file: &str,
    ) -> Result<Option<u64>> {
        // A helper an earlier, interrupted recovery recorded goes first: its
        // name is about to be replaced in the journal.
        let earlier = self.require_op(op_id)?;
        self.kill_helper(&earlier)?;
        let name = self.helper_attempt_name(op_id, "verify");
        self.update(|service| {
            // Recorded as an attempt that outlives the operation before Docker
            // is asked for it.
            Service::register_attempt(service, op_id, &name);
            let entry = service
                .op_mut(op_id)
                .context("the journal entry vanished")?;
            entry.verify_name = Some(name.clone());
            entry.verify_id = None;
            Ok(())
        })?;
        let helper = docker::create_helper(&HelperSpec {
            name: name.clone(),
            image: HELPER_IMAGE.into(),
            mounts: vec![(volume.to_string(), "/v".into(), true)],
            env: vec![("FILE".to_string(), file.to_string())],
            labels: vec![
                (docker::LABEL_PROJECT.into(), self.project.to_string()),
                (docker::LABEL_SERVICE.into(), self.key.clone()),
                (docker::LABEL_HELPER.into(), op_id.to_string()),
            ],
            script: r#"if [ -f "/v/$FILE" ]; then stat -c %s "/v/$FILE"; else echo missing; fi"#
                .into(),
        })?;
        self.update(|service| {
            Service::attempt_created(service, &name, &helper);
            let entry = service
                .op_mut(op_id)
                .context("the journal entry vanished")?;
            entry.verify_id = Some(helper.clone());
            Ok(())
        })?;
        let outcome = docker::run_helper(&helper, Instant::now() + Duration::from_secs(60));
        let gone = docker::kill_and_confirm(&helper, Duration::from_secs(20));
        if gone.is_ok() {
            self.update(|service| {
                if let Some(entry) = service.op_mut(op_id) {
                    entry.verify_name = None;
                    entry.verify_id = None;
                }
                // Created, removed by id and confirmed gone: resolved.
                service.helpers.retain(|h| h.name != name);
                Ok(())
            })?;
        }
        let outcome = outcome?;
        gone?;
        match outcome {
            HelperOutcome::Exited { code: 0, stdout } => match stdout.trim() {
                "missing" => Ok(None),
                size => size
                    .parse()
                    .map(Some)
                    .with_context(|| format!("unreadable size of {file}: {size}")),
            },
            HelperOutcome::Exited { code, stdout } => {
                bail!(
                    "the verify helper exited with status {code}: {}",
                    stdout.trim()
                )
            }
            HelperOutcome::TimedOut => bail!("the verify helper did not finish in 60 seconds"),
        }
    }

    fn exec_reset(&self, op: &Op) -> Result<()> {
        let branch_id = op.target;
        let plan = op.reset.clone().context("a reset has no plan")?;
        let branch = self.require_branch(branch_id)?;
        let source = op.source.context("a reset has no source")?;

        // 1. The replacement volume, filled from the source.
        let capture = match source {
            SourceRef::Empty => {
                self.ensure_volume(&plan.volume, Some(VolumeOwner::Branch(branch_id)))?;
                Capture {
                    captured_at: Some(Utc::now()),
                    exact: true,
                    last_save: false,
                    stopped: None,
                }
            }
            other => {
                let (copied, capture) =
                    self.copy_from(op, other, &plan.volume, VolumeOwner::Branch(branch_id))?;
                self.step(op.id, Step::Copied, |service| {
                    if let Some(entry) = service.op_mut(op.id)
                        && let Some(plan) = entry.reset.as_mut()
                    {
                        plan.captured_at = capture.captured_at;
                        plan.capture_exact = capture.exact;
                        plan.capture_last_save = capture.last_save;
                        plan.clone = copied.method;
                        plan.allocated_bytes = copied.allocated;
                    }
                    Ok(())
                })?;
                capture
            }
        };
        // A source stopped for the copy serves again before the replacement
        // is staged.
        let downtime = self.restore_source(op, &capture)?;

        // 2. The replacement is started on its own and checked, then thrown
        //    away without saving (a save would rewrite the shared file).
        //    MySQL's is bootstrapped instead: no network, the branch's own
        //    credentials set, shut down cleanly.
        self.step(op.id, Step::Staging, |service| {
            if let Some(ms) = downtime
                && let Some(entry) = service.op_mut(op.id)
            {
                entry.downtime_ms = Some(ms);
                if let Some(plan) = entry.reset.as_mut() {
                    plan.downtime_ms = Some(ms);
                }
            }
            Ok(())
        })?;
        let plan = self
            .require_op(op.id)?
            .reset
            .context("the reset plan vanished")?;
        if self.engine == Engine::Mysql {
            self.mysql_stage_reset(op, &branch, &plan, source == SourceRef::Empty)?;
        } else {
            self.stage_check(op, &branch, &plan)?;
        }

        // 3. The cutover: the old pair is recorded, stopped, and the new one
        //    started on the same port. The catalog moves to the new pair in
        //    one write, which is also the step to `cleanup`.
        let old_container = branch.container_id.clone();
        let old_running = self.is_running(&branch)?;
        self.step(op.id, Step::Cutover, |service| {
            if let Some(entry) = service.op_mut(op.id)
                && let Some(plan) = entry.reset.as_mut()
            {
                plan.old_container_id = old_container.clone();
                plan.old_volume = Some(branch.volume.clone());
                plan.old_was_running = old_running;
            }
            Ok(())
        })?;
        if old_running {
            self.stop_branch_for(Some(op.id), branch_id)?;
        }
        let launch = super::launch::Launch {
            branch: branch_id,
            name: plan.container_name.clone(),
            volume: plan.volume.clone(),
            pin: plan.pin.clone(),
            password: branch.password.clone(),
            domain: branch.domain.clone(),
        };
        let (new_id, port) = self.launch_container(&launch, branch.port, None, &mut |cid| {
            self.update(|service| {
                if let Some(entry) = service.op_mut(op.id)
                    && let Some(plan) = entry.reset.as_mut()
                {
                    plan.container_id = cid;
                }
                Ok(())
            })
        })?;
        self.wait_healthy(&new_id, &branch, port, Some(Duration::from_secs(180)))?;
        let plan = self
            .require_op(op.id)?
            .reset
            .context("the reset plan vanished")?;
        self.step(op.id, Step::Cleanup, |service| {
            let record = service
                .branch_mut(branch_id)
                .context("the branch vanished")?;
            record.container_id = Some(new_id.clone());
            record.container_name = plan.container_name.clone();
            record.volume = plan.volume.clone();
            record.image = plan.image.clone();
            record.pin = plan.pin.clone();
            record.generation += 1;
            record.port = Some(port);
            record.pending_port = None;
            record.source = source;
            record.captured_at = plan.captured_at;
            record.capture_exact = plan.capture_exact;
            record.capture_last_save = plan.capture_last_save;
            record.clone = plan.clone;
            record.allocated_bytes = plan.allocated_bytes;
            record.downtime_ms = plan.downtime_ms;
            record.updated_at = Utc::now();
            Ok(())
        })?;

        // 4. The replacement is validated and committed: the old pair goes.
        self.finish_reset_cleanup(op.id)
    }

    /// Start the replacement volume in a throwaway container on a port Docker
    /// picks, check it, and remove the container without saving.
    fn stage_check(
        &self,
        op: &Op,
        branch: &super::catalog::BranchRecord,
        plan: &super::catalog::ResetPlan,
    ) -> Result<()> {
        let launch = super::launch::Launch {
            branch: branch.id,
            name: plan.staging_name.clone(),
            volume: plan.volume.clone(),
            pin: plan.pin.clone(),
            password: branch.password.clone(),
            // The staging container has no public name.
            domain: None,
        };
        let spec = self.staging_spec(&launch);
        let id = docker::create_container(&spec)?;
        self.update(|service| {
            if let Some(entry) = service.op_mut(op.id)
                && let Some(plan) = entry.reset.as_mut()
            {
                plan.staging_id = Some(id.clone());
            }
            Ok(())
        })?;
        let checked = (|| -> Result<()> {
            docker::start_container(&id).map_err(|e| match e {
                docker::StartError::Bind(why) => anyhow::anyhow!("{why}"),
                docker::StartError::Other(e) => e,
            })?;
            let info = docker::inspect_container(&id)?.context("the staging container vanished")?;
            let port = info
                .published
                .context("the staging container publishes no port")?;
            profile::health(port, &branch.password, Duration::from_secs(60))
                .context("the replacement volume did not start")?;
            profile::shutdown_nosave(port, &branch.password);
            Ok(())
        })();
        let removed = docker::kill_and_confirm(&id, Duration::from_secs(20));
        self.update(|service| {
            if let Some(entry) = service.op_mut(op.id)
                && let Some(plan) = entry.reset.as_mut()
            {
                plan.staging_id = None;
            }
            Ok(())
        })?;
        checked?;
        removed
    }

    fn staging_spec(&self, launch: &super::launch::Launch) -> docker::ContainerSpec {
        let mut labels = self.labels(launch.branch);
        labels.push(("dev.devflow.staging".into(), "1".into()));
        docker::ContainerSpec {
            name: launch.name.clone(),
            image: launch.pin.clone(),
            volume: launch.volume.clone(),
            data_dir: profile::DATA_DIR.to_string(),
            host_port: None,
            container_port: profile::CONTAINER_PORT,
            isolated: false,
            files: Vec::new(),
            labels,
            command: profile::command(self.engine, &launch.password),
        }
    }

    /// Start a source this operation stopped, check it, and say how long it
    /// was away in milliseconds (`None` when it never stopped). Journaled as
    /// `Restoring` first; a failure rolls the operation back, which tries
    /// again.
    fn restore_source(&self, op: &Op, capture: &Capture) -> Result<Option<u64>> {
        let Some(stopped) = capture.stopped else {
            return Ok(None);
        };
        self.step(op.id, Step::Restoring, |_| Ok(()))?;
        self.bring_up(stopped.source, false)
            .context("the source could not be started again after the copy")?;
        // The source's downtime ends at its first answer; the check of how
        // it started is not part of it.
        let downtime = u64::try_from(stopped.since.elapsed().as_millis()).unwrap_or(u64::MAX);
        if self.engine == Engine::Mysql {
            self.mysql_require_clean_start(stopped.source)?;
        }
        Ok(Some(downtime))
    }

    /// Remove the pair a committed reset replaced, then end the operation.
    /// Idempotent: also what recovery runs for a reset found at `cleanup`.
    pub(super) fn finish_reset_cleanup(&self, op_id: Uuid) -> Result<()> {
        let op = self.require_op(op_id)?;
        let plan = op.reset.clone().context("a reset has no plan")?;
        if let Some(old) = &plan.old_container_id {
            // The record now points at the new container: this is the old one,
            // removed only while it still carries this branch's labels.
            self.remove_recorded_container(op.target, Some(old), old)?;
        }
        if let Some(volume) = &plan.old_volume {
            self.remove_recorded_volume(volume, VolumeOwner::Branch(op.target), false)?;
        }
        self.update(|service| {
            service
                .op_mut(op_id)
                .context("the journal entry vanished")?
                .advance(Step::Done)
        })
    }
}

/// What a clone captured.
#[derive(Debug, Clone, Copy)]
pub(super) struct Capture {
    pub captured_at: Option<DateTime<Utc>>,
    pub exact: bool,
    /// The data is the source's last save, cloned without a new one.
    pub last_save: bool,
    /// The source the coordinator stopped for the copy, to start again.
    pub stopped: Option<Stopped>,
}

/// A source container the coordinator stopped (MySQL), and when.
#[derive(Debug, Clone, Copy)]
pub(super) struct Stopped {
    pub source: Uuid,
    /// Just before the stop was asked for: the start of the downtime.
    pub since: Instant,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_helper_report_is_parsed() {
        let report = parse_copy(
            "noise\nresult=ok method=reflink size=100 mtime=1700000000 before=2000 after=1990\n",
        )
        .unwrap();
        assert_eq!(report.method, Some(CloneMethod::Reflink));
        assert_eq!(report.allocated, Some(10 * 1024));
        assert_eq!(report.mtime.unwrap().timestamp(), 1_700_000_000);
        assert!(!report.empty);
        let copy = parse_copy("result=ok method=copy size=1 mtime=1 before=10 after=20").unwrap();
        assert_eq!(copy.method, Some(CloneMethod::Copy));
        assert_eq!(copy.allocated, Some(0), "growth is never negative");
        assert!(parse_copy("result=empty").unwrap().empty);
        assert!(parse_copy("result=refused error=nope").is_none());
        assert!(parse_copy("garbage").is_none());
    }

    #[test]
    fn the_copy_script_never_uses_reflink_auto() {
        assert!(COPY_SCRIPT.contains("--reflink=always"));
        assert!(!COPY_SCRIPT.contains("--reflink=auto"));
        assert!(!COPY_SCRIPT.contains("--reflink "));
        // The full copy is gated by the journaled decision.
        assert!(COPY_SCRIPT.contains("ALLOW_COPY"));
        assert!(COPY_SCRIPT.contains("-a "), "ownership and mode are kept");
    }
}
