use super::super::{ConnectionInfo, DoctorCheck, DoctorReport, ServiceProvider, WorkspaceInfo};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct NeonProvider {
    client: Client,
    api_key: String,
    project_id: String,
    base_url: String,
}

#[derive(Debug, Serialize)]
struct CreateBranchRequest {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NeonBranch {
    id: String,
    name: String,
    created_at: DateTime<Utc>,
    #[serde(default)]
    parent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListBranchesResponse {
    workspaces: Vec<NeonBranch>,
}

#[derive(Debug, Deserialize)]
struct CreateBranchResponse {
    workspace: NeonBranch,
}

#[derive(Debug, Deserialize)]
struct NeonEndpoint {
    #[allow(dead_code)]
    id: String,
    database_host: String,
    database_name: String,
    database_user: String,
    #[serde(default)]
    database_password: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListEndpointsResponse {
    endpoints: Vec<NeonEndpoint>,
}

impl NeonProvider {
    pub fn new(api_key: String, project_id: String, base_url: Option<String>) -> Result<Self> {
        let client = Client::new();
        let base_url = base_url.unwrap_or_else(|| "https://console.neon.tech/api/v2".to_string());

        Ok(Self {
            client,
            api_key,
            project_id,
            base_url,
        })
    }

    async fn make_request<T: for<'de> Deserialize<'de>>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&impl Serialize>,
    ) -> Result<T> {
        let url = format!("{}/{}", self.base_url, path);
        let mut request = self
            .client
            .request(method, &url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json");

        if let Some(body) = body {
            request = request.json(body);
        }

        let response = request
            .send()
            .await
            .with_context(|| format!("Failed to send request to {}", url))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            anyhow::bail!(
                "Neon API request failed with status {}: {}",
                status,
                error_text
            );
        }

        response
            .json()
            .await
            .with_context(|| "Failed to parse JSON response from Neon API")
    }

    async fn get_workspace_endpoint(&self, workspace_name: &str) -> Result<NeonEndpoint> {
        let path = format!("projects/{}/endpoints", self.project_id);
        let response: ListEndpointsResponse = self
            .make_request(reqwest::Method::GET, &path, None::<&()>)
            .await?;

        for endpoint in response.endpoints {
            if endpoint.database_name == workspace_name || endpoint.id.contains(workspace_name) {
                return Ok(endpoint);
            }
        }

        anyhow::bail!("No endpoint found for workspace: {}", workspace_name);
    }
}

#[async_trait]
impl ServiceProvider for NeonProvider {
    async fn create_workspace(
        &self,
        workspace_name: &str,
        from_workspace: Option<&str>,
    ) -> Result<WorkspaceInfo> {
        let request = CreateBranchRequest {
            name: workspace_name.to_string(),
            parent_id: from_workspace.map(|s| s.to_string()),
        };

        let path = format!("projects/{}/workspaces", self.project_id);
        let response: CreateBranchResponse = self
            .make_request(reqwest::Method::POST, &path, Some(&request))
            .await?;

        Ok(WorkspaceInfo {
            name: response.workspace.name,
            created_at: Some(response.workspace.created_at),
            parent_workspace: response.workspace.parent_id,
            database_name: response.workspace.id,
            state: Some("running".to_string()),
        })
    }

    async fn delete_workspace(&self, workspace_name: &str) -> Result<()> {
        let workspaces = self.list_workspaces().await?;
        let workspace = workspaces
            .into_iter()
            .find(|b| b.name == workspace_name)
            .ok_or_else(|| anyhow::anyhow!("Workspace '{}' not found", workspace_name))?;

        let path = format!(
            "projects/{}/workspaces/{}",
            self.project_id, workspace.database_name
        );
        let _: serde_json::Value = self
            .make_request(reqwest::Method::DELETE, &path, None::<&()>)
            .await?;

        Ok(())
    }

    async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let path = format!("projects/{}/workspaces", self.project_id);
        let response: ListBranchesResponse = self
            .make_request(reqwest::Method::GET, &path, None::<&()>)
            .await?;

        let workspaces = response
            .workspaces
            .into_iter()
            .map(|workspace| WorkspaceInfo {
                name: workspace.name,
                created_at: Some(workspace.created_at),
                parent_workspace: workspace.parent_id,
                database_name: workspace.id,
                state: Some("running".to_string()),
            })
            .collect();

        Ok(workspaces)
    }

    async fn workspace_exists(&self, workspace_name: &str) -> Result<bool> {
        let workspaces = self.list_workspaces().await?;
        Ok(workspaces.iter().any(|b| b.name == workspace_name))
    }

    async fn switch_to_branch(&self, workspace_name: &str) -> Result<WorkspaceInfo> {
        let workspaces = self.list_workspaces().await?;
        workspaces
            .into_iter()
            .find(|b| b.name == workspace_name)
            .ok_or_else(|| anyhow::anyhow!("Workspace '{}' does not exist", workspace_name))
    }

    async fn get_connection_info(&self, workspace_name: &str) -> Result<ConnectionInfo> {
        let endpoint = self.get_workspace_endpoint(workspace_name).await?;

        let connection_string = if let Some(ref password) = endpoint.database_password {
            format!(
                "postgresql://{}:{}@{}/{}",
                endpoint.database_user, password, endpoint.database_host, endpoint.database_name
            )
        } else {
            format!(
                "postgresql://{}@{}/{}",
                endpoint.database_user, endpoint.database_host, endpoint.database_name
            )
        };

        Ok(ConnectionInfo {
            host: endpoint.database_host,
            port: 5432,
            database: endpoint.database_name,
            user: endpoint.database_user,
            password: endpoint.database_password,
            connection_string: Some(connection_string),
        })
    }

    async fn test_connection(&self) -> Result<()> {
        let _ = self.list_workspaces().await?;
        Ok(())
    }

    async fn doctor(&self) -> Result<DoctorReport> {
        let check = match self.test_connection().await {
            Ok(_) => DoctorCheck {
                name: "Neon API".to_string(),
                available: true,
                detail: "Connected to Neon API".to_string(),
            },
            Err(e) => DoctorCheck {
                name: "Neon API".to_string(),
                available: false,
                detail: format!("Failed: {}", e),
            },
        };
        Ok(DoctorReport {
            checks: vec![check],
        })
    }

    fn provider_name(&self) -> &'static str {
        "Neon"
    }

    fn supports_template_from_time(&self) -> bool {
        true
    }
}
