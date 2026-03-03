use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use super::approval::ApprovalStore;
use super::template::TemplateEngine;
use super::{HookContext, HookEntry, HookPhase, HooksConfig};

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
        let (command_template, extended) = match entry {
            HookEntry::Simple(cmd) => (cmd.as_str(), None),
            HookEntry::Extended(ext) => (ext.command.as_str(), Some(ext)),
        };

        // Check condition
        if let Some(ext) = &extended {
            if let Some(ref condition) = ext.condition {
                let rendered_condition = self.template_engine.render(condition, context)?;

                // Shell-based conditions are executable code too, so they must be
                // approved before evaluation when approvals are enabled.
                if self.require_approval && Self::condition_uses_shell(&rendered_condition) {
                    if self.non_interactive && self.project_key.is_none() {
                        anyhow::bail!(
                            "Cannot evaluate hook condition '{}' in non-interactive mode without a project key",
                            rendered_condition
                        );
                    }

                    if let Some(ref project_key) = self.project_key {
                        let approval_command = format!("condition: {}", rendered_condition);
                        let mut store = ApprovalStore::load().unwrap_or_default();
                        if !store.is_approved(project_key, &approval_command) {
                            if self.non_interactive {
                                anyhow::bail!(
                                    "Hook condition for '{}' requires approval in non-interactive mode: {}",
                                    name,
                                    rendered_condition
                                );
                            }
                            match Self::prompt_hook_approval(
                                &format!("{} (condition)", name),
                                &rendered_condition,
                            ) {
                                HookApprovalChoice::ApproveAlways => {
                                    if let Err(e) = store.approve(project_key, &approval_command) {
                                        log::warn!(
                                            "Failed to persist hook condition approval: {}",
                                            e
                                        );
                                    }
                                }
                                HookApprovalChoice::ApproveOnce => {
                                    // Run this time but don't persist
                                }
                                HookApprovalChoice::Deny => {
                                    return Ok(HookOutcome::Skipped(
                                        "condition command not approved by user".to_string(),
                                    ));
                                }
                            }
                        }
                    }
                }

                if !self.evaluate_condition(&rendered_condition)? {
                    return Ok(HookOutcome::Skipped(format!(
                        "condition '{}' was false",
                        rendered_condition
                    )));
                }
            }
        }

        // Render the command template
        let rendered_command = self.template_engine.render(command_template, context)?;

        // Check approval
        if self.require_approval {
            if self.non_interactive && self.project_key.is_none() {
                anyhow::bail!(
                    "Cannot evaluate hook '{}' in non-interactive mode without a project key",
                    name
                );
            }

            if let Some(ref project_key) = self.project_key {
                let mut store = ApprovalStore::load().unwrap_or_default();
                if !store.is_approved(project_key, &rendered_command) {
                    if self.non_interactive {
                        anyhow::bail!(
                            "Hook '{}' requires approval in non-interactive mode: {}",
                            name,
                            rendered_command
                        );
                    }
                    match Self::prompt_hook_approval(name, &rendered_command) {
                        HookApprovalChoice::ApproveAlways => {
                            if let Err(e) = store.approve(project_key, &rendered_command) {
                                log::warn!("Failed to persist hook approval: {}", e);
                            }
                        }
                        HookApprovalChoice::ApproveOnce => {
                            // Run this time but don't persist
                        }
                        HookApprovalChoice::Deny => {
                            return Ok(HookOutcome::Skipped("not approved by user".to_string()));
                        }
                    }
                }
            }
        }

        // Determine if this should run in the background
        let run_background = extended.map(|e| e.background).unwrap_or(false) || !phase_blocking;

        if run_background {
            let cmd = rendered_command.clone();
            let wd = self.working_dir.clone();
            let hook_name = name.to_string();
            let env_vars = extended.and_then(|e| e.environment.clone());
            let ctx_clone = context.clone();
            let te = TemplateEngine::new();
            let quiet_output = self.quiet_output;

            tokio::spawn(async move {
                match execute_shell_command(
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

            return Ok(HookOutcome::Background);
        }

        // Blocking execution
        if !self.quiet_output {
            println!("  Running: {} ({})", name, rendered_command);
        }

        let working_dir = if let Some(ext) = &extended {
            ext.working_dir
                .as_ref()
                .map(|wd| self.working_dir.join(wd))
                .unwrap_or_else(|| self.working_dir.clone())
        } else {
            self.working_dir.clone()
        };

        let env_vars = extended.and_then(|e| e.environment.clone());

        execute_shell_command(
            &rendered_command,
            &working_dir,
            env_vars.as_ref(),
            context,
            &self.template_engine,
            !self.quiet_output,
        )?;

        Ok(HookOutcome::Succeeded)
    }

    fn evaluate_condition(&self, rendered: &str) -> Result<bool> {
        if let Some(file_path) = rendered.strip_prefix("file_exists:") {
            let full_path = self.working_dir.join(file_path.trim());
            Ok(full_path.exists())
        } else if let Some(dir_path) = rendered.strip_prefix("dir_exists:") {
            let full_path = self.working_dir.join(dir_path.trim());
            Ok(full_path.is_dir())
        } else if rendered == "always" || rendered == "true" {
            Ok(true)
        } else if rendered == "never" || rendered == "false" {
            Ok(false)
        } else {
            // Treat unknown conditions as shell commands: exit 0 = true
            let output = Command::new("sh")
                .args(["-c", rendered])
                .current_dir(&self.working_dir)
                .output()
                .with_context(|| format!("Failed to evaluate condition: {}", rendered))?;
            Ok(output.status.success())
        }
    }

    fn condition_uses_shell(rendered: &str) -> bool {
        !rendered.starts_with("file_exists:")
            && !rendered.starts_with("dir_exists:")
            && rendered != "always"
            && rendered != "true"
            && rendered != "never"
            && rendered != "false"
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

/// Execute a shell command, returning an error if it fails.
fn execute_shell_command(
    command: &str,
    working_dir: &PathBuf,
    environment: Option<&HashMap<String, String>>,
    context: &HookContext,
    template_engine: &TemplateEngine,
    print_stdout: bool,
) -> Result<()> {
    let mut cmd = if cfg!(target_os = "windows") {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", command]);
        cmd
    } else {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", command]);
        cmd
    };

    cmd.current_dir(working_dir);

    // Set template-rendered environment variables
    if let Some(env_vars) = environment {
        for (key, value_template) in env_vars {
            let rendered_value = template_engine
                .render(value_template, context)
                .unwrap_or_else(|_| value_template.clone());
            cmd.env(key, rendered_value);
        }
    }

    let output = cmd
        .output()
        .with_context(|| format!("Failed to execute hook command: {}", command))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::bail!(
            "Hook command failed (exit {}): {}\nstdout: {}\nstderr: {}",
            output.status.code().unwrap_or(-1),
            command,
            stdout.trim(),
            stderr.trim()
        );
    }

    // Print stdout if non-empty
    let stdout = String::from_utf8_lossy(&output.stdout);
    if print_stdout && !stdout.trim().is_empty() {
        println!("{}", stdout.trim());
    }

    Ok(())
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
}
