use std::sync::Arc;
use tokio::sync::RwLock;

/// Registered project entry (path to a devflow-configured repo).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProjectEntry {
    pub path: String,
    pub name: String,
}

/// Application settings.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AppSettings {
    #[serde(default, skip_serializing)]
    pub projects: Vec<ProjectEntry>,
    #[serde(default)]
    pub proxy_auto_start: bool,
    #[serde(default)]
    #[serde(skip_serializing)]
    pub proxy_config: Option<serde_json::Value>,
    #[serde(default = "default_terminal_renderer")]
    pub terminal_renderer: String,
    #[serde(default = "default_terminal_font_size")]
    pub terminal_font_size: u16,
}

fn default_terminal_renderer() -> String {
    "auto".to_string()
}

fn default_terminal_font_size() -> u16 {
    14
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            projects: Vec::new(),
            proxy_auto_start: false,
            proxy_config: None,
            terminal_renderer: default_terminal_renderer(),
            terminal_font_size: default_terminal_font_size(),
        }
    }
}

impl AppSettings {
    pub fn load() -> Self {
        let path = settings_path();
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        } else {
            Self::default()
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = settings_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

fn settings_path() -> std::path::PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("devflow")
        .join("app-settings.json")
}

/// Application state managed by Tauri.
pub struct AppState {
    pub settings: Arc<RwLock<AppSettings>>,
    pub tray: std::sync::Mutex<Option<tauri::tray::TrayIcon>>,
    pub terminals: devflow_terminal::TerminalManager,
}

impl AppState {
    pub fn new() -> Self {
        let mut settings = AppSettings::load();
        if let Ok(mut registry) = devflow_core::state::LocalStateManager::new() {
            let migrated = settings.projects.iter().all(|p| {
                registry
                    .register_project(std::path::Path::new(&p.path), Some(&p.name))
                    .is_ok()
            });
            if migrated && !settings.projects.is_empty() {
                settings.projects.clear();
                let _ = settings.save();
            }
        }
        if settings.proxy_config.is_some() {
            log::warn!("Proxy configuration now comes from the global devflow config; the desktop no longer owns a separate runtime");
        }
        Self {
            settings: Arc::new(RwLock::new(settings)),
            tray: std::sync::Mutex::new(None),
            terminals: devflow_terminal::TerminalManager::new(),
        }
    }
}

pub fn registered_projects() -> anyhow::Result<Vec<ProjectEntry>> {
    let mut projects = devflow_core::state::LocalStateManager::new()?
        .list_all_projects()
        .into_iter()
        .filter(|(_, p)| p.control.visible)
        .map(|(path, p)| ProjectEntry {
            name: p.control.name.unwrap_or_else(|| {
                std::path::Path::new(&path)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            }),
            path,
        })
        .collect::<Vec<_>>();
    projects.sort_by(|a, b| a.name.cmp(&b.name).then(a.path.cmp(&b.path)));
    Ok(projects)
}
