use anyhow::{Context, Result};
use devflow_core::config::{Config, AI_TOOL_DIRS};
use devflow_core::vcs;
use std::collections::BTreeSet;
use std::path::Path;

/// Handle `devflow sync-ai-configs` — merge AI tool configs from current worktree
/// back to the main worktree.
pub(super) fn handle_sync_ai_configs(json_output: bool) -> Result<()> {
    let config_path = Config::find_config_file()?;
    let config = match &config_path {
        Some(p) => Config::from_file(p)?,
        None => Config::default(),
    };

    let vcs_repo = vcs::detect_vcs_provider(".")
        .context("Not inside a VCS repository")?;

    let current_dir = std::env::current_dir()?;

    let main_dir = vcs_repo
        .main_worktree_dir()
        .unwrap_or_else(|| current_dir.clone());

    // Don't sync if we're already in the main worktree
    let canonical_current = current_dir.canonicalize().unwrap_or(current_dir.clone());
    let canonical_main = main_dir.canonicalize().unwrap_or(main_dir.clone());
    if canonical_current == canonical_main {
        if json_output {
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "status": "skipped",
                "reason": "already in main worktree",
            }))?);
        } else {
            println!("Already in the main worktree, nothing to sync.");
        }
        return Ok(());
    }

    // Collect AI dirs to sync
    let wt_config = config.worktree.as_ref();
    let extra_dirs: Vec<&str> = wt_config
        .map(|wt| wt.extra_ai_dirs.iter().map(|s| s.as_str()).collect())
        .unwrap_or_default();
    let all_dirs: Vec<&str> = AI_TOOL_DIRS.iter().copied().chain(extra_dirs).collect();

    let mut synced_dirs: Vec<String> = Vec::new();
    let mut synced_files: Vec<String> = Vec::new();

    for dir_name in &all_dirs {
        let src_dir = current_dir.join(dir_name);
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
                    Ok(merged) => {
                        if merged {
                            synced_files.push(format!("{}/{}", dir_name, settings_file));
                        }
                    }
                    Err(e) => {
                        log::warn!("Failed to merge {}/{}: {}", dir_name, settings_file, e);
                        if !json_output {
                            eprintln!("Warning: Failed to merge {}/{}: {}", dir_name, settings_file, e);
                        }
                    }
                }
            }

            // Also copy other files/dirs additively
            if let Ok(count) = additive_copy_dir(&src_dir, &dst_dir, &[settings_file]) {
                if count > 0 {
                    synced_dirs.push(dir_name.to_string());
                }
            }
        } else {
            // For other dirs: additive copy (don't overwrite existing files)
            if let Ok(count) = additive_copy_dir(&src_dir, &dst_dir, &[]) {
                if count > 0 {
                    synced_dirs.push(dir_name.to_string());
                }
            }
        }
    }

    if json_output {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "status": "ok",
            "synced_dirs": synced_dirs,
            "synced_files": synced_files,
        }))?);
    } else if synced_dirs.is_empty() && synced_files.is_empty() {
        println!("No AI configs to sync.");
    } else {
        if !synced_files.is_empty() {
            for f in &synced_files {
                println!("Merged: {}", f);
            }
        }
        if !synced_dirs.is_empty() {
            for d in &synced_dirs {
                println!("Synced: {}/", d);
            }
        }
    }

    Ok(())
}

/// Union-merge `.claude/settings.local.json` permission arrays.
///
/// Reads both source and destination, merges `permissions.allow` arrays
/// (deduplicated), writes the result to destination.
fn merge_claude_permissions(src: &Path, dst: &Path) -> Result<bool> {
    let src_content = std::fs::read_to_string(src)
        .context("Failed to read source settings")?;
    let src_json: serde_json::Value = serde_json::from_str(&src_content)
        .context("Failed to parse source settings JSON")?;

    let dst_json: serde_json::Value = if dst.is_file() {
        let content = std::fs::read_to_string(dst)
            .context("Failed to read destination settings")?;
        serde_json::from_str(&content)
            .context("Failed to parse destination settings JSON")?
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
fn merge_json_permissions(base: &serde_json::Value, overlay: &serde_json::Value) -> serde_json::Value {
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
        let rel = entry.path().strip_prefix(src_root)
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
        let tmp = std::env::temp_dir().join("devflow_test_additive_copy");
        let _ = std::fs::remove_dir_all(&tmp);
        let src = tmp.join("src");
        let dst = tmp.join("dst");

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
        assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "existing");
        assert_eq!(std::fs::read_to_string(dst.join("sub/b.txt")).unwrap(), "b");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
