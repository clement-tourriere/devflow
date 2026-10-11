//! `devflow.toml` schema, loading with the machine-local overlay, raw-text
//! editing with concurrent-edit detection, and 0.x detection.
use crate::error::NotInitialized;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Project configuration file names, in lookup order.
pub const CONFIG_FILES: [&str; 2] = ["devflow.toml", ".devflow.toml"];
/// Machine-local overlay, merged on top of the project file. Never committed.
pub const LOCAL_CONFIG_FILE: &str = "devflow.local.toml";
/// Files that only devflow 0.x used.
const LEGACY_FILES: [&str; 4] = [
    ".devflow.yml",
    ".devflow.yaml",
    ".devflow.local.yml",
    ".devflow.local.yaml",
];
pub const MIGRATION_GUIDE: &str =
    "https://github.com/clement-tourriere/devflow/blob/main/docs/MIGRATING.md";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub project: ProjectSection,
    #[serde(default)]
    pub worktree: WorktreeSection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postgres: Option<PostgresSection>,
    #[serde(default)]
    pub hooks: HooksSection,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProjectSection {
    pub name: String,
    /// Stable across machines; written by `devflow init` and committed.
    pub id: uuid::Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct WorktreeSection {
    /// Parent directory for new worktrees, relative to the main checkout.
    /// `{project}` and `{repo}` expand to the project name and the main
    /// checkout's directory name.
    pub dir: String,
    /// Paths cloned from the source worktree into new worktrees when present.
    pub copy: Vec<String>,
}

impl Default for WorktreeSection {
    fn default() -> Self {
        Self {
            dir: "../{repo}.worktrees".into(),
            copy: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PostgresMode {
    /// devflow runs a PostgreSQL container with `docker compose`.
    #[default]
    Managed,
    /// An existing PostgreSQL ≥ 18 server reached through `admin_url`.
    External,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BusySourcePolicy {
    /// Terminate sessions on the source database, then clone (bounded retries).
    #[default]
    Disconnect,
    /// Refuse to clone a source database that has sessions.
    Fail,
}

/// Whether cloned databases share blocks with their source.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CowCapability {
    /// Block sharing was verified on the server's data directory.
    Verified,
    /// Clones are full copies.
    Copy,
    /// Not verifiable from here (external servers).
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct PostgresSection {
    pub mode: PostgresMode,
    /// Managed mode: image to run; must be PostgreSQL ≥ 18.
    pub image: String,
    /// External mode: admin (provisioner) connection URL. `DEVFLOW_PG_ADMIN_URL` overrides it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admin_url: Option<String>,
    /// Host written into workspace URLs when it differs from the admin endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_host: Option<String>,
    /// Port written into workspace URLs when it differs from the admin endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_port: Option<u16>,
    /// Managed mode: address the container port is published on.
    pub bind: String,
    /// Managed mode: fixed host port; 0 lets Docker pick one.
    pub port: u16,
    /// Display name of the default workspace's database branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// Default: `disconnect` for a managed server bound to loopback, `fail` otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_busy_source: Option<BusySourcePolicy>,
    /// Refuse to create branches unless CoW is verified.
    pub require_cow: bool,
    /// External mode: operator declaration after checking the server's filesystem.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cow: Option<CowCapability>,
}

impl Default for PostgresSection {
    fn default() -> Self {
        Self {
            mode: PostgresMode::Managed,
            image: "postgres:18".into(),
            admin_url: None,
            app_host: None,
            app_port: None,
            bind: "127.0.0.1".into(),
            port: 0,
            database: None,
            on_busy_source: None,
            require_cow: false,
            cow: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct HooksSection {
    /// Run once in a new (or adopted) worktree, after payloads, database and env.
    pub setup: Vec<String>,
    /// Run on every `devflow switch` into an existing workspace.
    pub switch: Vec<String>,
    /// Run before removal; a failure blocks removal unless forced.
    pub teardown: Vec<String>,
    /// Per-command timeout (humantime, e.g. "15m").
    pub timeout: String,
}

impl Default for HooksSection {
    fn default() -> Self {
        Self {
            setup: Vec::new(),
            switch: Vec::new(),
            teardown: Vec::new(),
            timeout: "15m".into(),
        }
    }
}

impl PostgresSection {
    /// The effective policy for cloning a source database that has sessions.
    pub fn busy_policy(&self) -> BusySourcePolicy {
        if let Some(policy) = self.on_busy_source {
            return policy;
        }
        let loopback = self
            .bind
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
            || self.bind == "localhost";
        if self.mode == PostgresMode::Managed && loopback {
            BusySourcePolicy::Disconnect
        } else {
            BusySourcePolicy::Fail
        }
    }
}

impl HooksSection {
    pub fn is_empty(&self) -> bool {
        self.setup.is_empty() && self.switch.is_empty() && self.teardown.is_empty()
    }

    pub fn timeout(&self) -> Result<Duration> {
        humantime::parse_duration(&self.timeout)
            .with_context(|| format!("invalid hooks.timeout '{}'", self.timeout))
    }
}

impl Config {
    /// The project file in `root`, if any.
    pub fn find_file(root: &Path) -> Option<PathBuf> {
        CONFIG_FILES
            .iter()
            .map(|name| root.join(name))
            .find(|path| path.is_file())
    }

    /// Load the project file plus `devflow.local.toml` from `root`. Without a
    /// project file the error is [`NotInitialized`] (or the 0.x guidance when
    /// a 0.x file is found).
    pub fn load(root: &Path) -> Result<(Config, PathBuf)> {
        let path = match Self::find_file(root) {
            Some(path) => path,
            None => {
                if let Some(legacy) = legacy_file(root) {
                    bail!(legacy_message(&legacy));
                }
                return Err(not_initialized(root));
            }
        };
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let mut value = parse_table(&text, &path)?;
        let local = root.join(LOCAL_CONFIG_FILE);
        if local.is_file() {
            let text = std::fs::read_to_string(&local)
                .with_context(|| format!("cannot read {}", local.display()))?;
            merge(&mut value, parse_table(&text, &local)?);
        }
        let config = Self::from_value(value, &path)?;
        Ok((config, path))
    }

    /// Parse a single document (no overlay); used to validate editor text.
    pub fn parse(text: &str) -> Result<Config> {
        let path = Path::new("devflow.toml");
        Self::from_value(parse_table(text, path)?, path)
    }

    fn from_value(value: toml::Table, path: &Path) -> Result<Config> {
        if looks_legacy(&value) {
            bail!(legacy_message(path));
        }
        let config: Config = toml::Value::Table(value)
            .try_into()
            .with_context(|| format!("invalid configuration in {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.project.name.trim().is_empty(),
            "project.name must not be empty"
        );
        anyhow::ensure!(!self.project.id.is_nil(), "project.id must be set");
        for entry in &self.worktree.copy {
            validate_relative(entry)
                .with_context(|| format!("invalid worktree.copy entry '{entry}'"))?;
        }
        self.hooks.timeout()?;
        if let Some(pg) = &self.postgres
            && pg.mode == PostgresMode::Managed
        {
            anyhow::ensure!(!pg.image.trim().is_empty(), "postgres.image must be set");
        }
        Ok(())
    }

    /// The text `devflow init` writes.
    pub fn template(name: &str, id: uuid::Uuid, postgres: Option<PostgresMode>) -> String {
        let mut text = format!(
            r#"[project]
name = "{name}"
id = "{id}"

[worktree]
# Parent directory for new worktrees ({{repo}} = this checkout's directory name).
dir = "../{{repo}}.worktrees"
# Ignored paths cloned (copy-on-write when possible) from the source worktree.
copy = [".env"]
"#
        );
        match postgres {
            Some(PostgresMode::Managed) => text.push_str(
                r#"
[postgres]
mode = "managed"
image = "postgres:18"
"#,
            ),
            Some(PostgresMode::External) => text.push_str(
                r#"
[postgres]
mode = "external"
# Put the admin URL in devflow.local.toml or DEVFLOW_PG_ADMIN_URL, not here.
"#,
            ),
            None => {}
        }
        text.push_str(
            r#"
[hooks]
# Shell commands run in a new worktree with DATABASE_URL and DEVFLOW_* set.
setup = []
teardown = []
"#,
        );
        text
    }
}

/// Raw configuration text for editors: preserves formatting and comments,
/// validates before saving, and refuses to overwrite a newer edit.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigDocument {
    pub path: PathBuf,
    pub content: String,
}

impl ConfigDocument {
    pub fn open(root: &Path) -> Result<Self> {
        let path = Config::find_file(root).ok_or_else(|| not_initialized(root))?;
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        Ok(Self { path, content })
    }

    /// Validate `text` as a project file (without the local overlay).
    pub fn validate(text: &str) -> Result<Config> {
        Config::parse(text)
    }

    /// Save `text` if the file still holds the content this document was
    /// opened with. Returns the reopened document.
    pub fn save(&self, text: &str) -> Result<Self> {
        Self::validate(text)?;
        let current = std::fs::read_to_string(&self.path)
            .with_context(|| format!("cannot read {}", self.path.display()))?;
        if current != self.content {
            return Err(crate::error::ConfigChanged {
                path: self.path.clone(),
            }
            .into());
        }
        crate::fsutil::write_atomic(&self.path, text.as_bytes(), false)?;
        Ok(Self {
            path: self.path.clone(),
            content: text.to_string(),
        })
    }
}

fn not_initialized(root: &Path) -> anyhow::Error {
    anyhow::Error::new(NotInitialized {
        root: root.to_path_buf(),
    })
}

fn parse_table(text: &str, path: &Path) -> Result<toml::Table> {
    text.parse::<toml::Table>()
        .with_context(|| format!("invalid TOML in {}", path.display()))
}

/// Merge `overlay` into `base`: tables merge recursively, other values replace.
fn merge(base: &mut toml::Table, overlay: toml::Table) {
    for (key, value) in overlay {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(base)), toml::Value::Table(overlay)) => merge(base, overlay),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

fn legacy_file(root: &Path) -> Option<PathBuf> {
    LEGACY_FILES
        .iter()
        .map(|name| root.join(name))
        .find(|path| path.exists())
}

/// 0.x TOML used these top-level keys, or phase-keyed hook maps.
fn looks_legacy(value: &toml::Table) -> bool {
    const KEYS: [&str; 7] = [
        "services",
        "processes",
        "agent",
        "default_workspace",
        "git",
        "commit",
        "proxy",
    ];
    if KEYS.iter().any(|key| value.contains_key(*key)) {
        return true;
    }
    value
        .get("hooks")
        .and_then(toml::Value::as_table)
        .is_some_and(|hooks| hooks.keys().any(|k| k.contains('-')))
}

fn legacy_message(path: &Path) -> String {
    format!(
        "{} is a devflow 0.x configuration. devflow 1.0 uses a new format and never adopts 0.x \
         containers or data. See {MIGRATION_GUIDE} (keep devflow 0.8 to export and remove old resources).",
        path.display()
    )
}

/// Reject absolute paths, `..` and empty components.
pub(crate) fn validate_relative(entry: &str) -> Result<()> {
    let path = Path::new(entry);
    anyhow::ensure!(!entry.trim().is_empty(), "empty path");
    anyhow::ensure!(path.is_relative(), "must be relative");
    for component in path.components() {
        match component {
            std::path::Component::Normal(_) | std::path::Component::CurDir => {}
            _ => bail!("must stay inside the worktree"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) {
        std::fs::write(dir.join(name), text).unwrap();
    }

    #[test]
    fn template_round_trips_and_local_overlay_merges() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4();
        write(
            dir.path(),
            "devflow.toml",
            &Config::template("shop", id, Some(PostgresMode::External)),
        );
        write(
            dir.path(),
            LOCAL_CONFIG_FILE,
            "[postgres]\nadmin_url = \"postgres://p@db/postgres\"\n",
        );
        let (config, path) = Config::load(dir.path()).unwrap();
        assert_eq!(path, dir.path().join("devflow.toml"));
        assert_eq!(config.project.id, id);
        assert_eq!(config.worktree.copy, vec![".env".to_string()]);
        let pg = config.postgres.unwrap();
        assert_eq!(pg.mode, PostgresMode::External);
        assert_eq!(pg.admin_url.as_deref(), Some("postgres://p@db/postgres"));
        assert_eq!(pg.busy_policy(), BusySourcePolicy::Fail);
        let managed = PostgresSection::default();
        assert_eq!(managed.busy_policy(), BusySourcePolicy::Disconnect);
        let remote = PostgresSection {
            bind: "100.64.0.2".into(),
            ..PostgresSection::default()
        };
        assert_eq!(remote.busy_policy(), BusySourcePolicy::Fail);
    }

    #[test]
    fn legacy_configs_are_refused_with_guidance() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".devflow.yml", "services: []\n");
        let error = Config::load(dir.path()).unwrap_err().to_string();
        assert!(error.contains("0.x"), "{error}");

        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            ".devflow.toml",
            "[hooks.post-create]\nmigrate = \"make migrate\"\n",
        );
        let error = Config::load(dir.path()).unwrap_err().to_string();
        assert!(error.contains(MIGRATION_GUIDE), "{error}");
    }

    #[test]
    fn unknown_keys_and_escaping_copy_paths_are_rejected() {
        let base = Config::template("shop", uuid::Uuid::new_v4(), None);
        assert!(Config::parse(&format!("{base}\n[extra]\nx = 1\n")).is_err());
        for bad in ["../secrets", "/etc/passwd", "a/../../b"] {
            let text = base.replace("copy = [\".env\"]", &format!("copy = [\"{bad}\"]"));
            assert!(Config::parse(&text).is_err(), "{bad} must be rejected");
        }
        assert!(Config::parse(&base).is_ok());
    }

    #[test]
    fn documents_refuse_stale_saves() {
        let dir = tempfile::tempdir().unwrap();
        let text = Config::template("shop", uuid::Uuid::new_v4(), None);
        write(dir.path(), "devflow.toml", &text);
        let doc = ConfigDocument::open(dir.path()).unwrap();
        let edited = format!("# comment\n{text}");
        let reopened = doc.save(&edited).unwrap();
        assert_eq!(reopened.content, edited);
        assert!(
            doc.save(&text).is_err(),
            "stale document must not overwrite"
        );
        assert!(reopened.save("not = [valid").is_err());
    }

    #[test]
    fn a_missing_project_file_is_a_typed_error_but_legacy_files_keep_their_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let error = Config::load(dir.path()).unwrap_err();
        let typed = error.downcast_ref::<NotInitialized>().expect("typed");
        assert_eq!(typed.root, dir.path());
        assert!(error.to_string().contains("devflow init"), "{error}");
        let error = ConfigDocument::open(dir.path()).unwrap_err();
        assert!(error.downcast_ref::<NotInitialized>().is_some());

        write(dir.path(), ".devflow.yml", "services: []\n");
        let error = Config::load(dir.path()).unwrap_err();
        assert!(error.downcast_ref::<NotInitialized>().is_none(), "{error}");
        assert!(error.to_string().contains("0.x"));
    }
}
