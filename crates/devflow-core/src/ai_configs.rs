//! Merge AI tool configuration (`.claude/`, `.cursor/`, …) from a linked
//! worktree back into the main worktree.
//!
//! `.claude/settings.local.json` gets a union-merge of its permission arrays;
//! every other file is copied additively (never overwriting main). Shared by
//! the `devflow sync-ai-configs` CLI command and the `sync-ai-configs` hook
//! action, so the GUI does not need the devflow binary on PATH.

use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::path::Path;

use crate::config::{Config, AI_TOOL_DIRS};

/// What a sync run did.
#[derive(Debug, Default)]
pub struct SyncAiConfigsOutcome {
    /// True when the run was a no-op because it ran in the main worktree.
    pub skipped_in_main: bool,
    /// Directories from which at least one file was copied additively.
    pub synced_dirs: Vec<String>,
    /// Individually merged files (currently `.claude/settings.local.json`).
    pub synced_files: Vec<String>,
    /// Non-fatal problems (unparseable settings JSON etc.).
    pub warnings: Vec<String>,
}

impl SyncAiConfigsOutcome {
    pub fn is_noop(&self) -> bool {
        self.synced_dirs.is_empty() && self.synced_files.is_empty()
    }
}

/// Merge AI tool config dirs from `worktree_dir` into the project's main
/// worktree. `config` supplies `worktree.extra_ai_dirs`.
pub fn sync_ai_configs(config: &Config, worktree_dir: &Path) -> Result<SyncAiConfigsOutcome> {
    let vcs_repo =
        crate::vcs::detect_vcs_provider(worktree_dir).context("Not inside a VCS repository")?;

    let main_dir = vcs_repo
        .main_worktree_dir()
        .unwrap_or_else(|| worktree_dir.to_path_buf());

    let mut outcome = SyncAiConfigsOutcome::default();

    // Don't sync if we're already in the main worktree.
    let canonical_current = worktree_dir
        .canonicalize()
        .unwrap_or_else(|_| worktree_dir.to_path_buf());
    let canonical_main = main_dir.canonicalize().unwrap_or_else(|_| main_dir.clone());
    if canonical_current == canonical_main {
        outcome.skipped_in_main = true;
        return Ok(outcome);
    }

    let extra_dirs: Vec<&str> = config
        .worktree
        .extra_ai_dirs
        .iter()
        .map(|s| s.as_str())
        .collect();
    let all_dirs: Vec<&str> = AI_TOOL_DIRS.iter().copied().chain(extra_dirs).collect();

    for dir_name in &all_dirs {
        let src_dir = worktree_dir.join(dir_name);
        let dst_dir = main_dir.join(dir_name);

        if !src_dir.is_dir() {
            continue;
        }

        if *dir_name == ".claude" {
            // Special handling: union-merge permissions in settings.local.json
            let settings_file = "settings.local.json";
            let src_settings = src_dir.join(settings_file);
            let dst_settings = dst_dir.join(settings_file);

            if src_settings.is_file() {
                match merge_claude_permissions(&src_settings, &dst_settings) {
                    Ok(true) => outcome
                        .synced_files
                        .push(format!("{}/{}", dir_name, settings_file)),
                    Ok(false) => {}
                    Err(e) => outcome.warnings.push(format!(
                        "Failed to merge {}/{}: {}",
                        dir_name, settings_file, e
                    )),
                }
            }

            // Also copy other files/dirs additively
            if let Ok(count) = additive_copy_dir(&src_dir, &dst_dir, &[settings_file]) {
                if count > 0 {
                    outcome.synced_dirs.push(dir_name.to_string());
                }
            }
        } else {
            // For other dirs: additive copy (don't overwrite existing files)
            if let Ok(count) = additive_copy_dir(&src_dir, &dst_dir, &[]) {
                if count > 0 {
                    outcome.synced_dirs.push(dir_name.to_string());
                }
            }
        }
    }

    Ok(outcome)
}

/// Union-merge `.claude/settings.local.json` permission arrays.
///
/// Reads both source and destination, merges `permissions.allow` arrays
/// (deduplicated), writes the result to destination.
fn merge_claude_permissions(src: &Path, dst: &Path) -> Result<bool> {
    let src_content = std::fs::read_to_string(src).context("Failed to read source settings")?;
    let src_json: serde_json::Value =
        serde_json::from_str(&src_content).context("Failed to parse source settings JSON")?;

    let dst_json: serde_json::Value = if dst.is_file() {
        let content =
            std::fs::read_to_string(dst).context("Failed to read destination settings")?;
        serde_json::from_str(&content).context("Failed to parse destination settings JSON")?
    } else {
        serde_json::json!({})
    };

    let merged = merge_json_permissions(&dst_json, &src_json);

    // Only write if something actually changed
    if merged == dst_json {
        return Ok(false);
    }

    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let output = serde_json::to_string_pretty(&merged)?;
    std::fs::write(dst, output)?;
    Ok(true)
}

/// Merge two JSON values, with special handling for `permissions.allow` arrays.
fn merge_json_permissions(
    base: &serde_json::Value,
    overlay: &serde_json::Value,
) -> serde_json::Value {
    match (base, overlay) {
        (serde_json::Value::Object(base_map), serde_json::Value::Object(overlay_map)) => {
            let mut result = base_map.clone();
            for (key, overlay_val) in overlay_map {
                let merged_val = if let Some(base_val) = result.get(key) {
                    if key == "allow" || key == "deny" {
                        // Union-merge arrays
                        merge_arrays(base_val, overlay_val)
                    } else {
                        merge_json_permissions(base_val, overlay_val)
                    }
                } else {
                    overlay_val.clone()
                };
                result.insert(key.clone(), merged_val);
            }
            serde_json::Value::Object(result)
        }
        _ => base.clone(),
    }
}

/// Union-merge two JSON arrays, deduplicating entries.
fn merge_arrays(base: &serde_json::Value, overlay: &serde_json::Value) -> serde_json::Value {
    let mut set = BTreeSet::new();
    let mut result = Vec::new();

    if let serde_json::Value::Array(arr) = base {
        for item in arr {
            let key = item.to_string();
            if set.insert(key) {
                result.push(item.clone());
            }
        }
    }
    if let serde_json::Value::Array(arr) = overlay {
        for item in arr {
            let key = item.to_string();
            if set.insert(key) {
                result.push(item.clone());
            }
        }
    }

    serde_json::Value::Array(result)
}

/// Additively copy files from `src` to `dst` — only copy files that don't
/// exist in `dst`. Skips files listed in `exclude`.
///
/// Returns the number of files copied.
fn additive_copy_dir(src: &Path, dst: &Path, exclude: &[&str]) -> Result<usize> {
    let mut count = 0;
    additive_copy_dir_inner(src, dst, src, exclude, &mut count)?;
    Ok(count)
}

fn additive_copy_dir_inner(
    src_root: &Path,
    dst_root: &Path,
    current_src: &Path,
    exclude: &[&str],
    count: &mut usize,
) -> Result<()> {
    let entries = std::fs::read_dir(current_src)?;
    for entry in entries {
        let entry = entry?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();

        // Check exclusion against relative path from src_root
        let rel = entry
            .path()
            .strip_prefix(src_root)
            .unwrap_or(Path::new(&*name))
            .to_path_buf();
        if exclude.iter().any(|e| rel == Path::new(e)) {
            continue;
        }

        let src_path = entry.path();
        let dst_path = dst_root.join(&rel);

        if src_path.is_dir() {
            additive_copy_dir_inner(src_root, dst_root, &src_path, exclude, count)?;
        } else if src_path.is_file() && !dst_path.exists() {
            if let Some(parent) = dst_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&src_path, &dst_path)?;
            *count += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merge_claude_permissions_union() {
        let base = serde_json::json!({
            "permissions": {
                "allow": ["Read", "Write"],
                "deny": ["Bash"]
            }
        });

        let overlay = serde_json::json!({
            "permissions": {
                "allow": ["Write", "Grep", "Glob"],
                "deny": ["Bash", "Edit"]
            }
        });

        let result = merge_json_permissions(&base, &overlay);
        let allow = result["permissions"]["allow"].as_array().unwrap();
        let deny = result["permissions"]["deny"].as_array().unwrap();

        // Should be union: Read, Write, Grep, Glob (deduplicated)
        assert_eq!(allow.len(), 4);
        assert!(allow.contains(&serde_json::json!("Read")));
        assert!(allow.contains(&serde_json::json!("Write")));
        assert!(allow.contains(&serde_json::json!("Grep")));
        assert!(allow.contains(&serde_json::json!("Glob")));

        // Deny: Bash, Edit
        assert_eq!(deny.len(), 2);
        assert!(deny.contains(&serde_json::json!("Bash")));
        assert!(deny.contains(&serde_json::json!("Edit")));
    }

    #[test]
    fn test_merge_arrays_dedup() {
        let a = serde_json::json!(["a", "b", "c"]);
        let b = serde_json::json!(["b", "c", "d"]);
        let result = merge_arrays(&a, &b);
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 4);
    }

    #[test]
    fn test_merge_empty_base() {
        let base = serde_json::json!({});
        let overlay = serde_json::json!({
            "permissions": {
                "allow": ["Read"]
            }
        });
        let result = merge_json_permissions(&base, &overlay);
        assert_eq!(result["permissions"]["allow"][0], "Read");
    }

    #[test]
    fn test_additive_copy_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");

        // Create source files
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), "a").unwrap();
        std::fs::write(src.join("sub/b.txt"), "b").unwrap();

        // Create destination with existing file
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(dst.join("a.txt"), "existing").unwrap();

        let count = additive_copy_dir(&src, &dst, &[]).unwrap();

        // Should only copy sub/b.txt (a.txt already exists)
        assert_eq!(count, 1);
        assert_eq!(
            std::fs::read_to_string(dst.join("a.txt")).unwrap(),
            "existing"
        );
        assert_eq!(std::fs::read_to_string(dst.join("sub/b.txt")).unwrap(), "b");
    }
}
