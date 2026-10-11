//! Before the first setup hook: does a hook that goes through mise get the
//! workspace's own variables?
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
//!   the hook gets, the variables read back from the process mise starts. A
//!   variable that differs was overridden, one that is gone was removed;
//! - for every hook that runs mise, whether mise trusts the worktree's
//!   configuration (`env_file::check_mise_trust`), before anything runs it:
//!   `mise exec` trusts what it runs when it has no terminal, so devflow asks
//!   with `mise env`, which refuses and trusts nothing;
//! - a hook the check cannot stand for (a `cd` ahead of mise, a mise task,
//!   `mise -C`) is named in a warning that recommends `devflow exec`, and is
//!   not blocked.
use crate::env_file::{
    self, DEFAULT_MISE_TIMEOUT, HookMise, MiseCommand, MiseDelivery, check_command_env,
    check_mise_trust, classify_hooks,
};
use crate::hooks::{HookCommand, HookPhase};
use std::path::Path;

/// What the check decided about running the setup hooks.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Verdict {
    /// Why no hook may run; the phase needs attention until it is resolved.
    pub block: Option<String>,
    /// Things worth saying that do not stop the hooks.
    pub warnings: Vec<String>,
    /// The check was skipped on request (`--skip-mise-check`).
    pub skipped: bool,
}

fn names(hooks: &[&HookMise]) -> String {
    hooks
        .iter()
        .map(|hook| hook.label.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Check the worktree's mise before `commands` run with `vars` (the variables
/// devflow gives them).
///
/// Only a worktree that has a `.devflow/env` is checked, and only for hooks
/// that run mise. `skip` records that the person asked not to
/// (`--skip-mise-check`).
///
/// - `Delivered`: the hooks run.
/// - `Overridden`, `Removed`, `Untrusted`, `Failed`: no hook runs.
/// - `Unavailable`: no hook runs, since one of them needs mise.
///
/// Whether mise trusts the configuration is asked for every hook that runs
/// mise, `devflow exec` inside it or not: `mise exec` would trust what it
/// runs on its own, and devflow does not let it. What the hook would receive
/// is only asked for the hooks that `devflow exec` does not cover and that a
/// check at the worktree root can stand for.
pub(super) fn check(
    workspace: &Path,
    commands: &[HookCommand],
    vars: &[(String, String)],
    skip: bool,
    mise: &MiseCommand,
) -> Verdict {
    let mut verdict = Verdict::default();
    if !env_file::path(workspace).is_file() {
        return verdict;
    }
    let hooks = classify_hooks(commands);
    let using: Vec<&HookMise> = hooks.iter().filter(|hook| hook.uses_mise).collect();
    if using.is_empty() {
        // No hook runs mise.
        return verdict;
    }
    if skip {
        verdict.skipped = true;
        verdict.warnings.push(
            "--skip-mise-check: setup hooks ran without checking that mise trusts the \
             worktree and leaves them the workspace's variables (recorded in the receipt)"
                .to_string(),
        );
        return verdict;
    }
    for hook in &hooks {
        if let Some(reason) = hook.unchecked {
            verdict.warnings.push(format!(
                "{} {}: the check of what mise does to the workspace's variables cannot stand \
                 for it, so it was not blocked. Run its command through `devflow exec` \
                 (`mise exec -- devflow exec -- COMMAND`) and the variables win whatever mise \
                 sets",
                hook.label,
                reason.describe()
            ));
        }
    }
    let checked: Vec<&HookMise> = hooks.iter().filter(|hook| hook.checked).collect();
    let delivery = if checked.is_empty() {
        check_mise_trust(
            workspace,
            vars,
            HookPhase::Setup,
            DEFAULT_MISE_TIMEOUT,
            mise,
        )
    } else {
        check_command_env(
            workspace,
            vars,
            HookPhase::Setup,
            DEFAULT_MISE_TIMEOUT,
            mise,
        )
    };
    const SKIP_HINT: &str = "No hook ran. Route the command through `devflow exec` \
         (`mise exec -- devflow exec -- COMMAND`), fix mise's configuration and run \
         `devflow setup`, or run the hooks anyway with `devflow setup --skip-mise-check`";
    match &delivery {
        MiseDelivery::Delivered | MiseDelivery::NotLoaded => {}
        MiseDelivery::Overridden { .. } | MiseDelivery::Removed { .. } => {
            verdict.block = Some(format!(
                "{} would not get the workspace's variables as devflow sets them: {}. \
                 {SKIP_HINT}",
                names(&checked),
                delivery.describe()
            ));
        }
        // Hooks that only run `devflow exec` inside mise do not depend on
        // what mise could read; a check that fails says nothing about them.
        MiseDelivery::Failed { .. } if checked.is_empty() => {}
        MiseDelivery::Failed { .. } => {
            verdict.block = Some(format!("{}. {SKIP_HINT}", delivery.describe()));
        }
        MiseDelivery::Untrusted { .. } => {
            // devflow never trusts a configuration on its own.
            verdict.block = Some(format!(
                "{}. No hook ran; run `devflow setup` again once mise trusts it (`devflow \
                 doctor` lists the ways to do that)",
                delivery.describe()
            ));
        }
        MiseDelivery::Unavailable => {
            verdict.block = Some(format!(
                "mise is not installed or not on PATH, and {} runs it. No hook ran; install \
                 mise (https://mise.jdx.dev), then run `devflow setup`, or run the hooks anyway \
                 with `devflow setup --skip-mise-check`",
                using[0].label
            ));
        }
    }
    verdict
}

#[cfg(all(test, unix))]
mod tests;
