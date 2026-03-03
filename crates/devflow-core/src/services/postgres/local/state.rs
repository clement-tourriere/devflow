use std::path::Path;

use anyhow::Context;
use rusqlite::Connection;

use super::model::{now_epoch_millis, BranchState, Project, StorageDriver, Workspace};

#[derive(Debug)]
pub struct NewProject {
    pub name: String,
    pub image: String,
    pub storage_driver: StorageDriver,
    pub storage_config: Option<String>,
    pub project_path: Option<String>,
}

#[derive(Debug)]
pub struct NewBranch {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub parent_workspace_id: Option<String>,
    pub state: BranchState,
    pub data_dir: String,
    pub container_name: String,
    pub port: u16,
    pub storage_metadata: Option<String>,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open SQLite db at {}", path.display()))?;

        let store = Self { conn };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> anyhow::Result<()> {
        self.conn
            .execute_batch(
                r#"
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS projects (
              id TEXT PRIMARY KEY,
              name TEXT NOT NULL UNIQUE,
              image TEXT NOT NULL,
              storage_driver TEXT NOT NULL DEFAULT 'copy',
              storage_config TEXT NULL,
              created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS workspaces (
              id TEXT PRIMARY KEY,
              project_id TEXT NOT NULL,
              name TEXT NOT NULL,
              parent_workspace_id TEXT NULL,
              state TEXT NOT NULL,
              data_dir TEXT NOT NULL,
              container_name TEXT NOT NULL,
              port INTEGER NOT NULL,
              storage_metadata TEXT NULL,
              created_at INTEGER NOT NULL,
              UNIQUE(project_id, name),
              FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE CASCADE,
              FOREIGN KEY(parent_workspace_id) REFERENCES workspaces(id) ON DELETE SET NULL
            );
            "#,
            )
            .context("failed to apply SQLite schema")?;

        ensure_column(
            &self.conn,
            "projects",
            "storage_driver",
            "TEXT NOT NULL DEFAULT 'copy'",
        )?;
        ensure_column(&self.conn, "projects", "storage_config", "TEXT NULL")?;
        ensure_column(&self.conn, "projects", "project_path", "TEXT NULL")?;
        ensure_column(&self.conn, "workspaces", "storage_metadata", "TEXT NULL")?;

        Ok(())
    }

    #[allow(dead_code)]
    pub fn list_projects(&self) -> anyhow::Result<Vec<Project>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, image, storage_driver, storage_config, project_path, created_at FROM projects ORDER BY created_at DESC"
        )?;

        let rows = stmt.query_map([], |row| {
            let driver_text: String = row.get(3)?;
            let storage_driver =
                StorageDriver::from_str(&driver_text).unwrap_or(StorageDriver::Copy);
            Ok(Project {
                id: row.get(0)?,
                name: row.get(1)?,
                image: row.get(2)?,
                storage_driver,
                storage_config: row.get(4)?,
                project_path: row.get(5)?,
                created_at: row.get(6)?,
            })
        })?;

        rows.collect::<Result<Vec<_>, _>>()
            .context("failed to list projects")
    }

    pub fn get_project_by_name(&self, name: &str) -> anyhow::Result<Option<Project>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, image, storage_driver, storage_config, project_path, created_at FROM projects WHERE name = ?1"
        )?;

        let mut rows = stmt.query([name])?;
        if let Some(row) = rows.next()? {
            let driver_text: String = row.get(3)?;
            let storage_driver =
                StorageDriver::from_str(&driver_text).unwrap_or(StorageDriver::Copy);
            return Ok(Some(Project {
                id: row.get(0)?,
                name: row.get(1)?,
                image: row.get(2)?,
                storage_driver,
                storage_config: row.get(4)?,
                project_path: row.get(5)?,
                created_at: row.get(6)?,
            }));
        }

        Ok(None)
    }

    pub fn create_project(&self, input: NewProject) -> anyhow::Result<Project> {
        let created_at = now_epoch_millis();
        let id = uuid::Uuid::new_v4().to_string();

        self.conn.execute(
            "INSERT INTO projects(id, name, image, storage_driver, storage_config, project_path, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![id, input.name, input.image, input.storage_driver.as_str(), input.storage_config, input.project_path, created_at],
        ).context("failed to insert project")?;

        Ok(Project {
            id,
            name: input.name,
            image: input.image,
            storage_driver: input.storage_driver,
            storage_config: input.storage_config,
            project_path: input.project_path,
            created_at,
        })
    }

    pub fn next_port(&self) -> anyhow::Result<u16> {
        let max_port: Option<u16> = self
            .conn
            .query_row("SELECT MAX(port) FROM workspaces", [], |row| row.get(0))
            .context("failed to compute next workspace port")?;

        Ok(max_port.map(|v| v.saturating_add(1)).unwrap_or(55432))
    }

    pub fn list_workspaces(&self, project_id: &str) -> anyhow::Result<Vec<Workspace>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, project_id, name, parent_workspace_id, state, data_dir, container_name, port, storage_metadata, created_at
            FROM workspaces
            WHERE project_id = ?1
            ORDER BY created_at DESC
            "#,
        )?;

        let rows = stmt.query_map([project_id], map_branch_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("failed to list workspaces")
    }

    #[allow(dead_code)]
    pub fn list_all_branches(&self) -> anyhow::Result<Vec<Workspace>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, project_id, name, parent_workspace_id, state, data_dir, container_name, port, storage_metadata, created_at
            FROM workspaces
            ORDER BY created_at DESC
            "#,
        )?;

        let rows = stmt.query_map([], map_branch_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("failed to list all workspaces")
    }

    pub fn get_workspace_by_name(
        &self,
        project_id: &str,
        workspace_name: &str,
    ) -> anyhow::Result<Option<Workspace>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, project_id, name, parent_workspace_id, state, data_dir, container_name, port, storage_metadata, created_at
            FROM workspaces
            WHERE project_id = ?1 AND name = ?2
            "#,
        )?;

        let mut rows = stmt.query(rusqlite::params![project_id, workspace_name])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(map_branch_row(row)?));
        }

        Ok(None)
    }

    pub fn create_workspace(&self, input: NewBranch) -> anyhow::Result<Workspace> {
        let created_at = now_epoch_millis();

        self.conn.execute(
            r#"
            INSERT INTO workspaces(id, project_id, name, parent_workspace_id, state, data_dir, container_name, port, storage_metadata, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            "#,
            rusqlite::params![
                input.id, input.project_id, input.name, input.parent_workspace_id,
                input.state.as_str(), input.data_dir, input.container_name, input.port,
                input.storage_metadata, created_at,
            ],
        ).context("failed to insert workspace")?;

        Ok(Workspace {
            id: input.id,
            project_id: input.project_id,
            name: input.name,
            parent_workspace_id: input.parent_workspace_id,
            state: input.state,
            data_dir: input.data_dir,
            container_name: input.container_name,
            port: input.port,
            storage_metadata: input.storage_metadata,
            created_at,
        })
    }

    pub fn update_branch_state(&self, branch_id: &str, state: BranchState) -> anyhow::Result<()> {
        self.conn
            .execute(
                "UPDATE workspaces SET state = ?1 WHERE id = ?2",
                rusqlite::params![state.as_str(), branch_id],
            )
            .context("failed to update workspace state")?;
        Ok(())
    }

    pub fn update_branch_storage_metadata(
        &self,
        branch_id: &str,
        storage_metadata: Option<&str>,
    ) -> anyhow::Result<()> {
        self.conn
            .execute(
                "UPDATE workspaces SET storage_metadata = ?1 WHERE id = ?2",
                rusqlite::params![storage_metadata, branch_id],
            )
            .context("failed to update workspace storage metadata")?;
        Ok(())
    }

    pub fn delete_workspace(&self, branch_id: &str) -> anyhow::Result<()> {
        self.conn
            .execute("DELETE FROM workspaces WHERE id = ?1", [branch_id])
            .context("failed to delete workspace")?;
        Ok(())
    }

    pub fn delete_project(&self, project_id: &str) -> anyhow::Result<()> {
        // ON DELETE CASCADE auto-removes all workspace rows
        self.conn
            .execute("DELETE FROM projects WHERE id = ?1", [project_id])
            .context("failed to delete project")?;
        Ok(())
    }

    /// Backfill project_path for an existing project (used when path was not stored at creation time).
    pub fn update_project_path(&self, project_id: &str, project_path: &str) -> anyhow::Result<()> {
        self.conn
            .execute(
                "UPDATE projects SET project_path = ?1 WHERE id = ?2",
                rusqlite::params![project_path, project_id],
            )
            .context("failed to update project path")?;
        Ok(())
    }
}

fn map_branch_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Workspace> {
    let state_text: String = row.get(4)?;
    let state = BranchState::from_str(&state_text).unwrap_or(BranchState::Failed);

    Ok(Workspace {
        id: row.get(0)?,
        project_id: row.get(1)?,
        name: row.get(2)?,
        parent_workspace_id: row.get(3)?,
        state,
        data_dir: row.get(5)?,
        container_name: row.get(6)?,
        port: row.get(7)?,
        storage_metadata: row.get(8)?,
        created_at: row.get(9)?,
    })
}

fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    column_definition: &str,
) -> anyhow::Result<()> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&pragma)?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row?.eq_ignore_ascii_case(column) {
            return Ok(());
        }
    }

    let alter = format!("ALTER TABLE {table} ADD COLUMN {column} {column_definition}");
    conn.execute(&alter, [])?;
    Ok(())
}
