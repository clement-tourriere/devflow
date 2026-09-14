mod support;
use serde_json::Value;
use std::{path::Path, process::Command};

struct Fixture {
    _root: tempfile::TempDir,
    repo: std::path::PathBuf,
}
impl Fixture {
    fn new(extra: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        support::init(&repo);
        std::fs::write(repo.join(".devflow.yml"), format!("name: review\nworktree:\n  path_template: ../nested/workspaces/{{workspace}}\n  copy_files: []\n  copy_ai_configs: false\n{extra}")).unwrap();
        support::commit(&repo);
        Self { _root: root, repo }
    }
    fn run(&self, args: &[&str]) -> (bool, Value) {
        let output = Command::new(env!("CARGO_BIN_EXE_devflow"))
            .current_dir(&self.repo)
            .env("DEVFLOW_CONFIG_DIR", self._root.path().join("config"))
            .env(
                "DEVFLOW_PROCESS_STATE_DIR",
                self._root.path().join("processes"),
            )
            .env_remove("DEVFLOW_APPROVE_HOOKS")
            .args(["--json", "--non-interactive"])
            .args(args)
            .output()
            .unwrap();
        let value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|e| panic!("one JSON document required: {e}; {output:?}"));
        (output.status.success(), value)
    }
}

#[test]
fn errors_always_emit_one_json_document() {
    let f = Fixture::new("");
    for args in [
        vec!["switch", "missing"],
        vec!["--unknown-option"],
        vec!["shell-init", "bash"],
    ] {
        let (ok, result) = f.run(&args);
        assert!(!ok, "{result}");
        assert_eq!(result["status"], "error");
        assert!(result["error"]["message"].is_string());
    }
}

#[test]
fn nested_creation_preserves_parents_and_reports_skipped_pre_hooks() {
    let f = Fixture::new("hooks:\n  pre-switch:\n    approval-needed: 'echo should-not-run'\n");
    let (ok, first) = f.run(&[
        "switch",
        "-c",
        "agent/task",
        "--no-services",
        "--no-processes",
        "--data-from",
        "main",
    ]);
    assert!(ok, "{first}");
    assert!(Path::new(first["worktree_path"].as_str().unwrap()).is_dir());
    assert_eq!(first["parent"], "main");
    assert_eq!(first["operation"]["data_parent"], "main");
    assert_eq!(first["operation"]["status"], "needs_attention");
    assert!(first["hook_results"]
        .as_array()
        .unwrap()
        .iter()
        .any(|h| h["phase"] == "pre-switch" && h["skipped"] == 1));
    let (ok, retry) = f.run(&[
        "switch",
        "-c",
        "agent/task",
        "--no-services",
        "--no-processes",
        "--no-verify",
    ]);
    assert!(ok, "{retry}");
    assert_eq!(first["operation"]["id"], retry["operation"]["id"]);
    assert_eq!(retry["operation"]["status"], "ready");
    assert_eq!(retry["parent"], "main");
}

#[test]
fn configured_processes_start_for_first_child_without_polluting_json() {
    let f = Fixture::new("processes:\n  provider: native\n  auto_start: true\n  daemons:\n    worker:\n      run: sleep 20\n      ready_cmd: printf READY_NOISE\n      ready_timeout: 2\n");
    assert!(f.run(&["hook", "approvals", "add", "sleep 20"]).0);
    let (ok, output) = f.run(&["switch", "-c", "first-child", "--no-services"]);
    // Cleanup before assertions so failures do not leave a test daemon alive.
    let stopped = f.run(&["process", "stop", "--all", "--workspace", "first-child"]);
    assert!(ok, "{output}");
    assert!(stopped.0, "{}", stopped.1);
    assert_eq!(output["processes_started"], 1);
    assert_eq!(output["operation"]["status"], "ready");
}

#[cfg(unix)]
#[test]
fn post_create_retries_after_a_confined_file_action_fails() {
    let f = Fixture::new("hooks:\n  post-create:\n    marker:\n      action:\n        type: write-file\n        path: escape/marker\n        content: ready\n");
    let outside = f._root.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("marker"), "original").unwrap();
    std::os::unix::fs::symlink(&outside, f.repo.join("escape")).unwrap();
    support::commit(&f.repo);
    let (ok, first) = f.run(&["switch", "-c", "task", "--no-services", "--no-processes"]);
    assert!(!ok, "{first}");
    assert_eq!(
        std::fs::read_to_string(outside.join("marker")).unwrap(),
        "original"
    );
    let tree = Path::new(first["worktree_path"].as_str().unwrap());
    std::fs::remove_file(tree.join("escape")).unwrap();
    let (ok, retry) = f.run(&["switch", "task", "--no-services", "--no-processes"]);
    assert!(ok, "{retry}");
    assert_eq!(first["operation"]["id"], retry["operation"]["id"]);
    assert_eq!(retry["operation"]["status"], "ready");
    assert_eq!(
        std::fs::read_to_string(tree.join("escape/marker")).unwrap(),
        "ready"
    );
}

#[test]
fn command_arguments_are_passed_literally() {
    let fixture = Fixture::new("");
    let (ok, output) = fixture.run(&[
        "switch",
        "-c",
        "argv",
        "--no-services",
        "--no-processes",
        "--no-verify",
        "-x",
        "printf",
        "--",
        "%s\n",
        "hello world",
        "$(touch unwanted)",
        "*",
        "",
        "a'b",
    ]);
    assert!(ok, "{output}");
    assert_eq!(
        output["execution"]["stdout"],
        "hello world\n$(touch unwanted)\n*\n\na'b\n"
    );
    assert!(!Path::new(output["worktree_path"].as_str().unwrap())
        .join("unwanted")
        .exists());
}

#[test]
fn human_and_json_modes_both_gate_execution_on_readiness() {
    let fixture = Fixture::new("hooks:\n  pre-switch:\n    prerequisite: 'printf UNAPPROVED'\n");
    let (ok, output) = fixture.run(&[
        "switch",
        "-c",
        "json-task",
        "--no-services",
        "--no-processes",
        "-x",
        "touch",
        "--",
        "execution-ran",
    ]);
    assert!(ok, "{output}");
    assert_eq!(output["execution"]["status"], "skipped");
    let human = Command::new(env!("CARGO_BIN_EXE_devflow"))
        .current_dir(&fixture.repo)
        .env("DEVFLOW_CONFIG_DIR", fixture._root.path().join("config"))
        .env_remove("DEVFLOW_APPROVE_HOOKS")
        .args([
            "--non-interactive",
            "switch",
            "-c",
            "human-task",
            "--no-services",
            "--no-processes",
            "-x",
            "touch",
            "--",
            "execution-ran",
        ])
        .output()
        .unwrap();
    assert!(human.status.success(), "{human:?}");
    assert!(String::from_utf8_lossy(&human.stderr).contains("Command skipped"));
    let (_, inventory) = fixture.run(&["list"]);
    for workspace in inventory["workspaces"].as_array().unwrap() {
        if let Some(path) = workspace["worktree_path"].as_str() {
            assert!(!Path::new(path).join("execution-ran").exists());
        }
    }
}

#[cfg(feature = "service-local")]
#[test]
fn unsupported_reset_fails_without_claiming_success() {
    let fixture = Fixture::new("services:\n  - name: cache\n    service_type: generic\n    generic:\n      image: redis:7-alpine\n");
    let (ok, output) = fixture.run(&["service", "reset", "never-created"]);
    assert!(!ok, "{output}");
    assert!(output["error"]["message"]
        .as_str()
        .unwrap()
        .contains("does not support reset"));
}

#[test]
fn optional_unapproved_hook_does_not_block_ready_workspace() {
    let f = Fixture::new(
        "hooks:\n  post-create:\n    optional:\n      command: 'false'\n      required: false\n",
    );
    let (ok, result) = f.run(&[
        "switch",
        "-c",
        "optional",
        "--no-services",
        "--no-processes",
        "-x",
        "printf",
        "--",
        "ready",
    ]);
    assert!(ok, "{result}");
    assert_eq!(result["operation"]["status"], "ready");
    assert_eq!(result["execution"]["stdout"], "ready");
}

#[test]
fn required_background_hook_finishes_before_execution() {
    let command = "sleep 0.1; printf ready > prepared";
    let f=Fixture::new(&format!("hooks:\n  post-create:\n    prepare:\n      command: '{command}'\n      background: true\n"));
    let (ok, value) = f.run(&["hook", "approvals", "add", command]);
    assert!(ok, "{value}");
    let (ok, value) = f.run(&[
        "switch",
        "-c",
        "background",
        "--no-services",
        "--no-processes",
        "-x",
        "cat",
        "--",
        "prepared",
    ]);
    assert!(ok, "{value}");
    assert_eq!(value["execution"]["stdout"], "ready");
    assert_eq!(value["operation"]["status"], "ready");
}

#[test]
fn operation_resume_preserves_saved_intent_and_history() {
    let f = Fixture::new("hooks:\n  post-create:\n    prepare: 'printf ready > prepared'\n");
    let (ok, first) = f.run(&["switch", "-c", "resume", "--no-services", "--no-processes"]);
    assert!(ok, "{first}");
    let id = first["operation"]["id"].as_str().unwrap();
    let (ok, inspect) = f.run(&["operation", "inspect", id]);
    assert!(ok, "{inspect}");
    assert_eq!(inspect["status"], "needs_attention");
    let (ok, approval) = f.run(&["hook", "approvals", "add", "printf ready > prepared"]);
    assert!(ok, "{approval}");
    let (ok, resumed) = f.run(&["operation", "resume", id]);
    assert!(ok, "{resumed}");
    assert_eq!(resumed["operation"]["status"], "ready");
    assert_eq!(resumed["operation"]["id"], id);
    let (ok, output) = f.run(&[
        "exec",
        "--workspace",
        "resume",
        "--",
        "printf",
        "%s",
        "hello world",
    ]);
    assert!(ok, "{output}");
    assert_eq!(output["stdout"], "hello world");
    let (ok, second) = f.run(&["switch", "resume", "--no-services", "--no-processes"]);
    assert!(ok, "{second}");
    let (ok, history) = f.run(&["operation", "list", "--history"]);
    assert!(ok, "{history}");
    assert!(history.as_array().unwrap().len() >= 2);
}

#[test]
fn pins_block_explicit_removal_until_unpinned() {
    let f = Fixture::new("");
    let (ok, value) = f.run(&["switch", "-c", "pinned", "--no-services", "--no-processes"]);
    assert!(ok, "{value}");
    let (ok, value) = f.run(&["retention", "pin", "pinned"]);
    assert!(ok, "{value}");
    let (ok, value) = f.run(&["remove", "pinned", "--force"]);
    assert!(!ok, "{value}");
    assert!(value.to_string().contains("pinned"));
    let (ok, value) = f.run(&["retention", "unpin", "pinned"]);
    assert!(ok, "{value}");
    let (ok, value) = f.run(&["remove", "pinned", "--force"]);
    assert!(ok, "{value}");
}

#[test]
fn worktree_payload_copy_needs_no_helper_programs() {
    for use_jj in [false, true] {
        let f = Fixture::new("");
        std::fs::write(f.repo.join(".devflow.yml"), "name: native-copy\nworktree:\n  path_template: ../worktrees/{workspace}\n  copy_files: [payload]\n  copy_ai_configs: false\n").unwrap();
        std::fs::write(f.repo.join(".gitignore"), "payload/\n").unwrap();
        std::fs::create_dir(f.repo.join("payload")).unwrap();
        std::fs::write(f.repo.join("payload/marker"), "original").unwrap();
        support::commit(&f.repo);
        if use_jj {
            devflow_core::vcs::JjRepository::init(&f.repo, true).unwrap();
        }
        let output = Command::new(env!("CARGO_BIN_EXE_devflow"))
            .current_dir(&f.repo)
            .env("PATH", f._root.path().join("no-tools"))
            .env("DEVFLOW_CONFIG_DIR", f._root.path().join("config"))
            .env(
                "DEVFLOW_PROCESS_STATE_DIR",
                f._root.path().join("processes"),
            )
            .args([
                "--json",
                "--non-interactive",
                "switch",
                "-c",
                "native",
                "--no-services",
                "--no-processes",
                "--no-verify",
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        let worktree = Path::new(result["worktree_path"].as_str().unwrap());
        assert_eq!(
            std::fs::read_to_string(worktree.join("payload/marker")).unwrap(),
            "original"
        );
        std::fs::write(worktree.join("payload/marker"), "changed").unwrap();
        assert_eq!(
            std::fs::read_to_string(f.repo.join("payload/marker")).unwrap(),
            "original"
        );
    }
}

#[test]
fn retired_helpers_report_migration_errors_without_project_setup() {
    let f = Fixture::new("");
    for args in [
        vec!["commit", "--ai"],
        vec!["update", "--check"],
        vec!["setup-zfs"],
    ] {
        let (ok, result) = f.run(&args);
        assert!(!ok, "{result}");
        assert!(
            result["error"]["message"]
                .as_str()
                .unwrap()
                .contains("retired"),
            "{result}"
        );
    }
}
