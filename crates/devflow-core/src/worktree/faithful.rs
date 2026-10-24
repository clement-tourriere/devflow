//! Repositories libgit2 cannot check out faithfully.
//!
//! devflow makes worktrees through libgit2, which runs neither Git LFS's
//! smudge filter nor the fetch a partial clone needs for a missing object. A
//! worktree made anyway would hold LFS pointer files where the real files
//! belong, or fail halfway with a half-written checkout. Both are refused
//! before anything is created, as a [`Problem`] whose remedies are the way
//! round: a worktree made by Git itself, which the Git hook then sets up.
//!
//! The checks judge the commit that is about to be checked out, and read what
//! decides Git's attributes and filters:
//!
//! - every `.gitattributes` of that commit's tree (all of it, each distinct
//!   subtree once), `info/attributes`, the effective `core.attributesFile`
//!   (or Git's default for it) and the system attributes file: any of them
//!   that sets `filter=lfs` makes the repository an LFS repository;
//! - any `filter.lfs.*` key in the repository's own configuration (its
//!   `config`, an included file, `config.worktree`). A `filter.lfs` section in
//!   the global or system configuration alone, which `git lfs install`
//!   writes, does not: with no attribute naming the filter it never runs;
//! - `extensions.partialClone` and `remote.*.promisor` in the repository's
//!   own configuration.
//!
//! Absence has to be established, not assumed: a tree too large to scan within
//! [`MAX_TREE_ENTRIES`], or an attributes file that exists but cannot be read,
//! is refused as "cannot tell".
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
    check_within(repo, commit, root, MAX_TREE_ENTRIES)
}

fn check_within(
    repo: &Repository,
    commit: &Commit,
    root: &Path,
    max_entries: usize,
) -> Result<Vec<String>> {
    let gathered = gather(repo);
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
    let lfs = lfs(repo, commit, &gathered, max_entries);
    if let (Lfs::No | Lfs::Unknown(_), Some(why)) = (&lfs, &gathered.unknown) {
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
    /// Keys (lowercased) and values of the repository's own configuration:
    /// its `config`, files it includes and `config.worktree`; never the
    /// global or system files.
    settings: Vec<(String, String)>,
    /// Attributes files named by `includeIf "hasconfig:…"` files, which
    /// libgit2 does not evaluate, at any level.
    attribute_files: Vec<std::path::PathBuf>,
    /// A condition or an included file that could not be evaluated or read:
    /// what it holds is not "nothing".
    unknown: Option<String>,
}

/// One configuration entry, with the level of the file it came from.
struct Record {
    name: String,
    value: String,
    level: ConfigLevel,
}

fn records(config: &git2::Config) -> Vec<Record> {
    let Ok(mut entries) = config.entries(None) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    while let Some(entry) = entries.next() {
        let Ok(entry) = entry else { continue };
        if let Ok(name) = entry.name() {
            // A key without a value is `true`.
            out.push(Record {
                name: name.to_string(),
                value: entry.value().unwrap_or("true").to_string(),
                level: entry.level(),
            });
        }
    }
    out
}

fn repository_level(level: ConfigLevel) -> bool {
    matches!(
        level,
        ConfigLevel::Local | ConfigLevel::Worktree | ConfigLevel::App
    )
}

/// Nested `hasconfig` includes followed this deep.
const MAX_INCLUDE_DEPTH: usize = 4;

fn gather(repo: &Repository) -> Gathered {
    let mut gathered = Gathered::default();
    let Ok(config) = repo.config() else {
        return gathered;
    };
    let all = records(&config);
    let urls: Vec<String> = all
        .iter()
        .filter(|r| {
            let name = r.name.to_ascii_lowercase();
            name.starts_with("remote.") && name.ends_with(".url")
        })
        .map(|r| r.value.clone())
        .collect();
    gathered.settings = all
        .iter()
        .filter(|r| repository_level(r.level))
        .map(|r| (r.name.to_ascii_lowercase(), r.value.clone()))
        .collect();
    follow_conditional_includes(repo, &all, &urls, 0, &mut gathered);
    gathered
}

/// libgit2 evaluates `gitdir`, `gitdir/i` and `onbranch` conditions but
/// skips `hasconfig:remote.*.url:` ones: evaluate those against the
/// configured remote URLs and read the files they include. A condition of
/// another `hasconfig` form, or a pattern this cannot match exactly, makes
/// the configuration unknown.
fn follow_conditional_includes(
    repo: &Repository,
    records: &[Record],
    urls: &[String],
    depth: usize,
    gathered: &mut Gathered,
) {
    for record in records {
        let lower = record.name.to_ascii_lowercase();
        let Some(condition) = lower
            .strip_prefix("includeif.")
            .and_then(|rest| rest.strip_suffix(".path"))
        else {
            continue;
        };
        if !condition.starts_with("hasconfig:") {
            continue;
        }
        // The pattern keeps its case; the keyword does not.
        let original = &record.name["includeif.".len()..record.name.len() - ".path".len()];
        let Some(pattern) = condition
            .strip_prefix("hasconfig:remote.*.url:")
            .map(|_| &original["hasconfig:remote.*.url:".len()..])
        else {
            gathered.unknown = Some(
                "an includeIf condition other than hasconfig:remote.*.url cannot be evaluated"
                    .to_string(),
            );
            continue;
        };
        if pattern.contains(['[', '\\']) {
            gathered.unknown =
                Some("an includeIf hasconfig pattern uses a character class".to_string());
            continue;
        }
        if !urls.iter().any(|url| wildmatch(pattern, url)) {
            continue;
        }
        if depth >= MAX_INCLUDE_DEPTH {
            gathered.unknown = Some("includeIf files nest too deeply".to_string());
            continue;
        }
        let path = include_path(repo, &record.value, record.level);
        let included = match git2::Config::open(&path) {
            Ok(config) => records_of(&config, record.level),
            // Git ignores an include that is not there.
            Err(_) if !path.exists() => continue,
            Err(_) => {
                gathered.unknown = Some("an included configuration file cannot be read".into());
                continue;
            }
        };
        for r in &included {
            let name = r.name.to_ascii_lowercase();
            if name == "core.attributesfile" {
                gathered.attribute_files.push(expand(&r.value));
            }
            if repository_level(record.level) {
                gathered.settings.push((name, r.value.clone()));
            }
        }
        follow_conditional_includes(repo, &included, urls, depth + 1, gathered);
    }
}

/// The entries of an included file, all taking the level of the including one.
fn records_of(config: &git2::Config, level: ConfigLevel) -> Vec<Record> {
    records(config)
        .into_iter()
        .map(|r| Record { level, ..r })
        .collect()
}

/// An `include` path: `~/` is the home directory, a relative path is relative
/// to the directory of the file holding the include.
fn include_path(repo: &Repository, value: &str, level: ConfigLevel) -> std::path::PathBuf {
    let path = expand(value);
    if path.is_absolute() {
        return path;
    }
    let home = dirs::home_dir().unwrap_or_default();
    let base = match level {
        ConfigLevel::Global => home,
        ConfigLevel::XDG => home.join(".config").join("git"),
        ConfigLevel::System | ConfigLevel::ProgramData => "/etc".into(),
        _ => repo.commondir().to_path_buf(),
    };
    base.join(path)
}

fn expand(value: &str) -> std::path::PathBuf {
    match value.strip_prefix("~/").zip(dirs::home_dir()) {
        Some((rest, home)) => home.join(rest),
        None => value.into(),
    }
}

/// Git's glob for `hasconfig`: `?` is one character, `*` and `**` any run of
/// them. `*` is allowed to cross `/` here, which only makes more includes
/// match than Git's, never fewer.
fn wildmatch(pattern: &str, text: &str) -> bool {
    fn go(p: &[char], t: &[char]) -> bool {
        match p.split_first() {
            None => t.is_empty(),
            Some(('*', rest)) => (0..=t.len()).any(|i| go(rest, &t[i..])),
            Some(('?', rest)) => !t.is_empty() && go(rest, &t[1..]),
            Some((c, rest)) => t.first() == Some(c) && go(rest, &t[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    go(&p, &t)
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
    /// Something sets `filter=lfs` or configures the filter.
    Uses(String),
    /// Absence could not be established.
    Unknown(String),
    No,
}

fn lfs(repo: &Repository, commit: &Commit, gathered: &Gathered, max: usize) -> Lfs {
    if let Some((name, _)) = gathered
        .settings
        .iter()
        .find(|(name, _)| name.starts_with("filter.lfs."))
    {
        return Lfs::Uses(format!("{name} is set in the repository's configuration"));
    }
    let mut unknown = None;
    let included = gathered.attribute_files.iter().map(|path| {
        (
            format!("the attributes file {}", path.display()),
            path.clone(),
        )
    });
    for (label, path) in attribute_files(repo).into_iter().chain(included) {
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

/// Attribute files outside the commit that Git consults for every path.
fn attribute_files(repo: &Repository) -> Vec<(String, std::path::PathBuf)> {
    let mut files = vec![(
        "info/attributes".to_string(),
        repo.commondir().join("info").join("attributes"),
    )];
    let configured = repo
        .config()
        .ok()
        .and_then(|c| c.get_path("core.attributesfile").ok());
    let user = configured.or_else(|| {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".config")))?;
        Some(base.join("git").join("attributes"))
    });
    if let Some(path) = user {
        files.push((format!("the attributes file {}", path.display()), path));
    }
    for system in [
        "/etc/gitattributes",
        "/usr/local/etc/gitattributes",
        "/opt/homebrew/etc/gitattributes",
    ] {
        files.push((system.to_string(), system.into()));
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

    #[test]
    fn hasconfig_patterns_match_like_git_or_wider() {
        assert!(wildmatch(
            "git@github.com:work/**",
            "git@github.com:work/app.git"
        ));
        assert!(wildmatch(
            "git@github.com:work/*",
            "git@github.com:work/team/app.git"
        ));
        assert!(wildmatch(
            "https://*.example.com/**",
            "https://git.example.com/a/b"
        ));
        assert!(wildmatch("ssh://host/repo?.git", "ssh://host/repo1.git"));
        assert!(!wildmatch(
            "git@github.com:work/**",
            "git@github.com:me/app.git"
        ));
        assert!(!wildmatch("https://a/**", "ssh://a/x"));
    }

    fn verdict(t: &TestRepo, max: usize) -> Result<Vec<String>> {
        let repo = t.project.repo().unwrap();
        let commit = repo.head().unwrap().peel_to_commit().unwrap();
        check_within(&repo, &commit, &t.root, max)
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
        let message = refusal(check_within(&git, &commit, &t.root, MAX_TREE_ENTRIES));
        assert!(message.contains("d299/.gitattributes"), "{message}");

        // The same tree without LFS passes.
        let clean = big_commit(&git, 300, 100, Some("*.bin text\n"));
        let commit = git.find_commit(clean).unwrap();
        assert!(check_within(&git, &commit, &t.root, MAX_TREE_ENTRIES).is_ok());
    }

    #[test]
    fn a_tree_beyond_the_bound_is_refused_not_assumed_clean() {
        let t = repo(None);
        let git = t.project.repo().unwrap();
        let clean = big_commit(&git, 50, 100, None);
        let commit = git.find_commit(clean).unwrap();
        assert!(check_within(&git, &commit, &t.root, 10_000).is_ok());
        let message = refusal(check_within(&git, &commit, &t.root, 1_000));
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
        assert!(check_within(&git, &commit, &t.root, 5_000).is_ok());
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

    #[test]
    fn the_repositorys_own_configuration_counts_at_every_file_it_reads() {
        // An included file is part of the repository's configuration.
        let t = repo(None);
        let git = t.project.repo().unwrap();
        let included = t.root.join(".git/lfs.inc");
        std::fs::write(
            &included,
            "[filter \"lfs\"]\n\tsmudge = git-lfs smudge -- %f\n",
        )
        .unwrap();
        git.config()
            .unwrap()
            .set_str("include.path", included.to_str().unwrap())
            .unwrap();
        assert!(refusal(verdict(&t, MAX_TREE_ENTRIES)).contains("filter.lfs.smudge"));

        // The worktree-specific file too.
        let t = repo(None);
        let git = t.project.repo().unwrap();
        std::fs::write(
            t.root.join(".git/config.worktree"),
            "[filter \"lfs\"]\n\trequired = true\n",
        )
        .unwrap();
        git.config()
            .unwrap()
            .set_str("extensions.worktreeConfig", "true")
            .unwrap();
        let _ = git
            .config()
            .unwrap()
            .set_bool("extensions.worktreeConfig", true);
        // Only asserted where libgit2 reads the file; refusing is the safe side.
        let reads = t
            .project
            .repo()
            .unwrap()
            .config()
            .unwrap()
            .get_string("filter.lfs.required")
            .is_ok();
        assert_eq!(verdict(&t, MAX_TREE_ENTRIES).is_err(), reads);
    }

    fn with_remote(t: &TestRepo, url: &str) {
        t.project.repo().unwrap().remote("origin", url).unwrap();
    }

    fn include_if(t: &TestRepo, condition: &str, file_text: &str) {
        let included = t.root.join(".git").join("cond.inc");
        std::fs::write(&included, file_text).unwrap();
        let mut local = std::fs::read_to_string(t.root.join(".git/config")).unwrap();
        local.push_str(&format!(
            "[includeIf \"{condition}\"]\n\tpath = {}\n",
            included.display()
        ));
        std::fs::write(t.root.join(".git/config"), local).unwrap();
    }

    #[test]
    fn a_matching_hasconfig_include_is_read_though_libgit2_skips_it() {
        // filter.lfs through an include that applies to this remote.
        let t = repo(None);
        with_remote(&t, "git@github.com:work/app.git");
        include_if(
            &t,
            "hasconfig:remote.*.url:git@github.com:work/**",
            "[filter \"lfs\"]\n\tsmudge = git-lfs smudge -- %f\n",
        );
        assert!(refusal(verdict(&t, MAX_TREE_ENTRIES)).contains("filter.lfs.smudge"));

        // The same include for another remote applies to nothing here.
        let t = repo(None);
        with_remote(&t, "git@github.com:me/app.git");
        include_if(
            &t,
            "hasconfig:remote.*.url:git@github.com:work/**",
            "[filter \"lfs\"]\n\tsmudge = git-lfs smudge -- %f\n",
        );
        assert!(verdict(&t, MAX_TREE_ENTRIES).is_ok());

        // A core.attributesFile that a matching include supplies is read.
        let t = repo(None);
        with_remote(&t, "https://example.com/team/app.git");
        let attributes = t.root.join(".git/attrs");
        std::fs::write(&attributes, "*.psd filter=lfs\n").unwrap();
        include_if(
            &t,
            "hasconfig:remote.*.url:https://example.com/**",
            &format!("[core]\n\tattributesFile = {}\n", attributes.display()),
        );
        assert!(refusal(verdict(&t, MAX_TREE_ENTRIES)).contains("attributes file"));

        // So are partial-clone keys.
        let t = repo(None);
        with_remote(&t, "https://example.com/team/app.git");
        include_if(
            &t,
            "hasconfig:remote.*.url:https://example.com/**",
            "[remote \"origin\"]\n\tpromisor = 2\n",
        );
        assert!(refusal(verdict(&t, MAX_TREE_ENTRIES)).contains("partial clone"));
    }

    #[test]
    fn a_condition_that_cannot_be_evaluated_is_refused_not_ignored() {
        for condition in [
            "hasconfig:remote.*.url:https://[ab].example.com/**",
            "hasconfig:branch.*.remote:origin",
        ] {
            let t = repo(None);
            with_remote(&t, "https://a.example.com/x.git");
            include_if(&t, condition, "[core]\n\tfoo = 1\n");
            let message = refusal(verdict(&t, MAX_TREE_ENTRIES));
            assert!(
                message.contains("cannot read this repository's configuration"),
                "{message}"
            );
        }
        // An include that is simply missing is ignored, as Git does.
        let t = repo(None);
        with_remote(&t, "https://a.example.com/x.git");
        include_if(&t, "hasconfig:remote.*.url:https://a.example.com/**", "");
        std::fs::remove_file(t.root.join(".git/cond.inc")).unwrap();
        assert!(verdict(&t, MAX_TREE_ENTRIES).is_ok());
    }

    #[test]
    fn a_promisor_value_git_cannot_read_is_refused() {
        let t = repo(None);
        with_remote(&t, "https://a.example.com/x.git");
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
            check_within(&git, &commit, &t.root, usize::MAX).unwrap();
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
            let verdict = check_within(&other, &commit, &t.root, usize::MAX);
            eprintln!(
                "that repository, {count} entries: {:?} ({})",
                started.elapsed(),
                if verdict.is_ok() { "clean" } else { "refused" }
            );
        }
    }
}
