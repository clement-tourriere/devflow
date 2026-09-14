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
use std::{collections::BTreeMap, sync::Mutex};

fn repository() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    support::init_repository(root.path(), "trunk");
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
    support::init_repository(&old, "trunk");
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
    std::fs::write(a.path().join("lock"), "v1").unwrap();
    std::fs::write(b.path().join("lock"), "v1").unwrap();
    std::fs::write(project.path().join("artifacts/value"), "original").unwrap();
    let mut config = Config::default();
    config.worktree.caches = vec![devflow_core::cache::CacheSpec {
        path: "artifacts".into(),
        inputs: vec!["lock".into()],
        policy: devflow_core::cache::CachePolicy::Copy,
        max_bytes: 1024,
    }];
    #[cfg(unix)]
    {
        std::fs::create_dir(project.path().join("artifacts/bin")).unwrap();
        std::os::unix::fs::symlink("../value", project.path().join("artifacts/bin/tool")).unwrap();
    }
    assert!(
        devflow_core::cache::materialize(&config, project.path(), project.path())
            .unwrap()
            .is_empty()
    );
    let incompatible = tempfile::tempdir().unwrap();
    std::fs::write(incompatible.path().join("lock"), "another branch").unwrap();
    let miss =
        devflow_core::cache::materialize(&config, project.path(), incompatible.path()).unwrap();
    assert!(miss[0].miss_reason.is_some());
    assert!(!incompatible.path().join("artifacts").exists());
    assert_eq!(
        std::fs::read_to_string(project.path().join("artifacts/value")).unwrap(),
        "original"
    );
    let first = devflow_core::cache::materialize(&config, project.path(), a.path()).unwrap();
    assert!(!first[0].reused);
    std::fs::write(a.path().join("artifacts/value"), "changed").unwrap();
    #[cfg(unix)]
    {
        assert_eq!(
            std::fs::read_link(a.path().join("artifacts/bin/tool")).unwrap(),
            std::path::Path::new("../value")
        );
        assert_eq!(
            std::fs::read_to_string(a.path().join("artifacts/bin/tool")).unwrap(),
            "changed"
        );
    }
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
    // A different source branch must not invalidate this destination's inputs.
    assert!(devflow_core::cache::materialize(&config, project.path(), a.path()).unwrap()[0].reused);
    std::fs::write(a.path().join("lock"), "v2").unwrap();
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
    #[cfg(unix)]
    {
        let escaped = tempfile::tempdir().unwrap();
        std::fs::write(escaped.path().join("lock"), "unsafe").unwrap();
        std::fs::write(project.path().join("lock"), "unsafe").unwrap();
        std::os::unix::fs::symlink("../lock", project.path().join("artifacts/escape")).unwrap();
        assert!(
            devflow_core::cache::materialize(&config, project.path(), escaped.path())
                .unwrap_err()
                .to_string()
                .contains("escapes")
        );
        assert!(!escaped.path().join("artifacts").exists());
    }
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

#[tokio::test]
async fn operation_cancellation_accepts_only_proven_no_effect_failures() {
    let _environment = support::isolated_config();
    let root = repository();
    let mut rejected = OperationJournal::begin(root.path(), "trunk", "remove", None, None, false)
        .await
        .unwrap();
    let id = rejected.record.id.clone();
    let failure = rejected.fail(anyhow::anyhow!("policy rejected"));
    assert_eq!(
        failure
            .downcast_ref::<devflow_core::workspace::operation::OperationError>()
            .unwrap()
            .operation
            .status,
        "cancelled"
    );
    drop(rejected);
    assert!(OperationJournal::pending(root.path()).unwrap().is_empty());
    assert_eq!(
        OperationJournal::cancel(root.path(), &id)
            .await
            .unwrap()
            .status,
        "cancelled"
    );

    let mut legacy = OperationJournal::begin(root.path(), "task", "remove", None, None, false)
        .await
        .unwrap();
    legacy.record.schema_version = 2;
    legacy.record.mutation_started = true;
    let legacy_id = legacy.record.id.clone();
    legacy.fail(anyhow::anyhow!(
        "Refusing to delete workspace 'task':\nworkspace is dirty"
    ));
    drop(legacy);
    assert_eq!(
        OperationJournal::cancel(root.path(), &legacy_id)
            .await
            .unwrap()
            .status,
        "cancelled"
    );

    let mut partial = OperationJournal::begin(root.path(), "partial", "remove", None, None, false)
        .await
        .unwrap();
    partial.phase("remove-resources").unwrap();
    let partial_id = partial.record.id.clone();
    partial.fail(anyhow::anyhow!("provider failed after deletion started"));
    drop(partial);
    assert!(OperationJournal::cancel(root.path(), &partial_id)
        .await
        .is_err());
    assert_eq!(OperationJournal::pending(root.path()).unwrap().len(), 1);
}

#[tokio::test]
async fn cancelled_execution_kills_its_child_and_records_interruption() {
    use devflow_core::execution::{self, records::ExecutionState, CommandSpec};
    let _environment = support::isolated_config();
    let root = repository();
    let spec = CommandSpec::Program {
        program: "sleep".into(),
        args: vec!["30".into()],
    };
    {
        let config = Config::default();
        let execution =
            execution::execute_in_workspace(&config, root.path(), "trunk", &spec, false, true);
        tokio::pin!(execution);
        tokio::select! {
            result = &mut execution => panic!("long execution ended early: {result:?}"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(300)) => (),
        }
    }
    let record = execution::records::list(root.path()).unwrap().remove(0);
    assert!(matches!(record.state, ExecutionState::Interrupted));
    let child = record.child.unwrap();
    for _ in 0..100 {
        if !child.is_alive() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(!child.is_alive());
}

#[cfg(unix)]
#[tokio::test]
async fn project_teardown_rejects_dirty_worktrees_and_retains_state_after_cleanup_failure() {
    use devflow_core::{
        project::{destroy, DestroyOptions},
        vcs::{GitRepository, VcsProvider},
    };
    use std::os::unix::fs::PermissionsExt;
    let _environment = support::isolated_config();
    let root = repository();
    let mut config = Config::default();
    config.git.main_workspace = "trunk".into();
    config.worktree.path_template = "worktrees/{workspace}".into();
    let config_path = root.path().join(".devflow.yml");
    std::fs::write(&config_path, serde_yaml_ng::to_string(&config).unwrap()).unwrap();
    let options = application::SwitchOptions {
        create_if_missing: true,
        lifecycle: devflow_core::workspace::LifecycleOptions {
            skip_hooks: true,
            skip_services: true,
            skip_processes: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let switched = application::switch_workspace(&config, root.path(), "task", &options)
        .await
        .unwrap();
    assert!(switched.ready(), "{switched:?}");
    let path = switched.worktree.unwrap().path;
    std::fs::create_dir(path.join("blocked")).unwrap();
    std::fs::write(path.join("blocked/value"), "preserve").unwrap();
    assert!(destroy(root.path(), DestroyOptions::default(), None)
        .await
        .is_err());
    assert_eq!(
        std::fs::read_to_string(path.join("blocked/value")).unwrap(),
        "preserve"
    );
    std::fs::set_permissions(path.join("blocked"), std::fs::Permissions::from_mode(0o500)).unwrap();
    let result = destroy(
        root.path(),
        DestroyOptions {
            force_worktrees: true,
        },
        None,
    )
    .await
    .unwrap();
    if path.join("blocked").exists() {
        std::fs::set_permissions(path.join("blocked"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
    }
    if !result.succeeded() {
        let receipts = root.path().join(".git/devflow-removals");
        for entry in std::fs::read_dir(receipts).unwrap() {
            let receipt = entry.unwrap().path();
            if receipt.extension().is_some_and(|ext| ext == "json") {
                let value: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(receipt).unwrap()).unwrap();
                let staged = std::path::PathBuf::from(value["staged"].as_str().unwrap());
                if staged.join("blocked").exists() {
                    std::fs::set_permissions(
                        staged.join("blocked"),
                        std::fs::Permissions::from_mode(0o700),
                    )
                    .unwrap();
                }
            }
        }
        assert!(!result.state_cleared);
        assert!(result
            .resources
            .iter()
            .any(|r| r.kind == "worktree" && !r.success));
        assert!(config_path.exists());
        assert!(LocalStateManager::new()
            .unwrap()
            .get_workspace_by_dir(root.path(), "task")
            .is_some());
        assert!(GitRepository::new(root.path())
            .unwrap()
            .worktree_path("task")
            .unwrap()
            .is_some());
        assert!(destroy(
            root.path(),
            DestroyOptions {
                force_worktrees: true
            },
            None
        )
        .await
        .unwrap()
        .succeeded());
    }
}

#[tokio::test]
async fn controller_start_tolerates_a_status_probe_holding_the_lease() {
    let _environment = support::isolated_config();
    let path = devflow_core::paths::devflow_config_dir()
        .unwrap()
        .join("daemon.lock");
    let probe = devflow_core::services::ownership::OperationLease::try_acquire(&path)
        .unwrap()
        .unwrap();
    let start = devflow_core::controller::run_once(30);
    tokio::pin!(start);
    tokio::select! {
        result = &mut start => panic!("startup confused a probe with an owner: {:?}", result.err()),
        _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => (),
    }
    drop(probe);
    start.await.unwrap();
    assert!(devflow_core::controller::owner().unwrap().is_none());
}
