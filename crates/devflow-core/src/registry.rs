//! Machine-local list of known projects, for the GUI and `devflow list --all`.
//! Removing an entry never touches the project or its data.
use crate::project::Project;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub id: uuid::Uuid,
    pub name: String,
    /// Main checkout.
    pub root: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default, rename = "project")]
    projects: Vec<Entry>,
}

fn path() -> Result<PathBuf> {
    Ok(crate::paths::config_dir()?.join("projects.toml"))
}

fn read() -> Result<File> {
    let path = path()?;
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("invalid {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(File::default()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn write(file: &File) -> Result<()> {
    let text = toml::to_string_pretty(file)?;
    crate::fsutil::write_atomic(&path()?, text.as_bytes(), false)
}

pub fn list() -> Result<Vec<Entry>> {
    Ok(read()?.projects)
}

fn with_lock<T>(change: impl FnOnce() -> Result<T>) -> Result<T> {
    crate::fsutil::with_lock(&crate::paths::config_dir()?.join("projects.lock"), change)
}

/// Add or refresh a project (matched by id or main-checkout path).
pub fn add(project: &Project) -> Result<()> {
    with_lock(|| add_locked(project))
}

fn add_locked(project: &Project) -> Result<()> {
    let mut file = read()?;
    let entry = Entry {
        id: project.id(),
        name: project.name().to_string(),
        root: project.root.clone(),
    };
    if file.projects.contains(&entry) {
        return Ok(());
    }
    file.projects
        .retain(|e| e.id != entry.id && e.root != entry.root);
    file.projects.push(entry);
    file.projects.sort_by(|a, b| a.name.cmp(&b.name));
    write(&file)
}

pub fn remove(root: &Path) -> Result<bool> {
    with_lock(|| remove_locked(root))
}

fn remove_locked(root: &Path) -> Result<bool> {
    let mut file = read()?;
    let before = file.projects.len();
    file.projects.retain(|e| e.root != root);
    let removed = file.projects.len() != before;
    if removed {
        write(&file)?;
    }
    Ok(removed)
}
