use anyhow::{Context, Result};
use devflow_core::config::Config;
use devflow_core::services;
use devflow_core::state::LocalStateManager;
use devflow_core::vcs;

use super::context::{context_matches_branch, resolve_branch_context};

/// Returns the workspace that was switched to, or `None` when the picker was
/// cancelled/failed — callers must not run follow-up actions (e.g. `-x`) in
/// that case.
pub(super) async fn handle_interactive_switch(
    config: &Config,
    config_path: &Option<std::path::PathBuf>,
) -> Result<Option<String>> {
    let mut workspace_names = std::collections::BTreeSet::new();
    let mut vcs_workspace_names = std::collections::HashSet::new();

    // 1) VCS workspaces (authoritative source)
    if let Ok(vcs_repo) = vcs::detect_vcs_provider(".") {
        if let Ok(vcs_branches) = vcs_repo.list_workspaces() {
            for workspace in vcs_branches {
                vcs_workspace_names.insert(workspace.name.clone());
                workspace_names.insert(workspace.name);
            }
        }
    }

    // 2) Devflow workspace registry
    if let Some(path) = config_path.as_ref() {
        if let Ok(state) = LocalStateManager::new() {
            for workspace in state.get_workspaces(path) {
                if vcs_workspace_names.is_empty() || vcs_workspace_names.contains(&workspace.name) {
                    workspace_names.insert(workspace.name);
                }
            }
        }
    }

    // 3) Service workspaces (best effort)
    if !config.resolve_services().is_empty() {
        if let Ok(providers) = services::factory::create_all_providers(config).await {
            for named in providers {
                if let Ok(service_branches) = named.provider.list_workspaces().await {
                    for workspace in service_branches {
                        if vcs_workspace_names.is_empty()
                            || vcs_workspace_names.contains(&workspace.name)
                        {
                            workspace_names.insert(workspace.name);
                        }
                    }
                }
            }
        }
    }

    // Include configured main workspace when visible in VCS (or if VCS probing failed).
    if vcs_workspace_names.is_empty() || vcs_workspace_names.contains(&config.git.main_workspace) {
        workspace_names.insert(config.git.main_workspace.clone());
    }

    let context = resolve_branch_context();
    let current_git = context.cwd_branch.clone();

    // Create workspace items with display info
    let mut branch_items: Vec<BranchItem> = workspace_names
        .iter()
        .map(|workspace| {
            let is_cwd = current_git.as_deref() == Some(workspace.as_str());
            let is_context = context_matches_branch(context.context_branch.as_deref(), workspace);

            BranchItem {
                name: workspace.clone(),
                display_name: workspace.clone(),
                is_cwd,
                is_context,
            }
        })
        .collect();

    // Add a "Create new workspace" option at the end
    branch_items.push(BranchItem {
        name: "__create_new__".to_string(),
        display_name: "+ Create new workspace".to_string(),
        is_cwd: false,
        is_context: false,
    });

    // Run interactive selector
    match run_interactive_selector(branch_items) {
        Ok(selected_branch) => {
            let target = if selected_branch == "__create_new__" {
                // Prompt for a new workspace name
                let new_name = inquire::Text::new("New workspace name:")
                    .with_help_message("Enter the name for the new workspace")
                    .prompt()
                    .context("Failed to read workspace name")?;
                let new_name = new_name.trim().to_string();
                if new_name.is_empty() {
                    anyhow::bail!("Workspace name cannot be empty");
                }
                (new_name, true)
            } else {
                (selected_branch, false)
            };
            let (workspace, create) = target;
            super::handle_switch_command(
                config,
                &workspace,
                config_path,
                create,
                None,  // from
                false, // no_services
                false, // no_processes
                false, // no_verify
                false, // json_output — interactive mode
                false, // non_interactive
                None,
                None,
                None, // copy_ignored — use config default
            )
            .await?;
            Ok(Some(workspace))
        }
        Err(e) => {
            match e {
                inquire::InquireError::OperationCanceled => {
                    println!("Cancelled.");
                }
                inquire::InquireError::OperationInterrupted => {
                    println!("Interrupted.");
                }
                _ => {
                    println!("Interactive mode failed: {}", e);
                    println!(
                        "Try using: devflow switch <workspace-name> or devflow switch --template"
                    );
                }
            }
            Ok(None)
        }
    }
}

#[derive(Clone)]
struct BranchItem {
    name: String,
    display_name: String,
    is_cwd: bool,
    is_context: bool,
}

fn run_interactive_selector(items: Vec<BranchItem>) -> Result<String, inquire::InquireError> {
    use inquire::Select;

    if items.is_empty() {
        return Err(inquire::InquireError::InvalidConfiguration(
            "No workspaces available".to_string(),
        ));
    }

    // Create display options with context/cwd markers.
    let options: Vec<String> = items
        .iter()
        .map(|item| {
            if item.is_context && item.is_cwd {
                format!("{} *", item.display_name)
            } else if item.is_context {
                format!("{} (context)", item.display_name)
            } else if item.is_cwd {
                format!("{} (cwd)", item.display_name)
            } else {
                item.display_name.clone()
            }
        })
        .collect();

    // Prefer context workspace as default; fall back to cwd workspace.
    let default = items
        .iter()
        .position(|item| item.is_context)
        .or_else(|| items.iter().position(|item| item.is_cwd));

    let mut select = Select::new("Select a workspace to switch to:", options.clone())
        .with_help_message(
        "Use arrow keys to navigate, type to filter, Enter to select, Esc to cancel (*=context+cwd)",
    );

    if let Some(default_index) = default {
        select = select.with_starting_cursor(default_index);
    }

    // Run the selector
    let selected_display = select.prompt()?;

    // Find the corresponding workspace name
    let selected_index = options
        .iter()
        .position(|opt| opt == &selected_display)
        .ok_or_else(|| {
            inquire::InquireError::InvalidConfiguration("Selected option not found".to_string())
        })?;

    Ok(items[selected_index].name.clone())
}
