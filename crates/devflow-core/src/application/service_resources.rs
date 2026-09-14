//! Resource mutations shared by service commands and the desktop.
use crate::{
    baseline::DataOrigin,
    config::Config,
    services::{factory, ServiceProvider, WorkspaceInfo},
    state::LocalStateManager,
    workspace::operation::{OperationJournal, OperationRecord},
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Request {
    Create { parent: Option<String> },
    Delete,
    Destroy,
}
#[derive(Debug, Serialize)]
pub struct Outcome {
    pub service: String,
    pub workspace: String,
    pub service_key: String,
    pub workspace_info: Option<WorkspaceInfo>,
    pub destroyed: Vec<String>,
    pub operation: OperationRecord,
}
pub async fn mutate(
    config: &Config,
    project: &Path,
    service: &str,
    workspace: &str,
    provider: &dyn ServiceProvider,
    request: &Request,
) -> Result<Outcome> {
    let action = match request {
        Request::Create { .. } => "create",
        Request::Delete => "delete",
        Request::Destroy => "destroy",
    };
    let kind = format!("service-resource-{action}:{service}");
    let mut journal = OperationJournal::begin(project, workspace, &kind, None, None, false).await?;
    journal.set_request(
        serde_json::json!({"service":service,"workspace":workspace,"resource_request":request}),
        config,
    )?;
    let result: Result<_> = async {
        let state = LocalStateManager::new()?;
        let key = if matches!(request, Request::Create { .. }) {
            state.resolve_workspace_service_key_by_dir(project, workspace)?
        } else {
            factory::resolve_service_operation_key(project, workspace, provider).await?
        };
        let mut info = None;
        let mut destroyed = Vec::new();
        match request {
            Request::Create { parent } => {
                let origin = if let Some(reference) =
                    parent.as_ref().filter(|p| p.starts_with("baseline:"))
                {
                    let control = state.control(project);
                    let pinned = control
                        .workspaces
                        .get(workspace)
                        .and_then(|w| w.data.get(service));
                    let exact = match pinned {
                        Some(DataOrigin::Baseline {
                            reference: saved, ..
                        }) if saved == reference || saved.starts_with(&format!("{reference}@")) => {
                            saved.as_str()
                        }
                        _ => reference,
                    };
                    let baseline = crate::baseline::resolve(project, exact)?;
                    anyhow::ensure!(
                        baseline.service == service,
                        "Baseline belongs to service '{}'",
                        baseline.service
                    );
                    let fingerprint = baseline
                        .engine_fingerprint
                        .context("Baseline is not sealed")?;
                    anyhow::ensure!(
                        provider.baseline_fingerprint(&baseline.service_key).await? == fingerprint,
                        "Baseline engine identity changed"
                    );
                    DataOrigin::Baseline {
                        reference: baseline.reference,
                        service_key: baseline.service_key,
                        generation: baseline.resource_id.0,
                        engine_fingerprint: fingerprint,
                    }
                } else if provider.data_strategy() == "empty" {
                    DataOrigin::Empty
                } else if let Some(parent) = parent {
                    DataOrigin::Workspace {
                        workspace: parent.clone(),
                        service_key: state.resolve_workspace_service_key_by_dir(project, parent)?,
                    }
                } else {
                    DataOrigin::Empty
                };
                let parent_key = match &origin {
                    DataOrigin::Workspace { service_key, .. }
                    | DataOrigin::Baseline { service_key, .. } => Some(service_key.as_str()),
                    DataOrigin::Empty => None,
                };
                let existing = provider
                    .list_workspaces()
                    .await?
                    .into_iter()
                    .find(|w| w.name == key);
                if let Some(existing) = &existing {
                    if parent.is_some() {
                        anyhow::ensure!(
                            existing.parent_workspace.as_deref() == parent_key,
                            "Service workspace already has a different data parent"
                        );
                    }
                }
                if let Some(previous) = state
                    .control(project)
                    .workspaces
                    .get(workspace)
                    .and_then(|w| w.data.get(service))
                {
                    if parent.is_some() {
                        anyhow::ensure!(
                            serde_json::to_value(previous)? == serde_json::to_value(&origin)?,
                            "Service workspace retains a different data origin"
                        );
                    }
                }
                journal.phase("provider")?;
                if let Some(existing) = existing {
                    info = Some(existing);
                } else {
                    crate::baseline::bind_resolved_origins(
                        project,
                        workspace,
                        &[(service.into(), origin.clone())].into_iter().collect(),
                    )?;
                    info = Some(provider.create_workspace(&key, parent_key).await?);
                }
                anyhow::ensure!(
                    provider.workspace_exists(&key).await?,
                    "Provider did not materialize the service workspace"
                );
            }
            Request::Delete => {
                journal.phase("provider")?;
                if provider.workspace_exists(&key).await? {
                    provider.delete_workspace(&key).await?;
                }
                anyhow::ensure!(
                    !provider.workspace_exists(&key).await?,
                    "Provider retained the service workspace after deletion"
                );
                LocalStateManager::new()?.update_control(project, |control| {
                    control.workspace(workspace).data.remove(service);
                    Ok(())
                })?;
                destroyed.push(key.clone());
            }
            Request::Destroy => {
                anyhow::ensure!(
                    provider.supports_destroy(),
                    "Provider does not support service destruction"
                );
                journal.phase("provider")?;
                destroyed = provider.destroy_project().await?;
                anyhow::ensure!(
                    provider.list_workspaces().await?.is_empty(),
                    "Provider retained resources after destruction"
                );
                LocalStateManager::new()?.update_control(project, |control| {
                    for workspace in control.workspaces.values_mut() {
                        workspace.data.remove(service);
                    }
                    control.baselines.retain(|_, versions| {
                        versions.retain(|b| b.service != service);
                        !versions.is_empty()
                    });
                    Ok(())
                })?;
            }
        }
        journal.completed("provider")?;
        Ok((key, info, destroyed))
    }
    .await;
    let (key, workspace_info, destroyed) = match result {
        Ok(result) => result,
        Err(error) => return Err(journal.fail(error)),
    };
    journal.finish(true)?;
    Ok(Outcome {
        service: service.into(),
        workspace: workspace.into(),
        service_key: key,
        workspace_info,
        destroyed,
        operation: journal.record.clone(),
    })
}

pub async fn mutate_all(
    config: &Config,
    project: &Path,
    workspace: &str,
    request: &Request,
) -> Result<Vec<factory::OrchestrationResult>> {
    let providers = factory::create_auto_workspace_providers(config).await?;
    let mut results = Vec::new();
    for named in providers {
        let result = mutate(
            config,
            project,
            &named.name,
            workspace,
            named.provider.as_ref(),
            request,
        )
        .await;
        results.push(match result {
            Ok(outcome) => factory::OrchestrationResult {
                service_name: named.name,
                success: true,
                message: "Service operation completed".into(),
                workspace_info: outcome.workspace_info,
            },
            Err(error) => factory::OrchestrationResult {
                service_name: named.name,
                success: false,
                message: format!("{error:#}"),
                workspace_info: None,
            },
        });
    }
    Ok(results)
}
