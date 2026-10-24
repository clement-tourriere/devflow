//! Project discovery: the main checkout, its configuration and Git common dir.
use crate::config::{Config, PostgresMode};
use crate::effective::{self, Effective, RecipesSource};
use anyhow::{Context, Result, bail};
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Let libgit2 open a partial clone.
///
/// libgit2 refuses to open a repository whose configuration names an
/// extension it does not know, and a partial clone names `partialClone`
/// (`git clone --filter=...`). Without this, such a repository is "not a Git
/// checkout" to devflow before anything else can be said about it: neither
/// the typed refusal of the libgit2 checkout nor the native checkout, which
/// takes partial clones, could ever be reached. Opening only reads the
/// repository's references and configuration; the objects a partial clone
/// leaves out are Git's to fetch, and nothing here reads them.
///
/// libgit2 keeps the list in a process-wide variable and does not guard it,
/// so this runs once, before the first repository is opened by the entry
/// points of the library (`Project`, `main_checkout`, the registry's health
/// check, the configuration editor).
pub(crate) fn accept_git_extensions() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: the one write to libgit2's list of extensions, made before
        // any of those entry points opens a repository.
        let _ = unsafe { git2::opts::set_extensions(&["partialclone"]) };
    });
}

#[derive(Debug, Clone)]
pub struct Project {
    /// The main checkout (the default workspace).
    pub root: PathBuf,
    /// The repository's common Git directory, shared by all worktrees.
    pub git_common_dir: PathBuf,
    /// The configuration. A project opened from disk carries the main
    /// checkout's; one resolved for a workspace by [`Project::at`] carries
    /// that workspace's effective configuration: the main checkout's shared
    /// sections with the workspace's own recipes (see [`crate::effective`]).
    pub config: Config,
    /// The main checkout's project file.
    pub config_path: PathBuf,
    /// Set by [`Project::at`]: the workspace this project was resolved for.
    pinned: Option<Arc<Pinned>>,
}

/// What a project resolved for one workspace remembers.
#[derive(Debug)]
struct Pinned {
    effective: Effective,
    /// The main checkout's own configuration, which a choice to use the
    /// default workspace's recipes (and every other workspace) starts from.
    main: Config,
}

impl Project {
    /// A project from its parts (tests and frontends that build one by hand):
    /// not resolved for any workspace.
    pub fn new(
        root: PathBuf,
        git_common_dir: PathBuf,
        config: Config,
        config_path: PathBuf,
    ) -> Self {
        Self {
            root,
            git_common_dir,
            config,
            config_path,
            pinned: None,
        }
    }

    /// The main checkout's configuration, whichever workspace this project was
    /// resolved for.
    pub fn main_config(&self) -> &Config {
        self.pinned
            .as_deref()
            .map_or(&self.config, |pinned| &pinned.main)
    }

    /// This project as workspace `dir` sees it: the same project, with the
    /// configuration `dir` runs (see [`crate::effective`]) and kept as it is
    /// read now for as long as the value lives, so one operation works from one
    /// configuration whatever its hooks do to `devflow.toml`. Borrowed when
    /// the project is already resolved for `dir`, or `dir` is the main
    /// checkout of a project read from disk.
    ///
    /// The choice to use the default workspace's recipes is read from the
    /// workspace's receipt; [`Project::resolve`] takes it from the caller.
    pub fn at(&self, dir: &Path) -> Cow<'_, Project> {
        let dir = canonical_or(dir);
        match &self.pinned {
            Some(pinned) if pinned.effective.dir == dir => return Cow::Borrowed(self),
            None if dir == self.root => return Cow::Borrowed(self),
            _ => {}
        }
        Cow::Owned(self.at_choosing(&dir, false))
    }

    /// [`Project::at`], always resolved afresh, with the choice to use the
    /// default workspace's recipes also when `requested` (a run that is about
    /// to record it): the choice the receipt holds, or this one.
    pub fn at_choosing(&self, dir: &Path, requested: bool) -> Project {
        let dir = canonical_or(dir);
        let recorded = dir != self.root
            && crate::worktree::git_dir_of(&dir)
                .and_then(|git_dir| crate::receipt::Receipt::load(&git_dir).ok().flatten())
                .is_some_and(|receipt| receipt.keeps_default_recipes());
        self.resolve(&dir, recorded || requested)
    }

    /// [`Project::at`] with the choice to use the default workspace's recipes
    /// given: where the workspace's own file is unusable, they apply when
    /// `use_default` is set and nothing runs otherwise. A usable file always
    /// wins.
    pub fn resolve(&self, dir: &Path, use_default: bool) -> Project {
        let dir = canonical_or(dir);
        let main = self.main_config();
        let (config, effective) =
            effective::resolve(&self.root, &self.config_path, main, &dir, use_default);
        Project {
            root: self.root.clone(),
            git_common_dir: self.git_common_dir.clone(),
            config,
            config_path: self.config_path.clone(),
            pinned: Some(Arc::new(Pinned {
                effective,
                main: main.clone(),
            })),
        }
    }

    /// Where this project's recipes come from: the main checkout's for a
    /// project that was not resolved for a workspace.
    pub fn recipes(&self) -> RecipesSource {
        match &self.pinned {
            Some(pinned) => pinned.effective.recipes.clone(),
            None => RecipesSource::Main {
                file: self.config_path.clone(),
            },
        }
    }

    /// The effective configuration this project was resolved for; for one that
    /// was not, the main checkout's own.
    pub fn effective(&self) -> Cow<'_, Effective> {
        match &self.pinned {
            Some(pinned) => Cow::Borrowed(&pinned.effective),
            None => Cow::Owned(
                effective::resolve(
                    &self.root,
                    &self.config_path,
                    &self.config,
                    &self.root,
                    false,
                )
                .1,
            ),
        }
    }

    /// Find the project containing `start` (any worktree or subdirectory).
    /// A repository without `devflow.toml` fails with
    /// [`NotInitialized`](crate::error::NotInitialized) naming its main checkout.
    pub fn discover(start: &Path) -> Result<Project> {
        accept_git_extensions();
        let repo = git2::Repository::discover(start)
            .with_context(|| format!("{} is not inside a Git repository", start.display()))?;
        Self::from_repo(&repo)
    }

    /// Open the project whose main checkout is `root`; fails with
    /// [`NotInitialized`](crate::error::NotInitialized) without `devflow.toml`.
    pub fn open(root: &Path) -> Result<Project> {
        accept_git_extensions();
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
        Ok(Project::new(root, canonical(&common)?, config, config_path))
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

    /// [`Project::reload`], which must find the same project (by identity,
    /// not by path): a folder that holds another project now is an error, and
    /// so are files that cannot be read.
    pub fn reload_same(&self) -> Result<Project> {
        let fresh = self.reload().with_context(|| {
            format!(
                "cannot read {} again; nothing was changed",
                self.config_path.display()
            )
        })?;
        anyhow::ensure!(
            fresh.id() == self.id(),
            "{} is project {} now, not {}; nothing was changed",
            fresh.config_path.display(),
            fresh.id(),
            self.id()
        );
        Ok(fresh)
    }
}

pub struct InitOptions {
    pub name: Option<String>,
    pub postgres: Option<PostgresMode>,
}

/// The main checkout of the repository containing `start` (any worktree or
/// subdirectory), whether or not it has a `devflow.toml`.
pub fn main_checkout(start: &Path) -> Result<PathBuf> {
    accept_git_extensions();
    let repo = git2::Repository::discover(start)
        .with_context(|| format!("{} is not inside a Git repository", start.display()))?;
    anyhow::ensure!(!repo.is_bare(), "bare repositories are not supported");
    let main = git2::Repository::open(repo.commondir())?;
    canonical(
        main.workdir()
            .context("the main repository has no checkout")?,
    )
}

/// The checkout (the main one or a linked worktree) that contains `start`: the
/// working tree of the repository found from it, resolved. `None` outside any
/// working tree.
pub fn checkout_containing(start: &Path) -> Option<PathBuf> {
    git2::Repository::discover(start)
        .ok()?
        .workdir()?
        .canonicalize()
        .ok()
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

fn canonical_or(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
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
