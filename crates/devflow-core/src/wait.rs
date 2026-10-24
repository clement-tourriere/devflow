//! Waiting for a setup that runs somewhere else: `devflow wait`,
//! `devflow setup --wait` and `devflow exec --wait`.
//!
//! An agent host creates a worktree and starts the agent (or a script) while
//! the Git hook, the host's own setup script or another terminal is still
//! setting the workspace up. This module looks at the workspace until the run
//! that holds its setup lock is over, and says how it ended. It never takes
//! the lock to wait for it: each look is one non-blocking attempt that is
//! released at once, so the waiter cannot hold up the run it waits for, and
//! the wait ends at a deadline whatever happens, with a status that says why.
use crate::project::Project;
use crate::receipt::{Receipt, Status};
use crate::remedy::{Problem, Remedy};
use crate::setup;
use crate::worktree::Workspace;
use anyhow::Result;
use serde::Serialize;
use std::time::{Duration, Instant};

/// How long [`until_settled`] waits when the caller names no timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// A workspace with no receipt and no run is given this long to show one: the
/// Git hook (or a host script) that is about to start the setup has not taken
/// its lock yet. After that it is reported as not set up.
const START_GRACE: Duration = Duration::from_secs(3);

/// How often the workspace is looked at.
const POLL: Duration = Duration::from_millis(150);

/// Parse a timeout such as `90s`, `10m` or `1h30m`. A bare number is seconds.
/// `0` looks once and does not wait.
pub fn parse_timeout(text: &str) -> std::result::Result<Duration, String> {
    let text = text.trim();
    if let Ok(seconds) = text.parse::<u64>() {
        return Ok(Duration::from_secs(seconds));
    }
    humantime::parse_duration(text).map_err(|_| {
        format!(
            "'{text}' is not a duration (for example 90s, 10m or 1h30m; a bare number is seconds)"
        )
    })
}

/// A duration as a person reads it (`15m`, `1m 30s`), whole seconds.
pub fn format_duration(duration: Duration) -> String {
    humantime::format_duration(Duration::from_secs(duration.as_secs())).to_string()
}

/// How a wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Settled {
    /// No run holds the lock and every phase is done.
    Ready,
    /// No run holds the lock and the workspace is not ready: a phase failed,
    /// was skipped for a reason that needs a person, or a configuration
    /// change reopened one.
    NeedsAttention,
    /// A run still holds the lock at the deadline.
    StillRunning,
    /// No run holds the lock and none showed up: devflow has not set this
    /// workspace up.
    NotSetUp,
}

impl Settled {
    /// The status the result stands for in setup's vocabulary.
    pub fn status(self) -> Status {
        match self {
            Settled::Ready => Status::Ready,
            Settled::StillRunning => Status::InProgress,
            Settled::NeedsAttention | Settled::NotSetUp => Status::NeedsAttention,
        }
    }

    pub fn is_ready(self) -> bool {
        self == Settled::Ready
    }
}

/// What a wait found. Not serializable on purpose: the receipt holds
/// credentials, so a frontend picks the fields it shows.
#[derive(Debug, Clone)]
pub struct Waited {
    pub workspace: Workspace,
    pub settled: Settled,
    /// The receipt as the last look read it; `None` when there is none.
    pub receipt: Option<Receipt>,
    /// How long the wait took, in milliseconds.
    pub waited_ms: u64,
    /// The deadline that was given, in milliseconds.
    pub timeout_ms: u64,
}

impl Waited {
    /// One sentence for a person: why the workspace is not ready, or nothing
    /// when it is.
    pub fn describe(&self) -> Option<String> {
        let name = &self.workspace.name;
        match self.settled {
            Settled::Ready => None,
            Settled::StillRunning => Some(format!(
                "{name} is still being set up after {}",
                format_duration(Duration::from_millis(self.timeout_ms))
            )),
            Settled::NotSetUp => Some(format!("{name} has not been set up by devflow")),
            Settled::NeedsAttention => Some(format!("{name} needs attention")),
        }
    }

    /// The typed refusal for a command that must not start in a workspace
    /// that is not ready: `None` when it is ready.
    pub fn refusal(&self) -> Option<anyhow::Error> {
        let message = self.describe()?;
        Some(
            Problem::new(format!("{message}, so the command was not started"))
                .with(Remedy::Setup {
                    workspace: self.workspace.path.clone(),
                })
                .into_error(),
        )
    }
}

/// Wait until no run holds the setup lock of `workspace` and say how it ended,
/// or until `timeout` is over. `timeout` of zero looks once.
///
/// A devflow started by a hook (`DEVFLOW_SETUP_ACTIVE`) does not wait: the run
/// that started the hook holds the lock until the hook is over, so it could
/// only wait for its own parent. It looks once.
pub fn until_settled(
    project: &Project,
    workspace: &Workspace,
    timeout: Duration,
    mut progress: impl FnMut(Duration),
) -> Result<Waited> {
    let timeout = if crate::receipt::started_by_a_hook() {
        Duration::ZERO
    } else {
        timeout
    };
    let started = Instant::now();
    loop {
        let running = Receipt::is_locked(&workspace.git_dir);
        // A receipt another project wrote, or one that cannot be read, is an
        // error the caller reports: it is not "nothing".
        let receipt = setup::receipt(project, workspace)?;
        let elapsed = started.elapsed();
        let settled = match (running, &receipt) {
            (false, Some(receipt)) => Some(if receipt.status() == Status::Ready {
                Settled::Ready
            } else {
                Settled::NeedsAttention
            }),
            (false, None) if elapsed >= START_GRACE.min(timeout) => Some(Settled::NotSetUp),
            (true, _) if elapsed >= timeout => Some(Settled::StillRunning),
            _ => None,
        };
        if let Some(settled) = settled {
            return Ok(Waited {
                workspace: workspace.clone(),
                settled,
                receipt,
                waited_ms: elapsed.as_millis() as u64,
                timeout_ms: timeout.as_millis() as u64,
            });
        }
        progress(elapsed);
        std::thread::sleep(
            POLL.min(timeout.saturating_sub(elapsed))
                .max(Duration::from_millis(10)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::testutil;

    #[test]
    fn timeouts_are_durations_or_bare_seconds() {
        assert_eq!(parse_timeout("90"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_timeout("10m"), Ok(Duration::from_secs(600)));
        assert_eq!(parse_timeout("1h30m"), Ok(Duration::from_secs(5400)));
        assert_eq!(parse_timeout("0"), Ok(Duration::ZERO));
        assert!(parse_timeout("soon").is_err());
    }

    #[test]
    fn a_workspace_nobody_set_up_is_reported_after_the_grace() {
        let t = testutil::repo(None);
        let workspace = crate::worktree::containing(&t.project, &t.root)
            .unwrap()
            .unwrap();
        // Zero looks once: nothing to wait for.
        let found = until_settled(&t.project, &workspace, Duration::ZERO, |_| {}).unwrap();
        assert_eq!(found.settled, Settled::NotSetUp);
        assert!(found.refusal().is_some());
        assert_eq!(found.settled.status(), Status::NeedsAttention);
    }

    #[test]
    fn a_run_that_holds_the_lock_is_waited_for_and_never_blocked() {
        let t = testutil::repo(None);
        let workspace = crate::worktree::containing(&t.project, &t.root)
            .unwrap()
            .unwrap();
        let lock = Receipt::try_lock(&workspace.git_dir).unwrap().unwrap();
        // A receipt exists but the run holds the lock: still running at the
        // deadline, and the waiter released its own attempts (the lock is
        // still ours to give up).
        let mut receipt = Receipt::new(t.project.id(), "main", None);
        receipt.set_phase(
            crate::receipt::Phase::Materialize,
            crate::receipt::PhaseState::Started,
            None,
        );
        receipt.save(&workspace.git_dir).unwrap();
        let started = Instant::now();
        let found =
            until_settled(&t.project, &workspace, Duration::from_millis(400), |_| {}).unwrap();
        assert_eq!(found.settled, Settled::StillRunning);
        assert!(started.elapsed() >= Duration::from_millis(400));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(found.settled.status(), Status::InProgress);
        drop(lock);
        let found = until_settled(&t.project, &workspace, Duration::ZERO, |_| {}).unwrap();
        assert_eq!(found.settled, Settled::NeedsAttention);
    }

    #[test]
    fn a_run_that_finishes_while_waiting_is_seen() {
        let t = testutil::repo(None);
        let workspace = crate::worktree::containing(&t.project, &t.root)
            .unwrap()
            .unwrap();
        let mut receipt = Receipt::new(t.project.id(), "main", None);
        for phase in crate::receipt::Phase::ALL {
            receipt.set_phase(phase, crate::receipt::PhaseState::Done, None);
        }
        receipt.save(&workspace.git_dir).unwrap();
        let lock = Receipt::try_lock(&workspace.git_dir).unwrap().unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(lock);
        });
        let found = until_settled(&t.project, &workspace, Duration::from_secs(10), |_| {}).unwrap();
        releaser.join().unwrap();
        assert_eq!(found.settled, Settled::Ready);
        assert!(found.waited_ms < 5000);
        assert!(found.refusal().is_none());
    }
}
