use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use devflow_core::config::Config;
use devflow_core::workspace::inventory::{
    build_workspace_inventory, WorkspaceInventory, WorkspaceNode,
};

pub(crate) async fn handle_workspace_list(
    config: &Config,
    config_path: &Option<PathBuf>,
    json_output: bool,
) -> Result<()> {
    // Preserve the caller's actual VCS context. In a linked worktree the
    // effective config may legitimately fall back to `.devflow.yml` in the
    // primary checkout; deriving context from that config path would then
    // incorrectly mark the primary workspace as current.
    let project_dir = super::super::operation_project_dir(config_path);
    let inventory = build_workspace_inventory(config, &project_dir).await?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&inventory)?);
    } else {
        print_workspace_inventory(&inventory);
    }
    Ok(())
}
pub(crate) fn print_workspace_inventory(inventory: &WorkspaceInventory) {
    println!(
        "Workspaces ({} · {})",
        inventory.project.name, inventory.project.vcs_provider
    );
    if let Some(context) = inventory.context_workspace.as_deref() {
        println!("Context: {context}");
    }
    if inventory.workspaces.is_empty() {
        println!("  (none)");
        return;
    }

    let nodes: HashMap<&str, &WorkspaceNode> = inventory
        .workspaces
        .iter()
        .map(|node| (node.name.as_str(), node))
        .collect();
    // The canonical order and connector data come from the shared flatten
    // in devflow-core, so CLI, TUI, and GUI render identical trees.
    for row in &inventory.flat_order {
        if let Some(node) = nodes.get(row.name.as_str()) {
            print_node(node, row);
        }
    }

    if !inventory.warnings.is_empty() {
        println!("Warnings:");
        for warning in &inventory.warnings {
            println!("  - {warning}");
        }
    }
}

/// Continuation columns for the levels above this node. `ancestor_has_next`
/// carries one entry per non-root ancestor level, so every entry draws a
/// column — the same interpretation the TUI uses.
fn ancestor_columns(row: &devflow_core::workspace::inventory::FlatWorkspaceRow) -> String {
    row.ancestor_has_next
        .iter()
        .map(|has_next| if *has_next { "│  " } else { "   " })
        .collect()
}

fn print_node(node: &WorkspaceNode, row: &devflow_core::workspace::inventory::FlatWorkspaceRow) {
    let columns = ancestor_columns(row);
    let (connector, prefix) = if row.depth == 0 {
        (String::new(), String::new())
    } else {
        (
            format!(
                "{columns}{}",
                if row.is_last_sibling {
                    "└─ "
                } else {
                    "├─ "
                }
            ),
            format!(
                "{columns}{}",
                if row.is_last_sibling { "   " } else { "│  " }
            ),
        )
    };

    let marker = if node.is_context { "* " } else { "  " };
    let mut tags = Vec::new();
    if node.is_default {
        tags.push("default".to_string());
    }
    tags.push(node.health.clone());
    if node.parent_state.as_deref() == Some("missing") {
        tags.push(format!(
            "parent missing: {}",
            node.parent.as_deref().unwrap_or("unknown")
        ));
    }
    if let Some(path) = node.worktree_path.as_deref() {
        tags.push(format!("path: {path}"));
    }
    println!("{marker}{connector}{}  [{}]", node.name, tags.join(", "));

    for service in &node.services {
        let state = if service.provisioned {
            service.state.as_deref().unwrap_or("available")
        } else {
            "not provisioned"
        };
        println!("{prefix}   • {}: {state}", service.name);
    }
    for process in &node.processes {
        println!(
            "{prefix}   ◦ process {}: {}",
            process.process, process.status
        );
    }
}
