//! Receipts: version 1 files migrate to version 2 field for field, in place,
//! with no provisioning effect.
use devflow_core::Config;
use devflow_core::config::Engine;
use devflow_core::receipt::{Phase, PhaseState, Receipt, Status, VERSION};
use serde_json::{Value, json};
use std::path::Path;

const PROJECT: &str = "0b3c9d2e-7f4a-4c1e-9a52-6d1f3b8e2a40";
const INSTANCE: &str = "7d0e8f3a-52b1-4a77-9f3e-0a1b2c3d4e5f";
const BRANCH: &str = "4c1d9e22-8a0b-4b55-a7c4-2f6e9d1b3a58";
const URL: &str = "postgres://dfl_a1b2c3d4e5f6:s3cr3tpass@127.0.0.1:20001/df_0b3c9d2e_a1b2c3d4e5f6"; // kingfisher:ignore
const ATTACHED: &str = "postgres://app:attachpw@db.internal:6543/shop_ci?sslmode=require"; // kingfisher:ignore

fn phase(state: &str, detail: Option<&str>, at: &str) -> Value {
    let mut record = json!({ "state": state, "updated_at": at });
    if let Some(detail) = detail {
        record["detail"] = json!(detail);
    }
    record
}

fn base(workspace: &str, phases: Value) -> Value {
    json!({
        "version": 1,
        "instance_id": INSTANCE,
        "project_id": PROJECT,
        "workspace": workspace,
        "code_source": "main",
        "phases": phases,
        "created_at": "2026-10-09T08:15:30.250Z",
        "updated_at": "2026-10-09T08:16:02.500Z",
    })
}

/// Setup stopped at the database phase with a failure, hooks untrusted.
fn interrupted() -> Value {
    let mut receipt = base(
        "feature/login",
        json!({
            "materialize": phase("done", Some("created /tmp/shop.worktrees/feature-login"), "2026-10-09T08:15:30.500Z"),
            "payload": phase("done", Some("2 of 2 entries copied from /tmp/shop"), "2026-10-09T08:15:31.250Z"),
            "database": phase("failed", Some("cannot reach the database server: connection refused"), "2026-10-09T08:15:40.750Z"),
            "hooks": {
                "state": "skipped",
                "detail": "setup hooks are not trusted; review them with `devflow trust`, then re-run `devflow setup`",
                "required_skip": true,
                "updated_at": "2026-10-09T08:15:41.125Z",
            },
        }),
    );
    receipt["data_source"] = json!("workspace:main");
    receipt["data_source_id"] = json!(format!("branch:{BRANCH}"));
    receipt
}

/// `--no-db` and `--no-hooks`: skips someone asked for.
fn deliberate_skips() -> Value {
    base(
        "agent/task",
        json!({
            "materialize": phase("done", Some("created /tmp/shop.worktrees/agent-task"), "2026-10-09T08:15:30.500Z"),
            "payload": phase("done", Some("nothing to copy"), "2026-10-09T08:15:31.250Z"),
            "database": phase("skipped", Some("--no-db"), "2026-10-09T08:15:31.500Z"),
            "env": phase("done", Some("5 variables"), "2026-10-09T08:15:31.750Z"),
            "hooks": phase("skipped", Some("--no-hooks"), "2026-10-09T08:15:32.125Z"),
        }),
    )
}

/// Attached with `--database-url`: no branch, not owned, an adopted checkout.
fn url_attachment() -> Value {
    let mut receipt = base(
        "sandbox",
        json!({
            "materialize": phase("done", Some("adopted existing worktree"), "2026-10-09T08:15:30.500Z"),
            "payload": phase("done", Some("nothing to copy"), "2026-10-09T08:15:31.250Z"),
            "database": phase("done", Some("attached to the given database URL"), "2026-10-09T08:15:31.500Z"),
            "env": phase("done", Some("12 variables"), "2026-10-09T08:15:31.750Z"),
            "hooks": phase("done", Some("2 commands"), "2026-10-09T08:15:35.375Z"),
        }),
    );
    receipt.as_object_mut().unwrap().remove("code_source");
    receipt["data"] = json!({
        "database": "shop_ci",
        "role": "app",
        "url": ATTACHED,
        "source": "url",
        "owned": false,
    });
    receipt["hooks_digest"] =
        json!("9f2c4b7e1a6d8035c4e7b2a91d0f5e68a3b7c1d4e9f20586b3a7d1c4e8f09a2b");
    receipt
}

/// A VM fork after `setup --new-instance`: late phases pending, the old
/// branch pinned as the clone source, no binding yet.
fn forked() -> Value {
    let mut receipt = base(
        "vm/one",
        json!({
            "materialize": phase("done", Some("adopted existing worktree"), "2026-10-09T08:15:30.500Z"),
            "payload": phase("done", Some("1 of 1 entries copied from /tmp/shop"), "2026-10-09T08:15:31.250Z"),
            "database": phase("pending", None, "2026-10-09T08:20:00.500Z"),
            "env": phase("pending", None, "2026-10-09T08:20:00.500Z"),
            "hooks": phase("pending", None, "2026-10-09T08:20:00.500Z"),
        }),
    );
    receipt["data_source"] = json!(format!("branch:{BRANCH}"));
    receipt["data_source_id"] = json!(format!("branch:{BRANCH}"));
    receipt
}

/// A finished workspace that owns its branch.
fn owned() -> Value {
    let mut receipt = base(
        "feature/search",
        json!({
            "materialize": phase("done", Some("created /tmp/shop.worktrees/feature-search"), "2026-10-09T08:15:30.500Z"),
            "payload": phase("done", Some("2 of 2 entries copied from /tmp/shop"), "2026-10-09T08:15:31.250Z"),
            "database": phase("done", Some("branch df_0b3c9d2e_a1b2c3d4e5f6 from workspace:main"), "2026-10-09T08:15:33.500Z"),
            "env": phase("done", Some("14 variables"), "2026-10-09T08:15:33.750Z"),
            "hooks": phase("done", Some("3 commands"), "2026-10-09T08:15:50.625Z"),
        }),
    );
    receipt["data_source"] = json!("workspace:main");
    receipt["data_source_id"] = json!(format!("branch:{BRANCH}"));
    receipt["data"] = json!({
        "branch_id": BRANCH,
        "database": "df_0b3c9d2e_a1b2c3d4e5f6",
        "role": "dfl_a1b2c3d4e5f6",
        "url": URL,
        "source": "workspace:main",
        "owned": true,
    });
    receipt["hooks_digest"] =
        json!("1d7a3c5e9b2f4086a1c3e5d7f9b2a4c6e8d0f1a3b5c7d9e2f4a6b8c0d1e3f5a7");
    receipt
}

fn fixtures() -> Vec<(&'static str, Value)> {
    vec![
        ("interrupted", interrupted()),
        ("deliberate_skips", deliberate_skips()),
        ("url_attachment", url_attachment()),
        ("forked", forked()),
        ("owned", owned()),
    ]
}

fn config(postgres_key: Option<&str>) -> Config {
    let service = postgres_key
        .map(|key| format!("[services.{key}]\nengine = \"postgres\"\n"))
        .unwrap_or_default();
    Config::parse(&format!(
        "[project]\nname = \"shop\"\nid = \"{PROJECT}\"\n{service}"
    ))
    .unwrap()
}

fn write(dir: &Path, receipt: &Value) {
    std::fs::create_dir_all(dir.join("devflow")).unwrap();
    std::fs::write(
        Receipt::path(dir),
        serde_json::to_vec_pretty(receipt).unwrap(),
    )
    .unwrap();
}

fn on_disk(dir: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(Receipt::path(dir)).unwrap()).unwrap()
}

/// What a migration says about the phases it reopens.
const REOPENED_ENV: &str = "rewrite .devflow/env";
const REOPENED_HOOKS: &str = "run again after the environment is rewritten";

/// The version 2 document a version 1 one must become: nothing lost, `data`
/// under the service key, `database` renamed, requested skips marked, and the
/// environment (with the hooks that follow it) reopened rather than stamped
/// with a generation nothing was written under. The reopened phases keep their
/// timestamps; their new detail is taken from `stored`, after checking it.
fn expected(v1: &Value, key: Option<&str>, config: &Config, stored: &Value) -> Value {
    let mut v2 = v1.clone();
    let object = v2.as_object_mut().unwrap();
    object.insert("version".into(), json!(2));
    let phases = object.get_mut("phases").unwrap().as_object_mut().unwrap();
    if let Some(record) = phases.remove("database") {
        phases.insert("services".into(), record);
    }
    for record in phases.values_mut() {
        let detail = record["detail"].as_str().unwrap_or_default().to_string();
        let asked = ["--no-db", "--no-hooks"].contains(&detail.as_str());
        if record["state"] == "skipped" && record.get("required_skip").is_none() && asked {
            record["deliberate"] = json!(true);
        }
    }
    if v1["phases"]["env"]["state"] == "done" {
        for (name, needle) in [("env", REOPENED_ENV), ("hooks", REOPENED_HOOKS)] {
            // Hooks that ran run again; a skip someone asked for stays.
            let Some(record) = phases.get_mut(name).filter(|r| r["state"] == "done") else {
                continue;
            };
            let detail = stored["phases"][name]["detail"].clone();
            assert!(
                detail.as_str().unwrap().contains(needle),
                "{name}: {detail}"
            );
            record["state"] = json!("skipped");
            record["required_skip"] = json!(true);
            record["detail"] = detail;
        }
    }
    if let Some(mut data) = object.remove("data") {
        match key {
            Some(key) => {
                data["engine"] = json!("postgres");
                data["applied"] = json!(config.service_generation(key).unwrap());
                object.insert("services".into(), json!({ key: data }));
            }
            None => {
                object.insert("unmapped".into(), data);
            }
        }
    }
    v2
}

#[test]
fn every_version_one_field_survives_the_migration() {
    for (name, v1) in fixtures() {
        let config = config(Some("main-db"));
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &v1);
        let lock = Receipt::try_lock(dir.path()).unwrap().unwrap();
        let migrated = Receipt::migrate_in_place(dir.path(), &config, &lock)
            .unwrap()
            .unwrap();
        assert_eq!(migrated.version, VERSION, "{name}");
        let stored = on_disk(dir.path());
        assert_eq!(
            stored,
            expected(&v1, Some("main-db"), &config, &stored),
            "{name}"
        );
        assert!(
            stored.get("env_generation").is_none(),
            "{name}: no generation is claimed for a file version 1 wrote"
        );
        // Reading it again, from disk, gives the same receipt, and a second
        // migration changes nothing.
        assert_eq!(
            Receipt::load(dir.path()).unwrap().unwrap(),
            migrated,
            "{name}"
        );
        let before = std::fs::read(Receipt::path(dir.path())).unwrap();
        Receipt::migrate_in_place(dir.path(), &config, &lock).unwrap();
        assert_eq!(std::fs::read(Receipt::path(dir.path())).unwrap(), before);
    }
}

#[test]
fn without_a_postgres_service_the_binding_is_kept_as_unmapped() {
    for (name, v1) in fixtures() {
        let config = config(None);
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &v1);
        let lock = Receipt::try_lock(dir.path()).unwrap().unwrap();
        let migrated = Receipt::migrate_in_place(dir.path(), &config, &lock)
            .unwrap()
            .unwrap();
        let stored = on_disk(dir.path());
        let want = expected(&v1, None, &config, &stored);
        if v1.get("data").is_some() && v1["phases"]["env"]["state"] == "done" {
            let detail = stored["phases"]["env"]["detail"].as_str().unwrap();
            assert!(detail.contains("no longer exported"), "{name}: {detail}");
            assert_eq!(migrated.status(), Status::NeedsAttention, "{name}");
        }
        // A binding that cannot be placed is kept in full; nothing else moves.
        assert_eq!(stored, want, "{name}");
        assert_eq!(
            migrated.unmapped.is_some(),
            v1.get("data").is_some(),
            "{name}"
        );
        assert!(migrated.services.is_empty(), "{name}");
        assert!(migrated.postgres(&config).is_none(), "{name}");
    }
}

#[test]
fn migrated_receipts_keep_their_status_and_meaning() {
    let config = config(Some("db"));
    let load = |v1: Value| {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &v1);
        Receipt::load_for(dir.path(), &config).unwrap().unwrap()
    };
    let branch_source = Some(format!("branch:{BRANCH}"));
    // Interrupted: the failed phase (renamed) still needs attention.
    let receipt = load(interrupted());
    assert_eq!(receipt.status(), Status::NeedsAttention);
    let services = receipt.phase(Phase::Services);
    assert_eq!(services.state, PhaseState::Failed);
    assert!(services.detail.unwrap().contains("connection refused"));
    assert!(receipt.phase(Phase::Hooks).required_skip);
    assert_eq!(receipt.data_source_id, branch_source);
    // Deliberate skips are marked as asked for and kept; the environment the
    // old file holds is reopened (nothing was written under the generation).
    let receipt = load(deliberate_skips());
    assert_eq!(receipt.status(), Status::NeedsAttention);
    assert!(receipt.phase(Phase::Env).required_skip);
    assert!(receipt.phase(Phase::Services).deliberate);
    let hooks = receipt.phase(Phase::Hooks);
    assert!(hooks.deliberate && !hooks.required_skip);
    // A URL-only attachment keeps owned = false and no branch.
    let receipt = load(url_attachment());
    let (key, binding) = receipt.postgres(&config).unwrap();
    assert_eq!(key, "db");
    assert_eq!(binding.engine, Engine::Postgres);
    assert!(!binding.owned && binding.branch_id.is_none());
    assert_eq!(binding.url, ATTACHED);
    assert_eq!(binding.source, "url");
    // The fork has no binding and still resumes at the services phase.
    let receipt = load(forked());
    assert!(receipt.services.is_empty() && receipt.unmapped.is_none());
    assert_eq!(receipt.status(), Status::InProgress);
    assert_eq!(receipt.data_source, branch_source);
    // The owned receipt keeps its authority over its branch. It was ready;
    // its environment and the hooks after it are redone by the next setup.
    let receipt = load(owned());
    assert_eq!(receipt.status(), Status::NeedsAttention);
    assert!(receipt.phase(Phase::Env).required_skip);
    assert!(receipt.phase(Phase::Hooks).required_skip);
    assert_eq!(receipt.env_generation, None);
    let (_, binding) = receipt.postgres(&config).unwrap();
    assert!(binding.owned);
    assert_eq!(binding.branch_id, Some(BRANCH.parse().unwrap()));
    assert_eq!(binding.url, URL);
    assert_eq!(receipt.instance_id, INSTANCE.parse::<uuid::Uuid>().unwrap());
}

#[test]
fn a_receipt_from_a_newer_devflow_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut future = owned();
    future["version"] = json!(3);
    write(dir.path(), &future);
    let error = Receipt::load(dir.path()).unwrap_err().to_string();
    assert!(error.contains("newer devflow"), "{error}");
}
