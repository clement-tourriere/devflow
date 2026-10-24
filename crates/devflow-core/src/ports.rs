//! The app port: one port per workspace, exported as `DEVFLOW_PORT`.
//!
//! Two dev servers started from two workspaces both want port 3000. A
//! workspace gets one port of its own, chosen once from `[worktree] port_range`
//! (default 40000–49999), recorded in its receipt ([`Receipt::app_port`]) and
//! kept: the same journaled, allocate-once-and-keep rule as the host ports of
//! container services. The variable is generated, so `[env]` recipes and
//! compose files may use `${DEVFLOW_PORT}` and no recipe can set it.
//!
//! # Allocation
//!
//! Choosing is serialised across every project on the machine by a lock in the
//! data directory (`ports/lock`), and the ports handed out are listed in a
//! small ledger beside it (`ports/ledger.json`): two workspaces whose dev
//! servers are not running both look free to a bind check, and the ledger is
//! what keeps them apart. A reservation belongs to a workspace **instance**
//! (project id and instance id), never to a path: the directory it was made for
//! is only recorded to name the holder in a message, and is refreshed whenever
//! the workspace is seen again, so `git worktree move` frees nothing. An entry
//! is dropped only when its directory is gone *and* no checkout registered with
//! its repository carries its instance any more; anything that cannot be read
//! keeps the reservation. A new identity for a checkout ([`forget_replaced`])
//! gives up that checkout's old reservation and nothing else. The order is fixed:
//!
//! 1. the machine-wide lock is taken (innermost: after the process and the
//!    receipt lock of the workspace, never before them);
//! 2. a candidate is picked: the first port, from a starting point derived from
//!    the project and the workspace's instance id and wrapping around the
//!    range, that no other workspace holds in the ledger and that **binds**
//!    (nothing listens on it);
//! 3. the candidate is written to the receipt (`pending_app_port`) before
//!    anything is reserved, so a run that stops after this resumes at the same
//!    candidate;
//! 4. it is written to the ledger, and then to the receipt as the workspace's
//!    port, with the pending candidate cleared.
//!
//! # Kept
//!
//! A port the receipt holds is never replaced. When something listens on it
//! later, or another workspace holds it in the ledger too, that is reported
//! (see [`Ensured::warnings`]) and the port stays: moving it silently would
//! leave the variable in one file, a compose file, a browser tab and a proxy
//! route saying different things. A workspace whose port must change is set up
//! again with a new identity.
//!
//! # `CONDUCTOR_PORT`
//!
//! Conductor gives every workspace a block of ten ports and tells its scripts
//! the first as `CONDUCTOR_PORT`, with the workspace it belongs to as
//! `CONDUCTOR_WORKSPACE_PATH`. A workspace that has no port yet and is set up
//! by Conductor's script uses that port ([`conductor_port`]) instead of
//! allocating, recorded with its source. The variable is only believed on
//! positive evidence: `CONDUCTOR_WORKSPACE_PATH` must name this very workspace.
//! A shell that merely inherited `CONDUCTOR_PORT` (an agent inside one Conductor
//! workspace creating several with `devflow switch -c`) allocates normally.
use crate::config::Config;
use crate::project::Project;
use crate::receipt::Receipt;
use crate::worktree::Workspace;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// How long a run waits for another to finish choosing a port.
const LOCK_WAIT: Duration = Duration::from_secs(30);

/// The most candidates one allocation tries.
const MAX_ATTEMPTS: usize = 4096;

/// Where a workspace's port came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortSource {
    /// devflow chose it from the configured range.
    Allocated,
    /// Conductor's `CONDUCTOR_PORT`, at creation.
    Conductor,
}

/// What [`ensure`] found or did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ensured {
    /// The workspace's port; `None` when none could be chosen (the reason is
    /// in `warnings`).
    pub port: Option<u16>,
    /// The port was chosen by this call: the receipt changed and `.devflow/env`
    /// has to be written to carry it.
    pub allocated: bool,
    /// What is wrong with the port or the choice, in words (a kept port that
    /// something listens on, a range with nothing free). Never fatal.
    pub warnings: Vec<String>,
    /// What a person can do about a warning that has a remedy (an unreadable
    /// ledger: open the file).
    pub remedies: Vec<crate::remedy::Remedy>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Entry {
    port: u16,
    project_id: Uuid,
    instance_id: Uuid,
    /// The workspace's directory when it was last seen: to name the holder in a
    /// message, and to tell an entry whose directory is gone from one whose
    /// checkout was moved. Never the key.
    path: PathBuf,
    /// The repository's common Git directory: where the checkouts that carry
    /// this instance are looked for before an entry is freed. Empty in an entry
    /// an earlier build wrote, which is then never freed.
    #[serde(default)]
    common_dir: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Ledger {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    entries: Vec<Entry>,
}

const LEDGER_VERSION: u32 = 1;

fn dir() -> Result<PathBuf> {
    Ok(crate::paths::data_dir()?.join("ports"))
}

/// The machine-wide lock; released on drop.
struct Held {
    _file: std::fs::File,
    ledger: PathBuf,
}

fn lock(wait: Duration) -> Result<Held> {
    let dir = dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("lock"))
        .context("cannot open the port lock")?;
    let start = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => {
                return Ok(Held {
                    _file: file,
                    ledger: dir.join("ledger.json"),
                });
            }
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(error).context("cannot lock the port ledger");
            }
        }
        anyhow::ensure!(
            start.elapsed() < wait,
            "another devflow is choosing a port; try again in a moment"
        );
        std::thread::sleep(Duration::from_millis(40));
    }
}

impl Held {
    /// The ledger, reconciled with what actually owns each reservation (see
    /// [`Evidence`]): an entry whose workspace identity no checkout carries is
    /// dropped from the returned copy (the next write makes that durable).
    ///
    /// A ledger that cannot be read is **not** an empty ledger: with it gone,
    /// a second workspace could be given a stopped server's port and no bind
    /// check could tell. It is an error naming the file, with the remedy to
    /// open it; nothing is set aside or rewritten.
    fn read(&self) -> std::result::Result<Ledger, crate::remedy::Problem> {
        let mut ledger = self.read_unreconciled()?;
        ledger.entries = reconcile(std::mem::take(&mut ledger.entries));
        Ok(ledger)
    }

    /// The ledger as the file has it, entries nobody carries any more included.
    fn read_unreconciled(&self) -> std::result::Result<Ledger, crate::remedy::Problem> {
        let unreadable = |why: String| {
            crate::remedy::Problem::new(format!(
                "the port ledger {} cannot be read ({why}), so no port can be chosen safely:                  another workspace's reservation could be handed out again",
                self.ledger.display()
            ))
            .with(crate::remedy::Remedy::OpenFile {
                path: self.ledger.clone(),
            })
        };
        let text = match std::fs::read_to_string(&self.ledger) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Ledger::default());
            }
            Err(error) => return Err(unreadable(error.to_string())),
        };
        serde_json::from_str(&text).map_err(|error| unreadable(error.to_string()))
    }

    fn write(&self, ledger: &mut Ledger) -> Result<()> {
        ledger.version = LEDGER_VERSION;
        ledger.entries.sort_by_key(|entry| entry.port);
        crate::fsutil::write_atomic(&self.ledger, &serde_json::to_vec_pretty(ledger)?, false)
    }
}

/// What the checkouts of one repository say about who they are, read once.
/// `None` for a repository whose checkouts cannot all be read: nothing can then
/// be concluded from it.
#[derive(Debug)]
struct Checkouts(Vec<Checkout>);

#[derive(Debug)]
struct Checkout {
    /// Where the checkout is registered, when that could be told.
    path: Option<PathBuf>,
    /// The `(project, instance)` its receipt carries; `None` without a receipt.
    identity: Option<(Uuid, Uuid)>,
}

type Evidence = std::collections::HashMap<PathBuf, Option<Checkouts>>;

fn same_place(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

/// Read the main checkout's and every registered worktree's receipt under the
/// common Git directory `common`. Any read that fails makes the whole answer
/// `None`.
fn read_checkouts(common: &Path) -> Option<Checkouts> {
    let mut found = Vec::new();
    let one = |admin: &Path, path: Option<PathBuf>| -> Option<Checkout> {
        let identity = match Receipt::load(admin) {
            Ok(Some(receipt)) => Some((receipt.project_id, receipt.instance_id)),
            Ok(None) => None,
            Err(_) => return None,
        };
        Some(Checkout { path, identity })
    };
    found.push(one(common, common.parent().map(Path::to_path_buf))?);
    match std::fs::read_dir(common.join("worktrees")) {
        Ok(entries) => {
            for entry in entries {
                let admin = entry.ok()?.path();
                if !admin.is_dir() {
                    continue;
                }
                // `gitdir` holds `<worktree>/.git`.
                let path = std::fs::read_to_string(admin.join("gitdir"))
                    .ok()
                    .and_then(|text| PathBuf::from(text.trim()).parent().map(Path::to_path_buf));
                found.push(one(&admin, path)?);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return None,
    }
    Some(Checkouts(found))
}

/// What the checkouts say about one reservation.
enum Standing {
    /// Nothing can be concluded (no repository recorded, a receipt that fails
    /// to read): the reservation stays as it is.
    Unknown,
    /// No checkout of the repository carries the entry's `(project, instance)`:
    /// the workspace is gone, whether its directory exists (a reused path) or not.
    Gone,
    /// Some checkout carries it. `at_path` says whether that is the one the entry
    /// names; `carrier` is where a carrier is registered now (the checkout moved).
    Carried {
        at_path: bool,
        carrier: Option<PathBuf>,
    },
}

fn standing(entry: &Entry, evidence: &mut Evidence) -> Standing {
    if entry.common_dir.as_os_str().is_empty() || !entry.common_dir.is_dir() {
        return Standing::Unknown;
    }
    let checkouts = evidence
        .entry(entry.common_dir.clone())
        .or_insert_with(|| read_checkouts(&entry.common_dir));
    let Some(Checkouts(checkouts)) = checkouts else {
        return Standing::Unknown;
    };
    let wanted = Some((entry.project_id, entry.instance_id));
    let carriers: Vec<&Checkout> = checkouts.iter().filter(|c| c.identity == wanted).collect();
    if carriers.is_empty() {
        return Standing::Gone;
    }
    let at_path = carriers.iter().any(|c| {
        c.path
            .as_deref()
            .is_some_and(|p| same_place(p, &entry.path))
    });
    Standing::Carried {
        at_path,
        carrier: carriers.iter().find_map(|c| c.path.clone()),
    }
}

/// Reconcile the reservations with what actually owns them, whether the
/// directory an entry names exists or not:
///
/// - an entry whose `(project, instance)` no checkout carries is dropped (a
///   removed workspace, a path reused for another one);
/// - one a checkout carries at the entry's path stays;
/// - one that is carried only elsewhere (the checkout moved, and another may
///   even have been registered at the old path since) **stays**, with its path
///   refreshed to where the carrier is: the identity is the key, not the path.
///   If several entries are left for one identity, those at a path that does
///   not carry it are duplicates of those that do (a copy that took a new
///   identity and never gave its old reservation up) and are dropped, keeping a
///   single one when none is at its carrier.
///
/// Whatever cannot be read keeps its entry untouched.
fn reconcile(entries: Vec<Entry>) -> Vec<Entry> {
    let mut evidence = Evidence::new();
    let standings: Vec<Standing> = entries
        .iter()
        .map(|entry| standing(entry, &mut evidence))
        .collect();
    let identity = |e: &Entry| (e.project_id, e.instance_id);
    // Identities that have an entry standing at a path that carries them.
    let settled: std::collections::HashSet<(Uuid, Uuid)> = entries
        .iter()
        .zip(&standings)
        .filter(|(_, standing)| {
            matches!(
                standing,
                Standing::Carried { at_path: true, .. } | Standing::Unknown
            )
        })
        .map(|(entry, _)| identity(entry))
        .collect();
    let mut moved_kept: std::collections::HashSet<(Uuid, Uuid)> = Default::default();
    let mut kept = Vec::with_capacity(entries.len());
    for (mut entry, standing) in entries.into_iter().zip(standings) {
        match standing {
            Standing::Unknown | Standing::Carried { at_path: true, .. } => kept.push(entry),
            Standing::Gone => {}
            Standing::Carried { carrier, .. } => {
                // Carried elsewhere: a duplicate if another entry of the identity
                // is at its carrier, else the one that follows the checkout.
                let id = identity(&entry);
                if settled.contains(&id) || !moved_kept.insert(id) {
                    continue;
                }
                if let Some(carrier) = carrier {
                    entry.path = carrier;
                }
                kept.push(entry);
            }
        }
    }
    kept
}

/// The ports `config` allows, in the order a workspace tries them: a starting
/// point derived from the project and the workspace's instance, then its
/// successors, wrapping around the range.
pub fn candidates(config: &Config, project_id: Uuid, instance_id: Uuid) -> Vec<u16> {
    let [first, last] = config.worktree.app_port_range();
    let span = u32::from(last) - u32::from(first) + 1;
    let digest = Sha256::digest(format!("devflow.app-port.v1\0{project_id}\0{instance_id}"));
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    let start = (u64::from_be_bytes(head) % u64::from(span)) as u32;
    (0..span.min(MAX_ATTEMPTS as u32))
        .map(|i| (u32::from(first) + (start + i) % span) as u16)
        .collect()
}

/// Whether nothing listens on `port`, on the loopback addresses and on every
/// interface (a dev server may bind either). An address this machine does not
/// have does not count against the port.
pub fn is_free(port: u16) -> bool {
    ["127.0.0.1", "0.0.0.0", "::1"]
        .into_iter()
        .all(|host| match TcpListener::bind((host, port)) {
            Ok(_) => true,
            Err(error) => error.kind() != std::io::ErrorKind::AddrInUse,
        })
}

/// Conductor's port for the workspace at `path`: `CONDUCTOR_PORT` is a port
/// and `CONDUCTOR_WORKSPACE_PATH` names this workspace. Without that path there
/// is no evidence the port is this workspace's (the variable may only have been
/// inherited), so there is no port.
pub fn conductor_port(path: &Path) -> Option<u16> {
    let port = std::env::var("CONDUCTOR_PORT")
        .ok()?
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|port| *port >= 1024)?;
    let running_in = PathBuf::from(std::env::var_os("CONDUCTOR_WORKSPACE_PATH")?);
    (running_in.canonicalize().unwrap_or(running_in) == *path).then_some(port)
}

/// Give the workspace a port if it has none, and say what stands in the way of
/// the one it has. The caller holds the workspace's receipt lock; the receipt is
/// saved here whenever it changes.
pub fn ensure(project: &Project, workspace: &Workspace, receipt: &mut Receipt) -> Ensured {
    ensure_with(project, workspace, receipt, &is_free)
}

/// [`ensure`] with the bind check chosen by the caller.
pub fn ensure_with(
    project: &Project,
    workspace: &Workspace,
    receipt: &mut Receipt,
    free: &dyn Fn(u16) -> bool,
) -> Ensured {
    match try_ensure(project, workspace, receipt, free) {
        Ok(ensured) => ensured,
        Err(error) => Ensured {
            port: receipt.app_port,
            allocated: false,
            warnings: vec![format!("could not give the workspace a port: {error:#}")],
            remedies: crate::remedy::remedies_of(&error),
        },
    }
}

fn try_ensure(
    project: &Project,
    workspace: &Workspace,
    receipt: &mut Receipt,
    free: &dyn Fn(u16) -> bool,
) -> Result<Ensured> {
    let mut out = Ensured::default();
    let held = lock(LOCK_WAIT)?;
    let identity = (project.id(), receipt.instance_id);
    let mine = |entry: &Entry| (entry.project_id, entry.instance_id) == identity;
    let read = held.read();
    // A port the workspace has stays even when the ledger cannot be read; only
    // choosing one needs it.
    if let (Err(problem), Some(port)) = (&read, receipt.app_port) {
        out.port = Some(port);
        out.warnings.push(problem.message.clone());
        out.remedies.extend(problem.remedies.clone());
        return Ok(out);
    }
    let mut ledger = read.map_err(anyhow::Error::new)?;

    // A port the workspace has stays. It is added to the ledger when that
    // lost it, and reported when it cannot be used as it is.
    if let Some(port) = receipt.app_port {
        out.port = Some(port);
        if let Some(other) = ledger
            .entries
            .iter()
            .find(|entry| entry.port == port && !mine(entry))
        {
            out.warnings.push(format!(
                "DEVFLOW_PORT {port} is also held by the workspace at {}; devflow keeps the port \
                 a workspace has, so one of the two has to be set up again with a new identity",
                other.path.display()
            ));
        } else {
            // Ours: kept, with where the checkout is now (it may have moved).
            let fresh = entry_for(project, workspace, receipt, port);
            let here = workspace.path.clone();
            match ledger
                .entries
                .iter_mut()
                .find(|entry| entry.port == port && is_here(entry, identity, &here))
            {
                Some(entry) if *entry == fresh => {}
                Some(entry) => {
                    *entry = fresh;
                    held.write(&mut ledger)?;
                }
                None => {
                    ledger.entries.push(fresh);
                    held.write(&mut ledger)?;
                }
            }
        }
        if receipt.pending_app_port.take().is_some() {
            receipt.save(&workspace.git_dir)?;
        }
        return Ok(out);
    }

    // Conductor's port is used where Conductor made the workspace.
    if let Some(port) = conductor_port(&workspace.path) {
        receipt.pending_app_port = None;
        receipt.app_port = Some(port);
        receipt.app_port_source = Some(PortSource::Conductor);
        receipt.save(&workspace.git_dir)?;
        if ledger
            .entries
            .iter()
            .any(|entry| entry.port == port && !mine(entry))
        {
            out.warnings.push(format!(
                "CONDUCTOR_PORT {port} is also held by another devflow workspace"
            ));
        }
        let here = workspace.path.clone();
        ledger
            .entries
            .retain(|entry| !is_here(entry, identity, &here));
        ledger
            .entries
            .push(entry_for(project, workspace, receipt, port));
        held.write(&mut ledger)?;
        out.port = Some(port);
        out.allocated = true;
        return Ok(out);
    }

    let mut order = candidates(&project.config, project.id(), receipt.instance_id);
    // Resume where an interrupted allocation stopped: every candidate before
    // it was refused already.
    if let Some(at) = receipt
        .pending_app_port
        .and_then(|pending| order.iter().position(|port| *port == pending))
    {
        order.drain(..at);
    }
    let [first, last] = project.config.worktree.app_port_range();
    let mut refused = 0usize;
    for port in order {
        if ledger
            .entries
            .iter()
            .any(|entry| entry.port == port && !mine(entry))
            || !free(port)
        {
            refused += 1;
            continue;
        }
        // Intent first: the candidate is in the receipt before it is reserved.
        if receipt.pending_app_port != Some(port) {
            receipt.pending_app_port = Some(port);
            receipt.save(&workspace.git_dir)?;
        }
        let here = workspace.path.clone();
        ledger
            .entries
            .retain(|entry| !is_here(entry, identity, &here));
        ledger
            .entries
            .push(entry_for(project, workspace, receipt, port));
        held.write(&mut ledger)?;
        receipt.app_port = Some(port);
        receipt.app_port_source = Some(PortSource::Allocated);
        receipt.pending_app_port = None;
        receipt.save(&workspace.git_dir)?;
        out.port = Some(port);
        out.allocated = true;
        return Ok(out);
    }
    out.warnings.push(format!(
        "no port between {first} and {last} is free ({refused} tried): free some or widen \
         [worktree] port_range; the workspace has no DEVFLOW_PORT until then"
    ));
    if receipt.pending_app_port.take().is_some() {
        receipt.save(&workspace.git_dir)?;
    }
    Ok(out)
}

/// Whether `entry` is this workspace's own reservation: its instance, at this
/// checkout or at a directory that no longer exists (the checkout moved). An
/// entry of the same instance at another directory that is still there is a
/// copy's original: it is left alone.
fn is_here(entry: &Entry, identity: (Uuid, Uuid), path: &Path) -> bool {
    (entry.project_id, entry.instance_id) == identity
        && (entry.path == path || !entry.path.exists())
}

fn entry_for(project: &Project, workspace: &Workspace, receipt: &Receipt, port: u16) -> Entry {
    Entry {
        port,
        project_id: project.id(),
        instance_id: receipt.instance_id,
        path: workspace.path.clone(),
        common_dir: project.git_common_dir.clone(),
    }
}

/// Give back the port the workspace `(project_id, instance_id)` reserved for
/// the checkout at `path` (its removal). Best effort: an entry that stays is
/// dropped by the next reconciliation once nothing carries its identity.
pub fn release(project_id: Uuid, instance_id: Uuid, path: &Path) {
    forget_replaced(project_id, instance_id, path);
}

/// A checkout at `path` got a new identity: give up the reservation its old
/// instance made **for this checkout**. The same instance's entry for another
/// path (the original of a copied checkout, still running elsewhere) stays.
///
/// Call it only after the replacement receipt is saved. A run that never gets
/// here (a crash, a lock that timed out) leaves nothing behind for good: the
/// checkout at `path` no longer carries the old identity, so the next
/// reconciliation of the ledger drops the entry.
pub fn forget_replaced(project_id: Uuid, old_instance: Uuid, path: &Path) {
    let Ok(held) = lock(Duration::from_secs(2)) else {
        return;
    };
    let Ok(mut ledger) = held.read() else {
        return;
    };
    let before = ledger.entries.len();
    ledger.entries.retain(|entry| {
        !((entry.project_id, entry.instance_id) == (project_id, old_instance) && entry.path == path)
    });
    if ledger.entries.len() != before {
        let _ = held.write(&mut ledger);
    }
}

/// Give back every port the workspace instance `instance_id` of the project
/// `project_id` reserved, wherever the checkout it was made for was seen last.
/// For a clean-up that has established that the instance is gone everywhere (no
/// registered checkout carries it, the directory it had is gone): the
/// reservation is the instance's, not a path's, so nothing is matched by path.
/// Best effort, like [`release`].
pub fn release_instance(project_id: Uuid, instance_id: Uuid) {
    let Ok(held) = lock(Duration::from_secs(2)) else {
        return;
    };
    // An unreadable ledger is left as it is (see `Held::read`).
    let Ok(mut ledger) = held.read() else {
        return;
    };
    let before = ledger.entries.len();
    ledger
        .entries
        .retain(|entry| !(entry.project_id == project_id && entry.instance_id == instance_id));
    if ledger.entries.len() != before {
        let _ = held.write(&mut ledger);
    }
}

/// Which of `ports` the workspace instance `instance_id` of `project_id`
/// holds in the ledger, in the order given: one bounded look (a short wait for
/// the machine-wide lock, one read of the file) and **no** reconciliation of
/// the ledger with the repositories of the machine, which a validation that
/// runs every couple of seconds must not cost. `None` when the ledger cannot
/// be locked or read: unknown is not "no".
pub fn holds_all(project_id: Uuid, instance_id: Uuid, ports: &[u16]) -> Option<Vec<bool>> {
    let held = lock(Duration::from_millis(500)).ok()?;
    let ledger = held.read_unreconciled().ok()?;
    Some(
        ports
            .iter()
            .map(|port| {
                ledger.entries.iter().any(|entry| {
                    entry.port == *port
                        && entry.project_id == project_id
                        && entry.instance_id == instance_id
                })
            })
            .collect(),
    )
}

/// The ports reserved on this machine: `(port, project, instance)`.
pub fn reserved() -> Vec<(u16, Uuid, Uuid)> {
    let Ok(held) = lock(Duration::from_secs(2)) else {
        return Vec::new();
    };
    held.read()
        .map(|ledger| {
            ledger
                .entries
                .iter()
                .map(|entry| (entry.port, entry.project_id, entry.instance_id))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(range: Option<[u16; 2]>) -> Config {
        let mut config = Config::default();
        config.worktree.port_range = range;
        config
    }

    #[test]
    fn candidates_stay_in_the_range_and_start_by_identity() {
        let project = Uuid::new_v4();
        let one = Uuid::new_v4();
        let two = Uuid::new_v4();
        let a = candidates(&config(Some([41000, 41009])), project, one);
        assert_eq!(a.len(), 10);
        assert!(a.iter().all(|port| (41000..=41009).contains(port)));
        let mut sorted = a.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 10, "every port once");
        // Deterministic for an identity, different for another.
        assert_eq!(a, candidates(&config(Some([41000, 41009])), project, one));
        let big = candidates(&config(None), project, one);
        assert_ne!(big[0], candidates(&config(None), project, two)[0]);
        assert!(big.iter().all(|port| (40000..=49999).contains(port)));
        assert_eq!(big.len(), MAX_ATTEMPTS);
    }

    #[test]
    fn a_range_is_validated() {
        let mut config = Config::default();
        config.project.name = "x".into();
        config.project.id = Uuid::new_v4();
        config.worktree.port_range = Some([50, 60]);
        assert!(config.validate().is_err());
        config.worktree.port_range = Some([5000, 4000]);
        assert!(config.validate().is_err());
        config.worktree.port_range = Some([5000, 5000]);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn a_port_something_listens_on_is_not_free() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!is_free(port));
        drop(listener);
        assert!(is_free(port));
    }

    #[test]
    fn conductor_port_is_believed_only_for_its_own_workspace() {
        // (Environment variables are process-wide: one test sets them all.)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap();
        // SAFETY: the only test that touches these variables.
        unsafe {
            std::env::remove_var("CONDUCTOR_WORKSPACE_PATH");
            std::env::set_var("CONDUCTOR_PORT", "55100");
        }
        // An inherited port alone is no evidence: it could be any workspace's.
        assert_eq!(conductor_port(&path), None);
        unsafe { std::env::set_var("CONDUCTOR_WORKSPACE_PATH", &path) };
        assert_eq!(conductor_port(&path), Some(55100));
        assert_eq!(conductor_port(&path.join("elsewhere")), None);
        unsafe { std::env::set_var("CONDUCTOR_PORT", "80") };
        assert_eq!(
            conductor_port(&path),
            None,
            "a privileged port is not an app port"
        );
        unsafe { std::env::set_var("CONDUCTOR_PORT", "nope") };
        assert_eq!(conductor_port(&path), None);
        unsafe {
            std::env::remove_var("CONDUCTOR_PORT");
            std::env::remove_var("CONDUCTOR_WORKSPACE_PATH");
        }
    }
}
