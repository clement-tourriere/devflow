use std::collections::{HashMap, HashSet};
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
    let mut visited = HashSet::new();
    for root in &inventory.roots {
        print_node(root, "", "", &nodes, &mut visited);
    }

    // Defensive fallback for corrupt/cyclic lineage: never hide a workspace.
    for node in &inventory.workspaces {
        if !visited.contains(node.name.as_str()) {
            print_node(&node.name, "", "", &nodes, &mut visited);
        }
    }

    if !inventory.warnings.is_empty() {
        println!("Warnings:");
        for warning in &inventory.warnings {
            println!("  - {warning}");
        }
    }
}

fn print_node<'a>(
    name: &'a str,
    prefix: &str,
    connector: &str,
    nodes: &HashMap<&'a str, &'a WorkspaceNode>,
    visited: &mut HashSet<&'a str>,
) {
    let Some(node) = nodes.get(name).copied() else {
        return;
    };
    if !visited.insert(name) {
        return;
    }

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

    for (index, child) in node.children.iter().enumerate() {
        let last = index + 1 == node.children.len();
        let child_connector = if last {
            format!("{prefix}└─ ")
        } else {
            format!("{prefix}├─ ")
        };
        let child_prefix = if last {
            format!("{prefix}   ")
        } else {
            format!("{prefix}│  ")
        };
        print_node(child, &child_prefix, &child_connector, nodes, visited);
    }
}
