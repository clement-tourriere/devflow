use anyhow::{Context, Result};
use devflow_core::config::{Config, EffectiveConfig};
use devflow_core::state::LocalStateManager;
use devflow_core::vcs;
use std::path::PathBuf;

/// Known top-level keys in .devflow.yml
const KNOWN_TOP_LEVEL_KEYS: &[&str] = &[
    "name",
    "default_vcs",
    "git",
    "behavior",
    "services",
    "processes",
    "worktree",
    "hooks",
    "execute",
    "commit",
    "agent",
];

/// Validate the configuration file for errors and unknown fields.
pub(super) fn validate_config(config_path: &Option<PathBuf>, json_output: bool) -> Result<()> {
    let path = config_path
        .as_ref()
        .context("No configuration file found. Run 'devflow init' to create one.")?;

    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {}", path.display()))?;

    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // 1. Parse as YAML and check for syntax errors
    let raw: serde_yaml_ng::Value = match serde_yaml_ng::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            errors.push(format!("YAML syntax error: {}", e));
            return report_validation(&errors, &warnings, json_output);
        }
    };

    // 2. Check for unknown top-level keys
    if let serde_yaml_ng::Value::Mapping(map) = &raw {
        for key in map.keys() {
            if let serde_yaml_ng::Value::String(k) = key {
                if !KNOWN_TOP_LEVEL_KEYS.contains(&k.as_str()) {
                    warnings.push(format!("Unknown top-level key '{}'", k));
                }
            }
        }
    }

    // 3. Try loading via the normal Config loader
    match Config::from_file(path) {
        Ok(_config) => {
            // Config loaded successfully — additional checks could go here
        }
        Err(e) => {
            errors.push(format!("Config loading failed: {:#}", e));
        }
    }

    // 4. Check service entries for type/service_type consistency
    if let serde_yaml_ng::Value::Mapping(map) = &raw {
        if let Some(serde_yaml_ng::Value::Mapping(processes)) = map.get("processes") {
            if let Some(provider) = processes.get("provider").and_then(|v| v.as_str()) {
                if !["native", "pitchfork"].contains(&provider) {
                    errors.push(format!(
                        "processes.provider '{}' is not supported (use 'native' or 'pitchfork')",
                        provider
                    ));
                }
            }
            if let Some(serde_yaml_ng::Value::Mapping(pitchfork)) = processes.get("pitchfork") {
                if let Some(policy) = pitchfork.get("config_policy").and_then(|v| v.as_str()) {
                    let known = ["devflow-owned", "import", "mirror", "merge"];
                    if !known.contains(&policy) {
                        errors.push(format!(
                            "processes.pitchfork.config_policy '{}' is not supported (use devflow-owned, import, mirror, or merge)",
                            policy
                        ));
                    }
                }
                if let Some(external) = pitchfork.get("external_daemons").and_then(|v| v.as_str()) {
                    let known = ["hide", "show", "importable"];
                    if !known.contains(&external) {
                        errors.push(format!(
                            "processes.pitchfork.external_daemons '{}' is not supported (use hide, show, or importable)",
                            external
                        ));
                    }
                }
                if let Some(serde_yaml_ng::Value::Mapping(web_ui)) = pitchfork.get("web_ui") {
                    if let Some(edit_mode) = web_ui.get("edit_mode").and_then(|v| v.as_str()) {
                        let known = ["readonly", "warn", "merge"];
                        if !known.contains(&edit_mode) {
                            errors.push(format!(
                                "processes.pitchfork.web_ui.edit_mode '{}' is not supported (use readonly, warn, or merge)",
                                edit_mode
                            ));
                        }
                    }
                }
            }
            if let Some(serde_yaml_ng::Value::Mapping(daemons)) = processes.get("daemons") {
                for (name_value, daemon_value) in daemons {
                    let name = name_value.as_str().unwrap_or("<unknown>");
                    if let serde_yaml_ng::Value::Mapping(daemon) = daemon_value {
                        if !daemon.contains_key("run") {
                            errors.push(format!(
                                "process '{}' is missing required field 'run'",
                                name
                            ));
                        }
                    }
                }
            }
        }

        if let Some(serde_yaml_ng::Value::Sequence(services)) = map.get("services") {
            for (idx, svc) in services.iter().enumerate() {
                if let serde_yaml_ng::Value::Mapping(svc_map) = svc {
                    let fallback_name = format!("#{}", idx);
                    let name = svc_map
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&fallback_name);
                    let svc_type = svc_map
                        .get("service_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("postgres");

                    // Check for known service types
                    {
                        let known_types = [
                            "postgres",
                            "clickhouse",
                            "mysql",
                            "mariadb",
                            "generic",
                            "plugin",
                            "redis",
                            "rustfs",
                            "s3",
                            "objectstorage",
                        ];
                        if !known_types.contains(&svc_type) {
                            warnings.push(format!(
                                "Service '{}': unknown service_type '{}'",
                                name, svc_type
                            ));
                        }
                    }

                    // Check for type/provider consistency
                    let provider_type = svc_map.get("type").and_then(|v| v.as_str());
                    if let Some(pt) = provider_type {
                        let known_providers = ["local", "neon", "dblab", "xata", "shared"];
                        if !known_providers.contains(&pt) {
                            warnings.push(format!(
                                "Service '{}': unknown provider type '{}'",
                                name, pt
                            ));
                        }
                    }
                }
            }
        }
    }

    report_validation(&errors, &warnings, json_output)
}

fn report_validation(errors: &[String], warnings: &[String], json_output: bool) -> Result<()> {
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "valid": errors.is_empty(),
                "errors": errors,
                "warnings": warnings,
            }))?
        );
    } else {
        if errors.is_empty() && warnings.is_empty() {
            println!("✓ Configuration is valid.");
        }
        if !warnings.is_empty() {
            println!("Warnings:");
            for w in warnings {
                println!("  ⚠ {}", w);
            }
        }
        if !errors.is_empty() {
            println!("Errors:");
            for e in errors {
                println!("  ✗ {}", e);
            }
        }
    }
    if !errors.is_empty() {
        anyhow::bail!(
            "Configuration validation failed with {} error(s)",
            errors.len()
        );
    }
    Ok(())
}

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

/// Run doctor pre-checks (VCS, config, hooks). Returns `true` if every check
/// passed (warnings don't count as failures), `false` if any `[FAIL]` occurred.
pub(super) fn run_doctor_pre_checks(
    config: &Config,
    config_path: &Option<std::path::PathBuf>,
) -> bool {
    let mut healthy = true;
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
        Err(_) => {
            println!("  [FAIL] VCS repository: not found");
            healthy = false;
        }
    }

    // VCS hooks
    let hooks_dir = std::path::Path::new(".git/hooks");
    let has_hooks = if hooks_dir.exists() {
        let post_checkout = hooks_dir.join("post-checkout");
        if let Ok(ref vcs) = vcs_repo {
            post_checkout.exists() && vcs.is_devflow_hook(&post_checkout).unwrap_or(false)
        } else {
            post_checkout.exists()
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

    if let Some(path) = config_path {
        match LocalStateManager::new() {
            Ok(state) => {
                let config_services = config.resolve_services();
                let local_services = state.get_services(path).unwrap_or_default();
                if local_services.is_empty() {
                    println!(
                        "  [OK] Service config sources: {} service(s) from config, none from local state",
                        config_services.len()
                    );
                } else {
                    let names = local_services
                        .iter()
                        .map(|service| service.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    println!(
                        "  [OK] Service config sources: {} service(s) from config, {} from local state ({})",
                        config_services
                            .iter()
                            .filter(|service| !local_services.iter().any(|local| local.name == service.name))
                            .count(),
                        local_services.len(),
                        names
                    );
                }
            }
            Err(e) => println!("  [WARN] Service config sources: inspection failed ({})", e),
        }
    }

    // Workspace filter regex
    if let Some(ref regex_pattern) = config.git.workspace_filter_regex {
        match regex::Regex::new(regex_pattern) {
            Ok(_) => println!("  [OK] Workspace filter regex: valid"),
            Err(e) => {
                println!("  [FAIL] Workspace filter regex: {}", e);
                healthy = false;
            }
        }
    }

    println!();
    healthy
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
        || effective_config.env_config.workspace_filter_regex.is_some()
        || effective_config.env_config.disabled_workspaces.is_some()
        || effective_config
            .env_config
            .current_workspace_disabled
            .is_some();

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
        if let Some(ref regex) = effective_config.env_config.workspace_filter_regex {
            println!("  DEVFLOW_BRANCH_FILTER_REGEX: {}", regex);
        }
        if let Some(ref workspaces) = effective_config.env_config.disabled_workspaces {
            println!("  DEVFLOW_DISABLED_BRANCHES: {}", workspaces.join(","));
        }
        if let Some(current_disabled) = effective_config.env_config.current_workspace_disabled {
            println!("  DEVFLOW_CURRENT_BRANCH_DISABLED: {}", current_disabled);
        }
    }

    println!();

    // Show local config overrides
    println!("📁 Local Config File Overrides:");
    if let Some(ref local_config) = effective_config.local_config {
        println!("  ✅ Local config file found (.devflow.local.yml)");
        if local_config.disabled.is_some()
            || local_config.disabled_workspaces.is_some()
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
