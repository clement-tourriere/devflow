//! Before the first hook of a phase: does a hook that goes through mise get
//! the workspace's own variables, and none of anyone else's?
//!
//! This is the one gate for every phase: setup hooks ([`check`] with
//! [`HookPhase::Setup`]), switch hooks and teardown hooks. A daemon that starts
//! with `mise = true` asks the same question of the same environment through
//! [`check_command_env`], with the environment [`ChildEnv::from_file`] reads
//! from the workspace's `.devflow/env`.
//!
//! Hooks get `.devflow/env`'s variables from devflow directly, but a command
//! that goes through mise (`mise exec`, `mise run`, a shim) has mise's `[env]`
//! applied over them. A `_.file = ".env"` listed after `.devflow/env`, a
//! `mise.local.toml`, a plain `[env]` key, an environment-specific file or the
//! global configuration then replaces the workspace's `DATABASE_URL` with the
//! main checkout's, and a postinstall script or a migration runs against the
//! wrong database.
//!
//! What a hook receives is decided by the command, so the gate reads the
//! commands, not the files that happen to exist:
//!
//! - a hook that runs `devflow exec` inside mise (`mise exec -- devflow exec
//!   -- CMD`) needs no check of the variables: devflow sets them after mise;
//! - for any other hook that runs mise, the gate asks mise what the hook's
//!   process would see (`env_file::check_command_env`): the same environment
//!   the hook gets, every name the workspace owns read back from the process
//!   mise starts. A variable that differs was overridden, one that is gone was
//!   removed, and a name the workspace owns and sets no value for (a `--no-db`
//!   workspace's `DATABASE_URL`) that is there was reintroduced;
//! - for every hook that runs mise, whether mise trusts the worktree's
//!   configuration (`env_file::check_mise_trust`), before anything runs it:
//!   `mise exec` trusts what it runs when it has no terminal, so devflow asks
//!   with `mise env`, which refuses and trusts nothing;
//! - a hook the check cannot stand for (a `cd` ahead of mise, a mise task,
//!   `mise -C`) is named in a warning that recommends `devflow exec`, and is
//!   not blocked.
use crate::env_file::{
    DEFAULT_MISE_TIMEOUT, HookMise, MiseCommand, MiseDelivery, check_command_env, check_mise_trust,
    classify_hooks,
};
use crate::env_owner::ChildEnv;
use crate::hooks::{HookCommand, HookPhase};
use crate::project::Project;
use crate::receipt::Receipt;
use crate::remedy::Remedy;
use crate::worktree::Workspace;
use anyhow::Result;
use std::path::Path;

/// What the check decided about running the hooks of a phase.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Verdict {
    /// Why no hook may run; the phase needs attention until it is resolved.
    pub block: Option<String>,
    /// What fixes the block when it is not the project's configuration (an
    /// unsupported mise: the page to upgrade it); empty otherwise.
    pub remedies: Vec<Remedy>,
    /// Things worth saying that do not stop the hooks.
    pub warnings: Vec<String>,
    /// The check was skipped on request (`--skip-mise-check`).
    pub skipped: bool,
}

/// What the person does next when no hook ran, by phase.
fn then(phase: HookPhase) -> &'static str {
    match phase {
        HookPhase::Setup => "run setup again",
        HookPhase::Switch => "switch again",
        HookPhase::Teardown => "remove the workspace again",
    }
}

/// What else the person can do when the check says no hook may run.
fn instead(phase: HookPhase) -> &'static str {
    match phase {
        HookPhase::Setup => ", or ask setup to skip this check",
        HookPhase::Switch => "",
        HookPhase::Teardown => ", or force the removal, which leaves the teardown hooks out",
    }
}

/// Whether a reason carries the guidance for the person (what to run, what to
/// route through `devflow exec`). Only the setup phase's does, as it always
/// has: its reason is recorded in the receipt and shown by the CLI. The
/// switch and teardown reasons reach warnings, errors and the app, which do
/// not show command lines: they say what is wrong, and the way on is a typed
/// remedy ([`check_phase`]).
fn guided(phase: HookPhase) -> bool {
    phase == HookPhase::Setup
}

fn names(hooks: &[&HookMise]) -> String {
    hooks
        .iter()
        .map(|hook| hook.label.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}
/// Check the worktree's mise before the `commands` of `phase` run with `env`
/// (the environment devflow gives them, built for that phase).
///
/// Only hooks that run mise are checked, and they are checked whether or not
/// the worktree still has its `.devflow/env`: what a hook gets is `env`, built
/// from the receipt, never the file (a removal after generated files were
/// cleaned still runs approved teardown hooks with it). `skip` records that
/// the person asked not to (`--skip-mise-check`).
///
/// - `Delivered`: the hooks run.
/// - `Overridden`, `Removed`, `Untrusted`, `Unsupported`, `Failed`: no hook
///   runs.
/// - `Unavailable`: no hook runs, since one of them needs mise.
///
/// Whether mise trusts the configuration is asked for every hook that runs
/// mise, `devflow exec` inside it or not: `mise exec` would trust what it
/// runs on its own, and devflow does not let it. What the hook would receive
/// is only asked for the hooks that `devflow exec` does not cover and that a
/// check at the worktree root can stand for.
pub fn check(
    workspace: &Path,
    phase: HookPhase,
    commands: &[HookCommand],
    env: &ChildEnv,
    skip: bool,
    mise: &MiseCommand,
) -> Verdict {
    let mut verdict = Verdict::default();
    let mut hooks = classify_hooks(commands);
    // (The classification names hooks as setup hooks.)
    for hook in &mut hooks {
        hook.label = hook
            .label
            .replacen("setup hook", &format!("{} hook", phase.as_str()), 1);
    }
    let using: Vec<&HookMise> = hooks.iter().filter(|hook| hook.uses_mise).collect();
    if using.is_empty() {
        // No hook runs mise.
        return verdict;
    }
    if skip {
        verdict.skipped = true;
        verdict.warnings.push(format!(
            "the mise check was skipped on request: {} hooks ran without checking that mise trusts \
             the worktree and leaves them the workspace's variables{}",
            phase.as_str(),
            if phase == HookPhase::Setup {
                " (recorded in the receipt)"
            } else {
                ""
            }
        ));
        return verdict;
    }
    for hook in &hooks {
        if let Some(reason) = hook.unchecked {
            verdict.warnings.push(format!(
                "{} {}: the check of what mise does to the workspace's variables cannot stand \
                 for it, so it was not blocked{}",
                hook.label,
                reason.describe(),
                if guided(phase) {
                    ". Run its command through `devflow exec` \
                     (`mise exec -- devflow exec -- COMMAND`) and the variables win whatever \
                     mise sets"
                } else {
                    ""
                }
            ));
        }
    }
    let checked: Vec<&HookMise> = hooks.iter().filter(|hook| hook.checked).collect();
    let delivery = if checked.is_empty() {
        check_mise_trust(workspace, env, phase, DEFAULT_MISE_TIMEOUT, mise)
    } else {
        check_command_env(workspace, env, phase, DEFAULT_MISE_TIMEOUT, mise)
    };
    let skip_hint = if guided(phase) {
        format!(
            " No hook ran. Route the command through `devflow exec` \
             (`mise exec -- devflow exec -- COMMAND`) or fix mise's configuration, then {}{}",
            then(phase),
            instead(phase)
        )
    } else {
        String::new()
    };
    match &delivery {
        MiseDelivery::Delivered | MiseDelivery::NotLoaded => {}
        MiseDelivery::Overridden { .. }
        | MiseDelivery::Removed { .. }
        | MiseDelivery::Reintroduced { .. } => {
            verdict.block = Some(format!(
                "{} would not get the workspace's variables as devflow sets them: {}.{skip_hint}",
                names(&checked),
                delivery.describe()
            ));
        }
        // Hooks that only run `devflow exec` inside mise do not depend on
        // what mise could read; a check that fails says nothing about them.
        MiseDelivery::Failed { .. } if checked.is_empty() => {}
        MiseDelivery::Failed { .. } => {
            verdict.block = Some(format!("{}.{skip_hint}", delivery.describe()));
        }
        MiseDelivery::Untrusted { .. } => {
            // devflow never trusts a configuration on its own.
            verdict.block = Some(if guided(phase) {
                format!(
                    "{}. No hook ran; {} once mise trusts it (the doctor checks list the ways \
                     to do that)",
                    delivery.describe(),
                    then(phase)
                )
            } else if let MiseDelivery::Untrusted { path } = &delivery {
                // (`describe` tells the person which command trusts it.)
                format!(
                    "mise does not trust {}, and devflow never trusts a configuration for the \
                     person",
                    path.display()
                )
            } else {
                delivery.describe()
            });
        }
        MiseDelivery::Unsupported { problem } => {
            verdict.block = Some(if guided(phase) {
                format!(
                    "{}. No hook ran; upgrade mise, then {}{}",
                    problem.message(),
                    then(phase),
                    instead(phase)
                )
            } else {
                problem.message()
            });
            verdict.remedies = problem.remedies();
        }
        MiseDelivery::Unavailable => {
            verdict.block = Some(if guided(phase) {
                format!(
                    "mise is not installed or not on PATH, and {} runs it. No hook ran; \
                     install mise (https://mise.jdx.dev), then {}{}",
                    using[0].label,
                    then(phase),
                    instead(phase)
                )
            } else {
                format!(
                    "mise is not installed or not on PATH, and {} runs it",
                    using[0].label
                )
            });
        }
    }
    verdict
}

/// [`check`] for the hooks of `phase` that run in `workspace`, at the places
/// other than the setup phase (switch and teardown hooks): with the
/// environment those hooks get, and the remedies that apply when no hook may
/// run.
///
/// The remedy is the project's configuration, where the hook is written: the
/// fix is a hook routed through `devflow exec`, or a mise configuration that
/// leaves the workspace's variables alone, and devflow trusts nothing for the
/// person.
pub(super) fn check_phase(
    project: &Project,
    workspace: &Workspace,
    receipt: &Receipt,
    phase: HookPhase,
    skip: bool,
) -> Result<(Verdict, Vec<Remedy>)> {
    // The environment the hooks will get: built from the receipt, as the
    // hooks are started with it. One that names a workspace that cannot be
    // resolved is a block like any other (hooks started with it would be
    // refused too), with its own remedy.
    let project = &*project.at(&workspace.path);
    let env = match super::workspace_env(project, workspace, receipt, true).child() {
        Ok(env) => env,
        Err(stale) => {
            let verdict = Verdict {
                block: Some(stale.to_string()),
                ..Verdict::default()
            };
            return Ok((verdict, vec![Remedy::ClearInherited]));
        }
    };
    let verdict = check(
        &workspace.path,
        phase,
        &project.config.hooks.commands(phase)?,
        &env,
        skip,
        &MiseCommand::default(),
    );
    let remedies = if verdict.block.is_none() {
        Vec::new()
    } else if verdict.remedies.is_empty() {
        vec![Remedy::OpenFile {
            path: project.config_path.clone(),
        }]
    } else {
        // Not the project's configuration that is at fault: the tool is.
        verdict.remedies.clone()
    };
    Ok((verdict, remedies))
}

#[cfg(all(test, unix))]
mod tests;
