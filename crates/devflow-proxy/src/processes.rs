//! Discovery of devflow-managed host processes for proxy routing.
//!
//! Process records are written by `devflow-core` under the user state
//! directory. The proxy reads those records directly so it can route
//! `process.workspace.project.<suffix>` to `127.0.0.1:<port>` without needing
//! Docker or a dependency cycle back to core.

use crate::discovery::ProxyTarget;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

const PROCESS_CONTAINER_PREFIX: &str = "devflow-process:";

#[derive(Debug, Deserialize)]
struct ProcessStateRecord {
    process: String,
    workspace: String,
    project_key: String,
    project_name: String,
    pid: Option<u32>,
    ports: Vec<u16>,
    status: String,
}

/// Return whether a proxy target comes from a devflow host process record.
pub fn is_process_target(target: &ProxyTarget) -> bool {
    target.container_id.starts_with(PROCESS_CONTAINER_PREFIX)
}

/// Discover currently recorded devflow processes from the configured state dir.
pub fn discover_process_targets(domain_suffix: &str) -> Vec<ProxyTarget> {
    let Ok(root) = process_state_root() else {
        return Vec::new();
    };
    discover_process_targets_from(&root, domain_suffix)
}

fn discover_process_targets_from(root: &Path, domain_suffix: &str) -> Vec<ProxyTarget> {
    let mut out = Vec::new();
    if !root.exists() {
        return out;
    }

    let Ok(projects) = fs::read_dir(root) else {
        return out;
    };
    for project in projects.flatten() {
        let workspaces_dir = project.path().join("workspaces");
        let Ok(workspaces) = fs::read_dir(workspaces_dir) else {
            continue;
        };
        for workspace in workspaces.flatten() {
            let processes_dir = workspace.path().join("processes");
            let Ok(records) = fs::read_dir(processes_dir) else {
                continue;
            };
            for record in records.flatten() {
                if record.path().extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let Ok(content) = fs::read_to_string(record.path()) else {
                    continue;
                };
                let Ok(record) = serde_json::from_str::<ProcessStateRecord>(&content) else {
                    continue;
                };
                if let Some(target) = process_record_to_target(record, domain_suffix) {
                    out.push(target);
                }
            }
        }
    }
    out
}

fn process_record_to_target(
    record: ProcessStateRecord,
    domain_suffix: &str,
) -> Option<ProxyTarget> {
    let pid = record.pid?;
    if !matches!(record.status.as_str(), "running" | "ready") || !process_alive(pid) {
        return None;
    }
    let port = *record.ports.first()?;
    let service = sanitize_label(&record.process);
    let workspace = sanitize_label(&record.workspace);
    let project = sanitize_label(&record.project_name);
    let suffix = domain_suffix.trim_start_matches('.').to_ascii_lowercase();
    let domain = format!("{service}.{workspace}.{project}.{suffix}");
    let container_id = format!(
        "{PROCESS_CONTAINER_PREFIX}{}:{}:{}",
        record.project_key, record.workspace, record.process
    );

    Some(ProxyTarget {
        domain,
        container_ip: "127.0.0.1".to_string(),
        port,
        container_id,
        container_name: format!("process:{}:{}", record.workspace, record.process),
        project: Some(project),
        service: Some(service),
        workspace: Some(workspace),
    })
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    kill(Pid::from_raw(pid as i32), None).is_ok()
}

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    true
}

fn process_state_root() -> anyhow::Result<PathBuf> {
    if let Ok(path) = std::env::var("DEVFLOW_PROCESS_STATE_DIR") {
        return Ok(PathBuf::from(path));
    }
    Ok(dirs::state_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/state")))
        .ok_or_else(|| anyhow::anyhow!("failed to resolve user state directory"))?
        .join("devflow")
        .join("processes"))
}

fn sanitize_label(input: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in input.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "process".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn discovers_process_target_from_state_record() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp
            .path()
            .join("projecthash")
            .join("workspaces")
            .join("feature-auth")
            .join("processes");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("api.json"),
            format!(
                r#"{{
              "process":"api",
              "workspace":"feature/auth",
              "project_key":"/repo/app",
              "project_name":"My App",
              "pid":{},
              "ports":[3007],
              "status":"ready"
            }}"#,
                std::process::id()
            ),
        )
        .unwrap();

        let targets = discover_process_targets_from(tmp.path(), "local");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].domain, "api.feature-auth.my-app.local");
        assert_eq!(targets[0].container_ip, "127.0.0.1");
        assert_eq!(targets[0].port, 3007);
        assert!(is_process_target(&targets[0]));
    }

    #[test]
    fn skips_stopped_or_unported_records() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp
            .path()
            .join("projecthash")
            .join("workspaces")
            .join("main")
            .join("processes");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("worker.json"),
            r#"{
              "process":"worker",
              "workspace":"main",
              "project_key":"/repo/app",
              "project_name":"app",
              "pid":null,
              "ports":[],
              "status":"stopped"
            }"#,
        )
        .unwrap();

        assert!(discover_process_targets_from(tmp.path(), "local").is_empty());
    }
}
