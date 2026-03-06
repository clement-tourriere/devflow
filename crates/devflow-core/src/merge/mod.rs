pub mod checks;
pub mod train;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::vcs::VcsProvider;

// ── Merge Check Trait ───────────────────────────────────────────────────────

/// Severity level for merge check results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckSeverity {
    /// Blocks the merge.
    Error,
    /// Advisory only — does not block.
    Warning,
}

/// Result of a single merge check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeCheckResult {
    pub check_name: String,
    pub passed: bool,
    pub severity: CheckSeverity,
    pub message: String,
    pub files: Vec<String>,
    pub suggestion: Option<String>,
}

/// Aggregate report from all merge readiness checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeReadinessReport {
    pub source: String,
    pub target: String,
    /// `true` if no Error-severity check failed.
    pub ready: bool,
    pub checks: Vec<MergeCheckResult>,
}

/// A single merge readiness check.
pub trait MergeCheck: Send + Sync {
    fn name(&self) -> &str;
    fn check(
        &self,
        repo: &dyn VcsProvider,
        source: &str,
        target: &str,
    ) -> Result<MergeCheckResult>;
}

/// Run all configured merge checks and return a readiness report.
pub fn run_checks(
    checks: &[Box<dyn MergeCheck>],
    repo: &dyn VcsProvider,
    source: &str,
    target: &str,
) -> MergeReadinessReport {
    let mut results = Vec::new();

    for check in checks {
        match check.check(repo, source, target) {
            Ok(result) => results.push(result),
            Err(e) => results.push(MergeCheckResult {
                check_name: check.name().to_string(),
                passed: false,
                severity: CheckSeverity::Error,
                message: format!("Check failed to run: {}", e),
                files: vec![],
                suggestion: None,
            }),
        }
    }

    let ready = !results
        .iter()
        .any(|r| !r.passed && r.severity == CheckSeverity::Error);

    MergeReadinessReport {
        source: source.to_string(),
        target: target.to_string(),
        ready,
        checks: results,
    }
}

/// Build the list of merge checks from configuration.
pub fn build_checks_from_config(
    config: &crate::config::MergeConfig,
) -> Vec<Box<dyn MergeCheck>> {
    let mut result: Vec<Box<dyn MergeCheck>> = Vec::new();

    for check_config in &config.checks {
        match check_config.check_type.as_str() {
            "sequential-files" => {
                let dir_pattern = check_config
                    .directory_pattern
                    .clone()
                    .unwrap_or_else(|| "*/migrations/".to_string());
                let file_pattern = check_config
                    .file_pattern
                    .clone()
                    .unwrap_or_else(|| r"^\d{4}_.*\.py$".to_string());
                let severity = check_config
                    .severity
                    .as_deref()
                    .map(|s| match s {
                        "warning" => CheckSeverity::Warning,
                        _ => CheckSeverity::Error,
                    })
                    .unwrap_or(CheckSeverity::Error);
                let label = check_config
                    .label
                    .clone()
                    .unwrap_or_else(|| "Sequential file conflict".to_string());
                result.push(Box::new(checks::SequentialFileCheck {
                    label,
                    directory_pattern: dir_pattern,
                    file_pattern,
                    severity,
                }));
            }
            "git-conflicts" => {
                let severity = check_config
                    .severity
                    .as_deref()
                    .map(|s| match s {
                        "warning" => CheckSeverity::Warning,
                        _ => CheckSeverity::Error,
                    })
                    .unwrap_or(CheckSeverity::Error);
                result.push(Box::new(checks::GitConflictCheck { severity }));
            }
            "hook" => {
                if let Some(ref command) = check_config.command {
                    let severity = check_config
                        .severity
                        .as_deref()
                        .map(|s| match s {
                            "warning" => CheckSeverity::Warning,
                            _ => CheckSeverity::Error,
                        })
                        .unwrap_or(CheckSeverity::Warning);
                    let label = check_config
                        .label
                        .clone()
                        .unwrap_or_else(|| command.clone());
                    result.push(Box::new(checks::HookCheck {
                        label,
                        command: command.clone(),
                        severity,
                    }));
                }
            }
            _ => {
                log::warn!(
                    "Unknown merge check type: '{}'",
                    check_config.check_type
                );
            }
        }
    }

    result
}

// ── Rebase Result ───────────────────────────────────────────────────────────

/// Result of a rebase operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebaseResult {
    pub success: bool,
    pub commits_replayed: usize,
    pub conflicts: bool,
    pub conflict_files: Vec<String>,
}

// ── Cascade Report ──────────────────────────────────────────────────────────

/// Report on workspaces affected by a merge into their parent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CascadeReport {
    pub affected_children: Vec<String>,
    pub needs_rebase: Vec<CascadeRebaseNeeded>,
}

/// A workspace that needs rebasing after a parent merged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CascadeRebaseNeeded {
    pub workspace: String,
    pub reason: String,
}

/// Build a cascade report after merging `source` into `target`.
///
/// Finds children of `source` in the workspace registry and runs merge
/// readiness checks against the updated `target`.
pub fn build_cascade_report(
    repo: &dyn VcsProvider,
    project_path: &std::path::Path,
    source: &str,
    target: &str,
    merge_config: Option<&crate::config::MergeConfig>,
) -> Result<CascadeReport> {
    let state_mgr = crate::state::LocalStateManager::new()?;
    let workspaces = state_mgr.get_workspaces_by_dir(project_path);

    let children: Vec<String> = workspaces
        .iter()
        .filter(|w| w.parent.as_deref() == Some(source))
        .map(|w| w.name.clone())
        .collect();

    let mut needs_rebase = Vec::new();

    if let Some(merge_cfg) = merge_config {
        let checks = build_checks_from_config(merge_cfg);
        for child in &children {
            let report = run_checks(&checks, repo, child, target);
            if !report.ready {
                let reasons: Vec<String> = report
                    .checks
                    .iter()
                    .filter(|c| !c.passed && c.severity == CheckSeverity::Error)
                    .map(|c| c.message.clone())
                    .collect();
                needs_rebase.push(CascadeRebaseNeeded {
                    workspace: child.clone(),
                    reason: reasons.join("; "),
                });
            }
        }
    }

    Ok(CascadeReport {
        affected_children: children,
        needs_rebase,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_readiness_report_ready_when_all_pass() {
        let results = vec![MergeCheckResult {
            check_name: "test".to_string(),
            passed: true,
            severity: CheckSeverity::Error,
            message: "OK".to_string(),
            files: vec![],
            suggestion: None,
        }];

        let report = MergeReadinessReport {
            source: "feature".to_string(),
            target: "main".to_string(),
            ready: !results
                .iter()
                .any(|r| !r.passed && r.severity == CheckSeverity::Error),
            checks: results,
        };

        assert!(report.ready);
    }

    #[test]
    fn test_readiness_report_not_ready_on_error() {
        let results = vec![MergeCheckResult {
            check_name: "conflict".to_string(),
            passed: false,
            severity: CheckSeverity::Error,
            message: "Conflicts detected".to_string(),
            files: vec!["foo.rs".to_string()],
            suggestion: Some("Resolve conflicts first".to_string()),
        }];

        let report = MergeReadinessReport {
            source: "feature".to_string(),
            target: "main".to_string(),
            ready: !results
                .iter()
                .any(|r| !r.passed && r.severity == CheckSeverity::Error),
            checks: results,
        };

        assert!(!report.ready);
    }

    #[test]
    fn test_readiness_report_ready_on_warning_only() {
        let results = vec![MergeCheckResult {
            check_name: "test".to_string(),
            passed: false,
            severity: CheckSeverity::Warning,
            message: "Tests failing".to_string(),
            files: vec![],
            suggestion: None,
        }];

        let report = MergeReadinessReport {
            source: "feature".to_string(),
            target: "main".to_string(),
            ready: !results
                .iter()
                .any(|r| !r.passed && r.severity == CheckSeverity::Error),
            checks: results,
        };

        assert!(report.ready);
    }
}
