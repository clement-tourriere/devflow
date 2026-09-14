//! Versioned provider catalog shared by every frontend. No engine is started.
use super::OperationCapabilities;
use serde::Serialize;
#[derive(Debug, Clone, Serialize)]
pub struct ProviderDescriptor {
    pub schema_version: u32,
    pub provider: &'static str,
    pub service_types: &'static [&'static str],
    pub available: bool,
    pub experimental: bool,
    pub data_strategy: &'static str,
    pub operations: OperationCapabilities,
    pub required_configuration: &'static [&'static str],
    pub allocation_limit: Option<u32>,
}
pub fn providers() -> Vec<ProviderDescriptor> {
    let local = cfg!(feature = "service-local");
    let containers = OperationCapabilities {
        start: true,
        stop: true,
        logs: true,
        ..Default::default()
    };
    vec![
        ProviderDescriptor {
            schema_version: 1,
            provider: "local",
            service_types: &["postgres"],
            available: local,
            experimental: false,
            data_strategy: "physical_copy",
            operations: OperationCapabilities {
                reset: true,
                seed: true,
                snapshot: true,
                ..containers
            },
            required_configuration: &[],
            allocation_limit: None,
        },
        ProviderDescriptor {
            schema_version: 1,
            provider: "local",
            service_types: &["mysql", "clickhouse", "redis", "generic"],
            available: local,
            experimental: false,
            data_strategy: "per_workspace_container",
            operations: containers,
            required_configuration: &[],
            allocation_limit: None,
        },
        ProviderDescriptor {
            schema_version: 1,
            provider: "shared",
            service_types: &["postgres", "clickhouse", "rustfs"],
            available: local,
            experimental: false,
            data_strategy: "logical_namespace",
            operations: OperationCapabilities::default(),
            required_configuration: &[],
            allocation_limit: None,
        },
        ProviderDescriptor {
            schema_version: 1,
            provider: "shared",
            service_types: &["redis"],
            available: local,
            experimental: false,
            data_strategy: "empty_database",
            operations: OperationCapabilities::default(),
            required_configuration: &[],
            allocation_limit: Some(15),
        },
        ProviderDescriptor {
            schema_version: 1,
            provider: "neon",
            service_types: &["postgres"],
            available: cfg!(feature = "service-neon"),
            experimental: true,
            data_strategy: "remote_branch",
            operations: OperationCapabilities::default(),
            required_configuration: &["neon.api_key", "neon.project_id"],
            allocation_limit: None,
        },
        ProviderDescriptor {
            schema_version: 1,
            provider: "dblab",
            service_types: &["postgres"],
            available: cfg!(feature = "service-dblab"),
            experimental: true,
            data_strategy: "remote_clone",
            operations: OperationCapabilities::default(),
            required_configuration: &["dblab.api_url", "dblab.auth_token"],
            allocation_limit: None,
        },
        ProviderDescriptor {
            schema_version: 1,
            provider: "xata",
            service_types: &["postgres"],
            available: cfg!(feature = "service-xata"),
            experimental: true,
            data_strategy: "remote_branch",
            operations: OperationCapabilities::default(),
            required_configuration: &["xata.api_key", "xata.organization_id", "xata.project_id"],
            allocation_limit: None,
        },
        ProviderDescriptor {
            schema_version: 1,
            provider: "plugin",
            service_types: &["plugin"],
            available: true,
            experimental: true,
            data_strategy: "negotiated",
            operations: OperationCapabilities::default(),
            required_configuration: &["plugin.path or plugin.name"],
            allocation_limit: None,
        },
    ]
}
pub fn validate_choice(provider: &str, service: &str) -> anyhow::Result<ProviderDescriptor> {
    let descriptor = providers()
        .into_iter()
        .find(|p| p.provider == provider && p.service_types.contains(&service))
        .ok_or_else(|| anyhow::anyhow!("unsupported provider '{provider}' for '{service}'"))?;
    anyhow::ensure!(
        descriptor.available,
        "provider '{provider}' was not compiled into this application"
    );
    Ok(descriptor)
}
