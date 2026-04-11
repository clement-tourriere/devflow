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
fn hook_help() {
    let out = run(&["hook", "--help"]);
    assert!(out.status.success());
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
    // Should be valid JSON
    assert!(serde_json::from_str::<serde_json::Value>(&stdout).is_ok());
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
}
