//! Project discovery: the main checkout, its configuration and Git common dir.
use crate::config::{Config, PostgresMode};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Project {
    /// The main checkout (the default workspace).
    pub root: PathBuf,
    /// The repository's common Git directory, shared by all worktrees.
    pub git_common_dir: PathBuf,
    pub config: Config,
    pub config_path: PathBuf,
}

impl Project {
    /// Find the project containing `start` (any worktree or subdirectory).
    /// A repository without `devflow.toml` fails with
    /// [`NotInitialized`](crate::error::NotInitialized) naming its main checkout.
    pub fn discover(start: &Path) -> Result<Project> {
        let repo = git2::Repository::discover(start)
            .with_context(|| format!("{} is not inside a Git repository", start.display()))?;
        Self::from_repo(&repo)
    }

    /// Open the project whose main checkout is `root`; fails with
    /// [`NotInitialized`](crate::error::NotInitialized) without `devflow.toml`.
    pub fn open(root: &Path) -> Result<Project> {
        let repo = git2::Repository::open(root)
            .with_context(|| format!("{} is not a Git checkout", root.display()))?;
        Self::from_repo(&repo)
    }

    fn from_repo(repo: &git2::Repository) -> Result<Project> {
        if repo.is_bare() {
            bail!("bare repositories are not supported; use a repository with a main checkout");
        }
        let common = repo.commondir().to_path_buf();
        let main = git2::Repository::open(&common)
            .with_context(|| format!("cannot open main repository at {}", common.display()))?;
        let root = main
            .workdir()
            .context("the main repository has no checkout")?;
        let root = canonical(root)?;
        let (config, config_path) = Config::load(&root)?;
        Ok(Project {
            root,
            git_common_dir: canonical(&common)?,
            config,
            config_path,
        })
    }

    pub fn id(&self) -> uuid::Uuid {
        self.config.project.id
    }

    pub fn name(&self) -> &str {
        &self.config.project.name
    }

    pub fn repo(&self) -> Result<git2::Repository> {
        git2::Repository::open(&self.root)
            .with_context(|| format!("cannot open repository at {}", self.root.display()))
    }

    /// Reload configuration from disk (after an edit).
    pub fn reload(&self) -> Result<Project> {
        Self::open(&self.root)
    }
}

pub struct InitOptions {
    pub name: Option<String>,
    pub postgres: Option<PostgresMode>,
}

/// The main checkout of the repository containing `start` (any worktree or
/// subdirectory), whether or not it has a `devflow.toml`.
pub fn main_checkout(start: &Path) -> Result<PathBuf> {
    let repo = git2::Repository::discover(start)
        .with_context(|| format!("{} is not inside a Git repository", start.display()))?;
    anyhow::ensure!(!repo.is_bare(), "bare repositories are not supported");
    let main = git2::Repository::open(repo.commondir())?;
    canonical(
        main.workdir()
            .context("the main repository has no checkout")?,
    )
}

/// Write `devflow.toml` in the main checkout containing `start` and register
/// the project. Refuses to overwrite an existing configuration.
pub fn init(start: &Path, options: InitOptions) -> Result<Project> {
    let root = main_checkout(start)?;
    if let Some(existing) = Config::find_file(&root) {
        bail!("{} already exists", existing.display());
    }
    let name = match options.name {
        Some(name) => name,
        None => root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .context("cannot derive a project name; pass --name")?,
    };
    let text = Config::template(&name, uuid::Uuid::new_v4(), options.postgres);
    crate::fsutil::write_atomic(
        &root.join(crate::config::CONFIG_FILES[0]),
        text.as_bytes(),
        false,
    )?;
    let project = Project::open(&root)?;
    crate::registry::add(&project)?;
    Ok(project)
}

fn canonical(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("cannot resolve {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::NotInitialized;

    #[test]
    fn repositories_without_a_project_file_report_where_to_initialize() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("bare-project");
        std::fs::create_dir_all(&root).unwrap();
        let repo = git2::Repository::init(&root).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree = repo
            .find_tree(repo.index().unwrap().write_tree().unwrap())
            .unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
            .unwrap();

        for error in [
            Project::open(&root).unwrap_err(),
            Project::discover(&root).unwrap_err(),
        ] {
            let typed = error.downcast_ref::<NotInitialized>().expect("typed");
            assert_eq!(typed.root, root);
        }
        // From a linked worktree it names the main checkout.
        let linked = dir.path().join("linked");
        repo.worktree("linked", &linked, None).unwrap();
        let error = Project::discover(&linked).unwrap_err();
        assert_eq!(error.downcast_ref::<NotInitialized>().unwrap().root, root);

        // Not a repository at all stays an ordinary error.
        let plain = tempfile::tempdir().unwrap();
        let error = Project::open(plain.path()).unwrap_err();
        assert!(
            error.downcast_ref::<NotInitialized>().is_none(),
            "{error:#}"
        );
    }
}
