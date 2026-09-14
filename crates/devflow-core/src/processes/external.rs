//! Small external-supervisor boundary for records from the retired embedding.
use anyhow::{Context, Result};
use std::{path::Path, time::Duration};

pub(super) async fn stop_legacy_pitchfork(id: Option<&str>, project: &Path) -> Result<()> {
    let Some(id) = id else {
        return Ok(());
    };
    anyhow::ensure!(
        id.starts_with("df") && id.contains('/') && !id.starts_with('-'),
        "invalid recorded devflow supervisor identity"
    );
    let Ok(binary) = which::which("pitchfork") else {
        // Embedded supervisors died with their original CLI. The native stop
        // path below still stops any surviving process from its saved record.
        log::warn!(
            "External pitchfork CLI unavailable; stopping the recorded legacy process directly"
        );
        return Ok(());
    };
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(binary)
            .args(["stop", "--", id])
            .current_dir(project)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("external supervisor stop timed out")??;
    anyhow::ensure!(
        output.status.success(),
        "external supervisor could not stop {id}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}
