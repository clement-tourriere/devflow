//! Claude Code's `WorktreeCreate` and `WorktreeRemove` hooks.
//!
//! Contract (Claude Code hooks reference, verified Oct 2026): the create hook
//! gets `{"session_id", "transcript_path", "cwd", "hook_event_name", "name"}` on
//! standard input and must print the absolute path of the worktree it made as
//! the last non-empty line of standard output; any other output on standard
//! output is read as part of the path, so everything else goes to standard
//! error. A non-zero exit fails the creation. The remove hook gets
//! `worktree_path`, must delete the directory (exit 0 counts as removed) and a
//! non-zero exit fails the removal when the directory is still there. Claude
//! Code never deletes the branch of a hook-made worktree.
use crate::project::Project;
use crate::receipt::{Phase, PhaseState, Status};
use crate::remedy::{Problem, Remedy};
use crate::setup::{
    self, CreateRequest, ExpectedWorkspace, Progress, RemoveOptions, RemoveOutcome, SetupOptions,
    SetupOutcome,
};
use crate::worktree::{self, Workspace};
use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::{Path, PathBuf};

/// The workspace (and branch) a Claude Code worktree `name` becomes. Claude
/// Code's own worktrees are on `worktree-<name>`; so are these.
pub fn workspace_name(name: &str) -> String {
    format!("worktree-{name}")
}

/// What `WorktreeCreate` hands the hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateInput {
    pub name: String,
    /// The session's directory, which tells which project is meant.
    pub cwd: Option<PathBuf>,
}

fn read_input(text: &str, what: &str) -> Result<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|_| anyhow::anyhow!("standard input is not the JSON document {what} sends"))?;
    anyhow::ensure!(
        value.is_object(),
        "standard input is not the JSON document {what} sends"
    );
    Ok(value)
}

/// Parse the JSON Claude Code writes to a `WorktreeCreate` hook.
pub fn parse_create_input(text: &str) -> Result<CreateInput> {
    let value = read_input(text, "WorktreeCreate")?;
    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .context("the WorktreeCreate document has no \"name\"")?;
    Ok(CreateInput {
        name: name.to_string(),
        cwd: value.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from),
    })
}

/// What `WorktreeRemove` hands the hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveInput {
    pub worktree_path: PathBuf,
    /// The session's directory, which tells the project when the worktree's
    /// own directory is already gone.
    pub cwd: Option<PathBuf>,
}

/// Parse the JSON Claude Code writes to a `WorktreeRemove` hook.
pub fn parse_remove_input(text: &str) -> Result<RemoveInput> {
    let value = read_input(text, "WorktreeRemove")?;
    let path = value
        .get("worktree_path")
        .and_then(|v| v.as_str())
        .filter(|path| !path.is_empty())
        .context("the WorktreeRemove document has no \"worktree_path\"")?;
    Ok(RemoveInput {
        worktree_path: PathBuf::from(path),
        cwd: value.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from),
    })
}

#[derive(Debug)]
pub struct ClaudeCreated {
    pub workspace: Workspace,
    /// `false` when the worktree already existed and was entered again (Claude
    /// Code reuses a name).
    pub created: bool,
    pub outcome: SetupOutcome,
}

/// Create (or enter again) the workspace for a Claude Code worktree named
/// `name`, non-interactively, and make sure it is ready.
///
/// A workspace that is not ready after the run is an error carrying remedies:
/// the hook fails, Claude Code reports that the creation failed, and the
/// worktree stays where it is, so running the creation again (after the person
/// approved what it asks to run, say) resumes it.
pub fn claude_create(
    project: &Project,
    name: &str,
    progress: &mut dyn Progress,
) -> Result<ClaudeCreated> {
    let workspace_name = workspace_name(name);
    let options = SetupOptions {
        non_interactive: true,
        ..SetupOptions::default()
    };
    let (outcome, created) = match worktree::find(project, &workspace_name)? {
        Some(existing) if existing.missing => {
            return Err(Problem::new(format!(
                "the worktree of '{workspace_name}' is registered with Git but its folder is gone"
            ))
            .with(Remedy::PruneWorktrees {
                root: project.root.clone(),
            })
            .into_error());
        }
        Some(existing) => {
            reusable(project, &existing, &workspace_name)?;
            (
                setup::activate(project, &existing, options, progress)?,
                false,
            )
        }
        None => (
            setup::create_and_activate(
                project,
                CreateRequest {
                    name: workspace_name,
                    from: None,
                    base: None,
                    options,
                },
                progress,
            )?,
            true,
        ),
    };
    // Whatever was entered, it must not be the repository itself: Claude Code
    // would work in (and may later remove) the main checkout.
    if outcome.workspace.is_default || outcome.workspace.path == project.root {
        return Err(Problem::new(format!(
            "'{}' is the main checkout, not a worktree for Claude Code",
            outcome.workspace.name
        ))
        .into_error());
    }
    if outcome.status != Status::Ready {
        return Err(not_ready(&outcome));
    }
    Ok(ClaudeCreated {
        workspace: outcome.workspace.clone(),
        created,
        outcome,
    })
}

/// A workspace found under the name of a Claude Code worktree is entered again
/// only when devflow created it as that worktree: never the main checkout (it
/// answers to the name when it is on that branch), and never a checkout whose
/// receipt does not say so. A worktree somebody made on the branch
/// `worktree-NAME` themselves, adopted by `devflow setup` or not set up at
/// all, is not Claude Code's: it would become eligible for Claude's cleanup.
/// A creation that stopped before its receipt was written is not resumed
/// either; the remedy points at the checkout.
fn reusable(project: &Project, existing: &Workspace, workspace_name: &str) -> Result<()> {
    if existing.is_default || existing.path == project.root {
        return Err(Problem::new(format!(
            "'{workspace_name}' is the main checkout (it is on that branch); a worktree for Claude Code needs another name"
        ))
        .into_error());
    }
    let receipt = setup::receipt(project, existing)?;
    // A receipt written by a creation names the workspace it was made from;
    // an adopted checkout has none.
    let created_here = receipt
        .as_ref()
        .is_some_and(|r| r.workspace == workspace_name && r.code_source.is_some());
    if !created_here {
        return Err(Problem::new(format!(
            "the checkout at {} is on the branch '{workspace_name}' but devflow did not create it for Claude Code; it is left alone. Use another name, or remove that worktree first",
            existing.path.display()
        ))
        .with(Remedy::Reveal {
            path: existing.path.clone(),
        })
        .into_error());
    }
    Ok(())
}

/// A workspace that did not become ready, as an error that says why.
fn not_ready(outcome: &SetupOutcome) -> anyhow::Error {
    let mut reasons = Vec::new();
    for phase in Phase::ALL {
        let record = outcome.receipt.phase(phase);
        let skipped = record.state == PhaseState::Skipped && record.required_skip;
        if record.state == PhaseState::Failed || skipped {
            reasons.push(format!(
                "{}: {}",
                phase.as_str(),
                record.detail.as_deref().unwrap_or("no details recorded")
            ));
        }
    }
    let mut problem = Problem::new(format!(
        "{} is at {} but is not ready{}",
        outcome.workspace.name,
        outcome.workspace.path.display(),
        if reasons.is_empty() {
            String::new()
        } else {
            format!(" ({})", reasons.join("; "))
        }
    ));
    for remedy in &outcome.remedies {
        problem = problem.with(remedy.clone());
    }
    if outcome.remedies.is_empty() {
        problem = problem.with(Remedy::Setup {
            workspace: outcome.workspace.path.clone(),
        });
    }
    problem.into_error()
}

#[derive(Debug)]
pub enum ClaudeRemoved {
    Removed(Box<RemoveOutcome>),
    /// The directory is already gone.
    AlreadyGone,
    /// The path is not the root of a devflow workspace (some other hook's
    /// worktree): nothing was done.
    NotOurs(&'static str),
}

/// Take down the workspace whose checkout is registered at `path`. Never
/// forces: a dirty worktree, a failing teardown hook, daemons whose state is
/// unknown (a gone checkout's daemons are never stopped by devflow, so they
/// refuse a removal that is not forced) or a removal that stopped half-way is
/// an error that says why, and Claude Code keeps the worktree while its
/// directory exists. The Git branch is kept.
///
/// `path` is the one this module printed: absolute, with no link in it. Any
/// symbolic link on the way is refused before anything is resolved, so a
/// stored path that became a link to another workspace can never select that
/// one; the workspace removed is the one Git registered at exactly that path,
/// and its directory must belong to that registration (its Git directory is
/// the registered one). A failure to find out whose a worktree is, an invalid
/// `devflow.toml` included, is an error and not "not ours": Claude Code reads
/// exit 0 as removed. Only a path that is positively foreign (not in a Git
/// repository, or in one without a `devflow.toml`) exits 0 without doing
/// anything.
///
/// A directory that is already gone is still a registered worktree to Git and
/// may own data: when `cwd` names the project, the removal runs for it too.
pub fn claude_remove(
    path: &Path,
    cwd: Option<&Path>,
    progress: &mut dyn Progress,
) -> Result<ClaudeRemoved> {
    let path = plain_path(path)?;
    // Absent only when the system says so; any other failure to look at the
    // path is an error (Claude Code reads exit 0 as removed).
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return remove_gone(&path, cwd, progress);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("cannot look at {}", path.display()));
        }
    }
    let Some(project) = project_of(&path)? else {
        return Ok(ClaudeRemoved::NotOurs(
            "it is not in a repository that devflow manages",
        ));
    };
    let Some(workspace) = registered(&project, &path)? else {
        return Ok(ClaudeRemoved::NotOurs("it is not a registered worktree"));
    };
    if workspace.is_default {
        return Ok(ClaudeRemoved::NotOurs("it is the main checkout"));
    }
    verify_registration(&project, &workspace, &path)?;
    remove_workspace(&project, workspace, progress)
}

/// The administrative directory Git registered for the worktree at `path`,
/// found from Git's own side: `<common>/worktrees/<id>` is the one whose
/// `gitdir` backlink names `<path>/.git`. It does not depend on the checkout,
/// whose `.git` file anyone can point anywhere.
fn registered_admin(project: &Project, path: &Path) -> Result<PathBuf> {
    let repo = project.repo()?;
    let commondir = repo.commondir().to_path_buf();
    let wanted = path.join(".git");
    let mut found = Vec::new();
    let names: Vec<String> = repo
        .worktrees()?
        .iter()
        .filter_map(|name| name.ok().flatten().map(str::to_string))
        .collect();
    for name in names {
        let admin = commondir.join("worktrees").join(&name);
        let Ok(backlink) = std::fs::read_to_string(admin.join("gitdir")) else {
            continue;
        };
        if Path::new(backlink.trim()) == wanted {
            found.push(admin);
        }
    }
    match found.as_slice() {
        [admin] => Ok(admin.clone()),
        [] => anyhow::bail!(
            "Git has no registration whose backlink names {}; nothing was removed",
            wanted.display()
        ),
        _ => anyhow::bail!(
            "more than one registration names {}; nothing was removed",
            wanted.display()
        ),
    }
}

/// The checkout at `path` belongs to the registration found for it: its `.git`
/// is a plain file (not a link) whose `gitdir:` line names the administrative
/// directory Git's side gave, and that is the directory the workspace's
/// receipt would be read from. A `.git` file pointed at another worktree's
/// administrative directory fails here, so its receipt, data and registration
/// are never the ones consumed.
fn verify_registration(project: &Project, workspace: &Workspace, path: &Path) -> Result<()> {
    let admin = registered_admin(project, path)?;
    let dot_git = path.join(".git");
    let meta = std::fs::symlink_metadata(&dot_git)
        .with_context(|| format!("cannot look at {}", dot_git.display()))?;
    anyhow::ensure!(
        meta.is_file(),
        "{} is not the plain file a linked worktree has; nothing was removed",
        dot_git.display()
    );
    let text = std::fs::read_to_string(&dot_git)
        .with_context(|| format!("cannot read {}", dot_git.display()))?;
    let named = text
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))
        .map(|rest| path.join(rest.trim()))
        .context("the checkout's .git file names no Git directory")?;
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    anyhow::ensure!(
        same(&named, &admin) && same(&workspace.git_dir, &admin),
        "{} is not the checkout Git registered for '{}' (its .git names another Git directory); nothing was removed",
        path.display(),
        workspace.name
    );
    Ok(())
}

/// `path` as given, which must be absolute and name no symbolic link: a link
/// would let a stored path stand for another worktree.
fn plain_path(path: &Path) -> Result<PathBuf> {
    use std::path::Component;
    anyhow::ensure!(
        path.is_absolute(),
        "the worktree path must be absolute, not {}",
        path.display()
    );
    let mut clean = PathBuf::new();
    let mut below_missing = false;
    for part in path.components() {
        match part {
            Component::CurDir | Component::ParentDir => {
                anyhow::bail!("the worktree path {} is not normalized", path.display())
            }
            Component::Normal(_) => {
                clean.push(part);
                if !below_missing {
                    match std::fs::symlink_metadata(&clean) {
                        Ok(meta) if meta.file_type().is_symlink() => {
                            return Err(Problem::new(format!(
                                "{} goes through a symbolic link ({}); nothing was removed",
                                path.display(),
                                clean.display()
                            ))
                            .into_error());
                        }
                        Ok(_) => {}
                        // Absent: nothing below it can be a link either.
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            below_missing = true;
                        }
                        Err(error) => {
                            return Err(error)
                                .with_context(|| format!("cannot look at {}", clean.display()));
                        }
                    }
                }
            }
            other => clean.push(other),
        }
    }
    Ok(clean)
}

/// The project whose repository contains `start`; `None` when `start` is
/// positively not a devflow project's (outside any repository, or in one with
/// no `devflow.toml`). Anything else that goes wrong is an error.
fn project_of(start: &Path) -> Result<Option<Project>> {
    match git2::Repository::discover(start) {
        Ok(_) => {}
        Err(error) if error.code() == git2::ErrorCode::NotFound => return Ok(None),
        Err(error) => {
            return Err(anyhow::anyhow!(
                "cannot look at the repository of {}: {}",
                start.display(),
                error.message()
            ));
        }
    }
    match Project::discover(start) {
        Ok(project) => Ok(Some(project)),
        Err(error)
            if error
                .downcast_ref::<crate::error::NotInitialized>()
                .is_some() =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// The workspace Git registered at exactly `path`.
fn registered(project: &Project, path: &Path) -> Result<Option<Workspace>> {
    Ok(worktree::list(project)?
        .into_iter()
        .find(|workspace| workspace.path == path))
}

/// The directory is gone: remove the registration and what it owned, if Git
/// still has the worktree registered under that path.
fn remove_gone(
    path: &Path,
    cwd: Option<&Path>,
    progress: &mut dyn Progress,
) -> Result<ClaudeRemoved> {
    let Some(cwd) = cwd else {
        return Ok(ClaudeRemoved::AlreadyGone);
    };
    let Some(project) = project_of(cwd)? else {
        return Ok(ClaudeRemoved::AlreadyGone);
    };
    let Some(workspace) = registered(&project, path)? else {
        return Ok(ClaudeRemoved::AlreadyGone);
    };
    if workspace.is_default || !workspace.missing {
        return Ok(ClaudeRemoved::AlreadyGone);
    }
    remove_workspace(&project, workspace, progress)
}

fn remove_workspace(
    project: &Project,
    workspace: Workspace,
    progress: &mut dyn Progress,
) -> Result<ClaudeRemoved> {
    let instance_id = setup::receipt(project, &workspace)
        .ok()
        .flatten()
        .map(|receipt| receipt.instance_id);
    let options = RemoveOptions {
        // The caller is the host, not a person standing in the worktree.
        cwd: Some(project.root.clone()),
        expect: Some(ExpectedWorkspace {
            path: workspace.path.clone(),
            instance_id,
            fingerprint: workspace.fingerprint.clone(),
            missing: false,
        }),
        ..RemoveOptions::default()
    };
    let outcome = setup::remove(
        project,
        &workspace.path.to_string_lossy(),
        options,
        progress,
    )?;
    Ok(ClaudeRemoved::Removed(Box::new(outcome)))
}

/// Keeps standard output clean for the one line a Claude Code hook may print.
///
/// Anything a child process or a library writes to standard output would be
/// read by Claude Code as part of the worktree path, so for the whole run
/// descriptor 1 points at standard error and the real standard output is held
/// here until [`StdoutGuard::print_line`].
pub struct StdoutGuard {
    original: Option<std::fs::File>,
}

impl StdoutGuard {
    #[cfg(unix)]
    pub fn redirect() -> std::io::Result<StdoutGuard> {
        use std::os::fd::FromRawFd;
        // SAFETY: plain descriptor duplication; the duplicate is owned by the
        // returned `File`, and descriptor 1 is pointed at descriptor 2.
        unsafe {
            // Close-on-exec: a child the hook starts and leaves running
            // (`sleep 600 &`) must not keep Claude Code's pipe open.
            let original = libc::fcntl(1, libc::F_DUPFD_CLOEXEC, 3);
            if original < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::dup2(2, 1) < 0 {
                let error = std::io::Error::last_os_error();
                libc::close(original);
                return Err(error);
            }
            Ok(StdoutGuard {
                original: Some(std::fs::File::from_raw_fd(original)),
            })
        }
    }

    #[cfg(not(unix))]
    pub fn redirect() -> std::io::Result<StdoutGuard> {
        Ok(StdoutGuard { original: None })
    }

    /// Print `line` on the real standard output, last.
    pub fn print_line(mut self, line: &str) -> std::io::Result<()> {
        match self.original.take() {
            Some(mut file) => {
                writeln!(file, "{line}")?;
                file.flush()
            }
            None => {
                let mut out = std::io::stdout();
                writeln!(out, "{line}")?;
                out.flush()
            }
        }
    }
}

/// The path to print: absolute, without `.` or `..` and through no symbolic
/// link (Claude Code refuses such a path).
pub fn printable_path(workspace: &Workspace) -> Result<String> {
    let canonical = workspace
        .path
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", workspace.path.display()))?;
    match canonical.to_str() {
        Some(text) if !text.contains('\n') => Ok(text.to_string()),
        _ => bail!("the worktree path cannot be printed on one line"),
    }
}
