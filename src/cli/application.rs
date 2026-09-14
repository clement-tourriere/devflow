use anyhow::{Context, Result};
use clap::Subcommand;
use devflow_core::{
    application, baseline,
    config::Config,
    execution::{execute_in_workspace, CommandSpec},
    services::factory,
    state::LocalStateManager,
    workspace::operation::OperationJournal,
};
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum BaselineCommands {
    /// Import, optionally migrate and validate, then seal a PostgreSQL baseline.
    Prepare {
        name: String,
        #[arg(long)]
        service: String,
        #[arg(long = "from")]
        source: PathBuf,
        #[arg(long)]
        migrate: Option<PathBuf>,
        #[arg(long)]
        validate: Option<PathBuf>,
    },
    List,
    Inspect {
        reference: String,
    },
    /// Delete an unused baseline version (dependants always prevent deletion).
    Remove {
        reference: String,
    },
}
#[derive(Subcommand)]
pub enum OperationCommands {
    List {
        #[arg(long)]
        history: bool,
    },
    Inspect {
        id: String,
    },
    /// Retry saved intent. Shell hooks can run again; they must tolerate retries.
    Resume {
        id: String,
    },
}
#[derive(Subcommand)]
pub enum RetentionCommands {
    Plan {
        #[arg(long)]
        service: Option<String>,
        #[arg(long, default_value_t = 10)]
        max_count: usize,
    },
    Apply {
        #[arg(long)]
        service: Option<String>,
        #[arg(long, default_value_t = 10)]
        max_count: usize,
    },
    Pin {
        workspace: String,
        #[arg(long)]
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
    },
    Unpin {
        workspace: String,
    },
    Expire {
        workspace: String,
        at: chrono::DateTime<chrono::Utc>,
    },
}
#[derive(Subcommand)]
pub enum ProjectCommands {
    List,
    /// Update a registered project's path while preserving resource identities.
    Relocate {
        old: PathBuf,
        new: PathBuf,
    },
}

pub async fn handle(
    cmd: &super::Commands,
    config: &Config,
    project: &Path,
    json: bool,
) -> Result<Option<serde_json::Value>> {
    use super::Commands;
    let _policy_lease = if matches!(
        cmd,
        Commands::Retention {
            action: RetentionCommands::Pin { .. }
                | RetentionCommands::Unpin { .. }
                | RetentionCommands::Expire { .. }
        }
    ) {
        Some(devflow_core::workspace::operation::project_policy_lease(project, true).await?)
    } else {
        None
    };
    let result = match cmd {
        Commands::Exec {
            workspace,
            shell,
            detach,
            command,
        } => {
            let workspace = workspace
                .clone()
                .or(devflow_core::vcs::detect_vcs_provider(project)?.current_workspace()?)
                .context("select a workspace with --workspace")?;
            let spec = if let Some(script) = shell {
                CommandSpec::Shell {
                    script: script.clone(),
                    args: command.clone(),
                }
            } else {
                let (program, args) = command
                    .split_first()
                    .context("provide a program after -- or use --shell SCRIPT")?;
                CommandSpec::Program {
                    program: program.clone(),
                    args: args.to_vec(),
                }
            };
            let output =
                execute_in_workspace(config, project, &workspace, &spec, *detach, json).await?;
            serde_json::to_value(output)?
        }
        Commands::Baseline { action } => match action {
            BaselineCommands::Prepare {
                name,
                service,
                source,
                migrate,
                validate,
            } => serde_json::to_value(
                baseline::prepare(
                    config,
                    project,
                    name,
                    service,
                    source,
                    migrate.as_deref(),
                    validate.as_deref(),
                )
                .await?,
            )?,
            BaselineCommands::List => serde_json::to_value(baseline::list(project)?)?,
            BaselineCommands::Inspect { reference } => serde_json::to_value(
                baseline::list(project)?
                    .into_iter()
                    .find(|b| &b.reference == reference)
                    .context("baseline not found")?,
            )?,
            BaselineCommands::Remove { reference } => {
                baseline::remove(config, project, reference).await?;
                serde_json::json!({"removed":reference})
            }
        },
        Commands::Operation { action } => match action {
            OperationCommands::List { history } => {
                serde_json::to_value(OperationJournal::records(project, *history)?)?
            }
            OperationCommands::Inspect { id } => {
                serde_json::to_value(OperationJournal::inspect(project, id)?)?
            }
            OperationCommands::Resume { id } => {
                application::resume_operation(config, project, id).await?
            }
        },
        Commands::Retention { action } => match action {
            RetentionCommands::Plan { service, max_count }
            | RetentionCommands::Apply { service, max_count } => {
                let provider = factory::resolve_provider(config, service.as_deref()).await?;
                if matches!(action, RetentionCommands::Apply { .. }) {
                    serde_json::to_value(
                        application::cleanup_service(
                            config,
                            project,
                            &provider.name,
                            provider.provider.as_ref(),
                            *max_count,
                        )
                        .await?,
                    )?
                } else {
                    serde_json::to_value(
                        application::plan_service_cleanup(
                            config,
                            project,
                            &provider.name,
                            provider.provider.as_ref(),
                            *max_count,
                        )
                        .await?,
                    )?
                }
            }
            RetentionCommands::Pin {
                workspace,
                expires_at,
            } => LocalStateManager::new()?.update_control(project, |c| {
                let w = c.workspace(workspace);
                w.pinned = true;
                w.expires_at = *expires_at;
                Ok(serde_json::to_value(w)?)
            })?,
            RetentionCommands::Unpin { workspace } => {
                LocalStateManager::new()?.update_control(project, |c| {
                    let w = c.workspace(workspace);
                    w.pinned = false;
                    Ok(serde_json::to_value(w)?)
                })?
            }
            RetentionCommands::Expire { workspace, at } => LocalStateManager::new()?
                .update_control(project, |c| {
                    let w = c.workspace(workspace);
                    w.expires_at = Some(*at);
                    Ok(serde_json::to_value(w)?)
                })?,
        },
        Commands::Project { action } => match action {
            ProjectCommands::List => {
                serde_json::to_value(LocalStateManager::new()?.list_all_projects())?
            }
            ProjectCommands::Relocate { old, new } => {
                serde_json::json!({"project_id":LocalStateManager::new()?.relocate_project(old,new)?,"path":new})
            }
        },
        _ => return Ok(None),
    };
    Ok(Some(result))
}
