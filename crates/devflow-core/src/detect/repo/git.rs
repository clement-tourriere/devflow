//! Git ignore status, answered without libgit2.
//!
//! libgit2 opens `.git/config`, `.git/info/exclude`, `.gitignore` files and
//! the index by itself, so a FIFO in any of those places blocks the scan, a
//! `[include]` or `core.excludesFile` can pull in files from anywhere, and no
//! byte limit applies. This module instead reads every input through the
//! same bounded, confined reader as the rest of the scan and feeds the lines
//! to an in-process matcher (`ignore::gitignore`, which compiles patterns but
//! never touches the disk here).
//!
//! # What is consulted
//!
//! In Git's precedence order: the `.gitignore` of each directory from the
//! one holding the path up to the root (deeper files win), then
//! `$GIT_DIR/info/exclude`. A tracked file is never ignored, so the index is
//! read as well (see [`super::gitindex`]).
//!
//! # What is not
//!
//! The user's global excludes (`core.excludesFile`, `~/.config/git/ignore`)
//! and `.git/config` are not read: a path ignored only by a global rule is
//! reported as not ignored. Symlinked `.gitignore` files are skipped, as Git
//! (2.32 and later) skips them. `core.ignorecase` is not applied.
//!
//! The status is `None` (unknown) whenever an input exists but cannot be used
//! safely or completely: not a regular file, over the size cap, too many
//! patterns, an index that cannot be parsed (or is split or sparse, or expands
//! to more paths than the scan allows), or a `.git` file whose target cannot
//! be verified.
//!
//! # Linked worktrees, submodules and separate git directories
//!
//! A `.git` file points at a git directory that lives outside the checkout.
//! That directory is opened as a second sandbox and its rules are only used
//! when the relationship is verified. A linked worktree is: its git
//! directory names a common directory (`commondir`) and a `gitdir` file that
//! points back at this very `.git` file. Only `HEAD`, `commondir`, `gitdir`,
//! `info/exclude` and `index` are read from it; they are parsed into yes/no
//! answers and never retained.
//!
//! A git directory without `commondir` (a submodule's, or one made by
//! `--separate-git-dir`) has no such back pointer in a file the scan reads:
//! the only one is `core.worktree` in `.git/config`, and reading Git config
//! is exactly what this module avoids. A directory holding nothing but a
//! `HEAD` and an `info/exclude` is indistinguishable from the real thing, so
//! such a checkout is still reported as a repository but its ignore status is
//! unknown, with a warning.
use super::sandbox::{Kind, Sandbox};
use super::{Fetch, MAX_FILE_BYTES, Repo, is_plain_relative, join};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// Largest index the scan reads; a bigger index makes tracked status unknown.
const MAX_INDEX_BYTES: u64 = 8 * 1024 * 1024;
/// Most ignore patterns compiled over the whole scan.
const MAX_PATTERNS: usize = 10_000;
/// Longest pattern line accepted.
const MAX_PATTERN_BYTES: usize = 2048;
/// Name that stands for "something inside the directory" when asking whether
/// a directory's contents are ignored (`/tmp/*` ignores `tmp/anything`).
const PROBE: &str = ".devflow-probe";

/// The patterns of one ignore file.
#[derive(Clone)]
enum Layer {
    /// There is no such file.
    None,
    /// The compiled patterns.
    Rules(Rc<Gitignore>),
    /// The file exists but could not be used completely.
    Unknown,
}

/// Where the git directory is, when it can be trusted.
struct GitDirs {
    /// The directory holding `index`.
    gitdir: Sandbox,
    /// The common directory (`info/exclude`) of a linked worktree.
    common: Option<Sandbox>,
}

impl GitDirs {
    fn common(&self) -> &Sandbox {
        self.common.as_ref().unwrap_or(&self.gitdir)
    }
}

/// Everything learned about the root's Git state, computed on first use.
pub(in crate::detect) struct GitState {
    repository: bool,
    dirs: Option<GitDirs>,
    exclude: OnceCell<Layer>,
    per_dir: RefCell<HashMap<String, Layer>>,
    patterns: Cell<usize>,
    tracked: OnceCell<Option<HashSet<Vec<u8>>>>,
}

impl GitState {
    fn new(repository: bool, dirs: Option<GitDirs>) -> Self {
        Self {
            repository,
            dirs,
            exclude: OnceCell::new(),
            per_dir: RefCell::new(HashMap::new()),
            patterns: Cell::new(0),
            tracked: OnceCell::new(),
        }
    }
}

impl Repo {
    fn git_state(&self) -> &GitState {
        self.git.get_or_init(|| discover(self))
    }

    /// Whether the root is the top of a Git checkout (a `.git` directory, or a
    /// `.git` file pointing at a git directory).
    pub(crate) fn is_git(&self) -> bool {
        self.git_state().repository
    }

    /// Whether Git ignore rules cover `rel`. `None` when the root is not a
    /// Git checkout or the answer cannot be given safely (see the module
    /// documentation).
    ///
    /// A directory counts as ignored when the directory itself is ignored
    /// (`node_modules/`) or everything inside it is (`/tmp/*`, which Rails and
    /// Laravel use next to a tracked `.keep`). A file that is tracked is never
    /// ignored, even if a rule matches it.
    pub(crate) fn git_ignored(&self, rel: &str, is_dir: bool) -> Option<bool> {
        let git = self.git_state();
        if !git.repository || rel.is_empty() || rel == "." || !is_plain_relative(rel) {
            return None;
        }
        git.dirs.as_ref()?;
        if is_dir {
            if git.is_ignored(self, rel, true)? {
                return Some(true);
            }
            return git.is_ignored(self, &join(rel, PROBE), false);
        }
        if !git.is_ignored(self, rel, false)? {
            return Some(false);
        }
        git.is_tracked(self, rel).map(|tracked| !tracked)
    }
}

/// Find out whether the root is a checkout and where its git directory is.
fn discover(repo: &Repo) -> GitState {
    let meta = match repo.fs.metadata(".git") {
        Ok(meta) => meta,
        Err(denied) => {
            repo.explain(".git", denied);
            return GitState::new(false, None);
        }
    };
    match meta.kind {
        Kind::Dir => match repo.fs.open_dir(".git") {
            Ok(gitdir) if is_git_dir(&gitdir) => GitState::new(
                true,
                Some(GitDirs {
                    gitdir,
                    common: None,
                }),
            ),
            _ => GitState::new(false, None),
        },
        Kind::File => linked(repo),
        Kind::Other => GitState::new(false, None),
    }
}

fn is_git_dir(dir: &Sandbox) -> bool {
    dir.metadata("HEAD").is_ok_and(|m| m.kind == Kind::File)
}

/// A first line of at most a few KiB from a small file of a git directory.
fn small_file(repo: &Repo, dir: &Sandbox, name: &str) -> Fetch {
    repo.fetch(dir, name, &format!(".git/{name}"), 4096, false, true)
}

fn first_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

/// A `.git` file: `gitdir: <path>` to a linked worktree's or submodule's git
/// directory.
fn linked(repo: &Repo) -> GitState {
    let not_a_checkout = || GitState::new(false, None);
    let Fetch::Data(bytes) = repo.fetch(&repo.fs, ".git", ".git", 4096, false, true) else {
        return not_a_checkout();
    };
    let line = first_line(&bytes);
    let Some(target) = line
        .strip_prefix("gitdir:")
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return not_a_checkout();
    };
    let target = repo.root.join(target);
    let Ok(gitdir) = Sandbox::open_ambient(&target) else {
        repo.warn(".git", "the git directory it points to cannot be opened");
        return not_a_checkout();
    };
    if !is_git_dir(&gitdir) {
        return not_a_checkout();
    }
    // It is a checkout; whether its git directory can be trusted is the rest.
    let unverified = |why: &str| {
        repo.warn(".git", format!("{why}; Git ignore status is unavailable"));
        GitState::new(true, None)
    };
    let common = match small_file(repo, &gitdir, "commondir") {
        // A submodule's or separate git directory has no common directory and
        // nothing this module reads says that it belongs to this checkout.
        Fetch::Missing => {
            return unverified(
                "the git directory is not a linked worktree's, so it cannot be tied to this checkout",
            );
        }
        Fetch::Unusable => return unverified("the worktree's commondir is unreadable"),
        Fetch::Data(bytes) => {
            let Ok(common) = Sandbox::open_ambient(&target.join(first_line(&bytes))) else {
                return unverified("the worktree's common directory cannot be opened");
            };
            if !is_git_dir(&common) {
                return unverified("the worktree's common directory is not a git directory");
            }
            common
        }
    };
    // A linked worktree's git directory names the `.git` file that owns it.
    let Fetch::Data(bytes) = small_file(repo, &gitdir, "gitdir") else {
        return unverified("the worktree has no link back to this directory");
    };
    let owner = std::fs::canonicalize(first_line(&bytes));
    let ours = std::fs::canonicalize(repo.root.join(".git"));
    if owner.is_err() || owner.ok() != ours.ok() {
        return unverified("the worktree's link does not point back to this directory");
    }
    GitState::new(
        true,
        Some(GitDirs {
            gitdir,
            common: Some(common),
        }),
    )
}

impl GitState {
    /// `$GIT_DIR/info/exclude`.
    fn exclude_layer(&self, repo: &Repo) -> Layer {
        self.exclude
            .get_or_init(|| match &self.dirs {
                None => Layer::Unknown,
                Some(dirs) => self.load(
                    repo,
                    dirs.common(),
                    "info/exclude",
                    ".git/info/exclude",
                    true,
                ),
            })
            .clone()
    }

    /// The `.gitignore` of directory `dir` (`""` or `"."` for the root).
    fn dir_layer(&self, repo: &Repo, dir: &str) -> Layer {
        let key = if dir.is_empty() { "." } else { dir };
        if let Some(layer) = self.per_dir.borrow().get(key) {
            return layer.clone();
        }
        let path = join(key, ".gitignore");
        // Git does not read a symlinked .gitignore.
        let layer = if repo.is_symlink(&path) {
            Layer::None
        } else {
            self.load(repo, &repo.fs, &path, &path, false)
        };
        self.per_dir
            .borrow_mut()
            .insert(key.to_string(), layer.clone());
        layer
    }

    fn load(&self, repo: &Repo, fs: &Sandbox, rel: &str, label: &str, trusted: bool) -> Layer {
        match repo.fetch(fs, rel, label, MAX_FILE_BYTES, true, trusted) {
            Fetch::Missing => Layer::None,
            Fetch::Unusable => Layer::Unknown,
            Fetch::Data(bytes) => self.compile(repo, label, &bytes),
        }
    }

    fn compile(&self, repo: &Repo, label: &str, bytes: &[u8]) -> Layer {
        let text = String::from_utf8_lossy(bytes);
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        let mut builder = GitignoreBuilder::new(".");
        for line in text.lines() {
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            if line.len() > MAX_PATTERN_BYTES || self.patterns.get() >= MAX_PATTERNS {
                repo.warn(
                    label,
                    "too many or too long ignore patterns; Git ignore status is unavailable",
                );
                return Layer::Unknown;
            }
            self.patterns.set(self.patterns.get() + 1);
            // A pattern that does not compile is skipped, as Git skips it.
            let _ = builder.add_line(None, line);
        }
        match builder.build() {
            Ok(rules) => Layer::Rules(Rc::new(rules)),
            Err(_) => Layer::Unknown,
        }
    }

    /// Whether `rel` is ignored, or `None` when an input was unusable.
    ///
    /// Walks the path from the root: an ignored directory ignores everything
    /// inside it, and nothing below can re-include it.
    fn is_ignored(&self, repo: &Repo, rel: &str, is_dir: bool) -> Option<bool> {
        let parts: Vec<&str> = rel
            .split('/')
            .filter(|p| !p.is_empty() && *p != ".")
            .collect();
        // The `.gitignore` files met so far, with the depth they live at.
        let mut stack: Vec<(usize, Rc<Gitignore>)> = Vec::new();
        for depth in 0..parts.len() {
            match self.dir_layer(repo, &parts[..depth].join("/")) {
                Layer::Unknown => return None,
                Layer::Rules(rules) => stack.push((depth, rules)),
                Layer::None => {}
            }
            let dir = depth + 1 < parts.len() || is_dir;
            let mut verdict = None;
            for (at, rules) in stack.iter().rev() {
                let matched = rules.matched(parts[*at..=depth].join("/"), dir);
                if !matched.is_none() {
                    verdict = Some(matched.is_ignore());
                    break;
                }
            }
            if verdict.is_none() {
                match self.exclude_layer(repo) {
                    Layer::Unknown => return None,
                    Layer::Rules(rules) => {
                        let matched = rules.matched(parts[..=depth].join("/"), dir);
                        if !matched.is_none() {
                            verdict = Some(matched.is_ignore());
                        }
                    }
                    Layer::None => {}
                }
            }
            if verdict == Some(true) {
                return Some(true);
            }
        }
        Some(false)
    }

    /// Whether the index has an entry for `rel`; `None` when the index cannot
    /// be read completely.
    fn is_tracked(&self, repo: &Repo, rel: &str) -> Option<bool> {
        let tracked = self.tracked.get_or_init(|| self.load_index(repo));
        Some(tracked.as_ref()?.contains(rel.as_bytes()))
    }

    fn load_index(&self, repo: &Repo) -> Option<HashSet<Vec<u8>>> {
        let dirs = self.dirs.as_ref()?;
        match repo.fetch(
            &dirs.gitdir,
            "index",
            ".git/index",
            MAX_INDEX_BYTES,
            true,
            true,
        ) {
            // A repository without an index has nothing tracked yet.
            Fetch::Missing => Some(HashSet::new()),
            Fetch::Unusable => None,
            Fetch::Data(bytes) => {
                // Expanded paths count against the scan's byte budget, like
                // the bytes read to get here.
                let mut budget = repo.bytes_left();
                let outcome = super::gitindex::tracked_paths(&bytes, &mut budget);
                repo.spend_bytes(repo.bytes_left() - budget);
                match outcome {
                    Ok(paths) => Some(paths),
                    Err(reason) => {
                        repo.warn(
                            ".git/index",
                            format!("{reason}; tracked files cannot be told from ignored ones"),
                        );
                        None
                    }
                }
            }
        }
    }
}
