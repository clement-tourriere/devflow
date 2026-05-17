//! CLI smoke tests — verify that every subcommand at least parses and
//! runs without panicking.
//!
//! These tests invoke the compiled `devflow` binary with `--help` to
//! verify argument parsing, and run safe read-only commands in a temp
//! repo to verify basic functionality.

use std::path::PathBuf;
use std::process::Command;

/// Resolve the path to the devflow binary built by cargo.
fn devflow_bin() -> PathBuf {
    // cargo test sets this for integration tests
    let mut path = PathBuf::from(env!("CARGO_BIN_EXE_devflow"));
    if !path.exists() {
        // Fallback: try the target/debug directory
        path = PathBuf::from("target/debug/devflow");
    }
    path
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(devflow_bin())
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("Failed to execute devflow {:?}: {}", args, e))
}

/// Run a command inside a temporary git repo.
fn run_in_repo(args: &[&str]) -> std::process::Output {
    let tmp = tempfile::tempdir().unwrap();
    // Initialize a git repo
    Command::new("git")
        .args(["init"])
        .current_dir(tmp.path())
        .output()
        .expect("git init");
    Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(tmp.path())
        .output()
        .expect("git config email");
    Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(tmp.path())
        .output()
        .expect("git config name");
    Command::new("git")
        .args(["commit", "--allow-empty", "-m", "init"])
        .current_dir(tmp.path())
        .output()
        .expect("git commit");

    Command::new(devflow_bin())
        .args(args)
        .current_dir(tmp.path())
        .output()
        .unwrap_or_else(|e| panic!("Failed to execute devflow {:?}: {}", args, e))
}

// ── Help text tests (verify argument parsing) ──────────────────────────────

#[test]
fn version_flag() {
    let out = run(&["--version"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("devflow"));
}

#[test]
fn help_flag() {
    let out = run(&["--help"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("switch"));
    assert!(stdout.contains("list"));
}

#[test]
fn help_all_flag() {
    let out = run(&["--help-all"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("service"));
    assert!(stdout.contains("hook"));
    assert!(stdout.contains("proxy"));
    assert!(stdout.contains("agent"));
}

#[test]
fn switch_help() {
    let out = run(&["switch", "--help"]);
    assert!(out.status.success());
}

#[test]
fn service_help() {
    let out = run(&["service", "--help"]);
    assert!(out.status.success());
}

#[test]
fn json_service_doctor_runs_without_services() {
    let out = run_in_repo(&["--json", "service", "doctor", "--strict"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid service doctor JSON");
    assert_eq!(json["healthy"].as_bool(), Some(true));
    assert_eq!(json["strict"].as_bool(), Some(true));
    assert_eq!(json["failed_checks"].as_u64(), Some(0));
    assert_eq!(json["summary"]["failed"].as_u64(), Some(0));
    assert!(json["next_steps"]
        .as_array()
        .is_some_and(|steps| steps.is_empty()));
}

#[test]
fn service_add_rejects_invalid_local_postgres_storage() {
    let out = run_in_repo(&[
        "--non-interactive",
        "service",
        "add",
        "app-db",
        "--provider",
        "local",
        "--service-type",
        "postgres",
        "--storage",
        "mystery",
    ]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("invalid --storage value"));
}

#[test]
fn hook_help() {
    let out = run(&["hook", "--help"]);
    assert!(out.status.success());
}

#[test]
fn hook_explain_post_switch_describes_synchronous_execution() {
    let out = run(&["hook", "explain", "post-switch"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Execution: Synchronous by default"));
    assert!(stdout.contains("Failures:  Continue by default"));
    assert!(!stdout.contains("runs in background"));
}

#[test]
fn proxy_help() {
    let out = run(&["proxy", "--help"]);
    assert!(out.status.success());
}

#[test]
fn agent_help() {
    let out = run(&["agent", "--help"]);
    assert!(out.status.success());
}

#[test]
fn completions_bash() {
    let out = run(&["completions", "bash"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("devflow"));
}

#[test]
fn completions_zsh() {
    let out = run(&["completions", "zsh"]);
    assert!(out.status.success());
}

#[test]
fn completions_fish() {
    let out = run(&["completions", "fish"]);
    assert!(out.status.success());
}

// ── Read-only commands in a real repo ──────────────────────────────────────

#[test]
fn doctor_runs() {
    let out = run_in_repo(&["doctor"]);
    assert!(out.status.success());
}

#[test]
fn json_doctor_runs() {
    let out = run_in_repo(&["--json", "doctor"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid doctor JSON");
    let checks = json["general"]["checks"]
        .as_array()
        .expect("general checks array");
    assert!(checks.iter().any(|check| check["name"] == "Config file"));
    assert!(checks.iter().any(|check| check["name"] == "Agent guides"));
    assert!(json["general"]["summary"]["total"].as_u64().unwrap_or(0) >= 2);
    assert!(json["healthy"].is_boolean());
    assert!(json["failed_checks"].is_number());
    assert!(json["summary"]["total"].as_u64().unwrap_or(0) >= checks.len() as u64);
    assert!(json["next_steps"].is_array());
    assert!(json["general"]["next_steps"].is_array());
}

#[test]
fn json_doctor_strict_exits_nonzero_on_failed_checks() {
    let out = run_in_repo(&["--json", "doctor", "--strict"]);
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid doctor JSON");
    assert_eq!(json["healthy"].as_bool(), Some(false));
    assert_eq!(json["strict"].as_bool(), Some(true));
    assert!(json["failed_checks"].as_u64().unwrap_or(0) > 0);
    let next_steps = json["next_steps"].as_array().expect("next_steps array");
    assert!(next_steps
        .iter()
        .any(|step| step["command"] == "devflow init"));
}

#[test]
fn list_in_bare_repo() {
    let out = run_in_repo(&["list"]);
    // May succeed or fail gracefully (no .devflow.yml), but should not panic
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"));
}

#[test]
fn json_list_in_bare_repo() {
    let out = run_in_repo(&["--json", "list"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"));
}

#[test]
fn status_in_bare_repo() {
    let out = run_in_repo(&["status"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"));
}

#[test]
fn config_in_bare_repo() {
    let out = run_in_repo(&["config"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"));
}

#[test]
fn hook_show_in_bare_repo() {
    let out = run_in_repo(&["hook", "show"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"));
}

#[test]
fn hook_explain_in_bare_repo() {
    let out = run_in_repo(&["hook", "explain"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"));
}

#[test]
fn hook_actions_in_bare_repo() {
    let out = run_in_repo(&["hook", "actions"]);
    assert!(out.status.success());
}

#[test]
fn hook_recipes_in_bare_repo() {
    let out = run_in_repo(&["hook", "recipes"]);
    assert!(out.status.success());
}

#[test]
fn capabilities_runs() {
    let out = run_in_repo(&["capabilities"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"));
}

#[test]
fn json_capabilities_runs() {
    let out = run_in_repo(&["--json", "capabilities"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"));
}

#[test]
fn shell_init_bash() {
    let out = run(&["shell-init", "bash"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("devflow"));
}

#[test]
fn shell_init_zsh() {
    let out = run(&["shell-init", "zsh"]);
    assert!(out.status.success());
}

#[test]
fn gc_list() {
    let out = run_in_repo(&["gc", "--list"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"));
}

// ── Init + basic lifecycle in temp repo ────────────────────────────────────

#[test]
fn init_creates_config() {
    let tmp = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init"])
        .current_dir(tmp.path())
        .output()
        .expect("git init");
    Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(tmp.path())
        .output()
        .ok();
    Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(tmp.path())
        .output()
        .ok();
    Command::new("git")
        .args(["commit", "--allow-empty", "-m", "init"])
        .current_dir(tmp.path())
        .output()
        .ok();

    let out = Command::new(devflow_bin())
        .args(["--non-interactive", "init"])
        .current_dir(tmp.path())
        .output()
        .expect("devflow init");

    assert!(out.status.success(), "init failed: {:?}", out);
    assert!(tmp.path().join(".devflow.yml").exists());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Next steps:"));
    assert!(stdout.contains("devflow --json doctor --strict"));
    assert!(stdout.contains("devflow --json --non-interactive switch -c agent/first-task"));
}

#[test]
fn json_init_reports_onboarding_next_steps() {
    let tmp = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init"])
        .current_dir(tmp.path())
        .output()
        .expect("git init");

    let out = Command::new(devflow_bin())
        .args([
            "--json",
            "--non-interactive",
            "init",
            "--name",
            "smoke-init",
        ])
        .current_dir(tmp.path())
        .output()
        .expect("devflow init");

    assert!(out.status.success(), "init failed: {:?}", out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid init JSON");
    assert_eq!(json["status"], "ok");
    assert_eq!(json["action"], "init");
    let next_steps = json["next_steps"].as_array().expect("next_steps array");
    assert!(next_steps
        .iter()
        .any(|step| step["command"] == "devflow --json doctor --strict"));
    assert!(next_steps.iter().any(
        |step| step["command"] == "devflow --json --non-interactive switch -c agent/first-task"
    ));
    assert!(next_steps.iter().any(|step| step["agent_safe"] == true));
}

#[test]
fn json_switch_create_reports_agent_workdir() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();

    let init = Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&repo)
        .output()
        .expect("git init");
    if !init.status.success() {
        Command::new("git")
            .args(["init"])
            .current_dir(&repo)
            .output()
            .expect("git init fallback");
        Command::new("git")
            .args(["checkout", "-b", "main"])
            .current_dir(&repo)
            .output()
            .expect("git checkout main");
    }
    Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(&repo)
        .output()
        .expect("git config email");
    Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(&repo)
        .output()
        .expect("git config name");
    Command::new("git")
        .args(["commit", "--allow-empty", "-m", "init"])
        .current_dir(&repo)
        .output()
        .expect("git commit");

    std::fs::write(
        repo.join(".devflow.yml"),
        r#"name: smoke-agent
worktree:
  enabled: true
  path_template: "../{repo}-{workspace}"
"#,
    )
    .unwrap();

    let xdg = tmp.path().join("xdg");
    let link = Command::new(devflow_bin())
        .args(["link", "main"])
        .env("XDG_DATA_HOME", &xdg)
        .current_dir(&repo)
        .output()
        .expect("devflow link");
    assert!(link.status.success(), "link failed: {:?}", link);

    let out = Command::new(devflow_bin())
        .args([
            "--json",
            "--non-interactive",
            "switch",
            "-c",
            "agent/smoke",
            "--no-services",
            "--no-verify",
        ])
        .env("XDG_DATA_HOME", &xdg)
        .current_dir(&repo)
        .output()
        .expect("devflow switch");
    assert!(out.status.success(), "switch failed: {:?}", out);

    let stdout = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let worktree_path = json["worktree_path"].as_str().expect("worktree_path");
    assert_eq!(json["agent_workdir"].as_str(), Some(worktree_path));
    let cd_hint = format!("cd {worktree_path}");
    assert_eq!(json["cd_hint"].as_str(), Some(cd_hint.as_str()));
    assert!(std::path::Path::new(worktree_path).is_dir());

    let context = Command::new(devflow_bin())
        .args(["--json", "agent", "context", "--workspace", "agent/smoke"])
        .env("XDG_DATA_HOME", &xdg)
        .current_dir(&repo)
        .output()
        .expect("devflow agent context");
    assert!(context.status.success(), "context failed: {:?}", context);
    let context_stdout = String::from_utf8_lossy(&context.stdout);
    let context_json: serde_json::Value = serde_json::from_str(&context_stdout).unwrap();
    assert_eq!(context_json["agent_workdir"].as_str(), Some(worktree_path));
    assert_eq!(context_json["cd_hint"].as_str(), Some(cd_hint.as_str()));

    let status = Command::new(devflow_bin())
        .args(["--json", "agent", "status"])
        .env("XDG_DATA_HOME", &xdg)
        .current_dir(&repo)
        .output()
        .expect("devflow agent status");
    assert!(status.status.success(), "status failed: {:?}", status);
    let status_stdout = String::from_utf8_lossy(&status.stdout);
    let status_json: serde_json::Value = serde_json::from_str(&status_stdout).unwrap();
    assert!(status_json["total_workspaces"].as_u64().unwrap_or(0) >= 1);
    assert!(status_json["agent_workspaces"].is_number());
    assert!(status_json["executed_workspaces"].is_number());
    let workspaces = status_json["workspaces"].as_array().unwrap();
    let first_entry = workspaces
        .first()
        .expect("at least one workspace status entry");
    assert!(first_entry["agent_workdir"].as_str().is_some());
}
