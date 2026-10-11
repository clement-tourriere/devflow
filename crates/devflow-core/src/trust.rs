//! Hook trust: approval of the exact `[hooks]` content per project checkout.
//!
//! Approvals live in `<config dir>/trust.toml`. An entry is keyed by project
//! id *and* main-checkout path, so approving a clone does not approve another
//! clone of the same repository.
use crate::config::HooksSection;
use crate::project::Project;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TrustState {
    NoHooks,
    Trusted,
    /// Never approved, or approved content has changed since.
    Untrusted {
        digest: String,
        previously_trusted: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    project_id: uuid::Uuid,
    root: PathBuf,
    digest: String,
    approved_at: DateTime<Utc>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default, rename = "approval")]
    approvals: Vec<Entry>,
}

/// SHA-256 (hex) of the canonical JSON of the merged `[hooks]` section.
/// Struct fields serialize in declaration order, so the text is stable.
pub fn digest(hooks: &HooksSection) -> String {
    let json = serde_json::to_vec(hooks).expect("hooks serialize to JSON");
    Sha256::digest(&json)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn state(project: &Project) -> Result<TrustState> {
    state_in(&crate::paths::config_dir()?, project)
}

/// Record approval of the current `[hooks]` content.
pub fn approve(project: &Project) -> Result<()> {
    approve_in(&crate::paths::config_dir()?, project)
}

pub fn revoke(project: &Project) -> Result<bool> {
    revoke_in(&crate::paths::config_dir()?, project)
}

/// Everything one approval covers: every command of every hook phase.
/// Frontends show all of it, then approve exactly this content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Scope {
    pub setup: Vec<String>,
    pub switch: Vec<String>,
    pub teardown: Vec<String>,
    pub timeout: String,
    pub digest: String,
}

pub fn scope(project: &Project) -> Scope {
    let hooks = &project.config.hooks;
    Scope {
        setup: hooks.setup.clone(),
        switch: hooks.switch.clone(),
        teardown: hooks.teardown.clone(),
        timeout: hooks.timeout.clone(),
        digest: digest(hooks),
    }
}

/// Approve the content the user reviewed. Re-reads the configuration and
/// fails if `[hooks]` changed since `reviewed` was taken, so an approval never
/// covers commands nobody saw.
pub fn approve_scope(project: &Project, reviewed: &Scope) -> Result<()> {
    let current = project.reload()?;
    anyhow::ensure!(
        digest(&current.config.hooks) == reviewed.digest,
        "the hooks in {} changed since they were shown; review them again",
        current.config_path.display()
    );
    approve(&current)
}

/// `DEVFLOW_TRUST_HOOKS=1` trusts hooks for one run (CI, agents).
pub fn env_override() -> bool {
    std::env::var("DEVFLOW_TRUST_HOOKS").is_ok_and(|v| v == "1" || v == "true")
}

fn state_in(dir: &Path, project: &Project) -> Result<TrustState> {
    let hooks = &project.config.hooks;
    if hooks.is_empty() {
        return Ok(TrustState::NoHooks);
    }
    let digest = digest(hooks);
    let file = read(dir)?;
    Ok(match file.approvals.iter().find(|e| matches(e, project)) {
        Some(entry) if entry.digest == digest => TrustState::Trusted,
        previous => TrustState::Untrusted {
            digest,
            previously_trusted: previous.is_some(),
        },
    })
}

fn approve_in(dir: &Path, project: &Project) -> Result<()> {
    with_lock(dir, || {
        let mut file = read(dir)?;
        file.approvals.retain(|e| !matches(e, project));
        file.approvals.push(Entry {
            project_id: project.id(),
            root: project.root.clone(),
            digest: digest(&project.config.hooks),
            approved_at: Utc::now(),
        });
        write(dir, &file)
    })
}

fn revoke_in(dir: &Path, project: &Project) -> Result<bool> {
    with_lock(dir, || {
        let mut file = read(dir)?;
        let before = file.approvals.len();
        file.approvals.retain(|e| !matches(e, project));
        let removed = file.approvals.len() != before;
        if removed {
            write(dir, &file)?;
        }
        Ok(removed)
    })
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

    fn with_hooks(setup: &[&str]) -> (testutil::TestRepo, Project) {
        let t = testutil::repo(None);
        let mut project = t.project.clone();
        project.config.hooks.setup = setup.iter().map(|s| s.to_string()).collect();
        (t, project)
    }

    #[test]
    fn digest_is_stable_hex_and_sensitive_to_every_field() {
        let base = HooksSection::default();
        let d = digest(&base);
        assert_eq!(d.len(), 64);
        assert!(d.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(d, digest(&base.clone()));
        let mut changed = base.clone();
        changed.setup.push("make".into());
        assert_ne!(d, digest(&changed));
        let mut changed = base.clone();
        changed.teardown.push("make clean".into());
        assert_ne!(d, digest(&changed));
        let mut changed = base.clone();
        changed.timeout = "1m".into();
        assert_ne!(d, digest(&changed));
    }

    #[test]
    fn no_hooks_never_needs_trust() {
        let dir = tempfile::tempdir().unwrap();
        let (_t, project) = with_hooks(&[]);
        assert_eq!(state_in(dir.path(), &project).unwrap(), TrustState::NoHooks);
    }

    #[test]
    fn approval_tracks_exact_content() {
        let dir = tempfile::tempdir().unwrap();
        let (_t, mut project) = with_hooks(&["bun install"]);
        let expected = digest(&project.config.hooks);
        assert_eq!(
            state_in(dir.path(), &project).unwrap(),
            TrustState::Untrusted {
                digest: expected.clone(),
                previously_trusted: false
            }
        );
        approve_in(dir.path(), &project).unwrap();
        assert_eq!(state_in(dir.path(), &project).unwrap(), TrustState::Trusted);

        project.config.hooks.setup.push("rm -rf /".into());
        match state_in(dir.path(), &project).unwrap() {
            TrustState::Untrusted {
                digest,
                previously_trusted,
            } => {
                assert!(previously_trusted);
                assert_ne!(digest, expected);
            }
            other => panic!("{other:?}"),
        }

        // Approving again replaces the entry instead of adding one.
        approve_in(dir.path(), &project).unwrap();
        assert_eq!(state_in(dir.path(), &project).unwrap(), TrustState::Trusted);
        let text = std::fs::read_to_string(dir.path().join("trust.toml")).unwrap();
        assert_eq!(text.matches("[[approval]]").count(), 1, "{text}");

        assert!(revoke_in(dir.path(), &project).unwrap());
        assert!(!revoke_in(dir.path(), &project).unwrap());
        assert!(matches!(
            state_in(dir.path(), &project).unwrap(),
            TrustState::Untrusted {
                previously_trusted: false,
                ..
            }
        ));
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
    fn corrupt_files_are_reported_not_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("trust.toml"), "approval = 3").unwrap();
        let (_t, project) = with_hooks(&["make"]);
        assert!(state_in(dir.path(), &project).is_err());
    }
}
