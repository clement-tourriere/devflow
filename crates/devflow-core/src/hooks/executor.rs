use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use tokio::task::JoinHandle;

use super::actions;
use super::actions::shell::run_shell_command;
use super::approval::ApprovalStore;
use super::template::TemplateEngine;
use super::{ActionHookEntry, HookContext, HookEntry, HookPhase, HooksConfig};

/// Background hook tasks spawned by any engine in this process.
///
/// Hooks on non-blocking phases are fire-and-forget tokio tasks; in the
/// short-lived CLI they used to race process exit and silently never run.
/// CLI entry points now await them via [`wait_for_background_hooks`].
static BACKGROUND_HOOKS: StdMutex<Vec<(String, JoinHandle<()>)>> = StdMutex::new(Vec::new());

fn track_background_hook(name: &str, handle: JoinHandle<()>) {
    if let Ok(mut tasks) = BACKGROUND_HOOKS.lock() {
        tasks.retain(|(_, h)| !h.is_finished());
        tasks.push((name.to_string(), handle));
    }
}

/// Await completion of all background hooks spawned so far in this process.
///
/// Returns `(completed, total)`. Tasks still running after `timeout` are
/// left detached (the pre-existing behavior for all background hooks).
pub async fn wait_for_background_hooks(timeout: Duration) -> (usize, usize) {
    let tasks: Vec<(String, JoinHandle<()>)> = match BACKGROUND_HOOKS.lock() {
        Ok(mut t) => t.drain(..).collect(),
        Err(_) => return (0, 0),
    };
    let total = tasks.len();
    if total == 0 {
        return (0, 0);
    }
    let mut completed = 0;
    let deadline = tokio::time::Instant::now() + timeout;
    for (name, handle) in tasks {
        match tokio::time::timeout_at(deadline, handle).await {
            Ok(_) => completed += 1,
            Err(_) => log::warn!(
                "Background hook '{}' did not finish within {:?}; leaving it detached",
                name,
                timeout
            ),
        }
    }
    (completed, total)
}

/// `DEVFLOW_APPROVE_HOOKS=1` auto-approves config hooks — the escape hatch
/// for agents and CI where interactive approval is impossible.
fn env_auto_approve() -> bool {
    std::env::var("DEVFLOW_APPROVE_HOOKS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
        .unwrap_or(false)
}

/// Executes hooks for a given phase, handling template rendering,
/// approval checks, conditions, and blocking/background dispatch.
pub struct HookEngine {
    template_engine: TemplateEngine,
    hooks_config: HooksConfig,
    working_dir: PathBuf,
    /// Project key for the approval store (canonicalized project dir)
    project_key: Option<String>,
    /// Whether to require approval for hooks from project config
    require_approval: bool,
    /// Whether interactive prompts are allowed.
    non_interactive: bool,
    /// Auto-approve hooks without prompting (DEVFLOW_APPROVE_HOOKS=1).
    auto_approve: bool,
    /// Whether to suppress hook stdout-friendly output.
    quiet_output: bool,
}

/// Result of running hooks for a phase.
#[derive(Debug, Default)]
pub struct HookRunResult {
    /// Number of hooks that ran successfully
    pub succeeded: usize,
    /// Number of hooks that were skipped (condition false, unapproved, etc.)
    pub skipped: usize,
    /// Number of hooks that failed (but continued due to continue_on_error)
    pub failed: usize,
    /// Number of hooks spawned in the background
    pub background: usize,
    /// Error messages from failed hooks
    pub errors: Vec<String>,
}

impl HookEngine {
    /// Create a new HookEngine.
    ///
    /// - `hooks_config`: The parsed hooks section from devflow config
    /// - `working_dir`: Project root directory
    /// - `project_key`: Optional project identifier for approval tracking
    pub fn new(
        hooks_config: HooksConfig,
        working_dir: PathBuf,
        project_key: Option<String>,
    ) -> Self {
        Self {
            template_engine: TemplateEngine::new(),
            hooks_config,
            working_dir,
            project_key,
            require_approval: true,
            non_interactive: false,
            auto_approve: env_auto_approve(),
            quiet_output: false,
        }
    }

    /// Create a HookEngine for automated executions where prompts are not allowed.
    pub fn new_non_interactive(
        hooks_config: HooksConfig,
        working_dir: PathBuf,
        project_key: Option<String>,
    ) -> Self {
        Self {
            template_engine: TemplateEngine::new(),
            hooks_config,
            working_dir,
            project_key,
            require_approval: true,
            non_interactive: true,
            auto_approve: env_auto_approve(),
            quiet_output: false,
        }
    }

    /// Create a HookEngine that does not require approval (e.g., for user-invoked manual runs).
    pub fn new_no_approval(hooks_config: HooksConfig, working_dir: PathBuf) -> Self {
        Self {
            template_engine: TemplateEngine::new(),
            hooks_config,
            working_dir,
            project_key: None,
            require_approval: false,
            non_interactive: false,
            auto_approve: env_auto_approve(),
            quiet_output: false,
        }
    }

    /// Return a cloned engine configuration with output verbosity configured.
    pub fn with_quiet_output(mut self, quiet: bool) -> Self {
        self.quiet_output = quiet;
        self
    }

    /// Check whether the config has any hooks for the given phase.
    pub fn has_hooks_for(&self, phase: &HookPhase) -> bool {
        self.hooks_config
            .get(phase)
            .map(|hooks| !hooks.is_empty())
            .unwrap_or(false)
    }

    /// Run all hooks registered for the given phase.
    pub async fn run_phase(
        &self,
        phase: &HookPhase,
        context: &HookContext,
    ) -> Result<HookRunResult> {
        let hooks = match self.hooks_config.get(phase) {
            Some(hooks) if !hooks.is_empty() => hooks,
            _ => return Ok(HookRunResult::default()),
        };

        let mut result = HookRunResult::default();
        let phase_blocking = phase.is_blocking();

        log::info!("Running hooks for phase: {}", phase);

        for (name, entry) in hooks {
            match self
                .run_single_hook(name, entry, context, phase_blocking)
                .await
            {
                Ok(HookOutcome::Succeeded) => {
                    result.succeeded += 1;
                }
                Ok(HookOutcome::Skipped(reason)) => {
                    log::debug!("Hook '{}' skipped: {}", name, reason);
                    result.skipped += 1;
                }
                Ok(HookOutcome::Background) => {
                    result.background += 1;
                }
                Err(e) => {
                    let continue_on_error = match entry {
                        HookEntry::Simple(_) => !phase_blocking,
                        HookEntry::Extended(ext) => {
                            ext.continue_on_error.unwrap_or(!phase_blocking)
                        }
                        HookEntry::Action(act) => act.continue_on_error.unwrap_or(!phase_blocking),
                    };

                    if continue_on_error {
                        log::warn!("Hook '{}' failed (continuing): {}", name, e);
                        eprintln!("  Warning: hook '{}' failed: {}", name, e);
                        result.failed += 1;
                        result.errors.push(format!("{}: {}", name, e));
                    } else {
                        return Err(e).with_context(|| format!("Hook '{}' failed", name));
                    }
                }
            }
        }

        Ok(result)
    }

    /// Run all hooks for a phase, printing a header/footer summary.
    pub async fn run_phase_verbose(
        &self,
        phase: &HookPhase,
        context: &HookContext,
    ) -> Result<HookRunResult> {
        if !self.has_hooks_for(phase) {
            return Ok(HookRunResult::default());
        }

        println!("Running {} hooks...", phase);
        let result = self.run_phase(phase, context).await?;

        if result.succeeded > 0 || result.background > 0 {
            let mut parts = vec![];
            if result.succeeded > 0 {
                parts.push(format!("{} succeeded", result.succeeded));
            }
            if result.background > 0 {
                parts.push(format!("{} background", result.background));
            }
            if result.skipped > 0 {
                parts.push(format!("{} skipped", result.skipped));
            }
            if result.failed > 0 {
                parts.push(format!("{} failed", result.failed));
            }
            println!("  Hooks complete: {}", parts.join(", "));
        }

        Ok(result)
    }

    async fn run_single_hook(
        &self,
        name: &str,
        entry: &HookEntry,
        context: &HookContext,
        phase_blocking: bool,
    ) -> Result<HookOutcome> {
        match entry {
            HookEntry::Simple(cmd) => {
                self.run_shell_hook(name, cmd, None, context, phase_blocking)
                    .await
            }
            HookEntry::Extended(ext) => {
                self.run_shell_hook(name, &ext.command, Some(ext), context, phase_blocking)
                    .await
            }
            HookEntry::Action(act) => {
                self.run_action_hook(name, act, context, phase_blocking)
                    .await
            }
        }
    }

    /// Execute a shell-based hook (Simple or Extended entry).
    async fn run_shell_hook(
        &self,
        name: &str,
        command_template: &str,
        extended: Option<&super::ExtendedHookEntry>,
        context: &HookContext,
        phase_blocking: bool,
    ) -> Result<HookOutcome> {
        // Check condition
        if let Some(ext) = &extended {
            if let Some(outcome) = self.check_condition(name, &ext.condition, context)? {
                return Ok(outcome);
            }
        }

        // Render the command template
        let rendered_command = self.template_engine.render(command_template, context)?;

        // Check approval (shell commands always require approval).
        // Keyed on the command TEMPLATE so an approval survives across
        // workspaces; the rendered form is what the user sees in the prompt.
        if self.require_approval {
            if let Some(outcome) = self.check_approval(name, command_template, &rendered_command)? {
                return Ok(outcome);
            }
        }

        // Determine if this should run in the background
        let run_background = extended.map(|e| e.background).unwrap_or(false) || !phase_blocking;

        // Resolve working_dir once — background hooks must honor it too.
        let working_dir = if let Some(ext) = &extended {
            ext.working_dir
                .as_ref()
                .map(|wd| self.working_dir.join(wd))
                .unwrap_or_else(|| self.working_dir.clone())
        } else {
            self.working_dir.clone()
        };

        if run_background {
            let cmd = rendered_command.clone();
            let wd = working_dir.clone();
            let hook_name = name.to_string();
            let env_vars = extended.and_then(|e| e.environment.clone());
            let ctx_clone = context.clone();
            let te = TemplateEngine::new();
            let quiet_output = self.quiet_output;

            let handle = tokio::spawn(async move {
                match run_shell_command(
                    &cmd,
                    &wd,
                    env_vars.as_ref(),
                    &ctx_clone,
                    &te,
                    !quiet_output,
                ) {
                    Ok(_) => log::debug!("Background hook '{}' completed", hook_name),
                    Err(e) => log::warn!("Background hook '{}' failed: {}", hook_name, e),
                }
            });
            track_background_hook(name, handle);

            return Ok(HookOutcome::Background);
        }

        // Blocking execution
        if !self.quiet_output {
            println!("  Running: {} ({})", name, rendered_command);
        }

        let env_vars = extended.and_then(|e| e.environment.clone());

        run_shell_command(
            &rendered_command,
            &working_dir,
            env_vars.as_ref(),
            context,
            &self.template_engine,
            !self.quiet_output,
        )?;

        Ok(HookOutcome::Succeeded)
    }

    /// Execute an action-based hook entry.
    async fn run_action_hook(
        &self,
        name: &str,
        act: &ActionHookEntry,
        context: &HookContext,
        phase_blocking: bool,
    ) -> Result<HookOutcome> {
        // Check condition
        if let Some(outcome) = self.check_condition(name, &act.condition, context)? {
            return Ok(outcome);
        }

        // Check approval only for actions that require it (shell, docker-exec, http).
        // Keyed on the templates so an approval survives across workspaces.
        if self.require_approval && act.action.requires_approval() {
            let (approval_key, display) = match &act.action {
                super::HookAction::Shell { command } => (
                    command.clone(),
                    self.template_engine.render(command, context)?,
                ),
                super::HookAction::DockerExec {
                    container, command, ..
                } => {
                    let key = format!("docker exec {} sh -c '{}'", container, command);
                    let c = self.template_engine.render(container, context)?;
                    let cmd = self.template_engine.render(command, context)?;
                    (key, format!("docker exec {} sh -c '{}'", c, cmd))
                }
                super::HookAction::Http {
                    url,
                    method,
                    body,
                    headers,
                } => {
                    let key = format!(
                        "http {} {} body={:?} headers={:?}",
                        method, url, body, headers
                    );
                    let rendered_url = self.template_engine.render(url, context)?;
                    (key, format!("http {} {}", method, rendered_url))
                }
                // Any other action that requires approval must be handled above;
                // bail explicitly instead of panicking so adding a new action
                // surfaces a clear error rather than crashing the process.
                other => anyhow::bail!(
                    "approval required for action '{}' but no approval key is defined",
                    other.type_name()
                ),
            };
            if let Some(outcome) = self.check_approval(name, &approval_key, &display)? {
                return Ok(outcome);
            }
        }

        // Determine if this should run in the background
        let run_background = act.background || !phase_blocking;

        let working_dir = act
            .working_dir
            .as_ref()
            .map(|wd| self.working_dir.join(wd))
            .unwrap_or_else(|| self.working_dir.clone());

        if run_background {
            let action = act.action.clone();
            let ctx_clone = context.clone();
            let te = TemplateEngine::new();
            let hook_name = name.to_string();
            let wd = working_dir.clone();
            let quiet_output = self.quiet_output;

            let handle = tokio::spawn(async move {
                match actions::execute_action(&action, &ctx_clone, &te, &wd, !quiet_output).await {
                    Ok(r) => {
                        log::debug!("Background hook '{}' completed: {}", hook_name, r.summary)
                    }
                    Err(e) => log::warn!("Background hook '{}' failed: {}", hook_name, e),
                }
            });
            track_background_hook(name, handle);

            return Ok(HookOutcome::Background);
        }

        // Blocking execution
        if !self.quiet_output {
            println!("  Running: {} (action: {})", name, act.action.type_name());
        }

        let result = actions::execute_action(
            &act.action,
            context,
            &self.template_engine,
            &working_dir,
            !self.quiet_output,
        )
        .await?;

        if !self.quiet_output {
            log::debug!("Action result: {}", result.summary);
        }

        Ok(HookOutcome::Succeeded)
    }

    /// Check a condition expression. Returns `Some(HookOutcome::Skipped(..))` if the
    /// condition is false or denied, `None` if the hook should proceed.
    fn check_condition(
        &self,
        name: &str,
        condition: &Option<String>,
        context: &HookContext,
    ) -> Result<Option<HookOutcome>> {
        let condition = match condition {
            Some(c) => c,
            None => return Ok(None),
        };

        let rendered_condition = self.template_engine.render(condition, context)?;

        // Shell-based conditions are executable code too, so they must be
        // approved before evaluation when approvals are enabled.
        // Keyed on the raw condition template so approval survives across
        // workspaces (rendered conditions differ per workspace).
        if self.require_approval
            && !self.auto_approve
            && Self::condition_uses_shell(&rendered_condition, context)
        {
            let cond_name = format!("{} (condition)", name);
            let Some(ref project_key) = self.project_key else {
                if self.non_interactive {
                    return Ok(Some(self.skip_unapproved(&cond_name, &rendered_condition)));
                }
                log::warn!(
                    "No project key available; skipping approval prompt for condition of hook '{}'",
                    name
                );
                return Ok(None);
            };

            let approval_key = format!("condition: {}", condition);
            let mut store = ApprovalStore::load().unwrap_or_default();
            if !store.is_approved(project_key, &approval_key) {
                if self.non_interactive {
                    return Ok(Some(self.skip_unapproved(&cond_name, &rendered_condition)));
                }
                match Self::prompt_hook_approval(&cond_name, &rendered_condition) {
                    HookApprovalChoice::ApproveAlways => {
                        if let Err(e) = store.approve(project_key, &approval_key) {
                            log::warn!("Failed to persist hook condition approval: {}", e);
                        }
                    }
                    HookApprovalChoice::ApproveOnce => {}
                    HookApprovalChoice::Deny => {
                        return Ok(Some(HookOutcome::Skipped(
                            "condition command not approved by user".to_string(),
                        )));
                    }
                }
            }
        }

        if !self.evaluate_condition(&rendered_condition, context)? {
            return Ok(Some(HookOutcome::Skipped(format!(
                "condition '{}' was false",
                rendered_condition
            ))));
        }

        Ok(None)
    }

    /// Check approval for a hook command/action.
    ///
    /// `approval_key` is the stable identity stored in the approval store
    /// (the un-rendered template from the config file); `display` is the
    /// rendered command shown to the user.
    /// Returns `Some(HookOutcome::Skipped(..))` if denied, `None` if approved.
    fn check_approval(
        &self,
        name: &str,
        approval_key: &str,
        display: &str,
    ) -> Result<Option<HookOutcome>> {
        if self.auto_approve {
            log::info!("Hook '{}' auto-approved via DEVFLOW_APPROVE_HOOKS", name);
            return Ok(None);
        }

        let Some(ref project_key) = self.project_key else {
            if self.non_interactive {
                return Ok(Some(self.skip_unapproved(name, display)));
            }
            log::warn!(
                "No project key available; skipping approval prompt for hook '{}'",
                name
            );
            return Ok(None);
        };

        let mut store = ApprovalStore::load().unwrap_or_default();
        if !store.is_approved(project_key, approval_key) {
            if self.non_interactive {
                return Ok(Some(self.skip_unapproved(name, display)));
            }
            match Self::prompt_hook_approval(name, display) {
                HookApprovalChoice::ApproveAlways => {
                    if let Err(e) = store.approve(project_key, approval_key) {
                        log::warn!("Failed to persist hook approval: {}", e);
                    }
                }
                HookApprovalChoice::ApproveOnce => {}
                HookApprovalChoice::Deny => {
                    return Ok(Some(HookOutcome::Skipped(
                        "not approved by user".to_string(),
                    )));
                }
            }
        }

        Ok(None)
    }

    /// Skip an unapproved hook in non-interactive mode — visibly, instead of
    /// aborting the whole command after the workspace already exists.
    fn skip_unapproved(&self, name: &str, display: &str) -> HookOutcome {
        eprintln!(
            "  Warning: hook '{}' skipped — not approved for non-interactive mode.",
            name
        );
        eprintln!("    command: {}", display);
        eprintln!(
            "    Approve it once interactively, or set DEVFLOW_APPROVE_HOOKS=1 for automated runs."
        );
        HookOutcome::Skipped("requires approval (non-interactive mode)".to_string())
    }

    fn evaluate_condition(&self, rendered: &str, context: &HookContext) -> Result<bool> {
        // ── Path conditions ──
        // Comma-separated alternatives: true when ANY of them exists
        // (e.g. "file_exists:mise.toml,.mise.toml").
        if let Some(file_paths) = rendered.strip_prefix("file_exists:") {
            return Ok(file_paths
                .split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .any(|p| self.working_dir.join(p).exists()));
        }
        if let Some(dir_paths) = rendered.strip_prefix("dir_exists:") {
            return Ok(dir_paths
                .split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .any(|p| self.working_dir.join(p).is_dir()));
        }
        // Comma-separated alternatives: true when ANY binary is installed
        // (resolves via PATH or the mise shims dir, like hook execution does).
        if let Some(binaries) = rendered.strip_prefix("command_exists:") {
            return Ok(binaries
                .split(',')
                .map(str::trim)
                .filter(|b| !b.is_empty())
                .any(super::detect::binary_exists));
        }

        // ── Boolean literals ──
        if rendered == "always" || rendered == "true" {
            return Ok(true);
        }
        if rendered == "never" || rendered == "false" {
            return Ok(false);
        }

        // ── Worktree conditions ──
        if rendered == "is_worktree" {
            return Ok(context.worktree_path.is_some());
        }
        if rendered == "not_worktree" {
            return Ok(context.worktree_path.is_none());
        }

        // ── Trigger source conditions ──
        if let Some(source) = rendered.strip_prefix("trigger_is:") {
            return Ok(context.trigger_source == source.trim());
        }
        if let Some(source) = rendered.strip_prefix("trigger_not:") {
            return Ok(context.trigger_source != source.trim());
        }

        // ── Workspace name matching ──
        if let Some(pattern) = rendered.strip_prefix("workspace_matches:") {
            let re = regex::Regex::new(pattern.trim())
                .with_context(|| format!("Invalid regex in condition: {}", pattern))?;
            return Ok(re.is_match(&context.workspace));
        }
        if let Some(name) = rendered.strip_prefix("workspace_is:") {
            return Ok(context.workspace == name.trim());
        }
        if let Some(name) = rendered.strip_prefix("workspace_not:") {
            return Ok(context.workspace != name.trim());
        }

        // ── Environment variable conditions ──
        if let Some(var) = rendered.strip_prefix("env_set:") {
            return Ok(std::env::var(var.trim()).is_ok());
        }
        if let Some(pair) = rendered.strip_prefix("env_is:") {
            if let Some((var, expected)) = pair.trim().split_once('=') {
                return Ok(std::env::var(var).ok().as_deref() == Some(expected));
            }
            return Ok(false);
        }

        // ── Default workspace condition ──
        if rendered == "is_default_workspace" {
            return Ok(context.workspace == context.default_workspace);
        }
        if rendered == "not_default_workspace" {
            return Ok(context.workspace != context.default_workspace);
        }

        // ── Fallback: shell command ──
        let output = std::process::Command::new("sh")
            .args(["-c", rendered])
            .current_dir(&self.working_dir)
            .output()
            .with_context(|| format!("Failed to evaluate condition: {}", rendered))?;
        Ok(output.status.success())
    }

    fn condition_uses_shell(rendered: &str, context: &HookContext) -> bool {
        // All built-in conditions are safe (no arbitrary code execution)
        let _ = context; // context available for future conditions
        !Self::is_builtin_condition(rendered)
    }

    /// Whether a condition string is a built-in (non-shell) condition.
    pub fn is_builtin_condition(rendered: &str) -> bool {
        rendered.starts_with("file_exists:")
            || rendered.starts_with("dir_exists:")
            || rendered.starts_with("command_exists:")
            || rendered.starts_with("trigger_is:")
            || rendered.starts_with("trigger_not:")
            || rendered.starts_with("workspace_matches:")
            || rendered.starts_with("workspace_is:")
            || rendered.starts_with("workspace_not:")
            || rendered.starts_with("env_set:")
            || rendered.starts_with("env_is:")
            || matches!(
                rendered,
                "always"
                    | "true"
                    | "never"
                    | "false"
                    | "is_worktree"
                    | "not_worktree"
                    | "is_default_workspace"
                    | "not_default_workspace"
            )
    }

    /// Prompt the user to approve a hook command before execution.
    fn prompt_hook_approval(hook_name: &str, rendered_command: &str) -> HookApprovalChoice {
        println!();
        println!("  Hook '{}' wants to run:", hook_name);
        println!("    {}", rendered_command);
        println!();

        let options = vec![
            "Approve (always for this command)",
            "Approve (this time only)",
            "Deny (skip this hook)",
        ];

        match inquire::Select::new("Allow this hook to run?", options).prompt() {
            Ok(choice) => {
                if choice.starts_with("Approve (always") {
                    HookApprovalChoice::ApproveAlways
                } else if choice.starts_with("Approve (this") {
                    HookApprovalChoice::ApproveOnce
                } else {
                    HookApprovalChoice::Deny
                }
            }
            Err(_) => {
                // On cancel/interrupt, deny
                println!("  Hook denied.");
                HookApprovalChoice::Deny
            }
        }
    }
}

enum HookApprovalChoice {
    ApproveAlways,
    ApproveOnce,
    Deny,
}

enum HookOutcome {
    Succeeded,
    Skipped(String),
    Background,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::{
        ExtendedHookEntry, HookContext, HookEntry, HookPhase, HooksConfig, IndexMap,
    };

    fn make_engine(hooks: HooksConfig) -> HookEngine {
        let working_dir = std::env::current_dir().unwrap();
        HookEngine::new_no_approval(hooks, working_dir)
    }

    fn basic_context() -> HookContext {
        HookContext {
            workspace: "feature/test".to_string(),
            repo: "myapp".to_string(),
            default_workspace: "main".to_string(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_no_hooks_returns_empty_result() {
        let engine = make_engine(IndexMap::new());
        let result = engine
            .run_phase(&HookPhase::PostCreate, &basic_context())
            .await
            .unwrap();
        assert_eq!(result.succeeded, 0);
        assert_eq!(result.skipped, 0);
    }

    #[tokio::test]
    async fn test_simple_echo_hook() {
        let mut hooks: HooksConfig = IndexMap::new();
        let mut phase_hooks = IndexMap::new();
        phase_hooks.insert(
            "greet".to_string(),
            HookEntry::Simple("echo hello {{ workspace }}".to_string()),
        );
        hooks.insert(HookPhase::PostCreate, phase_hooks);

        let engine = make_engine(hooks);
        let result = engine
            .run_phase(&HookPhase::PostCreate, &basic_context())
            .await
            .unwrap();
        assert_eq!(result.succeeded, 1);
    }

    #[tokio::test]
    async fn test_condition_never_skips() {
        let mut hooks: HooksConfig = IndexMap::new();
        let mut phase_hooks = IndexMap::new();
        phase_hooks.insert(
            "skip-me".to_string(),
            HookEntry::Extended(ExtendedHookEntry {
                command: "echo should not run".to_string(),
                working_dir: None,
                continue_on_error: None,
                condition: Some("never".to_string()),
                environment: None,
                background: false,
            }),
        );
        hooks.insert(HookPhase::PostCreate, phase_hooks);

        let engine = make_engine(hooks);
        let result = engine
            .run_phase(&HookPhase::PostCreate, &basic_context())
            .await
            .unwrap();
        assert_eq!(result.skipped, 1);
        assert_eq!(result.succeeded, 0);
    }

    #[tokio::test]
    async fn test_failing_hook_with_continue() {
        let mut hooks: HooksConfig = IndexMap::new();
        let mut phase_hooks = IndexMap::new();
        phase_hooks.insert(
            "fail-ok".to_string(),
            HookEntry::Extended(ExtendedHookEntry {
                command: "exit 1".to_string(),
                working_dir: None,
                continue_on_error: Some(true),
                condition: None,
                environment: None,
                background: false,
            }),
        );
        hooks.insert(HookPhase::PostCreate, phase_hooks);

        let engine = make_engine(hooks);
        let result = engine
            .run_phase(&HookPhase::PostCreate, &basic_context())
            .await
            .unwrap();
        assert_eq!(result.failed, 1);
        assert_eq!(result.succeeded, 0);
    }

    #[test]
    fn test_has_hooks_for() {
        let mut hooks: HooksConfig = IndexMap::new();
        let mut phase_hooks = IndexMap::new();
        phase_hooks.insert("a".to_string(), HookEntry::Simple("echo a".to_string()));
        hooks.insert(HookPhase::PostCreate, phase_hooks);

        let engine = make_engine(hooks);
        assert!(engine.has_hooks_for(&HookPhase::PostCreate));
        assert!(!engine.has_hooks_for(&HookPhase::PreSwitch));
    }

    #[tokio::test]
    async fn test_action_hook_write_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut hooks: HooksConfig = IndexMap::new();
        let mut phase_hooks = IndexMap::new();
        phase_hooks.insert(
            "write-test".to_string(),
            HookEntry::Action(crate::hooks::ActionHookEntry {
                action: crate::hooks::HookAction::WriteFile {
                    path: "test-output.txt".to_string(),
                    content: "workspace={{ workspace }}".to_string(),
                    mode: crate::hooks::WriteMode::Overwrite,
                },
                working_dir: None,
                continue_on_error: None,
                condition: None,
                environment: None,
                background: false,
            }),
        );
        hooks.insert(HookPhase::PostCreate, phase_hooks);

        let engine = HookEngine::new_no_approval(hooks, tmp.path().to_path_buf());
        let result = engine
            .run_phase(&HookPhase::PostCreate, &basic_context())
            .await
            .unwrap();
        assert_eq!(result.succeeded, 1);

        let content = std::fs::read_to_string(tmp.path().join("test-output.txt")).unwrap();
        assert_eq!(content, "workspace=feature/test");
    }

    #[tokio::test]
    async fn test_condition_is_worktree() {
        let engine = make_engine(IndexMap::new());
        let mut ctx = basic_context();

        // No worktree
        ctx.worktree_path = None;
        assert!(!engine.evaluate_condition("is_worktree", &ctx).unwrap());
        assert!(engine.evaluate_condition("not_worktree", &ctx).unwrap());

        // With worktree
        ctx.worktree_path = Some("/tmp/worktree".to_string());
        assert!(engine.evaluate_condition("is_worktree", &ctx).unwrap());
        assert!(!engine.evaluate_condition("not_worktree", &ctx).unwrap());
    }

    #[tokio::test]
    async fn test_condition_trigger_source() {
        let engine = make_engine(IndexMap::new());
        let mut ctx = basic_context();
        ctx.trigger_source = "vcs".to_string();

        assert!(engine.evaluate_condition("trigger_is:vcs", &ctx).unwrap());
        assert!(!engine.evaluate_condition("trigger_is:cli", &ctx).unwrap());
        assert!(engine.evaluate_condition("trigger_not:cli", &ctx).unwrap());
        assert!(!engine.evaluate_condition("trigger_not:vcs", &ctx).unwrap());
    }

    #[tokio::test]
    async fn test_condition_workspace_matching() {
        let engine = make_engine(IndexMap::new());
        let ctx = basic_context(); // workspace = "feature/test"

        assert!(engine
            .evaluate_condition("workspace_is:feature/test", &ctx)
            .unwrap());
        assert!(!engine
            .evaluate_condition("workspace_is:main", &ctx)
            .unwrap());
        assert!(engine
            .evaluate_condition("workspace_not:main", &ctx)
            .unwrap());
        assert!(!engine
            .evaluate_condition("workspace_not:feature/test", &ctx)
            .unwrap());
        assert!(engine
            .evaluate_condition("workspace_matches:^feature/.*", &ctx)
            .unwrap());
        assert!(!engine
            .evaluate_condition("workspace_matches:^hotfix/.*", &ctx)
            .unwrap());
    }

    #[tokio::test]
    async fn test_condition_default_workspace() {
        let engine = make_engine(IndexMap::new());
        let mut ctx = basic_context(); // workspace = "feature/test", default = "main"

        assert!(!engine
            .evaluate_condition("is_default_workspace", &ctx)
            .unwrap());
        assert!(engine
            .evaluate_condition("not_default_workspace", &ctx)
            .unwrap());

        ctx.workspace = "main".to_string();
        assert!(engine
            .evaluate_condition("is_default_workspace", &ctx)
            .unwrap());
        assert!(!engine
            .evaluate_condition("not_default_workspace", &ctx)
            .unwrap());
    }

    #[tokio::test]
    async fn test_condition_env_vars() {
        let engine = make_engine(IndexMap::new());
        let ctx = basic_context();

        // Set a temp env var for testing
        std::env::set_var("DEVFLOW_TEST_COND_VAR", "hello");
        assert!(engine
            .evaluate_condition("env_set:DEVFLOW_TEST_COND_VAR", &ctx)
            .unwrap());
        assert!(engine
            .evaluate_condition("env_is:DEVFLOW_TEST_COND_VAR=hello", &ctx)
            .unwrap());
        assert!(!engine
            .evaluate_condition("env_is:DEVFLOW_TEST_COND_VAR=world", &ctx)
            .unwrap());
        std::env::remove_var("DEVFLOW_TEST_COND_VAR");
        assert!(!engine
            .evaluate_condition("env_set:DEVFLOW_TEST_COND_VAR", &ctx)
            .unwrap());
    }

    #[tokio::test]
    async fn test_non_interactive_unapproved_hook_skips_instead_of_failing() {
        let mut hooks: HooksConfig = IndexMap::new();
        let mut phase_hooks = IndexMap::new();
        phase_hooks.insert(
            "needs-approval".to_string(),
            HookEntry::Simple("echo should not run".to_string()),
        );
        hooks.insert(HookPhase::PostCreate, phase_hooks);

        // A project key that cannot have stored approvals
        let engine = HookEngine::new_non_interactive(
            hooks,
            std::env::current_dir().unwrap(),
            Some("/nonexistent/devflow-test-project-approval-key".to_string()),
        );

        let result = engine
            .run_phase(&HookPhase::PostCreate, &basic_context())
            .await
            .expect("unapproved hook must skip, not abort the phase");
        assert_eq!(result.skipped, 1);
        assert_eq!(result.succeeded, 0);
        assert_eq!(result.failed, 0);
    }

    #[tokio::test]
    async fn test_file_exists_condition_alternatives() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = HookEngine::new_no_approval(IndexMap::new(), tmp.path().to_path_buf());
        let ctx = basic_context();

        assert!(!engine
            .evaluate_condition("file_exists:mise.toml,.mise.toml", &ctx)
            .unwrap());

        std::fs::write(tmp.path().join("mise.toml"), "[tools]").unwrap();
        assert!(engine
            .evaluate_condition("file_exists:mise.toml,.mise.toml", &ctx)
            .unwrap());
        // Single-path form still works
        assert!(engine
            .evaluate_condition("file_exists:mise.toml", &ctx)
            .unwrap());
        assert!(!engine
            .evaluate_condition("file_exists:.mise.toml", &ctx)
            .unwrap());
    }

    #[tokio::test]
    async fn test_command_exists_condition() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = HookEngine::new_no_approval(IndexMap::new(), tmp.path().to_path_buf());
        let ctx = basic_context();

        assert!(engine
            .evaluate_condition("command_exists:sh", &ctx)
            .unwrap());
        assert!(!engine
            .evaluate_condition("command_exists:definitely-missing-bin-devflow-xyz", &ctx)
            .unwrap());
        // Comma-separated alternatives: any installed binary satisfies it
        assert!(engine
            .evaluate_condition("command_exists:definitely-missing-bin-devflow-xyz,sh", &ctx)
            .unwrap());
    }

    #[tokio::test]
    async fn test_background_hooks_complete_via_wait() {
        let tmp = tempfile::tempdir().unwrap();
        let mut hooks: HooksConfig = IndexMap::new();
        let mut phase_hooks = IndexMap::new();
        phase_hooks.insert(
            "bg-marker".to_string(),
            HookEntry::Simple("sleep 0.2 && touch bg-done.txt".to_string()),
        );
        // post-switch is a non-blocking phase → hook runs in the background
        hooks.insert(HookPhase::PostSwitch, phase_hooks);

        let engine = HookEngine::new_no_approval(hooks, tmp.path().to_path_buf());
        let result = engine
            .run_phase(&HookPhase::PostSwitch, &basic_context())
            .await
            .unwrap();
        assert_eq!(result.background, 1);
        assert!(
            !tmp.path().join("bg-done.txt").exists(),
            "hook should still be running"
        );

        let (_, total) = wait_for_background_hooks(Duration::from_secs(10)).await;
        assert!(total >= 1);
        assert!(
            tmp.path().join("bg-done.txt").exists(),
            "background hook must have completed after wait"
        );
    }

    #[test]
    fn test_is_builtin_condition() {
        assert!(HookEngine::is_builtin_condition("is_worktree"));
        assert!(HookEngine::is_builtin_condition("not_worktree"));
        assert!(HookEngine::is_builtin_condition("trigger_is:vcs"));
        assert!(HookEngine::is_builtin_condition("trigger_not:cli"));
        assert!(HookEngine::is_builtin_condition(
            "workspace_matches:^feat/.*"
        ));
        assert!(HookEngine::is_builtin_condition("workspace_is:main"));
        assert!(HookEngine::is_builtin_condition("workspace_not:main"));
        assert!(HookEngine::is_builtin_condition("env_set:FOO"));
        assert!(HookEngine::is_builtin_condition("env_is:FOO=bar"));
        assert!(HookEngine::is_builtin_condition("is_default_workspace"));
        assert!(HookEngine::is_builtin_condition("not_default_workspace"));
        assert!(HookEngine::is_builtin_condition("file_exists:foo.txt"));
        assert!(HookEngine::is_builtin_condition("dir_exists:src"));
        assert!(HookEngine::is_builtin_condition("command_exists:mise"));
        assert!(HookEngine::is_builtin_condition("always"));
        assert!(HookEngine::is_builtin_condition("never"));
        assert!(HookEngine::is_builtin_condition("true"));
        assert!(HookEngine::is_builtin_condition("false"));

        // Shell commands are NOT built-in
        assert!(!HookEngine::is_builtin_condition("test -f foo.txt"));
        assert!(!HookEngine::is_builtin_condition("grep -q bar baz"));
    }

    #[tokio::test]
    async fn test_action_hook_condition_skips() {
        let tmp = tempfile::tempdir().unwrap();
        let mut hooks: HooksConfig = IndexMap::new();
        let mut phase_hooks = IndexMap::new();
        phase_hooks.insert(
            "skip-action".to_string(),
            HookEntry::Action(crate::hooks::ActionHookEntry {
                action: crate::hooks::HookAction::WriteFile {
                    path: "should-not-exist.txt".to_string(),
                    content: "nope".to_string(),
                    mode: crate::hooks::WriteMode::Overwrite,
                },
                working_dir: None,
                continue_on_error: None,
                condition: Some("never".to_string()),
                environment: None,
                background: false,
            }),
        );
        hooks.insert(HookPhase::PostCreate, phase_hooks);

        let engine = HookEngine::new_no_approval(hooks, tmp.path().to_path_buf());
        let result = engine
            .run_phase(&HookPhase::PostCreate, &basic_context())
            .await
            .unwrap();
        assert_eq!(result.skipped, 1);
        assert!(!tmp.path().join("should-not-exist.txt").exists());
    }
}
