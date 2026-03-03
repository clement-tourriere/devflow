use super::docker::{ContainerStatus, DockerRuntime};
use super::model::{BranchState, Workspace};

/// Determine state changes needed by checking Docker container states.
/// Returns a list of (branch_id, new_state) pairs.
pub async fn compute_state_changes(
    runtime: &DockerRuntime,
    workspaces: Vec<Workspace>,
) -> Vec<(String, BranchState)> {
    if workspaces.is_empty() {
        return vec![];
    }

    // Check if Docker is available
    let doctor = runtime.doctor().await;

    if !doctor.available {
        log::warn!(
            "Docker unavailable during reconciliation: {}; normalizing provisioning workspaces only",
            doctor.detail
        );

        return workspaces
            .into_iter()
            .filter(|b| b.state == BranchState::Provisioning)
            .map(|b| (b.id, BranchState::Stopped))
            .collect();
    }

    let mut changes = vec![];
    for workspace in workspaces {
        let next_state = match runtime.container_status(&workspace.container_name).await {
            Ok(ContainerStatus::Running) => BranchState::Running,
            Ok(ContainerStatus::Paused) => {
                match runtime.unpause_branch(&workspace.container_name).await {
                    Ok(()) => BranchState::Running,
                    Err(err) => {
                        log::warn!(
                            "Failed to unpause container '{}' during reconciliation: {}",
                            workspace.container_name,
                            err
                        );
                        BranchState::Failed
                    }
                }
            }
            Ok(ContainerStatus::Exited)
            | Ok(ContainerStatus::NotFound)
            | Ok(ContainerStatus::Other(_)) => {
                if std::path::Path::new(&workspace.data_dir).exists() {
                    BranchState::Stopped
                } else {
                    BranchState::Failed
                }
            }
            Err(err) => {
                log::warn!(
                    "Failed to inspect container '{}' while reconciling: {}; leaving state unchanged",
                    workspace.container_name, err
                );
                continue;
            }
        };

        if next_state != workspace.state {
            changes.push((workspace.id, next_state));
        }
    }

    log::info!("Reconciliation completed: {} state changes", changes.len());
    changes
}
