//! Cross-layer contracts exercised without live cloud credentials.
mod support;
use anyhow::Result;
use async_trait::async_trait;
use devflow_core::{
    application,
    config::Config,
    services::{ConnectionInfo, DoctorReport, ServiceProvider, WorkspaceInfo},
    state::LocalStateManager,
    workspace::operation::OperationJournal,
};
use std::{collections::BTreeMap, path::Path, sync::Mutex};

fn git(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn repository() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-b", "trunk"]);
    git(
        root.path(),
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "fixture",
        ],
    );
    root
}
struct Fake {
    workspaces: Mutex<BTreeMap<String, WorkspaceInfo>>,
    deleted: Mutex<Vec<String>>,
}
impl Fake {
    fn new(names: &[(&str, Option<&str>)]) -> Self {
        Self {
            workspaces: Mutex::new(
                names
                    .iter()
                    .map(|(name, parent)| {
                        (
                            name.to_string(),
                            WorkspaceInfo {
                                name: name.to_string(),
                                created_at: Some(chrono::Utc::now()),
                                parent_workspace: parent.map(str::to_string),
                                database_name: name.to_string(),
                                state: Some("ready".into()),
                            },
                        )
                    })
                    .collect(),
            ),
            deleted: Mutex::new(Vec::new()),
        }
    }
}
#[async_trait]
impl ServiceProvider for Fake {
    async fn create_workspace(&self, name: &str, parent: Option<&str>) -> Result<WorkspaceInfo> {
        let workspace = WorkspaceInfo {
            name: name.into(),
            parent_workspace: parent.map(str::to_string),
            created_at: Some(chrono::Utc::now()),
            database_name: name.into(),
            state: Some("ready".into()),
        };
        self.workspaces
            .lock()
            .unwrap()
            .insert(name.into(), workspace.clone());
        Ok(workspace)
    }
    async fn delete_workspace(&self, name: &str) -> Result<()> {
        self.deleted.lock().unwrap().push(name.into());
        anyhow::ensure!(name != "broken", "injected provider deletion failure");
        self.workspaces.lock().unwrap().remove(name);
        Ok(())
    }
    async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        Ok(self.workspaces.lock().unwrap().values().cloned().collect())
    }
    async fn workspace_exists(&self, name: &str) -> Result<bool> {
        Ok(self.workspaces.lock().unwrap().contains_key(name))
    }
    async fn switch_to_branch(&self, name: &str) -> Result<WorkspaceInfo> {
        self.workspaces
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing"))
    }
    async fn get_connection_info(&self, _: &str) -> Result<ConnectionInfo> {
        anyhow::bail!("fixture has no connection")
    }
    async fn doctor(&self) -> Result<DoctorReport> {
        Ok(DoctorReport { checks: vec![] })
    }
    async fn test_connection(&self) -> Result<()> {
        Ok(())
    }
    fn provider_name(&self) -> &'static str {
        "fixture"
    }
}

#[tokio::test]
async fn cleanup_protects_configured_default_pins_and_data_dependencies_and_reports_failures() {
    let _environment = support::isolated_config();
    let root = repository();
    let mut config = Config::default();
    config.git.main_workspace = "trunk".into();
    LocalStateManager::new()
        .unwrap()
        .update_control(root.path(), |c| {
            c.workspace("pinned").pinned = true;
            Ok(())
        })
        .unwrap();
    let provider = Fake::new(&[
        ("trunk", None),
        ("scratch", None),
        ("pinned", None),
        ("parent", None),
        ("child", Some("parent")),
        ("broken", None),
    ]);
    let preview = application::plan_service_cleanup(&config, root.path(), "db", &provider, 0)
        .await
        .unwrap();
    assert!(
        !preview
            .entries
            .iter()
            .find(|e| e.workspace == "trunk")
            .unwrap()
            .delete
    );
    assert!(
        !preview
            .entries
            .iter()
            .find(|e| e.workspace == "pinned")
            .unwrap()
            .delete
    );
    assert!(
        !preview
            .entries
            .iter()
            .find(|e| e.workspace == "parent")
            .unwrap()
            .delete
    );
    let result = application::cleanup_service(&config, root.path(), "db", &provider, 0)
        .await
        .unwrap();
    assert!(result.failures.contains_key("broken"));
    assert_eq!(result.operation.status, "needs_attention");
    let deleted = provider.deleted.lock().unwrap();
    assert!(!deleted
        .iter()
        .any(|n| matches!(n.as_str(), "trunk" | "pinned" | "parent")));
    assert!(deleted.contains(&"scratch".to_string()));
}

#[tokio::test]
async fn relocation_preserves_identity_namespace_and_operation_history() {
    let _environment = support::isolated_config();
    let root = tempfile::tempdir().unwrap();
    let old = root.path().join("old");
    let new = root.path().join("new");
    std::fs::create_dir(&old).unwrap();
    git(&old, &["init", "-b", "trunk"]);
    git(
        &old,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "fixture",
        ],
    );
    let mut registry = LocalStateManager::new().unwrap();
    registry.ensure_default_workspace(&old, "trunk").unwrap();
    let before = LocalStateManager::new().unwrap().control(&old);
    let mut operation = OperationJournal::begin(&old, "trunk", "switch", None, None, false)
        .await
        .unwrap();
    let id = operation.record.id.clone();
    operation.finish(true).unwrap();
    drop(operation);
    std::fs::rename(&old, &new).unwrap();
    LocalStateManager::new()
        .unwrap()
        .relocate_project(&old, &new)
        .unwrap();
    let after = LocalStateManager::new().unwrap().control(&new);
    assert_eq!(before.id, after.id);
    assert_eq!(before.resource_identity, after.resource_identity);
    assert_eq!(before.workspaces["trunk"].id, after.workspaces["trunk"].id);
    assert_eq!(
        OperationJournal::inspect(&new, &id).unwrap().status,
        "ready"
    );
    let mut next = OperationJournal::begin(&new, "trunk", "switch", None, None, false)
        .await
        .unwrap();
    assert_ne!(next.record.id, id);
    next.finish(true).unwrap();
    drop(next);
    assert_eq!(OperationJournal::records(&new, true).unwrap().len(), 2);
}

#[tokio::test]
async fn independent_workspaces_share_policy_lease_but_retention_waits() {
    let _environment = support::isolated_config();
    let root = repository();
    let a = OperationJournal::begin(root.path(), "a", "switch", None, None, true)
        .await
        .unwrap();
    let b = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        OperationJournal::begin(root.path(), "b", "switch", None, None, true),
    )
    .await
    .unwrap()
    .unwrap();
    let blocked = tokio::time::timeout(
        std::time::Duration::from_millis(120),
        OperationJournal::begin(
            root.path(),
            "__retention__",
            "cleanup:db",
            None,
            None,
            false,
        ),
    )
    .await;
    assert!(blocked.is_err());
    drop(a);
    drop(b);
    let _cleanup = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        OperationJournal::begin(
            root.path(),
            "__retention__",
            "cleanup:db",
            None,
            None,
            false,
        ),
    )
    .await
    .unwrap()
    .unwrap();
}

#[test]
fn cache_copies_are_independent_and_shared_artifacts_are_read_only() {
    let _environment = support::isolated_config();
    let project = tempfile::tempdir().unwrap();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    std::fs::create_dir(project.path().join("artifacts")).unwrap();
    std::fs::write(project.path().join("lock"), "v1").unwrap();
    std::fs::write(project.path().join("artifacts/value"), "original").unwrap();
    let mut config = Config::default();
    config.worktree.caches = vec![devflow_core::cache::CacheSpec {
        path: "artifacts".into(),
        inputs: vec!["lock".into()],
        policy: devflow_core::cache::CachePolicy::Copy,
        max_bytes: 1024,
    }];
    let first = devflow_core::cache::materialize(&config, project.path(), a.path()).unwrap();
    assert!(!first[0].reused);
    std::fs::write(a.path().join("artifacts/value"), "changed").unwrap();
    config.worktree.caches[0].policy = devflow_core::cache::CachePolicy::SharedReadOnly;
    let second = devflow_core::cache::materialize(&config, project.path(), b.path()).unwrap();
    assert!(second[0].reused);
    assert_eq!(
        std::fs::read_to_string(b.path().join("artifacts/value")).unwrap(),
        "original"
    );
    assert!(std::fs::metadata(b.path().join("artifacts/value"))
        .unwrap()
        .permissions()
        .readonly());
    assert!(!std::fs::metadata(project.path().join("artifacts/value"))
        .unwrap()
        .permissions()
        .readonly());
    config.worktree.caches[0].policy = devflow_core::cache::CachePolicy::Copy;
    devflow_core::cache::materialize(&config, project.path(), a.path()).unwrap();
    std::fs::write(project.path().join("lock"), "v2").unwrap();
    assert!(
        devflow_core::cache::materialize(&config, project.path(), a.path())
            .unwrap_err()
            .to_string()
            .contains("generation")
    );
    assert_eq!(
        std::fs::read_to_string(a.path().join("artifacts/value")).unwrap(),
        "changed"
    );
    config.worktree.caches[0].path = "../escape".into();
    assert!(devflow_core::cache::materialize(&config, project.path(), a.path()).is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn plugin_negotiates_capabilities_and_receives_scoped_operation_identity() {
    let _environment = support::isolated_config();
    let root = repository();
    let script = root.path().join("plugin.py");
    std::fs::write(
        &script,
        r#"#!/usr/bin/python3
import json,sys
r=json.load(sys.stdin)
with open('requests.jsonl','a') as f: f.write(json.dumps(r)+'\n')
if r['method']=='negotiate': result={'protocol_version':1,'operations':{'start':True}}
elif r['method']=='list_workspaces': result=[]
elif r['method']=='workspace_exists': result=True
elif r['method']=='start_workspace': result=None
else: raise Exception('unexpected method '+r['method'])
print(json.dumps({'ok':True,'result':result}))
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config: devflow_core::config::PluginConfig =
        serde_json::from_value(serde_json::json!({"path":"./plugin.py","timeout":5})).unwrap();
    let provider =
        devflow_core::services::plugin::PluginProvider::connect("db", &config, root.path())
            .await
            .unwrap();
    assert!(provider.operations().start);
    assert!(!provider.operations().reset);
    let result = application::service_operation(
        &Config::default(),
        root.path(),
        "db",
        "task",
        &provider,
        devflow_core::services::ServiceOperation::Start,
    )
    .await
    .unwrap();
    let requests = std::fs::read_to_string(root.path().join("requests.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let start = requests
        .iter()
        .find(|r| r["method"] == "start_workspace")
        .unwrap();
    assert_eq!(start["operation_id"], result.operation.id);
    assert_eq!(start["protocol_version"], 1);
    assert!(start["resource_id"].as_str().unwrap().len() >= 32);
    assert!(start["deadline"].is_string());
}

#[cfg(unix)]
#[tokio::test]
async fn failed_project_destroy_keeps_configuration_and_ownership_for_retry() {
    let _environment = support::isolated_config();
    let root = repository();
    let script = root.path().join("plugin.py");
    std::fs::write(&script, r#"#!/usr/bin/python3
import json,sys
r=json.load(sys.stdin)
if r['method']=='negotiate': result={'protocol_version':1}
elif r['method']=='list_workspaces': result=[{'name':'trunk','database_name':'db'}]
elif r['method']=='delete_workspace':
    print(json.dumps({'ok':False,'error':{'code':'busy','message':'provider unavailable','retryable':True}}))
    sys.exit(0)
else: raise Exception(r['method'])
print(json.dumps({'ok':True,'result':result}))
"#).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config_path = root.path().join(".devflow.yml");
    std::fs::write(
        &config_path,
        "services:\n  - name: db\n    service_type: plugin\n    plugin:\n      path: ./plugin.py\n",
    )
    .unwrap();
    let id = LocalStateManager::new()
        .unwrap()
        .register_project(root.path(), None)
        .unwrap();
    let result = devflow_core::project::destroy(root.path(), Default::default(), None)
        .await
        .unwrap();
    assert!(!result.succeeded());
    assert!(!result.services_destroyed[0].success);
    assert!(result.services_destroyed[0]
        .error
        .as_deref()
        .unwrap()
        .contains("provider unavailable"));
    assert!(config_path.is_file());
    assert!(!result.state_cleared);
    assert_eq!(
        LocalStateManager::new().unwrap().control(root.path()).id,
        id
    );
}
