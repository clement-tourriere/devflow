//! Activation: entering a workspace.
//!
//! [`activate`] (an existing workspace) and [`create_and_activate`] (a new one)
//! run the same steps in the same order, whoever asks, so the command line and
//! the desktop app cannot drift apart:
//!
//! 1. set up, or resume an unfinished setup;
//! 2. repair what is derived from the receipt and has gone missing
//!    ([`Repair`]);
//! 3. refresh the database endpoint a restarted server moved;
//! 4. start the container services ([`ServiceState`]);
//! 5. run the `switch` hooks;
//! 6. start the daemons `[pitchfork] autostart` asks for.
//!
//! Steps 1 to 5 run under both of the workspace's locks, the process lock
//! first and the receipt lock second (the lock order in [`crate::receipt`]),
//! so nothing else sets the workspace up, changes its receipt or starts its
//! daemons while its switch hooks run. Before step 6 the receipt lock is
//! released (a start records its intent in the receipt under it) and the
//! process lock is kept and handed to the start. A devflow that a hook started
//! ([`crate::receipt::started_by_a_hook`]) tries both locks once and gets
//! [`Busy`](crate::error::Busy) instead of waiting for its parent.
//!
//! Setup's status is separate from all this: a ready workspace whose service
//! could not start is still `ready` (and exits 0); the outcome says which
//! service failed and why.
use super::{
    CreateRequest, Held, Phase, PhaseState, Progress, Run, Runner, SetupOptions, SetupOutcome,
    Status, SwitchHooks, complete, create_held, recipes_verdict, refresh_endpoint, setup_held,
    switch_hooks,
};
use crate::config::Engine;
use crate::pitchfork::Pitchfork;
use crate::processes::{self, NamespaceOutcome};
use crate::project::Project;
use crate::receipt::{Receipt, ReceiptLock};
use crate::remedy::Remedy;
use crate::services::{self, Service, catalog::BranchRecord, docker};
use crate::worktree::Workspace;
use anyhow::{Context, Result};
use serde::Serialize;

/// Enter an existing workspace: the steps in the module documentation. A
/// workspace devflow has not set up yet is adopted. Switch hooks only run once
/// the workspace is ready; an untrusted or failing switch hook is a warning.
///
/// Like [`setup`](super::setup) this is for the workspace the caller named, and
/// it never honours `DEVFLOW_INSTANCE_ID`.
pub fn activate(
    project: &Project,
    workspace: &Workspace,
    options: SetupOptions,
    progress: &mut dyn Progress,
) -> Result<SetupOutcome> {
    let held = setup_held(
        project,
        workspace,
        options.clone(),
        Run::activation(),
        &mut || {},
        progress,
    )?
    .context("setup did not run")?;
    finish(project, held, &options, progress)
}

/// Create a workspace and enter it: the first entry is a switch too. A
/// workspace of that name whose setup did not finish is resumed instead.
/// Entering it runs the steps of [`activate`] under the one receipt lock that
/// creation took.
pub fn create_and_activate(
    project: &Project,
    request: CreateRequest,
    progress: &mut dyn Progress,
) -> Result<SetupOutcome> {
    let options = request.options.clone();
    let held = create_held(project, request, progress, true)?;
    finish(project, held, &options, progress)
}

/// Steps 2 (already done by setup, under the same lock) to 6, for a workspace
/// whose setup just ran.
fn finish(
    project: &Project,
    held: Held,
    options: &SetupOptions,
    progress: &mut dyn Progress,
) -> Result<SetupOutcome> {
    let Held {
        mut outcome,
        lock,
        process,
        fresh,
    } = held;
    let workspace = outcome.workspace.clone();

    // When the workspace was last entered: recorded first, so a crash later
    // in the activation still leaves it.
    outcome.receipt.last_active_at = Some(chrono::Utc::now());
    outcome.receipt.save(&workspace.git_dir)?;

    match refresh_endpoint(
        project,
        &workspace,
        &mut outcome.receipt,
        recipes_verdict(project, &workspace.path, options.approval.as_ref()).approved(),
        &lock,
    ) {
        Ok(Some(note)) => outcome.warnings.push(note),
        Ok(None) => {}
        Err(e) => outcome
            .warnings
            .push(format!("could not refresh the database URL: {e:#}")),
    }

    if !options.no_db {
        start_containers(project, &mut outcome, &fresh);
    }

    match switch_hooks(project, &workspace, &outcome.receipt, options, progress)? {
        SwitchHooks::Ran(report, warnings) => {
            outcome.warnings.extend(warnings);
            if let Some(failure) = report.failure() {
                outcome.warnings.push(failure);
            }
            outcome.switch_hooks = Some(report);
        }
        SwitchHooks::NotRun(Some((why, remedies))) => {
            outcome.warnings.push(why);
            outcome.remedies.extend(remedies);
        }
        SwitchHooks::NotRun(None) => {}
    }
    // The receipt lock goes (a start records its intent in the receipt under
    // it); the process lock stays, and is the start's.
    drop(lock);

    let pitchfork = options.pitchfork.clone().or_else(Pitchfork::locate);
    match &process {
        Some(process) if outcome.status == Status::Ready => {
            outcome.autostart = processes::autostart_locked(
                pitchfork.as_ref(),
                project,
                &outcome.workspace,
                &processes::StartOptions::default(),
                process,
            );
        }
        Some(_) => {}
        None => autostart(project, &mut outcome, pitchfork.as_ref()),
    }
    Ok(outcome)
}

/// `[pitchfork] autostart`: start the daemons of a workspace that is `ready`,
/// and record what happened in [`SetupOutcome::autostart`]. Nothing when the
/// project does not ask for it. Process state is reported apart from
/// readiness: a start that fails or is refused never changes the workspace's
/// status or a command's exit code. Entering a workspace does this as its last
/// step; `setup` leaves it to its caller, which passes the pitchfork it uses.
pub fn autostart(project: &Project, outcome: &mut SetupOutcome, pitchfork: Option<&Pitchfork>) {
    if outcome.status != Status::Ready {
        return;
    }
    outcome.autostart = processes::autostart(
        pitchfork,
        project,
        &outcome.workspace,
        processes::DEFAULT_START_TIMEOUT,
    );
}

// ---------------------------------------------------------------------------
// What an activation reports

/// A workspace whose checkout now holds another branch than the one it was
/// created for. The workspace is its receipt (which names the data it owns),
/// not the branch: the data, the services and the variables still belong to
/// `workspace`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BranchDrift {
    /// The name the workspace was created under, which its data belongs to.
    pub workspace: String,
    /// The branch the checkout holds now; `None` for a detached HEAD.
    pub branch: Option<String>,
    /// Short commit id of HEAD, for a detached HEAD.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
}

impl BranchDrift {
    /// Whether `workspace` (as [`crate::worktree::list`] reads the checkout)
    /// holds another branch than the one `receipt` was created for.
    pub fn of(workspace: &Workspace, receipt: &Receipt) -> Option<Self> {
        let now = workspace.branch.as_deref();
        if now == Some(receipt.workspace.as_str()) {
            return None;
        }
        // Adopted while detached: it was named after its directory, and a
        // detached HEAD is what it holds. Recorded when it was adopted, never
        // guessed from a name: a workspace whose name is also its directory's
        // that detaches its HEAD has moved.
        if now.is_none() && receipt.adopted_detached {
            return None;
        }
        Some(Self {
            workspace: receipt.workspace.clone(),
            branch: now.map(str::to_string),
            head: workspace.head.clone(),
        })
    }

    /// In words: "now on branch X; its data still belongs to workspace Y".
    pub fn describe(&self) -> String {
        let now = match (&self.branch, &self.head) {
            (Some(branch), _) => format!("branch {branch}"),
            (None, Some(head)) => format!("a detached HEAD at {head}"),
            (None, None) => "a detached HEAD".to_string(),
        };
        format!(
            "now on {now}; its data still belongs to workspace {}",
            self.workspace
        )
    }
}

/// What activation or setup rebuilt because a file derived from the receipt
/// had gone missing (a `git clean -fdx`, a deleted directory). Nothing is
/// provisioned, rotated or run again for a repair: the files are written from
/// the receipt's own bindings, identity and the approved recipes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Repair {
    /// `.devflow/env` was missing or unreadable.
    EnvFile,
    /// devflow's own `pitchfork.local.toml` (the daemons' namespace) was missing.
    PitchforkNamespace,
    /// The workspace was set up before it had a port of its own: it was given
    /// one and `.devflow/env` carries it as `DEVFLOW_PORT` now.
    AppPort,
}

impl Repair {
    /// In words, for a person.
    pub fn describe(self) -> &'static str {
        match self {
            Repair::EnvFile => {
                "rebuilt .devflow/env from the workspace's record: no data was created, no hook \
                 ran again, and the recorded credentials were written as they are, not checked \
                 against the server"
            }
            Repair::PitchforkNamespace => "wrote pitchfork.local.toml again",
            Repair::AppPort => {
                "gave the workspace its own port and wrote it to .devflow/env as DEVFLOW_PORT"
            }
        }
    }
}

/// How one container service stands after the workspace was entered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServiceState {
    pub key: String,
    pub engine: Engine,
    pub state: ServiceRun,
    /// Why a service failed to start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceRun {
    /// Its container was already up.
    Running,
    /// devflow started it (or created it) in this run.
    Started,
    /// It could not be started; the reason is in [`ServiceState::reason`].
    Failed,
}

impl SetupOutcome {
    /// What activation or setup did beyond the phases, in words: the branch
    /// the checkout holds when it is not the one the workspace was created
    /// for, and what was repaired. For a frontend to show next to the status;
    /// the structured fields hold the same facts.
    pub fn notes(&self) -> Vec<String> {
        let mut notes: Vec<String> = self
            .repaired
            .iter()
            .map(|repair| repair.describe().to_string())
            .collect();
        if let Some(drift) = &self.drift {
            notes.push(format!("{}: {}", self.workspace.name, drift.describe()));
        }
        notes
    }
}

// ---------------------------------------------------------------------------
// Repair

/// What is wrong with `.devflow/env` of a workspace that was set up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvFileProblem {
    /// There is no file (a `git clean -fdx`, a deleted directory).
    Missing,
    /// The file is there and cannot be read as text (not UTF-8, no
    /// permission, a directory, a link to nothing).
    Unreadable,
    /// The file was written by an earlier devflow and does not list the names
    /// the workspace owns ([`crate::env_file::Contents::lacks_manifest`]): nothing that
    /// needs them runs from it until it is written again.
    NoManifest,
    /// A `--no-db` workspace whose file was stamped by an earlier devflow with
    /// a generation that did not cover the names the configuration declares
    /// ([`Receipt::env_generation_is_legacy`]): it cannot tell that PostgreSQL
    /// came, or that a service exports another variable, so the names the file
    /// lists may be stale even though it has a manifest.
    OldGeneration,
}

impl EnvFileProblem {
    /// In words, for a list or a banner: it names the file, not a command.
    pub fn describe(self) -> &'static str {
        match self {
            EnvFileProblem::Missing => {
                ".devflow/env is missing; entering the workspace or running setup writes it again"
            }
            EnvFileProblem::Unreadable => {
                ".devflow/env cannot be read; entering the workspace or running setup writes it \
                 again"
            }
            EnvFileProblem::NoManifest => {
                ".devflow/env was written by an earlier devflow and does not list the names the \
                 workspace owns; entering the workspace or running setup writes it again"
            }
            EnvFileProblem::OldGeneration => {
                ".devflow/env was written by an earlier devflow that could not see which names a \
                 service gives this workspace; entering the workspace or running setup writes it \
                 again"
            }
        }
    }
}

/// What is wrong with the file itself: missing, unreadable, or written by an
/// earlier devflow without the list of the names the workspace owns (the
/// manifest `devflow exec` and the hooks need).
pub(super) fn env_file_problem(workspace: &Workspace) -> Option<EnvFileProblem> {
    match std::fs::read_to_string(crate::env_file::path(&workspace.path)) {
        // The one predicate for "written without the manifest": the contents'.
        Ok(text) if crate::env_file::parse(&text).lacks_manifest() => {
            Some(EnvFileProblem::NoManifest)
        }
        Ok(_) => None,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(EnvFileProblem::Missing),
        Err(_) => Some(EnvFileProblem::Unreadable),
    }
}

/// Whether `.devflow/env` of `workspace` has to be written again, and why: the
/// file has a problem ([`env_file_problem`]), or the receipt recorded it under
/// a generation of an earlier devflow that could not see the names a service
/// gives a `--no-db` workspace ([`Receipt::env_generation_is_legacy`]), which
/// a manifest that exists does not cure. Everything that asks goes through
/// this one function: the shortcuts of `setup --if-new`, the list's note, and
/// the repair itself.
pub(super) fn env_problem(
    workspace: &Workspace,
    receipt: &Receipt,
    config: &crate::Config,
) -> Option<EnvFileProblem> {
    env_file_problem(workspace).or_else(|| {
        receipt
            .env_generation_is_legacy(config)
            .then_some(EnvFileProblem::OldGeneration)
    })
}

/// The project uses pitchfork in this workspace and devflow's own
/// `pitchfork.local.toml` is not there.
fn namespace_missing(workspace: &Workspace) -> bool {
    processes::uses_pitchfork(&workspace.path)
        && workspace
            .path
            .join(processes::LOCAL_FILE)
            .symlink_metadata()
            .is_err()
}

/// A workspace whose env phase finished has files derived from its receipt,
/// and they can go missing. Whether `receipt` (read the way the next setup
/// reads it: migrated, and with what the configuration invalidates reopened)
/// is such a workspace and one of them is gone.
///
/// A workspace set up before ports existed has no `DEVFLOW_PORT` yet. Giving
/// it one needs the env phase again, which only completes when the recipes are
/// approved: `port_ok` says whether they are (asked only when it matters), so a
/// run nobody can approve for never turns a ready workspace into one that
/// needs attention for a port.
pub(super) fn repair_needed(
    workspace: &Workspace,
    receipt: &Receipt,
    config: &crate::Config,
    port_ok: &dyn Fn() -> bool,
) -> bool {
    complete(receipt, Phase::Env)
        && (env_problem(workspace, receipt, config).is_some()
            || namespace_missing(workspace)
            || (receipt.app_port.is_none() && port_ok()))
}

/// What a run repairs, decided from the receipt before anything is written.
///
/// A repair is not a setup: it brings back a derived file from what the
/// receipt already records and nothing else.
///
/// - Nothing is provisioned, and the data is not touched; the credentials in
///   the receipt are written as they are. They are neither claimed to be valid
///   (an external rotation or a dropped role makes them stale, and this does
///   not look) nor rotated.
/// - No hook runs again: `services` and `hooks` stay as they are. That holds
///   for a lost file only. A real change (a service added or changed, a lost
///   branch, an `[env]` edit) has already reopened the phases it concerns when
///   the receipt was read, hooks included, and they run.
/// - The variables are the env phase's own: the recipes only if the current
///   scope is approved, the services a `--no-db` skip left out stay out, and a
///   moved managed endpoint is followed before the file is written
///   ([`follow_endpoint`]). The file is written by the phase's writer
///   (`WorkspaceEnv::write`), so it carries the owned-names manifest, and the
///   names it exports are added to the receipt's history
///   (`Receipt::record_exported`), as for any env phase. That also repairs a
///   file an earlier devflow wrote without the manifest, before the switch
///   hooks (which refuse such a file) run. A `--no-db` workspace whose receipt
///   still holds the generation recorded before the declared names were part
///   of it is written again the same way even when its file has a manifest,
///   and stamped with the current generation, so a service that comes later
///   reopens the environment as it should.
#[derive(Debug, Default)]
pub(super) struct RepairPlan {
    /// `.devflow/env` is written again, by running the env phase.
    pub env: bool,
    /// The workspace had no port of its own when the run began (it was set up
    /// before there was one): the env phase gives it one and writes it.
    port_missing: bool,
    /// devflow's `pitchfork.local.toml` was not there when the run began.
    namespace_missing: bool,
}

impl RepairPlan {
    /// Decide, and reopen the env phase in `receipt` when its file is gone
    /// (the caller saves the receipt before the phase runs).
    ///
    /// `port_ok`: the recipes are approved, so the env phase can complete and a
    /// workspace without a port can be given one (see [`repair_needed`]).
    pub fn reopen(
        workspace: &Workspace,
        receipt: &mut Receipt,
        config: &crate::Config,
        port_ok: bool,
    ) -> Self {
        if !complete(receipt, Phase::Env) {
            // Not finished: the phases run anyway, and writing the files is
            // setup, not repair.
            return Self::default();
        }
        let env = env_problem(workspace, receipt, config).is_some();
        let port_missing = receipt.app_port.is_none() && port_ok;
        if env || port_missing {
            // (Only the env phase: hooks and services stay as they are.)
            receipt.set_phase(Phase::Env, PhaseState::Pending, None);
        }
        Self {
            env,
            port_missing,
            namespace_missing: namespace_missing(workspace),
        }
    }

    /// After the run: write the namespace file if it was lost (the env phase
    /// writes it when it runs; the check is idempotent), and record what was
    /// repaired in the outcome.
    pub fn finish(self, runner: &mut Runner<'_>) {
        if self.env {
            runner.repaired.push(Repair::EnvFile);
        }
        if self.port_missing && runner.receipt.app_port.is_some() {
            runner.repaired.push(Repair::AppPort);
        }
        if self.namespace_missing {
            let outcome =
                processes::ensure_namespace(runner.project, &runner.workspace, &runner.receipt);
            if matches!(
                outcome,
                NamespaceOutcome::Written { .. } | NamespaceOutcome::Current { .. }
            ) {
                runner.repaired.push(Repair::PitchforkNamespace);
            }
            if let Some(warning) = outcome.warning()
                && !runner.warnings.contains(&warning)
            {
                runner.warnings.push(warning);
            }
        }
    }
}

/// Follow a managed server that came back on another port before a lost
/// `.devflow/env` is written from the receipt's URL: the receipt is updated
/// first, so the file is written with the endpoint that works now. Best
/// effort, like the refresh that ends an entry. The caller holds the receipt
/// lock and has saved the receipt.
pub(super) fn follow_endpoint(
    project: &Project,
    workspace: &Workspace,
    receipt: &mut Receipt,
    approval: Option<&crate::trust::Approval>,
    lock: &ReceiptLock,
) -> Vec<String> {
    let recipes = recipes_verdict(project, &workspace.path, approval).approved();
    match refresh_endpoint(project, workspace, receipt, recipes, lock) {
        Ok(note) => note.into_iter().collect(),
        Err(e) => vec![format!("could not refresh the database URL: {e:#}")],
    }
}

// ---------------------------------------------------------------------------
// Container services

/// Start the containers of the workspace's container services. A workspace
/// that was already `ready` skips the services phase, but its containers may
/// have stopped (a Docker restart, `devflow service down`): they start on
/// activation, not when something connects. A container that cannot start
/// (its port is taken) is a warning with a way to start it again, and the
/// service reads `failed`: the switch itself happened.
fn start_containers(project: &Project, outcome: &mut SetupOutcome, fresh: &[String]) {
    for (key, engine, _) in project.config.containers() {
        let Some(id) = services::workspace::binding(&outcome.receipt, key, engine)
            .filter(|binding| !binding.orphaned)
            .and_then(|binding| binding.branch_id)
        else {
            continue;
        };
        let opened = Service::open(project, key);
        let was_running = opened
            .as_ref()
            .ok()
            .and_then(|service| service.branch(id).ok().flatten())
            .and_then(|record| container_running(&record));
        let started = opened.and_then(|service| service.up(id));
        let created_here = fresh.iter().any(|created| created == key);
        let state = service_state(key, engine, was_running, created_here, started.map(|_| ()));
        if state.state == ServiceRun::Failed {
            outcome.warnings.push(format!(
                "could not start service '{key}': {}",
                state.reason.as_deref().unwrap_or("unknown reason")
            ));
            outcome.remedies.push(Remedy::StartService {
                root: project.root.clone(),
                key: key.to_string(),
                branch: Some(id),
                workspace: Some(outcome.workspace.path.clone()),
            });
        }
        outcome.services_state.push(state);
    }
}

/// Whether Docker says the branch's container is running; `None` when it
/// cannot be told (no container recorded, Docker unreachable).
pub(super) fn container_running(record: &BranchRecord) -> Option<bool> {
    let reference = record
        .container_id
        .as_deref()
        .unwrap_or(record.container_name.as_str());
    docker::inspect_container(reference)
        .ok()
        .map(|container| container.is_some_and(|c| c.running))
}

/// The state to report for a service: `failed` with the reason when the start
/// did not work; otherwise `started` when this run created it or found it not
/// running, and `running` when it was already up.
fn service_state(
    key: &str,
    engine: Engine,
    was_running: Option<bool>,
    created_here: bool,
    started: Result<()>,
) -> ServiceState {
    let (state, reason) = match started {
        Err(error) => (ServiceRun::Failed, Some(format!("{error:#}"))),
        Ok(()) if created_here || was_running == Some(false) => (ServiceRun::Started, None),
        Ok(()) => (ServiceRun::Running, None),
    };
    ServiceState {
        key: key.to_string(),
        engine,
        state,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(branch: Option<&str>, dir: &str) -> Workspace {
        Workspace {
            name: branch
                .map(str::to_string)
                .unwrap_or_else(|| dir.to_string()),
            branch: branch.map(str::to_string),
            path: std::path::PathBuf::from("/work").join(dir),
            git_dir: std::path::PathBuf::from("/work").join(dir).join(".git"),
            is_default: false,
            locked: false,
            missing: false,
            head: Some("abc1234".into()),
            fingerprint: None,
        }
    }

    fn receipt(name: &str) -> Receipt {
        Receipt::new(uuid::Uuid::new_v4(), name, None)
    }

    #[test]
    fn a_checkout_on_its_own_branch_has_not_drifted() {
        assert_eq!(
            BranchDrift::of(
                &workspace(Some("feature/a"), "feature-a"),
                &receipt("feature/a")
            ),
            None
        );
    }

    #[test]
    fn another_branch_is_drift_and_says_whose_data_it_is() {
        let drift = BranchDrift::of(
            &workspace(Some("other"), "feature-a"),
            &receipt("feature/a"),
        )
        .expect("drift");
        assert_eq!(drift.workspace, "feature/a");
        assert_eq!(drift.branch.as_deref(), Some("other"));
        assert_eq!(
            drift.describe(),
            "now on branch other; its data still belongs to workspace feature/a"
        );
    }

    #[test]
    fn a_detached_head_is_drift_unless_the_workspace_was_adopted_detached() {
        let drift =
            BranchDrift::of(&workspace(None, "feature-a"), &receipt("feature/a")).expect("drift");
        assert_eq!(
            drift.describe(),
            "now on a detached HEAD at abc1234; its data still belongs to workspace feature/a"
        );
        // A workspace named like its directory has moved all the same when it
        // detaches: only an adoption while detached, recorded, is meant to be.
        assert!(BranchDrift::of(&workspace(None, "scratch"), &receipt("scratch")).is_some());
        let mut adopted = receipt("scratch");
        adopted.adopted_detached = true;
        assert_eq!(BranchDrift::of(&workspace(None, "scratch"), &adopted), None);
        // And an adopted one that later holds a branch has moved too.
        assert!(BranchDrift::of(&workspace(Some("topic"), "scratch"), &adopted).is_some());
    }

    #[test]
    fn a_failed_start_has_a_reason_and_a_running_service_is_not_called_started() {
        let key = "cache";
        let failed = service_state(
            key,
            Engine::Valkey,
            Some(false),
            false,
            Err(anyhow::anyhow!("port taken")),
        );
        assert_eq!(failed.state, ServiceRun::Failed);
        assert_eq!(failed.reason.as_deref(), Some("port taken"));

        let up = service_state(key, Engine::Valkey, Some(true), false, Ok(()));
        assert_eq!((up.state, up.reason), (ServiceRun::Running, None));

        let started = service_state(key, Engine::Valkey, Some(false), false, Ok(()));
        assert_eq!(started.state, ServiceRun::Started);

        // Created by this very run: it was started, whatever Docker says now.
        let created = service_state(key, Engine::Valkey, Some(true), true, Ok(()));
        assert_eq!(created.state, ServiceRun::Started);

        // Docker could not say: devflow only reports what it knows it did.
        let unknown = service_state(key, Engine::Valkey, None, false, Ok(()));
        assert_eq!(unknown.state, ServiceRun::Running);
    }
}
