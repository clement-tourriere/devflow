use anyhow::Result;
use devflow_core::config::Config;
use devflow_core::processes::{
    PitchforkProcessConfig, ProcessDaemonConfig, ProcessPortBump, ProcessPortConfig,
    ProcessesConfig,
};
use devflow_core::services::{self};
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
            let default_workspace = config.git.main_workspace.as_str();
            let fallback_dir = std::env::current_dir().unwrap_or_else(|_| ".".into());
            let project_dir = config.project_root.as_deref().unwrap_or(&fallback_dir);
            let service_key =
                match devflow_core::state::LocalStateManager::new().and_then(|state| {
                    state.resolve_workspace_service_key_by_dir(project_dir, default_workspace)
                }) {
                    Ok(service_key) => service_key,
                    Err(error) => {
                        eprintln!(
                            "Warning: refusing to initialize service workspace '{}': {}",
                            default_workspace, error
                        );
                        return;
                    }
                };
            match be.create_workspace(&service_key, None).await {
                Ok(info) => {
                    if !quiet_output {
                        println!("Created '{}' workspace", default_workspace);
                    }
                    if let Ok(conn) = be.get_connection_info(&service_key).await {
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
                            println!("Seeding '{}' workspace from: {}", default_workspace, source);
                        }
                        match be.seed_from_source(&service_key, source).await {
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
                        "Warning: could not create default workspace '{}' for '{}': {}",
                        default_workspace, named_cfg.name, e
                    );
                    eprintln!(
                        "  You can create it later with: devflow service create {}",
                        default_workspace
                    );
                }
            }
        }
        Err(e) => {
            eprintln!(
                "Warning: could not initialize service '{}': {}",
                named_cfg.name, e
            );
            eprintln!(
                "  You can create the default workspace later with: devflow service create {}",
                config.git.main_workspace
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
/// The teardown itself lives in [`devflow_core::project::destroy`] and is
/// shared with the GUI (and any future frontend); this handler only renders
/// the confirmation preview and the outcome.
pub(super) async fn handle_destroy_project(
    force: bool,
    json_output: bool,
    non_interactive: bool,
) -> Result<()> {
    let project_dir = std::env::current_dir()?;
    let plan = devflow_core::project::destroy_plan(&project_dir)?;

    // Confirm unless --force
    if !force {
        if json_output || non_interactive {
            anyhow::bail!(
                "Use --force to confirm project destruction in non-interactive or JSON output mode"
            );
        }

        println!(
            "This will permanently destroy the devflow project '{}':",
            plan.project_name
        );
        println!();

        if !plan.services.is_empty() {
            println!("  Services ({}):", plan.services.len());
            for name in &plan.services {
                println!("    - {} (all workspaces and data)", name);
            }
        } else {
            println!("  Services: none configured");
        }

        if !plan.worktrees.is_empty() {
            println!("  Worktrees ({}):", plan.worktrees.len());
            for path in &plan.worktrees {
                println!("    - {}", path.display());
            }
        }

        if plan.has_vcs {
            println!("  VCS hooks: will be uninstalled");
        }

        println!("  Workspace processes: will be stopped, runtime state cleared");
        println!("  Workspace registry: will be cleared");

        if let Some(ref path) = plan.config_path {
            println!("  Config: {} (will be deleted)", path.display());
        }
        if let Some(ref path) = plan.local_config_path {
            println!("  Local config: {} (will be deleted)", path.display());
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

    let options = devflow_core::project::DestroyOptions {
        force_worktrees: force,
    };
    let print_line = |line: &str| println!("{line}");
    let progress: devflow_core::project::DestroyProgress =
        if json_output { None } else { Some(&print_line) };
    let outcome = devflow_core::project::destroy(&project_dir, options, progress).await?;

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": "ok",
                "project": outcome.project_name,
                "processes_stopped": outcome.processes_stopped,
                "services": outcome.services_destroyed,
                "worktrees_removed": outcome.worktrees_removed,
                "hooks_uninstalled": outcome.hooks_uninstalled,
                "state_cleared": outcome.state_cleared,
                "config_deleted": outcome.config_deleted,
                "local_config_deleted": outcome.local_config_deleted,
            }))?
        );
    } else {
        println!();
        println!("Project '{}' destroyed.", outcome.project_name);
    }

    Ok(())
}
