//! One controller owner per devflow configuration directory.
use crate::{process_identity::ProcessIdentity, services::ownership::OperationLease};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, time::Duration};

pub fn status_path() -> Result<PathBuf> {
    Ok(crate::paths::devflow_config_dir()?.join("daemon-status.json"))
}
fn owner_path() -> Result<PathBuf> {
    Ok(crate::paths::devflow_config_dir()?.join("daemon-owner.json"))
}
fn lock_path() -> Result<PathBuf> {
    Ok(crate::paths::devflow_config_dir()?.join("daemon.lock"))
}

struct Owner {
    identity: ProcessIdentity,
    _lease: OperationLease,
}
impl Owner {
    async fn acquire() -> Result<Self> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let lease = loop {
            if let Some(lease) = OperationLease::try_acquire(&lock_path()?)? {
                break lease;
            }
            // Status probes briefly take the lease too. Only a published,
            // live owner establishes another controller; allow probes and
            // in-progress startup to finish before deciding.
            anyhow::ensure!(
                !read_identity()?.is_some_and(|identity| identity.is_alive()),
                "Controller is already running"
            );
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "Controller lease is busy but its owner has not published an identity"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let identity = ProcessIdentity::read(std::process::id())
            .context("Cannot verify controller process identity")?;
        crate::services::ownership::atomic_write(&owner_path()?, &serde_json::to_vec(&identity)?)?;
        Ok(Self {
            identity,
            _lease: lease,
        })
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        if let Ok(path) = owner_path() {
            if std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<ProcessIdentity>(&bytes).ok())
                .as_ref()
                == Some(&self.identity)
            {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}
fn read_identity() -> Result<Option<ProcessIdentity>> {
    match std::fs::read(owner_path()?) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("Cannot inspect controller identity"),
    }
}
pub fn owner() -> Result<Option<ProcessIdentity>> {
    let Some(identity) = read_identity()? else {
        return Ok(None);
    };
    if OperationLease::try_acquire(&lock_path()?)?.is_some() {
        return Ok(None);
    }
    Ok(identity.is_alive().then_some(identity))
}
pub async fn stop() -> Result<Option<u32>> {
    let Some(identity) = owner()? else {
        return Ok(None);
    };
    if !identity.terminate()? {
        return Ok(None);
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while owner()?.as_ref() == Some(&identity) {
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "Controller has not released its lease after SIGTERM"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(Some(identity.pid))
}
pub async fn run_once(interval: u64) -> Result<DaemonStatus> {
    let _owner = Owner::acquire().await?;
    reconcile_once(interval).await
}
pub async fn run_loop(interval: u64) -> Result<()> {
    let _owner = Owner::acquire().await?;
    #[cfg(unix)]
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let shutdown = async {
        #[cfg(unix)]
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    };
    tokio::select! {
        _ = shutdown => Ok(()),
        result = async {
            loop {
                if let Err(error) = reconcile_once(interval).await { log::warn!("Controller reconciliation failed: {error:#}"); }
                tokio::time::sleep(Duration::from_secs(interval.max(1))).await;
            }
            #[allow(unreachable_code)] Ok::<(), anyhow::Error>(())
        } => result,
    }
}

#[derive(Serialize, Deserialize, Default)]
pub struct DaemonStatus {
    pub last_reconcile: Option<String>,
    pub interval_secs: u64,
    pub projects: usize,
    pub engines_total: usize,
    pub engines_running: usize,
    pub processes_reconciled: usize,
    pub processes_failed: usize,
    pub detail: Vec<DaemonEngineLine>,
    pub process_detail: Vec<DaemonProcessLine>,
}

#[derive(Serialize, Deserialize)]
pub struct DaemonEngineLine {
    pub project: String,
    pub service: String,
    pub provider: String,
    pub running: bool,
}

#[derive(Serialize, Deserialize)]
pub struct DaemonProcessLine {
    pub project: String,
    pub workspace: String,
    pub process: String,
    pub action: String,
    pub success: bool,
}

/// Run a single reconcile pass and persist the status file.
async fn reconcile_once(interval: u64) -> Result<DaemonStatus> {
    let results = crate::services::factory::reconcile_all_projects().await;
    let process_results = crate::processes::reconcile_all_projects_processes().await;

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

    crate::services::ownership::atomic_write(
        &status_path()?,
        &serde_json::to_vec_pretty(&status)?,
    )?;
    Ok(status)
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}
