//! `.worktreeinclude`: the files Git ignores that a new worktree should still
//! get (`.env`, local certificates, a secrets file).
//!
//! Claude Code, Codex, Conductor and worktrunk read the same file, so a
//! project that lists its local files once serves every tool. The file has
//! `.gitignore` syntax; a file is carried over when a pattern matches it **and**
//! Git ignores it, so a tracked file is never duplicated (the checkout already
//! has it) and a pattern can never name something that is not local state.
//! devflow merges what matches with `[worktree] copy` and clones the result
//! the same way, reporting each entry like a copy entry.
//!
//! Matching works from what Git reports as ignored in the source checkout. An
//! ignored directory as a whole (`node_modules/`) is one entry when a pattern
//! matches it; it is looked inside only when a pattern names it (a pattern
//! with a path under it, or `**/` followed by its name), because looking
//! inside every ignored directory would walk dependency trees. The walk is
//! bounded and says so when it stopped.
use anyhow::{Context, Result};
use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::collections::BTreeSet;
use std::path::Path;

/// The file's name, at the root of the checkout.
pub const FILE: &str = ".worktreeinclude";

/// Largest `.worktreeinclude` read.
const MAX_BYTES: u64 = 256 * 1024;
/// Entries looked at inside ignored directories, over the whole run.
const MAX_WALK_ENTRIES: usize = 20_000;
/// Deepest level looked at inside an ignored directory.
const MAX_DEPTH: usize = 12;

/// What the file selected.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Included {
    /// Relative paths, `/`-separated, sorted; none is inside another.
    pub entries: Vec<String>,
    /// Lines that are not patterns, and limits that were reached.
    pub warnings: Vec<String>,
}

/// The files `.worktreeinclude` selects in `source`, the checkout payloads are
/// cloned from. The file is read from there, else from `fallback` (the main
/// checkout). No file selects nothing.
pub fn resolve(source: &Path, fallback: &Path) -> Result<Included> {
    let Some((root, text)) = read(source, fallback)? else {
        return Ok(Included::default());
    };
    let mut warnings = Vec::new();
    let patterns = parse(&root, &text, &mut warnings);
    let Some((matcher, lines)) = patterns else {
        return Ok(Included {
            entries: Vec::new(),
            warnings,
        });
    };
    let repo = git2::Repository::open(source)
        .with_context(|| format!("{} is not a Git checkout", source.display()))?;
    let mut options = git2::StatusOptions::new();
    options
        .include_ignored(true)
        .include_untracked(false)
        .recurse_ignored_dirs(false)
        .exclude_submodules(true);
    let statuses = repo
        .statuses(Some(&mut options))
        .context("cannot list the ignored files")?;
    let mut found = BTreeSet::new();
    let mut budget = MAX_WALK_ENTRIES;
    let mut cut = false;
    for status in statuses.iter() {
        if !status.status().contains(git2::Status::IGNORED) {
            continue;
        }
        let Ok(path) = status.path() else {
            continue;
        };
        let is_dir = path.ends_with('/');
        let relative = path.trim_end_matches('/');
        if selects(&matcher, relative, is_dir) {
            found.insert(relative.to_string());
        } else if is_dir && may_hold_matches(&lines, relative) {
            walk(
                source,
                relative,
                1,
                &matcher,
                &mut found,
                &mut budget,
                &mut cut,
            );
        }
    }
    if cut {
        warnings.push(format!(
            "{FILE}: looked inside ignored directories only as far as {MAX_WALK_ENTRIES} entries or {MAX_DEPTH} levels; name the files more precisely to be sure they are found"
        ));
    }
    Ok(Included {
        entries: outermost(found),
        warnings,
    })
}

/// The file and the checkout it came from.
fn read(source: &Path, fallback: &Path) -> Result<Option<(std::path::PathBuf, String)>> {
    for root in [source, fallback] {
        let path = root.join(FILE);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_file() && meta.len() <= MAX_BYTES => {
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("cannot read {}", path.display()))?;
                return Ok(Some((root.to_path_buf(), text)));
            }
            // A link, a directory or an oversized file is not read: it would
            // decide which files leave this checkout.
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read {}", path.display()));
            }
        }
    }
    Ok(None)
}

/// The patterns (as a matcher and as their text, `!` lines left out).
fn parse(root: &Path, text: &str, warnings: &mut Vec<String>) -> Option<(Gitignore, Vec<String>)> {
    let mut builder = GitignoreBuilder::new(root);
    let mut lines = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() || trimmed.trim_start().starts_with('#') {
            continue;
        }
        match builder.add_line(None, trimmed) {
            Ok(_) => {
                if !trimmed.starts_with('!') {
                    lines.push(trimmed.trim().to_string());
                }
            }
            Err(_) => warnings.push(format!(
                "{FILE}, line {}: not a pattern; ignored",
                index + 1
            )),
        }
    }
    if lines.is_empty() {
        return None;
    }
    builder.build().ok().map(|matcher| (matcher, lines))
}

/// Whether a pattern selects `relative` (or a directory it is in).
fn selects(matcher: &Gitignore, relative: &str, is_dir: bool) -> bool {
    matches!(
        matcher.matched_path_or_any_parents(Path::new(relative), is_dir),
        Match::Ignore(_)
    )
}

/// Whether a pattern names the ignored directory `dir`, so that what is inside
/// is worth looking at: it starts with the directory's path, or is `**/`
/// followed by the directory's own name.
fn may_hold_matches(patterns: &[String], dir: &str) -> bool {
    let name = dir.rsplit('/').next().unwrap_or(dir);
    patterns.iter().any(|pattern| {
        let pattern = pattern.trim_start_matches('/');
        if let Some(rest) = pattern.strip_prefix("**/") {
            return rest.split('/').next() == Some(name) && rest.contains('/');
        }
        pattern.starts_with(&format!("{dir}/")) && pattern.len() > dir.len() + 1
    })
}

fn walk(
    root: &Path,
    dir: &str,
    depth: usize,
    matcher: &Gitignore,
    found: &mut BTreeSet<String>,
    budget: &mut usize,
    cut: &mut bool,
) {
    if depth > MAX_DEPTH {
        *cut = true;
        return;
    }
    let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
        return;
    };
    for entry in entries.flatten() {
        if *budget == 0 {
            *cut = true;
            return;
        }
        *budget -= 1;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name == ".git" {
            continue;
        }
        let relative = format!("{dir}/{name}");
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let is_dir = file_type.is_dir();
        if selects(matcher, &relative, is_dir) {
            found.insert(relative);
        } else if is_dir {
            walk(root, &relative, depth + 1, matcher, found, budget, cut);
        }
    }
}

/// `found` without the entries that are inside another entry.
fn outermost(found: BTreeSet<String>) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    for entry in found {
        let covered = kept.iter().any(|parent| {
            entry
                .strip_prefix(parent.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
        });
        if !covered && crate::config::validate_relative(&entry).is_ok() {
            kept.push(entry);
        }
    }
    kept
}

/// `entry` without the spellings that name the same path: `.` components,
/// repeated and trailing separators (`./certs//local.pem/` is
/// `certs/local.pem`). `..` and absolute paths are left for
/// [`crate::config::validate_relative`] to refuse. An entry that normalizes to
/// nothing is returned as it is.
fn normalize(entry: &str) -> String {
    let parts: Vec<&str> = entry
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    if parts.is_empty() || entry.starts_with('/') {
        return entry.to_string();
    }
    parts.join("/")
}

/// Whether `inner` is inside the directory entry `outer` (both normalized).
fn is_inside(outer: &str, inner: &str) -> bool {
    inner
        .strip_prefix(outer)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// `copy` plus the included entries that no `copy` entry already covers, and
/// how many were added. Entries are compared in their normalized spelling
/// (see [`normalize`]) and returned in it.
///
/// An entry comes before the entries inside it. Entries are installed one
/// after the other, and one that is installed under an existing parent's name
/// (`certs/local.pem` creates `certs`) would make the copy of the whole
/// directory find its destination taken and leave the rest of it behind;
/// copied first, the directory brings what is inside it, and the entries after
/// it report that they were already there. The order is otherwise the
/// caller's.
pub fn merged(copy: &[String], included: &[String]) -> (Vec<String>, usize) {
    let copy: Vec<String> = copy.iter().map(|entry| normalize(entry)).collect();
    let mut all: Vec<String> = Vec::new();
    let mut added = 0;
    let place = |all: &mut Vec<String>, entry: &String| {
        if all.contains(entry) {
            return;
        }
        match all.iter().position(|existing| is_inside(entry, existing)) {
            Some(at) => all.insert(at, entry.clone()),
            None => all.push(entry.clone()),
        }
    };
    for entry in &copy {
        place(&mut all, entry);
    }
    for entry in included.iter().map(|entry| normalize(entry)) {
        let covered = copy
            .iter()
            .any(|existing| *existing == entry || is_inside(existing, &entry));
        if !covered {
            place(&mut all, &entry);
            added += 1;
        }
    }
    (all, added)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::testutil;

    fn write(root: &Path, relative: &str, text: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn setup(ignore: &str, include: &str) -> testutil::TestRepo {
        let t = testutil::repo(None);
        write(
            &t.root,
            ".gitignore",
            &format!("{ignore}\ndevflow.local.toml\n"),
        );
        write(&t.root, "tracked.env", "tracked\n");
        write(&t.root, FILE, include);
        let repo = t.project.repo().unwrap();
        testutil::commit_all(&repo, "ignore rules");
        t
    }

    #[test]
    fn selects_ignored_files_that_match_and_never_tracked_ones() {
        let t = setup(
            ".env\n.env.*\n*.pem\ncerts/\nnode_modules/\n",
            ".env\n.env.local\n*.env\ncerts/local/**\n*.pem\nnode_modules/\n",
        );
        write(&t.root, ".env", "A=1\n");
        write(&t.root, ".env.local", "B=2\n");
        write(&t.root, ".env.other", "C=3\n");
        write(&t.root, "key.pem", "k\n");
        write(&t.root, "certs/local/dev.crt", "c\n");
        write(&t.root, "certs/prod/p.crt", "p\n");
        write(&t.root, "node_modules/x/index.js", "x\n");
        let found = resolve(&t.root, &t.root).unwrap();
        assert!(found.warnings.is_empty(), "{:?}", found.warnings);
        // `tracked.env` matches `*.env` but is tracked: never an entry. The
        // ignored directory is one entry; `certs/` is named by a path under it.
        assert_eq!(
            found.entries,
            [
                ".env",
                ".env.local",
                "certs/local/dev.crt",
                "key.pem",
                "node_modules"
            ]
        );
    }

    #[test]
    fn negation_comments_blank_lines_and_bad_lines() {
        let t = setup(".env*\n", "# local files\n\n.env*\n!.env.example\n");
        write(&t.root, ".env", "A=1\n");
        write(&t.root, ".env.local", "B=2\n");
        let found = resolve(&t.root, &t.root).unwrap();
        assert_eq!(found.entries, [".env", ".env.local"]);
    }

    #[test]
    fn a_pattern_looks_inside_an_ignored_directory_only_when_it_names_it() {
        let t = setup(
            ".claude/\nvendor/\ndeps/\n",
            "**/.claude/skills/*.md\nvendor/**/config.json\n*.json\n",
        );
        write(&t.root, ".claude/skills/review.md", "r\n");
        write(&t.root, ".claude/other.txt", "o\n");
        write(&t.root, "vendor/a/b/config.json", "{}\n");
        write(&t.root, "vendor/a/other.json", "{}\n");
        // `*.json` is a bare name: it does not walk `deps/`.
        write(&t.root, "deps/deep/pkg.json", "{}\n");
        let found = resolve(&t.root, &t.root).unwrap();
        // Inside `vendor/`, which a pattern names, every pattern applies.
        assert_eq!(
            found.entries,
            [
                ".claude/skills/review.md",
                "vendor/a/b/config.json",
                "vendor/a/other.json"
            ]
        );
    }

    #[test]
    fn no_file_a_link_or_nothing_that_matches_select_nothing() {
        let t = testutil::repo(None);
        assert_eq!(resolve(&t.root, &t.root).unwrap(), Included::default());
        write(&t.root, FILE, ".env\n");
        // Nothing ignored exists.
        assert!(resolve(&t.root, &t.root).unwrap().entries.is_empty());
        #[cfg(unix)]
        {
            std::fs::remove_file(t.root.join(FILE)).unwrap();
            write(&t.root, "elsewhere", ".env\n");
            std::os::unix::fs::symlink(t.root.join("elsewhere"), t.root.join(FILE)).unwrap();
            write(&t.root, ".env", "A=1\n");
            assert!(resolve(&t.root, &t.root).unwrap().entries.is_empty());
        }
    }

    #[test]
    fn the_main_checkouts_file_serves_a_source_that_does_not_have_one() {
        let t = setup(".env\n", ".env\n");
        write(&t.root, ".env", "A=1\n");
        let other = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(other.path()).unwrap();
        write(other.path(), ".gitignore", ".env\n");
        write(other.path(), ".env", "Z=1\n");
        testutil::commit_all(&repo, "x");
        let found = resolve(other.path(), &t.root).unwrap();
        assert_eq!(found.entries, [".env"]);
    }

    #[test]
    fn merging_keeps_copy_entries_first_and_skips_what_they_cover() {
        let copy = vec!["node_modules".to_string(), ".env".to_string()];
        let included = vec![
            ".env".to_string(),
            "node_modules/.cache".to_string(),
            "certs/dev.crt".to_string(),
        ];
        let (all, added) = merged(&copy, &included);
        assert_eq!(all, ["node_modules", ".env", "certs/dev.crt"]);
        assert_eq!(added, 1);
    }

    #[test]
    fn a_directory_is_copied_before_the_entries_inside_it() {
        let copy = vec!["certs/local.pem".to_string(), ".env".to_string()];
        let included = vec!["certs".to_string()];
        let (all, added) = merged(&copy, &included);
        assert_eq!(all, ["certs", "certs/local.pem", ".env"]);
        assert_eq!(added, 1);
        // Also among the `copy` entries themselves.
        let (all, _) = merged(&["a/b/c".to_string(), "a/b".to_string()], &[]);
        assert_eq!(all, ["a/b", "a/b/c"]);
    }

    #[test]
    fn spellings_of_one_path_are_one_path() {
        for spelling in [
            "./certs/local.pem",
            "certs//local.pem",
            "certs/./local.pem/",
            "./certs/./local.pem",
        ] {
            let (all, added) = merged(&[spelling.to_string()], &["certs".to_string()]);
            assert_eq!(all, ["certs", "certs/local.pem"], "{spelling}");
            assert_eq!(added, 1, "{spelling}");
        }
        // The included entry in another spelling is covered by, or equal to, a copy entry.
        let (all, added) = merged(
            &["certs".to_string()],
            &["./certs/".to_string(), "certs/./a".to_string()],
        );
        assert_eq!(all, ["certs"]);
        assert_eq!(added, 0);
        let (all, _) = merged(&[".env".to_string(), "./.env".to_string()], &[]);
        assert_eq!(all, [".env"]);
        // What cannot be normalized is passed on for validation to refuse.
        let (all, _) = merged(&["../x".to_string(), ".".to_string()], &[]);
        assert_eq!(all, ["../x", "."]);
    }

    #[test]
    fn the_whole_directory_arrives_when_a_file_inside_it_is_also_listed() {
        let t = testutil::repo(None);
        let linked = t.dir.path().join("linked");
        std::fs::create_dir_all(&linked).unwrap();
        write(&t.root, "certs/local.pem", "pem\n");
        write(&t.root, "certs/other.crt", "crt\n");
        let (entries, _) = merged(&["certs/local.pem".to_string()], &["certs".to_string()]);
        let reports = crate::cow::copy_paths(&t.root, &linked, &entries).unwrap();
        assert_eq!(
            std::fs::read_to_string(linked.join("certs/other.crt")).unwrap(),
            "crt\n",
            "{reports:?}"
        );
        assert_eq!(
            std::fs::read_to_string(linked.join("certs/local.pem")).unwrap(),
            "pem\n"
        );
        assert!(matches!(
            reports[1].result,
            crate::cow::EntryResult::AlreadyPresent
        ));
    }
}
