//! Repositories libgit2 cannot check out faithfully.
//!
//! devflow makes worktrees through libgit2, which runs neither Git LFS's
//! smudge filter nor the fetch a partial clone needs for a missing object. A
//! worktree made anyway would hold LFS pointer files where the real files
//! belong, or fail halfway with a half-written checkout. Both are refused
//! before anything is created, as a [`Problem`] whose remedies are the way
//! round: a worktree made by Git itself, which the Git hook then sets up.
//!
//! **LFS is decided by attributes alone.** Pointer files appear only where an
//! attribute names `filter=lfs`; a `filter.lfs` section in a configuration file
//! changes nothing on its own. So a repository uses LFS when any attributes
//! source contains `filter=lfs` (an `[attr]` macro definition included): every
//! `.gitattributes` of the commit about to be checked out (its whole tree, each
//! distinct subtree once), `info/attributes`, the effective `core.attributesFile`
//! (or Git's default location for it) and the system attributes file.
//!
//! **Configuration is consulted for two things:** where `core.attributesFile`
//! points, and the partial-clone keys `git clone --filter` writes to the
//! repository's own configuration (`extensions.partialClone`,
//! `remote.*.promisor`). Besides libgit2's effective configuration, the
//! include graph is read from the files themselves ([`super::rawconfig`]),
//! because libgit2 skips `includeIf "hasconfig:remote.*.url:…"`.
//!
//! Absence has to be established, not assumed: a tree too large to scan within
//! [`MAX_TREE_ENTRIES`], an attributes or configuration file that exists but
//! cannot be read, or an include condition that cannot be evaluated is refused
//! as "cannot tell".
use super::rawconfig;
use crate::remedy::{Problem, Remedy};
use anyhow::Result;
use git2::{Commit, ConfigLevel, ObjectType, Oid, Repository};
use std::collections::HashSet;
use std::path::Path;

/// Where the Git LFS and partial clone problems point to.
pub const GUIDE_URL: &str = "https://github.com/clement-tourriere/devflow/blob/main/docs/ARCHITECTURE.md#checkouts-devflow-does-not-make";

/// `.gitattributes` files larger than this are not read; Git itself ignores
/// anything beyond 100 MiB, so this is far above any real file.
const MAX_ATTRIBUTES_BYTES: usize = 16 << 20;
/// Tree entries looked at while scanning a commit for `.gitattributes` files,
/// each distinct subtree counted once. A tree beyond it is refused, not
/// assumed free of LFS.
pub const MAX_TREE_ENTRIES: usize = 5_000_000;

/// Refuse a commit libgit2 cannot check out faithfully; return the warnings
/// for what it can check out but not quite like Git (a sparse checkout).
pub fn check(repo: &Repository, commit: &Commit, root: &Path) -> Result<Vec<String>> {
    check_within(repo, commit, root, MAX_TREE_ENTRIES, &gather(repo))
}

fn check_within(
    repo: &Repository,
    commit: &Commit,
    root: &Path,
    max_entries: usize,
    gathered: &Gathered,
) -> Result<Vec<String>> {
    let settings = &gathered.settings;
    let way_on = |problem: Problem| {
        problem
            .with(Remedy::InstallGitHook {
                root: root.to_path_buf(),
            })
            .with(Remedy::OpenGuide {
                url: GUIDE_URL.to_string(),
            })
            .into_error()
    };
    let lfs = lfs(repo, commit, gathered, max_entries);
    // A configuration that cannot be read completely is reported first: what
    // the attributes say may be overridden in the part that was not read.
    if let Some(why) = &gathered.unknown {
        return Err(way_on(Problem::new(format!(
            "devflow cannot read this repository's configuration completely ({why}), so it \
             cannot tell whether it uses Git LFS or is a partial clone, which a checkout through \
             libgit2 would get wrong. Make the worktree with Git itself; the Git hook then sets \
             it up"
        ))));
    }
    match lfs {
        Lfs::Uses(why) => {
            return Err(way_on(Problem::new(format!(
                "this repository uses Git LFS ({why}). devflow checks out with libgit2, which \
                 does not run LFS filters: the new worktree would hold pointer files instead of \
                 the real ones. Make the worktree with Git itself; the Git hook then sets it up"
            ))));
        }
        Lfs::Unknown(why) => {
            return Err(way_on(Problem::new(format!(
                "devflow cannot tell whether this repository uses Git LFS ({why}), and a \
                 checkout through libgit2 would hold pointer files if it does. Make the worktree \
                 with Git itself; the Git hook then sets it up"
            ))));
        }
        Lfs::No => {}
    }
    match partial_clone_reason(settings) {
        Ok(None) => {}
        Ok(Some(why)) => {
            return Err(way_on(Problem::new(format!(
                "this repository is a partial clone ({why}). devflow checks out with libgit2, \
                 which cannot fetch the objects a partial clone leaves out: the checkout could \
                 fail halfway or miss files. Make the worktree with Git itself; the Git hook \
                 then sets it up"
            ))));
        }
        Err(key) => {
            return Err(way_on(Problem::new(format!(
                "devflow cannot tell whether this repository is a partial clone: {key} is not a \
                 value Git reads as true or false. Fix it, or make the worktree with Git itself; \
                 the Git hook then sets it up"
            ))));
        }
    }
    let mut warnings = Vec::new();
    // An unreadable value is taken as set: the warning is the safe side.
    if settings
        .iter()
        .any(|(name, value)| name == "core.sparsecheckout" && parse_bool(value).unwrap_or(true))
    {
        warnings.push(
            "this repository uses a sparse checkout (core.sparseCheckout); the new worktree is a \
             full checkout"
                .to_string(),
        );
    }
    Ok(warnings)
}

/// What the configuration says that decides the checkout.
#[derive(Default)]
struct Gathered {
    /// Keys (lowercased) and values of the repository's own configuration, as
    /// libgit2 and the walk of its files read it; never the global or system
    /// files. A key without a value is `true`.
    settings: Vec<(String, String)>,
    /// The effective `core.attributesFile` (the last one Git reads), if any.
    attributes_file: Option<rawconfig::AttributesSetting>,
    /// Where Git looks for the user's attributes when none is set.
    default_attributes: Option<std::path::PathBuf>,
    /// A condition or a file that could not be evaluated or read: what it
    /// holds is not "nothing".
    unknown: Option<String>,
}

fn repository_level(level: ConfigLevel) -> bool {
    matches!(
        level,
        ConfigLevel::Local | ConfigLevel::Worktree | ConfigLevel::App
    )
}

fn gather(repo: &Repository) -> Gathered {
    let env = rawconfig::Env::process();
    gather_from(repo, &rawconfig::roots(repo, &env), &env)
}

fn gather_from(repo: &Repository, roots: &[rawconfig::Root], env: &rawconfig::Env) -> Gathered {
    let mut gathered = Gathered::default();
    let mut urls = Vec::new();
    match repo.config() {
        Ok(config) => {
            if let Ok(mut entries) = config.entries(None) {
                while let Some(entry) = entries.next() {
                    let Ok(entry) = entry else { continue };
                    let Ok(name) = entry.name() else { continue };
                    let name = name.to_ascii_lowercase();
                    // A key without a value is `true`; a value that is not
                    // text reads as a character no boolean or path is made of.
                    let value = if entry.has_value() {
                        String::from_utf8_lossy(entry.value_bytes()).into_owned()
                    } else {
                        "true".to_string()
                    };
                    if name.starts_with("remote.") && name.ends_with(".url") {
                        urls.push(value.clone());
                    }
                    if repository_level(entry.level()) {
                        gathered.settings.push((name, value));
                    }
                }
            }
        }
        Err(_) => {
            gathered.unknown = Some("the repository's configuration cannot be read".into());
        }
    }
    let found = rawconfig::walk(repo, roots, env, &urls);
    gathered.settings.extend(found.settings);
    gathered.default_attributes = env.default_attributes();
    gathered.attributes_file = found.attributes_file.or_else(|| {
        // Set somewhere the walk does not read: libgit2's view stands.
        let path = repo.config().ok()?.get_path("core.attributesfile").ok()?;
        Some(if path.is_absolute() {
            rawconfig::AttributesSetting::Path(path)
        } else {
            rawconfig::AttributesSetting::Unusable(
                "core.attributesFile is a relative path, which has no directory to be \
                 relative to"
                    .to_string(),
            )
        })
    });
    if gathered.unknown.is_none() {
        gathered.unknown = found.unknown;
    }
    gathered
}

/// Git's boolean: `true`, `yes`, `on` or a non-zero integer (`k`, `m`, `g`
/// suffixes allowed); `false`, `no`, `off`, an empty value or `0`. `None` for
/// anything else.
fn parse_bool(value: &str) -> Option<bool> {
    let v = value.trim().to_ascii_lowercase();
    match v.as_str() {
        "true" | "yes" | "on" => return Some(true),
        "false" | "no" | "off" | "" => return Some(false),
        _ => {}
    }
    let digits = v.strip_suffix(['k', 'm', 'g']).unwrap_or(&v);
    digits.parse::<i64>().ok().map(|n| n != 0)
}

/// Why the configuration makes this a partial clone, `Ok(None)` when it does
/// not, and the key whose value cannot be interpreted otherwise.
fn partial_clone_reason(settings: &[(String, String)]) -> Result<Option<String>, String> {
    for (name, value) in settings {
        if name == "extensions.partialclone" {
            return Ok(Some("extensions.partialClone is set".to_string()));
        }
        let Some(remote) = name
            .strip_prefix("remote.")
            .and_then(|r| r.strip_suffix(".promisor"))
        else {
            continue;
        };
        match parse_bool(value) {
            Some(true) => return Ok(Some(format!("remote.{remote}.promisor is true"))),
            Some(false) => {}
            None => return Err(name.clone()),
        }
    }
    Ok(None)
}

enum Lfs {
    /// An attributes source sets `filter=lfs`.
    Uses(String),
    /// Absence could not be established.
    Unknown(String),
    No,
}

fn lfs(repo: &Repository, commit: &Commit, gathered: &Gathered, max: usize) -> Lfs {
    use rawconfig::AttributesSetting::{Disabled, Path as File, Unusable};
    let mut unknown = None;
    let mut sources = fixed_attribute_files(repo);
    // Only the effective `core.attributesFile` counts: an earlier setting,
    // however unusable, was overridden. With none, Git's default location.
    match &gathered.attributes_file {
        Some(File(path)) => sources.push((
            format!("the attributes file {}", path.display()),
            path.clone(),
        )),
        Some(Disabled) => {}
        Some(Unusable(why)) => unknown = Some(why.clone()),
        None => {
            if let Some(path) = &gathered.default_attributes {
                sources.push((
                    format!("the attributes file {}", path.display()),
                    path.clone(),
                ));
            }
        }
    }
    for (label, path) in sources {
        match std::fs::read(&path) {
            Ok(bytes) if sets_lfs_filter(&String::from_utf8_lossy(&bytes)) => {
                return Lfs::Uses(format!("{label} sets filter=lfs"));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => unknown = Some(format!("{label} cannot be read")),
        }
    }
    let tree = match commit.tree() {
        Ok(tree) => tree,
        Err(_) => return Lfs::Unknown("the commit's tree cannot be read".into()),
    };
    match scan_tree(repo, &tree, max) {
        Scan::Found(why) => Lfs::Uses(why),
        Scan::Incomplete(why) => Lfs::Unknown(why),
        Scan::Clean => match unknown {
            Some(why) => Lfs::Unknown(why),
            None => Lfs::No,
        },
    }
}

/// Attributes files outside the commit that are not named by configuration:
/// the repository's `info/attributes` and the system file.
fn fixed_attribute_files(repo: &Repository) -> Vec<(String, std::path::PathBuf)> {
    let mut files = vec![(
        "info/attributes".to_string(),
        repo.commondir().join("info").join("attributes"),
    )];
    if std::env::var_os("GIT_ATTR_NOSYSTEM").is_none_or(|v| v.is_empty()) {
        for system in [
            "/etc/gitattributes",
            "/usr/local/etc/gitattributes",
            "/opt/homebrew/etc/gitattributes",
        ] {
            files.push((system.to_string(), system.into()));
        }
    }
    files
}

enum Scan {
    Found(String),
    Clean,
    /// The scan could not cover the whole tree.
    Incomplete(String),
}

/// Look at every `.gitattributes` of `tree`, each distinct subtree once.
fn scan_tree(repo: &Repository, tree: &git2::Tree, max_entries: usize) -> Scan {
    let mut seen: HashSet<Oid> = HashSet::from([tree.id()]);
    let mut pending = vec![(tree.clone(), String::new())];
    let mut entries = 0usize;
    while let Some((current, dir)) = pending.pop() {
        for entry in current.iter() {
            entries += 1;
            if entries > max_entries {
                return Scan::Incomplete(format!(
                    "the commit's tree has more than {max_entries} entries and was not scanned \
                     in full"
                ));
            }
            match entry.kind() {
                Some(ObjectType::Tree) => {
                    if seen.insert(entry.id()) {
                        match repo.find_tree(entry.id()) {
                            Ok(sub) => {
                                let name = String::from_utf8_lossy(entry.name_bytes());
                                pending.push((sub, format!("{dir}{name}/")));
                            }
                            Err(_) => {
                                return Scan::Incomplete(
                                    "a directory of the commit cannot be read".to_string(),
                                );
                            }
                        }
                    }
                }
                // A symbolic link named `.gitattributes` is not read by Git.
                Some(ObjectType::Blob)
                    if entry.name_bytes() == b".gitattributes" && entry.filemode() != 0o120000 =>
                {
                    let Ok(blob) = repo.find_blob(entry.id()) else {
                        return Scan::Incomplete(format!("{dir}.gitattributes cannot be read"));
                    };
                    if blob.size() > MAX_ATTRIBUTES_BYTES {
                        return Scan::Incomplete(format!("{dir}.gitattributes is too large"));
                    }
                    if sets_lfs_filter(&String::from_utf8_lossy(blob.content())) {
                        return Scan::Found(format!("{dir}.gitattributes sets filter=lfs"));
                    }
                }
                _ => {}
            }
        }
    }
    Scan::Clean
}

/// Whether attribute lines set `filter=lfs` (a negation or another filter
/// does not count). A macro definition (`[attr]name ... filter=lfs`) counts:
/// whatever uses the macro sets the filter.
fn sets_lfs_filter(text: &str) -> bool {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .any(|line| line.split_whitespace().skip(1).any(|a| a == "filter=lfs"))
}

#[cfg(test)]
mod oracle;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::testutil::{TestRepo, repo};

    #[test]
    fn only_a_set_lfs_filter_counts() {
        assert!(sets_lfs_filter(
            "*.psd filter=lfs diff=lfs merge=lfs -text\n"
        ));
        assert!(sets_lfs_filter("# docs\n\n*.bin\ttext filter=lfs\n"));
        assert!(sets_lfs_filter("[attr]lfs filter=lfs diff=lfs\n"));
        assert!(!sets_lfs_filter("# *.psd filter=lfs\n"));
        assert!(!sets_lfs_filter("*.psd filter=other\n"));
        assert!(!sets_lfs_filter("*.psd -filter\n"));
        assert!(
            !sets_lfs_filter("filter=lfs\n"),
            "a pattern, not an attribute"
        );
    }

    #[test]
    fn promisor_values_are_read_as_booleans() {
        let promisor = |value: &str| {
            partial_clone_reason(&[("remote.origin.promisor".to_string(), value.to_string())])
        };
        for yes in ["true", "TRUE", "yes", "on", "1", "2", "-1", "1k"] {
            assert!(matches!(promisor(yes), Ok(Some(_))), "{yes}");
        }
        for no in ["false", "no", "off", "0", "", "0k"] {
            assert_eq!(promisor(no), Ok(None), "{no:?}");
        }
        for odd in ["maybe", "1.5", "tru"] {
            assert_eq!(
                promisor(odd),
                Err("remote.origin.promisor".to_string()),
                "{odd}"
            );
        }
        assert!(matches!(
            partial_clone_reason(&[("extensions.partialclone".to_string(), "origin".to_string())]),
            Ok(Some(_))
        ));
        assert_eq!(partial_clone_reason(&[]), Ok(None));
    }

    /// The files of `t`'s repository alone: no user or system configuration,
    /// so a test never depends on the machine it runs on.
    fn own_roots(t: &TestRepo) -> Vec<rawconfig::Root> {
        ["config", "config.worktree"]
            .into_iter()
            .map(|name| rawconfig::Root {
                path: t.root.join(".git").join(name),
                repository: true,
            })
            .collect()
    }

    fn verdict(t: &TestRepo, max: usize) -> Result<Vec<String>> {
        verdict_with(t, &[], max)
    }

    /// `global` files are read as the user's own, with `$HOME` at the root of
    /// the test directory.
    fn verdict_with(t: &TestRepo, global: &[&Path], max: usize) -> Result<Vec<String>> {
        let repo = t.project.repo().unwrap();
        let commit = repo.head().unwrap().peel_to_commit().unwrap();
        let mut roots: Vec<rawconfig::Root> = global
            .iter()
            .map(|path| rawconfig::Root {
                path: path.to_path_buf(),
                repository: false,
            })
            .collect();
        roots.extend(own_roots(t));
        let env = rawconfig::Env {
            home: Some(t.dir.path().to_path_buf()),
            xdg_config_home: None,
        };
        let gathered = gather_from(&repo, &roots, &env);
        check_within(&repo, &commit, &t.root, max, &gathered)
    }

    fn refusal(result: Result<Vec<String>>) -> String {
        let error = result.expect_err("refused");
        error
            .downcast_ref::<Problem>()
            .expect("typed")
            .message
            .clone()
    }

    /// A commit holding `dirs` directories of `files` distinct empty files
    /// each, with `attributes` in the last directory (no working tree).
    fn big_commit(
        repo: &Repository,
        dirs: usize,
        files: usize,
        attributes: Option<&str>,
    ) -> git2::Oid {
        let empty = repo.blob(b"").unwrap();
        let mut root = repo.treebuilder(None).unwrap();
        for d in 0..dirs {
            let mut dir = repo.treebuilder(None).unwrap();
            for f in 0..files {
                dir.insert(format!("f{d}-{f}"), empty, 0o100644).unwrap();
            }
            if d + 1 == dirs
                && let Some(text) = attributes
            {
                dir.insert(
                    ".gitattributes",
                    repo.blob(text.as_bytes()).unwrap(),
                    0o100644,
                )
                .unwrap();
            }
            root.insert(format!("d{d}"), dir.write().unwrap(), 0o040000)
                .unwrap();
        }
        let tree = repo.find_tree(root.write().unwrap()).unwrap();
        let sig = crate::worktree::testutil::signature();
        repo.commit(None, &sig, &sig, "big", &tree, &[]).unwrap()
    }

    #[test]
    fn the_whole_tree_is_scanned_not_a_prefix_of_it() {
        let t = repo(None);
        let git = t.project.repo().unwrap();
        // 30,000 entries before the one `.gitattributes`, in the last directory.
        let id = big_commit(&git, 300, 100, Some("*.bin filter=lfs\n"));
        let commit = git.find_commit(id).unwrap();
        let message = refusal(check_within(
            &git,
            &commit,
            &t.root,
            MAX_TREE_ENTRIES,
            &Gathered::default(),
        ));
        assert!(message.contains("d299/.gitattributes"), "{message}");

        // The same tree without LFS passes.
        let clean = big_commit(&git, 300, 100, Some("*.bin text\n"));
        let commit = git.find_commit(clean).unwrap();
        assert!(
            check_within(
                &git,
                &commit,
                &t.root,
                MAX_TREE_ENTRIES,
                &Gathered::default()
            )
            .is_ok()
        );
    }

    #[test]
    fn a_tree_beyond_the_bound_is_refused_not_assumed_clean() {
        let t = repo(None);
        let git = t.project.repo().unwrap();
        let clean = big_commit(&git, 50, 100, None);
        let commit = git.find_commit(clean).unwrap();
        assert!(check_within(&git, &commit, &t.root, 10_000, &Gathered::default()).is_ok());
        let message = refusal(check_within(
            &git,
            &commit,
            &t.root,
            1_000,
            &Gathered::default(),
        ));
        assert!(
            message.contains("cannot tell") && message.contains("not scanned in full"),
            "{message}"
        );
    }

    #[test]
    fn identical_subtrees_are_scanned_once() {
        let t = repo(None);
        let git = t.project.repo().unwrap();
        let empty = git.blob(b"").unwrap();
        let mut leaf = git.treebuilder(None).unwrap();
        for f in 0..100 {
            leaf.insert(format!("f{f}"), empty, 0o100644).unwrap();
        }
        let leaf = leaf.write().unwrap();
        let mut root = git.treebuilder(None).unwrap();
        for d in 0..2000 {
            root.insert(format!("d{d}"), leaf, 0o040000).unwrap();
        }
        let tree = git.find_tree(root.write().unwrap()).unwrap();
        let sig = crate::worktree::testutil::signature();
        let id = git.commit(None, &sig, &sig, "same", &tree, &[]).unwrap();
        let commit = git.find_commit(id).unwrap();
        // 200,000 paths, but only 2,100 distinct entries to look at.
        assert!(check_within(&git, &commit, &t.root, 5_000, &Gathered::default()).is_ok());
    }

    #[test]
    fn attribute_files_outside_the_commit_count() {
        // info/attributes
        let t = repo(None);
        let git = t.project.repo().unwrap();
        std::fs::create_dir_all(git.commondir().join("info")).unwrap();
        std::fs::write(
            git.commondir().join("info/attributes"),
            "*.psd filter=lfs\n",
        )
        .unwrap();
        assert!(refusal(verdict(&t, MAX_TREE_ENTRIES)).contains("info/attributes"));

        // The effective core.attributesFile, whatever level sets it.
        let t = repo(None);
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "*.psd filter=lfs diff=lfs\n").unwrap();
        t.project
            .repo()
            .unwrap()
            .config()
            .unwrap()
            .set_str("core.attributesFile", file.path().to_str().unwrap())
            .unwrap();
        assert!(refusal(verdict(&t, MAX_TREE_ENTRIES)).contains("attributes file"));

        // One that exists but cannot be read is not "nothing".
        let t = repo(None);
        let git = t.project.repo().unwrap();
        std::fs::create_dir_all(git.commondir().join("info/attributes")).unwrap();
        assert!(refusal(verdict(&t, MAX_TREE_ENTRIES)).contains("cannot tell"));
    }

    fn write(dir: &Path, name: &str, text: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    fn lfs_attributes(dir: &Path) -> std::path::PathBuf {
        write(dir, "attrs", "*.psd filter=lfs diff=lfs\n")
    }

    fn add_to_local_config(t: &TestRepo, text: &str) {
        let path = t.root.join(".git/config");
        let mut config = std::fs::read_to_string(&path).unwrap();
        config.push_str(text);
        std::fs::write(path, config).unwrap();
    }

    #[test]
    fn a_key_without_a_value_is_a_boolean_not_a_crash() {
        // `[fetch] prune` is in countless global configurations; libgit2 hands
        // such an entry over without a value.
        let t = repo(None);
        add_to_local_config(
            &t,
            "[fetch]\n\tprune\n[remote \"origin\"]\n\turl = https://a.example.com/x.git\n",
        );
        assert!(verdict(&t, MAX_TREE_ENTRIES).is_ok());
        // A bare `promisor` is `true`.
        add_to_local_config(&t, "[remote \"origin\"]\n\tpromisor\n");
        assert!(refusal(verdict(&t, MAX_TREE_ENTRIES)).contains("partial clone"));
        // A value that is not text neither panics nor passes for a boolean.
        let t = repo(None);
        let path = t.root.join(".git/config");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(b"[remote \"origin\"]\n\tpromisor = \xff\xfe\n");
        std::fs::write(&path, bytes).unwrap();
        assert!(refusal(verdict(&t, MAX_TREE_ENTRIES)).contains("not a value Git reads"));
    }

    #[test]
    fn filter_lfs_configuration_without_an_attribute_is_not_lfs() {
        // `git lfs install` writes the filter; nothing names it, so no pointer
        // file can appear.
        let t = repo(None);
        add_to_local_config(
            &t,
            "[filter \"lfs\"]\n\tsmudge = git-lfs smudge -- %f\n\trequired = true\n",
        );
        let global = write(
            t.dir.path(),
            "global",
            "[filter \"lfs\"]\n\tclean = git-lfs clean -- %f\n",
        );
        assert!(verdict_with(&t, &[&global], MAX_TREE_ENTRIES).is_ok());
        // With an attribute it is.
        std::fs::create_dir_all(t.root.join(".git/info")).unwrap();
        std::fs::write(t.root.join(".git/info/attributes"), "*.psd filter=lfs\n").unwrap();
        assert!(refusal(verdict_with(&t, &[&global], MAX_TREE_ENTRIES)).contains("Git LFS"));
    }

    #[test]
    fn includes_resolve_from_the_directory_of_the_file_that_holds_them() {
        // global -> sub/a.inc -> b.inc (relative to sub/) -> core.attributesFile.
        let t = repo(None);
        let home = t.dir.path().join("home");
        let attrs = lfs_attributes(&home);
        let global = write(&home, ".gitconfig", "[include]\n\tpath = conf/a.inc\n");
        write(&home, "conf/a.inc", "[include]\n\tpath = b.inc\n");
        write(
            &home,
            "conf/b.inc",
            &format!("[core]\n\tattributesFile = {}\n", attrs.display()),
        );
        // The decoy a wrong base directory would find.
        write(&home, "b.inc", "");
        let message = refusal(verdict_with(&t, &[&global], MAX_TREE_ENTRIES));
        assert!(
            message.contains("attributes file") && message.contains("Git LFS"),
            "{message}"
        );

        // `~/` is the test's home.
        let t = repo(None);
        let attrs = lfs_attributes(t.dir.path());
        let global = write(t.dir.path(), "g", "[include]\n\tpath = ~/inc\n");
        write(
            t.dir.path(),
            "inc",
            &format!("[core]\n\tattributesFile = {}\n", attrs.display()),
        );
        assert!(refusal(verdict_with(&t, &[&global], MAX_TREE_ENTRIES)).contains("Git LFS"));

        // A relative core.attributesFile has no directory to be relative to.
        let t = repo(None);
        let global = write(t.dir.path(), "g", "[core]\n\tattributesFile = attrs\n");
        assert!(refusal(verdict_with(&t, &[&global], MAX_TREE_ENTRIES)).contains("cannot tell"));
    }

    #[test]
    fn a_hasconfig_include_reaches_a_nested_gitdir_include() {
        let t = repo(None);
        let home = t.dir.path().join("home");
        let attrs = lfs_attributes(&home);
        t.project
            .repo()
            .unwrap()
            .remote("origin", "https://example.com/team/app.git")
            .unwrap();
        let global = write(
            &home,
            ".gitconfig",
            "[includeIf \"hasconfig:remote.*.url:https://example.com/**\"]\n\tpath = work.inc\n",
        );
        // The nested condition is evaluated for this repository.
        write(
            &home,
            "work.inc",
            &format!(
                "[includeIf \"gitdir:{}/\"]\n\tpath = deep.inc\n\
                 [includeIf \"gitdir:/somewhere/else/\"]\n\tpath = wrong.inc\n",
                t.root.display()
            ),
        );
        write(
            &home,
            "deep.inc",
            &format!("[core]\n\tattributesFile = {}\n", attrs.display()),
        );
        write(
            &home,
            "wrong.inc",
            "[core]\n\tattributesFile = /nonexistent-wrong\n",
        );
        let message = refusal(verdict_with(&t, &[&global], MAX_TREE_ENTRIES));
        assert!(
            message.contains("Git LFS") && message.contains("attrs"),
            "{message}"
        );

        // Another repository's gitdir condition does not apply.
        let other = repo(None);
        let global = write(
            &home,
            "other-gitconfig",
            &format!(
                "[includeIf \"gitdir:{}/\"]\n\tpath = deep.inc\n",
                t.root.display()
            ),
        );
        assert!(verdict_with(&other, &[&global], MAX_TREE_ENTRIES).is_ok());
    }

    #[test]
    fn onbranch_and_gitdir_patterns_follow_git() {
        let t = repo(None);
        let home = t.dir.path().join("home");
        let attrs = lfs_attributes(&home);
        let inc = write(
            &home,
            "inc",
            &format!("[core]\n\tattributesFile = {}\n", attrs.display()),
        );
        for (condition, applies) in [
            ("onbranch:main", true),
            ("onbranch:ma*", true),
            ("onbranch:release/**", false),
            ("gitdir:**/shop/.git", true),
            ("gitdir:shop/.git", true),
            ("gitdir/i:**/SHOP/.GIT", true),
            ("gitdir:**/other/.git", false),
            ("gitdir:~/nowhere/", false),
        ] {
            let global = write(
                &home,
                "gitconfig",
                &format!("[includeIf \"{condition}\"]\n\tpath = {}\n", inc.display()),
            );
            let verdict = verdict_with(&t, &[&global], MAX_TREE_ENTRIES);
            assert_eq!(verdict.is_err(), applies, "{condition}: {verdict:?}");
        }
    }

    fn include_if(t: &TestRepo, condition: &str, file_text: &str) {
        let included = write(&t.root.join(".git"), "cond.inc", file_text);
        add_to_local_config(
            t,
            &format!(
                "[includeIf \"{condition}\"]\n\tpath = {}\n",
                included.display()
            ),
        );
    }

    #[test]
    fn a_matching_hasconfig_include_supplies_attributes_and_partial_clone_keys() {
        let t = repo(None);
        t.project
            .repo()
            .unwrap()
            .remote("origin", "https://example.com/team/app.git")
            .unwrap();
        let attrs = lfs_attributes(t.dir.path());
        include_if(
            &t,
            "hasconfig:remote.*.url:https://example.com/**",
            &format!("[core]\n\tattributesFile = {}\n", attrs.display()),
        );
        assert!(refusal(verdict(&t, MAX_TREE_ENTRIES)).contains("attributes file"));

        let t = repo(None);
        t.project
            .repo()
            .unwrap()
            .remote("origin", "https://example.com/team/app.git")
            .unwrap();
        include_if(
            &t,
            "hasconfig:remote.*.url:https://example.com/**",
            "[remote \"origin\"]\n\tpromisor = 2\n",
        );
        assert!(refusal(verdict(&t, MAX_TREE_ENTRIES)).contains("partial clone"));

        // The same include for another remote applies to nothing here.
        let t = repo(None);
        t.project
            .repo()
            .unwrap()
            .remote("origin", "git@github.com:me/app.git")
            .unwrap();
        include_if(
            &t,
            "hasconfig:remote.*.url:https://example.com/**",
            "[remote \"origin\"]\n\tpromisor = true\n",
        );
        assert!(verdict(&t, MAX_TREE_ENTRIES).is_ok());
    }

    #[test]
    fn a_directory_wildcard_matches_zero_or_more_directories() {
        for (pattern, url, applies) in [
            (
                "https://**/team/app.git",
                "https://example.com/team/app.git",
                true,
            ),
            ("https://**/team/app.git", "https://team/app.git", true),
            (
                "https://**/team/app.git",
                "https://a/b/c/team/app.git",
                true,
            ),
            ("https://*/team/app.git", "https://a/b/team/app.git", false),
            ("**/app.git", "https://example.com/app.git", true),
        ] {
            let t = repo(None);
            t.project.repo().unwrap().remote("origin", url).unwrap();
            include_if(
                &t,
                &format!("hasconfig:remote.*.url:{pattern}"),
                "[remote \"origin\"]\n\tpromisor = true\n",
            );
            assert_eq!(
                verdict(&t, MAX_TREE_ENTRIES).is_err(),
                applies,
                "{pattern} against {url}"
            );
        }
    }

    #[test]
    fn a_pathological_pattern_does_not_stall_the_check() {
        let t = repo(None);
        let url = format!("https://example.com/{}", "a".repeat(3000));
        t.project.repo().unwrap().remote("origin", &url).unwrap();
        include_if(
            &t,
            &format!("hasconfig:remote.*.url:{}x", "*a".repeat(60)),
            "[remote \"origin\"]\n\tpromisor = true\n",
        );
        let started = std::time::Instant::now();
        assert!(verdict(&t, MAX_TREE_ENTRIES).is_ok());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn a_condition_that_cannot_be_evaluated_is_refused_not_ignored() {
        // Another form of hasconfig.
        let t = repo(None);
        t.project
            .repo()
            .unwrap()
            .remote("origin", "https://a.example.com/x.git")
            .unwrap();
        include_if(
            &t,
            "hasconfig:branch.*.remote:origin",
            "[core]\n\tfoo = 1\n",
        );
        let message = refusal(verdict(&t, MAX_TREE_ENTRIES));
        assert!(
            message.contains("cannot read this repository's configuration"),
            "{message}"
        );
        // An include that is simply missing is ignored, as Git does.
        let t = repo(None);
        add_to_local_config(&t, "[include]\n\tpath = /nonexistent/devflow/include\n");
        assert!(verdict(&t, MAX_TREE_ENTRIES).is_ok());
        // A bracket expression is evaluated, as Git evaluates it.
        let t = repo(None);
        t.project
            .repo()
            .unwrap()
            .remote("origin", "https://a.example.com/x.git")
            .unwrap();
        include_if(
            &t,
            "hasconfig:remote.*.url:https://[ab].example.com/**",
            "[remote \"origin\"]\n\tpromisor = true\n",
        );
        assert!(refusal(verdict(&t, MAX_TREE_ENTRIES)).contains("partial clone"));
        let t = repo(None);
        t.project
            .repo()
            .unwrap()
            .remote("origin", "https://c.example.com/x.git")
            .unwrap();
        include_if(
            &t,
            "hasconfig:remote.*.url:https://[ab].example.com/**",
            "[remote \"origin\"]\n\tpromisor = true\n",
        );
        assert!(verdict(&t, MAX_TREE_ENTRIES).is_ok());
    }

    #[test]
    fn an_include_that_cannot_be_read_is_not_absent() {
        use std::os::unix::fs::PermissionsExt;
        // (An unreadable include in the repository's own configuration stops
        // libgit2 from opening it at all; the user's files are devflow's to
        // judge.)
        let t = repo(None);
        let included = write(t.dir.path(), "locked.inc", "[core]\n\tfoo = 1\n");
        let global = write(
            t.dir.path(),
            "g",
            &format!("[include]\n\tpath = {}\n", included.display()),
        );
        std::fs::set_permissions(&included, std::fs::Permissions::from_mode(0o000)).unwrap();
        let readable = std::fs::read(&included).is_ok();
        let result = verdict_with(&t, &[&global], MAX_TREE_ENTRIES);
        std::fs::set_permissions(&included, std::fs::Permissions::from_mode(0o644)).unwrap();
        if readable {
            // Running as root: permissions do not stop us.
            return;
        }
        let message = refusal(result);
        assert!(
            message.contains("cannot read") && message.contains("locked.inc"),
            "{message}"
        );

        // The same through a directory that cannot be entered.
        let t = repo(None);
        let dir = t.dir.path().join("closed");
        write(&dir, "x.inc", "");
        let global = write(
            t.dir.path(),
            "g",
            &format!("[include]\n\tpath = {}/x.inc\n", dir.display()),
        );
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = verdict_with(&t, &[&global], MAX_TREE_ENTRIES);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(refusal(result).contains("cannot read"));
    }

    #[test]
    fn a_promisor_value_git_cannot_read_is_refused() {
        let t = repo(None);
        t.project
            .repo()
            .unwrap()
            .remote("origin", "https://a.example.com/x.git")
            .unwrap();
        let git = t.project.repo().unwrap();
        git.config()
            .unwrap()
            .set_str("remote.origin.promisor", "2")
            .unwrap();
        assert!(refusal(verdict(&t, MAX_TREE_ENTRIES)).contains("partial clone"));
        git.config()
            .unwrap()
            .set_str("remote.origin.promisor", "maybe")
            .unwrap();
        assert!(refusal(verdict(&t, MAX_TREE_ENTRIES)).contains("not a value Git reads"));
        git.config()
            .unwrap()
            .set_str("remote.origin.promisor", "0")
            .unwrap();
        assert!(verdict(&t, MAX_TREE_ENTRIES).is_ok());
    }

    /// Measurement, not a check:
    /// `DEVFLOW_SCAN_REPO=/path/to/repo cargo test -p devflow-core --release --lib
    /// faithful::tests::scan_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing"]
    fn scan_cost() {
        let t = repo(None);
        let git = t.project.repo().unwrap();
        for (dirs, files) in [(200, 100), (2_000, 100), (10_000, 100)] {
            let id = big_commit(&git, dirs, files, Some("*.bin text\n"));
            let commit = git.find_commit(id).unwrap();
            let started = std::time::Instant::now();
            check_within(&git, &commit, &t.root, usize::MAX, &Gathered::default()).unwrap();
            eprintln!(
                "{} entries ({dirs} directories): {:?}",
                dirs * files + dirs,
                started.elapsed()
            );
        }
        if let Some(path) = std::env::var_os("DEVFLOW_SCAN_REPO") {
            let other = Repository::open(path).unwrap();
            let commit = other.head().unwrap().peel_to_commit().unwrap();
            let mut count = 0usize;
            commit
                .tree()
                .unwrap()
                .walk(git2::TreeWalkMode::PreOrder, |_, _| {
                    count += 1;
                    git2::TreeWalkResult::Ok
                })
                .unwrap();
            let started = std::time::Instant::now();
            let verdict = check_within(&other, &commit, &t.root, usize::MAX, &Gathered::default());
            eprintln!(
                "that repository, {count} entries: {:?} ({})",
                started.elapsed(),
                if verdict.is_ok() { "clean" } else { "refused" }
            );
        }
    }
}
