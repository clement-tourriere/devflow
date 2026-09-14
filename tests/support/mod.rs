//! Native Git fixture setup. Only CLI contract calls execute a binary.
use std::path::Path;

pub fn init(root: &Path) {
    git2::Repository::init_opts(
        root,
        git2::RepositoryInitOptions::new().initial_head("main"),
    )
    .unwrap();
}

pub fn commit(root: &Path) {
    let repo = git2::Repository::open(root).unwrap();
    let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
    let mut index = repo.index().unwrap();
    index
        .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let parent = repo.head().ok().and_then(|head| head.peel_to_commit().ok());
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "fixture",
        &tree,
        &parent.iter().collect::<Vec<_>>(),
    )
    .unwrap();
}
