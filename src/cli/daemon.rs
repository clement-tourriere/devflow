//! The devflow controller daemon: a long-running process that keeps every
//! registered project's shared global engines (postgres/redis/rustfs/clickhouse
//! configured with `type: shared`) running and reconciles managed process
//! desired state plus `watch`/`retry` behavior.
//!
//! Foreground and detached starts share a verified owner lease. The
//! reconcile loop runs service engine reconciliation plus process desired-state,
//! watch, and retry reconciliation.

use anyhow::{Context, Result};
use devflow_core::controller::{self, status_path, DaemonStatus};
const DEFAULT_INTERVAL_SECS: u64 = 30;

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

            if once {
                let status = controller::run_once(interval).await?;
                print_status(&status, json_output);
                return Ok(());
            }

            if foreground {
                controller::run_loop(interval).await
            } else {
                // Detach: re-exec ourselves in foreground mode in the background.
                let mut child = devflow_core::detach::spawn_detached(&[
                    "daemon",
                    "start",
                    "--foreground",
                    "--interval",
                    &interval.to_string(),
                ])
                .context("Failed to spawn daemon process")?;
                let pid = child.id();
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
                loop {
                    if controller::owner()
                        .ok()
                        .flatten()
                        .is_some_and(|owner| owner.pid == pid)
                    {
                        break;
                    }
                    if let Some(status) = child.try_wait()? {
                        anyhow::bail!("Controller startup failed ({status}); another process may own its lease");
                    }
                    if tokio::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        anyhow::bail!(
                            "Controller did not claim its lease before the startup deadline"
                        );
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                if json_output {
                    println!(
                        "{}",
                        serde_json::json!({"status": "started", "pid": pid, "interval_secs": interval})
                    );
                } else {
                    println!(
                        "Controller daemon started (pid {}, reconcile every {}s)",
                        pid, interval
                    );
                }
                Ok(())
            }
        }
        super::DaemonCommands::Stop => {
            if let Some(pid) = controller::stop().await? {
                if json_output {
                    println!("{}", serde_json::json!({"status": "stopped", "pid": pid}));
                } else {
                    println!("Controller daemon stopped (pid {})", pid);
                }
            } else if json_output {
                println!("{}", serde_json::json!({"status": "not_running"}));
            } else {
                println!("Controller daemon is not running.");
            }
            Ok(())
        }
        super::DaemonCommands::Status => {
            let owner = controller::owner()?;
            let running = owner.is_some();
            let status: DaemonStatus = std::fs::read_to_string(status_path()?)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "running": running,
                        "pid": owner.as_ref().map(|identity| identity.pid),
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
