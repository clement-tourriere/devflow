use anyhow::Result;
use devflow_core::config::Config;
use devflow_core::hooks::approval::ApprovalStore;
use devflow_core::processes::{
    PitchforkProcessConfig, ProcessDaemonConfig, ProcessPortBump, ProcessPortConfig,
    ProcessesConfig,
};
use devflow_core::services::{self};
use devflow_core::state::LocalStateManager;
use devflow_core::vcs;
use indexmap::IndexMap;
use serde_yaml_ng::Value;
use std::collections::{HashMap, HashSet};

/// Check if ZFS auto-setup should be offered during init (Linux only).
/// Returns `Some(data_root)` if a pool was created or already exists,
/// so the caller can set it on the `LocalServiceConfig`.
#[cfg(feature = "service-local")]
pub(super) async fn attempt_zfs_auto_setup(
    non_interactive: bool,
    quiet_output: bool,
) -> Option<String> {
    use devflow_core::services::postgres::local::storage::zfs_setup::*;

    // Use a placeholder path — the actual projects_root hasn't been established yet
    let placeholder = std::path::PathBuf::from("/var/lib/devflow/data/projects");
    let status = check_zfs_setup_status(&placeholder).await;

    match status {
        ZfsSetupStatus::NotSupported => None,
        ZfsSetupStatus::ToolsNotInstalled => {
            if !quiet_output {
                println!();
                println!("Tip: Install ZFS for near-instant Copy-on-Write service branching:");
                println!("  sudo apt install zfsutils-linux");
            }
            None
        }
        ZfsSetupStatus::AlreadyAvailable { root_dataset } => {
            if !quiet_output {
                println!();
                println!(
                    "ZFS dataset '{}' detected - will use ZFS for Copy-on-Write storage.",
                    root_dataset
                );
            }
            None
        }
        ZfsSetupStatus::DevflowPoolExists { mountpoint } => {
            if !quiet_output {
                println!();
                println!(
                    "ZFS pool 'devflow' already exists (mountpoint: {}).",
                    mountpoint
                );
            }
            Some(mountpoint)
        }
        ZfsSetupStatus::ToolsAvailableNoPool => {
            if non_interactive {
                if !quiet_output {
                    println!();
                    println!(
                        "ZFS tools detected but no pool found. Run 'devflow setup-zfs' to create one."
                    );
                }
                return None;
            }

            if quiet_output {
                return None;
            }

            println!();
            println!("ZFS tools detected but no ZFS pool found.");
            println!("devflow can create a file-backed ZFS pool for near-instant Copy-on-Write branching.");
            println!();
            println!("This will:");
            println!("  1. Create a 10G sparse image at /var/lib/devflow/pgdata.img");
            println!("  2. Create ZFS pool 'devflow' with compression=lz4, recordsize=8k");
            println!("  3. Mount at /var/lib/devflow/data");
            println!();
            println!("Note: This requires sudo. The 10G image is sparse (starts at ~0 disk usage, grows as needed).");
            println!();

            let confirm = inquire::Confirm::new("Create a file-backed ZFS pool?")
                .with_default(true)
                .prompt();

            match confirm {
                Ok(true) => {
                    let config = ZfsPoolSetupConfig::default();
                    match create_file_backed_pool(&config).await {
                        Ok(data_root) => {
                            println!("ZFS pool 'devflow' created successfully");
                            println!();
                            Some(data_root)
                        }
                        Err(e) => {
                            eprintln!("Warning: ZFS pool creation failed: {}", e);
                            eprintln!("Continuing without ZFS (will use copy/reflink fallback).");
                            None
                        }
                    }
                }
                Ok(false) => {
                    println!("Skipping ZFS setup. You can run 'devflow setup-zfs' later.");
                    None
                }
                Err(_) => {
                    println!("Skipping ZFS setup.");
                    None
                }
            }
        }
    }
}

pub(super) async fn init_local_service_main(
    config: &Config,
    named_cfg: &devflow_core::config::NamedServiceConfig,
    from: Option<&str>,
    quiet_output: bool,
) {
    match services::factory::create_provider_from_named_config(config, named_cfg).await {
        Ok(be) => {
            match be.create_workspace("main", None).await {
                Ok(info) => {
                    if !quiet_output {
                        println!("Created main workspace");
                    }
                    if let Ok(conn) = be.get_connection_info("main").await {
                        if let Some(ref uri) = conn.connection_string {
                            if !quiet_output {
                                println!("  Connection: {}", uri);
                            }
                        }
                    }
                    if let Some(state) = &info.state {
                        if !quiet_output {
                            println!("  State: {}", state);
                        }
                    }

                    // Seed if --from specified
                    if let Some(source) = from {
                        if !quiet_output {
                            println!("Seeding main workspace from: {}", source);
                        }
                        match be.seed_from_source("main", source).await {
                            Ok(_) => {
                                if !quiet_output {
                                    println!("Seeding completed successfully");
                                }
                            }
                            Err(e) => eprintln!("Warning: seeding failed: {}", e),
                        }
                    }
                }
                Err(e) => {
                    eprintln!(
                        "Warning: could not create main workspace for '{}': {}",
                        named_cfg.name, e
                    );
                    eprintln!("  You can create it later with: devflow service create main");
                }
            }
        }
        Err(e) => {
            eprintln!(
                "Warning: could not initialize service '{}': {}",
                named_cfg.name, e
            );
            eprintln!(
                "  You can create the main workspace later with: devflow service create main"
            );
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct ComposeProcessSuggestion {
    pub name: String,
    pub daemon: ProcessDaemonConfig,
}

pub(super) fn discover_compose_processes() -> Result<Vec<ComposeProcessSuggestion>> {
    let files = devflow_core::docker::find_docker_compose_files();
    let mut services = HashMap::new();

    for file in files {
        let content = std::fs::read_to_string(&file)?;
        let value: Value = serde_yaml_ng::from_str(&content)?;
        let Some(service_map) = mapping_get(&value, "services").and_then(Value::as_mapping) else {
            continue;
        };

        for (key, service_value) in service_map {
            let Some(name) = key.as_str() else { continue };
            if service_value.as_mapping().is_some() {
                services.insert(name.to_string(), service_value.clone());
            }
        }
    }

    let service_names: HashSet<String> = services.keys().cloned().collect();
    let mut suggestions = Vec::new();

    for (name, value) in services {
        let Some(service) = value.as_mapping() else {
            continue;
        };
        if is_compose_data_service(&name, &value) {
            continue;
        }

        let compose_command = mapping_get(&value, "command")
            .and_then(value_to_command)
            .unwrap_or_else(|| format!("docker compose up --no-deps {name}"));
        let port = extract_port_from_service(&value)
            .or_else(|| extract_port_from_command(&compose_command));
        let mut command = adapt_compose_command_for_host(&compose_command);
        if let Some(port) = port {
            command = command
                .replace(&format!("0.0.0.0:{port}"), "127.0.0.1:$PORT")
                .replace(&format!("localhost:{port}"), "127.0.0.1:$PORT");
        }

        let mut env = IndexMap::new();
        env.insert(
            "DEVFLOW_WORKSPACE".to_string(),
            "{{ workspace }}".to_string(),
        );

        let depends = extract_depends(service)
            .into_iter()
            .filter(|dep| service_names.contains(dep) && !is_data_service_name(dep))
            .collect();

        let daemon = ProcessDaemonConfig {
            run: command,
            dir: None,
            env,
            required: !looks_optional_process(&name),
            depends,
            port: port.map(|port| ProcessPortConfig {
                expect: vec![port],
                bump: ProcessPortBump(50),
            }),
            ready_delay: None,
            ready_port: None,
            ready_http: None,
            ready_cmd: None,
            ready_output: None,
            ready_timeout: None,
            stop_timeout: None,
            shutdown_signal: None,
            watch: Vec::new(),
            retry: None,
        };

        suggestions.push(ComposeProcessSuggestion { name, daemon });
    }

    suggestions.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(suggestions)
}

pub(super) fn install_compose_processes(
    config: &mut Config,
    config_path: &std::path::Path,
    suggestions: Vec<ComposeProcessSuggestion>,
) -> Result<usize> {
    if suggestions.is_empty() {
        return Ok(0);
    }

    let processes = config.processes.get_or_insert_with(|| ProcessesConfig {
        provider: "pitchfork".to_string(),
        auto_start: true,
        auto_stop: true,
        pitchfork: Some(PitchforkProcessConfig::default()),
        daemons: IndexMap::new(),
    });
    if processes.provider == "native" {
        processes.provider = "pitchfork".to_string();
    }

    let mut added = 0usize;
    for suggestion in suggestions {
        if !processes.daemons.contains_key(&suggestion.name) {
            processes.daemons.insert(suggestion.name, suggestion.daemon);
            added += 1;
        }
    }

    if added > 0 {
        config.save_to_file(config_path)?;
    }
    Ok(added)
}

fn mapping_get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value
        .as_mapping()
        .and_then(|mapping| mapping.get(Value::String(key.to_string())))
}

fn value_to_command(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Sequence(items) => {
            let parts: Vec<String> = items.iter().filter_map(value_scalar_to_string).collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join(" "))
            }
        }
        _ => None,
    }
}

fn value_scalar_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn adapt_compose_command_for_host(command: &str) -> String {
    let trimmed = command.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("uv run ")
        || trimmed.starts_with("mise x -- uv run ")
        || trimmed.starts_with("docker compose ")
    {
        return command.to_string();
    }

    let first = trimmed.split_whitespace().next().unwrap_or_default();
    let looks_python_entrypoint = matches!(
        first,
        "python"
            | "python3"
            | "celery"
            | "django-admin"
            | "gunicorn"
            | "uvicorn"
            | "rq"
            | "dramatiq"
    );
    if !looks_python_entrypoint {
        return command.to_string();
    }

    if std::path::Path::new("uv.lock").exists() || std::path::Path::new("pyproject.toml").exists() {
        if project_uses_mise_for_uv() {
            format!("mise x -- uv run {trimmed}")
        } else {
            format!("uv run {trimmed}")
        }
    } else if std::path::Path::new(".venv/bin/python").exists() {
        if matches!(first, "python" | "python3") {
            trimmed.replacen(first, ".venv/bin/python", 1)
        } else {
            format!(".venv/bin/{trimmed}")
        }
    } else {
        command.to_string()
    }
}

fn project_uses_mise_for_uv() -> bool {
    ["mise.toml", ".mise.toml"]
        .into_iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .any(|content| content.contains("uv") || content.contains("astral-sh/uv"))
}

fn is_compose_data_service(name: &str, value: &Value) -> bool {
    if is_data_service_name(name) {
        return true;
    }
    mapping_get(value, "image")
        .and_then(Value::as_str)
        .map(is_data_service_name)
        .unwrap_or(false)
}

fn is_data_service_name(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    [
        "postgres",
        "postgresql",
        "postgis",
        "pgvector",
        "timescale",
        "mysql",
        "mariadb",
        "clickhouse",
        "redis",
        "valkey",
        "dragonfly",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn looks_optional_process(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [
        "worker",
        "beat",
        "cron",
        "bot",
        "queue",
        "scheduler",
        "integration",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn extract_depends(service: &serde_yaml_ng::Mapping) -> Vec<String> {
    let Some(depends) = service.get(Value::String("depends_on".to_string())) else {
        return Vec::new();
    };
    match depends {
        Value::Sequence(items) => items
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect(),
        Value::Mapping(mapping) => mapping
            .keys()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn extract_port_from_service(value: &Value) -> Option<u16> {
    let ports = mapping_get(value, "ports")?.as_sequence()?;
    for port in ports {
        match port {
            Value::String(s) => {
                if let Some(port) = parse_compose_short_port(s) {
                    return Some(port);
                }
            }
            Value::Number(n) => {
                if let Some(port) = n.as_u64().and_then(|n| u16::try_from(n).ok()) {
                    return Some(port);
                }
            }
            Value::Mapping(mapping) => {
                for key in ["published", "target"] {
                    if let Some(port) = mapping
                        .get(Value::String(key.to_string()))
                        .and_then(value_to_port)
                    {
                        return Some(port);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn value_to_port(value: &Value) -> Option<u16> {
    match value {
        Value::Number(n) => n.as_u64().and_then(|n| u16::try_from(n).ok()),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn parse_compose_short_port(value: &str) -> Option<u16> {
    let without_protocol = value.split('/').next().unwrap_or(value);
    let parts: Vec<&str> = without_protocol.split(':').collect();
    match parts.as_slice() {
        [port] => port.parse().ok(),
        // HOST:CONTAINER or IP:HOST:CONTAINER. In both cases the published
        // host port is immediately before the container port.
        parts if parts.len() >= 2 => parts[parts.len() - 2].parse().ok(),
        _ => None,
    }
}

fn extract_port_from_command(command: &str) -> Option<u16> {
    let re = regex::Regex::new(r"(?:(?:0\.0\.0\.0|127\.0\.0\.1|localhost):)(\d{2,5})").ok()?;
    re.captures(command)
        .and_then(|captures| captures.get(1))
        .and_then(|port| port.as_str().parse().ok())
}

/// Destroy a devflow project and all associated resources.
///
/// This is the inverse of `devflow init`. It removes:
///   1. All service data (containers, databases, workspaces) via destroy_project()
///   2. Git worktrees created by devflow
///   3. VCS hooks installed by devflow
///   4. Workspace registry and local state for this project
///   5. Hook approvals for this project
///   6. Configuration files (.devflow.yml, .devflow.local.yml)
pub(super) async fn handle_destroy_project(
    config: &mut Config,
    config_path: &Option<std::path::PathBuf>,
    force: bool,
    json_output: bool,
    non_interactive: bool,
) -> Result<()> {
    let project_dir = std::env::current_dir()?;
    let project_name = config.name.clone().unwrap_or_else(|| {
        project_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string())
    });

    // Gather preview info
    let vcs_repo = vcs::detect_vcs_provider(".").ok();

    // Inject services from local state so we can destroy them
    if let Some(ref path) = config_path {
        if let Ok(state_mgr) = LocalStateManager::new() {
            if let Some(state_services) = state_mgr.get_services(path) {
                config.services = Some(state_services);
            }
        }
    }

    let service_configs = config.resolve_services();
    let config_file_path = project_dir.join(".devflow.yml");
    let local_config_path = project_dir.join(".devflow.local.yml");

    // Count worktrees
    let worktrees: Vec<vcs::WorktreeInfo> = vcs_repo
        .as_ref()
        .and_then(|repo| repo.list_worktrees().ok())
        .unwrap_or_default();
    // Filter to non-main worktrees (those that devflow would have created)
    let removable_worktrees: Vec<&vcs::WorktreeInfo> =
        worktrees.iter().filter(|wt| !wt.is_main).collect();

    // Confirm unless --force
    if !force {
        if json_output || non_interactive {
            anyhow::bail!(
                "Use --force to confirm project destruction in non-interactive or JSON output mode"
            );
        }

        println!(
            "This will permanently destroy the devflow project '{}':",
            project_name
        );
        println!();

        if !service_configs.is_empty() {
            println!("  Services ({}):", service_configs.len());
            for svc in &service_configs {
                println!("    - {} (all workspaces and data)", svc.name);
            }
        } else {
            println!("  Services: none configured");
        }

        if !removable_worktrees.is_empty() {
            println!("  Worktrees ({}):", removable_worktrees.len());
            for wt in &removable_worktrees {
                println!("    - {}", wt.path.display());
            }
        }

        if vcs_repo.is_some() {
            println!("  VCS hooks: will be uninstalled");
        }

        println!("  Workspace registry: will be cleared");

        if config_file_path.exists() {
            println!("  Config: {} (will be deleted)", config_file_path.display());
        }
        if local_config_path.exists() {
            println!(
                "  Local config: {} (will be deleted)",
                local_config_path.display()
            );
        }

        println!();
        println!("This is irreversible.");

        let confirm = inquire::Confirm::new("Are you sure you want to destroy this project?")
            .with_default(false)
            .prompt()?;

        if !confirm {
            println!("Aborted.");
            return Ok(());
        }
    }

    let mut destroyed_services: Vec<serde_json::Value> = Vec::new();
    let mut worktrees_removed = 0usize;
    let mut hooks_uninstalled = false;
    let mut state_cleared = false;
    let mut config_deleted = false;
    let mut local_config_deleted = false;

    // 1. Destroy all service data
    for svc_config in &service_configs {
        if !json_output {
            println!("Destroying service '{}'...", svc_config.name);
        }
        match services::factory::create_provider_from_named_config(config, svc_config).await {
            Ok(provider) => {
                if provider.supports_destroy() {
                    match provider.destroy_project().await {
                        Ok(workspaces) => {
                            if !json_output {
                                println!(
                                    "  Destroyed '{}': {} workspace(es) removed",
                                    svc_config.name,
                                    workspaces.len()
                                );
                            }
                            destroyed_services.push(serde_json::json!({
                                "service": svc_config.name,
                                "success": true,
                                "workspaces_destroyed": workspaces,
                            }));
                        }
                        Err(e) => {
                            log::warn!("Failed to destroy service '{}': {}", svc_config.name, e);
                            if !json_output {
                                println!(
                                    "  Warning: Failed to destroy '{}': {}",
                                    svc_config.name, e
                                );
                            }
                            destroyed_services.push(serde_json::json!({
                                "service": svc_config.name,
                                "success": false,
                                "error": e.to_string(),
                            }));
                        }
                    }
                } else {
                    // Provider doesn't support destroy — try deleting all workspaces individually
                    match provider.list_workspaces().await {
                        Ok(workspaces) => {
                            let mut deleted = 0;
                            for workspace in &workspaces {
                                if let Err(e) = provider.delete_workspace(&workspace.name).await {
                                    log::warn!(
                                        "Failed to delete workspace '{}' on '{}': {}",
                                        workspace.name,
                                        svc_config.name,
                                        e
                                    );
                                } else {
                                    deleted += 1;
                                }
                            }
                            if !json_output {
                                println!(
                                    "  Deleted {}/{} workspace(es) from '{}'",
                                    deleted,
                                    workspaces.len(),
                                    svc_config.name
                                );
                            }
                            destroyed_services.push(serde_json::json!({
                                "service": svc_config.name,
                                "success": true,
                                "branches_deleted": deleted,
                            }));
                        }
                        Err(e) => {
                            log::warn!(
                                "Failed to list workspaces for service '{}': {}",
                                svc_config.name,
                                e
                            );
                            if !json_output {
                                println!(
                                    "  Warning: Could not clean up '{}': {}",
                                    svc_config.name, e
                                );
                            }
                            destroyed_services.push(serde_json::json!({
                                "service": svc_config.name,
                                "success": false,
                                "error": e.to_string(),
                            }));
                        }
                    }
                }
            }
            Err(e) => {
                log::warn!(
                    "Failed to create provider for service '{}': {}",
                    svc_config.name,
                    e
                );
                if !json_output {
                    println!(
                        "  Warning: Could not initialize '{}': {}",
                        svc_config.name, e
                    );
                }
                destroyed_services.push(serde_json::json!({
                    "service": svc_config.name,
                    "success": false,
                    "error": e.to_string(),
                }));
            }
        }
    }

    // 2. Remove worktrees (if VCS available)
    if let Some(ref repo) = vcs_repo {
        for wt in &removable_worktrees {
            if !json_output {
                println!("Removing worktree: {}", wt.path.display());
            }
            if let Err(e) = repo.remove_worktree(&wt.path, force) {
                if !force {
                    log::warn!("Skipping worktree: {}", e);
                    if !json_output {
                        println!("  Skipping {}: {}", wt.path.display(), e);
                    }
                    continue;
                }
                log::warn!("Failed to remove worktree via VCS: {}", e);
                // Forced: fall back to filesystem removal
                if wt.path.exists() {
                    if let Err(e2) = std::fs::remove_dir_all(&wt.path) {
                        log::warn!("Failed to remove worktree directory: {}", e2);
                        if !json_output {
                            println!("  Warning: Could not remove {}: {}", wt.path.display(), e2);
                        }
                        continue;
                    }
                }
            }
            worktrees_removed += 1;
        }
    }

    // 3. Uninstall VCS hooks
    if let Some(ref repo) = vcs_repo {
        match repo.uninstall_hooks() {
            Ok(_) => {
                hooks_uninstalled = true;
                if !json_output {
                    println!("Uninstalled VCS hooks.");
                }
            }
            Err(e) => {
                log::warn!("Failed to uninstall hooks: {}", e);
                if !json_output {
                    println!("Warning: Could not uninstall hooks: {}", e);
                }
            }
        }
    }

    // 4. Clear local state (workspace registry, services, current workspace)
    if let Some(ref path) = config_path {
        if let Ok(mut state_mgr) = LocalStateManager::new() {
            if let Err(e) = state_mgr.remove_project(path) {
                log::warn!("Failed to clear project state: {}", e);
                if !json_output {
                    println!("Warning: Could not clear project state: {}", e);
                }
            } else {
                state_cleared = true;
                if !json_output {
                    println!("Cleared project state and workspace registry.");
                }
            }
        }
    }

    // 5. Clear hook approvals
    if let Some(ref path) = config_path {
        if let Ok(state_mgr) = LocalStateManager::new() {
            if let Some(project_key) = state_mgr.get_project_key_for(path) {
                if let Ok(mut store) = ApprovalStore::load() {
                    if let Err(e) = store.clear_project(&project_key) {
                        log::warn!("Failed to clear hook approvals: {}", e);
                    }
                }
            }
        }
    }

    // 6. Delete config files
    if config_file_path.exists() {
        if let Err(e) = std::fs::remove_file(&config_file_path) {
            log::warn!("Failed to delete config file: {}", e);
            if !json_output {
                println!(
                    "Warning: Could not delete {}: {}",
                    config_file_path.display(),
                    e
                );
            }
        } else {
            config_deleted = true;
            if !json_output {
                println!("Deleted {}", config_file_path.display());
            }
        }
    }
    if local_config_path.exists() {
        if let Err(e) = std::fs::remove_file(&local_config_path) {
            log::warn!("Failed to delete local config file: {}", e);
            if !json_output {
                println!(
                    "Warning: Could not delete {}: {}",
                    local_config_path.display(),
                    e
                );
            }
        } else {
            local_config_deleted = true;
            if !json_output {
                println!("Deleted {}", local_config_path.display());
            }
        }
    }

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": "ok",
                "project": project_name,
                "services": destroyed_services,
                "worktrees_removed": worktrees_removed,
                "hooks_uninstalled": hooks_uninstalled,
                "state_cleared": state_cleared,
                "config_deleted": config_deleted,
                "local_config_deleted": local_config_deleted,
            }))?
        );
    } else {
        println!();
        println!("Project '{}' destroyed.", project_name);
    }

    Ok(())
}
