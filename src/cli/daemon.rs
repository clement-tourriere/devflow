//! The devflow controller daemon: a long-running process that keeps every
//! registered project's shared global engines (postgres/redis/rustfs/clickhouse
//! configured with `type: shared`) running and reconciles managed process
//! desired state plus `watch`/`retry` behavior.
//!
//! It reuses the same detached-spawn + pidfile pattern as the proxy. The
//! reconcile loop runs service engine reconciliation plus process desired-state,
//! watch, and retry reconciliation.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const DEFAULT_INTERVAL_SECS: u64 = 30;

fn devflow_config_dir() -> Result<PathBuf> {
    let dir = devflow_core::paths::devflow_config_dir()?;
    std::fs::create_dir_all(&dir).ok();
    Ok(dir)
}

fn pid_path() -> Result<PathBuf> {
    Ok(devflow_config_dir()?.join("daemon.pid"))
}

fn status_path() -> Result<PathBuf> {
    Ok(devflow_config_dir()?.join("daemon-status.json"))
}

#[derive(Serialize, Deserialize, Default)]
struct DaemonStatus {
    last_reconcile: Option<String>,
    interval_secs: u64,
    projects: usize,
    engines_total: usize,
    engines_running: usize,
    processes_reconciled: usize,
    processes_failed: usize,
    detail: Vec<DaemonEngineLine>,
    process_detail: Vec<DaemonProcessLine>,
}

#[derive(Serialize, Deserialize)]
struct DaemonEngineLine {
    project: String,
    service: String,
    provider: String,
    running: bool,
}

#[derive(Serialize, Deserialize)]
struct DaemonProcessLine {
    project: String,
    workspace: String,
    process: String,
    action: String,
    success: bool,
}

/// Is a process with `pid` alive? (signal 0 probes without killing.)
#[cfg(unix)]
fn process_alive(pid: i32) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    kill(Pid::from_raw(pid), None).is_ok()
}
#[cfg(not(unix))]
fn process_alive(_pid: i32) -> bool {
    false
}

fn read_pid() -> Option<i32> {
    let path = pid_path().ok()?;
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

pub(super) async fn handle_daemon_command(
    action: super::DaemonCommands,
    json_output: bool,
) -> Result<()> {
    match action {
        super::DaemonCommands::Start {
            interval,
            foreground,
            once,
        } => {
            let interval = interval.unwrap_or(DEFAULT_INTERVAL_SECS).max(1);

            // Refuse to double-start.
            if !once && !foreground {
                if let Some(pid) = read_pid() {
                    if process_alive(pid) {
                        if json_output {
                            println!(
                                "{}",
                                serde_json::json!({"error": "already_running", "pid": pid})
                            );
                        } else {
                            anyhow::bail!(
                                "Daemon already running (pid {}). Stop it first with: devflow daemon stop",
                                pid
                            );
                        }
                        return Ok(());
                    }
                }
            }

            if once {
                let status = reconcile_once(interval).await?;
                print_status(&status, json_output);
                return Ok(());
            }

            if foreground {
                run_loop(interval).await
            } else {
                // Detach: re-exec ourselves in foreground mode in the background.
                let exe = std::env::current_exe()?;
                let child = std::process::Command::new(exe)
                    .args([
                        "daemon",
                        "start",
                        "--foreground",
                        "--interval",
                        &interval.to_string(),
                    ])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .context("Failed to spawn daemon process")?;
                std::fs::write(pid_path()?, child.id().to_string())?;
                if json_output {
                    println!(
                        "{}",
                        serde_json::json!({"status": "started", "pid": child.id(), "interval_secs": interval})
                    );
                } else {
                    println!(
                        "Controller daemon started (pid {}, reconcile every {}s)",
                        child.id(),
                        interval
                    );
                }
                Ok(())
            }
        }
        super::DaemonCommands::Stop => {
            let path = pid_path()?;
            if let Some(pid) = read_pid() {
                #[cfg(unix)]
                {
                    use nix::sys::signal::{kill, Signal};
                    use nix::unistd::Pid;
                    let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
                }
                let _ = std::fs::remove_file(&path);
                if json_output {
                    println!("{}", serde_json::json!({"status": "stopped", "pid": pid}));
                } else {
                    println!("Controller daemon stopped (pid {})", pid);
                }
            } else if json_output {
                println!("{}", serde_json::json!({"status": "not_running"}));
            } else {
                println!("Controller daemon is not running (no pidfile).");
            }
            Ok(())
        }
        super::DaemonCommands::Status => {
            let running = read_pid().map(process_alive).unwrap_or(false);
            let status: DaemonStatus = std::fs::read_to_string(status_path()?)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "running": running,
                        "pid": read_pid(),
                        "last_reconcile": status.last_reconcile,
                        "projects": status.projects,
                        "engines_running": status.engines_running,
                        "engines_total": status.engines_total,
                        "processes_reconciled": status.processes_reconciled,
                        "processes_failed": status.processes_failed,
                    })
                );
            } else {
                println!(
                    "Controller daemon: {}",
                    if running { "running" } else { "stopped" }
                );
                if let Some(ts) = &status.last_reconcile {
                    println!("  Last reconcile: {ts}");
                    println!(
                        "  {} engine(s) running / {} configured across {} project(s)",
                        status.engines_running, status.engines_total, status.projects
                    );
                    for line in &status.detail {
                        let mark = if line.running { "✓" } else { "✗" };
                        println!(
                            "    {} {} :: {} ({})",
                            mark, line.project, line.service, line.provider
                        );
                    }
                    if status.processes_reconciled > 0 {
                        println!(
                            "  {} process action(s), {} failed",
                            status.processes_reconciled, status.processes_failed
                        );
                        for line in &status.process_detail {
                            let mark = if line.success { "✓" } else { "✗" };
                            println!(
                                "    {} {} :: {} / {} ({})",
                                mark, line.project, line.workspace, line.process, line.action
                            );
                        }
                    }
                }
            }
            Ok(())
        }
    }
}

/// Run a single reconcile pass and persist the status file.
async fn reconcile_once(interval: u64) -> Result<DaemonStatus> {
    let results = devflow_core::services::factory::reconcile_all_projects().await;
    let process_results = devflow_core::processes::reconcile_all_projects_processes().await;

    let mut detail = Vec::new();
    let mut total = 0;
    let mut running = 0;
    for pr in &results {
        for e in &pr.engines {
            total += 1;
            if e.running {
                running += 1;
            }
            detail.push(DaemonEngineLine {
                project: pr.project.clone(),
                service: e.service_name.clone(),
                provider: e.provider.clone(),
                running: e.running,
            });
        }
    }

    let process_detail: Vec<DaemonProcessLine> = process_results
        .iter()
        .map(|p| DaemonProcessLine {
            project: p.project.clone(),
            workspace: p.workspace.clone(),
            process: p.process.clone(),
            action: p.action.clone(),
            success: p.success,
        })
        .collect();
    let processes_reconciled = process_detail.len();
    let processes_failed = process_detail.iter().filter(|p| !p.success).count();

    let status = DaemonStatus {
        // chrono is available via devflow-core's re-exports; format RFC3339.
        last_reconcile: Some(now_rfc3339()),
        interval_secs: interval,
        projects: results.len(),
        engines_total: total,
        engines_running: running,
        processes_reconciled,
        processes_failed,
        detail,
        process_detail,
    };

    if let Ok(path) = status_path() {
        if let Ok(json) = serde_json::to_string_pretty(&status) {
            let _ = std::fs::write(path, json);
        }
    }
    Ok(status)
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// The long-running reconcile loop. Reconciles immediately, then every
/// `interval` seconds, until SIGINT/SIGTERM.
async fn run_loop(interval: u64) -> Result<()> {
    log::info!("Controller daemon starting (reconcile every {}s)", interval);
    loop {
        match reconcile_once(interval).await {
            Ok(s) => log::info!(
                "Reconciled {} project(s): {}/{} engines running, {} process action(s)",
                s.projects,
                s.engines_running,
                s.engines_total,
                s.processes_reconciled
            ),
            Err(e) => log::warn!("Reconcile failed: {e:#}"),
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(interval)) => {}
            _ = tokio::signal::ctrl_c() => {
                log::info!("Controller daemon shutting down");
                let _ = std::fs::remove_file(pid_path()?);
                return Ok(());
            }
        }
    }
}

fn print_status(status: &DaemonStatus, json_output: bool) {
    if json_output {
        println!(
            "{}",
            serde_json::json!({
                "status": "ok",
                "projects": status.projects,
                "engines_running": status.engines_running,
                "engines_total": status.engines_total,
                "processes_reconciled": status.processes_reconciled,
                "processes_failed": status.processes_failed,
            })
        );
    } else if status.engines_total == 0 && status.processes_reconciled == 0 {
        println!("No shared global engines configured and no process actions needed.");
    } else {
        println!(
            "Reconciled {} project(s): {}/{} engines running",
            status.projects, status.engines_running, status.engines_total
        );
        for line in &status.detail {
            let mark = if line.running { "✓" } else { "✗" };
            println!(
                "  {} {} :: {} ({})",
                mark, line.project, line.service, line.provider
            );
        }
        if status.processes_reconciled > 0 {
            println!(
                "  Process reconcile: {} action(s), {} failed",
                status.processes_reconciled, status.processes_failed
            );
            for line in &status.process_detail {
                let mark = if line.success { "✓" } else { "✗" };
                println!(
                    "  {} {} :: {} / {} ({})",
                    mark, line.project, line.workspace, line.process, line.action
                );
            }
        }
    }
}
