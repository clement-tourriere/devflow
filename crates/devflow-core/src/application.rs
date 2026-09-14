//! Application policy shared by CLI, TUI, desktop and automation.
use crate::config::Config;
use crate::services::{factory, ServiceOperation, ServiceProvider, UnsupportedOperation};
use crate::state::LocalStateManager;
use crate::workspace::operation::{OperationJournal, OperationRecord};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub use crate::workspace::switch::{switch_workspace, SwitchOptions};

#[derive(Debug, Clone, Serialize)]
pub struct ServiceOutcome {
    pub workspace: String,
    pub service: String,
    pub service_key: String,
    pub action: ServiceOperation,
    pub operation: OperationRecord,
    pub data_origin: Option<crate::baseline::DataOrigin>,
}

pub async fn service_operation(
    config: &Config,
    project: &Path,
    service: &str,
    workspace: &str,
    provider: &dyn ServiceProvider,
    action: ServiceOperation,
) -> Result<ServiceOutcome> {
    if !provider.operations().supports(action) {
        return Err(UnsupportedOperation::new(provider.provider_name(), action).into());
    }
    let key = factory::resolve_service_operation_key(project, workspace, provider).await?;
    anyhow::ensure!(
        provider.workspace_exists(&key).await?,
        "service workspace '{workspace}' does not exist"
    );
    let mut journal = OperationJournal::begin(
        project,
        workspace,
        &format!("{action}:{service}"),
        None,
        None,
        false,
    )
    .await?;
    journal.set_request(
        serde_json::json!({"service":service,"workspace":workspace,"action":action}),
        config,
    )?;
    let origin = LocalStateManager::new()?
        .control(project)
        .workspaces
        .get(workspace)
        .and_then(|w| w.data.get(service))
        .cloned();
    let result: Result<()> =
        async {
            if let Some(crate::baseline::DataOrigin::Baseline {
                service_key,
                engine_fingerprint,
                ..
            }) = &origin
            {
                anyhow::ensure!(
                    &provider.baseline_fingerprint(service_key).await? == engine_fingerprint,
                    "recorded baseline generation no longer matches"
                );
                anyhow::ensure!(
                    provider.list_workspaces().await?.iter().any(
                        |w| w.name == key && w.parent_workspace.as_deref() == Some(service_key)
                    ),
                    "workspace data parent differs from its recorded baseline"
                );
            }
            journal.phase("provider")?;
            match action {
                ServiceOperation::Start => provider.start_workspace(&key).await?,
                ServiceOperation::Stop => provider.stop_workspace(&key).await?,
                ServiceOperation::Reset => provider.reset_workspace(&key).await?,
                _ => anyhow::bail!("{action} requires its dedicated application request"),
            }
            journal.completed("provider")?;
            journal.phase("verify")?;
            anyhow::ensure!(
                provider.workspace_exists(&key).await?,
                "service workspace disappeared during {action}"
            );
            journal.completed("verify")?;
            journal.finish(true)?;
            Ok(())
        }
        .await;
    if let Err(error) = result {
        return Err(journal.fail(error));
    }
    Ok(ServiceOutcome {
        workspace: workspace.into(),
        service: service.into(),
        service_key: key,
        action,
        data_origin: origin,
        operation: journal.record.clone(),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionEntry {
    pub workspace: String,
    pub service_key: String,
    pub reasons: Vec<String>,
    pub delete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionPlan {
    pub service: String,
    pub max_count: usize,
    pub entries: Vec<RetentionEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CleanupOutcome {
    pub plan: RetentionPlan,
    pub deleted: Vec<String>,
    pub failures: BTreeMap<String, String>,
    pub operation: OperationRecord,
}

pub async fn plan_service_cleanup(
    config: &Config,
    project: &Path,
    service: &str,
    provider: &dyn ServiceProvider,
    max_count: usize,
) -> Result<RetentionPlan> {
    anyhow::ensure!(
        provider.supports_cleanup(),
        "{} does not support cleanup",
        provider.provider_name()
    );
    let state = LocalStateManager::new()?;
    let registered = state.get_workspaces_by_dir(project);
    let control = state.control(project);
    let mut protected: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut protect = |raw: &str, reason: &str| -> Result<()> {
        let key = state.resolve_workspace_service_key_by_dir(project, raw)?;
        for name in [raw, &key] {
            protected
                .entry(name.into())
                .or_default()
                .push(reason.into());
        }
        Ok(())
    };
    protect(&config.git.main_workspace, "configured default workspace")?;
    let repo = crate::vcs::detect_vcs_provider(project)
        .context("cannot verify workspace retention protections")?;
    if let Some(current) = repo.current_workspace()? {
        protect(&current, "current workspace")?;
    }
    for wt in repo.list_worktrees()? {
        if let Some(raw) = &wt.workspace {
            if wt.is_main {
                protect(raw, "primary checkout")?;
            } else {
                protect(
                    raw,
                    "materialized worktree; remove the workspace explicitly",
                )?;
            }
            if repo.worktree_is_locked(&wt.path)? {
                protect(raw, "locked worktree")?;
            }
            if repo.worktree_is_dirty(&wt.path)? {
                protect(raw, "dirty worktree")?;
            }
        }
    }
    for (name, workspace) in &control.workspaces {
        if workspace.pinned {
            protect(name, "pinned workspace")?;
        }
        if workspace
            .expires_at
            .is_some_and(|expiry| expiry > chrono::Utc::now())
        {
            protect(name, "retention expiry has not elapsed")?;
        }
    }
    for workspace in &registered {
        if matches!(
            workspace.execution_status.as_deref(),
            Some("running" | "detached")
        ) {
            protect(&workspace.name, "active execution")?;
        }
        if let Some(parent) = workspace
            .data_parent
            .as_ref()
            .or(workspace.parent.as_ref())
            .filter(|p| !p.starts_with("baseline:"))
        {
            protect(parent, "recorded data parent")?;
        }
    }
    for operation in OperationJournal::pending(project)? {
        if operation.kind.starts_with("cleanup:") {
            continue;
        }
        protect(&operation.workspace, "unfinished operation")?;
    }
    for baseline in control
        .baselines
        .values()
        .flatten()
        .filter(|b| b.service == service)
    {
        protected
            .entry(baseline.service_key.clone())
            .or_default()
            .push("retained baseline version".into());
    }
    let mut workspaces = provider.list_workspaces().await?;
    for workspace in &workspaces {
        if let Some(parent) = &workspace.parent_workspace {
            protected
                .entry(parent.clone())
                .or_default()
                .push("provider data dependency".into());
        }
    }
    workspaces.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.name.cmp(&b.name)));
    let mut kept = 0;
    let entries = workspaces
        .into_iter()
        .map(|workspace| {
            let raw = registered
                .iter()
                .find(|r| r.service_key == workspace.name || r.name == workspace.name)
                .map(|r| r.name.clone())
                .unwrap_or_else(|| workspace.name.clone());
            let mut reasons = protected.get(&workspace.name).cloned().unwrap_or_default();
            reasons.extend(protected.get(&raw).cloned().unwrap_or_default());
            reasons.sort();
            reasons.dedup();
            let delete = if reasons.is_empty() {
                kept += 1;
                if kept > max_count {
                    true
                } else {
                    reasons.push("within retention budget".into());
                    false
                }
            } else {
                false
            };
            RetentionEntry {
                workspace: raw,
                service_key: workspace.name,
                reasons,
                delete,
            }
        })
        .collect();
    Ok(RetentionPlan {
        service: service.into(),
        max_count,
        entries,
    })
}

pub async fn cleanup_service(
    config: &Config,
    project: &Path,
    service: &str,
    provider: &dyn ServiceProvider,
    max_count: usize,
) -> Result<CleanupOutcome> {
    let mut journal = OperationJournal::begin(
        project,
        "__retention__",
        &format!("cleanup:{service}"),
        None,
        None,
        false,
    )
    .await?;
    journal.set_request(
        serde_json::json!({"service":service,"max_count":max_count}),
        config,
    )?;
    let plan = match plan_service_cleanup(config, project, service, provider, max_count).await {
        Ok(plan) => plan,
        Err(error) => return Err(journal.fail(error)),
    };
    journal.phase("delete")?;
    let mut deleted = Vec::new();
    let mut failures = BTreeMap::new();
    for entry in plan.entries.iter().filter(|entry| entry.delete) {
        // Re-plan immediately before each mutation. A newly created child or
        // changed worktree must not inherit an obsolete deletion decision.
        let current = plan_service_cleanup(config, project, service, provider, max_count).await?;
        if !current
            .entries
            .iter()
            .any(|e| e.service_key == entry.service_key && e.delete)
        {
            continue;
        }
        match provider.delete_workspace(&entry.service_key).await {
            Ok(()) => {
                deleted.push(entry.workspace.clone());
                journal.checkpoint(
                    &format!("delete:{}", entry.service_key),
                    crate::workspace::operation::StepKind::Cleanup,
                    true,
                    None,
                )?;
            }
            Err(error) => {
                journal.checkpoint(
                    &format!("delete:{}", entry.service_key),
                    crate::workspace::operation::StepKind::Cleanup,
                    false,
                    Some(format!("{error:#}")),
                )?;
                failures.insert(entry.workspace.clone(), format!("{error:#}"));
            }
        }
    }
    journal.finish(failures.is_empty())?;
    Ok(CleanupOutcome {
        plan,
        deleted,
        failures,
        operation: journal.record.clone(),
    })
}

/// Journal an explicit import. A failed import must remain visible until repaired.
pub async fn seed_service(
    config: &Config,
    project: &Path,
    service: &str,
    workspace: &str,
    source: &str,
) -> Result<OperationRecord> {
    let provider = factory::resolve_provider(config, Some(service)).await?;
    anyhow::ensure!(
        provider.provider.operations().seed,
        "service does not support seeding"
    );
    let key =
        factory::resolve_service_operation_key(project, workspace, provider.provider.as_ref())
            .await?;
    let mut journal = OperationJournal::begin(
        project,
        workspace,
        &format!("seed:{service}"),
        None,
        None,
        false,
    )
    .await?;
    // Remote connection strings can contain credentials; never persist them.
    journal.set_request(serde_json::json!({"service":service,"workspace":workspace,"source_fingerprint":crate::services::ownership::identity_digest(&[source])}), config)?;
    journal.phase("seed")?;
    if let Err(error) = provider.provider.seed_from_source(&key, source).await {
        return Err(journal.fail(error));
    }
    journal.completed("seed")?;
    journal.finish(true)?;
    Ok(journal.record.clone())
}

pub async fn resume_operation(
    config: &Config,
    project: &Path,
    id: &str,
) -> Result<serde_json::Value> {
    let record = OperationJournal::inspect(project, id)?;
    if record.status == "ready" {
        return Ok(serde_json::to_value(record)?);
    }
    anyhow::ensure!(
        record.status != "cancelled",
        "cancelled operations cannot be resumed"
    );
    let request = &record.request;
    if record.kind == "switch" {
        let options: SwitchOptions = serde_json::from_value(
            request
                .get("options")
                .cloned()
                .context("legacy operation: retry the original switch command")?,
        )?;
        return Ok(serde_json::to_value(
            switch_workspace(config, project, &record.workspace, &options).await?,
        )?);
    }
    if record.kind == "remove" {
        let options: crate::workspace::delete::DeleteOptions = serde_json::from_value(
            request
                .get("options")
                .cloned()
                .context("legacy operation: retry the original remove command")?,
        )?;
        return Ok(serde_json::to_value(
            crate::workspace::delete::delete_workspace(
                config,
                project,
                &record.workspace,
                &options,
            )
            .await?,
        )?);
    }
    if let Some(service) = record.kind.strip_prefix("cleanup:") {
        let provider = factory::resolve_provider(config, Some(service)).await?;
        let budget = request
            .get("max_count")
            .and_then(|v| v.as_u64())
            .context("cleanup budget missing")? as usize;
        return Ok(serde_json::to_value(
            cleanup_service(config, project, service, provider.provider.as_ref(), budget).await?,
        )?);
    }
    if let Some(service) = request.get("service").and_then(|v| v.as_str()) {
        if let Some(action) = request.get("action") {
            let provider = factory::resolve_provider(config, Some(service)).await?;
            let action = serde_json::from_value(action.clone())?;
            return Ok(serde_json::to_value(
                service_operation(
                    config,
                    project,
                    service,
                    &record.workspace,
                    provider.provider.as_ref(),
                    action,
                )
                .await?,
            )?);
        }
    }
    anyhow::bail!("operation {} requires its original explicit command and inputs; inspect its checkpoints before retrying shell hooks or data imports", record.id)
}
