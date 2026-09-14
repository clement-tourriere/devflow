//! One-shot review probes. Every repository is disposable; no helper programs.
use anyhow::Result;
use devflow_core::vcs::{jj::JjRepository, GitRepository, VcsProvider};
use serde_json::{json, Value};
use std::{fs, path::Path};

fn commit(
    git: &git2::Repository,
    reference: &str,
    parent: Option<git2::Oid>,
    content: &str,
) -> Result<git2::Oid> {
    let blob = git.blob(content.as_bytes())?;
    let mut builder = git.treebuilder(None)?;
    builder.insert("file", blob, 0o100644)?;
    let tree = git.find_tree(builder.write()?)?;
    let sig = git2::Signature::now("Review fixture", "review@example.invalid")?;
    let parent = parent.map(|id| git.find_commit(id)).transpose()?;
    Ok(git.commit(
        Some(reference),
        &sig,
        &sig,
        "fixture",
        &tree,
        &parent.iter().collect::<Vec<_>>(),
    )?)
}

fn git_fixture(root: &Path) -> Result<(git2::Repository, git2::Oid)> {
    let git = git2::Repository::init_opts(
        root,
        git2::RepositoryInitOptions::new().initial_head("main"),
    )?;
    let base = commit(&git, "HEAD", None, "base\n")?;
    git.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))?;
    Ok((git, base))
}

fn staged_text(git: &git2::Repository) -> Result<Option<String>> {
    let mut index = git.index()?;
    index.read(true)?;
    let entry = index.get_path(Path::new("file"), 0);
    entry
        .map(|entry| Ok(String::from_utf8_lossy(git.find_blob(entry.id)?.content()).into_owned()))
        .transpose()
}

fn staging() -> Result<Value> {
    let root = tempfile::tempdir()?;
    let (git, _) = git_fixture(root.path())?;
    let jj = JjRepository::init(root.path(), true)?;
    fs::write(root.path().join("file"), "staged\n")?;
    let mut index = git.index()?;
    index.add_path(Path::new("file"))?;
    index.write()?;
    fs::write(root.path().join("file"), "unstaged\n")?;
    let before = staged_text(&git)?;
    let result = jj.list_workspaces();
    Ok(
        json!({"index_before":before, "index_after":staged_text(&git)?, "worktree_after":fs::read_to_string(root.path().join("file"))?, "list_error":result.err().map(|e|format!("{e:#}"))}),
    )
}

fn merge_state() -> Result<Value> {
    let root = tempfile::tempdir()?;
    let (git, base) = git_fixture(root.path())?;
    commit(&git, "HEAD", Some(base), "ours\n")?;
    git.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))?;
    let other = commit(&git, "refs/heads/other", Some(base), "theirs\n")?;
    let jj = JjRepository::init(root.path(), true)?;
    git.merge(&[&git.find_annotated_commit(other)?], None, None)?;
    let before = format!("{:?}", git.state());
    let conflicts_before = git.index()?.has_conflicts();
    let result = jj.list_workspaces();
    let mut index = git.index()?;
    index.read(true)?;
    Ok(
        json!({"state_before":before,"conflicts_before":conflicts_before,"state_after":format!("{:?}",git.state()),"conflicts_after":index.has_conflicts(),"merge_head_after":git.path().join("MERGE_HEAD").exists(),"list_error":result.err().map(|e|format!("{e:#}"))}),
    )
}

fn stale_child() -> Result<Value> {
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().join("primary");
    let child = fixture.path().join("child");
    let jj = JjRepository::init(&root, false)?;
    fs::write(root.join("file"), "base\n")?;
    jj.create_workspace("feature", Some("main"))?;
    jj.create_worktree("feature", &child)?;
    JjRepository::new(&child)?;
    fs::write(root.join("file"), "new main\n")?;
    jj.list_workspaces()?;
    let opening = JjRepository::new(&child);
    Ok(
        json!({"primary_after":fs::read_to_string(root.join("file"))?,"child_after":fs::read_to_string(child.join("file"))?,"child_open_error":opening.err().map(|e|format!("{e:#}"))}),
    )
}

#[cfg(unix)]
fn payload_escape() -> Result<Value> {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    let outside = root.path().join("outside");
    fs::create_dir_all(source.join("payload"))?;
    fs::create_dir(&target)?;
    fs::create_dir(&outside)?;
    fs::write(source.join("payload/secret"), "fixture secret\n")?;
    symlink(&outside, target.join("payload"))?;
    let mut config = devflow_core::config::Config::default();
    config.worktree.copy_files = vec!["payload/secret".into()];
    config.worktree.copy_ai_configs = false;
    config.worktree.copy_ignored = false;
    let copied = devflow_core::workspace::worktree::copy_worktree_payload(
        None, &config, &source, &target, None, None,
    );
    fs::write(source.join(".env"), "FIXTURE_SECRET=synthetic\n")?;
    symlink(outside.join("copied.env"), target.join(".env"))?;
    config.worktree.copy_files = devflow_core::config::Config::default().worktree.copy_files;
    let default_copied = devflow_core::workspace::worktree::copy_worktree_payload(
        None, &config, &source, &target, None, None,
    );
    Ok(
        json!({"reported_copied":copied,"escaped_file_created":outside.join("secret").exists(),"external_content":fs::read_to_string(outside.join("secret")).ok(),"default_env_reported_copied":default_copied,"default_env_escaped":outside.join("copied.env").exists()}),
    )
}

fn hooks_replacement() -> Result<Value> {
    let root = tempfile::tempdir()?;
    let (git, _) = git_fixture(root.path())?;
    let hooks = git.path().join("hooks");
    fs::create_dir_all(&hooks)?;
    for name in ["pre-commit", "post-checkout"] {
        fs::write(
            hooks.join(name),
            "#!/bin/sh\n# existing project checks\nexit 1\n",
        )?;
    }
    GitRepository::new(root.path())?.install_hooks()?;
    Ok(
        json!({"pre_commit_preserved":fs::read_to_string(hooks.join("pre-commit"))?.contains("existing project checks"),"post_checkout_preserved":fs::read_to_string(hooks.join("post-checkout"))?.contains("existing project checks"),"internal_git_wrappers":fs::read_to_string(hooks.join("post-checkout"))?.matches("git rev-parse").count()}),
    )
}

#[cfg(unix)]
fn failed_remove() -> Result<Value> {
    use std::os::unix::fs::PermissionsExt;
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().join("primary");
    let parent = fixture.path().join("restricted");
    let child = parent.join("child");
    let jj = JjRepository::init(&root, false)?;
    fs::write(root.join("file"), "base\n")?;
    jj.create_workspace("feature", Some("main"))?;
    jj.create_worktree("feature", &child)?;
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o500))?;
    let removed = jj.remove_worktree(&child, true);
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
    let path_after = jj.worktree_path("feature")?;
    let retry = jj.remove_worktree(&child, true);
    Ok(
        json!({"first_error":removed.err().map(|e|format!("{e:#}")),"registered_after_failure":path_after.is_some(),"retry_error":retry.err().map(|e|format!("{e:#}")),"directory_retained":child.exists()}),
    )
}

fn main() -> Result<()> {
    let config = tempfile::tempdir()?;
    let jj_config = config.path().join("jj.toml");
    fs::write(
        &jj_config,
        "[user]\nname = 'Review fixture'\nemail = 'review@example.invalid'\n",
    )?;
    std::env::set_var("JJ_CONFIG", &jj_config);
    let mut probes = serde_json::Map::new();
    let mut capture = |name: &str, result: Result<Value>| {
        probes.insert(
            name.into(),
            result.unwrap_or_else(|e| json!({"probe_error":format!("{e:#}")})),
        );
    };
    capture("staging", staging());
    capture("merge_state", merge_state());
    capture("stale_child", stale_child());
    capture("hooks_replacement", hooks_replacement());
    #[cfg(unix)]
    capture("payload_escape", payload_escape());
    #[cfg(unix)]
    capture("failed_remove", failed_remove());
    println!("{}", serde_json::to_string_pretty(&probes)?);
    Ok(())
}
