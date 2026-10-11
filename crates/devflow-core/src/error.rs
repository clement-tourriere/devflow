//! Typed errors that frontends act on (offer `--force`, show an
//! "initialize" button, report `in_progress`). They travel inside
//! `anyhow::Error`; match them with `error.downcast_ref::<T>()` instead of
//! inspecting message text.
//!
//! A message says what is wrong, in words that make sense in any frontend. What
//! a person can do about it is a [`Remedy`](crate::remedy::Remedy), which each
//! frontend renders its own way (the CLI as a command, the desktop app as a
//! button); [`crate::remedy::remedies_of`] reads them off an error.
use std::path::PathBuf;

/// The checkout has no `devflow.toml`.
#[derive(Debug, Clone)]
pub struct NotInitialized {
    pub root: PathBuf,
}

impl std::fmt::Display for NotInitialized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no devflow.toml in {}", self.root.display())
    }
}

impl std::error::Error for NotInitialized {}

/// Which refused shape a configuration file has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShapeKind {
    /// The top-level `[postgres]` table of an earlier 1.0 build: devflow can
    /// rewrite it (see [`crate::config_edit::migrate`]).
    PreRelease,
    /// A devflow 0.x configuration: never adopted, never rewritten.
    Legacy,
    /// 1.0 keys mixed with 0.x keys.
    Mixed,
}

/// A configuration file has a shape devflow 1.0 refuses. `path` is the file
/// that has it (the project file, `devflow.local.toml` or a 0.x file).
#[derive(Debug, Clone)]
pub struct ConfigShape {
    pub kind: ShapeKind,
    pub path: PathBuf,
    pub message: String,
}

impl std::fmt::Display for ConfigShape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigShape {}

/// What holds a workspace's lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BusyWith {
    /// A setup run (or a removal): the receipt lock.
    #[default]
    Setup,
    /// A start, stop, restart, removal or instance replacement of the
    /// workspace's pitchfork daemons: the process lock.
    Processes,
}

/// Another devflow operation holds this workspace's lock.
#[derive(Debug, Clone)]
pub struct Busy {
    pub workspace: String,
    pub with: BusyWith,
}

impl Busy {
    /// A setup run (the receipt lock) is in the way.
    pub fn setup(workspace: &str) -> Self {
        Busy {
            workspace: workspace.to_string(),
            with: BusyWith::Setup,
        }
    }

    /// Another operation on the workspace's daemons (the process lock) is in
    /// the way.
    pub fn processes(workspace: &str) -> Self {
        Busy {
            workspace: workspace.to_string(),
            with: BusyWith::Processes,
        }
    }
}

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.with {
            BusyWith::Setup => write!(
                f,
                "setup is already running for '{}'; wait for it to finish",
                self.workspace
            ),
            BusyWith::Processes => write!(
                f,
                "another devflow operation is starting, stopping or replacing the daemons of \
                 '{}'; wait for it to finish",
                self.workspace
            ),
        }
    }
}

impl std::error::Error for Busy {}

/// A removal protection refused the operation before any effect.
#[derive(Debug, Clone)]
pub struct Protected {
    pub workspace: String,
    pub reason: String,
    /// `--force` would override this protection (dirty, locked, teardown
    /// failure, untrusted teardown); `false` for the default workspace and
    /// the caller's own workspace.
    pub forceable: bool,
    /// What can lift the protection besides force (approve the teardown
    /// hooks, ...); see [`crate::remedy::remedies_of`].
    pub remedies: Vec<crate::remedy::Remedy>,
}

impl std::fmt::Display for Protected {
    /// The reason only: that `force` overrides it is `forceable`, which each
    /// frontend words its own way.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for Protected {}

/// The configuration file changed on disk since the editor opened it.
#[derive(Debug, Clone)]
pub struct ConfigChanged {
    pub path: PathBuf,
    /// A save that landed while devflow was writing could not be put back
    /// where it was; it is kept in this `<name>.devflow-conflict-<timestamp>`
    /// file next to `path`.
    pub kept: Option<PathBuf>,
}

impl ConfigChanged {
    /// The file changed before anything was written; nothing was moved aside.
    pub fn new(path: PathBuf) -> Self {
        Self { path, kept: None }
    }
}

impl std::fmt::Display for ConfigChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} changed on disk; reload before saving",
            self.path.display()
        )?;
        if let Some(kept) = &self.kept {
            write!(
                f,
                " (the version saved meanwhile is kept at {}; compare it with {} before deleting it)",
                kept.display(),
                self.path.display()
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigChanged {}

/// A plan no longer describes the checkout: a file it read or would write
/// changed (or appeared, or went), or so did what detection read. Nothing
/// was overwritten; make a new plan. The message says what changed and, when
/// an earlier apply is unfinished, how it is left.
#[derive(Debug, Clone)]
pub struct StalePlan {
    pub message: String,
    /// The repository the plan was for; empty when it is not known.
    pub root: PathBuf,
}

impl std::fmt::Display for StalePlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StalePlan {}

/// A frontend asked to change a workspace's container-service branch, naming
/// the workspace instance and branch it had shown, and the workspace holds
/// others now (`setup --new-instance` or a rebind ran in between). Nothing
/// was changed.
#[derive(Debug, Clone)]
pub struct WorkspaceChanged {
    pub workspace: String,
    pub service: String,
    /// What differs, for the message.
    pub detail: String,
}

impl std::fmt::Display for WorkspaceChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the workspace changed since you reviewed it: {} (service '{}' of '{}'); nothing was \
             changed, look at it again",
            self.detail, self.service, self.workspace
        )
    }
}

impl std::error::Error for WorkspaceChanged {}

/// A frontend asked to start or stop a workspace's processes, naming the
/// workspace instance (the receipt's `instance_id`) it had shown, and the
/// checkout at that path holds another one now (`setup --new-instance`, or a
/// different workspace took the path). Nothing was started or stopped.
#[derive(Debug, Clone)]
pub struct InstanceChanged {
    pub workspace: String,
    /// What differs, for the message.
    pub detail: String,
}

impl std::fmt::Display for InstanceChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the workspace changed since you reviewed it: {} ('{}'); nothing was started or \
             stopped, look at it again",
            self.detail, self.workspace
        )
    }
}

impl std::error::Error for InstanceChanged {}

/// The PostgreSQL project lock connection died: another process may now be
/// working on the project, so the operation stopped without touching anything
/// else. Finishing interrupted work settles what it left.
#[derive(Debug, Clone)]
pub struct LockLost {
    /// The project's root, for the remedy.
    pub root: PathBuf,
}

impl std::fmt::Display for LockLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "the project lock connection was lost, so the operation stopped; finish the \
             interrupted work before retrying",
        )
    }
}

impl std::error::Error for LockLost {}

/// One workspace whose receipt could not be read, so its instance is not
/// known.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct UnreadReceipt {
    pub workspace: String,
    /// Why (a position and key path, never the receipt's content).
    pub reason: String,
}

/// Who owns a branch is not known: the scan of the workspaces' receipts could
/// not read all of them, and an unreadable receipt may hold the instance that
/// owns the branch. An orphan is only ever decided from a complete scan, so
/// nothing was dropped.
#[derive(Debug, Clone)]
pub struct OwnershipUnknown {
    pub problems: Vec<UnreadReceipt>,
}

impl std::fmt::Display for OwnershipUnknown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cannot tell whether a workspace owns the branch: {} receipt(s) could not be read ({}); \
             nothing was dropped. Repair or remove them, or drop the branch by its id",
            self.problems.len(),
            self.problems
                .iter()
                .map(|p| format!("{}: {}", p.workspace, p.reason))
                .collect::<Vec<_>>()
                .join("; ")
        )
    }
}

impl std::error::Error for OwnershipUnknown {}
