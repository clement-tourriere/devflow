//! Thin desktop adapters for the shared workspace application API.
use devflow_core::{
    baseline,
    workspace::{operation::OperationJournal, plan},
};
use std::path::Path;

#[tauri::command]
pub async fn list_baselines(project_path: String) -> Result<Vec<baseline::Baseline>, String> {
    baseline::list(Path::new(&project_path)).map_err(super::format_error)
}
#[tauri::command]
pub async fn list_operations(
    project_path: String,
) -> Result<Vec<devflow_core::workspace::operation::OperationRecord>, String> {
    OperationJournal::records(Path::new(&project_path), false).map_err(super::format_error)
}
#[tauri::command]
pub async fn resume_operation(
    project_path: String,
    id: String,
) -> Result<serde_json::Value, String> {
    let project = Path::new(&project_path);
    let config = super::project_config::load_project_config_with_local_state(project)?;
    devflow_core::application::resume_operation(&config, project, &id)
        .await
        .map_err(super::format_error)
}
#[tauri::command]
pub async fn cancel_operation(
    project_path: String,
    id: String,
) -> Result<devflow_core::workspace::operation::OperationRecord, String> {
    OperationJournal::cancel(Path::new(&project_path), &id)
        .await
        .map_err(super::format_error)
}
#[tauri::command]
pub async fn plan_workspace(
    project_path: String,
    workspace: String,
    from_workspace: Option<String>,
    data_from_workspace: Option<String>,
) -> Result<plan::WorkspacePlan, String> {
    let project = Path::new(&project_path);
    let config = super::project_config::load_project_config_with_local_state(project)?;
    plan::resolve(
        &config,
        project,
        &workspace,
        &devflow_core::workspace::switch::SwitchOptions {
            create_if_missing: true,
            from_workspace,
            data_from_workspace,
            ..Default::default()
        },
    )
    .map_err(super::format_error)
}
