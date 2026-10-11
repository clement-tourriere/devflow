//! Git worktrees through libgit2: listing, creation, removal and branch
//! deletion. devflow never shells out to `git` for these.
use crate::error::Protected;
use crate::project::Project;
use crate::remedy::{Problem, Remedy};
use anyhow::{Context, Result, bail, ensure};
use git2::{
    BranchType, ErrorCode, Repository, StatusOptions, WorktreeAddOptions, WorktreeLockStatus,
    WorktreePruneOptions,
};
use serde::Serialize;
use std::path::{Component, Path, PathBuf};

mod prune;
pub use prune::{
    PruneCandidate, PruneOutcome, PruneRefusal, PruneRefused, PruneTarget,
    candidates as prune_candidates, prune,
};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Workspace {
    /// Branch name, or the worktree's directory name when HEAD is detached.
    pub name: String,
    pub branch: Option<String>,
    pub path: PathBuf,
    /// This worktree's own Git directory (`.git` for the main checkout,
    /// `.git/worktrees/<id>` for linked worktrees).
    pub git_dir: PathBuf,
    /// The main checkout.
    pub is_default: bool,
    pub locked: bool,
    /// Registered with Git but its directory is missing.
    pub missing: bool,
    /// Short commit id of HEAD.
    pub head: Option<String>,
    /// Identity of the checkout directory on disk (see [`fingerprint`]);
    /// `None` when it is missing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

/// Device, inode and creation time of a directory: tells a checkout apart from
/// another one created later at the same path, even when neither has a
/// receipt. Contents changing inside it do not change it.
pub fn fingerprint(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    let born = meta
        .created()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos().to_string())
        .unwrap_or_default();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(format!("{}:{}:{born}", meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    {
        (!born.is_empty()).then_some(born)
    }
}

/// The main checkout first, then linked worktrees sorted by name.
pub fn list(project: &Project) -> Result<Vec<Workspace>> {
    let repo = project.repo()?;
    let (branch, head) = head_info(&repo);
    let mut all = vec![Workspace {
        name: branch.clone().unwrap_or_else(|| dir_name(&project.root)),
        branch,
        path: project.root.clone(),
        git_dir: canonical(repo.path()),
        is_default: true,
        locked: false,
        missing: false,
        head,
        fingerprint: fingerprint(&project.root),
    }];
    let mut linked = Vec::new();
    for id in repo.worktrees()?.iter().flatten().flatten() {
        let Ok(worktree) = repo.find_worktree(id) else {
            continue;
        };
        let path = canonical(worktree.path());
        let admin = repo.commondir().join("worktrees").join(id);
        let opened = if path.is_dir() {
            Repository::open(&path).ok()
        } else {
            None
        };
        let missing = opened.is_none();
        let git_dir = opened
            .as_ref()
            .map(|r| canonical(r.path()))
            .unwrap_or_else(|| canonical(&admin));
        // A missing checkout still has HEAD in its administrative directory.
        let (branch, head) = match &opened {
            Some(r) => head_info(r),
            None => Repository::open(&admin)
                .map(|r| head_info(&r))
                .unwrap_or((None, None)),
        };
        let fingerprint = if missing { None } else { fingerprint(&path) };
        linked.push(Workspace {
            name: branch.clone().unwrap_or_else(|| dir_name(&path)),
            branch,
            path,
            git_dir,
            is_default: false,
            locked: matches!(worktree.is_locked(), Ok(WorktreeLockStatus::Locked(_))),
            missing,
            head,
            fingerprint,
        });
    }
    linked.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
    all.extend(linked);
    Ok(all)
}

/// Find by branch name, worktree name, or path.
pub fn find(project: &Project, name: &str) -> Result<Option<Workspace>> {
    let all = list(project)?;
    if let Some(found) = all
        .iter()
        .find(|w| w.name == name || w.branch.as_deref() == Some(name))
    {
        return Ok(Some(found.clone()));
    }
    // Git's own worktree name (what `git worktree list` calls the id).
    if let Some(found) = all
        .iter()
        .find(|w| !w.is_default && w.git_dir.file_name().is_some_and(|n| n == name))
    {
        return Ok(Some(found.clone()));
    }
    Ok(Path::new(name)
        .canonicalize()
        .ok()
        .and_then(|p| all.into_iter().find(|w| w.path == p)))
}

/// The workspace containing `dir`, if any. Nested checkouts win over their
/// enclosing one (worktrees may live inside the main checkout).
pub fn containing(project: &Project, dir: &Path) -> Result<Option<Workspace>> {
    let dir = canonical(dir);
    Ok(list(project)?
        .into_iter()
        .filter(|w| !w.missing && dir.starts_with(&w.path))
        .max_by_key(|w| w.path.components().count()))
}

/// Where a new worktree for `name` goes: `<worktree.dir>/<sanitize(name)>`.
/// `{repo}` is the main checkout's directory name, `{project}` the sanitized
/// project name; a relative `worktree.dir` is relative to the main checkout.
pub fn path_for(project: &Project, name: &str) -> PathBuf {
    let dir = project
        .config
        .worktree
        .dir
        .replace("{repo}", &dir_name(&project.root))
        .replace("{project}", &sanitize(project.name()));
    let base = match dir.strip_prefix("~/").zip(dirs::home_dir()) {
        Some((rest, home)) => home.join(rest),
        None => project.root.join(&dir),
    };
    normalize(&base).join(sanitize(name))
}

/// Reject names Git cannot use as a branch.
pub fn validate_name(name: &str) -> Result<()> {
    ensure!(!name.trim().is_empty(), "workspace name is empty");
    ensure!(
        git2::Branch::name_is_valid(name)?,
        "'{name}' is not a valid Git branch name"
    );
    Ok(())
}

/// Add a worktree for branch `name`. Creates the branch from `from`'s HEAD
/// (default: the main checkout's HEAD) when it does not exist yet.
///
/// Creations are serialized per repository: two names can map to the same
/// directory and Git worktree name (`feature/a`, `feature-a`), and the checks
/// below are only meaningful while no other creation runs. The lock only
/// coordinates devflow, though: another tool may create the same worktree
/// between the checks and the libgit2 call. A failed `repo.worktree()` therefore
/// never deletes anything, because neither the error code nor the earlier
/// absence checks prove who made what is on disk. Whatever it left is reported
/// by path for the user to inspect.
pub fn create(project: &Project, name: &str, from: Option<&Workspace>) -> Result<Workspace> {
    validate_name(name)?;
    let lock = project.git_common_dir.join("devflow").join("create.lock");
    crate::fsutil::with_lock(&lock, || create_locked(project, name, from))
}

fn create_locked(project: &Project, name: &str, from: Option<&Workspace>) -> Result<Workspace> {
    let repo = project.repo()?;
    let path = path_for(project, name);
    ensure!(
        path.symlink_metadata().is_err(),
        "{} already exists; choose another name or remove it",
        path.display()
    );
    let admin_name = sanitize(name);
    let admin = repo.commondir().join("worktrees").join(&admin_name);
    if repo.find_worktree(&admin_name).is_ok() || admin.exists() {
        return Err(Problem::new(format!(
            "a Git worktree named '{admin_name}' is already registered; if its directory is \
             gone, forget the registration"
        ))
        .with(Remedy::PruneWorktrees {
            root: project.root.clone(),
        })
        .into_error());
    }
    if let Some(other) = list(project)?
        .into_iter()
        .find(|w| w.branch.as_deref() == Some(name))
    {
        bail!(
            "branch '{name}' is already checked out at {}",
            other.path.display()
        );
    }

    // The commit of a branch this call created (exclusively: `force` is off).
    let created_branch = match repo.find_branch(name, BranchType::Local) {
        Ok(_) => None,
        Err(e) if e.code() == ErrorCode::NotFound => {
            let tip = match from {
                Some(from) => {
                    ensure!(
                        !from.missing,
                        "workspace '{}' is missing on disk",
                        from.name
                    );
                    head_commit(&Repository::open(&from.path)?, &from.name)?
                }
                None => head_commit(&repo, "the default workspace")?,
            };
            repo.branch(name, &repo.find_commit(tip)?, false)
                .with_context(|| format!("cannot create branch '{name}'"))?;
            Some(tip)
        }
        Err(e) => return Err(e.into()),
    };

    let added = (|| -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let branch = repo.find_branch(name, BranchType::Local)?.into_reference();
        let mut options = WorktreeAddOptions::new();
        options.reference(Some(&branch));
        #[cfg(test)]
        testhook::before_add();
        // libgit2 creates the checkout and the administrative directory
        // exclusively: it never adopts a directory that already exists.
        repo.worktree(&admin_name, &path, Some(&options))
            .map(|_| ())
            .map_err(|e| {
                let left: Vec<String> = [&path, &admin]
                    .into_iter()
                    .filter(|p| p.symlink_metadata().is_ok())
                    .map(|p| p.display().to_string())
                    .collect();
                let note = if left.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; devflow left {} in place because it cannot tell whether another tool \
                         or a half-finished checkout put it there (inspect it, then remove it \
                         with `git worktree remove --force` or `git worktree prune`)",
                        left.join(" and ")
                    )
                };
                anyhow::Error::new(e)
                    .context(format!("cannot add worktree at {}{note}", path.display()))
            })
    })();
    if let Err(error) = added {
        // The branch goes only with nothing on disk to explain it: a leftover
        // checkout or registration may be using it.
        let untouched = path.symlink_metadata().is_err() && !admin.exists();
        if untouched
            && let Some(tip) = created_branch
            && let Ok(mut branch) = repo.find_branch(name, BranchType::Local)
            && branch.get().target() == Some(tip)
        {
            let _ = branch.delete();
        }
        return Err(error);
    }
    find(project, name)?.context("the new worktree is not listed by Git")
}

/// Uncommitted changes, untracked (not ignored) files included.
pub fn is_dirty(workspace: &Workspace) -> Result<bool> {
    if workspace.missing {
        return Ok(false);
    }
    let repo = Repository::open(&workspace.path)
        .with_context(|| format!("cannot open {}", workspace.path.display()))?;
    let mut options = StatusOptions::new();
    options.include_untracked(true).include_ignored(false);
    Ok(!repo.statuses(Some(&mut options))?.is_empty())
}

/// A removal refusal as a typed error. `forceable` means `--force` overrides it.
pub(crate) fn protected(
    workspace: &str,
    reason: impl Into<String>,
    forceable: bool,
) -> anyhow::Error {
    protected_with(workspace, reason, forceable, Vec::new())
}

/// [`protected`] with the remedies that lift the protection.
pub(crate) fn protected_with(
    workspace: &str,
    reason: impl Into<String>,
    forceable: bool,
    remedies: Vec<crate::remedy::Remedy>,
) -> anyhow::Error {
    anyhow::Error::new(Protected {
        workspace: workspace.to_string(),
        reason: reason.into(),
        forceable,
        remedies,
    })
}

/// Delete the worktree directory and its Git registration. Refuses the main
/// checkout and locked worktrees (see [`force_remove`]).
pub fn remove(project: &Project, workspace: &Workspace) -> Result<()> {
    remove_inner(project, workspace, false)
}

/// Like [`remove`], but also removes a locked worktree.
pub fn force_remove(project: &Project, workspace: &Workspace) -> Result<()> {
    remove_inner(project, workspace, true)
}

/// Everything [`remove`] (or [`force_remove`] with `ignore_lock`) would refuse,
/// without any effect, so callers can check before their own first effect.
pub fn check_removable(project: &Project, workspace: &Workspace, ignore_lock: bool) -> Result<()> {
    plan_removal(project, workspace, ignore_lock).map(|_| ())
}

struct Removal {
    /// Git's name for the worktree (its administrative directory).
    id: String,
    /// The directory is present and positively this worktree's checkout.
    delete_dir: bool,
}

fn plan_removal(project: &Project, workspace: &Workspace, ignore_lock: bool) -> Result<Removal> {
    let name = workspace.name.as_str();
    if workspace.is_default {
        return Err(protected(
            name,
            format!("'{name}' is the default workspace and cannot be removed"),
            false,
        ));
    }
    if workspace.locked && !ignore_lock {
        return Err(protected(
            name,
            format!("'{name}' is locked (unlock it with `git worktree unlock`)"),
            true,
        ));
    }
    let path = &workspace.path;
    if project.root.starts_with(path) || project.git_common_dir.starts_with(path) {
        return Err(protected(
            name,
            format!(
                "refusing to delete {}: it contains the main checkout",
                path.display()
            ),
            false,
        ));
    }
    let repo = project.repo()?;
    let id = workspace
        .git_dir
        .file_name()
        .and_then(|n| n.to_str())
        .context("worktree has no registered name")?
        .to_string();
    ensure!(
        canonical(&repo.commondir().join("worktrees").join(&id)) == workspace.git_dir,
        "{} is not a linked worktree of this repository",
        path.display()
    );
    // The listing may be stale and the path may have been replaced since: only
    // delete a directory that is positively this worktree's checkout.
    let delete_dir = match path.symlink_metadata() {
        Ok(_) if is_checkout_of(path, &workspace.git_dir) => true,
        Ok(_) => {
            return Err(protected_with(
                name,
                format!(
                    "{} exists but is not the checkout of worktree '{name}' (its .git file does not point at {}); \
                     refusing to delete it. Move it away, or forget the registration",
                    path.display(),
                    workspace.git_dir.display()
                ),
                false,
                vec![Remedy::PruneWorktrees {
                    root: project.root.clone(),
                }],
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e).with_context(|| format!("cannot inspect {}", path.display())),
    };
    Ok(Removal { id, delete_dir })
}

/// Whether `dir`'s `.git` is a regular file pointing at the administrative
/// directory `admin`, which is how Git ties a checkout to its worktree.
fn is_checkout_of(dir: &Path, admin: &Path) -> bool {
    linked_git_dir(dir).is_some_and(|target| target == admin)
}

/// What the regular file `dir/.git` of a linked worktree points at
/// (`gitdir: …`, a relative path resolved against `dir`), canonicalized.
fn linked_git_dir(dir: &Path) -> Option<PathBuf> {
    let git_file = dir.join(".git");
    if !git_file.symlink_metadata().ok()?.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&git_file).ok()?;
    let target = text.lines().next()?.strip_prefix("gitdir:")?.trim();
    let target = Path::new(target);
    let target = if target.is_relative() {
        dir.join(target)
    } else {
        target.to_path_buf()
    };
    target.canonicalize().ok()
}

/// The Git directory of the checkout at `dir`, read from `dir/.git` without
/// running Git: `.git` itself for a main checkout, what its `.git` file points
/// at for a linked worktree. It is where devflow keeps the workspace's receipt.
/// `None` when `dir` is not a checkout.
pub fn git_dir_of(dir: &Path) -> Option<PathBuf> {
    let dot_git = dir.join(".git");
    if dot_git.symlink_metadata().ok()?.is_dir() {
        return dot_git.canonicalize().ok();
    }
    linked_git_dir(dir)
}

fn remove_inner(project: &Project, workspace: &Workspace, ignore_lock: bool) -> Result<()> {
    let plan = plan_removal(project, workspace, ignore_lock)?;
    let path = &workspace.path;
    if plan.delete_dir {
        match std::fs::remove_dir_all(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("cannot delete {}", path.display())),
        }
    }
    // A missing directory leaves only the registration to forget.
    let registration = project.repo()?.find_worktree(&plan.id)?;
    let mut options = WorktreePruneOptions::new();
    options.valid(true).locked(ignore_lock);
    registration
        .prune(Some(&mut options))
        .with_context(|| format!("cannot prune the Git registration of {}", workspace.name))
}

/// Whether `branch` is contained in the default workspace's HEAD, so deleting
/// it loses no commits.
pub fn branch_merged(project: &Project, branch: &str) -> Result<bool> {
    let repo = project.repo()?;
    let tip = repo
        .find_branch(branch, BranchType::Local)
        .with_context(|| format!("no local branch '{branch}'"))?
        .get()
        .peel_to_commit()?
        .id();
    let head = head_commit(&repo, "the default workspace")?;
    Ok(tip == head || repo.graph_descendant_of(head, tip)?)
}

/// Delete a local branch. `force` allows deleting unmerged branches.
pub fn delete_branch(project: &Project, branch: &str, force: bool) -> Result<()> {
    if let Some(w) = list(project)?
        .into_iter()
        .find(|w| w.branch.as_deref() == Some(branch))
    {
        bail!("branch '{branch}' is checked out at {}", w.path.display());
    }
    if !force && !branch_merged(project, branch)? {
        return Err(protected(
            branch,
            format!(
                "branch '{branch}' has commits that are not in the default workspace; merge it"
            ),
            true,
        ));
    }
    project
        .repo()?
        .find_branch(branch, BranchType::Local)
        .with_context(|| format!("no local branch '{branch}'"))?
        .delete()
        .with_context(|| format!("cannot delete branch '{branch}'"))
}

/// Directory-safe form of a workspace name (`feature/a b` → `feature-a-b`).
pub fn sanitize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        let c = if c.is_alphanumeric() || matches!(c, '.' | '_' | '-') {
            c
        } else {
            '-'
        };
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    // Leading dots would hide the directory (or make it `..`).
    let trimmed = out.trim_matches(|c| c == '-' || c == '.');
    if trimmed.is_empty() {
        "workspace".to_string()
    } else {
        trimmed.to_string()
    }
}

fn head_commit(repo: &Repository, what: &str) -> Result<git2::Oid> {
    Ok(repo
        .head()
        .and_then(|h| h.peel_to_commit())
        .with_context(|| format!("{what} has no commits yet"))?
        .id())
}

/// Branch (when HEAD is symbolic) and short commit id of HEAD.
fn head_info(repo: &Repository) -> (Option<String>, Option<String>) {
    let branch = repo.find_reference("HEAD").ok().and_then(|r| {
        let target = r.symbolic_target().ok().flatten()?;
        target.strip_prefix("refs/heads/").map(str::to_string)
    });
    let head = repo.head().and_then(|h| h.peel_to_commit()).ok().map(|c| {
        c.as_object()
            .short_id()
            .ok()
            .and_then(|b| b.as_str().ok().map(str::to_string))
            .unwrap_or_else(|| c.id().to_string().chars().take(7).collect())
    });
    (branch, head)
}

fn dir_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        // `Path::components` drops a trailing separator.
        path.components().collect()
    })
}

/// Resolve `.` and `..` lexically (the base is already canonical).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Lets a test act between devflow's checks and the libgit2 call, the window in
/// which another tool can create the same worktree.
#[cfg(test)]
mod testhook {
    use std::cell::RefCell;

    type Hook = Box<dyn FnOnce()>;

    thread_local! {
        static BEFORE_ADD: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    /// Runs once, on this thread, before the next `repo.worktree()` call.
    pub fn set(hook: impl FnOnce() + 'static) {
        BEFORE_ADD.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    pub fn before_add() {
        if let Some(hook) = BEFORE_ADD.with(|h| h.borrow_mut().take()) {
            hook();
        }
    }
}

/// Temporary repositories shared by the unit tests of this crate.
#[cfg(test)]
pub(crate) mod testutil {
    use crate::config::Config;
    use crate::project::Project;
    use std::path::PathBuf;

    pub struct TestRepo {
        pub dir: tempfile::TempDir,
        pub root: PathBuf,
        pub project: Project,
    }

    pub fn signature() -> git2::Signature<'static> {
        git2::Signature::now("Test", "test@example.com").unwrap()
    }

    /// Commit everything currently in `root`'s working tree.
    pub fn commit_all(repo: &git2::Repository, message: &str) -> git2::Oid {
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<_> = parent.iter().collect();
        let sig = signature();
        repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
            .unwrap()
    }

    /// A repository named `shop` on branch `main` with a committed
    /// `devflow.toml` (`config` replaces the template when given).
    pub fn repo(config: Option<&str>) -> TestRepo {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("shop");
        std::fs::create_dir_all(&root).unwrap();
        let mut options = git2::RepositoryInitOptions::new();
        options.initial_head("main");
        let repo = git2::Repository::init_opts(&root, &options).unwrap();
        let text = match config {
            Some(text) => text.to_string(),
            None => Config::template("shop", uuid::Uuid::new_v4(), None),
        };
        std::fs::write(root.join("devflow.toml"), text).unwrap();
        std::fs::write(
            root.join(".gitignore"),
            ".env\nnode_modules/\ndevflow.local.toml\n",
        )
        .unwrap();
        std::fs::write(root.join("README.md"), "shop\n").unwrap();
        commit_all(&repo, "initial");
        let project = Project::open(&root).unwrap();
        TestRepo { dir, root, project }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    #[test]
    fn sanitize_replaces_unsafe_characters() {
        assert_eq!(sanitize("feature/a b"), "feature-a-b");
        assert_eq!(sanitize("agent//TASK--1"), "agent-TASK-1");
        assert_eq!(sanitize("  spaced  "), "spaced");
        assert_eq!(sanitize(".."), "workspace");
        assert_eq!(sanitize("../etc/passwd"), "etc-passwd");
        assert_eq!(sanitize("v1.2_x"), "v1.2_x");
        assert_eq!(sanitize("///"), "workspace");
    }

    #[test]
    fn lists_main_first_and_expands_the_directory_template() {
        let t = repo(None);
        let all = list(&t.project).unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].is_default);
        assert_eq!(all[0].name, "main");
        assert_eq!(all[0].path, t.root);
        assert_eq!(all[0].git_dir, t.root.join(".git"));
        assert!(all[0].head.is_some());
        // Default template: ../{repo}.worktrees/<name>.
        assert_eq!(
            path_for(&t.project, "feature/a b"),
            t.root
                .parent()
                .unwrap()
                .join("shop.worktrees")
                .join("feature-a-b")
        );
    }

    #[test]
    fn project_placeholder_and_absolute_directories() {
        let t = repo(None);
        let mut project = t.project.clone();
        project.config.worktree.dir = "../{project}-wt".into();
        assert_eq!(
            path_for(&project, "x"),
            t.root.parent().unwrap().join("shop-wt").join("x")
        );
        let elsewhere = t.root.parent().unwrap().join("elsewhere");
        project.config.worktree.dir = elsewhere.to_string_lossy().into_owned();
        assert_eq!(path_for(&project, "x"), elsewhere.join("x"));
    }

    #[test]
    fn creates_a_worktree_with_a_new_branch_from_the_default_head() {
        let t = repo(None);
        let ws = create(&t.project, "feature/a", None).unwrap();
        assert_eq!(ws.branch.as_deref(), Some("feature/a"));
        assert_eq!(ws.name, "feature/a");
        assert_eq!(
            ws.path,
            t.root.parent().unwrap().join("shop.worktrees/feature-a")
        );
        assert!(ws.path.join("README.md").is_file());
        assert!(!ws.is_default && !ws.locked && !ws.missing);
        assert_eq!(ws.git_dir, t.root.join(".git/worktrees/feature-a"));
        let all = list(&t.project).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1], ws);
        assert_eq!(all[0].head, ws.head, "branch starts at the default HEAD");
        assert_eq!(find(&t.project, "feature/a").unwrap(), Some(ws.clone()));
        assert_eq!(find(&t.project, "feature-a").unwrap(), Some(ws.clone()));
        assert_eq!(
            find(&t.project, ws.path.to_str().unwrap()).unwrap(),
            Some(ws.clone())
        );
        assert_eq!(find(&t.project, "nope").unwrap(), None);
        assert!(!is_dirty(&ws).unwrap());
    }

    #[test]
    fn creates_from_another_workspace_and_reuses_existing_branches() {
        let t = repo(None);
        let a = create(&t.project, "a", None).unwrap();
        let repo_a = git2::Repository::open(&a.path).unwrap();
        std::fs::write(a.path.join("only-in-a.txt"), "a").unwrap();
        commit_all(&repo_a, "a work");
        let b = create(&t.project, "b", Some(&a)).unwrap();
        assert!(b.path.join("only-in-a.txt").is_file());
        assert_eq!(b.head, find(&t.project, "a").unwrap().unwrap().head);

        // An existing branch is checked out as-is, `from` is ignored.
        remove(&t.project, &b).unwrap();
        let again = create(&t.project, "b", None).unwrap();
        assert!(again.path.join("only-in-a.txt").is_file());
    }

    #[test]
    fn refuses_collisions_and_branches_checked_out_elsewhere() {
        let t = repo(None);
        create(&t.project, "feature/a", None).unwrap();
        // `feature-a` maps to the same directory.
        let error = create(&t.project, "feature-a", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("already exists"), "{error}");
        // The main checkout holds `main`.
        let error = create(&t.project, "main", None).unwrap_err().to_string();
        assert!(error.contains("already checked out"), "{error}");
        assert!(create(&t.project, "bad name", None).is_err());
        assert!(create(&t.project, "x..y", None).is_err());
        // Nothing was left behind by the refusals.
        assert_eq!(list(&t.project).unwrap().len(), 2);
        let repo = t.project.repo().unwrap();
        assert!(repo.find_branch("feature-a", BranchType::Local).is_err());
    }

    #[test]
    fn dirty_means_changes_or_untracked_but_not_ignored() {
        let t = repo(None);
        let ws = create(&t.project, "d", None).unwrap();
        std::fs::write(ws.path.join(".env"), "SECRET=1").unwrap();
        std::fs::create_dir_all(ws.path.join("node_modules/x")).unwrap();
        std::fs::write(ws.path.join("node_modules/x/i.js"), "").unwrap();
        crate::env_file::write(&ws.path, &[("A".into(), "b".into())]).unwrap();
        assert!(
            !is_dirty(&ws).unwrap(),
            "ignored files and .devflow do not count"
        );
        std::fs::write(ws.path.join("new.txt"), "x").unwrap();
        assert!(is_dirty(&ws).unwrap(), "untracked");
        std::fs::remove_file(ws.path.join("new.txt")).unwrap();
        std::fs::write(ws.path.join("README.md"), "changed").unwrap();
        assert!(is_dirty(&ws).unwrap(), "modified");
    }

    #[test]
    fn remove_deletes_directory_and_registration_but_keeps_the_branch() {
        let t = repo(None);
        let ws = create(&t.project, "gone", None).unwrap();
        remove(&t.project, &ws).unwrap();
        assert!(!ws.path.exists());
        assert!(!t.root.join(".git/worktrees/gone").exists());
        assert_eq!(list(&t.project).unwrap().len(), 1);
        let repo = t.project.repo().unwrap();
        assert!(repo.find_branch("gone", BranchType::Local).is_ok());
        // Branch is at the default HEAD, so it counts as merged.
        delete_branch(&t.project, "gone", false).unwrap();
        assert!(repo.find_branch("gone", BranchType::Local).is_err());
    }

    #[test]
    fn remove_refuses_default_locked_and_handles_missing_directories() {
        let t = repo(None);
        let default = list(&t.project).unwrap().remove(0);
        let error = remove(&t.project, &default).unwrap_err();
        let refusal = error.downcast_ref::<Protected>().expect("typed refusal");
        assert!(!refusal.forceable, "{refusal}");
        assert!(t.root.join("README.md").is_file());

        create(&t.project, "locked", None).unwrap();
        let repo = t.project.repo().unwrap();
        repo.find_worktree("locked")
            .unwrap()
            .lock(Some("keep"))
            .unwrap();
        let ws = find(&t.project, "locked").unwrap().unwrap();
        assert!(ws.locked);
        let error = remove(&t.project, &ws).unwrap_err();
        let refusal = error.downcast_ref::<Protected>().expect("typed refusal");
        assert!(refusal.forceable, "{refusal}");
        assert!(!refusal.to_string().contains("--force"), "{refusal}");
        assert!(ws.path.exists());
        force_remove(&t.project, &ws).unwrap();
        assert!(!ws.path.exists());

        let gone = create(&t.project, "vanished", None).unwrap();
        std::fs::remove_dir_all(&gone.path).unwrap();
        let listed = find(&t.project, "vanished").unwrap().unwrap();
        assert!(listed.missing);
        assert_eq!(listed.branch.as_deref(), Some("vanished"));
        assert!(!is_dirty(&listed).unwrap());
        remove(&t.project, &listed).unwrap();
        assert_eq!(list(&t.project).unwrap().len(), 1);
    }

    #[test]
    fn delete_branch_protects_checked_out_and_unmerged_branches() {
        let t = repo(None);
        let ws = create(&t.project, "work", None).unwrap();
        assert!(
            delete_branch(&t.project, "work", true).is_err(),
            "checked out"
        );
        std::fs::write(ws.path.join("w.txt"), "w").unwrap();
        commit_all(&git2::Repository::open(&ws.path).unwrap(), "w");
        remove(&t.project, &find(&t.project, "work").unwrap().unwrap()).unwrap();
        assert!(!branch_merged(&t.project, "work").unwrap());
        assert!(delete_branch(&t.project, "work", false).is_err());
        delete_branch(&t.project, "work", true).unwrap();
        assert!(
            delete_branch(&t.project, "work", true).is_err(),
            "already gone"
        );
    }

    #[test]
    fn containing_prefers_the_innermost_checkout() {
        let t = repo(None);
        let mut project = t.project.clone();
        project.config.worktree.dir = ".wt".into();
        let ws = create(&project, "inner", None).unwrap();
        assert!(ws.path.starts_with(&t.root));
        let sub = ws.path.join("deep");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(containing(&project, &sub).unwrap().unwrap().name, "inner");
        assert_eq!(containing(&project, &t.root).unwrap().unwrap().name, "main");
        assert!(containing(&project, t.dir.path()).unwrap().is_none());
    }

    #[test]
    fn removing_a_vanished_worktree_never_deletes_a_replacement_directory() {
        let t = repo(None);
        let gone = create(&t.project, "vanished", None).unwrap();
        std::fs::remove_dir_all(&gone.path).unwrap();
        // Something unrelated is put at the same path.
        std::fs::create_dir_all(gone.path.join("src")).unwrap();
        std::fs::write(gone.path.join("src/precious.txt"), "keep").unwrap();
        let listed = find(&t.project, "vanished").unwrap().unwrap();
        assert!(listed.missing, "no checkout is there any more");

        for forced in [false, true] {
            let result = if forced {
                force_remove(&t.project, &listed)
            } else {
                remove(&t.project, &listed)
            };
            let error = result.unwrap_err();
            let refusal = error.downcast_ref::<Protected>().expect("typed refusal");
            assert!(!refusal.forceable, "{refusal}");
            assert!(check_removable(&t.project, &listed, forced).is_err());
            assert_eq!(
                std::fs::read_to_string(gone.path.join("src/precious.txt")).unwrap(),
                "keep"
            );
            assert!(t.root.join(".git/worktrees/vanished").exists());
        }

        // A checkout of another worktree, or a symlinked `.git`, is not it either.
        let other = create(&t.project, "other", None).unwrap();
        std::fs::copy(other.path.join(".git"), gone.path.join(".git")).unwrap();
        assert!(
            remove(&t.project, &listed).is_err(),
            "points at another worktree"
        );
        std::fs::remove_file(gone.path.join(".git")).unwrap();
        std::os::unix::fs::symlink(other.path.join(".git"), gone.path.join(".git")).unwrap();
        assert!(remove(&t.project, &listed).is_err(), "symlinked .git");
        assert!(gone.path.join("src/precious.txt").is_file());

        // With the path really absent only the registration is forgotten.
        std::fs::remove_dir_all(&gone.path).unwrap();
        remove(&t.project, &listed).unwrap();
        assert!(!t.root.join(".git/worktrees/vanished").exists());
        assert!(find(&t.project, "vanished").unwrap().is_none());
        assert!(other.path.join("README.md").is_file());
    }

    #[test]
    fn a_checkout_is_identified_by_its_git_file() {
        let t = repo(None);
        let ws = create(&t.project, "idee", None).unwrap();
        assert!(is_checkout_of(&ws.path, &ws.git_dir));
        assert!(
            !is_checkout_of(&t.root, &ws.git_dir),
            ".git is a directory there"
        );
        assert!(!is_checkout_of(&ws.path.join("missing"), &ws.git_dir));
        // A relative gitdir is resolved against the checkout.
        std::fs::write(
            ws.path.join(".git"),
            "gitdir: ../../shop/.git/worktrees/idee\n",
        )
        .unwrap();
        assert!(is_checkout_of(&ws.path, &ws.git_dir));
    }

    #[test]
    fn concurrent_creations_sharing_a_directory_keep_exactly_one_intact() {
        for round in 0..8 {
            let t = repo(None);
            // Both names map to `<dir>/feature-rN` and Git worktree `feature-rN`.
            let names = [format!("feature/r{round}"), format!("feature-r{round}")];
            let barrier = std::sync::Barrier::new(2);
            let results: Vec<_> = std::thread::scope(|scope| {
                let handles: Vec<_> = names
                    .iter()
                    .map(|name| {
                        let (project, barrier) = (&t.project, &barrier);
                        scope.spawn(move || {
                            barrier.wait();
                            create(project, name, None)
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            let winners: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
            assert_eq!(winners.len(), 1, "round {round}: {results:?}");
            let winner = winners[0];
            let error = results.iter().find_map(|r| r.as_ref().err()).unwrap();
            assert!(error.to_string().contains("already exists"), "{error:#}");

            assert!(
                winner.path.join("README.md").is_file(),
                "winner's files survive"
            );
            let all = list(&t.project).unwrap();
            assert_eq!(all.len(), 2);
            assert!(!all[1].missing);
            assert!(
                t.root
                    .join(".git/worktrees")
                    .join(sanitize(&winner.name))
                    .is_dir()
            );
            let repo = t.project.repo().unwrap();
            let loser = names.iter().find(|n| **n != winner.name).unwrap();
            assert!(
                repo.find_branch(loser, BranchType::Local).is_err(),
                "the loser's branch is not left behind"
            );
            assert!(repo.find_branch(&winner.name, BranchType::Local).is_ok());
        }
    }
    /// What another tool does: add the worktree for an existing branch through
    /// libgit2, then keep working in it.
    fn external_worktree(root: &Path, name: &str, path: &Path) {
        let repo = Repository::open(root).unwrap();
        let branch = repo.find_branch(name, BranchType::Local).unwrap();
        let mut options = WorktreeAddOptions::new();
        options.reference(Some(branch.get()));
        repo.worktree(&sanitize(name), path, Some(&options))
            .unwrap();
        std::fs::write(path.join("theirs.txt"), "uncommitted work").unwrap();
    }

    fn assert_external_checkout_intact(t: &TestRepo, name: &str) {
        let path = path_for(&t.project, name);
        assert_eq!(
            std::fs::read_to_string(path.join("theirs.txt")).unwrap(),
            "uncommitted work"
        );
        assert!(path.join("README.md").is_file(), "checked-out files remain");
        assert!(path.join(".git").is_file());
        assert!(t.root.join(".git/worktrees").join(sanitize(name)).is_dir());
        let listed = find(&t.project, name).unwrap().expect("still registered");
        assert!(!listed.missing);
        assert_eq!(listed.path, path.canonicalize().unwrap());
        assert!(
            t.project
                .repo()
                .unwrap()
                .find_branch(name, BranchType::Local)
                .is_ok(),
            "the branch the other tool checked out is still there"
        );
    }

    #[test]
    fn a_creator_that_wins_the_race_never_loses_its_checkout() {
        // Both variants: the branch exists beforehand, or devflow creates it
        // just before the other tool picks it up.
        for (name, preexisting) in [("race/existing", true), ("race/created", false)] {
            let t = repo(None);
            if preexisting {
                let repo = t.project.repo().unwrap();
                let head = repo.head().unwrap().peel_to_commit().unwrap();
                repo.branch(name, &head, false).unwrap();
            }
            let (root, path) = (t.root.clone(), path_for(&t.project, name));
            testhook::set({
                let name = name.to_string();
                move || external_worktree(&root, &name, &path)
            });
            let error = create(&t.project, name, None).unwrap_err();
            let shown = format!("{error:#}");
            assert!(
                shown.contains(&path_for(&t.project, name).display().to_string())
                    && shown.contains("left"),
                "the error names what was left in place: {shown}"
            );
            assert_external_checkout_intact(&t, name);
            assert_eq!(list(&t.project).unwrap().len(), 2);
        }
    }

    #[test]
    fn a_failure_with_nothing_on_disk_removes_only_the_branch_it_created() {
        use std::os::unix::fs::PermissionsExt;
        let t = repo(None);
        // libgit2 cannot make its administrative directory here.
        let worktrees = t.root.join(".git/worktrees");
        std::fs::create_dir_all(&worktrees).unwrap();
        std::fs::set_permissions(&worktrees, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::write(worktrees.join("probe"), "").is_ok() {
            // Running as root: permissions do not stop us.
            return;
        }
        let path = path_for(&t.project, "doomed");
        let error = create(&t.project, "doomed", None).unwrap_err();
        std::fs::set_permissions(&worktrees, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!format!("{error:#}").contains("left"), "{error:#}");
        assert!(path.symlink_metadata().is_err());
        assert_eq!(std::fs::read_dir(&worktrees).unwrap().count(), 0);
        let repo = t.project.repo().unwrap();
        assert!(
            repo.find_branch("doomed", BranchType::Local).is_err(),
            "the branch devflow created for it is gone"
        );

        // A branch that was already there is never touched.
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("mine", &head, false).unwrap();
        std::fs::set_permissions(&worktrees, std::fs::Permissions::from_mode(0o555)).unwrap();
        create(&t.project, "mine", None).unwrap_err();
        std::fs::set_permissions(&worktrees, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(repo.find_branch("mine", BranchType::Local).is_ok());
    }

    #[test]
    fn directories_that_appear_before_libgit2_runs_are_left_for_the_user() {
        // Another tool made only the checkout directory, or only the
        // registration: libgit2 refuses both, and devflow must not tidy up
        // what it cannot prove it created. Empty or not.
        for (case, in_admin, with_file) in [
            ("a", false, true),
            ("b", false, false),
            ("c", true, true),
            ("d", true, false),
        ] {
            let t = repo(None);
            let name = format!("appears/{case}");
            let (path, admin) = (
                path_for(&t.project, &name),
                t.root.join(".git/worktrees").join(sanitize(&name)),
            );
            let theirs = if in_admin {
                admin.clone()
            } else {
                path.clone()
            };
            testhook::set({
                let theirs = theirs.clone();
                move || {
                    std::fs::create_dir_all(&theirs).unwrap();
                    if with_file {
                        std::fs::write(theirs.join("theirs.txt"), "keep").unwrap();
                    }
                }
            });
            let error = create(&t.project, &name, None).unwrap_err();
            let shown = format!("{error:#}");
            assert!(shown.contains(&theirs.display().to_string()), "{shown}");
            assert!(theirs.is_dir(), "{case}: {shown}");
            assert_eq!(theirs.join("theirs.txt").is_file(), with_file, "{case}");
            // The branch devflow created stays, as the leftover may use it.
            let git = t.project.repo().unwrap();
            assert!(git.find_branch(&name, BranchType::Local).is_ok(), "{case}");
        }
    }

    #[test]
    fn a_real_concurrent_creator_and_devflow_never_destroy_each_other() {
        for round in 0..12 {
            let t = repo(None);
            let name = format!("race/r{round}");
            let git = t.project.repo().unwrap();
            let head = git.head().unwrap().peel_to_commit().unwrap();
            git.branch(&name, &head, false).unwrap();
            let path = path_for(&t.project, &name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let barrier = std::sync::Barrier::new(2);
            let (ours, theirs) = std::thread::scope(|scope| {
                let ours = scope.spawn(|| {
                    barrier.wait();
                    create(&t.project, &name, None)
                });
                let theirs = scope.spawn(|| {
                    barrier.wait();
                    let repo = Repository::open(&t.root).unwrap();
                    let branch = repo.find_branch(&name, BranchType::Local).unwrap();
                    let mut options = WorktreeAddOptions::new();
                    options.reference(Some(branch.get()));
                    let made = repo.worktree(&sanitize(&name), &path, Some(&options));
                    if made.is_ok() {
                        std::fs::write(path.join("theirs.txt"), "uncommitted work").unwrap();
                    }
                    made.map(|_| ())
                });
                (ours.join().unwrap(), theirs.join().unwrap())
            });
            match (&ours, &theirs) {
                (Ok(_), _) => {
                    assert!(path.join("README.md").is_file(), "round {round}");
                }
                (Err(e), Ok(())) => {
                    assert_eq!(
                        std::fs::read_to_string(path.join("theirs.txt")).unwrap(),
                        "uncommitted work",
                        "round {round}: {e:#}"
                    );
                    assert!(path.join("README.md").is_file(), "round {round}");
                    assert!(path.join(".git").is_file(), "round {round}");
                    assert!(!find(&t.project, &name).unwrap().unwrap().missing);
                }
                // Both lost: nothing was deleted on top of whatever half exists.
                (Err(_), Err(_)) => {}
            }
        }
    }
}
