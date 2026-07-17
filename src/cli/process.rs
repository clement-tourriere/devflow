use anyhow::Result;
use devflow_core::config::Config;
use devflow_core::processes;
use devflow_core::vcs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn project_dir(config_path: &Option<PathBuf>) -> PathBuf {
    super::operation_project_dir(config_path)
}

fn current_workspace(config: &Config, workspace: Option<String>) -> String {
    workspace.unwrap_or_else(|| {
        vcs::detect_vcs_provider(".")
            .ok()
            .and_then(|repo| repo.current_workspace().ok().flatten())
            .unwrap_or_else(|| config.git.main_workspace.clone())
    })
}

pub(super) async fn handle_process_command(
    action: super::ProcessCommands,
    config: &Config,
    config_path: &Option<PathBuf>,
    json_output: bool,
) -> Result<()> {
    let project_dir = project_dir(config_path);

    match action {
        super::ProcessCommands::Start {
            names,
            all: _,
            workspace,
            force,
        } => {
            let workspace = current_workspace(config, workspace);
            let results = processes::start_workspace_processes(
                config,
                &project_dir,
                &workspace,
                &names,
                force,
            )
            .await?;
            print_results("start", &workspace, &results, json_output)?;
            fail_on_process_errors(&results)?;
        }
        super::ProcessCommands::Stop {
            names,
            all: _,
            workspace,
        } => {
            let workspace = current_workspace(config, workspace);
            let results =
                processes::stop_workspace_processes(config, &project_dir, &workspace, &names)
                    .await?;
            print_results("stop", &workspace, &results, json_output)?;
            fail_on_process_errors(&results)?;
        }
        super::ProcessCommands::Restart {
            names,
            all: _,
            workspace,
        } => {
            let workspace = current_workspace(config, workspace);
            let results =
                processes::restart_workspace_processes(config, &project_dir, &workspace, &names)
                    .await?;
            print_results("restart", &workspace, &results, json_output)?;
            fail_on_process_errors(&results)?;
        }
        super::ProcessCommands::Status { workspace }
        | super::ProcessCommands::List { workspace } => {
            let statuses =
                processes::list_workspace_processes(config, &project_dir, workspace.as_deref())?;
            if json_output {
                println!("{}", serde_json::to_string_pretty(&statuses)?);
            } else if statuses.is_empty() {
                println!("No process records found.");
            } else {
                println!("Processes:");
                for s in statuses {
                    let ports = if s.ports.is_empty() {
                        String::new()
                    } else {
                        format!(" ports={:?}", s.ports)
                    };
                    let pid = s
                        .pid
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| "-".to_string());
                    let required = if s.required { "" } else { " optional" };
                    let urls = if s.urls.is_empty() {
                        String::new()
                    } else {
                        format!(" url={}", s.urls.join(","))
                    };
                    let last_error = s
                        .last_error
                        .as_ref()
                        .map(|e| format!(" error={}", e))
                        .unwrap_or_default();
                    println!(
                        "  {} [{}] {}{} pid={}{}{}{}",
                        s.workspace, s.process, s.status, required, pid, ports, urls, last_error
                    );
                }
            }
        }
        super::ProcessCommands::Logs {
            name,
            workspace,
            tail,
            follow,
        } => {
            let workspace = current_workspace(config, workspace);
            let logs = processes::process_logs(config, &project_dir, &workspace, &name, tail)?;
            if json_output {
                if follow {
                    anyhow::bail!("--follow is not supported with --json");
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "workspace": workspace,
                        "process": name,
                        "logs": logs,
                    }))?
                );
            } else {
                print!("{}", logs);
                std::io::stdout().flush().ok();
                if follow {
                    let status = processes::list_workspace_processes(
                        config,
                        &project_dir,
                        Some(&workspace),
                    )?
                    .into_iter()
                    .find(|s| s.process == name)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "process '{}' has not been started in workspace '{}'",
                            name,
                            workspace
                        )
                    })?;
                    follow_process_log(
                        config,
                        &project_dir,
                        &workspace,
                        &name,
                        Path::new(&status.log_path),
                    )?;
                }
            }
        }
    }

    Ok(())
}

fn print_results(
    action: &str,
    workspace: &str,
    results: &[processes::ProcessResult],
    json_output: bool,
) -> Result<()> {
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "action": action,
                "workspace": workspace,
                "results": results,
            }))?
        );
    } else {
        for r in results {
            let ports = if r.ports.is_empty() {
                String::new()
            } else {
                format!(" ports={:?}", r.ports)
            };
            let prefix = if r.success { "✓" } else { "✗" };
            let required = if r.required { "" } else { " (optional)" };
            println!(
                "{} {}{}: {}{}",
                prefix, r.process, required, r.message, ports
            );
        }
    }
    Ok(())
}

fn follow_process_log(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    name: &str,
    path: &Path,
) -> Result<()> {
    let mut offset = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    loop {
        // Pitchfork logs are stored in SQLite; `process_logs` mirrors them to
        // the recorded log path so the same file-follow loop works for both
        // native and Pitchfork providers.
        let _ = processes::process_logs(config, project_dir, workspace, name, None);
        if let Ok(mut file) = std::fs::File::open(path) {
            let len = file.metadata()?.len();
            if len < offset {
                offset = 0;
            }
            file.seek(SeekFrom::Start(offset))?;
            let mut buf = String::new();
            file.read_to_string(&mut buf)?;
            if !buf.is_empty() {
                print!("{}", buf);
                std::io::stdout().flush().ok();
                offset = file.stream_position()?;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn fail_on_process_errors(results: &[processes::ProcessResult]) -> Result<()> {
    let failures = results.iter().filter(|r| !r.success && r.required).count();
    if failures > 0 {
        anyhow::bail!("{} required process operation(s) failed", failures);
    }
    Ok(())
}
