use super::super::{ConnectionInfo, DoctorCheck, DoctorReport, ServiceProvider, WorkspaceInfo};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};

const DEFAULT_BASE_URL: &str = "https://api.xata.tech";

#[derive(Debug, Clone)]
pub struct XataProvider {
    client: Client,
    api_key: String,
    base_url: String,
    organization_id: String,
    project_id: String,
}

#[derive(Debug, Deserialize)]
struct XataBranch {
    id: String,
    name: String,
    #[serde(rename = "createdAt")]
    created_at: Option<DateTime<Utc>>,
    #[serde(rename = "parentID")]
    #[allow(dead_code)]
    parent_id: Option<String>,
    #[allow(dead_code)]
    region: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListBranchesResponse {
    workspaces: Vec<XataBranch>,
}

#[derive(Debug, Serialize)]
struct CreateBranchRequest {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "parentID")]
    parent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BranchCredentials {
    username: String,
    password: String,
    host: Option<String>,
    port: Option<u16>,
    database: Option<String>,
}

impl XataProvider {
    pub fn new(
        api_key: String,
        organization_id: String,
        project_id: String,
        base_url: Option<String>,
    ) -> Result<Self> {
        let client = Client::new();

        Ok(Self {
            client,
            api_key,
            base_url: base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            organization_id,
            project_id,
        })
    }

    fn branches_url(&self) -> String {
        format!(
            "{}/organizations/{}/projects/{}/workspaces",
            self.base_url, self.organization_id, self.project_id
        )
    }

    fn branch_url(&self, branch_id: &str) -> String {
        format!("{}/{}", self.branches_url(), branch_id)
    }

    async fn api_request<T: for<'de> Deserialize<'de>>(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<&impl Serialize>,
    ) -> Result<T> {
        let mut request = self
            .client
            .request(method, url)
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
                "Xata API request failed with status {}: {}",
                status,
                error_text
            );
        }

        response
            .json()
            .await
            .with_context(|| "Failed to parse JSON response from Xata API")
    }

    async fn api_request_no_body(&self, method: reqwest::Method, url: &str) -> Result<()> {
        let request = self
            .client
            .request(method, url)
            .header("Authorization", format!("Bearer {}", self.api_key));

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
                "Xata API request failed with status {}: {}",
                status,
                error_text
            );
        }

        Ok(())
    }

    async fn fetch_branches(&self) -> Result<Vec<XataBranch>> {
        let response: ListBranchesResponse = self
            .api_request(reqwest::Method::GET, &self.branches_url(), None::<&()>)
            .await?;
        Ok(response.workspaces)
    }

    async fn find_branch_by_name(&self, workspace_name: &str) -> Result<Option<XataBranch>> {
        let normalized = Self::normalize_workspace_name(workspace_name);
        let workspaces = self.fetch_branches().await?;
        Ok(workspaces.into_iter().find(|b| b.name == normalized))
    }

    fn normalize_workspace_name(workspace_name: &str) -> String {
        workspace_name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect::<String>()
            .trim_matches('-')
            .to_string()
    }
}

#[async_trait]
impl ServiceProvider for XataProvider {
    async fn create_workspace(
        &self,
        workspace_name: &str,
        from_workspace: Option<&str>,
    ) -> Result<WorkspaceInfo> {
        let normalized_name = Self::normalize_workspace_name(workspace_name);

        // Resolve parent workspace ID if a from_workspace name is given
        let parent_id = if let Some(from_name) = from_workspace {
            let parent = self
                .find_branch_by_name(from_name)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Parent workspace '{}' not found", from_name))?;
            Some(parent.id)
        } else {
            None
        };

        let request = CreateBranchRequest {
            name: normalized_name.clone(),
            parent_id,
        };

        let workspace: XataBranch = self
            .api_request(reqwest::Method::POST, &self.branches_url(), Some(&request))
            .await?;

        Ok(WorkspaceInfo {
            name: workspace.name,
            created_at: workspace.created_at,
            parent_workspace: from_workspace.map(|s| s.to_string()),
            database_name: self.project_id.clone(),
            state: Some("running".to_string()),
        })
    }

    async fn delete_workspace(&self, workspace_name: &str) -> Result<()> {
        let workspace = self
            .find_branch_by_name(workspace_name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Workspace '{}' not found", workspace_name))?;

        self.api_request_no_body(reqwest::Method::DELETE, &self.branch_url(&workspace.id))
            .await
    }

    async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let workspaces = self.fetch_branches().await?;

        Ok(workspaces
            .into_iter()
            .map(|workspace| WorkspaceInfo {
                name: workspace.name,
                created_at: workspace.created_at,
                parent_workspace: None,
                database_name: self.project_id.clone(),
                state: Some("running".to_string()),
            })
            .collect())
    }

    async fn workspace_exists(&self, workspace_name: &str) -> Result<bool> {
        Ok(self.find_branch_by_name(workspace_name).await?.is_some())
    }

    async fn switch_to_branch(&self, workspace_name: &str) -> Result<WorkspaceInfo> {
        let normalized_name = Self::normalize_workspace_name(workspace_name);
        let workspaces = self.list_workspaces().await?;
        workspaces
            .into_iter()
            .find(|b| b.name == normalized_name)
            .ok_or_else(|| anyhow::anyhow!("Workspace '{}' does not exist", workspace_name))
    }

    async fn get_connection_info(&self, workspace_name: &str) -> Result<ConnectionInfo> {
        let workspace = self
            .find_branch_by_name(workspace_name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Workspace '{}' not found", workspace_name))?;

        let creds_url = format!("{}/credentials", self.branch_url(&workspace.id));
        let creds: BranchCredentials = self
            .api_request(reqwest::Method::GET, &creds_url, None::<&()>)
            .await?;

        let host = creds.host.unwrap_or_else(|| "localhost".to_string());
        let port = creds.port.unwrap_or(5432);
        let database = creds.database.unwrap_or_else(|| workspace.name.clone());

        let connection_string = format!(
            "postgresql://{}:{}@{}:{}/{}",
            creds.username, creds.password, host, port, database
        );

        Ok(ConnectionInfo {
            host,
            port,
            database,
            user: creds.username,
            password: Some(creds.password),
            connection_string: Some(connection_string),
        })
    }

    async fn test_connection(&self) -> Result<()> {
        let _ = self.fetch_branches().await?;
        Ok(())
    }

    async fn doctor(&self) -> Result<DoctorReport> {
        let check = match self.test_connection().await {
            Ok(_) => DoctorCheck {
                name: "Xata API".to_string(),
                available: true,
                detail: "Connected to Xata API".to_string(),
            },
            Err(e) => DoctorCheck {
                name: "Xata API".to_string(),
                available: false,
                detail: format!("Failed: {}", e),
            },
        };
        Ok(DoctorReport {
            checks: vec![check],
        })
    }

    fn provider_name(&self) -> &'static str {
        "Xata"
    }

    fn max_workspace_name_length(&self) -> usize {
        255
    }
}
