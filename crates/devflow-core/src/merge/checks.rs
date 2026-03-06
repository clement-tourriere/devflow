use anyhow::Result;
use regex::Regex;
use std::collections::HashMap;
use std::path::PathBuf;

use super::{CheckSeverity, MergeCheck, MergeCheckResult};
use crate::vcs::VcsProvider;

/// Detects when both source and target add numbered files in the same directory.
///
/// Common for Django/Rails migrations, Alembic revisions, etc.
pub struct SequentialFileCheck {
    pub label: String,
    pub directory_pattern: String,
    pub file_pattern: String,
    pub severity: CheckSeverity,
}

impl MergeCheck for SequentialFileCheck {
    fn name(&self) -> &str {
        &self.label
    }

    fn check(
        &self,
        repo: &dyn VcsProvider,
        source: &str,
        target: &str,
    ) -> Result<MergeCheckResult> {
        let base = repo.merge_base(source, target)?;
        let source_files = repo.changed_files_since(&base, source)?;
        let target_files = repo.changed_files_since(&base, target)?;

        let file_re = Regex::new(&self.file_pattern).unwrap_or_else(|_| Regex::new(".*").unwrap());
        let dir_glob = glob::Pattern::new(&self.directory_pattern)
            .unwrap_or_else(|_| glob::Pattern::new("*").unwrap());

        // Group added files by their parent directory
        let source_dirs = group_matching_files(&source_files, &dir_glob, &file_re);
        let target_dirs = group_matching_files(&target_files, &dir_glob, &file_re);

        // Find directories where both sides added files
        let mut conflict_files = Vec::new();
        for (dir, src_files) in &source_dirs {
            if let Some(tgt_files) = target_dirs.get(dir) {
                for f in src_files {
                    conflict_files.push(f.clone());
                }
                for f in tgt_files {
                    conflict_files.push(f.clone());
                }
            }
        }

        if conflict_files.is_empty() {
            Ok(MergeCheckResult {
                check_name: self.label.clone(),
                passed: true,
                severity: self.severity.clone(),
                message: "No sequential file conflicts detected".to_string(),
                files: vec![],
                suggestion: None,
            })
        } else {
            Ok(MergeCheckResult {
                check_name: self.label.clone(),
                passed: false,
                severity: self.severity.clone(),
                message: format!(
                    "Both branches add numbered files in the same directory ({} files)",
                    conflict_files.len()
                ),
                files: conflict_files,
                suggestion: Some("Renumber the files on one branch before merging".to_string()),
            })
        }
    }
}

fn group_matching_files(
    files: &[PathBuf],
    dir_glob: &glob::Pattern,
    file_re: &Regex,
) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for path in files {
        let parent = path
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        // Normalize directory path for matching: add trailing slash
        let dir_with_slash = if parent.ends_with('/') {
            parent.clone()
        } else {
            format!("{}/", parent)
        };

        if dir_glob.matches(&dir_with_slash) && file_re.is_match(&file_name) {
            map.entry(parent)
                .or_default()
                .push(path.to_string_lossy().to_string());
        }
    }
    map
}

/// Dry-run merge analysis to detect file conflicts without performing the merge.
pub struct GitConflictCheck {
    pub severity: CheckSeverity,
}

impl MergeCheck for GitConflictCheck {
    fn name(&self) -> &str {
        "git-conflicts"
    }

    fn check(
        &self,
        repo: &dyn VcsProvider,
        source: &str,
        _target: &str,
    ) -> Result<MergeCheckResult> {
        // Use merge_analysis via the VcsProvider. We attempt to detect conflicts
        // by checking if the merge base differs from both heads and there are
        // overlapping changed files.
        let base = match repo.merge_base(source, _target) {
            Ok(b) => b,
            Err(_) => {
                return Ok(MergeCheckResult {
                    check_name: "git-conflicts".to_string(),
                    passed: true,
                    severity: self.severity.clone(),
                    message: "Could not determine merge base — skipping conflict check".to_string(),
                    files: vec![],
                    suggestion: None,
                });
            }
        };

        let source_files = repo.changed_files_since(&base, source)?;
        let target_files = repo.changed_files_since(&base, _target)?;

        // Find files changed on both sides (potential conflicts)
        let source_set: std::collections::HashSet<_> = source_files.iter().collect();
        let conflicts: Vec<String> = target_files
            .iter()
            .filter(|f| source_set.contains(f))
            .map(|f| f.to_string_lossy().to_string())
            .collect();

        if conflicts.is_empty() {
            Ok(MergeCheckResult {
                check_name: "git-conflicts".to_string(),
                passed: true,
                severity: self.severity.clone(),
                message: "No conflicting files detected".to_string(),
                files: vec![],
                suggestion: None,
            })
        } else {
            Ok(MergeCheckResult {
                check_name: "git-conflicts".to_string(),
                passed: false,
                severity: self.severity.clone(),
                message: format!(
                    "{} file(s) changed on both branches — potential merge conflicts",
                    conflicts.len()
                ),
                files: conflicts,
                suggestion: Some("Review the overlapping changes before merging".to_string()),
            })
        }
    }
}

/// Run a user-defined command as a merge check.
pub struct HookCheck {
    pub label: String,
    pub command: String,
    pub severity: CheckSeverity,
}

impl MergeCheck for HookCheck {
    fn name(&self) -> &str {
        &self.label
    }

    fn check(
        &self,
        repo: &dyn VcsProvider,
        _source: &str,
        _target: &str,
    ) -> Result<MergeCheckResult> {
        let output = std::process::Command::new("sh")
            .args(["-c", &self.command])
            .current_dir(repo.repo_root())
            .output()?;

        if output.status.success() {
            Ok(MergeCheckResult {
                check_name: self.label.clone(),
                passed: true,
                severity: self.severity.clone(),
                message: "Check passed".to_string(),
                files: vec![],
                suggestion: None,
            })
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Ok(MergeCheckResult {
                check_name: self.label.clone(),
                passed: false,
                severity: self.severity.clone(),
                message: format!("Check failed: {}", stderr.trim()),
                files: vec![],
                suggestion: Some(format!("Fix the issue and re-run: {}", self.command)),
            })
        }
    }
}
