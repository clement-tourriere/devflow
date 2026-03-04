use anyhow::{Context, Result};
use devflow_core::config::{Config, EffectiveConfig};
use devflow_core::state::LocalStateManager;
use devflow_core::vcs;

pub(super) fn yes_no(value: Option<bool>) -> &'static str {
    if value.unwrap_or(false) {
        "yes"
    } else {
        "no"
    }
}

/// Detect the current shell from the `$SHELL` environment variable.
pub(super) fn detect_shell_from_env() -> Result<String> {
    let shell_path = std::env::var("SHELL")
        .context("Cannot auto-detect shell: $SHELL is not set. Please specify a shell: devflow shell-init <bash|zsh|fish>")?;
    let shell_name = std::path::Path::new(&shell_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or(shell_path.clone());
    match shell_name.as_str() {
        "bash" | "zsh" | "fish" => Ok(shell_name),
        other => anyhow::bail!(
            "Unsupported shell '{}' (from $SHELL={}). Supported shells: bash, zsh, fish",
            other,
            shell_path
        ),
    }
}

/// Whether the command is being executed through `devflow shell-init` wrapper.
pub(super) fn shell_integration_enabled() -> bool {
    std::env::var("DEVFLOW_SHELL_INTEGRATION")
        .map(|v| v == "1")
        .unwrap_or(false)
}

pub(super) fn print_manual_cd_hint(target: &std::path::Path) {
    println!(
        "Shell integration not detected. Run: cd \"{}\"",
        target.display()
    );
    println!("Note: devflow cannot change your parent shell directory without shell integration.");
    println!("Tip: add `eval \"$(devflow shell-init)\"` to your shell profile for auto-cd.");
}

pub(super) fn resolve_cd_target(path: &std::path::Path) -> Result<std::path::PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()?.join(path))
}

/// Print shell integration script for the given shell type.
///
/// Users should add `eval "$(devflow shell-init bash)"` (or zsh/fish) to their
/// shell profile. This defines a `devflow` wrapper function that:
/// 1. Runs `devflow` normally, preserving stderr
/// 2. Parses `DEVFLOW_CD=<path>` output from commands that request directory changes
/// 3. Automatically `cd`s into the target worktree directory
pub(super) fn print_shell_init(shell: &str) -> Result<()> {
    let script = match shell {
        "bash" => {
            r#"
# devflow shell integration (bash)
# Wrapper function that auto-cds when devflow emits DEVFLOW_CD
devflow() {
    local output
    output="$(DEVFLOW_SHELL_INTEGRATION=1 command devflow "$@")"
    local exit_code=$?

    # Print all output lines, skipping DEVFLOW_CD directives
    while IFS= read -r line; do
        case "$line" in
            DEVFLOW_CD=*)
                local target="${line#DEVFLOW_CD=}"
                if [ -d "$target" ]; then
                    cd "$target" || return 1
                    echo "Changed directory to: $target"
                fi
                ;;
            *)
                echo "$line"
                ;;
        esac
    done <<< "$output"

    return $exit_code
}
"#
        }
        "zsh" => {
            r#"
# devflow shell integration (zsh)
# Wrapper function that auto-cds when devflow emits DEVFLOW_CD
devflow() {
    local output
    output="$(DEVFLOW_SHELL_INTEGRATION=1 command devflow "$@")"
    local exit_code=$?

    # Print all output lines, skipping DEVFLOW_CD directives
    while IFS= read -r line; do
        case "$line" in
            DEVFLOW_CD=*)
                local target="${line#DEVFLOW_CD=}"
                if [ -d "$target" ]; then
                    cd "$target" || return 1
                    echo "Changed directory to: $target"
                fi
                ;;
            *)
                echo "$line"
                ;;
        esac
    done <<< "$output"

    return $exit_code
}
"#
        }
        "fish" => {
            r#"
# devflow shell integration (fish)
# Wrapper function that auto-cds when devflow emits DEVFLOW_CD
function devflow --wraps devflow --description "devflow with auto-cd"
    set -l output (env DEVFLOW_SHELL_INTEGRATION=1 command devflow $argv)
    set -l exit_code $status

    for line in $output
        if string match -q 'DEVFLOW_CD=*' -- $line
            set -l target (string replace 'DEVFLOW_CD=' '' -- $line)
            if test -d "$target"
                cd "$target"
                echo "Changed directory to: $target"
            end
        else
            echo $line
        end
    end

    return $exit_code
end
"#
        }
        _ => {
            anyhow::bail!(
                "Unsupported shell '{}'. Supported shells: bash, zsh, fish",
                shell
            );
        }
    };

    print!("{}", script.trim_start());
    Ok(())
}

pub(super) fn run_doctor_pre_checks(config: &Config, config_path: &Option<std::path::PathBuf>) {
    println!("General:");

    // Config file
    match config_path {
        Some(path) => println!("  [OK] Config file: {}", path.display()),
        None => {
            println!("  [WARN] Config file: not found (run 'devflow init' to create .devflow.yml)")
        }
    }

    // VCS repository
    let vcs_repo = vcs::detect_vcs_provider(".");
    match &vcs_repo {
        Ok(vcs) => println!("  [OK] {} repository: detected", vcs.provider_name()),
        Err(_) => println!("  [FAIL] VCS repository: not found"),
    }

    // VCS hooks
    let hooks_dir = std::path::Path::new(".git/hooks");
    let has_hooks = if hooks_dir.exists() {
        let post_checkout = hooks_dir.join("post-checkout");
        let post_merge = hooks_dir.join("post-merge");
        if let Ok(ref vcs) = vcs_repo {
            (post_checkout.exists() && vcs.is_devflow_hook(&post_checkout).unwrap_or(false))
                || (post_merge.exists() && vcs.is_devflow_hook(&post_merge).unwrap_or(false))
        } else {
            post_checkout.exists() || post_merge.exists()
        }
    } else {
        false
    };
    if has_hooks {
        println!("  [OK] VCS hooks: installed");
    } else {
        println!("  [WARN] VCS hooks: not installed (run 'devflow install-hooks')");
    }

    // Stale worktree metadata (present in VCS metadata but missing on disk)
    if let Ok(ref vcs) = vcs_repo {
        if vcs.supports_worktrees() {
            match vcs.list_worktrees() {
                Ok(worktrees) => {
                    let stale: Vec<_> = worktrees
                        .iter()
                        .filter(|wt| !wt.is_main && !wt.path.exists())
                        .collect();

                    if stale.is_empty() {
                        println!("  [OK] Worktree metadata: clean");
                    } else {
                        let suffix = if stale.len() == 1 { "y" } else { "ies" };
                        println!(
                            "  [WARN] Worktree metadata: {} stale entr{} (run 'git worktree prune')",
                            stale.len(),
                            suffix
                        );
                        for wt in stale.iter().take(5) {
                            let workspace = wt.workspace.as_deref().unwrap_or("<unknown>");
                            println!("         - {} -> {}", workspace, wt.path.display());
                        }
                    }
                }
                Err(e) => {
                    println!("  [WARN] Worktree metadata: inspection failed ({})", e);
                }
            }
        }
    }

    // Registry entries with missing worktree paths
    if let Some(path) = config_path {
        match LocalStateManager::new() {
            Ok(state) => {
                let missing: Vec<_> = state
                    .get_workspaces(path)
                    .into_iter()
                    .filter_map(|b| b.worktree_path.map(|p| (b.name, p)))
                    .filter(|(_, p)| !std::path::Path::new(p).exists())
                    .collect();

                if missing.is_empty() {
                    println!("  [OK] Workspace registry paths: clean");
                } else {
                    let suffix = if missing.len() == 1 { "y" } else { "ies" };
                    println!(
                        "  [WARN] Workspace registry paths: {} stale entr{}",
                        missing.len(),
                        suffix
                    );
                    for (workspace, wt_path) in missing.iter().take(5) {
                        println!("         - {} -> {}", workspace, wt_path);
                    }
                }
            }
            Err(e) => {
                println!(
                    "  [WARN] Workspace registry paths: inspection failed ({})",
                    e
                );
            }
        }
    }

    // Workspace filter regex
    if let Some(ref regex_pattern) = config.git.workspace_filter_regex {
        match regex::Regex::new(regex_pattern) {
            Ok(_) => println!("  [OK] Workspace filter regex: valid"),
            Err(e) => println!("  [FAIL] Workspace filter regex: {}", e),
        }
    }

    println!();
}

pub(super) fn show_effective_config(effective_config: &EffectiveConfig) -> Result<()> {
    println!("🔧 Effective Configuration");
    println!("==========================\n");

    // Show configuration status
    println!("📊 Status:");
    if effective_config.is_disabled() {
        println!("  ❌ devflow is DISABLED globally");
    } else {
        println!("  ✅ devflow is enabled");
    }

    if effective_config.should_skip_hooks() {
        println!("  ❌ Git hooks are DISABLED");
    } else {
        println!("  ✅ Git hooks are enabled");
    }

    if effective_config.is_current_workspace_disabled() {
        println!("  ❌ Current workspace operations are DISABLED");
    } else {
        println!("  ✅ Current workspace operations are enabled");
    }

    // Check if current git workspace is disabled
    match effective_config.check_current_git_workspace_disabled() {
        Ok(true) => println!("  ❌ Current Git workspace is DISABLED"),
        Ok(false) => {
            if let Ok(vcs_repo) = vcs::detect_vcs_provider(".") {
                if let Ok(Some(workspace)) = vcs_repo.current_workspace() {
                    println!(
                        "  ✅ Current {} workspace '{}' is enabled",
                        vcs_repo.provider_name(),
                        workspace
                    );
                } else {
                    println!("  ⚠️  Could not determine current workspace");
                }
            } else {
                println!("  ⚠️  Not in a VCS repository");
            }
        }
        Err(e) => println!("  ⚠️  Error checking current workspace: {}", e),
    }

    println!();

    // Show environment variable overrides
    println!("🌍 Environment Variable Overrides:");
    let has_env_overrides = effective_config.env_config.disabled.is_some()
        || effective_config.env_config.skip_hooks.is_some()
        || effective_config.env_config.auto_create.is_some()
        || effective_config.env_config.auto_switch.is_some()
        || effective_config.env_config.workspace_filter_regex.is_some()
        || effective_config.env_config.disabled_workspaces.is_some()
        || effective_config
            .env_config
            .current_workspace_disabled
            .is_some()
        || effective_config.env_config.database_host.is_some()
        || effective_config.env_config.database_port.is_some()
        || effective_config.env_config.database_user.is_some()
        || effective_config.env_config.database_password.is_some()
        || effective_config.env_config.database_prefix.is_some();

    if !has_env_overrides {
        println!("  (none)");
    } else {
        if let Some(disabled) = effective_config.env_config.disabled {
            println!("  DEVFLOW_DISABLED: {}", disabled);
        }
        if let Some(skip_hooks) = effective_config.env_config.skip_hooks {
            println!("  DEVFLOW_SKIP_HOOKS: {}", skip_hooks);
        }
        if let Some(auto_create) = effective_config.env_config.auto_create {
            println!("  DEVFLOW_AUTO_CREATE: {}", auto_create);
        }
        if let Some(auto_switch) = effective_config.env_config.auto_switch {
            println!("  DEVFLOW_AUTO_SWITCH: {}", auto_switch);
        }
        if let Some(ref regex) = effective_config.env_config.workspace_filter_regex {
            println!("  DEVFLOW_BRANCH_FILTER_REGEX: {}", regex);
        }
        if let Some(ref workspaces) = effective_config.env_config.disabled_workspaces {
            println!("  DEVFLOW_DISABLED_BRANCHES: {}", workspaces.join(","));
        }
        if let Some(current_disabled) = effective_config.env_config.current_workspace_disabled {
            println!("  DEVFLOW_CURRENT_BRANCH_DISABLED: {}", current_disabled);
        }
        if let Some(ref host) = effective_config.env_config.database_host {
            println!("  DEVFLOW_DATABASE_HOST: {}", host);
        }
        if let Some(port) = effective_config.env_config.database_port {
            println!("  DEVFLOW_DATABASE_PORT: {}", port);
        }
        if let Some(ref user) = effective_config.env_config.database_user {
            println!("  DEVFLOW_DATABASE_USER: {}", user);
        }
        if effective_config.env_config.database_password.is_some() {
            println!("  DEVFLOW_DATABASE_PASSWORD: [hidden]");
        }
        if let Some(ref prefix) = effective_config.env_config.database_prefix {
            println!("  DEVFLOW_DATABASE_PREFIX: {}", prefix);
        }
    }

    println!();

    // Show local config overrides
    println!("📁 Local Config File Overrides:");
    if let Some(ref local_config) = effective_config.local_config {
        println!("  ✅ Local config file found (.devflow.local.yml)");
        if local_config.disabled.is_some()
            || local_config.disabled_workspaces.is_some()
            || local_config.database.is_some()
            || local_config.git.is_some()
            || local_config.behavior.is_some()
        {
            println!("  Local overrides present (see merged config below)");
        } else {
            println!("  No overrides in local config");
        }
    } else {
        println!("  (no local config file found)");
    }

    println!();

    // Show service source
    println!("Services:");
    if let Ok(state) = LocalStateManager::new() {
        // Try to find config path to look up state services
        let config_path = Config::find_config_file().ok().flatten();
        let state_services = config_path.as_ref().and_then(|p| state.get_services(p));

        if let Some(ref services) = state_services {
            println!("  Source: local state (~/.config/devflow/local_state.yml)");
            for b in services {
                let default_marker = if b.default { " (default)" } else { "" };
                println!("  - {} [{}]{}", b.name, b.provider_type, default_marker);
            }
        } else {
            let committed_services = effective_config.config.resolve_services();
            if committed_services.is_empty() {
                println!("  (none configured)");
            } else {
                println!("  Source: committed config (.devflow.yml)");
                for b in &committed_services {
                    let default_marker = if b.default { " (default)" } else { "" };
                    println!("  - {} [{}]{}", b.name, b.provider_type, default_marker);
                }
            }
        }
    }

    println!();

    // Show final merged configuration
    println!("Final Merged Configuration:");
    let merged_config = effective_config.get_merged_config();
    println!("{}", serde_yaml_ng::to_string(&merged_config)?);

    Ok(())
}
