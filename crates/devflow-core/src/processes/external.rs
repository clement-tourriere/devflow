//! Migration guard for records owned by a retired external supervisor.
use anyhow::Result;
use std::path::Path;

pub(super) async fn stop_legacy_pitchfork(id: Option<&str>, _project: &Path) -> Result<()> {
    if let Some(id) = id {
        anyhow::bail!("Process is owned by the retired Pitchfork supervisor ({id}); stop it with that supervisor, then forget the stopped devflow record before restarting with the native runtime");
    }
    Ok(())
}
