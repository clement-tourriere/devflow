pub mod command_guard;
pub mod landlock;
pub mod platform;
pub mod seatbelt;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use command_guard::CommandGuard;
use platform::PlatformCapability;

// ── Config structs ──────────────────────────────────────────────────────────

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SandboxConfig {
    /// All new workspaces are sandboxed by default.
    #[serde(default)]
    pub default: bool,
    /// Agent workspaces (agent/*) are always sandboxed by default.
    #[serde(default = "default_true")]
    pub default_for_agents: bool,
    /// Filesystem sandbox configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filesystem: Option<SandboxFilesystemConfig>,
    /// Command blocking configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commands: Option<SandboxCommandConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SandboxFilesystemConfig {
    /// Extra paths that should be readable (beyond defaults).
    #[serde(default)]
    pub extra_read: Vec<String>,
    /// Extra paths that should be writable (beyond workspace dir).
    #[serde(default)]
    pub extra_write: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SandboxCommandConfig {
    /// Additional command patterns to block.
    #[serde(default)]
    pub extra_block: Vec<String>,
    /// Override default blocks for this project (allow these commands).
    #[serde(default)]
    pub allow: Vec<String>,
}

// ── Runtime policy ──────────────────────────────────────────────────────────

/// Runtime sandbox policy that combines command blocking with platform-level
/// filesystem isolation.
pub struct SandboxPolicy {
    pub command_guard: CommandGuard,
    pub platform: PlatformCapability,
    pub workspace_dir: PathBuf,
    pub extra_read_paths: Vec<PathBuf>,
    pub extra_write_paths: Vec<PathBuf>,
    /// Hold the temp file for the Seatbelt profile so it lives as long as the policy.
    #[cfg(target_os = "macos")]
    _seatbelt_profile: Option<tempfile::NamedTempFile>,
    #[cfg(target_os = "macos")]
    seatbelt_profile_path: Option<PathBuf>,
}

impl SandboxPolicy {
    /// Build a sandbox policy from config and runtime context.
    pub fn from_config(sandbox_config: &SandboxConfig, workspace_dir: &Path) -> Self {
        let (extra_block, allow) = sandbox_config
            .commands
            .as_ref()
            .map(|c| (c.extra_block.clone(), c.allow.clone()))
            .unwrap_or_default();

        let command_guard = CommandGuard::from_config(&extra_block, &allow);
        let platform_cap = platform::detect();

        let (extra_read, extra_write): (Vec<PathBuf>, Vec<PathBuf>) = sandbox_config
            .filesystem
            .as_ref()
            .map(|f| {
                (
                    f.extra_read.iter().map(PathBuf::from).collect::<Vec<_>>(),
                    f.extra_write.iter().map(PathBuf::from).collect::<Vec<_>>(),
                )
            })
            .unwrap_or_default();

        #[cfg(target_os = "macos")]
        let (seatbelt_file, seatbelt_path) = if platform_cap == PlatformCapability::Seatbelt {
            let profile =
                seatbelt::generate_seatbelt_profile(workspace_dir, &extra_read, &extra_write);
            match seatbelt::write_profile_to_temp(&profile) {
                Ok(tmp) => {
                    let path = tmp.path().to_path_buf();
                    (Some(tmp), Some(path))
                }
                Err(e) => {
                    log::warn!("Failed to write Seatbelt profile: {}", e);
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        Self {
            command_guard,
            platform: platform_cap,
            workspace_dir: workspace_dir.to_path_buf(),
            extra_read_paths: extra_read,
            extra_write_paths: extra_write,
            #[cfg(target_os = "macos")]
            _seatbelt_profile: seatbelt_file,
            #[cfg(target_os = "macos")]
            seatbelt_profile_path: seatbelt_path,
        }
    }

    /// Build a default sandbox policy (no config overrides).
    pub fn default_for_workspace(workspace_dir: &Path) -> Self {
        Self::from_config(&SandboxConfig::default(), workspace_dir)
    }

    /// Check if a command is allowed by the sandbox policy.
    pub fn validate_command(&self, command: &str) -> Result<()> {
        self.command_guard.check(command)
    }

    /// Wrap a command string for sandboxed execution.
    ///
    /// On macOS: wraps with `sandbox-exec -f <profile>`.
    /// On Linux with Landlock: returns the command as-is (Landlock is applied via pre_exec).
    /// On unsupported platforms: returns the command as-is (command guard still applies).
    ///
    /// Returns (program, args) tuple.
    pub fn wrap_command_string(&self, command: &str) -> (String, Vec<String>) {
        #[cfg(target_os = "macos")]
        {
            if let Some(ref profile_path) = self.seatbelt_profile_path {
                return seatbelt::wrap_command(command, profile_path);
            }
        }

        // Default: run via sh -c
        (
            "sh".to_string(),
            vec!["-c".to_string(), command.to_string()],
        )
    }

    /// Apply filesystem sandbox to a `std::process::Command`.
    ///
    /// On macOS: rewrites the command to run under sandbox-exec.
    /// On Linux: sets pre_exec for Landlock (unsafe, runs in child after fork).
    /// On unsupported: no-op (command guard still applies).
    #[allow(unused_variables)]
    pub fn apply_to_command(&self, cmd: &mut std::process::Command) -> Result<()> {
        match &self.platform {
            #[cfg(target_os = "linux")]
            PlatformCapability::Landlock { .. } => {
                let workspace_dir = self.workspace_dir.clone();
                let extra_read = self.extra_read_paths.clone();
                let extra_write = self.extra_write_paths.clone();

                // SAFETY: pre_exec runs in the child process after fork.
                // We only call landlock syscalls which are fork-safe.
                unsafe {
                    use std::os::unix::process::CommandExt;
                    cmd.pre_exec(move || {
                        landlock::apply_landlock(&workspace_dir, &extra_read, &extra_write)
                            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
                    });
                }
            }
            _ => {
                // Seatbelt is handled by wrapping the command string, not by modifying cmd.
                // No-op for unsupported platforms.
            }
        }
        Ok(())
    }
}

/// Resolve whether a workspace should be sandboxed based on flags and config.
///
/// Priority: CLI flags > env var > config defaults.
pub fn resolve_sandbox_enabled(
    sandboxed_flag: bool,
    no_sandbox_flag: bool,
    is_agent_workspace: bool,
    config: Option<&SandboxConfig>,
) -> bool {
    // CLI flags take highest priority
    if sandboxed_flag {
        return true;
    }
    if no_sandbox_flag {
        return false;
    }

    // Environment variable override
    if let Ok(val) = std::env::var("DEVFLOW_SANDBOX") {
        match val.to_lowercase().as_str() {
            "true" | "1" | "yes" => return true,
            "false" | "0" | "no" => return false,
            _ => {}
        }
    }

    // Config defaults
    if let Some(sandbox_config) = config {
        if is_agent_workspace && sandbox_config.default_for_agents {
            return true;
        }
        return sandbox_config.default;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_sandbox_flags_override() {
        assert!(resolve_sandbox_enabled(true, false, false, None));
        assert!(!resolve_sandbox_enabled(false, true, false, None));
        // --sandboxed wins over --no-sandbox
        assert!(resolve_sandbox_enabled(true, true, false, None));
    }

    #[test]
    fn test_resolve_sandbox_config_default() {
        let config = SandboxConfig {
            default: true,
            ..Default::default()
        };
        assert!(resolve_sandbox_enabled(false, false, false, Some(&config)));
    }

    #[test]
    fn test_resolve_sandbox_agent_default() {
        let config = SandboxConfig {
            default: false,
            default_for_agents: true,
            ..Default::default()
        };
        assert!(resolve_sandbox_enabled(false, false, true, Some(&config)));
        assert!(!resolve_sandbox_enabled(false, false, false, Some(&config)));
    }

    #[test]
    fn test_resolve_sandbox_no_config() {
        assert!(!resolve_sandbox_enabled(false, false, false, None));
        assert!(!resolve_sandbox_enabled(false, false, true, None));
    }

    #[test]
    fn test_policy_validate_command() {
        let policy = SandboxPolicy::default_for_workspace(Path::new("/tmp/workspace"));
        assert!(policy.validate_command("git commit -m 'test'").is_ok());
        assert!(policy.validate_command("git push").is_err());
    }

    #[test]
    fn test_policy_wrap_command_string() {
        let policy = SandboxPolicy::default_for_workspace(Path::new("/tmp/workspace"));
        let (prog, args) = policy.wrap_command_string("echo hello");

        // On macOS with sandbox-exec: wraps with sandbox-exec
        // On other platforms: wraps with sh -c
        assert!(!prog.is_empty());
        assert!(!args.is_empty());
    }
}
