use super::super::{ConnectionInfo, DoctorCheck, DoctorReport, ServiceProvider, WorkspaceInfo};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct NeonProvider {
    client: Client,
    api_key: String,
    project_id: String,
    base_url: String,
}

impl std::fmt::Debug for NeonProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never expose the API key via {:?} — it would leak into logs.
        f.debug_struct("NeonProvider")
            .field("client", &self.client)
            .field("api_key", &"<redacted>")
            .field("project_id", &self.project_id)
            .field("base_url", &self.base_url)
            .finish()
    }
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
    branches: Vec<NeonBranch>,
}

#[derive(Debug, Deserialize)]
struct CreateBranchResponse {
    branch: NeonBranch,
}

#[derive(Debug, Deserialize)]
struct NeonDatabase {
    name: String,
    owner_name: String,
}
#[derive(Debug, Deserialize)]
struct DatabasesResponse {
    databases: Vec<NeonDatabase>,
}
#[derive(Debug, Deserialize)]
struct ConnectionUriResponse {
    uri: String,
}

impl NeonProvider {
    pub fn new(api_key: String, project_id: String, base_url: Option<String>) -> Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()?;
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

    async fn find_branch(&self, name: &str) -> Result<WorkspaceInfo> {
        self.list_workspaces()
            .await?
            .into_iter()
            .find(|b| b.name == name)
            .with_context(|| format!("Neon branch '{name}' does not exist"))
    }
}

#[async_trait]
impl ServiceProvider for NeonProvider {
    async fn create_workspace(
        &self,
        workspace_name: &str,
        from_workspace: Option<&str>,
    ) -> Result<WorkspaceInfo> {
        let parent_id = if let Some(parent) = from_workspace {
            Some(self.find_branch(parent).await?.database_name)
        } else {
            None
        };
        let request = serde_json::json!({
            "branch": CreateBranchRequest { name: workspace_name.into(), parent_id },
            "endpoints": [{"type": "read_write"}]
        });

        let path = format!("projects/{}/branches", self.project_id);
        let response: CreateBranchResponse = self
            .make_request(reqwest::Method::POST, &path, Some(&request))
            .await?;

        Ok(WorkspaceInfo {
            name: response.branch.name,
            created_at: Some(response.branch.created_at),
            parent_workspace: from_workspace.map(String::from),
            database_name: response.branch.id,
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
            "projects/{}/branches/{}",
            self.project_id, workspace.database_name
        );
        let _: serde_json::Value = self
            .make_request(reqwest::Method::DELETE, &path, None::<&()>)
            .await?;

        Ok(())
    }

    async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let path = format!("projects/{}/branches", self.project_id);
        let response: ListBranchesResponse = self
            .make_request(reqwest::Method::GET, &path, None::<&()>)
            .await?;

        let names: std::collections::HashMap<_, _> = response
            .branches
            .iter()
            .map(|b| (b.id.clone(), b.name.clone()))
            .collect();
        let workspaces = response
            .branches
            .into_iter()
            .map(|workspace| WorkspaceInfo {
                name: workspace.name,
                created_at: Some(workspace.created_at),
                parent_workspace: workspace
                    .parent_id
                    .map(|id| names.get(&id).cloned().unwrap_or(id)),
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
        let branch = self.find_branch(workspace_name).await?;
        let path = format!(
            "projects/{}/branches/{}/databases",
            self.project_id, branch.database_name
        );
        let response: DatabasesResponse = self
            .make_request(reqwest::Method::GET, &path, None::<&()>)
            .await?;
        let database = if response.databases.len() == 1 { response.databases.into_iter().next() }
            else { response.databases.into_iter().find(|db| db.name == "neondb") }
            .context("Neon branch has no unambiguous default database (expected a single database or neondb)")?;
        let mut url = reqwest::Url::parse(&format!(
            "{}/projects/{}/connection_uri",
            self.base_url, self.project_id
        ))?;
        url.query_pairs_mut()
            .append_pair("branch_id", &branch.database_name)
            .append_pair("database_name", &database.name)
            .append_pair("role_name", &database.owner_name);
        let response: ConnectionUriResponse = self
            .client
            .get(url)
            .bearer_auth(&self.api_key)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let uri = reqwest::Url::parse(&response.uri)
            .context("Neon returned an invalid connection URI")?;
        Ok(ConnectionInfo {
            host: uri
                .host_str()
                .context("Neon connection URI has no host")?
                .into(),
            port: uri.port().unwrap_or(5432),
            database: database.name,
            user: database.owner_name,
            password: uri.password().map(|p| {
                percent_encoding::percent_decode_str(p)
                    .decode_utf8_lossy()
                    .into_owned()
            }),
            connection_string: Some(response.uri),
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

    fn data_strategy(&self) -> &'static str {
        "remote_clone"
    }

    fn provider_name(&self) -> &'static str {
        "Neon"
    }
}
