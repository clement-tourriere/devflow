use anyhow::Result;
use devflow_core::config::Config;
use devflow_core::services::ServiceProvider;

/// Handle `devflow plugin` subcommands.
pub(super) async fn handle_plugin_command(
    action: super::PluginCommands,
    config: &Config,
    json_output: bool,
) -> Result<()> {
    match action {
        super::PluginCommands::List => {
            let services = config.resolve_services();
            let plugins: Vec<_> = services
                .iter()
                .filter(|b| b.service_type == "plugin")
                .collect();

            if plugins.is_empty() {
                if json_output {
                    println!("[]");
                } else {
                    println!("No plugin services configured.");
                    println!(
                        "Add a service with service_type: plugin in your .devflow.yml to get started."
                    );
                }
                return Ok(());
            }

            if json_output {
                let items: Vec<serde_json::Value> = plugins
                    .iter()
                    .map(|p| {
                        let plugin_cfg = p.plugin.as_ref();
                        let executable = plugin_cfg
                            .and_then(|c| {
                                c.path.clone().or_else(|| {
                                    c.name.as_ref().map(|n| format!("devflow-plugin-{}", n))
                                })
                            })
                            .unwrap_or_else(|| "(not configured)".to_string());
                        serde_json::json!({
                            "name": p.name,
                            "executable": executable,
                            "auto_workspace": p.auto_workspace,
                            "timeout": plugin_cfg.map(|c| c.timeout).unwrap_or(30),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else {
                println!("Plugin services ({}):", plugins.len());
                for p in &plugins {
                    let plugin_cfg = p.plugin.as_ref();
                    let executable = plugin_cfg
                        .and_then(|c| {
                            c.path.clone().or_else(|| {
                                c.name.as_ref().map(|n| format!("devflow-plugin-{}", n))
                            })
                        })
                        .unwrap_or_else(|| "(not configured)".to_string());
                    println!("  {} -> {}", p.name, executable);
                    if let Some(cfg) = plugin_cfg {
                        println!("    timeout: {}s", cfg.timeout);
                    }
                    println!("    auto_workspace: {}", p.auto_workspace);
                }
            }
        }
        super::PluginCommands::Check { name } => {
            let services = config.resolve_services();
            let named = services.iter().find(|b| b.name == name).ok_or_else(|| {
                anyhow::anyhow!(
                    "Service '{}' not found in configuration. Available services: {}",
                    name,
                    services
                        .iter()
                        .map(|b| b.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;

            if named.service_type != "plugin" {
                anyhow::bail!(
                    "Service '{}' is not a plugin (service_type: '{}')",
                    name,
                    named.service_type
                );
            }

            let plugin_cfg = named.plugin.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Service '{}' has type 'plugin' but no plugin config section",
                    name
                )
            })?;

            // Try to create the provider and invoke provider_name
            match devflow_core::services::plugin::PluginProvider::new(&name, plugin_cfg) {
                Ok(provider) => {
                    // Try test_connection as a health check
                    match provider.test_connection().await {
                        Ok(()) => {
                            if json_output {
                                println!(
                                    "{}",
                                    serde_json::to_string_pretty(&serde_json::json!({
                                        "status": "ok",
                                        "name": name,
                                        "reachable": true,
                                    }))?
                                );
                            } else {
                                println!("Plugin '{}': OK (reachable and responding)", name);
                            }
                        }
                        Err(e) => {
                            if json_output {
                                println!(
                                    "{}",
                                    serde_json::to_string_pretty(&serde_json::json!({
                                        "status": "error",
                                        "name": name,
                                        "reachable": false,
                                        "error": e.to_string(),
                                    }))?
                                );
                            } else {
                                println!("Plugin '{}': FAIL ({})", name, e);
                            }

                            anyhow::bail!("Plugin '{}' is unreachable: {}", name, e);
                        }
                    }
                }
                Err(e) => {
                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "status": "error",
                                "name": name,
                                "reachable": false,
                                "error": e.to_string(),
                            }))?
                        );
                    } else {
                        println!("Plugin '{}': FAIL (could not initialize: {})", name, e);
                    }

                    anyhow::bail!("Plugin '{}' could not initialize: {}", name, e);
                }
            }
        }
        super::PluginCommands::Init { name, lang } => {
            let script = match lang.as_str() {
                "bash" | "sh" => generate_plugin_skeleton_bash(&name),
                "python" | "py" => generate_plugin_skeleton_python(&name),
                other => {
                    anyhow::bail!(
                        "Unsupported plugin language '{}'. Supported: bash, python",
                        other
                    );
                }
            };

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "name": name,
                        "lang": lang,
                        "script": script,
                    }))?
                );
            } else {
                println!("{}", script);
            }
        }
    }

    Ok(())
}

/// Generate a skeleton bash plugin script.
fn generate_plugin_skeleton_bash(name: &str) -> String {
    format!(
        r#"#!/usr/bin/env bash
# devflow plugin: {name}
#
# This plugin is invoked by devflow with a JSON request on stdin.
# It should write a JSON response to stdout.
#
# Install: chmod +x this file, then reference in .devflow.yml:
#   services:
#     - name: {name}
#       service_type: plugin
#       plugin:
#         path: ./plugins/devflow-plugin-{name}
#         config:
#           key: value
#
set -euo pipefail

# Read the full JSON request from stdin
REQUEST=$(cat)

METHOD=$(echo "$REQUEST" | jq -r '.method')
PARAMS=$(echo "$REQUEST" | jq -c '.params // {{}}'  )
CONFIG=$(echo "$REQUEST" | jq -c '.config // {{}}'  )
SERVICE=$(echo "$REQUEST" | jq -r '.service_name')

ok()    {{ echo "{{\\"ok\\": true,  \\"result\\": $1}}"; }}
error() {{ echo "{{\\"ok\\": false, \\"error\\": \\"$1\\"}}"; }}

case "$METHOD" in
  provider_name)
    ok "\"{name}\""
    ;;
  test_connection)
    ok "null"
    ;;
  create_workspace)
    BRANCH=$(echo "$PARAMS" | jq -r '.workspace_name')
    # TODO: implement workspace creation for {name}
    ok "{{\\"name\\": \\"$BRANCH\\", \\"created_at\\": null, \\"parent_workspace\\": null, \\"database_name\\": \\"$BRANCH\\"}}"
    ;;
  delete_workspace)
    BRANCH=$(echo "$PARAMS" | jq -r '.workspace_name')
    # TODO: implement workspace deletion for {name}
    ok "null"
    ;;
  list_workspaces)
    # TODO: implement workspace listing for {name}
    ok "[]"
    ;;
  workspace_exists)
    BRANCH=$(echo "$PARAMS" | jq -r '.workspace_name')
    # TODO: implement workspace existence check
    ok "false"
    ;;
  switch_to_branch)
    BRANCH=$(echo "$PARAMS" | jq -r '.workspace_name')
    ok "{{\\"name\\": \\"$BRANCH\\", \\"created_at\\": null, \\"parent_workspace\\": null, \\"database_name\\": \\"$BRANCH\\"}}"
    ;;
  get_connection_info)
    BRANCH=$(echo "$PARAMS" | jq -r '.workspace_name')
    ok "{{\\"host\\": \\"localhost\\", \\"port\\": 6379, \\"database\\": \\"$BRANCH\\", \\"user\\": \\"default\\", \\"password\\": null, \\"connection_string\\": null}}"
    ;;
  doctor)
    ok "{{\\"checks\\": [{{  \\"name\\": \\"{name}\\", \\"available\\": true, \\"detail\\": \\"Plugin is running\\"}}]}}"
    ;;
  *)
    error "Unsupported method: $METHOD"
    ;;
esac
"#
    )
}

/// Generate a skeleton Python plugin script.
fn generate_plugin_skeleton_python(name: &str) -> String {
    format!(
        r#"#!/usr/bin/env python3
"""devflow plugin: {name}

This plugin is invoked by devflow with a JSON request on stdin.
It should write a JSON response to stdout.

Install: chmod +x this file, then reference in .devflow.yml:
  services:
    - name: {name}
      service_type: plugin
      plugin:
        path: ./plugins/devflow-plugin-{name}
        config:
          key: value
"""
import json
import sys
from datetime import datetime, timezone


def ok(result=None):
    print(json.dumps({{"ok": True, "result": result}}))

def error(msg: str):
    print(json.dumps({{"ok": False, "error": msg}}))

def main():
    request = json.loads(sys.stdin.read())
    method = request.get("method", "")
    params = request.get("params", {{}})
    config = request.get("config", {{}})
    service_name = request.get("service_name", "")

    if method == "provider_name":
        ok("{name}")
    elif method == "test_connection":
        ok(None)
    elif method == "create_workspace":
        workspace = params["workspace_name"]
        # TODO: implement workspace creation for {name}
        ok({{"name": workspace, "created_at": None, "parent_workspace": None, "database_name": workspace}})
    elif method == "delete_workspace":
        workspace = params["workspace_name"]
        # TODO: implement workspace deletion for {name}
        ok(None)
    elif method == "list_workspaces":
        # TODO: implement workspace listing for {name}
        ok([])
    elif method == "workspace_exists":
        workspace = params["workspace_name"]
        # TODO: implement workspace existence check
        ok(False)
    elif method == "switch_to_branch":
        workspace = params["workspace_name"]
        ok({{"name": workspace, "created_at": None, "parent_workspace": None, "database_name": workspace}})
    elif method == "get_connection_info":
        workspace = params["workspace_name"]
        ok({{
            "host": "localhost",
            "port": 6379,
            "database": workspace,
            "user": "default",
            "password": None,
            "connection_string": None,
        }})
    elif method == "doctor":
        ok({{"checks": [{{"name": "{name}", "available": True, "detail": "Plugin is running"}}]}})
    elif method == "cleanup_old_workspaces":
        ok([])
    elif method == "destroy_project":
        ok([])
    else:
        error(f"Unsupported method: {{method}}")

if __name__ == "__main__":
    main()
"#
    )
}
