use devflow_core::merge::train::MergeTrainEngine;
use devflow_core::{config, vcs};
use serde::Serialize;

#[derive(Serialize)]
pub struct MergeTrainDto {
    pub id: String,
    pub target: String,
    pub status: String,
    pub entries: Vec<MergeTrainEntryDto>,
}

#[derive(Serialize)]
pub struct MergeTrainEntryDto {
    pub workspace: String,
    pub position: usize,
    pub status: String,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct MergeCheckResultDto {
    pub check_name: String,
    pub passed: bool,
    pub severity: String,
    pub message: String,
    pub files: Vec<String>,
    pub suggestion: Option<String>,
}

#[derive(Serialize)]
pub struct MergeReadinessReportDto {
    pub source: String,
    pub target: String,
    pub ready: bool,
    pub checks: Vec<MergeCheckResultDto>,
}

#[derive(Serialize)]
pub struct RebaseResultDto {
    pub success: bool,
    pub commits_replayed: usize,
    pub conflicts: bool,
    pub conflict_files: Vec<String>,
}

#[derive(Serialize)]
pub struct CascadeReportDto {
    pub affected_children: Vec<String>,
    pub needs_rebase: Vec<CascadeRebaseNeededDto>,
}

#[derive(Serialize)]
pub struct CascadeRebaseNeededDto {
    pub workspace: String,
    pub reason: String,
}

#[derive(Serialize)]
pub struct MergeResultDto {
    pub success: bool,
    pub message: String,
    pub cascade: Option<CascadeReportDto>,
}

fn load_config(project_path: &str) -> Result<config::Config, String> {
    let config_path = std::path::Path::new(project_path).join(".devflow.yml");
    if config_path.exists() {
        config::Config::from_file(&config_path).map_err(crate::commands::format_error)
    } else {
        Ok(config::Config::default())
    }
}

#[tauri::command]
pub async fn merge_check(
    project_path: String,
    source: String,
    target: Option<String>,
) -> Result<MergeReadinessReportDto, String> {
    let cfg = load_config(&project_path)?;
    let target = target.unwrap_or_else(|| cfg.git.main_workspace.clone());

    let vcs_repo = vcs::detect_vcs_provider(&project_path).map_err(crate::commands::format_error)?;

    let merge_config = cfg.merge.unwrap_or_default();
    let checks = devflow_core::merge::build_checks_from_config(&merge_config);
    let report = devflow_core::merge::run_checks(&checks, vcs_repo.as_ref(), &source, &target);

    Ok(MergeReadinessReportDto {
        source: report.source,
        target: report.target,
        ready: report.ready,
        checks: report
            .checks
            .into_iter()
            .map(|c| MergeCheckResultDto {
                check_name: c.check_name,
                passed: c.passed,
                severity: match c.severity {
                    devflow_core::merge::CheckSeverity::Error => "error".to_string(),
                    devflow_core::merge::CheckSeverity::Warning => "warning".to_string(),
                },
                message: c.message,
                files: c.files,
                suggestion: c.suggestion,
            })
            .collect(),
    })
}

#[tauri::command]
pub async fn rebase_workspace(
    project_path: String,
    workspace: String,
    target: Option<String>,
) -> Result<RebaseResultDto, String> {
    let cfg = load_config(&project_path)?;
    let target = target.unwrap_or_else(|| cfg.git.main_workspace.clone());

    let vcs_repo = vcs::detect_vcs_provider(&project_path).map_err(crate::commands::format_error)?;
    vcs_repo
        .checkout_workspace(&workspace)
        .map_err(crate::commands::format_error)?;

    let result = vcs_repo.rebase(&target).map_err(crate::commands::format_error)?;

    Ok(RebaseResultDto {
        success: result.success,
        commits_replayed: result.commits_replayed,
        conflicts: result.conflicts,
        conflict_files: result.conflict_files,
    })
}

#[tauri::command]
pub async fn merge_workspace(
    project_path: String,
    source: String,
    target: Option<String>,
    cleanup: Option<bool>,
) -> Result<MergeResultDto, String> {
    let cfg = load_config(&project_path)?;
    let target = target.unwrap_or_else(|| cfg.git.main_workspace.clone());
    let project_dir = std::path::Path::new(&project_path);

    let vcs_repo = vcs::detect_vcs_provider(&project_path).map_err(crate::commands::format_error)?;

    // Run readiness checks
    if let Some(ref merge_config) = cfg.merge {
        let checks = devflow_core::merge::build_checks_from_config(merge_config);
        if !checks.is_empty() {
            let report =
                devflow_core::merge::run_checks(&checks, vcs_repo.as_ref(), &source, &target);
            if !report.ready {
                return Err("Merge readiness checks failed".to_string());
            }
        }
    }

    // Perform merge
    let merge_dir = vcs_repo
        .worktree_path(&target)
        .map_err(crate::commands::format_error)?
        .unwrap_or_else(|| project_dir.to_path_buf());

    let merge_cfg = cfg.merge.clone().unwrap_or_default();
    let strategy = merge_cfg.effective_strategy();

    // If rebase strategy, rebase source onto target first
    if strategy == config::MergeStrategy::Rebase {
        if merge_dir == project_dir.to_path_buf() {
            vcs_repo
                .checkout_workspace(&source)
                .map_err(crate::commands::format_error)?;
        }
        let rebase_result = vcs_repo
            .rebase(&target)
            .map_err(crate::commands::format_error)?;
        if !rebase_result.success {
            return Err(format!(
                "Rebase conflicts in: {}",
                rebase_result.conflict_files.join(", ")
            ));
        }
    }

    let merge_vcs =
        vcs::detect_vcs_provider(&merge_dir).map_err(crate::commands::format_error)?;

    if merge_dir == project_dir.to_path_buf() {
        merge_vcs
            .checkout_workspace(&target)
            .map_err(crate::commands::format_error)?;
    }

    merge_vcs
        .merge_branch(&source)
        .map_err(crate::commands::format_error)?;

    // Build cascade report
    let cascade = devflow_core::merge::build_cascade_report(
        merge_vcs.as_ref(),
        project_dir,
        &source,
        &target,
        cfg.merge.as_ref(),
    )
    .ok()
    .filter(|c| !c.affected_children.is_empty())
    .map(|c| CascadeReportDto {
        affected_children: c.affected_children,
        needs_rebase: c
            .needs_rebase
            .into_iter()
            .map(|nr| CascadeRebaseNeededDto {
                workspace: nr.workspace,
                reason: nr.reason,
            })
            .collect(),
    });

    // Cleanup if requested (resolve from config when GUI passes None)
    let merge_defaults = cfg.merge.clone().unwrap_or_default();
    let effective_cleanup = merge_defaults.effective_cleanup(cleanup.unwrap_or(false));
    if effective_cleanup {
        let _ = vcs_repo.delete_workspace(&source);
    }

    Ok(MergeResultDto {
        success: true,
        message: format!("Merged '{}' into '{}'", source, target),
        cascade,
    })
}

#[tauri::command]
pub async fn train_add(
    project_path: String,
    workspace: Option<String>,
    target: Option<String>,
) -> Result<(), String> {
    let cfg = load_config(&project_path)?;
    let target = target.unwrap_or_else(|| cfg.git.main_workspace.clone());
    let project_dir = std::path::Path::new(&project_path);

    let workspace = match workspace {
        Some(w) => w,
        None => {
            let vcs_repo =
                vcs::detect_vcs_provider(&project_path).map_err(crate::commands::format_error)?;
            vcs_repo
                .current_workspace()
                .map_err(crate::commands::format_error)?
                .ok_or_else(|| "Could not determine current workspace".to_string())?
        }
    };

    let engine = MergeTrainEngine::new(project_dir, &cfg);
    engine.enqueue(&workspace, &target).map_err(crate::commands::format_error)
}

#[tauri::command]
pub async fn train_remove(
    project_path: String,
    workspace: String,
) -> Result<(), String> {
    let cfg = load_config(&project_path)?;
    let project_dir = std::path::Path::new(&project_path);
    let engine = MergeTrainEngine::new(project_dir, &cfg);
    engine.dequeue(&workspace).map_err(crate::commands::format_error)
}

#[tauri::command]
pub async fn train_status(
    project_path: String,
    target: Option<String>,
) -> Result<Option<MergeTrainDto>, String> {
    let cfg = load_config(&project_path)?;
    let target = target.unwrap_or_else(|| cfg.git.main_workspace.clone());
    let project_dir = std::path::Path::new(&project_path);

    let engine = MergeTrainEngine::new(project_dir, &cfg);
    let train = engine.status(&target).map_err(crate::commands::format_error)?;

    Ok(train.map(|t| MergeTrainDto {
        id: t.id,
        target: t.target,
        status: format!("{:?}", t.status).to_lowercase(),
        entries: t
            .entries
            .into_iter()
            .map(|e| MergeTrainEntryDto {
                workspace: e.workspace,
                position: e.position,
                status: format!("{:?}", e.status).to_lowercase(),
                error: e.error,
            })
            .collect(),
    }))
}

#[tauri::command]
pub async fn train_run(
    project_path: String,
    target: Option<String>,
    stop_on_failure: Option<bool>,
    cleanup: Option<bool>,
) -> Result<Vec<MergeTrainEntryDto>, String> {
    let cfg = load_config(&project_path)?;
    let target = target.unwrap_or_else(|| cfg.git.main_workspace.clone());
    let project_dir = std::path::Path::new(&project_path);

    let engine = MergeTrainEngine::new(project_dir, &cfg);
    let merge_defaults = cfg.merge.clone().unwrap_or_default();
    let effective_cleanup = merge_defaults.effective_cleanup(cleanup.unwrap_or(false));
    let results = engine
        .run(&target, stop_on_failure.unwrap_or(false), effective_cleanup)
        .map_err(crate::commands::format_error)?;

    Ok(results
        .into_iter()
        .map(|e| MergeTrainEntryDto {
            workspace: e.workspace,
            position: e.position,
            status: format!("{:?}", e.status).to_lowercase(),
            error: e.error,
        })
        .collect())
}

#[tauri::command]
pub async fn train_pause(project_path: String, target: Option<String>) -> Result<(), String> {
    let cfg = load_config(&project_path)?;
    let target = target.unwrap_or_else(|| cfg.git.main_workspace.clone());
    let project_dir = std::path::Path::new(&project_path);
    let engine = MergeTrainEngine::new(project_dir, &cfg);
    engine.pause(&target).map_err(crate::commands::format_error)
}

#[tauri::command]
pub async fn train_resume(project_path: String, target: Option<String>) -> Result<(), String> {
    let cfg = load_config(&project_path)?;
    let target = target.unwrap_or_else(|| cfg.git.main_workspace.clone());
    let project_dir = std::path::Path::new(&project_path);
    let engine = MergeTrainEngine::new(project_dir, &cfg);
    engine.resume(&target).map_err(crate::commands::format_error)
}
