use std::path::Path;

use crate::config::Config;
use crate::services::DoctorCheck;
use crate::state::LocalStateManager;
use crate::vcs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DoctorSummary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
}

impl DoctorSummary {
    pub fn from_checks(checks: &[DoctorCheck]) -> Self {
        let total = checks.len();
        let passed = checks.iter().filter(|check| check.available).count();
        Self {
            total,
            passed,
            failed: total.saturating_sub(passed),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DoctorNextStep {
    pub check: String,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// Convert failed doctor checks into actionable remediation hints for CLI,
/// TUI, GUI, and automation surfaces.
pub fn doctor_next_steps(checks: &[DoctorCheck]) -> Vec<DoctorNextStep> {
    checks
        .iter()
        .filter(|check| !check.available)
        .map(next_step_for_check)
        .collect()
}

fn next_step_for_check(check: &DoctorCheck) -> DoctorNextStep {
    let (action, command) = match check.name.as_str() {
        "Config file" => (
            "Create a devflow configuration file for this project.",
            Some("devflow init"),
        ),
        "Config syntax" => ("Fix .devflow.yml syntax, then rerun doctor.", None),
        "VCS repository" => (
            "Run devflow from a Git or Jujutsu repository.",
            Some("git init"),
        ),
        "VCS hooks" if check.detail.contains("shell integration") => (
            "Enable shell integration for VCS events that cannot use native hooks.",
            Some("devflow shell-init bash"),
        ),
        "VCS hooks" => (
            "Install devflow-managed VCS hooks for automatic workspace switching.",
            Some("devflow install-hooks"),
        ),
        "Worktree metadata" => (
            "Prune stale VCS worktree metadata.",
            Some("git worktree prune"),
        ),
        "Workspace registry paths" => (
            "Inspect stale workspace registry entries and remove workspaces that no longer exist.",
            Some("devflow gc --list"),
        ),
        "Workspace filter regex" => ("Fix git.workspace_filter_regex in .devflow.yml.", None),
        "Agent guides" => (
            "Generate or refresh project-specific agent guide files.",
            Some("devflow agent skill"),
        ),
        "Docker" => (
            "Start Docker and verify the daemon is reachable.",
            Some("docker info"),
        ),
        "Service doctor" => (
            "Run service diagnostics and inspect the reported provider error.",
            Some("devflow service doctor"),
        ),
        "Data root" => (
            "Ensure the configured service data root exists and is writable.",
            None,
        ),
        "Port range" => (
            "Choose an unprivileged service port range starting at 1024 or higher.",
            None,
        ),
        "Configured storage preference" => (
            "Choose an available storage driver or set local.storage to copy.",
            None,
        ),
        name if name.starts_with("Storage:") => (
            "Use an available storage driver or set local.storage to copy.",
            None,
        ),
        _ => (
            "Review the failing check detail and rerun devflow doctor after fixing it.",
            None,
        ),
    };

    DoctorNextStep {
        check: check.name.clone(),
        action: action.to_string(),
        command: command.map(str::to_string),
    }
}

fn check(name: &str, available: bool, detail: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        name: name.to_string(),
        available,
        detail: detail.into(),
    }
}

/// Project-level diagnostics shared by CLI, TUI, and GUI surfaces.
pub fn project_doctor_checks(
    config: &Config,
    config_path: Option<&Path>,
    project_dir: &Path,
) -> Vec<DoctorCheck> {
    let mut checks = Vec::new();

    checks.push(match config_path {
        Some(path) => check("Config file", true, path.display().to_string()),
        None => check(
            "Config file",
            false,
            "not found (run devflow init to create .devflow.yml)",
        ),
    });

    let vcs_repo = vcs::detect_vcs_provider(project_dir);
    match &vcs_repo {
        Ok(vcs) => checks.push(check(
            "VCS repository",
            true,
            format!("Detected {} repository", vcs.provider_name()),
        )),
        Err(e) => checks.push(check("VCS repository", false, e.to_string())),
    }

    match &vcs_repo {
        Ok(vcs) => {
            let hook_paths = vcs.hook_paths();
            let managed_hooks = hook_paths
                .iter()
                .filter(|path| path.exists())
                .filter(|path| vcs.is_devflow_hook(path).unwrap_or(false))
                .count();
            if managed_hooks > 0 {
                checks.push(check(
                    "VCS hooks",
                    true,
                    format!("{} managed hook(s) installed", managed_hooks),
                ));
            } else if hook_paths.is_empty() {
                checks.push(check(
                    "VCS hooks",
                    false,
                    "no native hook directory detected; use devflow shell-init if your VCS needs shell integration",
                ));
            } else {
                checks.push(check(
                    "VCS hooks",
                    false,
                    "not installed (run devflow install-hooks)",
                ));
            }
        }
        Err(_) => checks.push(check(
            "VCS hooks",
            false,
            "not checked because no VCS repository was detected",
        )),
    }

    if let Ok(ref vcs) = vcs_repo {
        if vcs.supports_worktrees() {
            match vcs.list_worktrees() {
                Ok(worktrees) => {
                    let stale: Vec<_> = worktrees
                        .iter()
                        .filter(|wt| !wt.is_main && !wt.path.exists())
                        .collect();

                    if stale.is_empty() {
                        checks.push(check("Worktree metadata", true, "No stale entries"));
                    } else {
                        let examples = stale
                            .iter()
                            .take(3)
                            .map(|wt| wt.path.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ");
                        checks.push(check(
                            "Worktree metadata",
                            false,
                            format!(
                                "{} stale entr{} (run git worktree prune): {}",
                                stale.len(),
                                if stale.len() == 1 { "y" } else { "ies" },
                                examples
                            ),
                        ));
                    }
                }
                Err(e) => checks.push(check(
                    "Worktree metadata",
                    false,
                    format!("Inspection failed: {}", e),
                )),
            }
        }
    }

    match LocalStateManager::new() {
        Ok(state) => {
            let missing: Vec<_> = state
                .get_workspaces_by_dir(project_dir)
                .into_iter()
                .filter_map(|workspace| workspace.worktree_path.map(|p| (workspace.name, p)))
                .filter(|(_, path)| !Path::new(path).exists())
                .collect();

            if missing.is_empty() {
                checks.push(check("Workspace registry paths", true, "No stale entries"));
            } else {
                let examples = missing
                    .iter()
                    .take(3)
                    .map(|(workspace, path)| format!("{} -> {}", workspace, path))
                    .collect::<Vec<_>>()
                    .join(", ");
                checks.push(check(
                    "Workspace registry paths",
                    false,
                    format!(
                        "{} stale entr{}: {}",
                        missing.len(),
                        if missing.len() == 1 { "y" } else { "ies" },
                        examples
                    ),
                ));
            }
        }
        Err(e) => checks.push(check(
            "Workspace registry paths",
            false,
            format!("Inspection failed: {}", e),
        )),
    }

    if let Some(ref regex_pattern) = config.git.workspace_filter_regex {
        match regex::Regex::new(regex_pattern) {
            Ok(_) => checks.push(check("Workspace filter regex", true, "Valid")),
            Err(e) => checks.push(check("Workspace filter regex", false, e.to_string())),
        }
    }

    let guide_status = crate::agent::check_agent_skills_installed(project_dir);
    checks.push(check(
        "Agent guides",
        guide_status.installed,
        if guide_status.installed {
            format!(
                "{} guide file(s) current",
                guide_status.installed_skills.len()
            )
        } else if !guide_status.installed_skills.is_empty() {
            if !guide_status.stale_skills.is_empty() && !guide_status.missing_skills.is_empty() {
                format!(
                    "{} generated, {} stale, {} missing",
                    guide_status.installed_skills.len(),
                    guide_status.stale_skills.len(),
                    guide_status.missing_skills.len()
                )
            } else if !guide_status.stale_skills.is_empty() {
                format!(
                    "{} generated, {} stale; regenerate guides",
                    guide_status.installed_skills.len(),
                    guide_status.stale_skills.len()
                )
            } else {
                format!(
                    "{} generated, {} missing",
                    guide_status.installed_skills.len(),
                    guide_status.missing_skills.len()
                )
            }
        } else {
            "Guide files not generated (run devflow agent skill)".to_string()
        },
    ));

    checks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_summary_counts_pass_fail() {
        let checks = vec![
            check("ok", true, "done"),
            check("bad", false, "missing"),
            check("ok2", true, "done"),
        ];
        assert_eq!(
            DoctorSummary::from_checks(&checks),
            DoctorSummary {
                total: 3,
                passed: 2,
                failed: 1,
            }
        );
    }

    #[test]
    fn doctor_next_steps_are_actionable_for_common_failures() {
        let checks = vec![
            check("Config file", false, "not found"),
            check("Agent guides", false, "missing"),
            check("Docker", false, "daemon unavailable"),
            check("ok", true, "done"),
        ];

        let next_steps = doctor_next_steps(&checks);
        assert_eq!(next_steps.len(), 3);
        assert_eq!(next_steps[0].command.as_deref(), Some("devflow init"));
        assert_eq!(
            next_steps[1].command.as_deref(),
            Some("devflow agent skill")
        );
        assert_eq!(next_steps[2].command.as_deref(), Some("docker info"));
    }

    #[test]
    fn project_doctor_reports_config_and_agent_guides() {
        let tmp =
            std::env::temp_dir().join(format!("devflow-doctor-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let checks = project_doctor_checks(&Config::default(), None, &tmp);
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(checks.iter().any(|c| c.name == "Config file"));
        assert!(checks.iter().any(|c| c.name == "Agent guides"));
    }
}
