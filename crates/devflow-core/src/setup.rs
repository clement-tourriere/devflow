//! The workspace pipeline: materialize → payload → services → env → hooks.
//!
//! Every phase is recorded in the workspace's receipt before its effect
//! (`started`) and after it (`done`/`skipped`/`failed`), so a crashed or
//! interrupted run resumes at the first incomplete phase. A run holds the
//! receipt lock; a second concurrent run fails with the typed [`Busy`] error.
use crate::config::Engine;
use crate::conn;
use crate::cow::EntryReport;
use crate::db::{
    BranchInfo, DataSource, RecordStatus, ResolvedSource, ScratchInfo, Server, SnapshotInfo,
};
use crate::error::{Busy, WorkspaceChanged};
use crate::hooks::{self, HookPhase, HookReport};
use crate::pitchfork::Pitchfork;
use crate::processes;
use crate::project::Project;
use crate::receipt::{
    self, DataBinding, ExtraBinding, PendingBranch, Phase, PhaseState, Receipt, ReceiptLock,
    ServiceBinding, Status,
};
use crate::remedy::{Problem, Remedy};
use crate::services::catalog::{BranchRecord, RecordState, SnapshotRecord, SourceRef};
use crate::services::{self, Adopted, NewBranch, Service};
use crate::trust::{self, Approval, TrustState, Verdict};
use crate::worktree::{self, Workspace, protected, protected_with};
use crate::{cow, env_file};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

mod mise_gate;

const UNTRUSTED: &str =
    "setup hooks are not trusted; review and approve them, then run setup again";
const UNTRUSTED_ENV: &str = "the [env] recipes and presets are not trusted; review and approve \
     them, then run setup again";
/// What a run reviewed is not what the workspace holds (an existing branch
/// with other hooks, a pitchfork file that changed): nothing ran on that
/// approval.
const DIFFERS: &str = "this workspace's configuration differs from what you reviewed; \
     review it again in the workspace, then run setup again";

/// The service key of a URL-only attachment (`devflow setup --database-url`)
/// in a project whose configuration has no PostgreSQL service.
const DEFAULT_POSTGRES_KEY: &str = "db";

/// Progress callbacks for frontends (CLI prints, GUI updates entities).
pub trait Progress {
    fn phase(&mut self, phase: Phase, state: PhaseState, detail: Option<&str>);
    /// A line of hook output.
    fn output(&mut self, line: &str);
}

pub struct NoProgress;

impl Progress for NoProgress {
    fn phase(&mut self, _: Phase, _: PhaseState, _: Option<&str>) {}
    fn output(&mut self, _: &str) {}
}

#[derive(Debug, Clone, Default)]
pub struct SetupOptions {
    /// Data origin for a new branch. Default: the code source's branch
    /// (the default workspace when adopting).
    pub data_from: Option<DataSource>,
    /// Attach to an existing database instead of provisioning one.
    pub database_url: Option<String>,
    pub no_db: bool,
    pub no_hooks: bool,
    /// The user's approval of a reviewed scope for this run: hooks and
    /// `[env]` recipes run even if the approval on record does not cover them
    /// (callers store it with `trust::approve_scope_at` for "always"), but only
    /// if the workspace's scope, read after its files exist, is exactly the one
    /// that was reviewed; otherwise the run is refused as unapproved and the
    /// outcome says why. `DEVFLOW_TRUST_HOOKS=1` approves without one.
    pub approval: Option<Approval>,
    /// Run the setup hooks without first checking that mise hands them the
    /// workspace's variables (`--skip-mise-check`). Recorded in the receipt.
    pub skip_mise_check: bool,
    /// The pitchfork asked, before a new instance identity replaces the
    /// receipt's, whether the old identity's daemons are gone; the one on
    /// `PATH` when `None`.
    pub pitchfork: Option<Pitchfork>,
}

#[derive(Debug, Clone)]
pub struct CreateRequest {
    pub name: String,
    /// Code origin workspace name. Default: the default workspace.
    pub from: Option<String>,
    pub options: SetupOptions,
}

#[derive(Debug, Clone, Serialize)]
pub struct SetupOutcome {
    pub workspace: Workspace,
    pub receipt: Receipt,
    pub status: Status,
    pub payload: Vec<EntryReport>,
    pub hooks: Option<HookReport>,
    pub warnings: Vec<String>,
    /// What a person can do about the outcome (approve what the workspace asks
    /// to run, run setup again), most useful first; empty for a ready
    /// workspace. See [`remedies_for`].
    #[serde(
        serialize_with = "crate::remedy::serialize_remedies",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub remedies: Vec<Remedy>,
    /// The run was given an approval of other content than the workspace
    /// holds, so its `env` and `hooks` phases were refused (they read
    /// `needs_attention`, nothing ran). A frontend can show the workspace's
    /// own scope and ask again.
    #[serde(skip)]
    pub approval_differs: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceStatus {
    pub workspace: Workspace,
    pub receipt: Option<Receipt>,
    /// `None` for checkouts devflow has not set up.
    pub status: Option<Status>,
    /// A setup run currently holds this workspace's lock.
    pub busy: bool,
}

impl WorkspaceStatus {
    /// What a person can do about this workspace's state: see [`remedies_for`].
    /// A workspace whose directory is gone has none here (that is the
    /// project's to settle: forget the registration or remove the workspace).
    pub fn remedies(&self, project: &Project) -> Vec<Remedy> {
        if self.workspace.missing {
            return Vec::new();
        }
        remedies_for(
            project,
            &self.workspace.path,
            self.receipt.as_ref(),
            self.busy,
        )
    }
}

/// What a person can do about the workspace at `path` whose receipt is
/// `receipt` (`None` for a checkout devflow has not set up): nothing while
/// a setup run holds it or when it is ready; otherwise approve what it asks
/// to run when that is what setup skipped (and the approval on record does not
/// cover it), then run setup. Decided from the receipt's phases and the trust
/// state, never from the wording of a reason.
pub fn remedies_for(
    project: &Project,
    path: &Path,
    receipt: Option<&Receipt>,
    busy: bool,
) -> Vec<Remedy> {
    remedies_given(path, receipt, busy, || {
        matches!(
            trust::state_at(project, path),
            Ok(TrustState::Untrusted { .. })
        )
    })
}

/// [`remedies_for`] with the question "is what the workspace asks to run
/// unapproved?" answered by `untrusted` (asked only when it matters).
fn remedies_given(
    path: &Path,
    receipt: Option<&Receipt>,
    busy: bool,
    untrusted: impl FnOnce() -> bool,
) -> Vec<Remedy> {
    if busy {
        return Vec::new();
    }
    let setup = Remedy::Setup {
        workspace: path.to_path_buf(),
    };
    let Some(receipt) = receipt else {
        return vec![setup];
    };
    if receipt.status() == Status::Ready {
        return Vec::new();
    }
    let awaiting_review = [Phase::Env, Phase::Hooks].into_iter().any(|phase| {
        let record = receipt.phase(phase);
        record.state == PhaseState::Skipped && record.required_skip
    });
    let mut remedies = Vec::new();
    if awaiting_review && untrusted() {
        remedies.push(Remedy::Trust {
            workspace: path.to_path_buf(),
        });
    }
    remedies.push(setup);
    remedies
}

/// Create a new workspace and run the whole pipeline. A workspace of that name
/// whose setup did not finish is resumed instead. `switch` hooks are not part
/// of creation: a caller that wants them runs [`run_switch_hooks`] afterwards.
pub fn create(
    project: &Project,
    request: CreateRequest,
    progress: &mut dyn Progress,
) -> Result<SetupOutcome> {
    let name = request.name.as_str();
    worktree::validate_name(name)?;
    // Before anything is created: an unusable URL leaves no worktree behind.
    validate_options(&request.options)?;
    let all = worktree::list(project)?;
    if let Some(existing) = all.iter().find(|w| w.branch.as_deref() == Some(name)) {
        if existing.is_default {
            return Err(Problem::new(format!(
                "'{name}' is the default workspace; switch to it instead of creating it"
            ))
            .with(Remedy::SwitchTo {
                root: project.root.clone(),
                name: name.to_string(),
            })
            .into_error());
        }
        let Some(receipt) = load_receipt(project, existing)? else {
            return Err(Problem::new(format!(
                "workspace '{name}' already exists at {} but devflow has not set it up",
                existing.path.display()
            ))
            .with(Remedy::Setup {
                workspace: existing.path.clone(),
            })
            .into_error());
        };
        ensure!(
            receipt.status() != Status::Ready,
            "workspace '{name}' already exists at {}",
            existing.path.display()
        );
        // Resuming never honours `DEVFLOW_INSTANCE_ID`: see [`setup`].
        let outcome = setup_as(
            project,
            existing,
            request.options,
            false,
            false,
            None,
            progress,
        )?;
        return outcome.context("setup did not run");
    }

    let from = match &request.from {
        Some(from) => Some(
            worktree::find(project, from)?
                .with_context(|| format!("no workspace named '{from}' to create from"))?,
        ),
        None => None,
    };
    let code_source = match &from {
        Some(from) => from.name.clone(),
        None => all[0].name.clone(),
    };

    progress.phase(Phase::Materialize, PhaseState::Started, None);
    let workspace = worktree::create(project, name, from.as_ref())?;
    let _lock = lock(&workspace)?;
    let mut receipt = Receipt::new(project.id(), &workspace.name, Some(code_source));
    let detail = format!("created {}", workspace.path.display());
    receipt.set_phase(Phase::Materialize, PhaseState::Done, Some(detail.clone()));
    receipt.save(&workspace.git_dir)?;
    progress.phase(Phase::Materialize, PhaseState::Done, Some(&detail));

    let mut runner = Runner::new(project, workspace, receipt, &request.options, progress);
    runner.run(&Phase::ALL)?;
    Ok(runner.finish())
}

/// Set up (or resume) an existing worktree. With `if_new`, does nothing when a
/// ready receipt exists; when another run holds the workspace's lock and there
/// is nothing ready to leave alone, that is [`Busy`](crate::error::Busy), not a
/// silent skip. `new_instance` replaces the receipt's instance id (VM forks)
/// and forgets its data binding; so does a `DEVFLOW_INSTANCE_ID` that names a
/// different instance than the receipt's (the receipt was copied), in which
/// case that id becomes the new identity. Repeating the same override changes
/// nothing.
///
/// This is the only entry point that reads `DEVFLOW_INSTANCE_ID`: it is an
/// orchestrator's instruction for the one workspace the explicit
/// `devflow setup` runs in, never ambient state. [`create`], [`switch`],
/// [`run_switch_hooks`], [`remove`], [`reset_data`] and [`list`] ignore it, so
/// a variable left in a shell cannot re-identify the workspace they act on.
/// An id that another worktree of the project already records is refused.
pub fn setup(
    project: &Project,
    workspace: &Workspace,
    options: SetupOptions,
    if_new: bool,
    new_instance: bool,
    progress: &mut dyn Progress,
) -> Result<Option<SetupOutcome>> {
    setup_as(
        project,
        workspace,
        options,
        if_new,
        new_instance,
        crate::receipt::instance_id_override(),
        progress,
    )
}

/// An instance id is one workspace's identity: the data branch it created is
/// found by it. Giving it to a second worktree would let either reset or drop
/// the other's data.
fn refuse_borrowed_identity(
    project: &Project,
    workspace: &Workspace,
    id: uuid::Uuid,
) -> Result<()> {
    for other in worktree::list(project)? {
        if other.git_dir == workspace.git_dir {
            continue;
        }
        let Ok(Some(receipt)) = Receipt::load(&other.git_dir) else {
            continue;
        };
        ensure!(
            receipt.project_id != project.id() || receipt.instance_id != id,
            "DEVFLOW_INSTANCE_ID {id} is the instance id recorded for workspace '{}' ({}); \
             refusing to give it to '{}'. The variable names the identity of one copied \
             workspace: unset it, or set the workspace up as a new instance",
            other.name,
            other.path.display(),
            workspace.name
        );
    }
    Ok(())
}

/// [`setup`] with the `DEVFLOW_INSTANCE_ID` override passed in.
fn setup_as(
    project: &Project,
    workspace: &Workspace,
    options: SetupOptions,
    if_new: bool,
    new_instance: bool,
    instance: Option<uuid::Uuid>,
    progress: &mut dyn Progress,
) -> Result<Option<SetupOutcome>> {
    setup_between(
        project,
        workspace,
        options,
        (if_new, new_instance, instance),
        &mut || {},
        progress,
    )
}

/// [`setup`] for a test that must act in the gap between setup's first look
/// at the receipt and its locks, as another process would: `between` runs
/// there, and `instance` stands for `DEVFLOW_INSTANCE_ID`.
#[doc(hidden)]
pub fn setup_interleaved(
    project: &Project,
    workspace: &Workspace,
    options: SetupOptions,
    (if_new, new_instance): (bool, bool),
    instance: Option<uuid::Uuid>,
    between: &mut dyn FnMut(),
    progress: &mut dyn Progress,
) -> Result<Option<SetupOutcome>> {
    setup_between(
        project,
        workspace,
        options,
        (if_new, new_instance, instance),
        between,
        progress,
    )
}

fn setup_between(
    project: &Project,
    workspace: &Workspace,
    options: SetupOptions,
    (if_new, new_instance, instance): (bool, bool, Option<uuid::Uuid>),
    between: &mut dyn FnMut(),
    progress: &mut dyn Progress,
) -> Result<Option<SetupOutcome>> {
    ensure!(
        !workspace.missing,
        "the directory of workspace '{}' is missing: {}",
        workspace.name,
        workspace.path.display()
    );
    validate_options(&options)?;
    if let Some(id) = instance {
        refuse_borrowed_identity(project, workspace, id)?;
    }
    let ready = |r: &Option<Receipt>| r.as_ref().is_some_and(|r| r.status() == Status::Ready);
    // A fork is requested explicitly, or by an instance id that is not the
    // receipt's. It comes before every "nothing to do" shortcut.
    let forks = |r: &Option<Receipt>| {
        new_instance
            || r.as_ref()
                .zip(instance)
                .is_some_and(|(r, id)| r.instance_id != id)
    };
    let before = load_receipt(project, workspace)?;
    if if_new && ready(&before) && !forks(&before) {
        return Ok(None);
    }
    // Replacing the identity is an operation on the workspace's daemons: the
    // namespace they run under goes with it. It holds the process lock, taken
    // before the receipt lock (the lock order), from here until the new
    // receipt is saved.
    let mut process_lock = if forks(&before) {
        Some(processes::process_lock(workspace)?)
    } else {
        None
    };
    // The gap in which another process can still change the receipt.
    between();
    // A ready receipt was left alone above; everything else needs this run.
    // Skipping it quietly would let a caller believe the workspace is set up.
    let lock = match acquire(&workspace.git_dir)? {
        Some(lock) => lock,
        None => return Err(busy(&workspace.name)),
    };
    // Another run may have finished while we waited for the lock. A version 1
    // receipt is migrated on disk here, which has no effect beyond the file.
    let mut existing = load_receipt_locked(project, workspace, &lock)?;
    let mut fork = forks(&existing);
    if if_new && ready(&existing) && !fork {
        return Ok(None);
    }
    if fork && process_lock.is_none() {
        // The receipt became a fork's after the first look (an orchestrator's
        // `DEVFLOW_INSTANCE_ID` against a receipt another setup replaced
        // meanwhile). The process lock comes before the check below, as on
        // the ordinary path: a start that records its intent and launches
        // under that lock is either finished and visible to the check, or
        // holds the lock and we give up. The lock order (process lock, then
        // receipt lock) is not kept here, so the lock is only tried: it
        // cannot wait for a process that is waiting for ours.
        process_lock = Some(
            Receipt::try_process_lock(&workspace.git_dir)?
                .ok_or_else(|| anyhow::Error::new(Busy::processes(&workspace.name)))?,
        );
        // What was read before that lock may no longer be true: read again,
        // under both locks.
        existing = load_receipt_locked(project, workspace, &lock)?;
        fork = forks(&existing);
        if if_new && ready(&existing) && !fork {
            return Ok(None);
        }
    }
    if fork && let Some(old) = &existing {
        // The identity is the namespace the workspace's daemons run under:
        // replacing it must not leave daemons running that devflow can no
        // longer name. A receipt copied from another machine describes that
        // machine's daemons, which `DEVFLOW_INSTANCE_ID` declares (see
        // `processes::check_replacement`).
        let copied = instance.is_some_and(|id| id != old.instance_id);
        let pitchfork = options.pitchfork.clone().or_else(Pitchfork::locate);
        processes::check_replacement(pitchfork.as_ref(), workspace, old, copied)?;
    }
    let mut receipt = match existing {
        Some(receipt) => receipt,
        None => {
            let mut receipt = Receipt::new(project.id(), &workspace.name, None);
            if let Some(id) = instance {
                receipt.instance_id = id;
            }
            receipt.set_phase(
                Phase::Materialize,
                PhaseState::Done,
                Some("adopted existing worktree".into()),
            );
            receipt
        }
    };
    if fork {
        fork_receipt(&mut receipt, &project.config, &options, instance);
    }
    receipt.save(&workspace.git_dir)?;
    // The new identity is in place; what follows does not touch the daemons.
    drop(process_lock);
    let mut runner = Runner::new(project, workspace.clone(), receipt, &options, progress);
    runner.warn_unmapped();
    runner.run(&Phase::ALL)?;
    Ok(Some(runner.finish()))
}

/// Start the containers of the workspace's container services: a workspace
/// that was already `ready` skips the services phase, but its containers may
/// have stopped (a Docker restart, `devflow service down`). Containers start
/// on activation, not when something connects. A container that cannot start
/// (its port is taken) is a warning: the switch itself happened.
fn start_containers(project: &Project, receipt: &Receipt) -> Vec<String> {
    let mut warnings = Vec::new();
    for (key, engine, _) in project.config.containers() {
        let Some(id) = services::workspace::binding(receipt, key, engine)
            .filter(|binding| !binding.orphaned)
            .and_then(|binding| binding.branch_id)
        else {
            continue;
        };
        let started = Service::open(project, key).and_then(|svc| svc.up(id));
        if let Err(error) = started {
            warnings.push(format!("could not start service '{key}': {error:#}"));
        }
    }
    warnings
}

/// Resume setup if incomplete, then run `switch` hooks. A workspace devflow has
/// not set up yet is adopted. Switch hooks only run once the workspace is
/// ready; an untrusted or failing switch hook is reported as a warning.
pub fn switch(
    project: &Project,
    workspace: &Workspace,
    options: SetupOptions,
    progress: &mut dyn Progress,
) -> Result<SetupOutcome> {
    // A switch never honours `DEVFLOW_INSTANCE_ID`: see [`setup`].
    let mut outcome = setup_as(
        project,
        workspace,
        options.clone(),
        false,
        false,
        None,
        progress,
    )?
    .context("setup did not run")?;
    match refresh_endpoint(
        project,
        workspace,
        &mut outcome.receipt,
        recipes_verdict(project, &workspace.path, options.approval.as_ref()).approved(),
    ) {
        Ok(Some(note)) => outcome.warnings.push(note),
        Ok(None) => {}
        Err(e) => outcome
            .warnings
            .push(format!("could not refresh the database URL: {e:#}")),
    }
    if !options.no_db {
        outcome
            .warnings
            .extend(start_containers(project, &outcome.receipt));
    }
    match switch_hooks(project, workspace, &outcome.receipt, &options, progress)? {
        SwitchHooks::Ran(report) => {
            if let Some(failure) = report.failure() {
                outcome.warnings.push(failure);
            }
            outcome.hooks = Some(report);
        }
        SwitchHooks::NotRun(Some((why, remedies))) => {
            outcome.warnings.push(why);
            outcome.remedies.extend(remedies);
        }
        SwitchHooks::NotRun(None) => {}
    }
    Ok(outcome)
}

/// Run the project's `switch` hooks in `workspace`, as [`switch`] does after
/// its setup. [`create`] does not run them; a caller that wants a new
/// workspace to behave like a switched-to one calls this after a successful
/// `create`. `None` means nothing ran: there are no switch hooks, `no_hooks`
/// is set, the workspace is not ready, or the hooks are not trusted
/// (`options.approval`, `DEVFLOW_TRUST_HOOKS=1`, or an approval on record).
/// A failing hook is reported in the returned [`HookReport`], not as an error.
pub fn run_switch_hooks(
    project: &Project,
    workspace: &Workspace,
    options: &SetupOptions,
    progress: &mut dyn Progress,
) -> Result<Option<HookReport>> {
    let Some(receipt) = load_receipt(project, workspace)? else {
        return Ok(None);
    };
    Ok(
        match switch_hooks(project, workspace, &receipt, options, progress)? {
            SwitchHooks::Ran(report) => Some(report),
            SwitchHooks::NotRun(_) => None,
        },
    )
}

enum SwitchHooks {
    Ran(HookReport),
    /// Did not run; the reason when the user should hear about it, with what
    /// can be done about it.
    NotRun(Option<(String, Vec<Remedy>)>),
}

fn switch_hooks(
    project: &Project,
    workspace: &Workspace,
    receipt: &Receipt,
    options: &SetupOptions,
    progress: &mut dyn Progress,
) -> Result<SwitchHooks> {
    if project.config.hooks.switch.is_empty() || options.no_hooks {
        return Ok(SwitchHooks::NotRun(None));
    }
    if receipt.status() != Status::Ready {
        return Ok(SwitchHooks::NotRun(Some((
            "switch hooks were not run: the workspace is not ready".into(),
            remedies_for(project, &workspace.path, Some(receipt), false),
        ))));
    }
    match hooks_verdict(project, &workspace.path, options.approval.as_ref())? {
        Verdict::Approved => {}
        Verdict::NotApproved => {
            return Ok(SwitchHooks::NotRun(Some((
                "switch hooks were not run: hooks are not trusted; review and approve them".into(),
                vec![Remedy::Trust {
                    workspace: workspace.path.clone(),
                }],
            ))));
        }
        Verdict::Differs => {
            return Ok(SwitchHooks::NotRun(Some((
                format!("switch hooks were not run: {DIFFERS}"),
                vec![Remedy::Trust {
                    workspace: workspace.path.clone(),
                }],
            ))));
        }
    }
    run_hooks(project, workspace, receipt, HookPhase::Switch, progress).map(SwitchHooks::Ran)
}

/// Whether the `[env]` recipes and presets may be applied in the workspace at
/// `dir`: there are none, or the scope is approved as hooks are (see
/// [`hooks_verdict`]). An unreadable approval file counts as not approved.
fn recipes_verdict(project: &Project, dir: &Path, approval: Option<&Approval>) -> Verdict {
    if project.config.env.is_empty() {
        return Verdict::Approved;
    }
    hooks_verdict(project, dir, approval).unwrap_or(Verdict::NotApproved)
}

/// Whether hooks may run in the workspace at `dir`: `DEVFLOW_TRUST_HOOKS=1`
/// is set, the approval on record covers the whole current scope of that
/// workspace (hooks, `[env]` and the pitchfork processes of `dir`, not of the
/// main checkout), or the caller's approval is of exactly that scope.
fn hooks_verdict(project: &Project, dir: &Path, approval: Option<&Approval>) -> Result<Verdict> {
    trust::verdict_at(project, dir, approval)
}

#[derive(Debug, Clone, Default)]
pub struct RemoveOptions {
    /// Allow dirty/locked worktrees and continue past teardown failures.
    pub force: bool,
    pub keep_data: bool,
    pub delete_branch: bool,
    /// The reviewed scope of the workspace being removed, approved for this
    /// run (see [`SetupOptions::approval`]); `DEVFLOW_TRUST_HOOKS=1` approves
    /// without one.
    pub approval: Option<Approval>,
    /// The pitchfork used to stop the workspace's daemons; the one on `PATH`
    /// when `None`.
    pub pitchfork: Option<Pitchfork>,
    /// The caller's working directory: removing the workspace containing it is refused.
    pub cwd: Option<std::path::PathBuf>,
    /// The workspace the caller showed the user. Once the name is resolved,
    /// and before any effect (teardown, dropping the data branch, deleting the
    /// worktree), removal is refused with a non-forceable
    /// [`Protected`](crate::error::Protected) error unless the name still
    /// resolves to this path and receipt instance. A stale view can never
    /// delete a workspace recreated in its place; `--force` does not override
    /// it.
    pub expect: Option<ExpectedWorkspace>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedWorkspace {
    /// Compared canonically with the resolved workspace's path.
    pub path: std::path::PathBuf,
    /// The receipt's instance id as [`list`] showed it; `None` when the
    /// workspace had no (readable, same-project) receipt. Must match both ways.
    pub instance_id: Option<uuid::Uuid>,
    /// [`worktree::fingerprint`] of the directory that was shown; catches a
    /// directory recreated at the same path when there is no receipt to
    /// compare. `None` skips this check.
    pub fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RemoveOutcome {
    pub workspace: Workspace,
    pub hooks: Option<HookReport>,
    pub dropped_branch: Option<uuid::Uuid>,
    pub deleted_branch: bool,
    /// The pitchfork daemons stopped before the workspace was removed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stopped_daemons: Vec<String>,
    pub warnings: Vec<String>,
}

/// A removal whose caller no longer sees what it showed the user.
fn check_expected(
    workspace: &Workspace,
    expect: &ExpectedWorkspace,
    instance: Option<uuid::Uuid>,
) -> Result<()> {
    let shown = |id: Option<uuid::Uuid>| match id {
        Some(id) => format!("receipt {id}"),
        None => "no receipt".to_string(),
    };
    let stale = |what: String| {
        protected(
            &workspace.name,
            format!(
                "workspace '{}' changed since it was shown ({what}); \
                 refresh the list and check before removing it",
                workspace.name
            ),
            false,
        )
    };
    if canonical(&expect.path) != workspace.path {
        return Err(stale(format!(
            "it was at {}, it is now at {}",
            expect.path.display(),
            workspace.path.display()
        )));
    }
    if expect.instance_id != instance {
        return Err(stale(format!(
            "it had {}, it now has {}",
            shown(expect.instance_id),
            shown(instance)
        )));
    }
    if let Some(shown_dir) = &expect.fingerprint
        && worktree::fingerprint(&workspace.path).as_ref() != Some(shown_dir)
    {
        return Err(stale("its directory was replaced".to_string()));
    }
    Ok(())
}

/// Remove a workspace. Every protection is checked before the first effect
/// (teardown hooks, then dropping the data branch, then the worktree). A
/// refusal is a [`Protected`](crate::error::Protected) error saying whether
/// `force` would override it; a running setup is [`Busy`].
pub fn remove(
    project: &Project,
    name: &str,
    options: RemoveOptions,
    progress: &mut dyn Progress,
) -> Result<RemoveOutcome> {
    let workspace =
        worktree::find(project, name)?.with_context(|| format!("no workspace named '{name}'"))?;
    // First of all, before any protection that might read or lock anything: a
    // caller's stale view must never reach a workspace recreated in its place.
    if let Some(expect) = &options.expect {
        check_expected(
            &workspace,
            expect,
            listed_receipt(project, &workspace).map(|r| r.instance_id),
        )?;
    }
    let force = options.force;
    let mut warnings = Vec::new();

    // --- protections: no side effects until all of them pass ---
    let name = workspace.name.as_str();
    if workspace.is_default {
        return Err(protected(
            name,
            format!("'{name}' is the default workspace and cannot be removed"),
            false,
        ));
    }
    if let Some(cwd) = &options.cwd
        && canonical(cwd).starts_with(&workspace.path)
    {
        return Err(protected(
            name,
            format!(
                "refusing to remove '{name}': the current directory is inside it ({})",
                workspace.path.display()
            ),
            false,
        ));
    }
    let all = worktree::list(project)?;
    if let Some(inner) = all
        .iter()
        .find(|w| w.path != workspace.path && w.path.starts_with(&workspace.path))
    {
        return Err(protected(
            name,
            format!(
                "refusing to remove '{name}': workspace '{}' lives inside it",
                inner.name
            ),
            false,
        ));
    }
    if workspace.locked && !force {
        return Err(protected(
            name,
            format!("'{name}' is locked (unlock it with `git worktree unlock`)"),
            true,
        ));
    }
    if !force && worktree::is_dirty(&workspace)? {
        return Err(protected(
            name,
            format!("'{name}' has uncommitted changes; commit or stash them"),
            true,
        ));
    }
    // What would be deleted must still be this worktree (never an unrelated
    // directory that took over a vanished one's path).
    worktree::check_removable(project, &workspace, workspace.locked)?;
    // Lock order: the process lock, then the receipt lock. A removal is one
    // operation on the workspace's daemons from the plan to the last effect:
    // nothing may start them in a worktree that is about to go.
    let process_lock = match workspace.git_dir.is_dir() {
        true => Some(processes::process_lock(&workspace)?),
        false => None,
    };
    let _lock = match workspace.git_dir.is_dir() {
        true => Some(acquire(&workspace.git_dir)?.ok_or_else(|| busy(name))?),
        false => None,
    };
    let receipt = match load_receipt(project, &workspace) {
        Ok(receipt) => receipt,
        Err(e) => {
            warnings.push(format!("{e:#}; its data and hooks are left alone"));
            None
        }
    };
    // The receipt read under the lock is the one that decides: another run may
    // have replaced it since the first look.
    if let Some(expect) = &options.expect {
        check_expected(&workspace, expect, receipt.as_ref().map(|r| r.instance_id))?;
    }

    // Daemons of this workspace that are running are stopped before anything
    // is removed (their lifecycle hooks come from the worktree's current
    // files, so this needs the approval too). Decided here, run below.
    let pitchfork = options.pitchfork.clone().or_else(Pitchfork::locate);
    // (A Git directory that is not there has no receipt and no namespace:
    // nothing to stop.)
    let running = match &process_lock {
        Some(lock) => {
            processes::plan_removal_locked(pitchfork.as_ref(), project, &workspace, force, lock)
        }
        None => Ok(None),
    }
    .map_err(|refusal| {
        protected(
            name,
            format!("cannot stop the running pitchfork daemons of '{name}': {refusal}"),
            true,
        )
    })?;

    let teardown = &project.config.hooks.teardown;
    let run_teardown = if teardown.is_empty() {
        false
    } else if workspace.missing {
        warnings.push("teardown hooks were not run: the worktree directory is missing".into());
        false
    } else {
        let verdict = hooks_verdict(project, &workspace.path, options.approval.as_ref())?;
        let trusted = verdict.approved();
        if !trusted && !force {
            return Err(protected_with(
                name,
                if verdict == Verdict::Differs {
                    "this workspace's configuration differs from what you reviewed; review its teardown hooks again in the workspace"
                } else {
                    "teardown hooks are not trusted; review and approve them"
                },
                true,
                vec![Remedy::Trust {
                    workspace: workspace.path.clone(),
                }],
            ));
        }
        if !trusted {
            warnings.push("teardown hooks were skipped because they are not trusted".into());
        }
        trusted
    };

    // Every branch this instance created for the workspace, the configured
    // service's first. Bindings of services the configuration no longer has
    // are included: the receipt disappears with the worktree, and nothing else
    // would ever find those branches again.
    let mut drop_plan: Option<(Server, Vec<uuid::Uuid>)> = None;
    if let Some(receipt) = &receipt
        && !options.keep_data
    {
        let primary = project.config.postgres().map(|(key, _)| key);
        let mut owned: Vec<(&str, uuid::Uuid)> = receipt
            .services
            .iter()
            .filter(|(_, b)| b.engine == Engine::Postgres && b.owned)
            .filter_map(|(key, b)| Some((key.as_str(), b.branch_id?)))
            .collect();
        owned.sort_by_key(|(key, _)| Some(*key) != primary);
        if !owned.is_empty() {
            match connect(project) {
                Ok(Some(server)) => {
                    let mut ids = Vec::new();
                    for (_, id) in owned {
                        match server.branch(id) {
                            Ok(Some(info)) if info.instance_id == Some(receipt.instance_id) => {
                                ids.push(id);
                            }
                            Ok(Some(_)) => warnings.push(format!(
                                "database branch {id} was created by another instance and was left alone"
                            )),
                            Ok(None) => {
                                warnings.push(format!("database branch {id} no longer exists"))
                            }
                            Err(e) => bail!(
                                "cannot inspect the database branch of '{}': {e:#}; \
                                 pass --keep-data to remove the worktree and leave the branch",
                                workspace.name
                            ),
                        }
                    }
                    if !ids.is_empty() {
                        drop_plan = Some((server, ids));
                    }
                }
                Ok(None) => warnings.push(
                    "the project has no postgres service, so the database branch was left alone"
                        .into(),
                ),
                Err(e) => bail!(
                    "cannot reach the database server to drop the branch of '{}': {e:#}; \
                     pass --keep-data to remove the worktree and leave the branch",
                    workspace.name
                ),
            }
        }
    }
    // The container branches (Valkey, Redis, MySQL) this instance created: checked
    // in the catalog now, dropped with the rest, by recorded identity.
    let mut container_drops: Vec<(Service, uuid::Uuid)> = Vec::new();
    if let Some(receipt) = &receipt
        && !options.keep_data
    {
        for (key, binding) in receipt
            .services
            .iter()
            .filter(|(_, b)| b.engine.is_container() && b.owned)
        {
            let Some(id) = binding.branch_id else {
                continue;
            };
            let inspected = Service::recorded(project, key, binding.engine)
                .and_then(|svc| Ok((svc.branch(id)?, svc)));
            match inspected {
                Ok((Some(record), svc)) if record.owner == Some(receipt.instance_id) => {
                    container_drops.push((svc, id));
                }
                Ok((Some(_), _)) => warnings.push(format!(
                    "branch {id} of service '{key}' was created by another instance and was \
                     left alone"
                )),
                Ok((None, _)) => {
                    warnings.push(format!("branch {id} of service '{key}' no longer exists"))
                }
                Err(e) => bail!(
                    "cannot inspect the branch of service '{key}' of '{}': {e:#}; pass \
                     --keep-data to remove the worktree and leave the branch",
                    workspace.name
                ),
            }
        }
    }
    if options.delete_branch
        && !force
        && let Some(branch) = &workspace.branch
        && !worktree::branch_merged(project, branch)?
    {
        return Err(protected(
            name,
            format!(
                "branch '{branch}' has commits that are not in the default workspace; merge it"
            ),
            true,
        ));
    }

    // --- effects ---
    let mut stopped_daemons = Vec::new();
    if let (Some(running), Some(lock)) = (&running, &process_lock) {
        let (ids, notes) =
            processes::stop_for_removal_locked(project, &workspace, running, force, lock)
                .map_err(|error| protected(name, format!("{error:#}"), true))?;
        stopped_daemons = ids;
        warnings.extend(notes);
    }
    let mut hook_report = None;
    if run_teardown {
        let receipt = receipt
            .clone()
            .unwrap_or_else(|| Receipt::new(project.id(), &workspace.name, None));
        let report = run_hooks(project, &workspace, &receipt, HookPhase::Teardown, progress)?;
        if let Some(failure) = report.failure() {
            if !force {
                return Err(protected(name, failure, true));
            }
            warnings.push(failure);
        }
        hook_report = Some(report);
    }

    let mut dropped_branch = None;
    if let Some((server, ids)) = drop_plan {
        for id in ids {
            match server.drop_branch(id) {
                Ok(()) => {
                    dropped_branch.get_or_insert(id);
                }
                Err(e) if force => {
                    warnings.push(format!("could not drop database branch {id}: {e:#}"))
                }
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("cannot drop database branch {id}; pass --keep-data to skip it")
                    });
                }
            }
        }
    }

    for (svc, id) in container_drops {
        match svc.remove_branch(id) {
            Ok(()) => {
                dropped_branch.get_or_insert(id);
            }
            Err(e) if force => warnings.push(format!(
                "could not drop branch {id} of service '{}': {e:#}",
                svc.key
            )),
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "cannot drop branch {id} of service '{}'; pass --keep-data to skip it",
                        svc.key
                    )
                });
            }
        }
    }

    if workspace.locked {
        worktree::force_remove(project, &workspace)?;
    } else {
        worktree::remove(project, &workspace)?;
    }

    let mut deleted_branch = false;
    if options.delete_branch {
        match &workspace.branch {
            Some(branch) => match worktree::delete_branch(project, branch, force) {
                Ok(()) => deleted_branch = true,
                Err(e) => warnings.push(format!("branch '{branch}' was not deleted: {e:#}")),
            },
            None => warnings.push("the workspace has no branch to delete".into()),
        }
    }
    Ok(RemoveOutcome {
        workspace,
        hooks: hook_report,
        dropped_branch,
        deleted_branch,
        stopped_daemons,
        warnings,
    })
}

pub fn list(project: &Project) -> Result<Vec<WorkspaceStatus>> {
    Ok(worktree::list(project)?
        .into_iter()
        .map(|workspace| {
            let receipt = listed_receipt(project, &workspace);
            let busy = receipt.is_some() && Receipt::is_locked(&workspace.git_dir);
            let status = match (&receipt, busy) {
                (Some(_), true) => Some(Status::InProgress),
                (Some(r), false) => Some(r.status()),
                (None, _) => None,
            };
            WorkspaceStatus {
                workspace,
                receipt,
                status,
                busy,
            }
        })
        .collect())
}

/// The receipt as [`list`] shows it: one that is unreadable or belongs to
/// another project is not shown, and so cannot be the receipt a caller saw.
fn listed_receipt(project: &Project, workspace: &Workspace) -> Option<Receipt> {
    load_receipt(project, workspace)
        .ok()
        .flatten()
        .filter(|r| r.project_id == project.id())
}

/// The receipt of `workspace` as setup sees it: a version 1 file is migrated
/// in memory, and what the current configuration invalidates (a service
/// added or changed since the workspace was set up) is reopened, so its
/// status is what the next setup would act on. Nothing is written, so it is
/// safe without the receipt lock. `None` for a workspace devflow has not set
/// up; an error for a receipt that is unreadable or another project's.
pub fn receipt(project: &Project, workspace: &Workspace) -> Result<Option<Receipt>> {
    load_receipt(project, workspace)
}

/// Variables written to `.devflow/env` and passed to hooks.
///
/// `DEVFLOW_WORKSPACE_ID` is informational (the receipt's instance id). It is
/// deliberately not called `DEVFLOW_INSTANCE_ID`, the orchestrator override that
/// `devflow setup` obeys: a shell that sourced this file must not carry one
/// workspace's identity into commands about another.
pub fn env_vars(
    project: &Project,
    workspace: &Workspace,
    receipt: &Receipt,
) -> Vec<(String, String)> {
    let mut vars = Vec::new();
    let mut push = |key: &str, value: String| vars.push((key.to_string(), value));
    // The postgres service's binding (see [`Receipt::postgres`]): bindings of
    // services the configuration no longer has are not exported.
    if let Some((_, binding)) = receipt.postgres(&project.config) {
        push("DATABASE_URL", binding.url.clone());
        // The `PG*` variables say what the URL says, as libpq reads it.
        match conn::Params::parse(&binding.url) {
            Ok(params) => {
                for (key, value) in params.env() {
                    push(key, value);
                }
            }
            Err(_) => {
                push("PGUSER", binding.role.clone());
                push("PGDATABASE", binding.database.clone());
            }
        }
        // The scratch databases (`extra_databases`): the branch's login and
        // password, another database, as `<NAME>_DATABASE_URL`. Only the ones
        // the configuration lists now (a removed name is an orphan on the
        // server and is no longer exported).
        if let Some((_, pg)) = project.config.postgres() {
            for name in &pg.extra_databases {
                if let Some(url) = binding.extra_url(name) {
                    push(&crate::config::extra_database_env_name(name), url);
                }
            }
        }
    }
    // Each container service's URL (with the branch's own password), under
    // the variable its section names. A binding of a service the
    // configuration no longer has is not exported.
    for (key, engine, _) in project.config.containers() {
        if let Some(binding) = receipt
            .services
            .get(key)
            .filter(|b| b.engine == engine && !b.orphaned)
            && let Some(name) = project.config.url_variable(key)
        {
            push(name, binding.url.clone());
            // A mysql service also exports its parts, for frameworks and
            // tools that want a host, a port and a login separately.
            if engine == Engine::Mysql
                && let Some(fields) = services::mysql::env_fields(&binding.url)
            {
                for (field, value) in fields {
                    push(field, value);
                }
            }
        }
    }
    push("DEVFLOW_PROJECT", project.name().to_string());
    push("DEVFLOW_PROJECT_ROOT", project.root.display().to_string());
    push("DEVFLOW_WORKSPACE", workspace.name.clone());
    push(
        "DEVFLOW_WORKSPACE_PATH",
        workspace.path.display().to_string(),
    );
    if let Some(source) = source_workspace(project, workspace, receipt).ok().flatten() {
        push("DEVFLOW_SOURCE_PATH", source.path.display().to_string());
    }
    push("DEVFLOW_WORKSPACE_ID", receipt.instance_id.to_string());
    // One Compose project per workspace, whatever the directory is called.
    push("COMPOSE_PROJECT_NAME", receipt.slug());
    vars
}

/// The variables of a workspace as `.devflow/env` holds them and hooks see
/// them: the generated ones ([`env_vars`]) plus, with `recipes`, the `[env]`
/// recipes and presets rendered against them.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceEnv {
    pub vars: Vec<(String, String)>,
    /// Values derived from credentials, to be masked whole in hook output.
    pub secrets: Vec<String>,
    /// Recipes and presets that were left out, and why.
    pub warnings: Vec<String>,
}

/// [`env_vars`] and, when `recipes` is set, the rendered `[env]` section.
pub fn workspace_env(
    project: &Project,
    workspace: &Workspace,
    receipt: &Receipt,
    recipes: bool,
) -> WorkspaceEnv {
    let mut vars = env_vars(project, workspace, receipt);
    if !recipes || project.config.env.is_empty() {
        return WorkspaceEnv {
            vars,
            ..WorkspaceEnv::default()
        };
    }
    let rendered = env_file::render(
        &project.config.env,
        &vars,
        receipt
            .postgres(&project.config)
            .map(|(_, binding)| binding.url.as_str()),
    );
    vars.extend(rendered.vars);
    WorkspaceEnv {
        vars,
        secrets: rendered.secrets,
        warnings: rendered.warnings,
    }
}

/// Replace a workspace's data with a fresh clone of `to`. The default is the
/// branch's own immutable provenance on the server (the `empty`, branch or
/// snapshot identity it was created from), never a workspace name looked up
/// again: names are reusable, so a recreated workspace of the same name must
/// not become the source of an older branch.
pub fn reset_data(
    project: &Project,
    workspace: &Workspace,
    to: Option<DataSource>,
) -> Result<BranchInfo> {
    let lock = lock(workspace)?;
    let receipt =
        load_receipt_locked(project, workspace, &lock)?.ok_or_else(|| needs_setup(workspace))?;
    let (_, binding) = receipt
        .postgres(&project.config)
        .context("this workspace has no database")?;
    let id = owned_branch(&receipt, binding)?;
    let server = connect_required(project)?;
    let info = server
        .branch(id)?
        .with_context(|| format!("database branch {id} no longer exists"))?;
    ensure!(
        info.instance_id == Some(receipt.instance_id),
        "database branch {id} was created by another workspace instance and cannot be reset from here"
    );
    let resolved = match to {
        Some(source) => {
            if let DataSource::Workspace { name } = &source {
                let other = worktree::find(project, name)?;
                ensure!(
                    other.as_ref().is_none_or(|w| w.path != workspace.path),
                    "a workspace cannot be reset from itself"
                );
            }
            resolve(project, &server, &source)?.0
        }
        None => provenance(&info)?,
    };
    ensure!(
        resolved != ResolvedSource::Branch(id),
        "a branch cannot be reset from itself"
    );
    server.reset_branch(id, resolved)
}

/// What a branch was created from, as the server recorded it.
fn provenance(info: &BranchInfo) -> Result<ResolvedSource> {
    match (info.source_kind.as_str(), info.source_id) {
        ("empty", _) => Ok(ResolvedSource::Empty),
        ("branch", Some(id)) => Ok(ResolvedSource::Branch(id)),
        ("snapshot", Some(id)) => Ok(ResolvedSource::Snapshot(id)),
        (kind, _) => bail!(
            "database branch {} has no usable recorded source ({kind}); pass one explicitly",
            info.id
        ),
    }
}

/// Snapshot a workspace's branch as a new version of `name`.
pub fn snapshot(project: &Project, workspace: &Workspace, name: &str) -> Result<SnapshotInfo> {
    crate::db::validate_snapshot_name(name)?;
    let receipt = load_receipt(project, workspace)?.ok_or_else(|| needs_setup(workspace))?;
    let id = receipt
        .postgres(&project.config)
        .and_then(|(_, binding)| binding.branch_id)
        .context("this workspace has no database branch to snapshot")?;
    connect_required(project)?.create_snapshot(name, id)
}

/// What a frontend showed of a workspace's container-service branch when the
/// person asked to change it: the workspace instance (the receipt's
/// `instance_id`) and the catalog branch its binding named. Core compares both
/// with the receipt as it is when the work starts and, if either differs (a
/// `setup --new-instance` or a rebind ran in between), refuses with the typed
/// [`WorkspaceChanged`] before any effect. The CLI passes `None`: it acts on
/// whatever the workspace holds now.
///
/// A reset also copies from a source the person chose, which a name can point
/// at differently by the time it is confirmed (another workspace's
/// `setup --new-instance`, a snapshot name given to another version). `source`
/// is that choice as the identity it resolved to when it was shown
/// ([`reset_source`]); the reset refuses unless the request resolves to the
/// same one, before any effect. `None` pins nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BranchExpect {
    pub instance: uuid::Uuid,
    pub branch: uuid::Uuid,
    /// The catalog identity the reset's source resolved to when shown.
    pub source: Option<services::catalog::SourceRef>,
}

impl BranchExpect {
    /// Is this what `workspace`'s receipt binds for service `key` now? The
    /// typed error says what differs.
    pub fn verify(&self, workspace: &Workspace, key: &str, receipt: &Receipt) -> Result<()> {
        let bound = receipt.services.get(key).and_then(|b| b.branch_id);
        let detail = if receipt.instance_id != self.instance {
            format!(
                "it is another instance now ({}, not {})",
                receipt.instance_id, self.instance
            )
        } else {
            match bound {
                Some(found) if found == self.branch => return Ok(()),
                Some(found) => format!("it is bound to branch {found}, not {}", self.branch),
                None => format!("branch {} is no longer bound to it", self.branch),
            }
        };
        Err(WorkspaceChanged {
            workspace: workspace.name.clone(),
            service: key.to_string(),
            detail,
        }
        .into())
    }
}

impl BranchExpect {
    /// Does the source a reset resolves to now (`resolved`) carry the
    /// identity that was shown? The typed error says what differs.
    pub fn verify_source(
        &self,
        workspace: &Workspace,
        key: &str,
        resolved: services::catalog::SourceRef,
    ) -> Result<()> {
        let Some(shown) = self.source else {
            return Ok(());
        };
        if shown == resolved {
            return Ok(());
        }
        Err(WorkspaceChanged {
            workspace: workspace.name.clone(),
            service: key.to_string(),
            detail: format!(
                "the data it would be reset from is {} now, not {} (a workspace was set up \
                 again, or a snapshot name points elsewhere)",
                services::workspace::encode_ref(resolved),
                services::workspace::encode_ref(shown)
            ),
        }
        .into())
    }
}

/// The source of a reset, resolved: the catalog identity it clones from and the
/// label the receipt records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetSource {
    pub source: services::catalog::SourceRef,
    pub label: String,
}

/// What a reset of `workspace`'s branch of container service `key` to `to` (or
/// to its recorded source) would clone from *now*, as identities and not names.
/// A frontend resolves this when it shows the reset and hands the identity back
/// in [`BranchExpect::source`], so the confirmation resets from what it
/// showed; it also computes the source's effects (`services::effects`) from
/// this same answer. Nothing is changed and Docker is not asked: the catalog
/// file and the receipts are read.
pub fn reset_source(
    project: &Project,
    workspace: &Workspace,
    key: &str,
    to: Option<&DataSource>,
) -> Result<ResetSource> {
    let engine = project
        .config
        .containers()
        .find(|(k, _, _)| *k == key)
        .map(|(_, engine, _)| engine)
        .with_context(|| {
            format!(
                "{} has no container service '{key}'",
                project.config_path.display()
            )
        })?;
    let receipt = load_receipt(project, workspace)?.ok_or_else(|| needs_setup(workspace))?;
    let binding = services::workspace::binding(&receipt, key, engine).with_context(|| {
        format!(
            "workspace '{}' has no branch of service '{key}'",
            workspace.name
        )
    })?;
    let id = binding.branch_id.context("the binding names no branch")?;
    let catalog = services::catalog::read_file(&services::catalog::Store::path_for(project.id())?)?;
    let records = catalog
        .as_ref()
        .and_then(|c| c.services.get(key))
        .with_context(|| format!("the catalog records nothing for service '{key}'"))?;
    let record = records
        .branch(id)
        .with_context(|| format!("service '{key}' has no branch {id}"))?;
    reset_target(
        project, key, engine, records, workspace, record, binding, to,
    )
}

/// The source a reset of `record` (the branch `binding` names) to `to` clones
/// from.
#[allow(clippy::too_many_arguments)]
fn reset_target(
    project: &Project,
    key: &str,
    engine: Engine,
    records: &services::catalog::ServiceCatalog,
    workspace: &Workspace,
    record: &BranchRecord,
    binding: &receipt::ServiceBinding,
    to: Option<&DataSource>,
) -> Result<ResetSource> {
    let (source, label) = match to {
        Some(source) => {
            if let DataSource::Workspace { name } = source {
                let other = worktree::find(project, name)?;
                ensure!(
                    other.as_ref().is_none_or(|w| w.path != workspace.path),
                    "a workspace cannot be reset from itself"
                );
            }
            services::workspace::resolve_in(project, key, engine, records, source)?
        }
        // The branch's own immutable provenance, never a name looked up again.
        None => (record.source, binding.source.clone()),
    };
    Ok(ResetSource { source, label })
}

/// The container service `key`, the receipt of `workspace` and the id of the
/// branch it is bound to. `expect` is checked against that receipt before
/// anything else is read.
fn container_binding(
    project: &Project,
    workspace: &Workspace,
    key: &str,
    expect: Option<BranchExpect>,
) -> Result<(Service, Receipt, uuid::Uuid)> {
    let receipt = load_receipt(project, workspace)?.ok_or_else(|| needs_setup(workspace))?;
    if let Some(expect) = expect {
        expect.verify(workspace, key, &receipt)?;
    }
    let svc = Service::open(project, key)?;
    let id = services::workspace::binding(&receipt, key, svc.engine)
        .and_then(|binding| binding.branch_id)
        .ok_or_else(|| {
            Problem::new(format!(
                "workspace '{}' has no branch of service '{key}'",
                workspace.name
            ))
            .with(Remedy::Setup {
                workspace: workspace.path.clone(),
            })
            .into_error()
        })?;
    Ok((svc, receipt, id))
}

/// The branch of container service `key` bound to `workspace`, from its
/// receipt and the catalog (nothing is started).
pub fn service_branch(
    project: &Project,
    workspace: &Workspace,
    key: &str,
) -> Result<(Service, BranchRecord)> {
    let (svc, _, id) = container_binding(project, workspace, key, None)?;
    let record = svc.require_branch(id)?;
    Ok((svc, record))
}

/// Snapshot the branch of container service `key` bound to `workspace` as a
/// new version of `name`: a `BGSAVE` and a reflink of `dump.rdb` into a new
/// volume, no container. With `expect`, only if the workspace still holds the
/// instance and branch the caller showed ([`WorkspaceChanged`] otherwise).
pub fn service_snapshot(
    project: &Project,
    workspace: &Workspace,
    key: &str,
    name: &str,
    expect: Option<BranchExpect>,
) -> Result<SnapshotRecord> {
    let (svc, _, id) = container_binding(project, workspace, key, expect)?;
    svc.snapshot_branch(id, name)
}

/// Replace the data of container service `key` in `workspace` with a fresh
/// clone of `to`, or of the branch's recorded source. A staged replacement is
/// started and checked before a journaled cutover; the URL (port and
/// password) does not change. Only the instance that created the branch may
/// reset it. With `expect`, the instance and branch the caller showed are
/// verified under the receipt lock before any effect: a workspace that holds
/// others now (`setup --new-instance`) is refused with [`WorkspaceChanged`].
pub fn service_reset(
    project: &Project,
    workspace: &Workspace,
    key: &str,
    to: Option<DataSource>,
    expect: Option<BranchExpect>,
) -> Result<BranchRecord> {
    let lock = lock(workspace)?;
    let mut receipt =
        load_receipt_locked(project, workspace, &lock)?.ok_or_else(|| needs_setup(workspace))?;
    if let Some(expect) = expect {
        expect.verify(workspace, key, &receipt)?;
    }
    let svc = Service::open(project, key)?;
    let binding = services::workspace::binding(&receipt, key, svc.engine).with_context(|| {
        format!(
            "workspace '{}' has no branch of service '{key}'",
            workspace.name
        )
    })?;
    ensure!(
        binding.owned,
        "this workspace's branch of '{key}' was not created by this instance ({}); set \
         the workspace up as a new instance to give it its own branch",
        receipt.instance_id
    );
    let id = binding.branch_id.context("the binding names no branch")?;
    let record = svc.require_branch(id)?;
    ensure!(
        record.owner == Some(receipt.instance_id),
        "branch {id} was created by another workspace instance and cannot be reset from here"
    );
    let ResetSource {
        source: target,
        label,
    } = reset_target(
        project,
        key,
        svc.engine,
        &svc.records()?,
        workspace,
        &record,
        binding,
        to.as_ref(),
    )?;
    // What was shown must be what is copied: a name that resolves to another
    // identity now is refused before the first effect.
    if let Some(expect) = expect {
        expect.verify_source(workspace, key, target)?;
    }
    let reset = svc.reset_branch(id, target)?;
    // The URL is unchanged (same port, same password); the report follows.
    if let Some(bound) = receipt.services.get_mut(key) {
        bound.source = label;
        bound.cloned = Some(receipt::CloneReport {
            method: reset.clone,
            captured_at: reset.captured_at,
            allocated_bytes: reset.allocated_bytes,
            source_downtime_ms: reset.downtime_ms,
        });
    }
    receipt.updated_at = chrono::Utc::now();
    receipt.save(&workspace.git_dir)?;
    Ok(reset)
}

/// Resolve a data source to server identities using local receipts.
pub fn resolve_source(
    project: &Project,
    server: &crate::db::Server,
    source: &DataSource,
) -> Result<crate::db::ResolvedSource> {
    Ok(resolve(project, server, source)?.0)
}

// ---------------------------------------------------------------------------

/// Outcome of one phase handler.
enum PhaseResult {
    Done(Option<String>),
    /// Nothing to do; the workspace is still ready. A configuration change
    /// that gives the phase work reopens it.
    Skipped(String),
    /// Skipped on request (`--no-db`, `--no-hooks`) or by design: the
    /// workspace is ready and no configuration change reopens the phase.
    SkippedOnRequest(String),
    /// Not run, and the workspace needs attention until it is.
    SkippedRequired(String),
    Failed(String),
}

/// The error of a phase that failed: the cause, under a message that says which
/// phase and where the workspace is kept. Its remedies are the cause's own
/// first (start the database, drop the lost branch, …) and then running setup
/// again, so that the retry never hides what the cause says to do
/// ([`Problem::caused_by`]: `remedies_of` reads the outermost layer only).
fn failed_phase(workspace: &Workspace, phase: Phase, cause: anyhow::Error) -> anyhow::Error {
    let problem = Problem::new(format!(
        "{} failed; '{}' is kept at {}",
        phase.as_str(),
        workspace.name,
        workspace.path.display()
    ))
    .with(Remedy::Setup {
        workspace: workspace.path.clone(),
    })
    .caused_by(&cause);
    cause.context(problem)
}

fn complete(receipt: &Receipt, phase: Phase) -> bool {
    let record = receipt.phase(phase);
    match record.state {
        PhaseState::Done => true,
        PhaseState::Skipped => !record.required_skip,
        _ => false,
    }
}

struct Runner<'a> {
    project: &'a Project,
    workspace: Workspace,
    receipt: Receipt,
    options: &'a SetupOptions,
    progress: &'a mut dyn Progress,
    payload: Vec<EntryReport>,
    hooks: Option<HookReport>,
    warnings: Vec<String>,
    /// A phase was refused because the run's approval is of other content
    /// than this workspace holds.
    differs: bool,
}

impl<'a> Runner<'a> {
    fn new(
        project: &'a Project,
        workspace: Workspace,
        receipt: Receipt,
        options: &'a SetupOptions,
        progress: &'a mut dyn Progress,
    ) -> Self {
        Self {
            project,
            workspace,
            receipt,
            options,
            progress,
            payload: Vec::new(),
            hooks: None,
            warnings: Vec::new(),
            differs: false,
        }
    }

    /// A version 1 binding the configuration could not place is reported on
    /// every run, never silently adopted.
    fn warn_unmapped(&mut self) {
        if let Some(unmapped) = &self.receipt.unmapped {
            self.warnings.push(format!(
                "this workspace's database ({}) was bound by a previous devflow version and the \
                 configuration has no postgres service to take it: it is kept as `unmapped` and \
                 not exported. Add `[services.NAME]` with `engine = \"postgres\"` to devflow.toml \
                 if it should be used",
                unmapped.database
            ));
        }
    }

    fn finish(self) -> SetupOutcome {
        let remedies = remedies_for(
            self.project,
            &self.workspace.path,
            Some(&self.receipt),
            false,
        );
        SetupOutcome {
            remedies,
            status: self.receipt.status(),
            workspace: self.workspace,
            receipt: self.receipt,
            payload: self.payload,
            hooks: self.hooks,
            warnings: self.warnings,
            approval_differs: self.differs,
        }
    }

    fn record(&mut self, phase: Phase, state: PhaseState, detail: Option<String>) -> Result<()> {
        self.receipt.set_phase(phase, state, detail.clone());
        self.receipt.save(&self.workspace.git_dir)?;
        self.progress.phase(phase, state, detail.as_deref());
        Ok(())
    }

    /// Run the incomplete phases among `phases`, in order.
    fn run(&mut self, phases: &[Phase]) -> Result<()> {
        for &phase in phases {
            if complete(&self.receipt, phase) {
                continue;
            }
            self.record(phase, PhaseState::Started, None)?;
            let result = match phase {
                Phase::Materialize => Ok(PhaseResult::Done(Some("worktree exists".into()))),
                Phase::Payload => self.payload(),
                Phase::Services => self.services(),
                Phase::Env => self.env(),
                Phase::Hooks => self.hooks(),
            };
            match result {
                Ok(PhaseResult::Done(detail)) => self.record(phase, PhaseState::Done, detail)?,
                Ok(PhaseResult::Skipped(detail)) => {
                    self.record(phase, PhaseState::Skipped, Some(detail))?
                }
                Ok(PhaseResult::SkippedOnRequest(detail)) => {
                    self.receipt.skip_deliberate(phase, detail.clone());
                    self.receipt.save(&self.workspace.git_dir)?;
                    self.progress
                        .phase(phase, PhaseState::Skipped, Some(&detail));
                }
                Ok(PhaseResult::SkippedRequired(detail)) => {
                    self.receipt.skip_required(phase, detail.clone());
                    self.receipt.save(&self.workspace.git_dir)?;
                    self.progress
                        .phase(phase, PhaseState::Skipped, Some(&detail));
                }
                Ok(PhaseResult::Failed(detail)) => {
                    self.record(phase, PhaseState::Failed, Some(detail))?
                }
                Err(error) => {
                    self.record(phase, PhaseState::Failed, Some(format!("{error:#}")))?;
                    return Err(failed_phase(&self.workspace, phase, error));
                }
            }
        }
        Ok(())
    }

    fn payload(&mut self) -> Result<PhaseResult> {
        let entries = &self.project.config.worktree.copy;
        if entries.is_empty() {
            return Ok(PhaseResult::Done(Some("nothing to copy".into())));
        }
        let Some(source) = source_workspace(self.project, &self.workspace, &self.receipt)? else {
            return Ok(PhaseResult::Skipped("this is the source checkout".into()));
        };
        self.payload = cow::copy_paths(&source.path, &self.workspace.path, entries)?;
        self.receipt.record_payload(&self.payload);
        let done = self
            .payload
            .iter()
            .filter(|e| matches!(e.result, cow::EntryResult::Done { .. }))
            .count();
        Ok(PhaseResult::Done(Some(format!(
            "{done} of {} copied from {}",
            crate::text::counted_as(self.payload.len() as u64, "entry", "entries"),
            source.path.display()
        ))))
    }

    /// Bind every configured service (PostgreSQL, Valkey, Redis, MySQL). Each
    /// service's binding is saved as soon as it is made, so a retry resumes
    /// with the services that already have one.
    fn services(&mut self) -> Result<PhaseResult> {
        if self.options.no_db {
            return Ok(PhaseResult::SkippedOnRequest(receipt::SKIP_NO_DB.into()));
        }
        let configured: Vec<(String, Engine)> = self
            .project
            .config
            .services
            .iter()
            .map(|(key, service)| (key.clone(), service.engine()))
            .collect();
        let mut details = Vec::new();
        for (key, engine) in &configured {
            let detail = match engine {
                Engine::Postgres => self.postgres(key)?,
                Engine::Valkey | Engine::Redis | Engine::Mysql => self.container(key, *engine)?,
            };
            details.push(format!("{key}: {detail}"));
        }
        if configured.is_empty() {
            // A URL-only attachment needs no service in the configuration.
            if self.options.database_url.is_none() {
                return Ok(PhaseResult::Skipped("no services configured".into()));
            }
            let detail = self.postgres(DEFAULT_POSTGRES_KEY)?;
            details.push(format!("{DEFAULT_POSTGRES_KEY}: {detail}"));
        }
        Ok(PhaseResult::Done(Some(details.join("; "))))
    }

    /// Record `data` as service `key`'s binding, with the scratch databases
    /// `extras` it lists, stamped with the generation of the configuration it
    /// is applied under, and save the receipt.
    fn bind(
        &mut self,
        key: &str,
        data: DataBinding,
        extras: BTreeMap<String, ExtraBinding>,
    ) -> Result<()> {
        let applied = self.project.config.service_generation(key);
        let mut binding = ServiceBinding::postgres(data, applied);
        binding.extras = extras;
        self.receipt.services.insert(key.to_string(), binding);
        // The credentials are recorded: nothing is left to recover.
        self.receipt.creating = None;
        self.receipt.save(&self.workspace.git_dir)
    }

    /// A URL the user attached names no branch devflow could add databases
    /// to: say that the configured scratch databases do not apply.
    fn warn_attachment_has_no_scratch(&mut self) {
        let configured = self
            .project
            .config
            .postgres()
            .is_some_and(|(_, pg)| !pg.extra_databases.is_empty());
        if configured {
            self.warnings.push(
                "extra_databases is set, but this workspace is attached to a database URL: \
                 devflow adds scratch databases only to branches it creates, so their URLs \
                 (TEST_DATABASE_URL and the like) are not exported"
                    .into(),
            );
        }
    }

    /// The scratch databases (`extra_databases`) the binding of branch `id`
    /// lists: those the configuration names that the server has ready.
    /// `known` is what the server already said about the branch; when it lacks
    /// a configured name and this workspace owns the branch, the server is
    /// asked to provision it (the branch predates the name). A branch this
    /// workspace does not own is never changed: the name is reported instead.
    fn scratch_bindings(
        &mut self,
        id: uuid::Uuid,
        owned: bool,
        known: &[ScratchInfo],
    ) -> Result<BTreeMap<String, ExtraBinding>> {
        let wanted = self
            .project
            .config
            .postgres()
            .map(|(_, pg)| pg.extra_databases.clone())
            .unwrap_or_default();
        if wanted.is_empty() {
            return Ok(BTreeMap::new());
        }
        let usable = |infos: &[ScratchInfo], name: &String| {
            infos
                .iter()
                .any(|e| e.name == *name && !e.orphaned && e.status == RecordStatus::Ready)
        };
        let mut infos = known.to_vec();
        if owned && wanted.iter().any(|name| !usable(&infos, name)) {
            infos = connect_required(self.project)?.ensure_scratch(id)?;
        }
        let mut extras = BTreeMap::new();
        for name in &wanted {
            match infos
                .iter()
                .find(|e| e.name == *name && !e.orphaned && e.status == RecordStatus::Ready)
            {
                Some(found) => {
                    extras.insert(
                        name.clone(),
                        ExtraBinding {
                            database: found.db_name.clone(),
                        },
                    );
                }
                None => self.warnings.push(format!(
                    "scratch database '{name}' is not available for branch {id}: this workspace \
                     did not create the branch, so devflow does not add to it"
                )),
            }
        }
        Ok(extras)
    }

    /// Point service `key`'s binding at the endpoint the server hands out
    /// now ([`Server::app_endpoint`]), keeping its credentials. Returns the
    /// new `host:port` when the URL changed.
    fn reconcile_endpoint(&mut self, key: &str, server: &Server) -> Result<Option<String>> {
        let (host, port) = server
            .app_endpoint()
            .context("cannot read the server's endpoint to check the workspace URL")?;
        Ok(self
            .receipt
            .services
            .get_mut(key)
            .and_then(|binding| rebind_endpoint(binding, &host, port)))
    }

    /// An existing binding is still good: record the generation it holds under.
    fn stamp(&mut self, key: &str) -> Result<()> {
        let applied = self.project.config.service_generation(key);
        if let Some(binding) = self.receipt.services.get_mut(key) {
            binding.applied = applied;
            binding.orphaned = false;
        }
        self.receipt.save(&self.workspace.git_dir)
    }

    /// The PostgreSQL service `key`: keep, attach or provision its binding.
    fn postgres(&mut self, key: &str) -> Result<String> {
        // A binding recorded before a crash: keep it if the branch is still there.
        if let Some(binding) = self.receipt.services.get(key).cloned() {
            match binding.branch_id {
                None => {
                    self.warn_attachment_has_no_scratch();
                    self.stamp(key)?;
                    return Ok("attached to the given database URL".into());
                }
                Some(id) => {
                    let server = connect_required(self.project)?;
                    match server.branch(id)? {
                        Some(info) if info.status == RecordStatus::Ready => {
                            // The endpoint settings (`port`, `app_host`,
                            // `app_port`) may have changed with the section:
                            // the URL follows them before the generation is
                            // recorded as applied and env and hooks run.
                            let moved = self.reconcile_endpoint(key, &server)?;
                            // The scratch databases follow the configuration
                            // the same way: a name added since is provisioned,
                            // one removed is no longer listed (its database
                            // stays, as a recorded orphan).
                            let extras = self.scratch_bindings(id, binding.owned, &info.extras)?;
                            let added = extras
                                .keys()
                                .filter(|name| !binding.extras.contains_key(*name))
                                .count();
                            if let Some(bound) = self.receipt.services.get_mut(key) {
                                bound.extras = extras;
                            }
                            self.stamp(key)?;
                            let mut detail = format!("reusing branch {}", info.db_name);
                            if let Some(endpoint) = moved {
                                detail.push_str(&format!("; its URL now points at {endpoint}"));
                            }
                            if added > 0 {
                                detail.push_str(&format!("; {added} scratch database(s) added"));
                            }
                            return Ok(detail);
                        }
                        _ => {
                            self.warnings.push(format!(
                                "database branch {id} is gone; provisioning a new one"
                            ));
                            self.receipt.services.remove(key);
                        }
                    }
                }
            }
        }
        if let Some(url) = self.options.database_url.clone() {
            let params = attach_params(&url)?;
            self.bind(
                key,
                DataBinding {
                    branch_id: None,
                    database: params.get("dbname").unwrap_or_default().to_string(),
                    role: params.get("user").unwrap_or_default().to_string(),
                    url: url.trim().to_string(),
                    source: "url".into(),
                    owned: false,
                },
                BTreeMap::new(),
            )?;
            self.warn_attachment_has_no_scratch();
            return Ok("attached to the given database URL".into());
        }
        let server = connect_required(self.project)?;
        let name = branch_name(self.project, &self.workspace);
        let instance = self.receipt.instance_id;

        // A previous attempt for this service recorded the identity of the
        // branch it was about to create and may have created it before
        // crashing; its password was never saved, so take over that branch
        // with fresh credentials instead of creating a second one. Only the
        // recorded identity qualifies: never a branch found by name or
        // instance, and never one recorded under another service key.
        // (A branch the earlier attempt never got as far as creating is no
        // branch: provisioning goes on.)
        if let Some(id) = recoverable(&self.receipt, key)
            && server.branch(id)?.is_some()
        {
            match server.rotate_credentials(id) {
                Ok(creds) => {
                    let source = self.receipt.data_source.clone().unwrap_or_default();
                    let extras = self.scratch_bindings(id, true, &creds.branch.extras)?;
                    self.bind(key, binding_from(&creds, source), extras)?;
                    return Ok(format!(
                        "recovered branch {} after an interrupted run",
                        creds.database
                    ));
                }
                Err(e) => self.warnings.push(format!(
                    "an earlier branch for this service could not be reused ({e:#}); \
                     creating a new one"
                )),
            }
        }

        let requested = self.pick_source()?;
        let (resolved, label) = match self.receipt.data_source_id.clone() {
            // Chosen by an earlier attempt: the same identity, whatever the
            // workspace or snapshot name means now.
            Some(pinned) => {
                let resolved = decode_resolved(&pinned)?;
                let label = self.receipt.data_source.clone().unwrap_or(pinned);
                ensure_available(&server, resolved).with_context(|| {
                    format!(
                        "the recorded data source ({label}) is unavailable; set the workspace up \
                         as a new instance with another data source"
                    )
                })?;
                (resolved, label)
            }
            None => {
                let (resolved, label) = self.resolve_for_branch(&server, &requested)?;
                // Pin the resolved source before the effect so a retry clones
                // exactly the same data.
                self.receipt.data_source = Some(label.clone());
                self.receipt.data_source_id = Some(encode_resolved(resolved));
                self.receipt.save(&self.workspace.git_dir)?;
                (resolved, label)
            }
        };
        // Record what is about to be created, under the identity it will
        // have, before the effect.
        let id = uuid::Uuid::new_v4();
        self.receipt.creating = Some(PendingBranch {
            service: key.to_string(),
            branch_id: id,
        });
        self.receipt.save(&self.workspace.git_dir)?;
        let creds = server.create_branch_with_id(id, &name, resolved, Some(instance))?;
        let detail = format!("branch {} from {label}", creds.database);
        let extras = self.scratch_bindings(id, true, &creds.branch.extras)?;
        self.bind(key, binding_from(&creds, label), extras)?;
        Ok(detail)
    }

    /// A container service (`valkey`, `redis`, `mysql`): keep, take over or provision
    /// its branch, and start its container. The branch is the workspace's
    /// own: a clone of the data source (copy-on-write when the Docker
    /// volumes allow), with its own port and password. A data source that
    /// does not exist for this service fails it; an empty one is never
    /// substituted.
    fn container(&mut self, key: &str, engine: Engine) -> Result<String> {
        let svc = Service::open(self.project, key)?;
        let instance = self.receipt.instance_id;

        // A binding recorded before: its branch is used if it is still there.
        if let Some(binding) = self.receipt.services.get(key).cloned() {
            ensure!(
                binding.engine == engine,
                "service '{key}' is bound as {} and configured as {engine}; their data is not \
                 interchangeable. Use another key for the new engine",
                binding.engine
            );
            let ready = match binding.branch_id {
                Some(id) => svc
                    .branch(id)?
                    .filter(|r| r.state == RecordState::Ready)
                    .map(|r| (id, r)),
                None => None,
            };
            if let Some((id, record)) = ready {
                // Containers start on setup and on every switch.
                let endpoint = svc.up(id)?;
                if let Some(bound) = self.receipt.services.get_mut(key) {
                    bound.url = endpoint.url.clone();
                    bound.endpoint = endpoint.container_endpoint.clone();
                }
                self.stamp(key)?;
                return Ok(format!(
                    "reusing branch {} on 127.0.0.1:{}",
                    short_id(&record.id),
                    endpoint.port
                ));
            }
            self.warnings.push(format!(
                "the branch of service '{key}' is gone; provisioning a new one"
            ));
            self.receipt.services.remove(key);
        }

        // A creation an earlier run began: by its recorded identity only.
        let mut id = uuid::Uuid::new_v4();
        if let Some(pending) = recoverable(&self.receipt, key) {
            match svc.adopt(pending, instance)? {
                Adopted::Ready(record) => {
                    let label = self
                        .receipt
                        .service_sources
                        .get(key)
                        .cloned()
                        .unwrap_or_default();
                    svc.up(record.id)?;
                    self.bind_container(key, &svc, &record.id, label)?;
                    return Ok(format!(
                        "recovered branch {} after an interrupted run",
                        short_id(&record.id)
                    ));
                }
                // Never recorded: the same identity is used again.
                Adopted::Gone => id = pending,
                // Rolled back: a new identity.
                Adopted::Unusable => {}
            }
        }

        // The label is the recorded one (a retry's `--data-from` is ignored
        // with a warning), and so is the identity it resolved to: that exact
        // branch or snapshot is cloned, or the service fails. A workspace or
        // snapshot name is never looked up again, because it may now mean
        // another branch.
        let requested = self.container_source(key)?;
        let (source, label) = match self.pinned_container_source(&svc, key)? {
            Some(pinned) => pinned,
            None => {
                let (source, label) = self.resolve_container_source(&svc, &requested)?;
                // Pin the resolved source before the effect: a retry makes
                // the same choice whatever `--data-from` says then.
                self.receipt
                    .service_source_ids
                    .insert(key.to_string(), services::workspace::encode_ref(source));
                (source, label)
            }
        };
        self.receipt
            .service_sources
            .insert(key.to_string(), label.clone());
        self.receipt.creating = Some(PendingBranch {
            service: key.to_string(),
            branch_id: id,
        });
        self.receipt.save(&self.workspace.git_dir)?;
        let record = svc.create_branch(NewBranch {
            id: Some(id),
            name: self.workspace.name.clone(),
            owner: Some(instance),
            source,
        })?;
        self.bind_container(key, &svc, &record.id, label.clone())?;
        let mut how = match record.clone {
            Some(method) => format!(" ({method})"),
            None if source == SourceRef::Empty => " (empty)".to_string(),
            None => String::new(),
        };
        // A source that had to stop for the copy (MySQL) says for how long.
        if let Some(ms) = record.downtime_ms {
            how.push_str(&format!(", source stopped {:.1} s", ms as f64 / 1000.0));
        }
        Ok(format!(
            "branch {} from {label}{how} on 127.0.0.1:{}",
            short_id(&record.id),
            record.port.unwrap_or_default()
        ))
    }

    /// Record the ready branch `id` as service `key`'s binding.
    fn bind_container(
        &mut self,
        key: &str,
        svc: &Service,
        id: &uuid::Uuid,
        label: String,
    ) -> Result<()> {
        let record = svc.require_branch(*id)?;
        let applied = self.project.config.service_generation(key);
        let binding = services::workspace::binding_of(svc, &record, label, applied)?;
        self.receipt.services.insert(key.to_string(), binding);
        // The credentials are recorded: nothing is left to recover.
        self.receipt.creating = None;
        self.receipt.save(&self.workspace.git_dir)
    }

    /// The source service `key` pinned in an earlier run, by catalog
    /// identity, with its label; `None` when nothing was pinned. A pinned
    /// source that is gone is an error: the label is not resolved again.
    fn pinned_container_source(
        &self,
        svc: &Service,
        key: &str,
    ) -> Result<Option<(SourceRef, String)>> {
        let Some(text) = self.receipt.service_source_ids.get(key) else {
            return Ok(None);
        };
        let label = self
            .receipt
            .service_sources
            .get(key)
            .cloned()
            .unwrap_or_else(|| text.clone());
        let source = services::workspace::decode_ref(text)?;
        services::workspace::require_source(svc, source).with_context(|| {
            format!(
                "the data source of service '{key}' was chosen as {label} ({text}) and is gone; \
                 devflow clones that exact source or none, and does not look the name up again \
                 (set the workspace up as a new instance, with a data source of your choice)"
            )
        })?;
        Ok(Some((source, label)))
    }

    /// Where service `key`'s data comes from: what it chose earlier (a source
    /// is immutable once chosen), else `--data-from`, else the code source's
    /// branch (an empty one for the default workspace).
    fn container_source(&mut self, key: &str) -> Result<DataSource> {
        if let Some(text) = self.receipt.service_sources.get(key).cloned() {
            let recorded = decode_source(&text)?;
            if let Some(requested) = &self.options.data_from
                && *requested != recorded
            {
                self.warnings.push(format!(
                    "the data source of service '{key}' was already chosen as {text}; ignoring \
                     --data-from {requested}"
                ));
            }
            return Ok(recorded);
        }
        if let Some(source) = &self.options.data_from {
            return Ok(source.clone());
        }
        if self.workspace.is_default {
            return Ok(DataSource::Empty);
        }
        let source = source_workspace(self.project, &self.workspace, &self.receipt)?
            .context("cannot find the workspace this one was created from")?;
        Ok(DataSource::Workspace { name: source.name })
    }

    /// Like [`services::workspace::resolve_source`], but gives the default
    /// workspace its (empty) root branches first when something wants to
    /// clone them and it has none yet.
    fn resolve_container_source(
        &mut self,
        svc: &Service,
        source: &DataSource,
    ) -> Result<(SourceRef, String)> {
        if let DataSource::Workspace { name } = source {
            let source_ws = worktree::find(self.project, name)?
                .with_context(|| format!("no workspace named '{name}' to take data from"))?;
            let bound = load_receipt(self.project, &source_ws)?
                .is_some_and(|r| services::workspace::binding(&r, &svc.key, svc.engine).is_some());
            if !bound && source_ws.is_default {
                ensure!(
                    source_ws.path != self.workspace.path,
                    "the default workspace cannot take its data from itself; use --data-from empty"
                );
                init_default(self.project, &source_ws)?;
                self.warnings.push(format!(
                    "created the root branch of service '{}' for the default workspace '{}' to \
                     clone from",
                    svc.key, source_ws.name
                ));
            }
        }
        services::workspace::resolve_source(self.project, svc, source)
    }

    /// The recorded source wins (data origin is immutable once chosen), then
    /// `--data-from`, then the code source's branch.
    fn pick_source(&mut self) -> Result<DataSource> {
        if let Some(text) = &self.receipt.data_source {
            let recorded = decode_source(text)?;
            if let Some(requested) = &self.options.data_from
                && *requested != recorded
            {
                self.warnings.push(format!(
                    "data source was already chosen as {text}; ignoring --data-from {requested}"
                ));
            }
            return Ok(recorded);
        }
        if let Some(source) = &self.options.data_from {
            return Ok(source.clone());
        }
        if self.workspace.is_default {
            return Ok(DataSource::Empty);
        }
        let source = source_workspace(self.project, &self.workspace, &self.receipt)?
            .context("cannot find the workspace this one was created from")?;
        Ok(DataSource::Workspace { name: source.name })
    }

    /// Like [`resolve`], but gives the default workspace its (empty) root
    /// branch first when something wants to clone it and it has none yet.
    fn resolve_for_branch(
        &mut self,
        server: &Server,
        source: &DataSource,
    ) -> Result<(ResolvedSource, String)> {
        if let DataSource::Workspace { name } = source {
            let source_ws = worktree::find(self.project, name)?
                .with_context(|| format!("no workspace named '{name}' to take data from"))?;
            let bound = load_receipt(self.project, &source_ws)?.is_some_and(|r| {
                r.postgres(&self.project.config)
                    .is_some_and(|(_, binding)| binding.branch_id.is_some())
            });
            if !bound && source_ws.is_default {
                ensure!(
                    source_ws.path != self.workspace.path,
                    "the default workspace cannot take its data from itself; use --data-from empty"
                );
                let created = init_default(self.project, &source_ws)?;
                self.warnings.push(format!(
                    "created database branch {created} for the default workspace '{}' to clone from",
                    source_ws.name
                ));
            }
        }
        resolve(self.project, server, source)
    }

    fn env(&mut self) -> Result<PhaseResult> {
        // The worktree's own pitchfork namespace, when the project uses
        // pitchfork: identity only, so it needs no approval.
        let namespace = processes::ensure_namespace(self.project, &self.workspace, &self.receipt);
        self.warnings.extend(namespace.warning());
        // Recipes and presets put values into the environment of everything
        // that loads the file; they run only for an approved scope. Without
        // approval the file holds what devflow generates, and the phase stays
        // open.
        let verdict = recipes_verdict(
            self.project,
            &self.workspace.path,
            self.options.approval.as_ref(),
        );
        let trusted = verdict.approved();
        let env = workspace_env(self.project, &self.workspace, &self.receipt, trusted);
        let vars = env.vars;
        self.warnings.extend(env.warnings);
        env_file::write(&self.workspace.path, &vars)?;
        if !trusted {
            self.differs |= verdict == Verdict::Differs;
            return Ok(PhaseResult::SkippedRequired(
                if verdict == Verdict::Differs {
                    DIFFERS
                } else {
                    UNTRUSTED_ENV
                }
                .into(),
            ));
        }
        self.receipt.stamp_env_generation(&self.project.config);
        // The file no longer exports the services the configuration dropped:
        // their removal is applied, though their bindings stay.
        self.receipt.acknowledge_removals(&self.project.config);
        Ok(PhaseResult::Done(Some(crate::text::counted(
            vars.len() as u64,
            "variable",
        ))))
    }

    fn hooks(&mut self) -> Result<PhaseResult> {
        if self.options.no_hooks {
            return Ok(PhaseResult::SkippedOnRequest(receipt::SKIP_NO_HOOKS.into()));
        }
        let hooks = &self.project.config.hooks;
        if hooks.setup.is_empty() {
            return Ok(PhaseResult::Done(Some("no setup hooks".into())));
        }
        let verdict = hooks_verdict(
            self.project,
            &self.workspace.path,
            self.options.approval.as_ref(),
        )?;
        if !verdict.approved() {
            self.differs |= verdict == Verdict::Differs;
            return Ok(PhaseResult::SkippedRequired(
                if verdict == Verdict::Differs {
                    DIFFERS
                } else {
                    UNTRUSTED
                }
                .into(),
            ));
        }
        // Before the first hook: does a hook that goes through mise get this
        // workspace's variables? Asked with the environment the hooks get.
        let vars = workspace_env(self.project, &self.workspace, &self.receipt, true).vars;
        let verdict = mise_gate::check(
            &self.workspace.path,
            &self.project.config.hooks.commands(HookPhase::Setup)?,
            &vars,
            self.options.skip_mise_check,
            &env_file::MiseCommand::default(),
        );
        self.warnings.extend(verdict.warnings);
        self.receipt.mise_check_skipped_at = verdict.skipped.then(chrono::Utc::now);
        if let Some(reason) = verdict.block {
            return Ok(PhaseResult::SkippedRequired(reason));
        }
        let report = run_hooks(
            self.project,
            &self.workspace,
            &self.receipt,
            HookPhase::Setup,
            self.progress,
        )?;
        self.receipt.hooks_digest = Some(trust::digest_at(self.project, &self.workspace.path));
        let result = match report.failure() {
            Some(failure) => PhaseResult::Failed(failure),
            None => PhaseResult::Done(Some(crate::text::counted(
                report.runs.len() as u64,
                "command",
            ))),
        };
        self.hooks = Some(report);
        Ok(result)
    }
}

/// Give the default workspace an empty root branch (and env file) so other
/// workspaces can clone it. Returns the branch's database name.
fn init_default(project: &Project, default: &Workspace) -> Result<String> {
    let lock = acquire(&default.git_dir)?.ok_or_else(|| busy(&default.name))?;
    let mut receipt = match load_receipt_locked(project, default, &lock)? {
        Some(receipt) => receipt,
        None => {
            let mut receipt = Receipt::new(project.id(), &default.name, None);
            receipt.set_phase(
                Phase::Materialize,
                PhaseState::Done,
                Some("default workspace".into()),
            );
            receipt
        }
    };
    // Nothing to copy, and setup hooks are for new worktrees.
    if !complete(&receipt, Phase::Payload) {
        receipt.set_phase(
            Phase::Payload,
            PhaseState::Skipped,
            Some("default workspace".into()),
        );
    }
    // Hooks that never ran; a failure, an untrusted skip or a phase an earlier
    // explicit setup left reopened keeps its state.
    if matches!(
        receipt.phase(Phase::Hooks).state,
        PhaseState::Pending | PhaseState::Started
    ) {
        receipt.skip_deliberate(Phase::Hooks, receipt::SKIP_DEFAULT_HOOKS.into());
    }
    for phase in [Phase::Services, Phase::Env] {
        if receipt.phase(phase).state != PhaseState::Done {
            receipt.set_phase(phase, PhaseState::Pending, None);
        }
    }
    receipt.save(&default.git_dir)?;
    let options = SetupOptions {
        data_from: Some(DataSource::Empty),
        no_hooks: true,
        ..SetupOptions::default()
    };
    let mut progress = NoProgress;
    let mut runner = Runner::new(project, default.clone(), receipt, &options, &mut progress);
    runner.run(&[Phase::Services, Phase::Env])?;
    let services: Vec<String> = runner.receipt.services.keys().cloned().collect();
    runner
        .receipt
        .postgres(&project.config)
        .map(|(_, binding)| binding.database.clone())
        .or_else(|| (!services.is_empty()).then(|| services.join(", ")))
        .context("the default workspace did not get a database")
}

/// The first eight hex digits of an id, for messages.
fn short_id(id: &uuid::Uuid) -> String {
    id.simple().to_string()[..8].to_string()
}

/// A managed server on an ephemeral port gets a new port after a restart:
/// point the receipt and `.devflow/env` at the current one. Best effort, and
/// never for URL-only attachments, which belong to whoever supplied them.
fn refresh_endpoint(
    project: &Project,
    workspace: &Workspace,
    receipt: &mut Receipt,
    recipes: bool,
) -> Result<Option<String>> {
    use crate::config::PostgresMode;
    let ephemeral = project.config.postgres().is_some_and(|(_, pg)| {
        pg.mode == PostgresMode::Managed && pg.port == 0 && pg.app_port.is_none()
    });
    let Some((key, binding)) = receipt
        .postgres(&project.config)
        .filter(|(_, b)| ephemeral && b.branch_id.is_some())
    else {
        return Ok(None);
    };
    let key = key.to_string();
    let Some(server) = Server::for_project(project)? else {
        return Ok(None);
    };
    // A stopped server has no port to report; that is not an error here.
    let Ok((host, port)) = server.app_endpoint() else {
        return Ok(None);
    };
    let Some(url) = with_endpoint(&binding.url, &host, port).filter(|u| *u != binding.url) else {
        return Ok(None);
    };
    let Some(_lock) = Receipt::try_lock(&workspace.git_dir)? else {
        return Ok(None);
    };
    if !rebind_url_if_unchanged(&workspace.git_dir, &project.config, receipt, &key, url)? {
        return Ok(None);
    }
    env_file::write(
        &workspace.path,
        &workspace_env(project, workspace, receipt, recipes).vars,
    )?;
    Ok(Some(format!(
        "the database endpoint changed to {host}:{port}; updated the receipt and .devflow/env"
    )))
}

/// With the receipt lock held: replace the URL of service `key`'s binding,
/// unless another process (a fork, a re-attach) rewrote the receipt since
/// `receipt` was read. Either way `receipt` ends up as what is on disk, so
/// later steps (switch hooks) use the current identity and binding.
fn rebind_url_if_unchanged(
    git_dir: &Path,
    config: &crate::Config,
    receipt: &mut Receipt,
    key: &str,
    url: String,
) -> Result<bool> {
    let seen = (receipt.instance_id, receipt.services.get(key).cloned());
    if let Some(current) = Receipt::load_for(git_dir, config)? {
        *receipt = current;
    }
    if (receipt.instance_id, receipt.services.get(key).cloned()) != seen {
        return Ok(false);
    }
    let Some(binding) = receipt.services.get_mut(key) else {
        return Ok(false);
    };
    binding.url = url;
    receipt.updated_at = chrono::Utc::now();
    receipt.save(git_dir)?;
    Ok(true)
}

/// Move a server-backed binding to `host:port`, credentials and query kept.
/// URL-only attachments belong to whoever supplied them and are never touched.
/// Returns the new `host:port` when the URL changed.
fn rebind_endpoint(binding: &mut ServiceBinding, host: &str, port: u16) -> Option<String> {
    if binding.is_url_only() {
        return None;
    }
    let url = with_endpoint(&binding.url, host, port).filter(|u| *u != binding.url)?;
    binding.url = url;
    Some(format!("{host}:{port}"))
}

/// `url` with its host and port replaced.
fn with_endpoint(url: &str, host: &str, port: u16) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let (authority, tail) = rest.split_once('/').unwrap_or((rest, ""));
    let userinfo = authority
        .rsplit_once('@')
        .map(|(u, _)| format!("{u}@"))
        .unwrap_or_default();
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    Some(format!("{scheme}://{userinfo}{host}:{port}/{tail}"))
}

/// The branch an interrupted creation for service `key` left behind: the one
/// the receipt recorded before asking for it. `None` when the creation in
/// flight was begun for another service, or when its branch is already
/// recorded as some service's binding (a stale record, never a second owner).
fn recoverable(receipt: &Receipt, key: &str) -> Option<uuid::Uuid> {
    let pending = receipt.creating.as_ref().filter(|p| p.service == key)?;
    let taken = receipt
        .services
        .values()
        .map(|b| b.branch_id)
        .chain(receipt.unmapped.as_ref().map(|d| d.branch_id))
        .any(|id| id == Some(pending.branch_id));
    (!taken).then_some(pending.branch_id)
}

/// Name of the branch created for `workspace`.
fn branch_name(project: &Project, workspace: &Workspace) -> String {
    if workspace.is_default {
        project
            .config
            .postgres()
            .and_then(|(_, pg)| pg.database.clone())
            .unwrap_or_else(|| project.name().to_string())
    } else {
        workspace.name.clone()
    }
}

fn binding_from(creds: &crate::db::BranchCredentials, source: String) -> DataBinding {
    DataBinding {
        branch_id: Some(creds.branch.id),
        database: creds.database.clone(),
        role: creds.user.clone(),
        url: creds.url.clone(),
        source,
        owned: true,
    }
}

/// The workspace whose code and payloads this one came from; `None` for the
/// default workspace itself.
fn source_workspace(
    project: &Project,
    workspace: &Workspace,
    receipt: &Receipt,
) -> Result<Option<Workspace>> {
    if workspace.is_default {
        return Ok(None);
    }
    let all = worktree::list(project)?;
    if let Some(name) = &receipt.code_source {
        if let Some(found) = all
            .iter()
            .find(|w| (w.name == *name || w.branch.as_deref() == Some(name)) && !w.missing)
        {
            return Ok((found.path != workspace.path).then(|| found.clone()));
        }
    } else if let Some(default) = all.first() {
        return Ok(Some(default.clone()));
    }
    // The recorded source is gone: the default workspace is the safe fallback.
    Ok(all.into_iter().next().filter(|w| w.path != workspace.path))
}

/// Resolve a request to server ids plus the label recorded in the receipt
/// (`empty`, `workspace:NAME`, `branch:ID`, `snapshot:NAME@vN`).
fn resolve(
    project: &Project,
    server: &Server,
    source: &DataSource,
) -> Result<(ResolvedSource, String)> {
    Ok(match source {
        DataSource::Empty => (ResolvedSource::Empty, encode_source(source)),
        DataSource::Workspace { name } => {
            let workspace = worktree::find(project, name)?
                .with_context(|| format!("no workspace named '{name}' to take data from"))?;
            let id = load_receipt(project, &workspace)?
                .and_then(|r| r.postgres(&project.config).and_then(|(_, b)| b.branch_id))
                .with_context(|| {
                    format!(
                        "workspace '{name}' has no database branch to clone; \
                         use --data-from empty or --data-from snapshot:NAME"
                    )
                })?;
            ensure_ready(server, id)?;
            (ResolvedSource::Branch(id), encode_source(source))
        }
        DataSource::Branch { .. } | DataSource::BranchPrefix { .. } => {
            let id = source
                .branch_id(server.list_branches()?.into_iter().map(|b| b.id))?
                .context("a branch source names a branch")?;
            ensure_ready(server, id)?;
            // The label records the whole id, never the prefix typed.
            (ResolvedSource::Branch(id), format!("branch:{id}"))
        }
        DataSource::Snapshot { name, version } => {
            let snapshot = server.find_snapshot(name, *version)?;
            (
                ResolvedSource::Snapshot(snapshot.id),
                format!("snapshot:{}@v{}", snapshot.name, snapshot.version),
            )
        }
    })
}

fn ensure_ready(server: &Server, id: uuid::Uuid) -> Result<()> {
    let branch = server
        .branch(id)?
        .with_context(|| format!("database branch {id} does not exist on the server"))?;
    ensure!(
        branch.status == RecordStatus::Ready,
        "database branch {id} is not ready ({:?})",
        branch.status
    );
    Ok(())
}

/// A pinned source must still exist and be usable.
fn ensure_available(server: &Server, source: ResolvedSource) -> Result<()> {
    match source {
        ResolvedSource::Empty => Ok(()),
        ResolvedSource::Branch(id) => ensure_ready(server, id),
        ResolvedSource::Snapshot(id) => {
            let found = server
                .list_snapshots()?
                .into_iter()
                .find(|s| s.id == id)
                .with_context(|| format!("snapshot {id} does not exist on the server"))?;
            ensure!(
                found.status == RecordStatus::Ready,
                "snapshot {}@v{} is not ready ({:?})",
                found.name,
                found.version,
                found.status
            );
            Ok(())
        }
    }
}

/// The identity form of a resolved source, as stored in
/// [`Receipt::data_source_id`]: `empty`, `branch:<id>` or `snapshot:<id>`.
fn encode_resolved(source: ResolvedSource) -> String {
    match source {
        ResolvedSource::Empty => "empty".to_string(),
        ResolvedSource::Branch(id) => format!("branch:{id}"),
        ResolvedSource::Snapshot(id) => format!("snapshot:{id}"),
    }
}

fn decode_resolved(text: &str) -> Result<ResolvedSource> {
    if text == "empty" {
        return Ok(ResolvedSource::Empty);
    }
    if let Some(id) = text.strip_prefix("branch:") {
        return Ok(ResolvedSource::Branch(id.parse()?));
    }
    if let Some(id) = text.strip_prefix("snapshot:") {
        return Ok(ResolvedSource::Snapshot(id.parse()?));
    }
    bail!("invalid recorded data source identity '{text}'")
}

/// `workspace:NAME` is explicit so a workspace named `empty` stays distinct.
fn encode_source(source: &DataSource) -> String {
    match source {
        DataSource::Workspace { name } => format!("workspace:{name}"),
        other => other.to_string(),
    }
}

fn decode_source(text: &str) -> Result<DataSource> {
    match text.strip_prefix("workspace:") {
        Some(name) => Ok(DataSource::Workspace {
            name: name.to_string(),
        }),
        None => text.parse(),
    }
}

/// A forked VM copied the receipt: new identity, and the copied binding loses
/// its authority. The new branch clones `--data-from`, else the old branch,
/// else the recorded source. `instance` is `DEVFLOW_INSTANCE_ID`: it names the
/// new identity unless it is the receipt's own.
fn fork_receipt(
    receipt: &mut Receipt,
    config: &crate::Config,
    options: &SetupOptions,
    instance: Option<uuid::Uuid>,
) {
    // The branch the copy knew: the configured service's, else any other
    // PostgreSQL binding's. It is only ever a clone source, never reused.
    let old_branch = receipt
        .postgres(config)
        .and_then(|(_, binding)| binding.branch_id)
        .or_else(|| {
            receipt
                .services
                .values()
                .filter(|binding| binding.engine == Engine::Postgres)
                .find_map(|binding| binding.branch_id)
        });
    let old_source = receipt.data_source.take();
    let old_source_id = receipt.data_source_id.take();
    // The container branches the copy knew are clone sources for the new
    // ones (unless `--data-from` says otherwise), never reused.
    let old_containers: Vec<(String, uuid::Uuid)> = receipt
        .services
        .iter()
        .filter(|(_, binding)| binding.engine.is_container())
        .filter_map(|(key, binding)| Some((key.clone(), binding.branch_id?)))
        .collect();
    receipt.service_sources.clear();
    receipt.service_source_ids.clear();
    if options.data_from.is_none() {
        for (key, id) in old_containers {
            receipt
                .service_sources
                .insert(key.clone(), format!("branch:{id}"));
            receipt
                .service_source_ids
                .insert(key, format!("branch:{id}"));
        }
    }
    receipt.instance_id = instance
        .filter(|id| *id != receipt.instance_id)
        .unwrap_or_else(uuid::Uuid::new_v4);
    receipt.services.clear();
    receipt.creating = None;
    receipt.unmapped = None;
    receipt.env_generation = None;
    receipt.hooks_digest = None;
    receipt.mise_check_skipped_at = None;
    // The copy's daemons ran under the namespace of the instance it came from.
    receipt.processes = None;
    (receipt.data_source, receipt.data_source_id) = match (&options.data_from, old_branch) {
        (Some(source), _) => (Some(encode_source(source)), None),
        (None, Some(id)) => (Some(format!("branch:{id}")), Some(format!("branch:{id}"))),
        (None, None) => (old_source, old_source_id),
    };
    for phase in [Phase::Services, Phase::Env, Phase::Hooks] {
        receipt.set_phase(phase, PhaseState::Pending, None);
    }
}

/// The receipt of `workspace`, refusing one that belongs to another project.
/// A version 1 file is migrated in memory; see [`load_receipt_locked`].
fn load_receipt(project: &Project, workspace: &Workspace) -> Result<Option<Receipt>> {
    let Some(receipt) = Receipt::load_for(&workspace.git_dir, &project.config)? else {
        return Ok(None);
    };
    let mut receipt = check_project(project, workspace, receipt)?;
    reopen_invalidated(project, &mut receipt);
    Ok(Some(receipt))
}

/// Reopen the phases that what changed since the workspace was set up
/// invalidates: a service added or changed in the configuration
/// ([`Receipt::reopen`]), and a bound branch the catalog no longer has (a lost
/// branch that was dropped), which would otherwise leave a ready workspace
/// with a connection that leads nowhere. Returns whether anything changed.
fn reopen_invalidated(project: &Project, receipt: &mut Receipt) -> bool {
    let mut changed = receipt.reopen(&project.config);
    let lost = services::workspace::lost_bindings(project, receipt);
    changed |= receipt.reopen_lost_branches(&lost);
    changed
}

/// [`load_receipt`] for a caller that holds the workspace's receipt lock: a
/// version 1 file is migrated on disk and the phases a configuration change
/// reopens are recorded, which has no effect beyond the file.
fn load_receipt_locked(
    project: &Project,
    workspace: &Workspace,
    lock: &ReceiptLock,
) -> Result<Option<Receipt>> {
    // Refuse another project's receipt before rewriting anything.
    if let Some(stored) = Receipt::load(&workspace.git_dir)? {
        check_project(project, workspace, stored)?;
    }
    let Some(mut receipt) = Receipt::migrate_in_place(&workspace.git_dir, &project.config, lock)?
    else {
        return Ok(None);
    };
    if reopen_invalidated(project, &mut receipt) {
        receipt.save(&workspace.git_dir)?;
    }
    Ok(Some(receipt))
}

fn check_project(project: &Project, workspace: &Workspace, receipt: Receipt) -> Result<Receipt> {
    ensure!(
        receipt.project_id == project.id(),
        "the devflow receipt of '{}' belongs to another project ({}, this one is {}); \
         remove {} to start over",
        workspace.name,
        receipt.project_id,
        project.id(),
        Receipt::path(&workspace.git_dir).display()
    );
    Ok(receipt)
}

fn lock(workspace: &Workspace) -> Result<crate::receipt::ReceiptLock> {
    acquire(&workspace.git_dir)?.ok_or_else(|| busy(&workspace.name))
}

/// The workspace has no receipt: devflow has not set it up.
fn needs_setup(workspace: &Workspace) -> anyhow::Error {
    Problem::new("this workspace has not been set up by devflow yet")
        .with(Remedy::Setup {
            workspace: workspace.path.clone(),
        })
        .into_error()
}

/// Another setup run holds the workspace's receipt lock.
fn busy(workspace: &str) -> anyhow::Error {
    anyhow::Error::new(Busy::setup(workspace))
}

/// [`Receipt::try_lock`], retried briefly: a lock is released when the last
/// duplicate of its file descriptor closes, and a process spawned by another
/// thread can hold a duplicate for a few milliseconds.
fn acquire(git_dir: &Path) -> Result<Option<crate::receipt::ReceiptLock>> {
    Receipt::lock_retrying(git_dir)
}

/// A branch this instance may reset or drop.
fn owned_branch(receipt: &Receipt, binding: &ServiceBinding) -> Result<uuid::Uuid> {
    let id = binding.branch_id.context(
        "this workspace is attached to a database by URL only; devflow cannot reset or drop it",
    )?;
    ensure!(
        binding.owned,
        "this workspace's database was not created by this instance ({}); \
         set the workspace up as a new instance to give it its own branch",
        receipt.instance_id
    );
    Ok(id)
}

fn connect(project: &Project) -> Result<Option<Server>> {
    match Server::for_project(project)? {
        Some(server) => {
            server.up()?;
            Ok(Some(server))
        }
        None => Ok(None),
    }
}

fn connect_required(project: &Project) -> Result<Server> {
    connect(project)?.context("this project has no postgres service in [services]")
}

/// Run the project's commands of `phase` in `workspace`.
fn run_hooks(
    project: &Project,
    workspace: &Workspace,
    receipt: &Receipt,
    phase: HookPhase,
    progress: &mut dyn Progress,
) -> Result<HookReport> {
    // Hooks only run for an approved scope, which includes the recipes.
    let env = workspace_env(project, workspace, receipt, true);
    hooks::run(
        phase,
        &project.config.hooks.commands(phase)?,
        &workspace.path,
        &env.vars,
        &env.secrets,
        &Receipt::logs_dir(&workspace.git_dir),
        &mut |line| progress.output(line),
    )
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize()
        .unwrap_or_else(|_| path.components().collect())
}

/// What `--database-url` may be: a `postgres://` or `postgresql://` URL that
/// [`conn::Params`] accepts, so that `DATABASE_URL` and the generated `PG*`
/// variables reach the same server. Rejected here, before anything is created,
/// rather than as a surprise in a hook later.
fn attach_params(url: &str) -> Result<conn::Params> {
    let url = url.trim();
    ensure!(
        url.starts_with("postgres://") || url.starts_with("postgresql://"),
        "--database-url must be a postgres:// or postgresql:// URL"
    );
    conn::Params::parse(url).map_err(|e| anyhow::anyhow!("--database-url cannot be used: {e}"))
}

/// Refuse options that cannot succeed before any effect.
fn validate_options(options: &SetupOptions) -> Result<()> {
    match (&options.database_url, options.no_db) {
        (Some(url), false) => attach_params(url).map(|_| ()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_endpoint_refresh_never_restores_a_receipt_rewritten_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::Config::default();
        config.set_postgres("db", Default::default());
        let service = |branch: Option<uuid::Uuid>, database: &str, role: &str, url: &str| {
            ServiceBinding::postgres(
                DataBinding {
                    branch_id: branch,
                    database: database.into(),
                    role: role.into(),
                    url: url.into(),
                    source: "empty".into(),
                    owned: branch.is_some(),
                },
                None,
            )
        };
        let mut original = Receipt::new(uuid::Uuid::new_v4(), "feature", None);
        original.services.insert(
            "db".into(),
            service(
                Some(uuid::Uuid::new_v4()),
                "old_db",
                "dfl_old",
                "postgres://dfl_old:pw@127.0.0.1:1111/old_db", // kingfisher:ignore
            ),
        );
        original.save(dir.path()).unwrap();
        // A switch read the receipt; meanwhile a fork re-attached the worktree.
        let mut stale = original.clone();
        let mut forked = original.clone();
        forked.instance_id = uuid::Uuid::new_v4();
        forked.services.insert(
            "db".into(),
            service(
                None,
                "new_db",
                "app",
                "postgres://app:pw@db.internal/new_db", // kingfisher:ignore
            ),
        );
        forked.save(dir.path()).unwrap();

        let new_url = "postgres://dfl_old:pw@127.0.0.1:2222/old_db".to_string(); // kingfisher:ignore
        assert!(
            !rebind_url_if_unchanged(dir.path(), &config, &mut stale, "db", new_url.clone())
                .unwrap()
        );
        assert_eq!(Receipt::load(dir.path()).unwrap().unwrap(), forked);
        assert_eq!(stale, forked, "later steps must see the current receipt");

        // Unchanged since it was read: the URL is rewritten.
        let mut fresh = forked.clone();
        fresh.services.get_mut("db").unwrap().url =
            "postgres://app:pw@db.internal:5432/new_db".into(); // kingfisher:ignore
        fresh.save(dir.path()).unwrap();
        let mut read = fresh.clone();
        let url = "postgres://app:pw@db.internal:6000/new_db".to_string(); // kingfisher:ignore
        assert!(
            rebind_url_if_unchanged(dir.path(), &config, &mut read, "db", url.clone()).unwrap()
        );
        assert_eq!(
            Receipt::load(dir.path()).unwrap().unwrap().services["db"].url,
            url
        );
    }

    #[test]
    fn attach_urls_are_checked_by_the_libpq_parser() {
        let ok = |url: &str| attach_params(url).unwrap();
        let params = ok("postgres://dfl_abc:p%40ss%3Aw@db.internal:6543/df_x_y?sslmode=require"); // kingfisher:ignore
        assert_eq!(params.get("user"), Some("dfl_abc"));
        assert_eq!(params.password(), Some("p@ss:w"));
        assert_eq!(params.get("host"), Some("db.internal"));
        assert_eq!(params.get("port"), Some("6543"));
        assert_eq!(params.get("dbname"), Some("df_x_y"));
        assert_eq!(params.get("sslmode"), Some("require"));
        // Whitespace around the URL is not part of it.
        ok("  postgresql://u@localhost/d \n");
        // libpq's own defaults need no URL parts.
        ok("postgres:///d");
        for (bad, why) in [
            ("mysql://x/y", "scheme"),
            ("host=h port=1 dbname=d", "conninfo is not a URL"),
            ("postgres://u@h1,h2/d", "several hosts"),
            ("postgres://u@h/d?host=a,b", "several hosts in a parameter"),
            ("postgres://u@h/d?sslpassword=x", "sslpassword"),
            ("postgres://u@h/d?replication=database", "replication"),
            ("postgres://u@h/d?x", "parameter without a value"),
        ] {
            let error = attach_params(bad).unwrap_err().to_string();
            assert!(error.starts_with("--database-url"), "{why}: {error}");
        }
        // A rejected URL never appears in its own error.
        let error = attach_params("postgres://u:topsecret@h1,h2/d") // kingfisher:ignore
            .unwrap_err()
            .to_string();
        assert!(!error.contains("topsecret"), "{error}");
    }

    #[test]
    fn generated_pg_variables_say_what_the_url_says() {
        let project_id = uuid::Uuid::new_v4();
        let vars_for = |url: &str| {
            let t = crate::worktree::testutil::repo(None);
            let ws = crate::worktree::list(&t.project).unwrap().remove(0);
            let mut receipt = Receipt::new(project_id, "w", None);
            receipt.services.insert(
                "db".into(),
                ServiceBinding::postgres(
                    DataBinding {
                        branch_id: None,
                        database: "fallback_db".into(),
                        role: "fallback_role".into(),
                        url: url.into(),
                        source: "url".into(),
                        owned: false,
                    },
                    None,
                ),
            );
            env_vars(&t.project, &ws, &receipt)
        };
        let get = |vars: &[(String, String)], key: &str| {
            vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
        };
        // hostaddr and a repeated sslmode: the last one wins, as in libpq.
        let url = "postgres://u:pw@db.example:6543/app?hostaddr=192.0.2.44&sslmode=disable&sslmode=require";
        let vars = vars_for(url);
        assert_eq!(get(&vars, "DATABASE_URL").as_deref(), Some(url));
        assert_eq!(get(&vars, "PGHOSTADDR").as_deref(), Some("192.0.2.44"));
        assert_eq!(get(&vars, "PGSSLMODE").as_deref(), Some("require"));
        assert_eq!(get(&vars, "PGHOST").as_deref(), Some("db.example"));
        assert_eq!(get(&vars, "PGPORT").as_deref(), Some("6543"));
        assert_eq!(get(&vars, "PGUSER").as_deref(), Some("u"));
        assert_eq!(get(&vars, "PGPASSWORD").as_deref(), Some("pw"));
        assert_eq!(get(&vars, "PGDATABASE").as_deref(), Some("app"));
        // IPv6 literal.
        let vars = vars_for("postgresql://u@[2001:db8::1]:5433/d");
        assert_eq!(get(&vars, "PGHOST").as_deref(), Some("2001:db8::1"));
        assert_eq!(get(&vars, "PGPORT").as_deref(), Some("5433"));
        assert_eq!(get(&vars, "PGPASSWORD"), None);
        // Query parameters override the authority; percent-decoded names count.
        let vars =
            vars_for("postgres://u:a@h1:1111/d1?host=h2&port=2222&%70assword=p%2Fw&dbname=d2");
        assert_eq!(get(&vars, "PGHOST").as_deref(), Some("h2"));
        assert_eq!(get(&vars, "PGPORT").as_deref(), Some("2222"));
        assert_eq!(get(&vars, "PGPASSWORD").as_deref(), Some("p/w"));
        assert_eq!(get(&vars, "PGDATABASE").as_deref(), Some("d2"));
        // Prisma-style options stay in DATABASE_URL and never reach PG*.
        let url = "postgres://u:pw@h:5432/app?schema=public&connection_limit=5";
        let vars = vars_for(url);
        assert_eq!(get(&vars, "DATABASE_URL").as_deref(), Some(url));
        assert!(
            vars.iter()
                .all(|(k, v)| !k.contains("SCHEMA") && !v.contains("public") || k == "DATABASE_URL"),
            "{vars:?}"
        );
        assert_eq!(get(&vars, "PGDATABASE").as_deref(), Some("app"));
        // Other mapped keywords come along.
        let vars =
            vars_for("postgres://u@h/d?sslrootcert=/ca.pem&application_name=x&connect_timeout=3");
        assert_eq!(get(&vars, "PGSSLROOTCERT").as_deref(), Some("/ca.pem"));
        assert_eq!(get(&vars, "PGAPPNAME").as_deref(), Some("x"));
        assert_eq!(get(&vars, "PGCONNECT_TIMEOUT").as_deref(), Some("3"));
        // A binding that does not parse still names its role and database.
        let vars = vars_for("not a url");
        assert_eq!(get(&vars, "PGUSER").as_deref(), Some("fallback_role"));
        assert_eq!(get(&vars, "PGDATABASE").as_deref(), Some("fallback_db"));
    }

    #[test]
    fn container_services_export_their_url_under_their_variable_and_as_a_secret() {
        const HEAD: &str =
            "[project]\nname = \"shop\"\nid = \"0b3c9d2e-1111-4222-8333-444455556666\"\n";
        let config = format!(
            "{HEAD}[services.cache]\nengine = \"valkey\"\n\n[services.queue]\nengine = \"redis\"\n\
             env = \"QUEUE_URL\"\n\n[env]\nSESSION_STORE = \"${{REDIS_URL}}\"\n"
        );
        let t = crate::worktree::testutil::repo(Some(&config));
        let ws = crate::worktree::list(&t.project).unwrap().remove(0);
        let mut receipt = Receipt::new(t.project.id(), "w", None);
        let cache_url = "redis://:s3cretpw@127.0.0.1:23456/0"; // kingfisher:ignore
        let queue_url = "redis://:0therpw@127.0.0.1:23457/0"; // kingfisher:ignore
        let binding = |engine, url: &str| {
            ServiceBinding::container(
                engine,
                uuid::Uuid::new_v4(),
                url.into(),
                Some("abcd1234.cache.shop-0b3c9d2e.devflow.orb.local:6379".into()),
                "empty".into(),
                receipt::CloneReport {
                    method: None,
                    captured_at: None,
                    allocated_bytes: None,
                    source_downtime_ms: None,
                },
                None,
            )
        };
        receipt
            .services
            .insert("cache".into(), binding(Engine::Valkey, cache_url));
        receipt
            .services
            .insert("queue".into(), binding(Engine::Redis, queue_url));
        let vars = env_vars(&t.project, &ws, &receipt);
        let get = |key: &str| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        assert_eq!(get("REDIS_URL").as_deref(), Some(cache_url));
        assert_eq!(get("QUEUE_URL").as_deref(), Some(queue_url));
        assert_eq!(get("DATABASE_URL"), None, "no postgres, no DATABASE_URL");

        // A recipe can use the URL and is then a secret, masked whole.
        let env = workspace_env(&t.project, &ws, &receipt, true);
        let store = env.vars.iter().find(|(k, _)| k == "SESSION_STORE").unwrap();
        assert_eq!(store.1, cache_url);
        assert!(
            env.secrets.iter().any(|s| s == cache_url),
            "{:?}",
            env.secrets
        );
        assert!(!crate::redact::text(cache_url).contains("s3cretpw"));

        // A binding the configuration dropped is not exported, and one whose
        // removal was applied stays out.
        receipt.services.get_mut("queue").unwrap().orphaned = true;
        let vars = env_vars(&t.project, &ws, &receipt);
        assert!(vars.iter().all(|(k, _)| k != "QUEUE_URL"));
        // A binding of another engine under the key is not this service's.
        receipt
            .services
            .insert("cache".into(), binding(Engine::Redis, cache_url));
        let vars = env_vars(&t.project, &ws, &receipt);
        assert!(vars.iter().all(|(k, _)| k != "REDIS_URL"));
    }

    #[test]
    fn a_mysql_service_exports_its_url_and_typed_fields_under_the_right_names() {
        const HEAD: &str =
            "[project]\nname = \"shop\"\nid = \"0b3c9d2e-1111-4222-8333-444455556666\"\n";
        let url = "mysql://app:s3cretpw@127.0.0.1:23456/shop"; // kingfisher:ignore
        let binding = ServiceBinding::container(
            Engine::Mysql,
            uuid::Uuid::new_v4(),
            url.into(),
            None,
            "empty".into(),
            receipt::CloneReport {
                method: None,
                captured_at: None,
                allocated_bytes: None,
                source_downtime_ms: None,
            },
            None,
        );
        // Alone, MySQL takes DATABASE_URL; the fields come with it, and a
        // recipe can use them.
        let config = format!(
            "{HEAD}[services.sql]\nengine = \"mysql\"\n\n[env]\nJDBC = \
             \"jdbc:mysql://${{MYSQL_HOST}}:${{MYSQL_PORT}}/${{MYSQL_DATABASE}}\"\n"
        );
        let t = crate::worktree::testutil::repo(Some(&config));
        let ws = crate::worktree::list(&t.project).unwrap().remove(0);
        let mut receipt = Receipt::new(t.project.id(), "w", None);
        receipt.services.insert("sql".into(), binding.clone());
        let vars = env_vars(&t.project, &ws, &receipt);
        let get = |key: &str| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        assert_eq!(get("DATABASE_URL").as_deref(), Some(url));
        assert_eq!(get("MYSQL_URL"), None);
        assert_eq!(get("MYSQL_HOST").as_deref(), Some("127.0.0.1"));
        assert_eq!(get("MYSQL_PORT").as_deref(), Some("23456"));
        assert_eq!(get("MYSQL_USER").as_deref(), Some("app"));
        assert_eq!(get("MYSQL_PASSWORD").as_deref(), Some("s3cretpw")); // kingfisher:ignore
        assert_eq!(get("MYSQL_DATABASE").as_deref(), Some("shop"));
        assert_eq!(get("PGHOST"), None, "the PG* variables are postgres's");
        let env = workspace_env(&t.project, &ws, &receipt, true);
        let jdbc = env.vars.iter().find(|(k, _)| k == "JDBC").unwrap();
        assert_eq!(jdbc.1, "jdbc:mysql://127.0.0.1:23456/shop");

        // Beside a postgres service, its URL is MYSQL_URL and DATABASE_URL
        // stays postgres's.
        let config = format!(
            "{HEAD}[services.db]\nengine = \"postgres\"\n\n[services.sql]\nengine = \"mysql\"\n"
        );
        let t = crate::worktree::testutil::repo(Some(&config));
        let ws = crate::worktree::list(&t.project).unwrap().remove(0);
        let mut receipt = Receipt::new(t.project.id(), "w", None);
        receipt.services.insert("sql".into(), binding);
        let vars = env_vars(&t.project, &ws, &receipt);
        let get = |key: &str| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        assert_eq!(get("MYSQL_URL").as_deref(), Some(url));
        assert_eq!(
            get("DATABASE_URL"),
            None,
            "no postgres binding, none made up"
        );
        assert_eq!(get("MYSQL_PASSWORD").as_deref(), Some("s3cretpw")); // kingfisher:ignore
    }

    #[test]
    fn endpoint_replacement_keeps_credentials_and_query() {
        assert_eq!(
            with_endpoint(
                "postgres://u:p%40@127.0.0.1:5433/db?sslmode=disable", // kingfisher:ignore
                "127.0.0.1",
                6000
            )
            .unwrap(),
            "postgres://u:p%40@127.0.0.1:6000/db?sslmode=disable" // kingfisher:ignore
        );
        assert_eq!(
            with_endpoint("postgres://u:p@h:1/db", "::1", 2).unwrap(),
            "postgres://u:p@[::1]:2/db"
        );
    }

    #[test]
    fn a_reused_binding_follows_the_endpoint_and_keeps_its_credentials() {
        let data = |branch: Option<uuid::Uuid>| DataBinding {
            branch_id: branch,
            database: "df_a_b".into(),
            role: "dfl_b".into(),
            url: "postgres://dfl_b:p%40ss@127.0.0.1:20001/df_a_b?sslmode=disable".into(), // kingfisher:ignore
            source: "empty".into(),
            owned: branch.is_some(),
        };
        let mut owned = ServiceBinding::postgres(data(Some(uuid::Uuid::new_v4())), None);
        assert_eq!(
            rebind_endpoint(&mut owned, "127.0.0.1", 20001),
            None,
            "an unchanged endpoint changes nothing"
        );
        assert_eq!(
            rebind_endpoint(&mut owned, "db.internal", 6543).as_deref(),
            Some("db.internal:6543")
        );
        assert_eq!(
            owned.url,
            "postgres://dfl_b:p%40ss@db.internal:6543/df_a_b?sslmode=disable" // kingfisher:ignore
        );
        // Attachments belong to whoever supplied them.
        let mut attached = ServiceBinding::postgres(data(None), None);
        let before = attached.url.clone();
        assert_eq!(rebind_endpoint(&mut attached, "db.internal", 6543), None);
        assert_eq!(attached.url, before);
    }

    #[test]
    fn data_sources_round_trip_through_receipts() {
        let id = uuid::Uuid::new_v4();
        for source in [
            DataSource::Empty,
            DataSource::Workspace {
                name: "empty".into(),
            },
            DataSource::Workspace {
                name: "feature/a".into(),
            },
            DataSource::Branch { id },
            DataSource::Snapshot {
                name: "seed".into(),
                version: Some(3),
            },
            DataSource::Snapshot {
                name: "seed".into(),
                version: None,
            },
        ] {
            assert_eq!(decode_source(&encode_source(&source)).unwrap(), source);
        }
    }

    #[test]
    fn forking_resets_identity_binding_and_late_phases() {
        let project_id = uuid::Uuid::new_v4();
        let mut receipt = Receipt::new(project_id, "feature", Some("main".into()));
        let branch = uuid::Uuid::new_v4();
        receipt.data_source = Some("workspace:main".into());
        receipt.services.insert(
            "db".into(),
            ServiceBinding::postgres(
                DataBinding {
                    branch_id: Some(branch),
                    database: "df_a_b".into(),
                    role: "dfl_b".into(),
                    url: "postgres://dfl_b:pw@h:1/df_a_b".into(), // kingfisher:ignore
                    source: "workspace:main".into(),
                    owned: true,
                },
                Some("generation".into()),
            ),
        );
        receipt.env_generation = Some("env".into());
        receipt.hooks_digest = Some("abc".into());
        receipt.creating = Some(PendingBranch {
            service: "db".into(),
            branch_id: uuid::Uuid::new_v4(),
        });
        let mut config = crate::Config::default();
        config.set_postgres("db", Default::default());
        for phase in Phase::ALL {
            receipt.set_phase(phase, PhaseState::Done, None);
        }
        let old_instance = receipt.instance_id;

        fork_receipt(&mut receipt, &config, &SetupOptions::default(), None);
        assert_ne!(receipt.instance_id, old_instance);
        assert!(receipt.services.is_empty() && receipt.unmapped.is_none());
        assert!(
            receipt.creating.is_none(),
            "a copy never recovers the original's creation"
        );
        assert!(receipt.hooks_digest.is_none() && receipt.env_generation.is_none());
        assert_eq!(receipt.data_source, Some(format!("branch:{branch}")));
        assert_eq!(
            receipt.data_source_id,
            Some(format!("branch:{branch}")),
            "the previous branch is pinned by identity"
        );
        assert_eq!(receipt.code_source.as_deref(), Some("main"));
        assert!(!complete(&receipt, Phase::Services));
        assert_eq!(receipt.status(), Status::InProgress);
        assert_eq!(receipt.phase(Phase::Payload).state, PhaseState::Done);
        assert_eq!(receipt.phase(Phase::Env).state, PhaseState::Pending);
        assert_eq!(receipt.phase(Phase::Hooks).state, PhaseState::Pending);

        // --data-from wins; without a binding the recorded source is kept.
        let mut receipt = Receipt::new(project_id, "feature", None);
        let snapshot = uuid::Uuid::new_v4();
        receipt.data_source = Some("snapshot:seed@v1".into());
        receipt.data_source_id = Some(format!("snapshot:{snapshot}"));
        fork_receipt(&mut receipt, &config, &SetupOptions::default(), None);
        assert_eq!(receipt.data_source.as_deref(), Some("snapshot:seed@v1"));
        assert_eq!(
            receipt.data_source_id,
            Some(format!("snapshot:{snapshot}")),
            "a kept source keeps its resolved identity"
        );
        fork_receipt(
            &mut receipt,
            &config,
            &SetupOptions {
                data_from: Some(DataSource::Empty),
                ..Default::default()
            },
            None,
        );
        assert_eq!(receipt.data_source.as_deref(), Some("empty"));
        assert_eq!(
            receipt.data_source_id, None,
            "resolved again when provisioning"
        );

        // DEVFLOW_INSTANCE_ID names the new identity unless it is the old one.
        let named = uuid::Uuid::new_v4();
        fork_receipt(&mut receipt, &config, &SetupOptions::default(), Some(named));
        assert_eq!(receipt.instance_id, named);
        fork_receipt(&mut receipt, &config, &SetupOptions::default(), Some(named));
        assert_ne!(
            receipt.instance_id, named,
            "same id again: fresh random one"
        );
    }

    #[test]
    fn an_interrupted_creation_is_recovered_by_its_recorded_identity_for_its_service_only() {
        let mut receipt = Receipt::new(uuid::Uuid::new_v4(), "w", None);
        let branch = uuid::Uuid::new_v4();
        assert_eq!(recoverable(&receipt, "db"), None, "nothing in flight");
        receipt.creating = Some(PendingBranch {
            service: "db".into(),
            branch_id: branch,
        });
        assert_eq!(recoverable(&receipt, "db"), Some(branch));
        // The service was renamed: the new key has no intent of its own.
        assert_eq!(recoverable(&receipt, "main"), None);
        // A branch already recorded (under any key, or unmapped) is nobody's
        // to recover again.
        let data = DataBinding {
            branch_id: Some(branch),
            database: "df_a_b".into(),
            role: "dfl_b".into(),
            url: "postgres://dfl_b:pw@h:1/df_a_b".into(), // kingfisher:ignore
            source: "empty".into(),
            owned: true,
        };
        receipt.services.insert(
            "old".into(),
            ServiceBinding::postgres(data.clone(), Some("g".into())),
        );
        assert_eq!(recoverable(&receipt, "db"), None);
        receipt.services.clear();
        receipt.unmapped = Some(data);
        assert_eq!(recoverable(&receipt, "db"), None);
    }

    #[test]
    fn a_workspace_that_is_not_ready_gets_remedies_from_its_phases_not_its_wording() {
        let path = Path::new("/work/feature");
        let setup = Remedy::Setup {
            workspace: path.to_path_buf(),
        };
        let trust = Remedy::Trust {
            workspace: path.to_path_buf(),
        };
        // Not set up at all: set it up.
        assert_eq!(
            remedies_given(path, None, false, || false),
            std::slice::from_ref(&setup)
        );
        // A setup run holds it: nothing to do but wait.
        assert!(remedies_given(path, None, true, || true).is_empty());

        let mut receipt = Receipt::new(uuid::Uuid::new_v4(), "feature", None);
        for phase in [Phase::Materialize, Phase::Payload, Phase::Services] {
            receipt.set_phase(phase, PhaseState::Done, None);
        }
        receipt.set_phase(Phase::Env, PhaseState::Done, None);
        receipt.skip_required(Phase::Hooks, "any words at all".into());
        // Hooks waiting for approval that has not been given: review first.
        assert_eq!(
            remedies_given(path, Some(&receipt), false, || true),
            [trust, setup.clone()]
        );
        // The approval is on record: only setup has to run again.
        assert_eq!(
            remedies_given(path, Some(&receipt), false, || false),
            std::slice::from_ref(&setup)
        );
        // A failed phase is not about approval, and the question is not asked.
        let mut failed = Receipt::new(uuid::Uuid::new_v4(), "feature", None);
        failed.set_phase(Phase::Materialize, PhaseState::Failed, Some("boom".into()));
        assert_eq!(
            remedies_given(path, Some(&failed), false, || panic!("not asked")),
            [setup]
        );
        // Ready: nothing to fix.
        receipt.set_phase(Phase::Hooks, PhaseState::Done, None);
        assert!(remedies_given(path, Some(&receipt), false, || true).is_empty());
    }

    #[test]
    fn what_setup_tells_a_person_has_no_command_line_in_it() {
        for text in [UNTRUSTED, UNTRUSTED_ENV, DIFFERS] {
            assert!(
                !text.contains("`devflow") && !text.contains("run devflow"),
                "{text}"
            );
        }
        let workspace = Workspace {
            name: "feature".into(),
            branch: Some("feature".into()),
            path: PathBuf::from("/work/feature"),
            git_dir: PathBuf::from("/repo/.git/worktrees/feature"),
            is_default: false,
            locked: false,
            missing: false,
            head: None,
            fingerprint: None,
        };
        let error = needs_setup(&workspace);
        assert!(!error.to_string().contains("devflow setup"), "{error}");
        assert_eq!(
            crate::remedy::remedies_of(&error),
            [Remedy::Setup {
                workspace: workspace.path.clone()
            }]
        );
        // A failed phase keeps the cause under a problem that carries the remedy.
        let failed = anyhow::anyhow!("boom").context(
            Problem::new("hooks failed; 'feature' is kept at /work/feature").with(Remedy::Setup {
                workspace: workspace.path.clone(),
            }),
        );
        assert_eq!(crate::remedy::remedies_of(&failed).len(), 1);
        assert!(format!("{failed:#}").ends_with("boom"), "{failed:#}");
    }

    #[test]
    fn resume_reruns_required_skips_and_failures_only() {
        let mut receipt = Receipt::new(uuid::Uuid::new_v4(), "w", None);
        receipt.set_phase(Phase::Payload, PhaseState::Done, None);
        receipt.set_phase(
            Phase::Services,
            PhaseState::Skipped,
            Some("no postgres service".into()),
        );
        receipt.skip_required(Phase::Hooks, UNTRUSTED.into());
        receipt.set_phase(Phase::Env, PhaseState::Started, None);
        assert!(complete(&receipt, Phase::Payload));
        assert!(complete(&receipt, Phase::Services));
        assert!(!complete(&receipt, Phase::Env));
        assert!(!complete(&receipt, Phase::Hooks));
        receipt.set_phase(Phase::Env, PhaseState::Failed, Some("boom".into()));
        assert!(!complete(&receipt, Phase::Env));
    }

    #[test]
    fn status_and_the_runner_agree_on_what_is_complete() {
        // Every state a phase can be in: the runner skips a phase exactly when
        // the receipt would not hold the workspace back for it.
        let mut cases = vec![
            (PhaseState::Pending, false, false),
            (PhaseState::Started, false, false),
            (PhaseState::Done, false, true),
            (PhaseState::Skipped, false, true),
            (PhaseState::Skipped, true, false),
            (PhaseState::Failed, false, false),
        ];
        for (state, required, complete_expected) in cases.drain(..) {
            let mut receipt = Receipt::new(uuid::Uuid::new_v4(), "w", None);
            for phase in Phase::ALL {
                receipt.set_phase(phase, PhaseState::Done, None);
            }
            if required {
                receipt.skip_required(Phase::Hooks, "untrusted".into());
            } else {
                receipt.set_phase(Phase::Hooks, state, None);
            }
            assert_eq!(
                complete(&receipt, Phase::Hooks),
                complete_expected,
                "{state:?}"
            );
            assert_eq!(
                receipt.status() == Status::Ready,
                complete_expected,
                "{state:?} required={required}"
            );
        }
    }

    #[test]
    fn data_source_identities_round_trip() {
        let id = uuid::Uuid::new_v4();
        for source in [
            ResolvedSource::Empty,
            ResolvedSource::Branch(id),
            ResolvedSource::Snapshot(id),
        ] {
            assert_eq!(decode_resolved(&encode_resolved(source)).unwrap(), source);
        }
        assert!(decode_resolved("workspace:main").is_err());
        assert!(decode_resolved("branch:not-a-uuid").is_err());
    }

    fn branch_info(kind: &str, source: Option<uuid::Uuid>) -> BranchInfo {
        BranchInfo {
            id: uuid::Uuid::new_v4(),
            name: "b".into(),
            db_name: "df_b".into(),
            role_name: "dfl_b".into(),
            source_kind: kind.into(),
            source_id: source,
            instance_id: None,
            status: RecordStatus::Ready,
            cow: crate::config::CowCapability::Unknown,
            error: None,
            created_at: chrono::Utc::now(),
            size_bytes: None,
            generation: 1,
            extras: Vec::new(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn default_reset_follows_the_branch_provenance_not_a_name() {
        let origin = uuid::Uuid::new_v4();
        assert_eq!(
            provenance(&branch_info("empty", None)).unwrap(),
            ResolvedSource::Empty
        );
        assert_eq!(
            provenance(&branch_info("branch", Some(origin))).unwrap(),
            ResolvedSource::Branch(origin)
        );
        assert_eq!(
            provenance(&branch_info("snapshot", Some(origin))).unwrap(),
            ResolvedSource::Snapshot(origin)
        );
        assert!(provenance(&branch_info("branch", None)).is_err());
        assert!(provenance(&branch_info("mystery", Some(origin))).is_err());
    }

    /// A project that configures `cache` and a workspace whose receipt binds
    /// it to branch `branch` (no Docker is involved until the expectation
    /// holds).
    fn bound_workspace(dir: &Path, branch: uuid::Uuid) -> (Project, Workspace, Receipt) {
        let config = crate::Config::parse(
            "[project]\nname = \"shop\"\nid = \"0b3c9d2e-1111-4222-8333-444455556666\"\n\
             [services.cache]\nengine = \"valkey\"\n",
        )
        .unwrap();
        let project = Project {
            root: dir.join("shop"),
            git_common_dir: dir.join("shop/.git"),
            config_path: dir.join("shop/devflow.toml"),
            config,
        };
        let git_dir = dir.join("git");
        std::fs::create_dir_all(&git_dir).unwrap();
        let workspace = Workspace {
            name: "feature".into(),
            branch: Some("feature".into()),
            path: dir.join("feature"),
            git_dir: git_dir.clone(),
            is_default: false,
            locked: false,
            missing: false,
            head: None,
            fingerprint: None,
        };
        let mut receipt = Receipt::new(project.id(), "feature", None);
        receipt.services.insert(
            "cache".into(),
            ServiceBinding::container(
                Engine::Valkey,
                branch,
                "redis://:pw@127.0.0.1:6381/0".into(), // kingfisher:ignore
                None,
                "empty".into(),
                receipt::CloneReport {
                    method: None,
                    captured_at: None,
                    allocated_bytes: None,
                    source_downtime_ms: None,
                },
                None,
            ),
        );
        receipt.save(&git_dir).unwrap();
        (project, workspace, receipt)
    }

    #[test]
    fn a_reset_or_snapshot_of_a_replaced_instance_or_branch_is_refused_before_any_effect() {
        let dir = tempfile::tempdir().unwrap();
        let branch = uuid::Uuid::new_v4();
        let (project, workspace, receipt) = bound_workspace(dir.path(), branch);
        // The dialog showed the instance and branch the receipt has now: the
        // check passes (the work would go on to Docker, which this test never
        // reaches).
        let shown = BranchExpect {
            instance: receipt.instance_id,
            branch,
            source: None,
        };
        shown.verify(&workspace, "cache", &receipt).unwrap();

        // `setup --new-instance` ran after the dialog opened: another
        // instance holds the workspace, with a branch of its own.
        let mut replaced = receipt.clone();
        replaced.instance_id = uuid::Uuid::new_v4();
        let replacement = uuid::Uuid::new_v4();
        replaced.services.get_mut("cache").unwrap().branch_id = Some(replacement);
        replaced.save(&workspace.git_dir).unwrap();
        let on_disk = Receipt::load(&workspace.git_dir).unwrap().unwrap();

        let reset = service_reset(&project, &workspace, "cache", None, Some(shown));
        let error = reset.unwrap_err();
        let changed = error
            .downcast_ref::<WorkspaceChanged>()
            .unwrap_or_else(|| panic!("not the typed error: {error:#}"));
        assert_eq!(
            (changed.workspace.as_str(), changed.service.as_str()),
            ("feature", "cache")
        );
        assert!(
            error.to_string().contains("changed since you reviewed it"),
            "{error}"
        );
        let snapshot = service_snapshot(&project, &workspace, "cache", "seed", Some(shown));
        assert!(
            snapshot
                .unwrap_err()
                .downcast_ref::<WorkspaceChanged>()
                .is_some()
        );
        // Nothing was written: the replacement's receipt is as it was.
        assert_eq!(
            Receipt::load(&workspace.git_dir)
                .unwrap()
                .unwrap()
                .instance_id,
            on_disk.instance_id
        );
        assert_eq!(
            Receipt::load(&workspace.git_dir).unwrap().unwrap().services["cache"].branch_id,
            Some(replacement)
        );

        // The same instance rebound to another branch is refused as well, and
        // so is a binding that went away.
        let mut rebound = receipt.clone();
        rebound.services.get_mut("cache").unwrap().branch_id = Some(replacement);
        assert!(shown.verify(&workspace, "cache", &rebound).is_err());
        let mut unbound = receipt.clone();
        unbound.services.clear();
        let error = shown.verify(&workspace, "cache", &unbound).unwrap_err();
        assert!(error.to_string().contains("no longer bound"), "{error}");
    }

    #[test]
    fn a_failed_phase_keeps_the_causes_remedies_before_the_retry() {
        let workspace = Workspace {
            name: "feature".into(),
            branch: Some("feature".into()),
            path: PathBuf::from("/work/feature"),
            git_dir: PathBuf::from("/repo/.git/worktrees/feature"),
            is_default: false,
            locked: false,
            missing: false,
            head: None,
            fingerprint: None,
        };
        let setup = Remedy::Setup {
            workspace: workspace.path.clone(),
        };
        let start = Remedy::StartDatabase {
            root: PathBuf::from("/work/shop"),
        };
        let cause = Problem::new("the managed PostgreSQL server is not running")
            .with(start.clone())
            .with(setup.clone())
            .into_error();
        let error = failed_phase(&workspace, Phase::Services, cause);
        assert!(
            error.to_string().starts_with("services failed;"),
            "{error:#}"
        );
        // The cause's fix is not hidden by the retry, and the retry is there once.
        assert_eq!(
            crate::remedy::remedies_of(&error),
            [start.clone(), setup.clone()]
        );
        // A cause with no remedies of its own leaves just the retry.
        let plain = failed_phase(&workspace, Phase::Hooks, anyhow::anyhow!("boom"));
        assert_eq!(
            crate::remedy::remedies_of(&plain),
            std::slice::from_ref(&setup)
        );
        // A lost branch's own remedy (drop it) comes first as well.
        let drop = Remedy::DropLostBranch {
            root: PathBuf::from("/work/shop"),
            key: "cache".into(),
            branch: uuid::Uuid::nil(),
            volume: "v".into(),
        };
        let lost = failed_phase(
            &workspace,
            Phase::Services,
            Problem::new("volume v no longer exists")
                .with(drop.clone())
                .into_error(),
        );
        assert_eq!(crate::remedy::remedies_of(&lost), [drop, setup]);
    }

    /// A ready workspace bound to a valkey branch, in a project whose catalog
    /// file holds (or does not hold) that branch.
    fn bound_to_cache(
        with_branch: bool,
    ) -> (crate::worktree::testutil::TestRepo, Workspace, uuid::Uuid) {
        bound_to_cache_with("", with_branch)
    }

    /// [`bound_to_cache`] whose `[hooks] setup` is `setup` (TOML array items).
    fn bound_to_cache_with(
        setup: &str,
        with_branch: bool,
    ) -> (crate::worktree::testutil::TestRepo, Workspace, uuid::Uuid) {
        use crate::services::catalog::{BranchRecord, Catalog, ServiceCatalog, VERSION};
        let config = format!(
            "{}\n[services.cache]\nengine = \"valkey\"\n",
            crate::Config::template("shop", uuid::Uuid::new_v4(), None)
                .replace("setup = []", &format!("setup = [{setup}]"))
        );
        let t = crate::worktree::testutil::repo(Some(&config));
        let ws = crate::worktree::create(&t.project, "feature", None).unwrap();
        let branch = uuid::Uuid::new_v4();
        let mut receipt = Receipt::new(t.project.id(), "feature", None);
        receipt.services.insert(
            "cache".into(),
            ServiceBinding::container(
                Engine::Valkey,
                branch,
                "redis://127.0.0.1:1".into(),
                None,
                "empty".into(),
                receipt::CloneReport {
                    method: None,
                    captured_at: None,
                    allocated_bytes: None,
                    source_downtime_ms: None,
                },
                t.project.config.service_generation("cache"),
            ),
        );
        for phase in Phase::ALL {
            receipt.set_phase(phase, PhaseState::Done, None);
        }
        receipt.save(&ws.git_dir).unwrap();
        let mut records = ServiceCatalog::new(Engine::Valkey);
        if with_branch {
            records.branches.push(BranchRecord::for_test(branch));
        }
        let catalog = Catalog {
            version: VERSION,
            project_id: t.project.id(),
            docker: "daemon".into(),
            services: [("cache".to_string(), records)].into(),
        };
        let path = services::catalog::Store::path_for(t.project.id()).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec(&catalog).unwrap()).unwrap();
        (t, ws, branch)
    }

    #[test]
    fn setup_reopens_a_ready_workspace_whose_bound_branch_is_gone_from_the_catalog() {
        let (t, ws, _) = bound_to_cache(true);
        // The branch is there: ready, and nothing is reopened.
        let lock = acquire(&ws.git_dir).unwrap().unwrap();
        let receipt = load_receipt_locked(&t.project, &ws, &lock)
            .unwrap()
            .unwrap();
        assert_eq!(receipt.status(), Status::Ready);
        drop(lock);

        // The lost branch is dropped: its record is gone from the catalog.
        let (t, ws, _) = bound_to_cache(false);
        // What a person sees (nothing is written) is a workspace that needs
        // attention, with setup as the way on.
        let seen = load_receipt(&t.project, &ws).unwrap().unwrap();
        assert_eq!(seen.status(), Status::NeedsAttention);
        assert_eq!(
            remedies_for(&t.project, &ws.path, Some(&seen), false),
            [Remedy::Setup {
                workspace: ws.path.clone()
            }]
        );
        assert_eq!(
            Receipt::load(&ws.git_dir).unwrap().unwrap().status(),
            Status::Ready,
            "reading does not write"
        );
        // Setup reopens services, env and hooks under the receipt lock, so the
        // runner gives the workspace a new branch instead of skipping them.
        let lock = acquire(&ws.git_dir).unwrap().unwrap();
        let receipt = load_receipt_locked(&t.project, &ws, &lock)
            .unwrap()
            .unwrap();
        for phase in [Phase::Services, Phase::Env, Phase::Hooks] {
            assert!(!complete(&receipt, phase), "{phase:?} runs again");
            assert!(receipt.phase(phase).required_skip);
        }
        assert!(complete(&receipt, Phase::Payload), "unrelated phases stay");
        let on_disk = Receipt::load(&ws.git_dir).unwrap().unwrap();
        assert_eq!(on_disk.status(), Status::NeedsAttention);
        // The binding is still the old one until services replaces it, and
        // asking again changes nothing.
        assert!(!reopen_invalidated(&t.project, &mut on_disk.clone()));
        drop(lock);
    }

    #[test]
    fn a_lost_branch_does_not_reopen_what_someone_asked_to_skip_or_prove_nothing_without_a_catalog()
    {
        let (t, ws, _) = bound_to_cache(false);
        // `--no-db` is a request that is kept.
        let mut receipt = Receipt::load(&ws.git_dir).unwrap().unwrap();
        receipt.skip_deliberate(Phase::Services, receipt::SKIP_NO_DB.into());
        receipt.save(&ws.git_dir).unwrap();
        let seen = load_receipt(&t.project, &ws).unwrap().unwrap();
        assert!(seen.phase(Phase::Services).deliberate);
        assert!(!seen.phase(Phase::Services).required_skip);
        // Nothing derived from services applies to such a workspace, so the
        // binding it kept reopens nothing.
        assert!(complete(&seen, Phase::Env) && complete(&seen, Phase::Hooks));
        assert_eq!(seen.status(), Status::Ready);

        // No catalog file proves nothing: nothing is reopened.
        let (t, ws, _) = bound_to_cache(true);
        std::fs::remove_file(services::catalog::Store::path_for(t.project.id()).unwrap()).unwrap();
        let seen = load_receipt(&t.project, &ws).unwrap().unwrap();
        assert_eq!(seen.status(), Status::Ready);
    }

    #[test]
    fn a_no_db_workspace_that_kept_a_lost_binding_stays_ready_and_runs_hooks_once() {
        let (t, ws, _) = bound_to_cache_with(r#""echo run >> .devflow/runs""#, false);
        trust::approve(&t.project).unwrap();
        // `switch feature --no-db` after the branch was lost: services are
        // skipped on request, the old binding stays, and setup has to
        // write the environment and run the hooks.
        let mut receipt = Receipt::load(&ws.git_dir).unwrap().unwrap();
        receipt.skip_deliberate(Phase::Services, receipt::SKIP_NO_DB.into());
        receipt.set_phase(Phase::Env, PhaseState::Pending, None);
        receipt.set_phase(Phase::Hooks, PhaseState::Pending, None);
        receipt.save(&ws.git_dir).unwrap();
        let runs = || {
            std::fs::read_to_string(ws.path.join(".devflow/runs"))
                .map(|text| text.lines().count())
                .unwrap_or(0)
        };
        let run_setup = || {
            setup(
                &t.project,
                &ws,
                SetupOptions::default(),
                false,
                false,
                &mut NoProgress,
            )
            .unwrap()
            .unwrap()
        };
        let outcome = run_setup();
        assert_eq!(
            outcome.status,
            Status::Ready,
            "{:?}",
            outcome.receipt.phases
        );
        assert_eq!(runs(), 1);
        assert!(
            outcome.receipt.services.contains_key("cache"),
            "the binding is kept, not cleared"
        );

        // Load, set up, load again: nothing flaps and the hooks do not run again.
        for _ in 0..3 {
            let seen = load_receipt(&t.project, &ws).unwrap().unwrap();
            assert_eq!(seen.status(), Status::Ready);
            let lock = acquire(&ws.git_dir).unwrap().unwrap();
            let locked = load_receipt_locked(&t.project, &ws, &lock)
                .unwrap()
                .unwrap();
            assert_eq!(locked.status(), Status::Ready);
            drop(lock);
            let outcome = run_setup();
            assert_eq!(outcome.status, Status::Ready);
            assert_eq!(runs(), 1, "hooks run once");
        }
    }
}
