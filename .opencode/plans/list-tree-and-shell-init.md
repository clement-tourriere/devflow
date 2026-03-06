# Plan: `devflow list` Tree View + Shell Init Hint

## Problem

1. **`devflow list` shows flat workspaces** — `print_enriched_branch_list` (cli.rs:1137) never reads the workspace registry and has no parent-child logic. The TUI has full tree rendering but the CLI list does not.

2. **`devflow switch` doesn't cd into worktree** — This is by design (a child process can't change parent shell's cwd). The `DEVFLOW_CD=` protocol requires `eval "$(devflow shell-init)"` in `.zshrc`, but `devflow init` doesn't mention this.

## Changes

### File: `src/cli.rs`

#### Change 1: Remove dead `print_branch_tree` (lines 1076-1132)

Delete the entire `#[allow(dead_code)]` function `print_branch_tree`. It's superseded by the new tree-rendering logic.

#### Change 2: Rewrite `print_enriched_branch_list` (lines 1137-1222)

Replace the flat list with a tree renderer that:

- **Signature change**: Add `config_path: &Option<PathBuf>` parameter
- **Load workspace registry**: Use `LocalStateManager::new()` + `get_workspaces(path)` to get `HashMap<String, Option<String>>` (name -> parent)
- **Build parent map**: From registry parents (primary) and service-level parents (fallback), filtering to only parents that exist in the known workspace set
- **Build children map**: `HashMap<&str, Vec<&str>>` from the parent map, sort children alphabetically
- **Find roots**: Branches with no parent or parent not in the known set
- **Sort roots**: Default workspace first, then current, then alphabetical
- **DFS tree print**: Recursive `print_node()` that renders with box-drawing chars (`├─`, `└─`, `│`), keeping the existing enrichment (current `*` marker, service state, worktree path)

For root nodes: `* main  [service: running, worktree: /path]`
For children:   `  ├─ feature-a  [worktree: /path]`
For nested:     `  │  └─ feature-a-sub  [worktree: /path]`

Full replacement code:

```rust
/// Print an enriched workspace list as a tree, showing git workspaces, worktree paths, and service status.
///
/// Unifies information from the VCS provider, the service provider, and the
/// workspace registry (for parent-child relationships) into a single tree view.
fn print_enriched_branch_list(
    service_branches: &[services::WorkspaceInfo],
    config: &Config,
    config_path: &Option<PathBuf>,
) {
    use std::collections::{HashMap, HashSet};

    // Gather VCS + worktree info
    let vcs_provider = vcs::detect_vcs_provider(".").ok();
    let git_branches: Vec<crate::vcs::WorkspaceInfo> = vcs_provider
        .as_ref()
        .and_then(|r| r.list_workspaces().ok())
        .unwrap_or_default();
    let worktrees: Vec<crate::vcs::WorktreeInfo> = vcs_provider
        .as_ref()
        .and_then(|r| r.list_worktrees().ok())
        .unwrap_or_default();
    let current_git = vcs_provider
        .as_ref()
        .and_then(|r| r.current_workspace().ok().flatten());

    // Build a set of service workspace names for quick lookup
    let service_names: HashSet<&str> = service_branches.iter().map(|b| b.name.as_str()).collect();

    // Build a worktree lookup: workspace name -> path
    let wt_lookup: HashMap<String, PathBuf> = worktrees
        .iter()
        .filter_map(|wt| wt.workspace.as_ref().map(|b| (b.clone(), wt.path.clone())))
        .collect();

    // Load the workspace registry for parent-child info
    let registry: HashMap<String, Option<String>> = config_path
        .as_ref()
        .and_then(|path| {
            LocalStateManager::new().ok().map(|state| {
                state
                    .get_workspaces(path)
                    .into_iter()
                    .map(|b| (b.name, b.parent))
                    .collect()
            })
        })
        .unwrap_or_default();

    // Collect all workspace names (union of git workspaces + service workspaces)
    let mut all_names: Vec<String> = Vec::new();
    let mut seen = HashSet::new();

    for gb in &git_branches {
        if seen.insert(gb.name.clone()) {
            all_names.push(gb.name.clone());
        }
    }
    for sb in service_branches {
        let normalized = &sb.name;
        if seen.insert(normalized.clone()) {
            all_names.push(normalized.clone());
        }
    }

    if all_names.is_empty() {
        println!("  (none)");
        return;
    }

    // Build parent map: child_name -> parent_name
    // Sources: 1) service-level parent, 2) registry parent (takes precedence)
    let mut parent_map: HashMap<&str, &str> = HashMap::new();

    for sb in service_branches {
        if let Some(ref parent) = sb.parent_workspace {
            if seen.contains(parent.as_str()) {
                parent_map.insert(sb.name.as_str(), parent.as_str());
            }
        }
    }
    for name in &all_names {
        if let Some(Some(ref parent)) = registry.get(name.as_str()) {
            if seen.contains(parent.as_str()) {
                parent_map.insert(name.as_str(), parent.as_str());
            }
        }
    }

    // Build children map
    let mut children_map: HashMap<&str, Vec<&str>> = HashMap::new();
    for (child, parent) in &parent_map {
        children_map.entry(parent).or_default().push(child);
    }
    // Sort children alphabetically for deterministic output
    for kids in children_map.values_mut() {
        kids.sort();
    }

    // Find root nodes (no parent, or parent not in the known set)
    let mut roots: Vec<&str> = all_names
        .iter()
        .filter(|name| !parent_map.contains_key(name.as_str()))
        .map(|s| s.as_str())
        .collect();

    // Sort roots: default workspace first, then current, then alphabetical
    roots.sort_by(|a, b| {
        let a_default = git_branches.iter().any(|gb| gb.name == *a && gb.is_default);
        let b_default = git_branches.iter().any(|gb| gb.name == *b && gb.is_default);
        if a_default != b_default {
            return b_default.cmp(&a_default);
        }
        let a_current = current_git.as_deref() == Some(*a);
        let b_current = current_git.as_deref() == Some(*b);
        if a_current != b_current {
            return b_current.cmp(&a_current);
        }
        a.cmp(b)
    });

    // Recursive tree printer
    fn print_node(
        name: &str,
        prefix: &str,
        connector: &str,
        children_map: &HashMap<&str, Vec<&str>>,
        current_git: &Option<String>,
        service_branches: &[services::WorkspaceInfo],
        service_names: &HashSet<&str>,
        wt_lookup: &HashMap<String, PathBuf>,
        config: &Config,
        git_branches: &[crate::vcs::WorkspaceInfo],
    ) {
        let is_current = current_git.as_deref() == Some(name);
        let marker = if is_current { "* " } else { "  " };

        let normalized = config.get_normalized_workspace_name(name);
        let has_service =
            service_names.contains(normalized.as_str()) || service_names.contains(name);

        let service_state = service_branches
            .iter()
            .find(|b| b.name == normalized || b.name == name)
            .and_then(|b| b.state.as_deref());

        let wt_path = wt_lookup.get(name);

        let mut parts = Vec::new();
        if let Some(state) = service_state {
            parts.push(format!("service: {}", state));
        } else if has_service {
            parts.push("service: ok".to_string());
        }
        if let Some(path) = wt_path {
            parts.push(format!("worktree: {}", path.display()));
        }

        let suffix = if parts.is_empty() {
            String::new()
        } else {
            format!("  [{}]", parts.join(", "))
        };

        // For root nodes, connector is empty — just use the marker.
        // For child nodes, connector includes the tree drawing characters.
        if connector.is_empty() {
            println!("{}{}{}", marker, name, suffix);
        } else {
            println!("{}{}{}{}", marker, connector, name, suffix);
        }

        if let Some(kids) = children_map.get(name) {
            let count = kids.len();
            for (i, child) in kids.iter().enumerate() {
                let is_last = i == count - 1;
                let child_connector = if is_last {
                    format!("{}└─ ", prefix)
                } else {
                    format!("{}├─ ", prefix)
                };
                let child_prefix = if is_last {
                    format!("{}   ", prefix)
                } else {
                    format!("{}│  ", prefix)
                };
                print_node(
                    child,
                    &child_prefix,
                    &child_connector,
                    children_map,
                    current_git,
                    service_branches,
                    service_names,
                    wt_lookup,
                    config,
                    git_branches,
                );
            }
        }
    }

    for root in &roots {
        print_node(
            root,
            "  ",  // base prefix (indentation for children of roots)
            "",    // no connector for root nodes
            &children_map,
            &current_git,
            service_branches,
            &service_names,
            &wt_lookup,
            config,
            &git_branches,
        );
    }
}
```

#### Change 3: Update `enrich_branch_list_json` (line 1225)

Add `config_path: &Option<PathBuf>` parameter. Load the workspace registry and add a `"parent"` field at the top level of each entry (from the registry, falling back to service parent):

```rust
fn enrich_branch_list_json(
    service_branches: &[services::WorkspaceInfo],
    config: &Config,
    config_path: &Option<PathBuf>,
) -> serde_json::Value {
```

After the existing `wt_lookup` and `service_map` setup, add:

```rust
    // Load the workspace registry for parent info
    let registry: std::collections::HashMap<String, Option<String>> = config_path
        .as_ref()
        .and_then(|path| {
            LocalStateManager::new().ok().map(|state| {
                state
                    .get_workspaces(path)
                    .into_iter()
                    .map(|b| (b.name, b.parent))
                    .collect()
            })
        })
        .unwrap_or_default();
```

Then in the per-entry loop, after the worktree field, add:

```rust
        // Parent from registry (preferred) or service
        let parent = registry
            .get(name.as_str())
            .and_then(|p| p.clone())
            .or_else(|| sb.and_then(|s| s.parent_workspace.clone()));
        if let Some(parent_name) = parent {
            entry["parent"] = serde_json::Value::String(parent_name);
        }
```

#### Change 4: Update call sites in `Commands::List` handler (line 2249)

Change:
```rust
print_enriched_branch_list(&workspaces, config);
```
To:
```rust
print_enriched_branch_list(&workspaces, config, &config_path);
```

And change:
```rust
let enriched = enrich_branch_list_json(&workspaces, config);
```
To:
```rust
let enriched = enrich_branch_list_json(&workspaces, config, &config_path);
```

#### Change 5: Update `handle_multi_service_aggregation` call site

Search for any other call to `print_enriched_branch_list` in `handle_multi_service_aggregation` and add `config_path` there too. Need to check if that function calls it — it likely uses a different path.

#### Change 6: Add shell-init hint to `devflow init` output (line 735)

After the existing "Next steps" lines (line 742), when worktrees are enabled, add:

```rust
                if enable_worktrees {
                    println!(
                        "  eval \"$(devflow shell-init)\"  Add to your shell profile for auto-cd into worktrees"
                    );
                }
```

This should be inserted right after line 742 (`devflow doctor` line), before the closing `}`.

## Expected Output After Fix

### `devflow list` (with `main` + `pouet` child):
```
Branches (Local (Docker + CoW)):
* main  [service: running, worktree: /Users/.../hoho/]
  └─ pouet  [worktree: /Users/.../hoho.pouet]
```

### `devflow init` (with worktrees enabled):
```
Next steps:
  devflow service add          Add a service provider (interactive wizard)
  devflow install-hooks        Install Git hooks for automatic branching
  devflow doctor               Check system health and configuration
  eval "$(devflow shell-init)"  Add to your shell profile for auto-cd into worktrees
```

## Verification

1. `cargo build` — must compile clean
2. `cargo test` — all 45 tests must pass
3. Manual test: `devflow init` in fresh repo, create workspace with `devflow switch`, run `devflow list` to confirm tree output
