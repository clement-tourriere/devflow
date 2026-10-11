//! The `mise-env` and `mise-wiring` checks: what mise does to the variables of
//! the current workspace's `.devflow/env`.
//!
//! `mise-env` asks the question setup asks before its first hook, with the
//! same machinery (`env_file::check_command_env`): run in the worktree with
//! the environment a hook gets, does the process mise starts still see
//! devflow's variables? It also lists the setup hooks that run mise without
//! `devflow exec` inside it, since those are the ones a mise configuration can
//! hand another database. `mise-wiring` asks what mise resolves on its own
//! (`mise env`): what shells and editors in the worktree get, which commands
//! devflow runs do not depend on.
//!
//! Whether the workspace involves mise is decided from the hooks' commands and
//! from the detection layer's complete discovery of mise's configuration files
//! (`conf.d` and environment-specific files included), never from a short
//! list of names. Names variables and files, never values.
use super::Check;
use crate::env_file::{
    self, DEFAULT_MISE_TIMEOUT, HookMise, MiseCommand, MiseDelivery, check_command_env,
    check_mise_delivery_with, classify_hooks,
};
use crate::hooks::HookPhase;
use crate::project::Project;
use crate::{setup, worktree};
use std::path::Path;

/// The checks for the workspace containing the current directory; silent
/// outside a workspace, in one without a generated env file, and in one that
/// has no mise configuration and no hook that runs mise.
pub(super) fn checks(project: &Project) -> Vec<Check> {
    match std::env::current_dir() {
        Ok(cwd) => checks_in(project, &cwd, &MiseCommand::default()),
        Err(_) => Vec::new(),
    }
}

pub(super) fn checks_in(project: &Project, cwd: &Path, mise: &MiseCommand) -> Vec<Check> {
    let Ok(list) = setup::list(project) else {
        return Vec::new();
    };
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let current = list
        .iter()
        .filter(|item| !item.workspace.missing)
        .filter(|item| cwd.starts_with(&item.workspace.path))
        .max_by_key(|item| item.workspace.path.components().count());
    let Some(current) = current else {
        return Vec::new();
    };
    let (path, name) = (&current.workspace.path, &current.workspace.name);
    if !env_file::path(path).is_file() {
        return Vec::new();
    }
    let commands = project
        .config
        .hooks
        .commands(HookPhase::Setup)
        .unwrap_or_default();
    let hooks = classify_hooks(&commands);
    let configured = !env_file::mise_configs(path).is_empty();
    if !configured && !hooks.iter().any(|hook| hook.unprotected) {
        return Vec::new();
    }
    let vars = env_file::read(path).unwrap_or_default();
    let delivery = check_command_env(path, &vars, HookPhase::Setup, DEFAULT_MISE_TIMEOUT, mise);
    let mut checks = vec![env_check(project, name, &delivery, &hooks)];
    if configured && delivery.is_delivered() {
        let keys: Vec<String> = vars.iter().map(|(key, _)| key.clone()).collect();
        checks.extend(wiring_check(
            name,
            &check_mise_delivery_with(path, &keys, DEFAULT_MISE_TIMEOUT, mise),
        ));
    }
    checks
}

const EXEC_HINT: &str = "run the commands that need the workspace's database through \
     `devflow exec` (in a hook: `mise exec -- devflow exec -- COMMAND`), which sets \
     .devflow/env after mise has";

fn env_check(
    project: &Project,
    workspace: &str,
    delivery: &MiseDelivery,
    hooks: &[HookMise],
) -> Check {
    let unrouted: Vec<&str> = hooks
        .iter()
        .filter(|hook| hook.unrouted)
        .map(|hook| hook.label.as_str())
        .collect();
    // Setup refuses to run the hooks that a check stands for when it fails.
    let blocking = hooks.iter().any(|hook| hook.checked);
    let cannot_check = hooks.iter().any(|hook| hook.unchecked.is_some());
    let mut detail = match delivery {
        MiseDelivery::Untrusted { path } => format!("{workspace}: {}", trust_offer(project, path)),
        other => format!("{workspace}: {}", other.describe()),
    };
    if !unrouted.is_empty() {
        detail.push_str(&format!(
            "; not routed through `devflow exec`: {}",
            unrouted.join(", ")
        ));
    }
    match delivery {
        MiseDelivery::Delivered if cannot_check => Check::warn(
            "mise-env",
            detail,
            format!(
                "some setup hooks run mise in a way the check cannot reproduce (a cd before \
                 mise, a mise task, `mise -C`); {EXEC_HINT}"
            ),
        ),
        MiseDelivery::Delivered if !unrouted.is_empty() => {
            Check::ok("mise-env", detail).hint(EXEC_HINT)
        }
        MiseDelivery::Delivered | MiseDelivery::NotLoaded => Check::ok("mise-env", detail),
        MiseDelivery::Overridden { .. } | MiseDelivery::Removed { .. } => {
            let hint = format!(
                "{EXEC_HINT}; or fix mise's configuration ([env] keys, later `_.file` entries, \
                 mise.local.toml, environment-specific files). Setup can be asked to skip \
                 this check and run the hooks anyway"
            );
            if blocking {
                Check::fail("mise-env", detail, hint)
            } else {
                Check::warn("mise-env", detail, hint)
            }
        }
        MiseDelivery::Untrusted { .. } => Check::warn(
            "mise-env",
            detail,
            "devflow never trusts mise configuration for you; setup waits until mise does",
        ),
        MiseDelivery::Unavailable => Check::warn(
            "mise-env",
            detail,
            "install: https://mise.jdx.dev (setup hooks that run mise cannot work without it)",
        ),
        MiseDelivery::Failed { .. } => Check::warn(
            "mise-env",
            detail,
            "run `mise exec -- true` in the workspace to see why",
        ),
    }
}

/// What shells and editors in the worktree get through mise.
fn wiring_check(workspace: &str, delivery: &MiseDelivery) -> Option<Check> {
    const UNAFFECTED: &str = "commands devflow runs are unaffected";
    Some(match delivery {
        MiseDelivery::Delivered => Check::ok(
            "mise-wiring",
            format!("{workspace}: mise loads .devflow/env for shells and editors in the worktree"),
        ),
        MiseDelivery::NotLoaded => Check::warn(
            "mise-wiring",
            format!(
                "{workspace}: shells in the worktree won't see the branch's variables through \
                 mise; {UNAFFECTED}"
            ),
            "setting the project up proposes `[env] _.file = \".devflow/env\"` for mise.toml",
        ),
        MiseDelivery::Overridden { keys } | MiseDelivery::Removed { keys } => Check::warn(
            "mise-wiring",
            format!(
                "{workspace}: shells in the worktree get {} from another source than \
                 .devflow/env through mise; {UNAFFECTED}",
                keys.join(", ")
            ),
            "put `.devflow/env` last in mise's `[env] _.file`, or run commands through \
             `devflow exec`",
        ),
        _ => return None,
    })
}

/// What a person can do about a configuration mise does not trust. Text only:
/// devflow never trusts a configuration, and never edits mise's settings.
pub(super) fn trust_offer(project: &Project, config: &Path) -> String {
    let file = env_file::shell_word(&config.display().to_string());
    let base = worktree::path_for(project, "probe")
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| project.root.clone());
    let base = env_file::shell_word(&base.display().to_string());
    format!(
        "mise does not trust {}. Your choices (devflow runs neither): `mise trust {file}` \
         trusts this one file, after you have read it; or `mise settings add \
         trusted_config_paths {base}` trusts the worktree directory, which trusts every mise \
         configuration file that appears under it from now on, in each worktree anyone creates \
         there, so choose it only if you trust everything that will be checked out there",
        config.display()
    )
}

#[cfg(all(test, unix))]
mod tests;
