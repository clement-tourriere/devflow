//! Machine-local list of known projects, for the GUI and `devflow list --all`.
//! Removing an entry never touches the project or its data.
use crate::project::Project;
use crate::remedy::{Problem, Remedy};
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

/// Forget the project whose main checkout is `root`. Only the entry goes: the
/// folder, the repository and the data are not touched. Returns whether there
/// was an entry.
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

/// The entry named by `selector`: a project id, or the path of its main
/// checkout (as registered, or resolved when the folder exists).
pub fn find(selector: &str) -> Result<Entry> {
    let entries = read()?.projects;
    let by_id = uuid::Uuid::parse_str(selector.trim()).ok();
    let path = Path::new(selector);
    let resolved = path.canonicalize().ok();
    entries
        .into_iter()
        .find(|entry| {
            by_id == Some(entry.id)
                || entry.root == path
                || resolved
                    .as_ref()
                    .is_some_and(|resolved| entry.root == *resolved)
        })
        .ok_or_else(|| {
            Problem::new("no registered project has that id or folder")
                .with(Remedy::Retry)
                .into_error()
        })
}

/// Whether `root` is not found right now (`NotFound`; a folder that cannot
/// be read for another reason, or that is not a folder, is not "missing").
/// A folder that is not found may have been moved or deleted, or sit on a drive
/// that is not connected: whoever asks should not say which.
fn not_found(root: &Path) -> bool {
    matches!(
        std::fs::metadata(root),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    )
}

/// The listed projects whose folder is not found right now, for showing
/// before asking whether to forget them (see [`forget_unavailable`]).
pub fn unavailable() -> Result<Vec<Entry>> {
    Ok(read()?
        .projects
        .into_iter()
        .filter(|e| not_found(&e.root))
        .collect())
}

/// Forget the `confirmed` entries, and only those, whose folder is still not
/// found. The list a person confirmed is the list that is acted on: an entry
/// that was not in it is left alone even if its folder has become unavailable
/// since (a drive unmounted meanwhile), and a confirmed entry whose folder is
/// back (the drive was mounted again), or that was moved to another folder
/// meanwhile, is kept too. An entry matches by id and folder. Returns the
/// entries removed.
pub fn forget_unavailable(confirmed: &[Entry]) -> Result<Vec<Entry>> {
    with_lock(|| {
        let mut file = read()?;
        let (removed, kept): (Vec<Entry>, Vec<Entry>) = file.projects.into_iter().partition(|e| {
            confirmed.iter().any(|c| c.id == e.id && c.root == e.root) && not_found(&e.root)
        });
        if !removed.is_empty() {
            file.projects = kept;
            write(&file)?;
        }
        Ok(removed)
    })
}

/// Point the registered project `id` at the folder it moved to.
///
/// `new_root` may be any folder of the repository; the entry gets its main
/// checkout. The folder must be that project: its `devflow.toml` has the same
/// project id (read without judging the rest of the file, so a project that
/// needs its configuration migrated can still be found again). The name is
/// refreshed from the file. Nothing on disk changes.
pub fn relocate(id: uuid::Uuid, new_root: &Path) -> Result<Entry> {
    let root = crate::project::main_checkout(new_root)?;
    let (found, name) = crate::config::Config::read_identity(&root)?;
    if found != id {
        return Err(Problem::new(format!(
            "{} holds the devflow project '{name}', not the one being located",
            root.display()
        ))
        .into_error());
    }
    with_lock(|| {
        let mut file = read()?;
        let Some(position) = file.projects.iter().position(|e| e.id == id) else {
            return Err(Problem::new("that project is no longer in the list")
                .with(Remedy::Retry)
                .into_error());
        };
        if let Some(other) = file.projects.iter().find(|e| e.root == root && e.id != id) {
            return Err(Problem::new(format!(
                "{} is already listed as '{}'",
                root.display(),
                other.name
            ))
            .into_error());
        }
        let entry = Entry { id, name, root };
        file.projects[position] = entry.clone();
        file.projects.sort_by(|a, b| a.name.cmp(&b.name));
        write(&file)?;
        Ok(entry)
    })
}
