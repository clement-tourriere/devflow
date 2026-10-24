//! Git worktrees through libgit2: listing, creation, removal and branch
//! deletion. devflow never shells out to `git` for these.
use crate::error::Protected;
use crate::project::Project;
use crate::remedy::{Problem, Remedy};
use anyhow::{Context, Result, anyhow, bail, ensure};
use git2::{
    BranchType, ErrorCode, Repository, StatusOptions, WorktreeAddOptions, WorktreeLockStatus,
    WorktreePruneOptions,
};
use serde::Serialize;
use std::path::{Component, Path, PathBuf};

mod evidence;
mod faithful;
#[cfg(test)]
mod gitoracle;
mod prune;
mod rawconfig;
pub use evidence::{Merge, MergeEvidence, merge_evidence};
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

/// Find by branch name, Git's worktree name, the name the workspace was created
/// under, or path.
///
/// A workspace is identified by its receipt, whose `workspace` is the name it
/// was created under, not by the branch that happens to be checked out in it
/// now: after `git switch other` inside `feature/a`, `other` (the branch there
/// now) and `feature/a` both find that checkout. The workspace comes back under
/// its receipt's name, whichever way it was found; `branch` says what the
/// checkout holds.
///
/// Every identity is looked at and the matches are counted by checkout. A name
/// that matches one checkout, however many ways, is that checkout. A name that
/// matches several is refused as a [`Problem`] listing them by path
/// ([`Remedy::UsePath`]): nothing is picked by an order of precedence, because
/// that would make `remove feature/a` act on a checkout the person did not mean
/// (workspace A moved to branch `other` and a workspace B then took A's old
/// branch: `feature/a` is both). A path names exactly one checkout.
pub fn find(project: &Project, name: &str) -> Result<Option<Workspace>> {
    let all = list(project)?;
    // A path names a registered checkout whether or not its directory is
    // still there: one that was deleted cannot be canonicalized, and is found
    // by the path it is registered under (which is how a listing names it).
    let path = canonical(Path::new(name));
    let mut matches: Vec<&Workspace> = Vec::new();
    for w in &all {
        let by_branch = w.name == name || w.branch.as_deref() == Some(name);
        // Git's own worktree name (what `git worktree list` calls the id).
        let by_id = !w.is_default && w.git_dir.file_name().is_some_and(|n| n == name);
        let by_receipt = receipt_name(project, w).is_some_and(|created| created == name);
        let by_path = w.path == path;
        if (by_branch || by_id || by_receipt || by_path)
            && !matches.iter().any(|seen| seen.path == w.path)
        {
            matches.push(w);
        }
    }
    match matches.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(named_by_receipt(project, (*one).clone()))),
        several => Err(ambiguous(project, name, several)),
    }
}

/// `name` matches several checkouts: say which, by path.
fn ambiguous(project: &Project, name: &str, matches: &[&Workspace]) -> anyhow::Error {
    let described: Vec<String> = matches
        .iter()
        .map(|w| {
            let created = receipt_name(project, w).unwrap_or_else(|| w.name.clone());
            let now = match &w.branch {
                Some(branch) if *branch == created => String::new(),
                Some(branch) => format!(", now on branch {branch}"),
                None => ", now on a detached HEAD".to_string(),
            };
            format!("{} (workspace {created}{now})", w.path.display())
        })
        .collect();
    Problem::new(format!(
        "'{name}' names more than one workspace: {}; name the one you mean by its path",
        described.join("; ")
    ))
    .with(Remedy::UsePath {
        name: name.to_string(),
        candidates: matches.iter().map(|w| w.path.clone()).collect(),
    })
    .into_error()
}

/// The name `workspace` was created under, from its receipt: `None` when it
/// has none, when it cannot be read, or when it is another project's.
fn receipt_name(project: &Project, workspace: &Workspace) -> Option<String> {
    crate::receipt::Receipt::load(&workspace.git_dir)
        .ok()
        .flatten()
        .filter(|receipt| receipt.project_id == project.id())
        .map(|receipt| receipt.workspace)
}

/// `workspace` under the name its receipt records.
fn named_by_receipt(project: &Project, mut workspace: Workspace) -> Workspace {
    if let Some(created) = receipt_name(project, &workspace) {
        workspace.name = created;
    }
    workspace
}

/// A name a workspace answers to, with where it is: what a completion or a
/// picker offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    pub name: String,
    pub path: PathBuf,
    /// The directory is gone.
    pub missing: bool,
}

/// Every name the workspaces of `project` go by, from local Git and receipts
/// alone (no server, no Docker): the name each was created under, and the
/// branch it holds now when that is another one. A name that two workspaces
/// answer to is listed once for each: [`find`] refuses it, listing both paths.
pub fn names(project: &Project) -> Result<Vec<Named>> {
    let mut found: Vec<Named> = Vec::new();
    for workspace in list(project)? {
        let created = receipt_name(project, &workspace).unwrap_or_else(|| workspace.name.clone());
        let held = workspace.branch.clone();
        for name in std::iter::once(created).chain(held) {
            if !found
                .iter()
                .any(|seen| seen.name == name && seen.path == workspace.path)
            {
                found.push(Named {
                    name,
                    path: workspace.path.clone(),
                    missing: workspace.missing,
                });
            }
        }
    }
    Ok(found)
}

/// Where a shell that moves from `from` into the workspace `target` should
/// land: the same subdirectory of the workspace it was in (`apps/web`) when
/// `target` has it, else `target`'s root. `source` is the workspace `from`
/// lies in, if any. A subdirectory that leaves `target` by a link is not kept.
pub fn landing(from: &Path, source: Option<&Workspace>, target: &Workspace) -> PathBuf {
    let relative = source
        .and_then(|source| from.strip_prefix(&source.path).ok())
        .filter(|relative| !relative.as_os_str().is_empty());
    let Some(relative) = relative else {
        return target.path.clone();
    };
    let candidate = target.path.join(relative);
    match candidate.canonicalize() {
        Ok(real) if real.is_dir() && real.starts_with(&target.path) => candidate,
        _ => target.path.clone(),
    }
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

/// Longest workspace name devflow accepts, in characters: it becomes a branch,
/// a directory and a Git worktree name, and file systems and tools stop
/// accepting those well before Git does.
pub const MAX_NAME_CHARS: usize = 100;

/// Reject names Git cannot use as a branch.
pub fn validate_name(name: &str) -> Result<()> {
    ensure!(!name.trim().is_empty(), "workspace name is empty");
    let length = name.chars().count();
    if length > MAX_NAME_CHARS {
        let start: String = name.chars().take(24).collect();
        bail!(
            "workspace names are limited to {MAX_NAME_CHARS} characters; '{start}…' has {length}"
        );
    }
    if !git2::Branch::name_is_valid(name).unwrap_or(false) {
        match invalid_reason(name) {
            Some(reason) => bail!("'{name}' is not a valid Git branch name: it {reason}"),
            None => bail!("'{name}' is not a valid Git branch name"),
        }
    }
    Ok(())
}

/// Why Git refuses `name` as a branch, for the common cases.
fn invalid_reason(name: &str) -> Option<&'static str> {
    if name.chars().any(char::is_whitespace) {
        Some("contains a space")
    } else if name.chars().any(char::is_control) {
        Some("contains a control character")
    } else if name.contains("..") {
        Some("contains '..'")
    } else if name.contains("@{") || name == "@" {
        Some("contains '@{'")
    } else if let Some(c) = name.chars().find(|c| "~^:?*[\\".contains(*c)) {
        Some(match c {
            '~' => "contains '~'",
            '^' => "contains '^'",
            ':' => "contains ':'",
            '?' => "contains '?'",
            '*' => "contains '*'",
            '[' => "contains '['",
            _ => "contains a backslash",
        })
    } else if name.starts_with('-') {
        Some("starts with '-'")
    } else if name.starts_with('/') || name.ends_with('/') || name.contains("//") {
        Some("has an empty part between slashes")
    } else if name.ends_with('.') {
        Some("ends with '.'")
    } else if name.split('/').any(|part| part.ends_with(".lock")) {
        Some("has a part ending in '.lock'")
    } else if name.split('/').any(|part| part.starts_with('.')) {
        Some("has a part starting with '.'")
    } else {
        None
    }
}

/// How [`create_with`] is told where the new branch starts.
#[derive(Debug, Clone, Copy, Default)]
pub struct CreateOptions<'a> {
    /// The workspace whose HEAD a new branch starts from when there is no
    /// `base` (default: the remote branch of that name when there is one,
    /// else the default workspace's HEAD).
    pub from: Option<&'a Workspace>,
    /// Any commit-ish a new branch starts from: a branch, a tag, a commit, a
    /// remote-tracking branch (`origin/x`, which a branch `x` then tracks).
    pub base: Option<&'a str>,
}

/// What [`create_with`] made, and where its branch came from.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Created {
    pub workspace: Workspace,
    pub origin: BranchOrigin,
    /// Things worth saying that did not stop the creation.
    pub warnings: Vec<String>,
}

/// Where a workspace's branch started and what it tracks.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BranchOrigin {
    pub branch: String,
    /// The full id of the commit the branch pointed at when the worktree was
    /// made.
    pub commit: String,
    /// The branch was made by this call (otherwise it already existed).
    pub created: bool,
    pub start: BranchStart,
    /// The remote-tracking branch the local one tracks (`origin/x`), when it
    /// tracks one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracks: Option<String>,
}

/// What a new branch was started from.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BranchStart {
    /// The local branch existed already.
    ExistingBranch,
    /// The one remote-tracking branch of that name (`origin/x`).
    RemoteBranch { name: String },
    /// The commit-ish the caller asked for, as they wrote it.
    Base { spec: String },
    /// The HEAD of a workspace the caller named.
    Workspace { name: String },
    /// The HEAD of the default workspace.
    DefaultWorkspace,
}

impl BranchOrigin {
    /// The sentence a person reads: `branch x at abc1234, created from
    /// origin/x, tracking origin/x`.
    pub fn describe(&self) -> String {
        let short: String = self.commit.chars().take(7).collect();
        let from = match &self.start {
            BranchStart::ExistingBranch => String::new(),
            BranchStart::RemoteBranch { name } => format!(", created from {name}"),
            BranchStart::Base { spec } => format!(", created from {spec}"),
            BranchStart::Workspace { name } => format!(", created from {name}"),
            BranchStart::DefaultWorkspace => ", created from the default workspace".to_string(),
        };
        let tracks = self
            .tracks
            .as_ref()
            .map(|t| format!(", tracking {t}"))
            .unwrap_or_default();
        format!("branch {} at {short}{from}{tracks}", self.branch)
    }
}

/// Add a worktree for branch `name`. Creates the branch from `from`'s HEAD
/// (default: see [`CreateOptions`]) when it does not exist yet.
pub fn create(project: &Project, name: &str, from: Option<&Workspace>) -> Result<Workspace> {
    create_with(project, name, &CreateOptions { from, base: None }).map(|c| c.workspace)
}

/// Add a worktree for branch `name`.
///
/// A branch that exists is checked out as it is (a `base` must then name its
/// commit). Otherwise it is created, at the first of: `base`; the HEAD of
/// `from`; the one remote-tracking branch `refs/remotes/<remote>/<name>`
/// (several: the remote `checkout.defaultRemote` names, else an ambiguity
/// error), which the new branch then tracks as `git worktree add` sets it up;
/// the default workspace's HEAD. devflow never fetches.
///
/// Creations are serialized per repository: two names can map to the same
/// directory and Git worktree name (`feature/a`, `feature-a`), and the checks
/// below are only meaningful while no other creation runs. The lock only
/// coordinates devflow, though: another tool may create the same worktree
/// between the checks and the libgit2 call. A failed `repo.worktree()` therefore
/// never deletes anything, because neither the error code nor the earlier
/// absence checks prove who made what is on disk. Whatever it left is reported
/// by path for the user to inspect.
pub fn create_with(project: &Project, name: &str, options: &CreateOptions) -> Result<Created> {
    validate_name(name)?;
    let lock = project.git_common_dir.join("devflow").join("create.lock");
    crate::fsutil::with_lock(&lock, || create_locked(project, name, options))
}

/// Where the new branch starts, decided before anything is created.
struct Start {
    commit: git2::Oid,
    kind: BranchStart,
    /// `(remote, remote-tracking branch, ref the remote calls it)` the new
    /// branch tracks: the last is `branch.<name>.merge`, which the remote's
    /// fetch refspec decides.
    tracks: Option<(String, String, String)>,
}

fn create_locked(project: &Project, name: &str, options: &CreateOptions) -> Result<Created> {
    let repo = project.repo()?;
    let path = path_for(project, name);
    let admin_name = sanitize(name);
    let admin = repo.commondir().join("worktrees").join(&admin_name);
    let others = list(project)?;
    if path.symlink_metadata().is_ok() {
        return Err(directory_in_use(project, name, &path, &others));
    }
    if repo.find_worktree(&admin_name).is_ok() || admin.exists() {
        let owner = others
            .iter()
            .find(|w| !w.missing && w.git_dir == canonical(&admin));
        return Err(match owner {
            Some(owner) => Problem::new(format!(
                "the Git worktree name '{admin_name}' already exists: workspace '{}' uses it",
                owner.name
            ))
            .with(Remedy::SwitchTo {
                root: project.root.clone(),
                name: owner.name.clone(),
            }),
            None => Problem::new(format!(
                "a Git worktree named '{admin_name}' is already registered; if its directory is \
                 gone, forget the registration"
            ))
            .with(Remedy::PruneWorktrees {
                root: project.root.clone(),
            }),
        }
        .into_error());
    }
    if let Some(other) = others.iter().find(|w| w.branch.as_deref() == Some(name)) {
        bail!(
            "branch '{name}' is already checked out at {}",
            other.path.display()
        );
    }

    // Everything that can refuse comes before the first effect.
    let mut warnings = Vec::new();
    let existing = match repo.find_branch(name, BranchType::Local) {
        Ok(branch) => Some(branch),
        Err(e) if e.code() == ErrorCode::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let base = options
        .base
        .map(|spec| resolve_base(&repo, spec))
        .transpose()?;
    let (commit, start) = match &existing {
        Some(branch) => {
            let tip = branch.get().peel_to_commit()?.id();
            if let Some(base) = &base
                && base.commit != tip
            {
                return Err(base_mismatch(name, tip, base));
            }
            (tip, None)
        }
        None => {
            check_namespace(&repo, name)?;
            let start = choose_start(project, &repo, name, options, base, &mut warnings)?;
            (start.commit, Some(start))
        }
    };
    warnings.extend(faithful::check(
        &repo,
        &repo.find_commit(commit)?,
        &project.root,
    )?);

    let tracks = match (&start, &existing) {
        (Some(start), _) => start
            .tracks
            .as_ref()
            .map(|(remote, branch, _)| (remote.clone(), branch.clone())),
        (None, Some(branch)) => branch
            .upstream()
            .ok()
            .and_then(|u| u.name().ok().flatten().map(str::to_string))
            .and_then(|full| {
                let (remote, _) = full.split_once('/')?;
                Some((remote.to_string(), full.clone()))
            }),
        (None, None) => None,
    };
    let tracking_name = match (&start, &tracks) {
        (Some(_), Some((remote, branch))) => Some(format!("{remote}/{branch}")),
        (None, Some((_, full))) => Some(full.clone()),
        _ => None,
    };

    // The commit of a branch this call created (exclusively: `force` is off).
    let created_branch = match &start {
        None => None,
        Some(start) => {
            repo.branch(name, &repo.find_commit(start.commit)?, false)
                .map_err(|e| anyhow!("cannot create branch '{name}': {}", e.message()))?;
            if let Some((remote, _, merge)) = &start.tracks
                && let Err(error) = track(&repo, name, remote, merge)
            {
                if let Ok(mut made) = repo.find_branch(name, BranchType::Local) {
                    let _ = made.delete();
                }
                return Err(error);
            }
            Some(start.commit)
        }
    };

    let added = (|| -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let branch = repo.find_branch(name, BranchType::Local)?.into_reference();
        let mut worktree_options = WorktreeAddOptions::new();
        worktree_options.reference(Some(&branch));
        #[cfg(test)]
        testhook::before_add();
        // An existing branch is checked out wherever it points now: if it
        // moved since the check above, the commit about to be checked out is
        // judged again.
        if existing.is_some() {
            let now = repo
                .find_branch(name, BranchType::Local)?
                .get()
                .peel_to_commit()?;
            if now.id() != commit {
                faithful::check(&repo, &now, &project.root)?;
            }
        }
        // libgit2 creates the checkout and the administrative directory
        // exclusively: it never adopts a directory that already exists.
        repo.worktree(&admin_name, &path, Some(&worktree_options))
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
                anyhow!(
                    "cannot add worktree at {}: {}{note}",
                    path.display(),
                    e.message()
                )
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
    let workspace = find(project, name)?.context("the new worktree is not listed by Git")?;
    Ok(Created {
        workspace,
        origin: BranchOrigin {
            branch: name.to_string(),
            commit: commit.to_string(),
            created: created_branch.is_some(),
            start: start.map(|s| s.kind).unwrap_or(BranchStart::ExistingBranch),
            tracks: tracking_name,
        },
        warnings,
    })
}

/// A starting point that is not where the existing branch is.
fn base_mismatch(name: &str, tip: git2::Oid, base: &Base) -> anyhow::Error {
    Problem::new(format!(
        "branch '{name}' already exists at {}, not at {} ({}); a starting point only applies to \
         a branch that is created",
        short_id(tip),
        short_id(base.commit),
        base.spec
    ))
    .into_error()
}

/// Check a starting point against the branch `name` that exists already (a
/// workspace being resumed): it must resolve, and to the branch's commit.
pub fn check_base(project: &Project, name: &str, spec: &str) -> Result<()> {
    let repo = project.repo()?;
    let base = resolve_base(&repo, spec)?;
    let tip = repo
        .find_branch(name, BranchType::Local)
        .with_context(|| format!("no local branch '{name}'"))?
        .get()
        .peel_to_commit()?
        .id();
    if base.commit != tip {
        return Err(base_mismatch(name, tip, &base));
    }
    Ok(())
}

/// A directory that is taken: say by whom when a workspace has it.
fn directory_in_use(
    project: &Project,
    name: &str,
    path: &Path,
    others: &[Workspace],
) -> anyhow::Error {
    let taken = canonical(path);
    match others.iter().find(|w| w.path == taken) {
        Some(owner) => {
            let because = if sanitize(&owner.name) == sanitize(name) {
                format!(
                    "'{name}' and '{}' both become the directory name '{}'",
                    owner.name,
                    sanitize(name)
                )
            } else {
                format!("workspace '{}' lives there", owner.name)
            };
            Problem::new(format!(
                "{} already exists: {because}; choose another name",
                path.display()
            ))
            .with(Remedy::SwitchTo {
                root: project.root.clone(),
                name: owner.name.clone(),
            })
            .into_error()
        }
        None => Problem::new(format!(
            "{} already exists; choose another name or remove it",
            path.display()
        ))
        .with(Remedy::Reveal {
            path: path.to_path_buf(),
        })
        .into_error(),
    }
}

/// Git stores branches as files: `feature` and `feature/x` cannot both exist.
fn check_namespace(repo: &Repository, name: &str) -> Result<()> {
    let parts: Vec<&str> = name.split('/').collect();
    let mut prefix = String::new();
    for part in &parts[..parts.len() - 1] {
        prefix.push_str(part);
        if repo.find_reference(&format!("refs/heads/{prefix}")).is_ok() {
            return Err(Problem::new(format!(
                "cannot create branch '{name}': the branch '{prefix}' exists, and Git cannot hold \
                 both a branch and a group of branches under the same name; choose another name"
            ))
            .into_error());
        }
        prefix.push('/');
    }
    if let Ok(mut inside) = repo.references_glob(&format!("refs/heads/{name}/*"))
        && let Some(Ok(first)) = inside.next()
    {
        let other = first
            .name()
            .ok()
            .and_then(|n| n.strip_prefix("refs/heads/"))
            .unwrap_or("another branch");
        return Err(Problem::new(format!(
            "cannot create branch '{name}': the branch '{other}' exists, and Git cannot hold both \
             a branch and a group of branches under the same name; choose another name"
        ))
        .into_error());
    }
    Ok(())
}

/// A resolved `--base`.
struct Base {
    spec: String,
    commit: git2::Oid,
    /// `(remote, branch)` when `spec` names a remote-tracking branch.
    remote: Option<(String, String)>,
}

fn resolve_base(repo: &Repository, spec: &str) -> Result<Base> {
    let unknown = || {
        Problem::new(format!(
            "cannot start from '{spec}': it is not a branch, a tag or a commit of this repository"
        ))
        .into_error()
    };
    let spec = spec.trim();
    ensure!(!spec.is_empty(), "the starting point is empty");
    let reference = repo
        .resolve_reference_from_short_name(spec)
        .ok()
        .or_else(|| repo.find_reference(spec).ok());
    let commit = match &reference {
        Some(reference) => reference.peel_to_commit().map_err(|_| unknown())?,
        None => repo
            .revparse_single(spec)
            .map_err(|_| unknown())?
            .peel_to_commit()
            .map_err(|_| {
                Problem::new(format!("cannot start from '{spec}': it is not a commit")).into_error()
            })?,
    };
    let remote = reference
        .as_ref()
        .and_then(|r| r.name().ok())
        .and_then(|full| full.strip_prefix("refs/remotes/"))
        .and_then(|rest| split_remote_branch(repo, rest));
    Ok(Base {
        spec: spec.to_string(),
        commit: commit.id(),
        remote,
    })
}

/// `origin/feature/x` as `("origin", "feature/x")`, preferring a configured
/// remote's name (remote names may contain slashes).
fn split_remote_branch(repo: &Repository, rest: &str) -> Option<(String, String)> {
    let configured = repo.remotes().ok()?;
    let remote = configured
        .iter()
        .flatten()
        .flatten()
        .filter(|r| rest.strip_prefix(*r).is_some_and(|b| b.starts_with('/')))
        .max_by_key(|r| r.len())
        .map(str::to_string)
        .or_else(|| rest.split_once('/').map(|(r, _)| r.to_string()))?;
    let branch = rest.strip_prefix(&remote)?.strip_prefix('/')?.to_string();
    (!branch.is_empty() && branch != "HEAD").then_some((remote, branch))
}

fn choose_start(
    project: &Project,
    repo: &Repository,
    name: &str,
    options: &CreateOptions,
    base: Option<Base>,
    warnings: &mut Vec<String>,
) -> Result<Start> {
    if let Some(base) = base {
        // `--base origin/x` for `x` tracks it.
        let tracks = base
            .remote
            .filter(|(_, branch)| branch == name)
            .and_then(|(remote, branch)| upstream(repo, remote, branch, warnings));
        return Ok(Start {
            commit: base.commit,
            kind: BranchStart::Base { spec: base.spec },
            tracks,
        });
    }
    let candidates = remote_branches(repo, name);
    if let Some(from) = options.from {
        ensure!(
            !from.missing,
            "workspace '{}' is missing on disk",
            from.name
        );
        if !candidates.is_empty() {
            let listed: Vec<String> = candidates
                .iter()
                .map(|(r, _)| format!("{r}/{name}"))
                .collect();
            warnings.push(format!(
                "{} exists but was not used: the branch starts from the HEAD of workspace '{}', \
                 which was named as the code source",
                listed.join(" and "),
                from.name
            ));
        }
        let commit = head_commit(&Repository::open(&from.path)?, &from.name)?;
        return Ok(Start {
            commit,
            kind: BranchStart::Workspace {
                name: from.name.clone(),
            },
            tracks: None,
        });
    }
    let chosen = match candidates.len() {
        0 => None,
        1 => candidates.into_iter().next(),
        _ => {
            let default = repo
                .config()
                .ok()
                .and_then(|c| c.get_string("checkout.defaultRemote").ok());
            let preferred = default
                .as_deref()
                .and_then(|d| candidates.iter().find(|(remote, _)| remote == d).cloned());
            match preferred {
                Some(found) => Some(found),
                None => {
                    let listed: Vec<String> = candidates
                        .iter()
                        .map(|(r, _)| format!("{r}/{name}"))
                        .collect();
                    return Err(Problem::new(format!(
                        "branch '{name}' exists on several remotes ({}) and none is the default; \
                         name the one to start from",
                        listed.join(", ")
                    ))
                    .with(Remedy::ChooseBase {
                        root: project.root.clone(),
                        name: name.to_string(),
                        candidates: listed,
                    })
                    .into_error());
                }
            }
        }
    };
    match chosen {
        Some((remote, commit)) => Ok(Start {
            commit,
            kind: BranchStart::RemoteBranch {
                name: format!("{remote}/{name}"),
            },
            tracks: upstream(repo, remote, name.to_string(), warnings),
        }),
        None => Ok(Start {
            commit: head_commit(repo, "the default workspace")?,
            kind: BranchStart::DefaultWorkspace,
            tracks: None,
        }),
    }
}

/// The remote-tracking branches named `name` (`refs/remotes/<remote>/<name>`)
/// with the commits they point at, sorted by remote.
fn remote_branches(repo: &Repository, name: &str) -> Vec<(String, git2::Oid)> {
    if name == "HEAD" {
        return Vec::new();
    }
    let Ok(refs) = repo.references_glob(&format!("refs/remotes/*/{name}")) else {
        return Vec::new();
    };
    let mut found: Vec<(String, git2::Oid)> = refs
        .flatten()
        .filter_map(|reference| {
            let full = reference
                .name()
                .ok()?
                .strip_prefix("refs/remotes/")?
                .to_string();
            let (remote, branch) = split_remote_branch(repo, &full)?;
            if branch != name {
                return None;
            }
            Some((remote, reference.peel_to_commit().ok()?.id()))
        })
        .collect();
    found.sort();
    found.dedup();
    found
}

/// What `branch.<name>.merge` must be for a branch tracking the remote-tracking
/// branch `refs/remotes/<remote>/<branch>`: the ref the remote calls it, found
/// by reversing the remote's fetch refspec (`+refs/heads/review/*:refs/remotes/origin/*`
/// makes `origin/topic` the remote's `refs/heads/review/topic`). When the
/// remote is not configured or no refspec covers the ref, nothing is derived:
/// the upstream is not set, and a warning says so.
fn upstream(
    repo: &Repository,
    remote: String,
    branch: String,
    warnings: &mut Vec<String>,
) -> Option<(String, String, String)> {
    let tracking = format!("refs/remotes/{remote}/{branch}");
    let merge = repo.find_remote(&remote).ok().and_then(|configured| {
        configured.refspecs().find_map(|spec| {
            if spec.direction() != git2::Direction::Fetch || !spec.dst_matches(&tracking) {
                return None;
            }
            spec.rtransform(&tracking)
                .ok()?
                .as_str()
                .ok()
                .map(str::to_string)
        })
    });
    if merge.is_none() {
        warnings.push(format!(
            "the new branch does not track {remote}/{branch}: remote '{remote}' has no fetch \
             refspec that maps it back to a branch of the remote, so the upstream cannot be \
             derived"
        ));
    }
    merge.map(|merge| (remote, branch, merge))
}

/// Set the upstream of local branch `name` the way `git worktree add` does:
/// `branch.<name>.remote` and `branch.<name>.merge` (the ref on the remote).
fn track(repo: &Repository, name: &str, remote: &str, merge: &str) -> Result<()> {
    let mut config = repo
        .config()?
        .open_level(git2::ConfigLevel::Local)
        .context("cannot open the repository's configuration")?;
    config
        .set_str(&format!("branch.{name}.remote"), remote)
        .and_then(|()| config.set_str(&format!("branch.{name}.merge"), merge))
        .map_err(|e| anyhow!("cannot record the upstream of '{name}': {}", e.message()))
}

fn short_id(id: git2::Oid) -> String {
    id.to_string().chars().take(7).collect()
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
    fn landing_keeps_the_subdirectory_when_the_target_has_it() {
        let t = repo(None);
        std::fs::create_dir_all(t.root.join("apps/web")).unwrap();
        std::fs::write(t.root.join("apps/web/index.js"), "x\n").unwrap();
        let main = list(&t.project).unwrap().remove(0);
        let other = create(&t.project, "feature/a", None).unwrap();
        // The target has no apps/web (it was made from a commit without): the root.
        let from = t.root.join("apps/web");
        assert_eq!(landing(&from, Some(&main), &other), other.path);
        // Once it has, the same subdirectory.
        std::fs::create_dir_all(other.path.join("apps/web")).unwrap();
        assert_eq!(
            landing(&from, Some(&main), &other),
            other.path.join("apps/web")
        );
        // From a workspace's root, or from outside any workspace: the root.
        assert_eq!(landing(&t.root, Some(&main), &other), other.path);
        assert_eq!(landing(&from, None, &other), other.path);
        // A subdirectory that is a link out of the target is not followed.
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(other.path.join("libs")).unwrap();
            std::fs::create_dir_all(t.root.join("libs/deep")).unwrap();
            std::fs::remove_dir_all(other.path.join("libs")).unwrap();
            std::os::unix::fs::symlink(outside.path(), other.path.join("libs")).unwrap();
            std::fs::create_dir_all(outside.path().join("deep")).unwrap();
            assert_eq!(
                landing(&t.root.join("libs/deep"), Some(&main), &other),
                other.path
            );
        }
    }

    #[test]
    fn names_list_what_each_workspace_answers_to() {
        let t = repo(None);
        let ws = create(&t.project, "feature/a", None).unwrap();
        let named = names(&t.project).unwrap();
        let listed: Vec<&str> = named.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(listed, ["main", "feature/a"]);
        assert_eq!(named[1].path, ws.path);
        assert!(named.iter().all(|n| !n.missing));
        // The branch the checkout holds now is a name too, once it differs
        // from the one the workspace was created under (a receipt records it).
        let mut receipt = crate::receipt::Receipt::new(t.project.id(), "created-as", None);
        receipt.save(&ws.git_dir).unwrap();
        let listed: Vec<String> = names(&t.project)
            .unwrap()
            .into_iter()
            .map(|n| n.name)
            .collect();
        assert_eq!(listed, ["main", "created-as", "feature/a"]);
        receipt.workspace = "feature/a".into();
        receipt.save(&ws.git_dir).unwrap();
        assert_eq!(names(&t.project).unwrap().len(), 2);
    }

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
    // --- where a new branch starts -----------------------------------------

    /// A fresh clone of `t`'s repository: `remote` is configured and holds a
    /// colleague's branch `names` (each with its own commit and file).
    fn with_remote_branches(t: &TestRepo, remote: &str, names: &[&str]) -> Vec<git2::Oid> {
        let repo = t.project.repo().unwrap();
        repo.remote(remote, "https://example.invalid/shop.git")
            .unwrap();
        let base = repo.head().unwrap().peel_to_commit().unwrap();
        names
            .iter()
            .map(|name| {
                let tree = base.tree().unwrap();
                let sig = signature();
                let id = repo
                    .commit(
                        None,
                        &sig,
                        &sig,
                        &format!("colleague work on {name}"),
                        &tree,
                        &[&base],
                    )
                    .unwrap();
                repo.reference(
                    &format!("refs/remotes/{remote}/{name}"),
                    id,
                    true,
                    "test fetch",
                )
                .unwrap();
                id
            })
            .collect()
    }

    fn upstream_of(t: &TestRepo, branch: &str) -> (Option<String>, Option<String>) {
        let config = t.project.repo().unwrap().config().unwrap();
        (
            config.get_string(&format!("branch.{branch}.remote")).ok(),
            config.get_string(&format!("branch.{branch}.merge")).ok(),
        )
    }

    #[test]
    fn a_branch_only_on_a_remote_is_created_there_and_tracks_it() {
        let t = repo(None);
        let ids = with_remote_branches(&t, "origin", &["feature-x", "feature/y"]);
        let created = create_with(&t.project, "feature-x", &CreateOptions::default()).unwrap();
        assert_eq!(created.origin.commit, ids[0].to_string());
        assert!(created.origin.created);
        assert_eq!(
            created.origin.start,
            BranchStart::RemoteBranch {
                name: "origin/feature-x".into()
            }
        );
        assert_eq!(created.origin.tracks.as_deref(), Some("origin/feature-x"));
        assert!(
            created
                .origin
                .describe()
                .contains("tracking origin/feature-x")
        );
        assert_ne!(
            created.workspace.head,
            list(&t.project).unwrap()[0].head,
            "not on the default workspace's commit"
        );
        assert_eq!(
            upstream_of(&t, "feature-x"),
            (Some("origin".into()), Some("refs/heads/feature-x".into()))
        );
        let repo = t.project.repo().unwrap();
        let branch = repo.find_branch("feature-x", BranchType::Local).unwrap();
        assert_eq!(
            branch.upstream().unwrap().name().unwrap(),
            Some("origin/feature-x")
        );
        // A name with a slash is found as well.
        let created = create_with(&t.project, "feature/y", &CreateOptions::default()).unwrap();
        assert_eq!(created.origin.commit, ids[1].to_string());
        assert_eq!(created.origin.tracks.as_deref(), Some("origin/feature/y"));
        // The checkout is that commit, not main's.
        let opened = Repository::open(&created.workspace.path).unwrap();
        assert_eq!(
            opened.head().unwrap().peel_to_commit().unwrap().id(),
            ids[1]
        );
    }

    #[test]
    fn the_upstream_follows_the_remotes_fetch_refspec() {
        let t = repo(None);
        let git = t.project.repo().unwrap();
        // A remote that maps only `review/*` of its branches, as `origin/*`.
        git.remote_with_fetch(
            "origin",
            "https://example.invalid/shop.git",
            "+refs/heads/review/*:refs/remotes/origin/*",
        )
        .unwrap();
        let head = git.head().unwrap().peel_to_commit().unwrap();
        git.reference("refs/remotes/origin/topic", head.id(), true, "fetch")
            .unwrap();
        let created = create_with(&t.project, "topic", &CreateOptions::default()).unwrap();
        assert_eq!(created.origin.tracks.as_deref(), Some("origin/topic"));
        assert_eq!(
            upstream_of(&t, "topic"),
            (
                Some("origin".into()),
                Some("refs/heads/review/topic".into())
            )
        );
        // `--base origin/topic` for `topic` derives it the same way.
        git.reference("refs/remotes/origin/other", head.id(), true, "fetch")
            .unwrap();
        let created = create_with(
            &t.project,
            "other",
            &CreateOptions {
                from: None,
                base: Some("origin/other"),
            },
        )
        .unwrap();
        assert_eq!(created.origin.tracks.as_deref(), Some("origin/other"));
        assert_eq!(
            upstream_of(&t, "other"),
            (
                Some("origin".into()),
                Some("refs/heads/review/other".into())
            )
        );
    }

    #[test]
    fn an_upstream_that_cannot_be_derived_is_not_set_and_said() {
        // The tracking ref exists, but its remote is not configured (it was
        // deleted) or no refspec of it covers the ref.
        let t = repo(None);
        let git = t.project.repo().unwrap();
        let head = git.head().unwrap().peel_to_commit().unwrap();
        git.reference("refs/remotes/gone/topic", head.id(), true, "fetch")
            .unwrap();
        let created = create_with(&t.project, "topic", &CreateOptions::default()).unwrap();
        assert_eq!(created.origin.tracks, None);
        assert_eq!(upstream_of(&t, "topic"), (None, None));
        assert!(
            created
                .warnings
                .iter()
                .any(|w| w.contains("gone/topic") && w.contains("cannot be derived")),
            "{:?}",
            created.warnings
        );
        // The branch is still started at that commit.
        assert_eq!(created.origin.commit, head.id().to_string());

        git.remote_with_fetch(
            "odd",
            "https://example.invalid/odd.git",
            "+refs/heads/main:refs/remotes/odd/main",
        )
        .unwrap();
        git.reference("refs/remotes/odd/side", head.id(), true, "fetch")
            .unwrap();
        let created = create_with(&t.project, "side", &CreateOptions::default()).unwrap();
        assert_eq!(created.origin.tracks, None);
        assert_eq!(upstream_of(&t, "side"), (None, None));
    }

    #[test]
    fn without_a_remote_branch_the_default_head_is_used() {
        let t = repo(None);
        let created = create_with(&t.project, "plain", &CreateOptions::default()).unwrap();
        assert_eq!(created.origin.start, BranchStart::DefaultWorkspace);
        assert_eq!(created.origin.tracks, None);
        assert_eq!(upstream_of(&t, "plain"), (None, None));
        assert!(created.warnings.is_empty());
    }

    #[test]
    fn several_remotes_need_the_default_remote_or_a_choice() {
        let t = repo(None);
        let first = with_remote_branches(&t, "origin", &["shared"]);
        let second = with_remote_branches_on(&t, "fork", "shared");
        let error = create_with(&t.project, "shared", &CreateOptions::default()).unwrap_err();
        let problem = error.downcast_ref::<Problem>().expect("typed");
        assert!(
            problem.message.contains("origin/shared") && problem.message.contains("fork/shared"),
            "{problem:?}"
        );
        assert!(
            problem.remedies.iter().any(
                |r| matches!(r, Remedy::ChooseBase { candidates, .. } if candidates.len() == 2)
            ),
            "{problem:?}"
        );
        // Nothing was created.
        let repo = t.project.repo().unwrap();
        assert!(repo.find_branch("shared", BranchType::Local).is_err());
        assert_eq!(list(&t.project).unwrap().len(), 1);

        // `checkout.defaultRemote` decides.
        repo.config()
            .unwrap()
            .set_str("checkout.defaultRemote", "fork")
            .unwrap();
        let created = create_with(&t.project, "shared", &CreateOptions::default()).unwrap();
        assert_eq!(created.origin.commit, second.to_string());
        assert_eq!(created.origin.tracks.as_deref(), Some("fork/shared"));
        assert_ne!(first[0], second);

        // A default remote that does not have the branch decides nothing.
        let t = repo_with_two_remotes();
        t.project
            .repo()
            .unwrap()
            .config()
            .unwrap()
            .set_str("checkout.defaultRemote", "elsewhere")
            .unwrap();
        assert!(create_with(&t.project, "shared", &CreateOptions::default()).is_err());
    }

    fn repo_with_two_remotes() -> TestRepo {
        let t = repo(None);
        with_remote_branches(&t, "origin", &["shared"]);
        with_remote_branches_on(&t, "fork", "shared");
        t
    }

    fn with_remote_branches_on(t: &TestRepo, remote: &str, name: &str) -> git2::Oid {
        let repo = t.project.repo().unwrap();
        repo.remote(remote, "https://example.invalid/fork.git")
            .unwrap();
        let base = repo.head().unwrap().peel_to_commit().unwrap();
        let sig = signature();
        let id = repo
            .commit(
                None,
                &sig,
                &sig,
                &format!("{remote} {name}"),
                &base.tree().unwrap(),
                &[&base],
            )
            .unwrap();
        repo.reference(&format!("refs/remotes/{remote}/{name}"), id, true, "test")
            .unwrap();
        id
    }

    #[test]
    fn a_named_code_source_wins_over_a_remote_branch_and_says_so() {
        let t = repo(None);
        with_remote_branches(&t, "origin", &["feature-x"]);
        let default = list(&t.project).unwrap().remove(0);
        let created = create_with(
            &t.project,
            "feature-x",
            &CreateOptions {
                from: Some(&default),
                base: None,
            },
        )
        .unwrap();
        assert_eq!(
            created.origin.start,
            BranchStart::Workspace {
                name: "main".into()
            }
        );
        assert_eq!(created.origin.tracks, None);
        assert!(
            created
                .warnings
                .iter()
                .any(|w| w.contains("origin/feature-x")),
            "{:?}",
            created.warnings
        );
    }

    #[test]
    fn a_base_is_any_commit_ish_resolved_once_and_reported() {
        let t = repo(None);
        let repo = t.project.repo().unwrap();
        let first = repo.head().unwrap().peel_to_commit().unwrap();
        // A second commit on main, so HEAD~1 is different from HEAD.
        std::fs::write(t.root.join("more.txt"), "more").unwrap();
        commit_all(&repo, "second");
        repo.tag_lightweight("v1", first.as_object(), false)
            .unwrap();

        for spec in [
            "v1",
            "HEAD~1",
            &first.id().to_string(),
            &first.id().to_string()[..10],
        ] {
            let name = format!("from-{}", sanitize(spec));
            let created = create_with(
                &t.project,
                &name,
                &CreateOptions {
                    from: None,
                    base: Some(spec),
                },
            )
            .unwrap();
            assert_eq!(created.origin.commit, first.id().to_string(), "{spec}");
            assert_eq!(
                created.origin.start,
                BranchStart::Base { spec: spec.into() }
            );
            assert_eq!(created.origin.tracks, None);
            assert!(!created.workspace.path.join("more.txt").exists(), "{spec}");
        }
        // A branch name works too, and the HEAD of --from is not consulted.
        let created = create_with(
            &t.project,
            "from-main",
            &CreateOptions {
                from: None,
                base: Some("main"),
            },
        )
        .unwrap();
        assert!(created.workspace.path.join("more.txt").is_file());

        let error = create_with(
            &t.project,
            "nowhere",
            &CreateOptions {
                from: None,
                base: Some("no-such-ref"),
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("no-such-ref"), "{error}");
        assert!(!format!("{error:#}").contains("class="), "{error:#}");
        assert!(repo.find_branch("nowhere", BranchType::Local).is_err());
    }

    #[test]
    fn a_remote_base_with_the_same_name_is_tracked() {
        let t = repo(None);
        let ids = with_remote_branches(&t, "origin", &["x", "other"]);
        let created = create_with(
            &t.project,
            "x",
            &CreateOptions {
                from: None,
                base: Some("origin/x"),
            },
        )
        .unwrap();
        assert_eq!(created.origin.commit, ids[0].to_string());
        assert_eq!(created.origin.tracks.as_deref(), Some("origin/x"));
        assert_eq!(
            upstream_of(&t, "x"),
            (Some("origin".into()), Some("refs/heads/x".into()))
        );
        // Another name starts there without tracking it.
        let created = create_with(
            &t.project,
            "mine",
            &CreateOptions {
                from: None,
                base: Some("origin/other"),
            },
        )
        .unwrap();
        assert_eq!(created.origin.commit, ids[1].to_string());
        assert_eq!(created.origin.tracks, None);
        assert_eq!(upstream_of(&t, "mine"), (None, None));
    }

    #[test]
    fn a_base_only_applies_to_a_new_branch() {
        let t = repo(None);
        let repo = t.project.repo().unwrap();
        let first = repo.head().unwrap().peel_to_commit().unwrap();
        std::fs::write(t.root.join("more.txt"), "more").unwrap();
        commit_all(&repo, "second");
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("exists", &head, false).unwrap();

        let first_id = first.id().to_string();
        let error = create_with(
            &t.project,
            "exists",
            &CreateOptions {
                from: None,
                base: Some(&first_id),
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error}");
        assert_eq!(list(&t.project).unwrap().len(), 1);
        // The same commit is accepted and reported as an existing branch.
        let created = create_with(
            &t.project,
            "exists",
            &CreateOptions {
                from: None,
                base: Some("main"),
            },
        )
        .unwrap();
        assert!(!created.origin.created);
        assert_eq!(created.origin.start, BranchStart::ExistingBranch);
    }

    // --- which commit is judged ---------------------------------------------

    /// `main` gets a commit with `.gitattributes` setting the LFS filter and
    /// the returned id is the commit before it (without LFS).
    fn lfs_on_main(t: &TestRepo) -> git2::Oid {
        let repo = t.project.repo().unwrap();
        let before = repo.head().unwrap().peel_to_commit().unwrap().id();
        std::fs::write(t.root.join(".gitattributes"), "*.psd filter=lfs\n").unwrap();
        commit_all(&repo, "lfs");
        before
    }

    #[test]
    fn a_reused_local_branch_is_judged_by_its_own_commit() {
        // The default workspace is clean, the branch carries LFS: refused.
        let t = repo(None);
        let repo = t.project.repo().unwrap();
        let before = repo.head().unwrap().peel_to_commit().unwrap();
        std::fs::write(t.root.join(".gitattributes"), "*.psd filter=lfs\n").unwrap();
        let with_lfs = commit_all(&repo, "lfs");
        repo.branch("has-lfs", &repo.find_commit(with_lfs).unwrap(), false)
            .unwrap();
        repo.reset(before.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        let error = create(&t.project, "has-lfs", None).unwrap_err();
        assert!(
            error
                .downcast_ref::<Problem>()
                .unwrap()
                .message
                .contains("Git LFS"),
            "{error:#}"
        );
        assert!(!path_for(&t.project, "has-lfs").exists());
        assert_eq!(list(&t.project).unwrap().len(), 1);
        // The branch is still there: it was not ours to remove.
        assert!(repo.find_branch("has-lfs", BranchType::Local).is_ok());

        // The other way round: LFS on the default HEAD does not stop a
        // branch whose own commit has none.
        let t = repo_without_lfs_branch();
        let created = create(&t.project, "clean", None).unwrap();
        assert!(created.path.join("README.md").is_file());
    }

    fn repo_without_lfs_branch() -> TestRepo {
        let t = repo(None);
        let before = lfs_on_main(&t);
        let git = t.project.repo().unwrap();
        git.branch("clean", &git.find_commit(before).unwrap(), false)
            .unwrap();
        t
    }

    #[test]
    fn a_branch_that_moves_before_the_checkout_is_judged_again() {
        let t = repo(None);
        let git = t.project.repo().unwrap();
        let clean = git.head().unwrap().peel_to_commit().unwrap();
        git.branch("moving", &clean, false).unwrap();
        // Another tool moves it to a commit with LFS after the first check.
        std::fs::write(t.root.join(".gitattributes"), "*.psd filter=lfs\n").unwrap();
        let with_lfs = commit_all(&git, "lfs");
        git.reset(clean.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        let root = t.root.clone();
        testhook::set(move || {
            let git = Repository::open(&root).unwrap();
            let target = git.find_commit(with_lfs).unwrap();
            git.branch("moving", &target, true).unwrap();
        });
        let error = create(&t.project, "moving", None).unwrap_err();
        assert!(
            error
                .downcast_ref::<Problem>()
                .expect("typed")
                .message
                .contains("Git LFS"),
            "{error:#}"
        );
        assert!(!path_for(&t.project, "moving").exists());
        assert_eq!(list(&t.project).unwrap().len(), 1);
    }

    // --- precedence ---------------------------------------------------------

    #[test]
    fn the_starting_point_is_base_then_from_then_remote_then_default() {
        // Four different commits: the default workspace's HEAD (main), the
        // HEAD of workspace `src`, `origin/NAME`, and a tag.
        let t = repo(None);
        let git = t.project.repo().unwrap();
        let main_head = git.head().unwrap().peel_to_commit().unwrap().id();
        let src = create(&t.project, "src", None).unwrap();
        std::fs::write(src.path.join("src.txt"), "src").unwrap();
        let src_git = Repository::open(&src.path).unwrap();
        let src_head = commit_all(&src_git, "src work");
        let remote = with_remote_branches(&t, "origin", &["pick", "pick2", "pick3", "pick4"]);
        let tag_target = {
            std::fs::write(t.root.join("tagged.txt"), "tagged").unwrap();
            let id = commit_all(&git, "tagged");
            git.tag_lightweight("v1", &git.find_object(id, None).unwrap(), false)
                .unwrap();
            git.reset(
                &git.find_object(main_head, None).unwrap(),
                git2::ResetType::Hard,
                None,
            )
            .unwrap();
            id
        };
        assert!(
            [main_head, src_head, remote[0], tag_target]
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                == 4
        );
        let src = find(&t.project, "src").unwrap().unwrap();

        // base + from + remote branch: the base.
        let created = create_with(
            &t.project,
            "pick",
            &CreateOptions {
                from: Some(&src),
                base: Some("v1"),
            },
        )
        .unwrap();
        assert_eq!(created.origin.commit, tag_target.to_string());
        assert_eq!(
            created.origin.start,
            BranchStart::Base { spec: "v1".into() }
        );
        assert_eq!(created.origin.tracks, None);

        // from + remote branch: the workspace, and the ignored remote is said.
        let created = create_with(
            &t.project,
            "pick2",
            &CreateOptions {
                from: Some(&src),
                base: None,
            },
        )
        .unwrap();
        assert_eq!(created.origin.commit, src_head.to_string());
        assert_eq!(
            created.origin.start,
            BranchStart::Workspace { name: "src".into() }
        );
        assert!(created.warnings.iter().any(|w| w.contains("origin/pick2")));

        // the remote branch alone: it, tracked.
        let created = create_with(&t.project, "pick3", &CreateOptions::default()).unwrap();
        assert_eq!(created.origin.commit, remote[2].to_string());
        assert_eq!(created.origin.tracks.as_deref(), Some("origin/pick3"));

        // nothing else: the default workspace's HEAD.
        let created = create_with(&t.project, "pick5", &CreateOptions::default()).unwrap();
        assert_eq!(created.origin.commit, main_head.to_string());
        assert_eq!(created.origin.start, BranchStart::DefaultWorkspace);

        // A base alone beats a remote branch of the same name, which it
        // names only by being it.
        let created = create_with(
            &t.project,
            "pick4",
            &CreateOptions {
                from: None,
                base: Some("v1"),
            },
        )
        .unwrap();
        assert_eq!(created.origin.commit, tag_target.to_string());
        assert_eq!(created.origin.tracks, None);
    }

    // --- what libgit2 cannot check out -------------------------------------

    fn untouched_after_refusal(t: &TestRepo, name: &str) {
        let repo = t.project.repo().unwrap();
        assert!(repo.find_branch(name, BranchType::Local).is_err());
        assert!(!path_for(&t.project, name).exists());
        assert_eq!(list(&t.project).unwrap().len(), 1);
    }

    #[test]
    fn git_lfs_is_refused_before_anything_is_created() {
        // `.gitattributes` in the commit.
        let t = repo(None);
        std::fs::write(
            t.root.join(".gitattributes"),
            "*.psd filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();
        commit_all(&t.project.repo().unwrap(), "lfs");
        let error = create(&t.project, "lfs-a", None).unwrap_err();
        let problem = error.downcast_ref::<Problem>().expect("typed");
        assert!(problem.message.contains("Git LFS"), "{problem:?}");
        assert!(!problem.message.contains("git lfs"), "no command line");
        assert!(
            problem
                .remedies
                .iter()
                .any(|r| matches!(r, Remedy::InstallGitHook { .. }))
        );
        untouched_after_refusal(&t, "lfs-a");

        // A nested file counts; one that only says something else does not.
        let t = repo(None);
        std::fs::create_dir_all(t.root.join("assets/raw")).unwrap();
        std::fs::write(
            t.root.join("assets/raw/.gitattributes"),
            "*.bin filter=lfs\n",
        )
        .unwrap();
        std::fs::write(t.root.join(".gitattributes"), "*.txt text\n").unwrap();
        commit_all(&t.project.repo().unwrap(), "nested");
        assert!(create(&t.project, "lfs-b", None).is_err());
        untouched_after_refusal(&t, "lfs-b");

        // The filter's configuration alone is not LFS: with no attribute naming
        // it, no pointer file can appear.
        let t = repo(None);
        t.project
            .repo()
            .unwrap()
            .config()
            .unwrap()
            .set_str("filter.lfs.smudge", "git-lfs smudge -- %f")
            .unwrap();
        let created = create(&t.project, "lfs-c", None).unwrap();
        assert!(created.path.join("README.md").is_file());

        // The base is what is judged: LFS only in another commit's tree does
        // not stop a worktree on a commit without it.
        let t = repo(None);
        let repo = t.project.repo().unwrap();
        let before = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
            .to_string();
        std::fs::write(t.root.join(".gitattributes"), "*.psd filter=lfs\n").unwrap();
        commit_all(&repo, "lfs");
        assert!(create(&t.project, "lfs-d", None).is_err());
        let created = create_with(
            &t.project,
            "lfs-d",
            &CreateOptions {
                from: None,
                base: Some(&before),
            },
        )
        .unwrap();
        assert!(created.workspace.path.join("README.md").is_file());
    }

    #[test]
    fn lfs_only_in_the_global_configuration_is_not_an_lfs_repository() {
        // `git lfs install` writes the filter globally; the repository has none.
        let t = repo(None);
        let repo = t.project.repo().unwrap();
        let global = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            global.path(),
            "[filter \"lfs\"]\n\tclean = git-lfs clean -- %f\n\tsmudge = git-lfs smudge -- %f\n",
        )
        .unwrap();
        let mut config = repo.config().unwrap();
        config
            .add_file(global.path(), git2::ConfigLevel::Global, true)
            .unwrap();
        assert!(config.get_string("filter.lfs.smudge").is_ok());
        let created = create(&t.project, "fine", None).unwrap();
        assert!(created.path.join("README.md").is_file());
    }

    #[test]
    fn partial_clones_are_refused() {
        for (key, value) in [
            ("extensions.partialClone", "origin"),
            ("remote.origin.promisor", "true"),
        ] {
            let t = repo(None);
            let repo = t.project.repo().unwrap();
            repo.remote("origin", "https://example.invalid/shop.git")
                .unwrap();
            repo.config().unwrap().set_str(key, value).unwrap();
            let error = create(&t.project, "partial", None).unwrap_err();
            let problem = error.downcast_ref::<Problem>().expect("typed");
            assert!(
                problem.message.contains("partial clone"),
                "{key}: {problem:?}"
            );
            untouched_after_refusal(&t, "partial");
        }
        // A promisor that is off is not one.
        let t = repo(None);
        let repo = t.project.repo().unwrap();
        repo.remote("origin", "https://example.invalid/shop.git")
            .unwrap();
        repo.config()
            .unwrap()
            .set_str("remote.origin.promisor", "false")
            .unwrap();
        create(&t.project, "whole", None).unwrap();
    }

    #[test]
    fn a_sparse_checkout_is_a_warning() {
        let t = repo(None);
        t.project
            .repo()
            .unwrap()
            .config()
            .unwrap()
            .set_bool("core.sparseCheckout", true)
            .unwrap();
        let created = create_with(&t.project, "sparse", &CreateOptions::default()).unwrap();
        assert!(
            created.warnings.iter().any(|w| w.contains("sparse")),
            "{:?}",
            created.warnings
        );
        assert!(created.workspace.path.join("README.md").is_file());
    }

    // --- plain errors ------------------------------------------------------

    #[test]
    fn a_branch_beside_a_group_of_branches_is_explained() {
        let t = repo(None);
        create(&t.project, "feature/x", None).unwrap();
        let error = create(&t.project, "feature", None).unwrap_err();
        let shown = format!("{error:#}");
        assert!(
            shown.contains("feature/x") && shown.contains("cannot hold both"),
            "{shown}"
        );
        assert!(!shown.contains("class="), "{shown}");
        let error = create(&t.project, "feature/x/deep", None).unwrap_err();
        let shown = format!("{error:#}");
        assert!(
            shown.contains("'feature/x'") && !shown.contains("class="),
            "{shown}"
        );
        assert!(!path_for(&t.project, "feature").exists());
        let repo = t.project.repo().unwrap();
        assert!(repo.find_branch("feature", BranchType::Local).is_err());
    }

    #[test]
    fn invalid_names_say_why() {
        for (name, why) in [
            ("bad name", "a space"),
            ("x..y", "'..'"),
            ("-dash", "'-'"),
            ("a//b", "empty part"),
            ("x.lock", ".lock"),
            ("a~b", "'~'"),
            ("trailing.", "ends with '.'"),
        ] {
            let error = validate_name(name).unwrap_err().to_string();
            assert!(error.contains(why), "{name}: {error}");
        }
        assert!(validate_name(&"a".repeat(100)).is_ok());
        let error = validate_name(&"a".repeat(101)).unwrap_err().to_string();
        assert!(
            error.contains("100 characters") && error.contains("101") && error.len() < 160,
            "{error}"
        );
        let t = repo(None);
        assert!(create(&t.project, &"é".repeat(101), None).is_err());
    }

    #[test]
    fn a_directory_collision_names_the_other_workspace() {
        let t = repo(None);
        create(&t.project, "feature/x", None).unwrap();
        let error = create(&t.project, "feature-x", None).unwrap_err();
        let problem = error.downcast_ref::<Problem>().expect("typed");
        assert!(
            problem.message.contains("already exists")
                && problem.message.contains("'feature/x'")
                && problem.message.contains("feature-x"),
            "{problem:?}"
        );
        assert!(
            problem
                .remedies
                .iter()
                .any(|r| matches!(r, Remedy::SwitchTo { name, .. } if name == "feature/x"))
        );
        // An unrelated directory is not blamed on a workspace.
        std::fs::create_dir_all(path_for(&t.project, "stray")).unwrap();
        let error = create(&t.project, "stray", None).unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error}");
        assert!(!error.to_string().contains("workspace '"), "{error}");
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
