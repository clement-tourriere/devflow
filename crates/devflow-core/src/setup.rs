//! The workspace pipeline: materialize → payload → database → env → hooks.
//!
//! Every phase is recorded in the workspace's receipt before its effect
//! (`started`) and after it (`done`/`skipped`/`failed`), so a crashed or
//! interrupted run resumes at the first incomplete phase. A run holds the
//! receipt lock; a second concurrent run fails with the typed [`Busy`] error.
use crate::conn;
use crate::cow::EntryReport;
use crate::db::{BranchInfo, DataSource, RecordStatus, ResolvedSource, Server, SnapshotInfo};
use crate::error::Busy;
use crate::hooks::{self, HookPhase, HookReport};
use crate::project::Project;
use crate::receipt::{DataBinding, Phase, PhaseState, Receipt, Status};
use crate::trust::{self, TrustState};
use crate::worktree::{self, Workspace, protected};
use crate::{cow, env_file};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::path::{Path, PathBuf};

const UNTRUSTED: &str =
    "setup hooks are not trusted; review them with `devflow trust`, then re-run `devflow setup`";

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
    /// Run hooks even if untrusted (the user approved the prompt — callers
    /// should call `trust::approve` for "always"). `DEVFLOW_TRUST_HOOKS=1`
    /// has the same effect without this flag.
    pub trust_hooks: bool,
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
        ensure!(
            !existing.is_default,
            "'{name}' is the default workspace; use `devflow switch {name}`"
        );
        let Some(receipt) = load_receipt(project, existing)? else {
            bail!(
                "workspace '{name}' already exists at {} but devflow has not set it up; \
                 run `devflow setup` inside it",
                existing.path.display()
            );
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
             workspace: unset it, or use `devflow setup --new-instance` for a fresh one",
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
    // A ready receipt was left alone above; everything else needs this run.
    // Skipping it quietly would let a caller believe the workspace is set up.
    let _lock = match acquire(&workspace.git_dir)? {
        Some(lock) => lock,
        None => return Err(busy(&workspace.name)),
    };
    // Another run may have finished while we waited for the lock.
    let existing = load_receipt(project, workspace)?;
    let fork = forks(&existing);
    if if_new && ready(&existing) && !fork {
        return Ok(None);
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
        fork_receipt(&mut receipt, &options, instance);
    }
    receipt.save(&workspace.git_dir)?;
    let mut runner = Runner::new(project, workspace.clone(), receipt, &options, progress);
    runner.run(&Phase::ALL)?;
    Ok(Some(runner.finish()))
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
    match refresh_endpoint(project, workspace, &mut outcome.receipt) {
        Ok(Some(note)) => outcome.warnings.push(note),
        Ok(None) => {}
        Err(e) => outcome
            .warnings
            .push(format!("could not refresh the database URL: {e:#}")),
    }
    match switch_hooks(project, workspace, &outcome.receipt, &options, progress)? {
        SwitchHooks::Ran(report) => {
            if let Some(failure) = report.failure() {
                outcome.warnings.push(failure);
            }
            outcome.hooks = Some(report);
        }
        SwitchHooks::NotRun(Some(why)) => outcome.warnings.push(why),
        SwitchHooks::NotRun(None) => {}
    }
    Ok(outcome)
}

/// Run the project's `switch` hooks in `workspace`, as [`switch`] does after
/// its setup. [`create`] does not run them; a caller that wants a new
/// workspace to behave like a switched-to one calls this after a successful
/// `create`. `None` means nothing ran: there are no switch hooks, `no_hooks`
/// is set, the workspace is not ready, or the hooks are not trusted
/// (`options.trust_hooks`, `DEVFLOW_TRUST_HOOKS=1`, or an approval on record).
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
    /// Did not run; the reason when the user should hear about it.
    NotRun(Option<String>),
}

fn switch_hooks(
    project: &Project,
    workspace: &Workspace,
    receipt: &Receipt,
    options: &SetupOptions,
    progress: &mut dyn Progress,
) -> Result<SwitchHooks> {
    let commands = &project.config.hooks.switch;
    if commands.is_empty() || options.no_hooks {
        return Ok(SwitchHooks::NotRun(None));
    }
    if receipt.status() != Status::Ready {
        return Ok(SwitchHooks::NotRun(Some(
            "switch hooks were not run: the workspace is not ready".into(),
        )));
    }
    if !hooks_trusted(project, options.trust_hooks)? {
        return Ok(SwitchHooks::NotRun(Some(
            "switch hooks were not run: hooks are not trusted; review them with `devflow trust`"
                .into(),
        )));
    }
    run_hooks(
        project,
        workspace,
        receipt,
        HookPhase::Switch,
        commands,
        progress,
    )
    .map(SwitchHooks::Ran)
}

/// Whether hooks may run: the caller approved this run, `DEVFLOW_TRUST_HOOKS=1`
/// is set, or the approval on record covers the current `[hooks]` content.
fn hooks_trusted(project: &Project, trust_hooks: bool) -> Result<bool> {
    Ok(trust_hooks
        || trust::env_override()
        || matches!(trust::state(project)?, TrustState::Trusted))
}

#[derive(Debug, Clone, Default)]
pub struct RemoveOptions {
    /// Allow dirty/locked worktrees and continue past teardown failures.
    pub force: bool,
    pub keep_data: bool,
    pub delete_branch: bool,
    /// Run teardown hooks even if untrusted; `DEVFLOW_TRUST_HOOKS=1` has the
    /// same effect.
    pub trust_hooks: bool,
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

    let teardown = &project.config.hooks.teardown;
    let run_teardown = if teardown.is_empty() {
        false
    } else if workspace.missing {
        warnings.push("teardown hooks were not run: the worktree directory is missing".into());
        false
    } else {
        let trusted = hooks_trusted(project, options.trust_hooks)?;
        if !trusted && !force {
            return Err(protected(
                name,
                "teardown hooks are not trusted; review them with `devflow trust`",
                true,
            ));
        }
        if !trusted {
            warnings.push("teardown hooks were skipped because they are not trusted".into());
        }
        trusted
    };

    let mut drop_plan = None;
    if let Some(receipt) = &receipt
        && let Some(binding) = &receipt.data
        && let (true, Some(id), false) = (binding.owned, binding.branch_id, options.keep_data)
    {
        match connect(project) {
            Ok(Some(server)) => match server.branch(id) {
                Ok(Some(info)) if info.instance_id == Some(receipt.instance_id) => {
                    drop_plan = Some((server, id));
                }
                Ok(Some(_)) => warnings.push(format!(
                    "database branch {id} was created by another instance and was left alone"
                )),
                Ok(None) => warnings.push(format!("database branch {id} no longer exists")),
                Err(e) => bail!(
                    "cannot inspect the database branch of '{}': {e:#}; \
                     pass --keep-data to remove the worktree and leave the branch",
                    workspace.name
                ),
            },
            Ok(None) => warnings.push(
                "the project has no [postgres] section, so the database branch was left alone"
                    .into(),
            ),
            Err(e) => bail!(
                "cannot reach the database server to drop the branch of '{}': {e:#}; \
                 pass --keep-data to remove the worktree and leave the branch",
                workspace.name
            ),
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
    let mut hook_report = None;
    if run_teardown {
        let receipt = receipt
            .clone()
            .unwrap_or_else(|| Receipt::new(project.id(), &workspace.name, None));
        let report = run_hooks(
            project,
            &workspace,
            &receipt,
            HookPhase::Teardown,
            teardown,
            progress,
        )?;
        if let Some(failure) = report.failure() {
            if !force {
                return Err(protected(name, failure, true));
            }
            warnings.push(failure);
        }
        hook_report = Some(report);
    }

    let mut dropped_branch = None;
    if let Some((server, id)) = drop_plan {
        match server.drop_branch(id) {
            Ok(()) => dropped_branch = Some(id),
            Err(e) if force => warnings.push(format!("could not drop database branch {id}: {e:#}")),
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("cannot drop database branch {id}; pass --keep-data to skip it")
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
    Receipt::load(&workspace.git_dir)
        .ok()
        .flatten()
        .filter(|r| r.project_id == project.id())
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
    if let Some(binding) = &receipt.data {
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
    vars
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
    let _lock = lock(workspace)?;
    let receipt = load_receipt(project, workspace)?
        .context("this workspace has no devflow receipt; run `devflow setup` first")?;
    let binding = receipt
        .data
        .as_ref()
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
    let receipt = load_receipt(project, workspace)?
        .context("this workspace has no devflow receipt; run `devflow setup` first")?;
    let id = receipt
        .data
        .as_ref()
        .and_then(|d| d.branch_id)
        .context("this workspace has no database branch to snapshot")?;
    connect_required(project)?.create_snapshot(name, id)
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
    /// Deliberately not run; the workspace is still ready.
    Skipped(String),
    /// Not run, and the workspace needs attention until it is.
    SkippedRequired(String),
    Failed(String),
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
        }
    }

    fn finish(self) -> SetupOutcome {
        SetupOutcome {
            status: self.receipt.status(),
            workspace: self.workspace,
            receipt: self.receipt,
            payload: self.payload,
            hooks: self.hooks,
            warnings: self.warnings,
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
                Phase::Database => self.database(),
                Phase::Env => self.env(),
                Phase::Hooks => self.hooks(),
            };
            match result {
                Ok(PhaseResult::Done(detail)) => self.record(phase, PhaseState::Done, detail)?,
                Ok(PhaseResult::Skipped(detail)) => {
                    self.record(phase, PhaseState::Skipped, Some(detail))?
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
                    return Err(error.context(format!(
                        "{} failed; '{}' is kept at {} (re-run `devflow setup` there to resume)",
                        phase.as_str(),
                        self.workspace.name,
                        self.workspace.path.display()
                    )));
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
        let done = self
            .payload
            .iter()
            .filter(|e| matches!(e.result, cow::EntryResult::Done { .. }))
            .count();
        Ok(PhaseResult::Done(Some(format!(
            "{done} of {} entries copied from {}",
            self.payload.len(),
            source.path.display()
        ))))
    }

    fn database(&mut self) -> Result<PhaseResult> {
        if self.options.no_db {
            return Ok(PhaseResult::Skipped("--no-db".into()));
        }
        // A binding recorded before a crash: keep it if the branch is still there.
        if let Some(binding) = self.receipt.data.clone() {
            match binding.branch_id {
                None => {
                    return Ok(PhaseResult::Done(Some(
                        "attached to the given database URL".into(),
                    )));
                }
                Some(id) => {
                    let server = connect_required(self.project)?;
                    match server.branch(id)? {
                        Some(info) if info.status == RecordStatus::Ready => {
                            return Ok(PhaseResult::Done(Some(format!(
                                "reusing branch {}",
                                info.db_name
                            ))));
                        }
                        _ => {
                            self.warnings.push(format!(
                                "database branch {id} is gone; provisioning a new one"
                            ));
                            self.receipt.data = None;
                        }
                    }
                }
            }
        }
        if let Some(url) = self.options.database_url.clone() {
            let params = attach_params(&url)?;
            self.receipt.data = Some(DataBinding {
                branch_id: None,
                database: params.get("dbname").unwrap_or_default().to_string(),
                role: params.get("user").unwrap_or_default().to_string(),
                url: url.trim().to_string(),
                source: "url".into(),
                owned: false,
            });
            return Ok(PhaseResult::Done(Some(
                "attached to the given database URL".into(),
            )));
        }
        if self.project.config.postgres.is_none() {
            return Ok(PhaseResult::Skipped("no [postgres] section".into()));
        }
        let server = connect_required(self.project)?;
        let name = branch_name(self.project, &self.workspace);
        let instance = self.receipt.instance_id;

        // A previous attempt recorded its source and may have created the
        // branch before crashing; its password was never saved, so take over
        // the branch with fresh credentials instead of creating a second one.
        if self.receipt.data_source.is_some()
            && let Some(existing) = server.branch_for_instance(instance)?
            && existing.name == name
        {
            match server.rotate_credentials(existing.id) {
                Ok(creds) => {
                    let source = self.receipt.data_source.clone().unwrap_or_default();
                    self.receipt.data = Some(binding_from(&creds, source));
                    return Ok(PhaseResult::Done(Some(format!(
                        "recovered branch {} after an interrupted run",
                        creds.database
                    ))));
                }
                Err(e) => self.warnings.push(format!(
                    "an earlier branch for this workspace could not be reused ({e:#}); creating a new one"
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
                        "the recorded data source ({label}) is unavailable; choose another with \
                         `devflow setup --new-instance --data-from SRC`"
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
        let creds = server.create_branch(&name, resolved, Some(instance))?;
        let detail = format!("branch {} from {label}", creds.database);
        self.receipt.data = Some(binding_from(&creds, label));
        Ok(PhaseResult::Done(Some(detail)))
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
            let bound = load_receipt(self.project, &source_ws)?
                .and_then(|r| r.data)
                .is_some_and(|d| d.branch_id.is_some());
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
        let vars = env_vars(self.project, &self.workspace, &self.receipt);
        env_file::write(&self.workspace.path, &vars)?;
        Ok(PhaseResult::Done(Some(format!("{} variables", vars.len()))))
    }

    fn hooks(&mut self) -> Result<PhaseResult> {
        if self.options.no_hooks {
            return Ok(PhaseResult::Skipped("--no-hooks".into()));
        }
        let hooks = &self.project.config.hooks;
        if hooks.setup.is_empty() {
            return Ok(PhaseResult::Done(Some("no setup hooks".into())));
        }
        if !hooks_trusted(self.project, self.options.trust_hooks)? {
            return Ok(PhaseResult::SkippedRequired(UNTRUSTED.into()));
        }
        let report = run_hooks(
            self.project,
            &self.workspace,
            &self.receipt,
            HookPhase::Setup,
            &hooks.setup,
            self.progress,
        )?;
        self.receipt.hooks_digest = Some(trust::digest(hooks));
        let result = match report.failure() {
            Some(failure) => PhaseResult::Failed(failure),
            None => PhaseResult::Done(Some(format!("{} commands", report.runs.len()))),
        };
        self.hooks = Some(report);
        Ok(result)
    }
}

/// Give the default workspace an empty root branch (and env file) so other
/// workspaces can clone it. Returns the branch's database name.
fn init_default(project: &Project, default: &Workspace) -> Result<String> {
    let _lock = acquire(&default.git_dir)?.ok_or_else(|| busy(&default.name))?;
    let mut receipt = match load_receipt(project, default)? {
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
    for (phase, why) in [
        (Phase::Payload, "default workspace"),
        (
            Phase::Hooks,
            "not run when the default workspace is initialized implicitly",
        ),
    ] {
        if !complete(&receipt, phase) {
            receipt.set_phase(phase, PhaseState::Skipped, Some(why.into()));
        }
    }
    for phase in [Phase::Database, Phase::Env] {
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
    runner.run(&[Phase::Database, Phase::Env])?;
    runner
        .receipt
        .data
        .map(|d| d.database)
        .context("the default workspace did not get a database")
}

/// A managed server on an ephemeral port gets a new port after a restart:
/// point the receipt and `.devflow/env` at the current one. Best effort, and
/// never for URL-only attachments, which belong to whoever supplied them.
fn refresh_endpoint(
    project: &Project,
    workspace: &Workspace,
    receipt: &mut Receipt,
) -> Result<Option<String>> {
    use crate::config::PostgresMode;
    let ephemeral = project.config.postgres.as_ref().is_some_and(|pg| {
        pg.mode == PostgresMode::Managed && pg.port == 0 && pg.app_port.is_none()
    });
    let Some(binding) = receipt
        .data
        .as_ref()
        .filter(|b| ephemeral && b.branch_id.is_some())
    else {
        return Ok(None);
    };
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
    if !rebind_url_if_unchanged(&workspace.git_dir, receipt, url)? {
        return Ok(None);
    }
    env_file::write(&workspace.path, &env_vars(project, workspace, receipt))?;
    Ok(Some(format!(
        "the database endpoint changed to {host}:{port}; updated the receipt and .devflow/env"
    )))
}

/// With the receipt lock held: replace the binding's URL, unless another
/// process (a fork, a re-attach) rewrote the receipt since `receipt` was read.
/// Either way `receipt` ends up as what is on disk, so later steps (switch
/// hooks) use the current identity and binding.
fn rebind_url_if_unchanged(git_dir: &Path, receipt: &mut Receipt, url: String) -> Result<bool> {
    let seen = (receipt.instance_id, receipt.data.clone());
    if let Some(current) = Receipt::load(git_dir)? {
        *receipt = current;
    }
    if (receipt.instance_id, receipt.data.clone()) != seen {
        return Ok(false);
    }
    let Some(binding) = receipt.data.as_mut() else {
        return Ok(false);
    };
    binding.url = url;
    receipt.updated_at = chrono::Utc::now();
    receipt.save(git_dir)?;
    Ok(true)
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

/// Name of the branch created for `workspace`.
fn branch_name(project: &Project, workspace: &Workspace) -> String {
    if workspace.is_default {
        project
            .config
            .postgres
            .as_ref()
            .and_then(|pg| pg.database.clone())
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
                .and_then(|r| r.data)
                .and_then(|d| d.branch_id)
                .with_context(|| {
                    format!(
                        "workspace '{name}' has no database branch to clone; \
                         use --data-from empty or --data-from snapshot:NAME"
                    )
                })?;
            ensure_ready(server, id)?;
            (ResolvedSource::Branch(id), encode_source(source))
        }
        DataSource::Branch { id } => {
            ensure_ready(server, *id)?;
            (ResolvedSource::Branch(*id), encode_source(source))
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
fn fork_receipt(receipt: &mut Receipt, options: &SetupOptions, instance: Option<uuid::Uuid>) {
    let old_branch = receipt.data.as_ref().and_then(|d| d.branch_id);
    let old_source = receipt.data_source.take();
    let old_source_id = receipt.data_source_id.take();
    receipt.instance_id = instance
        .filter(|id| *id != receipt.instance_id)
        .unwrap_or_else(uuid::Uuid::new_v4);
    receipt.data = None;
    receipt.hooks_digest = None;
    (receipt.data_source, receipt.data_source_id) = match (&options.data_from, old_branch) {
        (Some(source), _) => (Some(encode_source(source)), None),
        (None, Some(id)) => (Some(format!("branch:{id}")), Some(format!("branch:{id}"))),
        (None, None) => (old_source, old_source_id),
    };
    for phase in [Phase::Database, Phase::Env, Phase::Hooks] {
        receipt.set_phase(phase, PhaseState::Pending, None);
    }
}

/// The receipt of `workspace`, refusing one that belongs to another project.
fn load_receipt(project: &Project, workspace: &Workspace) -> Result<Option<Receipt>> {
    let Some(receipt) = Receipt::load(&workspace.git_dir)? else {
        return Ok(None);
    };
    ensure!(
        receipt.project_id == project.id(),
        "the devflow receipt of '{}' belongs to another project ({}, this one is {}); \
         remove {} to start over",
        workspace.name,
        receipt.project_id,
        project.id(),
        Receipt::path(&workspace.git_dir).display()
    );
    Ok(Some(receipt))
}

fn lock(workspace: &Workspace) -> Result<crate::receipt::ReceiptLock> {
    acquire(&workspace.git_dir)?.ok_or_else(|| busy(&workspace.name))
}

/// Another setup run holds the workspace's receipt lock.
fn busy(workspace: &str) -> anyhow::Error {
    anyhow::Error::new(Busy {
        workspace: workspace.to_string(),
    })
}

/// [`Receipt::try_lock`], retried briefly: a lock is released when the last
/// duplicate of its file descriptor closes, and a process spawned by another
/// thread can hold a duplicate for a few milliseconds.
fn acquire(git_dir: &Path) -> Result<Option<crate::receipt::ReceiptLock>> {
    for attempt in 0..10 {
        if let Some(lock) = Receipt::try_lock(git_dir)? {
            return Ok(Some(lock));
        }
        if attempt < 9 {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
    Ok(None)
}

/// A branch this instance may reset or drop.
fn owned_branch(receipt: &Receipt, binding: &DataBinding) -> Result<uuid::Uuid> {
    let id = binding.branch_id.context(
        "this workspace is attached to a database by URL only; devflow cannot reset or drop it",
    )?;
    ensure!(
        binding.owned,
        "this workspace's database was not created by this instance ({}); \
         run `devflow setup --new-instance` to give it its own branch",
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
    connect(project)?.context("this project has no [postgres] section")
}

fn run_hooks(
    project: &Project,
    workspace: &Workspace,
    receipt: &Receipt,
    phase: HookPhase,
    commands: &[String],
    progress: &mut dyn Progress,
) -> Result<HookReport> {
    hooks::run(
        phase,
        commands,
        &workspace.path,
        &env_vars(project, workspace, receipt),
        project.config.hooks.timeout()?,
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
        let mut original = Receipt::new(uuid::Uuid::new_v4(), "feature", None);
        original.data = Some(crate::receipt::DataBinding {
            branch_id: Some(uuid::Uuid::new_v4()),
            database: "old_db".into(),
            role: "dfl_old".into(),
            url: "postgres://dfl_old:pw@127.0.0.1:1111/old_db".into(), // kingfisher:ignore
            source: "empty".into(),
            owned: true,
        });
        original.save(dir.path()).unwrap();
        // A switch read the receipt; meanwhile a fork re-attached the worktree.
        let mut stale = original.clone();
        let mut forked = original.clone();
        forked.instance_id = uuid::Uuid::new_v4();
        forked.data = Some(crate::receipt::DataBinding {
            branch_id: None,
            database: "new_db".into(),
            role: "app".into(),
            url: "postgres://app:pw@db.internal/new_db".into(), // kingfisher:ignore
            source: "url".into(),
            owned: false,
        });
        forked.save(dir.path()).unwrap();

        let new_url = "postgres://dfl_old:pw@127.0.0.1:2222/old_db".to_string(); // kingfisher:ignore
        assert!(!rebind_url_if_unchanged(dir.path(), &mut stale, new_url.clone()).unwrap());
        assert_eq!(Receipt::load(dir.path()).unwrap().unwrap(), forked);
        assert_eq!(stale, forked, "later steps must see the current receipt");

        // Unchanged since it was read: the URL is rewritten.
        let mut fresh = forked.clone();
        fresh.data.as_mut().unwrap().url = "postgres://app:pw@db.internal:5432/new_db".into(); // kingfisher:ignore
        fresh.save(dir.path()).unwrap();
        let mut read = fresh.clone();
        let url = "postgres://app:pw@db.internal:6000/new_db".to_string(); // kingfisher:ignore
        assert!(rebind_url_if_unchanged(dir.path(), &mut read, url.clone()).unwrap());
        assert_eq!(
            Receipt::load(dir.path())
                .unwrap()
                .unwrap()
                .data
                .unwrap()
                .url,
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
            receipt.data = Some(DataBinding {
                branch_id: None,
                database: "fallback_db".into(),
                role: "fallback_role".into(),
                url: url.into(),
                source: "url".into(),
                owned: false,
            });
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
        receipt.data = Some(DataBinding {
            branch_id: Some(branch),
            database: "df_a_b".into(),
            role: "dfl_b".into(),
            url: "postgres://dfl_b:pw@h:1/df_a_b".into(), // kingfisher:ignore
            source: "workspace:main".into(),
            owned: true,
        });
        receipt.hooks_digest = Some("abc".into());
        for phase in Phase::ALL {
            receipt.set_phase(phase, PhaseState::Done, None);
        }
        let old_instance = receipt.instance_id;

        fork_receipt(&mut receipt, &SetupOptions::default(), None);
        assert_ne!(receipt.instance_id, old_instance);
        assert!(receipt.data.is_none());
        assert!(receipt.hooks_digest.is_none());
        assert_eq!(receipt.data_source, Some(format!("branch:{branch}")));
        assert_eq!(
            receipt.data_source_id,
            Some(format!("branch:{branch}")),
            "the previous branch is pinned by identity"
        );
        assert_eq!(receipt.code_source.as_deref(), Some("main"));
        assert!(!complete(&receipt, Phase::Database));
        assert_eq!(receipt.status(), Status::InProgress);
        assert_eq!(receipt.phase(Phase::Payload).state, PhaseState::Done);
        assert_eq!(receipt.phase(Phase::Env).state, PhaseState::Pending);
        assert_eq!(receipt.phase(Phase::Hooks).state, PhaseState::Pending);

        // --data-from wins; without a binding the recorded source is kept.
        let mut receipt = Receipt::new(project_id, "feature", None);
        let snapshot = uuid::Uuid::new_v4();
        receipt.data_source = Some("snapshot:seed@v1".into());
        receipt.data_source_id = Some(format!("snapshot:{snapshot}"));
        fork_receipt(&mut receipt, &SetupOptions::default(), None);
        assert_eq!(receipt.data_source.as_deref(), Some("snapshot:seed@v1"));
        assert_eq!(
            receipt.data_source_id,
            Some(format!("snapshot:{snapshot}")),
            "a kept source keeps its resolved identity"
        );
        fork_receipt(
            &mut receipt,
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
        fork_receipt(&mut receipt, &SetupOptions::default(), Some(named));
        assert_eq!(receipt.instance_id, named);
        fork_receipt(&mut receipt, &SetupOptions::default(), Some(named));
        assert_ne!(
            receipt.instance_id, named,
            "same id again: fresh random one"
        );
    }

    #[test]
    fn resume_reruns_required_skips_and_failures_only() {
        let mut receipt = Receipt::new(uuid::Uuid::new_v4(), "w", None);
        receipt.set_phase(Phase::Payload, PhaseState::Done, None);
        receipt.set_phase(
            Phase::Database,
            PhaseState::Skipped,
            Some("no [postgres]".into()),
        );
        receipt.skip_required(Phase::Hooks, UNTRUSTED.into());
        receipt.set_phase(Phase::Env, PhaseState::Started, None);
        assert!(complete(&receipt, Phase::Payload));
        assert!(complete(&receipt, Phase::Database));
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
}
