pub mod compose;
#[cfg(feature = "service-local")]
pub mod discovery;

// Re-export compose functions for backward compatibility
pub use compose::*;
#[cfg(feature = "service-local")]
pub use discovery::*;

#[cfg(feature = "service-local")]
pub mod settings {
    use crate::config::DockerCustomSettings;
    use bollard::models::{ContainerCreateBody, HostConfig, RestartPolicy, RestartPolicyNameEnum};

    /// Apply custom Docker settings to container creation parameters.
    pub fn apply_custom_settings(
        body: &mut ContainerCreateBody,
        host_config: &mut HostConfig,
        settings: &DockerCustomSettings,
    ) {
        if !settings.command.is_empty() {
            body.cmd = Some(settings.command.clone());
        }
        if !settings.environment.is_empty() {
            let env = body.env.get_or_insert_with(Vec::new);
            for (k, v) in &settings.environment {
                env.push(format!("{k}={v}"));
            }
        }
        if let Some(ref policy) = settings.restart_policy {
            host_config.restart_policy = Some(parse_restart_policy(policy));
        }
    }

    fn parse_restart_policy(policy: &str) -> RestartPolicy {
        if let Some(count_str) = policy.strip_prefix("on-failure:") {
            let count = count_str.parse::<i64>().unwrap_or(0);
            RestartPolicy {
                name: Some(RestartPolicyNameEnum::ON_FAILURE),
                maximum_retry_count: Some(count),
            }
        } else {
            let name = match policy {
                "always" => RestartPolicyNameEnum::ALWAYS,
                "unless-stopped" => RestartPolicyNameEnum::UNLESS_STOPPED,
                "on-failure" => RestartPolicyNameEnum::ON_FAILURE,
                _ => RestartPolicyNameEnum::EMPTY,
            };
            RestartPolicy {
                name: Some(name),
                maximum_retry_count: None,
            }
        }
    }
}
