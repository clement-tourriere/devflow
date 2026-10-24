//! Resting a workspace: suspend, pin and what "idle" means.
//!
//! Nothing here runs by itself. A person (or an agent) asks for a workspace to
//! be suspended, or for the idle ones to be, and nothing else ever does: other
//! agents work in other worktrees, so devflow has no "active workspace" and no
//! timer that could decide one is not in use.
//!
//! - **What suspend stops.** The workspace's own daemons (through the same
//!   gated stop `down` uses, with the same approval rules, never a raw kill)
//!   and its own containers (the Valkey, Redis and MySQL branches its receipt
//!   binds; volumes are kept). The project's shared PostgreSQL server is not a
//!   workspace's container and is never stopped. A daemon state devflow cannot
//!   read is unknown, not stopped: the workspace is then not suspended and the
//!   containers the daemons may be using stay up.
//! - **What is recorded.** `suspended_at` in the receipt, once everything that
//!   could be stopped is. Stopping creates nothing that has to be cleaned up,
//!   so the fact is written after the effects are seen to have happened: a
//!   crash in between leaves a workspace that is not marked and may be partly
//!   stopped, which is what it is, and running the command again converges.
//! - **Resuming** is entering the workspace ([`crate::setup::activate`]): it
//!   starts the containers, runs the `switch` hooks and the `[pitchfork]
//!   autostart` daemons, and clears the mark. Daemons that are not started on
//!   entry stay stopped until asked for.
//! - **Pins.** A pinned workspace is never selected by [`select_idle`].
//! - **Last activity** is the latest of the receipt's `last_active_at` (set by
//!   activation) and the modification time of `.devflow/active`, which
//!   `devflow exec` touches ([`touch_activity`]: at most one tiny write per
//!   half minute), and the time the workspace was created when it has neither.
use crate::error::Busy;
use crate::pitchfork::Pitchfork;
use crate::processes::{self, ProcessReport, Refusal};
use crate::project::Project;
use crate::receipt::{Receipt, ReceiptLock};
use crate::remedy::{Problem, Remedy};
use crate::services::workspace::{self as container_services, Changes};
use crate::setup;
use crate::worktree::{self, Workspace};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::path::Path;
use std::time::Duration;

/// The file `devflow exec` touches, in the workspace's `.devflow` directory.
pub const ACTIVITY_FILE: &str = "active";

/// How stale the touch file may be before `exec` writes it again.
const TOUCH_EVERY: Duration = Duration::from_secs(30);

/// Note that a command ran in the workspace whose worktree root is `root`.
///
/// This is on `devflow exec`'s fast path, so it is a few system calls and, at
/// most once per [`TOUCH_EVERY`], one change of an empty file's time. It never
/// creates the `.devflow` directory, never follows a link at either step (the
/// directory and the file are opened relative to the one before, without
/// following, and what was opened is checked to be a regular file), and never
/// fails: a read-only checkout simply has no touch.
pub fn touch_activity(root: &Path) {
    #[cfg(unix)]
    unix::touch(root);
    #[cfg(not(unix))]
    {
        let path = root.join(crate::env_file::DIR).join(ACTIVITY_FILE);
        if path
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_file())
            && let Ok(file) = std::fs::OpenOptions::new().write(true).open(&path)
        {
            let _ = file.set_modified(std::time::SystemTime::now());
        }
    }
}

/// When a command last ran in the workspace at `root`, as the activity file
/// says (opened without following links, like [`touch_activity`]).
fn touched_at(root: &Path) -> Option<DateTime<Utc>> {
    #[cfg(unix)]
    {
        unix::modified(root).map(DateTime::<Utc>::from)
    }
    #[cfg(not(unix))]
    {
        let meta = root
            .join(crate::env_file::DIR)
            .join(ACTIVITY_FILE)
            .symlink_metadata()
            .ok()?;
        meta.file_type()
            .is_file()
            .then(|| meta.modified().ok().map(DateTime::<Utc>::from))?
    }
}

#[cfg(unix)]
mod unix {
    use super::{ACTIVITY_FILE, TOUCH_EVERY};
    use std::ffi::CString;
    use std::fs::File;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::path::Path;

    const BASE: libc::c_int = libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;

    fn open_at(
        parent: RawFd,
        name: &str,
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> Option<OwnedFd> {
        let name = CString::new(name).ok()?;
        // SAFETY: `openat(2)` with a NUL-terminated name and a descriptor that
        // lives for the call.
        let fd = unsafe { libc::openat(parent, name.as_ptr(), flags, libc::c_uint::from(mode)) };
        // SAFETY: a descriptor `openat` just returned and nothing else owns.
        (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// The worktree root, then `.devflow` below it: a directory, not a link.
    fn devflow_dir(root: &Path) -> Option<OwnedFd> {
        let root = File::open(root).ok()?;
        if !root.metadata().ok()?.is_dir() {
            return None;
        }
        let fd = open_at(
            root.as_raw_fd(),
            crate::env_file::DIR,
            libc::O_RDONLY | libc::O_DIRECTORY | BASE,
            0,
        )?;
        Some(fd)
    }

    pub fn touch(root: &Path) {
        let Some(dir) = devflow_dir(root) else {
            return;
        };
        let existing = open_at(dir.as_raw_fd(), ACTIVITY_FILE, libc::O_WRONLY | BASE, 0);
        let fd = match existing {
            Some(fd) => fd,
            None => {
                // Not there (or a link, which this refuses too): create it
                // exclusively, so a link that appeared meanwhile is not used.
                let _ = open_at(
                    dir.as_raw_fd(),
                    ACTIVITY_FILE,
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | BASE,
                    0o600,
                );
                return;
            }
        };
        let file = File::from(fd);
        let Ok(meta) = file.metadata() else {
            return;
        };
        // What was opened, not what the name said a moment ago.
        if !meta.file_type().is_file() {
            return;
        }
        let fresh = meta
            .modified()
            .ok()
            .and_then(|at| at.elapsed().ok())
            .is_some_and(|age| age < TOUCH_EVERY);
        if !fresh {
            // SAFETY: `futimens(2)` on a descriptor this function owns; a null
            // time means now.
            unsafe { libc::futimens(file.as_raw_fd(), std::ptr::null()) };
        }
    }

    pub fn modified(root: &Path) -> Option<std::time::SystemTime> {
        let dir = devflow_dir(root)?;
        let fd = open_at(dir.as_raw_fd(), ACTIVITY_FILE, libc::O_RDONLY | BASE, 0)?;
        let meta = File::from(fd).metadata().ok()?;
        meta.file_type().is_file().then(|| meta.modified().ok())?
    }
}

/// When the workspace was last in use: the latest of its activation and its
/// last `devflow exec`; `None` when it has had neither since they were
/// recorded.
pub fn last_used(workspace: &Workspace, receipt: &Receipt) -> Option<DateTime<Utc>> {
    let touched = touched_at(&workspace.path);
    [receipt.last_active_at, touched]
        .into_iter()
        .flatten()
        .max()
}

/// [`last_used`], or when the workspace was created if it was never used.
pub fn last_activity(workspace: &Workspace, receipt: &Receipt) -> DateTime<Utc> {
    last_used(workspace, receipt).unwrap_or(receipt.created_at)
}

/// Read `--idle`: a duration like `3d`, `12h` or `90m`.
pub fn parse_idle(text: &str) -> Result<Duration> {
    let duration = humantime::parse_duration(text.trim())
        .map_err(|_| anyhow::anyhow!("expected a duration like \"3d\", \"12h\" or \"90m\""))?;
    anyhow::ensure!(!duration.is_zero(), "the idle time must be more than zero");
    Ok(duration)
}

// ---------------------------------------------------------------------------
// Which workspaces are idle

/// A workspace `--idle` would suspend.
#[derive(Debug, Clone, Serialize)]
pub struct Idle {
    pub workspace: Workspace,
    /// The receipt's instance id when it was selected: what is suspended is
    /// this workspace and not whatever answers to the name by then.
    pub instance: uuid::Uuid,
    pub last_active_at: DateTime<Utc>,
}

/// Why a workspace was not considered for `--idle`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum SkipReason {
    /// Pinned.
    Pinned,
    /// The project's main checkout.
    Default,
    /// The caller's own workspace.
    Current,
    /// Already suspended.
    Suspended,
    /// Devflow has not set it up, or its directory is gone.
    NotSetUp,
    /// Its receipt could not be read: its activity is not known, so it is
    /// never selected.
    Unreadable { detail: String },
}

/// A workspace `--idle` left alone, and why.
#[derive(Debug, Clone, Serialize)]
pub struct Skipped {
    pub workspace: String,
    pub path: std::path::PathBuf,
    #[serde(flatten)]
    pub reason: SkipReason,
}

/// What `--idle DURATION` selects.
#[derive(Debug, Clone, Default, Serialize)]
pub struct IdleSelection {
    /// Workspaces whose last activity is older than the duration, oldest first.
    pub idle: Vec<Idle>,
    /// Workspaces that were not considered, and why (those that are simply
    /// recent are not listed).
    pub skipped: Vec<Skipped>,
}

/// The unpinned workspaces of the project whose last activity is older than
/// `older_than`. The main checkout, a suspended workspace, the one containing
/// `current` and anything whose state cannot be read are never selected.
pub fn select_idle(
    project: &Project,
    older_than: Duration,
    now: DateTime<Utc>,
    current: Option<&Path>,
) -> Result<IdleSelection> {
    let mut selection = IdleSelection::default();
    let all = worktree::list(project)?;
    // The checkout containing the caller's directory (deepest path wins).
    let current = current.and_then(|cwd| {
        all.iter()
            .filter(|w| cwd.starts_with(&w.path))
            .max_by_key(|w| w.path.components().count())
            .map(|w| w.path.clone())
    });
    for workspace in all {
        let mut skip = |reason: SkipReason| {
            selection.skipped.push(Skipped {
                workspace: workspace.name.clone(),
                path: workspace.path.clone(),
                reason,
            });
        };
        if workspace.missing {
            skip(SkipReason::NotSetUp);
            continue;
        }
        if workspace.is_default {
            skip(SkipReason::Default);
            continue;
        }
        let receipt = match setup::receipt(project, &workspace) {
            Ok(Some(receipt)) => receipt,
            Ok(None) => {
                skip(SkipReason::NotSetUp);
                continue;
            }
            Err(error) => {
                skip(SkipReason::Unreadable {
                    detail: crate::redact::text(&format!("{error:#}")),
                });
                continue;
            }
        };
        if receipt.pinned {
            skip(SkipReason::Pinned);
            continue;
        }
        if receipt.suspended_at.is_some() {
            skip(SkipReason::Suspended);
            continue;
        }
        if current.as_deref() == Some(workspace.path.as_path()) {
            skip(SkipReason::Current);
            continue;
        }
        let last = last_activity(&workspace, &receipt);
        let idle_for = now.signed_duration_since(last).to_std().unwrap_or_default();
        if idle_for > older_than {
            selection.idle.push(Idle {
                workspace,
                instance: receipt.instance_id,
                last_active_at: last,
            });
        }
    }
    selection.idle.sort_by_key(|idle| idle.last_active_at);
    Ok(selection)
}

// ---------------------------------------------------------------------------
// Pin

/// Pin (or unpin) the workspace: `Ok(true)` when that changed anything.
pub fn set_pinned(project: &Project, workspace: &Workspace, pinned: bool) -> Result<bool> {
    // The process lock first (the lock order), which is also what keeps a
    // suspension from reading the pin as it was.
    let _process = processes::process_lock(workspace)?;
    let (_lock, mut receipt) = locked(project, workspace)?;
    if receipt.pinned == pinned {
        return Ok(false);
    }
    receipt.pinned = pinned;
    receipt.save(&workspace.git_dir)?;
    Ok(true)
}

/// Clear the mark of a workspace that something just started (its daemons or
/// its containers): it is not at rest any more. Best effort and silent: a
/// setup holding the lock, or a receipt that cannot be written, leaves the
/// mark, which only means it reads as suspended until the next entry.
pub(crate) fn clear_mark(project: &Project, workspace: &Workspace) {
    // Nothing to clear is the usual case, and it needs no lock (an activation
    // that holds the receipt lock has cleared the mark itself already).
    if matches!(
        Receipt::load(&workspace.git_dir),
        Ok(Some(receipt)) if receipt.suspended_at.is_none()
    ) {
        return;
    }
    let Ok(Some(lock)) = Receipt::lock_retrying(&workspace.git_dir) else {
        return;
    };
    if let Ok(Some(mut receipt)) =
        Receipt::migrate_in_place(&workspace.git_dir, &project.config, &lock)
        && receipt.suspended_at.take().is_some()
    {
        let _ = receipt.save(&workspace.git_dir);
    }
}

/// The workspace's receipt under the receipt lock, migrated; a workspace
/// devflow has not set up is a typed problem whose remedy is setup.
fn locked(project: &Project, workspace: &Workspace) -> Result<(ReceiptLock, Receipt)> {
    let lock = Receipt::lock_retrying(&workspace.git_dir)?
        .ok_or_else(|| anyhow::Error::new(Busy::setup(&workspace.name)))?;
    let receipt = Receipt::migrate_in_place(&workspace.git_dir, &project.config, &lock)?
        .ok_or_else(|| not_set_up(workspace))?;
    Ok((lock, receipt))
}

fn not_set_up(workspace: &Workspace) -> anyhow::Error {
    Problem::new(format!(
        "workspace '{}' is not set up, so there is nothing to rest or pin",
        workspace.name
    ))
    .with(Remedy::Setup {
        workspace: workspace.path.clone(),
    })
    .into_error()
}

// ---------------------------------------------------------------------------
// Suspend

/// What suspending one workspace did.
#[derive(Debug, Clone, Serialize)]
pub struct Suspension {
    pub workspace: String,
    pub path: std::path::PathBuf,
    /// Everything that could run is stopped and the receipt says so.
    pub suspended: bool,
    /// When it was suspended (the first time, when it already was).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suspended_at: Option<DateTime<Utc>>,
    /// The workspace's daemons as pitchfork reports them after the stop;
    /// `None` when it has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemons: Option<ProcessReport>,
    /// The workspace's own containers; `None` when daemons were not stopped,
    /// which keeps the containers they may use running.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub services: Option<Changes>,
    /// Why it is not suspended.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    #[serde(
        serialize_with = "crate::remedy::serialize_remedies",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub remedies: Vec<Remedy>,
    /// The workspace was chosen earlier and is not what was chosen any more
    /// (pinned, used or suspended meanwhile): it was left alone, which is not
    /// a failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left_alone: Option<String>,
}

impl Suspension {
    fn new(workspace: &Workspace) -> Self {
        Self {
            workspace: workspace.name.clone(),
            path: workspace.path.clone(),
            suspended: false,
            suspended_at: None,
            daemons: None,
            services: None,
            problem: None,
            remedies: Vec::new(),
            left_alone: None,
        }
    }

    fn refused(mut self, problem: impl Into<String>, remedies: Vec<Remedy>) -> Self {
        self.problem = Some(crate::redact::text(&problem.into()));
        self.remedies = remedies;
        self
    }
}

/// What a caller that chose the workspace earlier expects to still be true
/// when the suspension takes its locks (`suspend --idle`: the person was
/// shown a list and asked, and in between anything can happen).
#[derive(Debug, Clone)]
pub struct Expect {
    /// The instance that was selected.
    pub instance: uuid::Uuid,
    /// How long the workspace had to be unused.
    pub idle: Duration,
}

/// Why a workspace that was selected is not the one that was selected any
/// more, judged as it is now.
fn changed_since(expect: &Expect, workspace: &Workspace, receipt: &Receipt) -> Option<String> {
    if receipt.instance_id != expect.instance {
        return Some("it is another workspace than the one that was selected".to_string());
    }
    if receipt.pinned {
        return Some("it was pinned meanwhile".to_string());
    }
    if receipt.suspended_at.is_some() {
        return Some("it is suspended already".to_string());
    }
    let last = last_activity(workspace, receipt);
    let idle_for = Utc::now()
        .signed_duration_since(last)
        .to_std()
        .unwrap_or_default();
    (idle_for <= expect.idle).then(|| "it was used meanwhile".to_string())
}

/// Stop the workspace's own daemons and containers and record that it is
/// suspended. Never raises: what did not work is in the result.
///
/// The workspace's process lock is held from the first look to the record, so
/// nothing starts its daemons, enters it or pins it in between (those take the
/// same lock): the daemons stop through the gated stop `down` uses (the
/// approval of the pitchfork configuration, the state read back), and if they
/// cannot be seen to be stopped the containers they may be using are left
/// alone. Then, under the receipt lock too, the containers
/// ([`container_services::stop_services`]) and the record.
pub fn suspend(
    project: &Project,
    workspace: &Workspace,
    pitchfork: Option<&Pitchfork>,
) -> Suspension {
    suspend_expecting(project, workspace, pitchfork, None)
}

/// [`suspend`] for a workspace chosen earlier: with `expect`, nothing is
/// stopped unless it is still that workspace, unpinned, not suspended and
/// unused for the time asked, which is looked at again under the locks. A
/// workspace that changed is left alone and says why
/// ([`Suspension::left_alone`]); that is not a failure.
pub fn suspend_expecting(
    project: &Project,
    workspace: &Workspace,
    pitchfork: Option<&Pitchfork>,
    expect: Option<&Expect>,
) -> Suspension {
    let mut result = Suspension::new(workspace);
    if workspace.missing {
        return result.refused(
            format!("the directory of workspace '{}' is gone", workspace.name),
            Vec::new(),
        );
    }
    let process = match processes::process_lock(workspace) {
        Ok(lock) => lock,
        Err(error) => {
            let mut remedies = crate::remedy::remedies_of(&error);
            if error.downcast_ref::<Busy>().is_some() {
                remedies.push(Remedy::Retry);
            }
            return result.refused(format!("{error:#}"), remedies);
        }
    };
    let receipt = match setup::receipt(project, workspace) {
        Ok(Some(receipt)) => receipt,
        Ok(None) => {
            let error = not_set_up(workspace);
            return result.refused(error.to_string(), crate::remedy::remedies_of(&error));
        }
        Err(error) => {
            return result.refused(format!("cannot read the receipt: {error:#}"), Vec::new());
        }
    };
    if let Some(why) = expect.and_then(|expect| changed_since(expect, workspace, &receipt)) {
        result.left_alone = Some(why);
        return result;
    }
    let seen_active = receipt.last_active_at;

    // Daemons, whenever there may be any: its files declare some, any
    // pitchfork file is there, devflow recorded a start, or what devflow
    // recorded cannot be read. A daemon started outside devflow whose
    // definition is gone is found by asking pitchfork, not by the files.
    if processes::may_have_daemons(project, workspace) {
        match processes::stop_held(
            pitchfork,
            project,
            workspace,
            &processes::StopOptions::default(),
            &process,
        ) {
            Err(error) => {
                let mut remedies = crate::remedy::remedies_of(&error);
                if let Some(refusal) = error.downcast_ref::<Refusal>() {
                    remedies.extend(refusal.remedies(&workspace.path));
                }
                if error.downcast_ref::<Busy>().is_some() {
                    remedies.push(Remedy::Retry);
                }
                return result
                    .refused(format!("its daemons were not stopped: {error:#}"), remedies);
            }
            Ok(report) => {
                let problem = if let Some(why) = &report.state_error {
                    Some(format!(
                        "what pitchfork runs for this workspace could not be read after the \
                         stop, so it is not known to be stopped: {why}"
                    ))
                } else if !report.ran {
                    Some(format!(
                        "pitchfork did not stop the daemons{}",
                        report
                            .error
                            .as_deref()
                            .map(|e| format!(": {e}"))
                            .unwrap_or_default()
                    ))
                } else if !report.ok() {
                    Some("daemons of this workspace still run after the stop".to_string())
                } else {
                    None
                };
                result.daemons = Some(report);
                if let Some(problem) = problem {
                    return result.refused(problem, vec![Remedy::Retry]);
                }
            }
        }
    }

    // Containers and the record, under the receipt lock as well.
    let (_lock, mut receipt) = match locked(project, workspace) {
        Ok(held) => held,
        Err(error) => {
            let mut remedies = crate::remedy::remedies_of(&error);
            if error.downcast_ref::<Busy>().is_some() {
                remedies.push(Remedy::Retry);
            }
            return result.refused(format!("{error:#}"), remedies);
        }
    };
    if receipt.last_active_at != seen_active {
        return result.refused(
            "the workspace was entered while it was being suspended; it was left as it is",
            vec![Remedy::Retry],
        );
    }
    let changes = container_services::stop_services(project, workspace);
    let failed: Vec<String> = changes
        .services
        .iter()
        .filter(|s| s.change == container_services::Change::Failed)
        .map(|s| {
            format!(
                "{}: {}",
                s.key,
                s.reason.as_deref().unwrap_or("unknown reason")
            )
        })
        .collect();
    // A record that cannot be read says nothing about the containers: they
    // were not stopped, and that is not "no containers".
    if let Some(why) = &changes.error {
        result.services = Some(changes.clone());
        return result.refused(
            format!("the workspace's container services are not known: {why}"),
            vec![Remedy::Retry],
        );
    }
    result.services = Some(changes);
    if !failed.is_empty() {
        return result.refused(
            format!("a container did not stop ({})", failed.join("; ")),
            vec![Remedy::Retry],
        );
    }
    let at = *receipt.suspended_at.get_or_insert_with(Utc::now);
    match receipt
        .save(&workspace.git_dir)
        .context("cannot record in the receipt that the workspace is suspended")
    {
        Ok(()) => {
            result.suspended = true;
            result.suspended_at = Some(at);
            result
        }
        Err(error) => result.refused(format!("{error:#}"), vec![Remedy::Retry]),
    }
}

/// Clear the mark of the workspace that owns branch `branch` of container
/// service `key` in the project at `root`: something started its container,
/// so it is not at rest. Only a workspace that is positively the one whose
/// receipt binds the branch (and the only one that does) is touched; best
/// effort and silent, like [`clear_mark`].
pub fn clear_mark_of_branch(root: &Path, key: &str, branch: uuid::Uuid) {
    if root.as_os_str().is_empty() {
        return;
    }
    let Ok(project) = Project::open(root) else {
        return;
    };
    let Some(path) = container_services::bound_workspace(&project, key, branch) else {
        return;
    };
    if let Ok(workspaces) = worktree::list(&project)
        && let Some(workspace) = workspaces.iter().find(|w| w.path == path)
    {
        clear_mark(&project, workspace);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_durations_are_read_and_zero_is_refused() {
        assert_eq!(parse_idle("3d").unwrap(), Duration::from_secs(3 * 86_400));
        assert_eq!(parse_idle("90m").unwrap(), Duration::from_secs(5_400));
        assert!(parse_idle("soon").is_err());
        assert!(parse_idle("0s").is_err());
    }

    #[test]
    fn exec_touches_a_file_in_devflow_at_most_every_half_minute() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // No `.devflow` directory: nothing is created.
        touch_activity(root);
        assert!(!root.join(".devflow").exists());

        std::fs::create_dir(root.join(".devflow")).unwrap();
        let file = root.join(".devflow").join(ACTIVITY_FILE);
        touch_activity(root);
        assert_eq!(std::fs::metadata(&file).unwrap().len(), 0);

        // Fresh: left alone. Stale: moved forward.
        let old = std::time::SystemTime::now() - Duration::from_secs(3600);
        let handle = std::fs::OpenOptions::new().write(true).open(&file).unwrap();
        handle.set_modified(old).unwrap();
        touch_activity(root);
        let modified = std::fs::metadata(&file).unwrap().modified().unwrap();
        assert!(modified > old + Duration::from_secs(3000));
        let first = modified;
        touch_activity(root);
        assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), first);
    }

    #[cfg(unix)]
    #[test]
    fn exec_never_writes_through_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join(".devflow")).unwrap();
        let victim = root.join("victim");
        std::fs::write(&victim, "keep").unwrap();
        std::os::unix::fs::symlink(&victim, root.join(".devflow").join(ACTIVITY_FILE)).unwrap();
        touch_activity(root);
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
    }

    #[test]
    fn last_activity_is_the_latest_of_activation_and_exec() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace {
            name: "a".into(),
            branch: Some("a".into()),
            path: dir.path().to_path_buf(),
            git_dir: dir.path().join(".git"),
            is_default: false,
            locked: false,
            missing: false,
            head: None,
            fingerprint: None,
        };
        let mut receipt = Receipt::new(uuid::Uuid::new_v4(), "a", None);
        let created = receipt.created_at;
        assert_eq!(last_activity(&workspace, &receipt), created);

        let entered = created + chrono::Duration::hours(1);
        receipt.last_active_at = Some(entered);
        assert_eq!(last_activity(&workspace, &receipt), entered);

        // An exec after that wins; one before it does not.
        std::fs::create_dir(dir.path().join(".devflow")).unwrap();
        let file = dir.path().join(".devflow").join(ACTIVITY_FILE);
        std::fs::write(&file, "").unwrap();
        let handle = std::fs::OpenOptions::new().write(true).open(&file).unwrap();
        let later = std::time::SystemTime::now() + Duration::from_secs(3600);
        handle.set_modified(later).unwrap();
        assert_eq!(
            last_activity(&workspace, &receipt),
            DateTime::<Utc>::from(later)
        );
        handle
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(86_400 * 30))
            .unwrap();
        assert_eq!(last_activity(&workspace, &receipt), entered);
    }

    #[test]
    fn the_receipt_stays_version_2_and_old_receipts_read_without_the_marks() {
        let mut receipt = Receipt::new(uuid::Uuid::new_v4(), "a", None);
        let text = serde_json::to_string(&receipt).unwrap();
        assert!(
            !text.contains("suspended_at") && !text.contains("pinned"),
            "{text}"
        );
        assert_eq!(receipt.version, 2);
        // A receipt written before the marks existed.
        let back: Receipt = serde_json::from_str(&text).unwrap();
        assert!(back.suspended_at.is_none() && !back.pinned);
        receipt.pinned = true;
        receipt.suspended_at = Some(Utc::now());
        let back: Receipt =
            serde_json::from_str(&serde_json::to_string(&receipt).unwrap()).unwrap();
        assert!(back.pinned && back.suspended_at.is_some());
        assert_eq!(back.version, 2);
    }

    #[cfg(unix)]
    #[test]
    fn exec_never_creates_or_reads_the_activity_file_through_a_linked_devflow_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("work");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        // `.devflow` is a link to a directory elsewhere.
        std::os::unix::fs::symlink(&outside, root.join(".devflow")).unwrap();
        touch_activity(&root);
        assert!(
            std::fs::read_dir(&outside).unwrap().next().is_none(),
            "nothing was created outside the workspace"
        );
        // A file already there is not read as the workspace's activity either.
        std::fs::write(outside.join(ACTIVITY_FILE), "").unwrap();
        assert_eq!(touched_at(&root), None);
        let before = std::fs::metadata(outside.join(ACTIVITY_FILE))
            .unwrap()
            .modified()
            .unwrap();
        std::thread::sleep(Duration::from_millis(20));
        touch_activity(&root);
        assert_eq!(
            std::fs::metadata(outside.join(ACTIVITY_FILE))
                .unwrap()
                .modified()
                .unwrap(),
            before,
            "and it was not touched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_special_file_or_a_directory_in_place_of_the_activity_file_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".devflow").join(ACTIVITY_FILE)).unwrap();
        touch_activity(root);
        assert_eq!(touched_at(root), None);
        assert!(root.join(".devflow").join(ACTIVITY_FILE).is_dir());
    }

    #[test]
    fn the_activity_a_workspace_had_is_judged_again_for_a_selection() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace {
            name: "a".into(),
            branch: Some("a".into()),
            path: dir.path().to_path_buf(),
            git_dir: dir.path().join(".git"),
            is_default: false,
            locked: false,
            missing: false,
            head: None,
            fingerprint: None,
        };
        let mut receipt = Receipt::new(uuid::Uuid::new_v4(), "a", None);
        receipt.created_at = Utc::now() - chrono::Duration::days(10);
        let expect = Expect {
            instance: receipt.instance_id,
            idle: Duration::from_secs(3 * 86_400),
        };
        // Still idle: nothing changed.
        assert_eq!(changed_since(&expect, &workspace, &receipt), None);
        // Used, pinned, suspended or replaced meanwhile: not the one selected.
        let mut used = receipt.clone();
        used.last_active_at = Some(Utc::now());
        assert!(
            changed_since(&expect, &workspace, &used)
                .unwrap()
                .contains("used")
        );
        let mut pinned = receipt.clone();
        pinned.pinned = true;
        assert!(
            changed_since(&expect, &workspace, &pinned)
                .unwrap()
                .contains("pinned")
        );
        let mut suspended = receipt.clone();
        suspended.suspended_at = Some(Utc::now());
        assert!(changed_since(&expect, &workspace, &suspended).is_some());
        let other = Receipt::new(receipt.project_id, "a", None);
        assert!(
            changed_since(&expect, &workspace, &other)
                .unwrap()
                .contains("another workspace")
        );
    }
}
