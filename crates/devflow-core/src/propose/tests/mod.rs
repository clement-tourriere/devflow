//! Tests for the proposal layer: fixture repositories in temporary
//! directories, planned with a pinned project id.
use super::*;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

mod apply;
mod autostart;
mod conflicts;
mod containers;
mod preview;
mod stacks;
mod variety;

/// The project id every fixture plan uses.
pub(super) const ID: &str = "0b3c9d2e-7f4a-4c1e-9a52-6d1f3b8e2a40";

pub(super) struct Repo {
    dir: TempDir,
    root: PathBuf,
}

impl Repo {
    /// A Git repository holding `files` (`(path, content)`).
    pub fn new(files: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical root");
        let root = root.join("shop");
        fs::create_dir_all(&root).expect("create root");
        git2::Repository::init(&root).expect("git init");
        let repo = Self { dir, root };
        for (path, content) in files {
            repo.write(path, content);
        }
        repo
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn write(&self, path: &str, content: &str) {
        let full = self.root.join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(full, content).expect("write file");
    }

    pub fn mkdir(&self, path: &str) {
        fs::create_dir_all(self.root.join(path)).expect("create dir");
    }

    pub fn read(&self, path: &str) -> String {
        fs::read_to_string(self.root.join(path)).unwrap_or_else(|e| panic!("read {path}: {e}"))
    }

    pub fn exists(&self, path: &str) -> bool {
        self.root.join(path).exists()
    }

    pub fn options(&self) -> PlanOptions {
        PlanOptions {
            project_id: Some(ID.parse().unwrap()),
            ..PlanOptions::default()
        }
    }

    pub fn plan(&self) -> Proposal {
        propose(&self.root, &self.options()).expect("plan")
    }

    #[allow(dead_code)]
    pub fn keep(&self) -> &TempDir {
        &self.dir
    }
}

pub(super) fn ids(plan: &Proposal) -> Vec<&str> {
    plan.items.iter().map(|item| item.id.as_str()).collect()
}

pub(super) fn item<'a>(plan: &'a Proposal, id: &str) -> &'a Item {
    plan.item(id).unwrap_or_else(|| {
        panic!(
            "no item {id}; items: {:?}\nsatisfied: {:?}\nwarnings: {:?}",
            ids(plan),
            plan.already_configured,
            plan.warnings
        )
    })
}

/// The `run` of the hook an item adds.
pub(super) fn run(plan: &Proposal, id: &str) -> String {
    item(plan, id).value["run"]
        .as_str()
        .unwrap_or_else(|| panic!("{id} has no run: {:?}", item(plan, id).value))
        .to_string()
}

/// The hook `id` as it reads when `accepted` (and the hook itself) are
/// selected: the command it runs, `None` when it drops out because there is
/// nothing left to start.
pub(super) fn resolved_run(plan: &Proposal, accepted: &[&str], id: &str) -> Option<String> {
    let mut ids: Vec<String> = accepted.iter().map(|s| s.to_string()).collect();
    ids.push(id.to_string());
    plan.resolved(&ids)
        .item(id)
        .map(|item| item.value["run"].as_str().unwrap_or_default().to_string())
}

/// The ids of the items `select` would apply by default.
pub(super) fn defaults(plan: &Proposal) -> Vec<String> {
    plan.select(&[], &[], true).expect("select").accepted
}

pub(super) fn all(plan: &Proposal) -> Vec<String> {
    plan.items
        .iter()
        .filter(|i| i.is_actionable())
        .map(|i| i.id.clone())
        .collect()
}
