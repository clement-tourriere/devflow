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
    pub projects: Vec<ProjectEntry>,
    #[serde(default)]
    pub proxy_auto_start: bool,
    #[serde(default)]
    pub proxy_config: Option<devflow_proxy::ProxyConfig>,
    #[serde(default = "default_terminal_renderer")]
    pub terminal_renderer: String,
    #[serde(default = "default_terminal_font_size")]
    pub terminal_font_size: u16,
    /// Feature flag: enable smart merge features (readiness checks, rebase,
    /// merge trains, cascade notifications). Default: `false`.
    #[serde(default)]
    pub smart_merge: bool,
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
            smart_merge: false,
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
    pub proxy: RwLock<Option<Arc<devflow_proxy::ProxyHandle>>>,
    pub proxy_config: Arc<RwLock<devflow_proxy::ProxyConfig>>,
    pub tray: std::sync::Mutex<Option<tauri::tray::TrayIcon>>,
    pub terminals: devflow_terminal::TerminalManager,
}

impl AppState {
    pub fn new() -> Self {
        let settings = AppSettings::load();
        let proxy_config = settings.proxy_config.clone().unwrap_or_default();
        Self {
            settings: Arc::new(RwLock::new(settings)),
            proxy: RwLock::new(None),
            proxy_config: Arc::new(RwLock::new(proxy_config)),
            tray: std::sync::Mutex::new(None),
            terminals: devflow_terminal::TerminalManager::new(),
        }
    }
}
