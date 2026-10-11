//! Fixture repositories for the scan, built in temporary directories.
use super::*;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

mod accuracy;
mod detectors;
mod hardening;
mod round2;
mod round3;
mod security;
mod stacks;

/// A temporary repository populated from `(path, content)` pairs.
fn fixture(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for (path, content) in files {
        write(dir.path(), path, content);
    }
    dir
}

fn write(root: &Path, path: &str, content: &str) {
    let full = root.join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(full, content).expect("write file");
}

fn mkdir(root: &Path, path: &str) {
    fs::create_dir_all(root.join(path)).expect("create dir");
}

/// Initialize a Git repository so ignore status can be answered.
fn git_init(root: &Path) {
    git2::Repository::init(root).expect("git init");
}

fn scan_dir(dir: &TempDir) -> Facts {
    scan(dir.path()).expect("scan")
}

/// The facts as the JSON a consumer would see.
fn json(facts: &Facts) -> String {
    serde_json::to_string(facts).expect("json")
}

/// Whether any warning's message contains `needle`.
fn has_warning(facts: &Facts, needle: &str) -> bool {
    facts.warnings.iter().any(|w| w.message.contains(needle))
}

fn framework(facts: &Facts, framework: Framework) -> Option<&FrameworkFact> {
    facts.frameworks.iter().find(|f| f.framework == framework)
}

fn migration(facts: &Facts, tool: MigrationTool) -> Option<&MigrationFact> {
    facts.migrations.iter().find(|m| m.tool == tool)
}

fn artifact<'a>(facts: &'a Facts, path: &str) -> Option<&'a ArtifactFact> {
    facts.artifacts.iter().find(|a| a.path == path)
}

fn process<'a>(facts: &'a Facts, name: &str, source: ProcessSource) -> Option<&'a ProcessFact> {
    facts
        .processes
        .iter()
        .find(|p| p.name == name && p.source == source)
}

fn service<'a>(facts: &'a Facts, name: &str) -> &'a ComposeService {
    facts
        .compose
        .as_ref()
        .and_then(|c| c.services.iter().find(|s| s.name == name))
        .unwrap_or_else(|| panic!("compose service {name} not found"))
}
