//! Sandboxed, read-only access to the scanned directory.
//!
//! Every path the scan touches goes through [`Repo`], which
//!
//! - confines access to the root with directory-relative opens (the
//!   `sandbox` submodule, built on `cap-std`): a symlink that leaves the
//!   checkout is refused when it is followed, and swapping a file or
//!   directory for a link or a FIFO while the scan runs cannot redirect or
//!   block it;
//! - validates the descriptor it opened (a regular file within the size cap)
//!   instead of trusting an earlier look at the name;
//! - keeps credential files away from general readers: `.env*`, `.devflow/`,
//!   `.git/` and well-known credential stores are only opened by the keys-only
//!   [`Repo::open_env`], whatever symlink or hard link they are reached
//!   through. Three layers, because a name checked is not a file opened: the
//!   resolved name is checked; the path is then opened without following any
//!   link and re-verified with `lstat`, so a name swapped for a symlink
//!   between check and open is refused; and the opened descriptor is refused
//!   when its identity is that of a known protected file or when it has more
//!   than one hard link;
//! - charges every read byte and every filesystem visit against budgets shared
//!   by the whole scan, so work is bounded, not just results;
//! - answers Git questions from files it reads itself (the `git` submodule).
use super::facts::ScanWarning;
use anyhow::{Context, Result, bail};
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::path::{Component, Path};
use std::rc::Rc;

mod git;
mod gitindex;
mod sandbox;

pub(crate) use sandbox::Kind;
use sandbox::{Denied, EntryType, Sandbox};

/// Largest file the scan reads; bigger files are skipped with a warning.
pub(crate) const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Most entries examined in one directory, including the ones rejected.
const MAX_DIR_ENTRIES: usize = 4096;
/// Most bytes read from files over the whole scan.
const MAX_TOTAL_BYTES: u64 = 32 * 1024 * 1024;
/// Most filesystem visits (directory entries examined, paths resolved, files
/// opened) over the whole scan.
const MAX_WORK: u64 = 250_000;
/// Most warnings kept; the rest are summarized in one.
const MAX_WARNINGS: usize = 200;

/// The entries of one directory, by name.
#[derive(Debug, Default)]
pub(crate) struct DirIndex {
    entries: BTreeMap<String, Kind>,
    /// Names the directory has that are not in `entries`: symlinks that
    /// leave the repository or dangle, and entries the listing could not
    /// classify. They exist; what they hold is unknown.
    uninspectable: BTreeSet<String>,
    /// More entries existed than the listing examined, so a name that is
    /// absent may still exist.
    truncated: bool,
}

impl DirIndex {
    pub(crate) fn has_file(&self, name: &str) -> bool {
        self.entries.get(name) == Some(&Kind::File)
    }

    pub(crate) fn has_dir(&self, name: &str) -> bool {
        self.entries.get(name) == Some(&Kind::Dir)
    }

    /// Whether the directory has an entry called `name` of any kind, a symlink
    /// leaving the repository or one that dangles included. Nothing is
    /// followed: this says that something is there, not what.
    pub(crate) fn exists(&self, name: &str) -> bool {
        self.entries.contains_key(name) || self.uninspectable.contains(name)
    }

    /// Whether an entry called `name` exists that the scan cannot read as a
    /// file: a link out of the repository, a dangling one, a directory, a
    /// socket.
    pub(crate) fn hides(&self, name: &str) -> bool {
        self.exists(name) && !self.has_file(name)
    }

    /// More entries existed than were listed: a name that is absent may still
    /// exist.
    pub(crate) fn truncated(&self) -> bool {
        self.truncated
    }

    /// The first of `names` that exists as a file.
    pub(crate) fn first_file<'a>(&self, names: &[&'a str]) -> Option<&'a str> {
        names.iter().copied().find(|n| self.has_file(n))
    }

    /// Names of regular files, sorted.
    pub(crate) fn files(&self) -> impl Iterator<Item = &str> {
        self.entries
            .iter()
            .filter(|(_, k)| **k == Kind::File)
            .map(|(n, _)| n.as_str())
    }

    /// Names of directories, sorted.
    pub(crate) fn dirs(&self) -> impl Iterator<Item = &str> {
        self.entries
            .iter()
            .filter(|(_, k)| **k == Kind::Dir)
            .map(|(n, _)| n.as_str())
    }
}

/// The outcome of reading one file.
pub(super) enum Fetch {
    /// The bytes read (at most the requested limit).
    Data(Vec<u8>),
    /// There is no such file.
    Missing,
    /// The file exists but may not or cannot be used; a warning says why where
    /// that is not obvious.
    Unusable,
}

/// A `.env*` file opened for streaming; reads are charged to the scan's byte
/// budget and stop at the size cap.
pub(crate) struct EnvStream<'a> {
    file: cap_std::fs::File,
    repo: &'a Repo,
    path: String,
    remaining: u64,
}

impl Read for EnvStream<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 || buf.is_empty() {
            return Ok(0);
        }
        let budget = MAX_TOTAL_BYTES.saturating_sub(self.repo.bytes.get());
        if budget == 0 {
            self.repo
                .warn(&self.path, "read budget for this scan is exhausted");
            return Ok(0);
        }
        let wanted = (buf.len() as u64).min(self.remaining).min(budget) as usize;
        let read = self.file.read(&mut buf[..wanted])?;
        self.remaining -= read as u64;
        self.repo.bytes.set(self.repo.bytes.get() + read as u64);
        Ok(read)
    }
}

/// The scanned directory.
pub(crate) struct Repo {
    root: std::path::PathBuf,
    fs: Sandbox,
    warnings: RefCell<Vec<ScanWarning>>,
    suppressed_warnings: Cell<usize>,
    dirs: RefCell<HashMap<String, Rc<DirIndex>>>,
    bytes: Cell<u64>,
    work: Cell<u64>,
    /// `(device, inode)` of every protected file the scan knows of (`.env*`
    /// files and credential stores seen in a listing, the files of `.devflow/`
    /// and `.git/`): a general reader whose descriptor turns out to be one of
    /// them refuses it, whatever name it was reached through.
    protected_ids: RefCell<HashSet<(u64, u64)>>,
    git: OnceCell<git::GitState>,
}

/// Join a directory and a name into a `/`-separated path relative to the root.
pub(crate) fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() || dir == "." {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// Whether `rel` is a plain relative path: no `..`, no root, no prefix.
pub(crate) fn is_plain_relative(rel: &str) -> bool {
    !rel.contains('\\')
        && Path::new(rel)
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// Whether a file name belongs to a dotenv-style file or a credential store.
fn is_secret_name(name: &str) -> bool {
    const STORES: [&str; 8] = [
        ".npmrc",
        ".yarnrc",
        ".yarnrc.yml",
        ".netrc",
        ".pypirc",
        ".pgpass",
        ".git-credentials",
        "master.key",
    ];
    let lower = name.to_ascii_lowercase();
    lower.starts_with(".env")
        || STORES.contains(&lower.as_str())
        || lower.ends_with(".key")
        || lower.ends_with(".pem")
}

/// Whether a directory is `.git` or `.devflow`, whose files only the
/// keys-only readers and git's own state readers may open.
fn is_protected_dir_name(name: &str) -> bool {
    name.eq_ignore_ascii_case(".git") || name.eq_ignore_ascii_case(".devflow")
}

/// Whether `canonical` (a root-relative path with every symlink resolved)
/// is a file only the keys-only readers may open: dotenv files, anything
/// under `.devflow/` (the managed `env`, receipts) or `.git/`, and credential
/// stores. Names compare case-insensitively because the filesystem may.
fn is_protected(canonical: &Path) -> bool {
    let mut components = canonical.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            continue;
        };
        let name = name.to_string_lossy();
        if is_protected_dir_name(&name) {
            return true;
        }
        if components.peek().is_none() && is_secret_name(&name) {
            return true;
        }
    }
    false
}

impl Repo {
    /// Open `root`, which must be an existing directory.
    pub(crate) fn open(root: &Path) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("cannot access {}", root.display()))?;
        if !root.is_dir() {
            bail!("{} is not a directory", root.display());
        }
        let fs = Sandbox::open_ambient(&root)
            .with_context(|| format!("cannot access {}", root.display()))?;
        let repo = Self {
            root,
            fs,
            warnings: RefCell::new(Vec::new()),
            suppressed_warnings: Cell::new(0),
            dirs: RefCell::new(HashMap::new()),
            bytes: Cell::new(0),
            work: Cell::new(0),
            protected_ids: RefCell::new(HashSet::new()),
            git: OnceCell::new(),
        };
        repo.register_protected();
        Ok(repo)
    }

    /// Learn the identity of the protected files at the top of the checkout
    /// before anything else is read: the root's dotenv files and credential
    /// stores, and the files directly inside `.devflow/` and `.git/`: listing
    /// the root registers all of those), and the `.git` pointer file of a
    /// linked worktree.
    fn register_protected(&self) {
        let _ = self.list(".");
        self.remember_protected(".git");
    }

    /// Record the identity of every file directly inside `dir`.
    fn register_dir_files(&self, dir: &str) {
        if !self.charge(1) {
            return;
        }
        // Every entry examined and every lookup attempted is work, whether or
        // not it finds a file: a directory of dangling links costs as much as
        // one of files. The sandbox stops at what is left of the budget.
        let allowance =
            usize::try_from(MAX_WORK.saturating_sub(self.work.get())).unwrap_or(usize::MAX);
        let Ok(found) = self.fs.file_ids(dir, MAX_DIR_ENTRIES, allowance) else {
            return;
        };
        self.charge((found.examined + found.lookups) as u64);
        self.protected_ids.borrow_mut().extend(found.ids);
        if found.truncated {
            self.warn(
                dir,
                format!(
                    "only the first {MAX_DIR_ENTRIES} entries were checked for protected files"
                ),
            );
        }
        if found.out_of_work {
            self.warn(
                dir,
                "the work budget ran out before every file here was checked for protection",
            );
            // The allowance was all that was left, so one more unit trips the
            // budget and says so for the whole scan.
            self.charge(1);
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Record something the scan could not use.
    pub(crate) fn warn(&self, path: &str, message: impl Into<String>) {
        let warning = ScanWarning {
            path: path.to_string(),
            message: message.into(),
        };
        let mut warnings = self.warnings.borrow_mut();
        if warnings.contains(&warning) {
            return;
        }
        if warnings.len() >= MAX_WARNINGS {
            self.suppressed_warnings
                .set(self.suppressed_warnings.get() + 1);
            return;
        }
        warnings.push(warning);
    }

    pub(crate) fn take_warnings(&self) -> Vec<ScanWarning> {
        let mut warnings = std::mem::take(&mut *self.warnings.borrow_mut());
        let suppressed = self.suppressed_warnings.replace(0);
        if suppressed > 0 {
            warnings.push(ScanWarning {
                path: ".".to_string(),
                message: format!("{suppressed} further warnings were not recorded"),
            });
        }
        warnings
    }

    /// Work charged to the scan so far.
    pub(crate) fn work_spent(&self) -> u64 {
        self.work.get()
    }

    /// Charge `units` of filesystem work to the scan. `false` once the budget
    /// is spent: the caller must stop what it is doing (the results are
    /// incomplete, which one warning says).
    pub(crate) fn charge(&self, units: u64) -> bool {
        let spent = self.work.get().saturating_add(units);
        self.work.set(spent);
        if spent > MAX_WORK {
            self.warn(
                ".",
                "the work budget for this scan is exhausted; results may be incomplete",
            );
            return false;
        }
        true
    }

    /// Why a lookup of `rel` failed, as a warning where the cause matters.
    fn explain(&self, rel: &str, denied: Denied) {
        match denied {
            Denied::Missing | Denied::NotRegular => {}
            Denied::Escapes => self.warn(rel, "symlink resolves outside the repository; ignored"),
            Denied::Changed => self.warn(rel, "changed while the scan was running; ignored"),
            Denied::Other(e) => self.warn(rel, format!("cannot access: {e}")),
        }
    }

    fn plain(&self, rel: &str) -> bool {
        if is_plain_relative(rel) {
            true
        } else {
            self.warn(rel, "path is not relative to the repository; ignored");
            false
        }
    }

    /// The kind of `rel`, following symlinks that stay inside the root.
    pub(crate) fn kind(&self, rel: &str) -> Option<Kind> {
        if !self.plain(rel) || !self.charge(1) {
            return None;
        }
        match self.fs.metadata(rel) {
            Ok(meta) => Some(meta.kind),
            Err(denied) => {
                self.explain(rel, denied);
                None
            }
        }
    }

    pub(crate) fn is_file(&self, rel: &str) -> bool {
        self.kind(rel) == Some(Kind::File)
    }

    pub(crate) fn is_dir(&self, rel: &str) -> bool {
        self.kind(rel) == Some(Kind::Dir)
    }

    /// Whether `rel` is a symlink (even a harmless one); used so artifacts are
    /// never reported through a link.
    pub(crate) fn is_symlink(&self, rel: &str) -> bool {
        is_plain_relative(rel) && self.charge(1) && self.fs.is_symlink(rel)
    }

    /// The whole file as text, if it is a regular file within the size cap.
    /// Dotenv files and credential stores are never returned, even through a
    /// symlink or hard link: only [`Repo::open_env`] opens them.
    pub(crate) fn read_text(&self, rel: &str) -> Option<String> {
        self.read_capped(rel, MAX_FILE_BYTES, true)
    }

    /// At most the first `limit` bytes of a regular file; larger files are not
    /// an error here because only the head is wanted. Protected like
    /// [`Repo::read_text`].
    pub(crate) fn read_head(&self, rel: &str, limit: u64) -> Option<String> {
        self.read_capped(rel, limit.min(MAX_FILE_BYTES), false)
    }

    fn read_capped(&self, rel: &str, limit: u64, skip_if_larger: bool) -> Option<String> {
        if !self.plain(rel) {
            return None;
        }
        match self.fetch(&self.fs, rel, rel, limit, skip_if_larger, false) {
            Fetch::Data(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                Some(text.strip_prefix('\u{feff}').unwrap_or(&text).to_string())
            }
            Fetch::Missing | Fetch::Unusable => None,
        }
    }

    /// Read up to `limit` bytes of the regular file `rel` of `fs`.
    ///
    /// `label` names the file in warnings. With `whole`, a file over `limit`
    /// is skipped; without, only its head is returned. Unless `trusted`, a
    /// protected file is refused. The path is resolved once with symlinks
    /// confined to `fs`, checked against the protected names, and opened by
    /// its resolved name without following any link (a name swapped for a
    /// link after the check is refused); the opened descriptor is checked
    /// once more, by identity against the protected files and by link count,
    /// which catches hard links and swaps.
    fn fetch(
        &self,
        fs: &Sandbox,
        rel: &str,
        label: &str,
        limit: u64,
        whole: bool,
        trusted: bool,
    ) -> Fetch {
        if !self.charge(2) {
            return Fetch::Unusable;
        }
        let canonical = match fs.canonical(rel) {
            Ok(path) => path,
            Err(Denied::Missing) => return Fetch::Missing,
            Err(denied) => {
                self.explain(label, denied);
                return Fetch::Unusable;
            }
        };
        if !trusted && is_protected(&canonical) {
            self.warn(
                label,
                "environment or credentials file; only its variable names are read",
            );
            return Fetch::Unusable;
        }
        let canonical = canonical.to_string_lossy().into_owned();
        let (file, meta) = match fs.open_strict(Path::new(&canonical)) {
            Ok(opened) => opened,
            Err(Denied::Missing) => return Fetch::Missing,
            Err(Denied::NotRegular) => return Fetch::Unusable,
            Err(denied) => {
                self.explain(label, denied);
                return Fetch::Unusable;
            }
        };
        if !trusted {
            if meta
                .id
                .is_some_and(|id| self.protected_ids.borrow().contains(&id))
            {
                self.warn(
                    label,
                    "environment or credentials file; only its variable names are read",
                );
                return Fetch::Unusable;
            }
            // A second name is how a protected file gets read under an
            // innocent one, and that other name may be anywhere.
            if meta.links > 1 {
                self.warn(label, "has several hard links; not read");
                return Fetch::Unusable;
            }
        }
        if whole && meta.len > limit {
            self.warn(label, format!("skipped: larger than {} KiB", limit / 1024));
            return Fetch::Unusable;
        }
        if self.bytes.get() >= MAX_TOTAL_BYTES {
            self.warn(label, "skipped: read budget for this scan is exhausted");
            return Fetch::Unusable;
        }
        let mut bytes = Vec::new();
        // `take` bounds the read even if the file grows after the size check.
        if let Err(e) = file.take(limit.saturating_add(1)).read_to_end(&mut bytes) {
            self.warn(label, format!("cannot read: {e}"));
            return Fetch::Unusable;
        }
        if bytes.len() as u64 > limit {
            if whole {
                self.warn(label, format!("skipped: larger than {} KiB", limit / 1024));
                return Fetch::Unusable;
            }
            bytes.truncate(limit as usize);
        }
        self.bytes
            .set(self.bytes.get().saturating_add(bytes.len() as u64));
        Fetch::Data(bytes)
    }

    /// Open the dotenv file `rel` for streaming, without loading it into a
    /// string. This is the one reader of `.env*` files; callers keep variable
    /// names and drop everything else. The stream stops at the size cap and
    /// at the scan's byte budget.
    pub(crate) fn open_env(&self, rel: &str) -> Option<EnvStream<'_>> {
        if !self.plain(rel) || !self.charge(2) {
            return None;
        }
        let canonical = match self.fs.canonical(rel) {
            Ok(path) => path.to_string_lossy().into_owned(),
            Err(denied) => {
                self.explain(rel, denied);
                return None;
            }
        };
        let (file, meta) = match self.fs.open_regular(&canonical) {
            Ok(opened) => opened,
            Err(denied) => {
                self.explain(rel, denied);
                return None;
            }
        };
        if meta.len > MAX_FILE_BYTES {
            self.warn(
                rel,
                format!("skipped: larger than {} KiB", MAX_FILE_BYTES / 1024),
            );
            return None;
        }
        Some(EnvStream {
            file,
            repo: self,
            path: rel.to_string(),
            remaining: MAX_FILE_BYTES,
        })
    }

    /// Entries of directory `rel` (cached). Missing or rejected directories
    /// yield an empty index.
    pub(crate) fn list(&self, rel: &str) -> Rc<DirIndex> {
        let key = if rel.is_empty() { "." } else { rel };
        if let Some(index) = self.dirs.borrow().get(key) {
            return Rc::clone(index);
        }
        let index = Rc::new(self.read_dir(key));
        self.dirs
            .borrow_mut()
            .insert(key.to_string(), Rc::clone(&index));
        index
    }

    fn read_dir(&self, rel: &str) -> DirIndex {
        if !self.plain(rel) || !self.charge(1) {
            return DirIndex::default();
        }
        let listing = match self.fs.read_dir(rel, MAX_DIR_ENTRIES) {
            Ok(listing) => listing,
            Err(denied) => {
                self.explain(rel, denied);
                return DirIndex::default();
            }
        };
        if listing.truncated {
            self.warn(
                rel,
                format!("only the first {MAX_DIR_ENTRIES} entries were listed"),
            );
        }
        // Every entry examined counts, rejected or not.
        self.charge(listing.examined as u64);
        let mut entries = BTreeMap::new();
        let mut uninspectable = BTreeSet::new();
        for (name, entry_type) in listing.entries {
            let child = join(rel, &name);
            let kind = match entry_type {
                EntryType::File => Kind::File,
                EntryType::Dir => Kind::Dir,
                EntryType::Other => Kind::Other,
                // Resolve the target; links leaving the root are dropped (and
                // warned about) but remembered, because they are there.
                EntryType::Symlink => match self.kind(&child) {
                    Some(kind) => kind,
                    None => {
                        uninspectable.insert(name);
                        continue;
                    }
                },
            };
            if kind == Kind::File && is_secret_name(&name) {
                self.remember_protected(&child);
            }
            if kind == Kind::Dir && is_protected_dir_name(&name) {
                self.register_dir_files(&child);
            }
            entries.insert(name, kind);
        }
        DirIndex {
            entries,
            uninspectable,
            truncated: listing.truncated,
        }
    }

    /// Note the identity of a protected file (a dotenv-style file, a
    /// credential store, the `.git` pointer file) so it cannot be read
    /// through another name.
    fn remember_protected(&self, rel: &str) {
        if !self.charge(1) {
            return;
        }
        if let Ok(meta) = self.fs.metadata(rel)
            && meta.kind == Kind::File
            && let Some(id) = meta.id
        {
            self.protected_ids.borrow_mut().insert(id);
        }
    }

    /// Bytes the scan may still read.
    pub(crate) fn bytes_left(&self) -> u64 {
        MAX_TOTAL_BYTES.saturating_sub(self.bytes.get())
    }

    /// Charge `bytes` of work done on data already read (the expansion of the
    /// index) to the scan's byte budget.
    pub(crate) fn spend_bytes(&self, bytes: u64) {
        self.bytes.set(self.bytes.get().saturating_add(bytes));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_paths() {
        for path in [
            ".env",
            ".ENV",
            ".env.local",
            "apps/web/.env.production",
            ".envrc",
            ".devflow/env",
            "sub/.devflow/admin.json",
            ".git/config",
            ".npmrc",
            "config/master.key",
            "certs/server.pem",
        ] {
            assert!(is_protected(Path::new(path)), "{path}");
        }
        for path in [
            ".nvmrc",
            "package.json",
            "environment.md",
            "src/env.rs",
            ".github/workflows/ci.yml",
            "envs/dev.toml",
            ".gitignore",
        ] {
            assert!(!is_protected(Path::new(path)), "{path}");
        }
    }

    /// `(device, inode)` of a file, as a protected-file registry holds it.
    #[cfg(unix)]
    fn identity(root: &Path, rel: &str) -> (u64, u64) {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(root.join(rel)).expect("stat");
        (meta.dev(), meta.ino())
    }

    #[cfg(unix)]
    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, content).expect("write");
    }

    /// The identity of the files a general reader must never return is known
    /// before the scan reads anything, not only once a listing has met them.
    #[cfg(unix)]
    #[test]
    fn r2_finding_1_protected_files_are_registered_when_the_scan_opens() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        for (rel, content) in [
            (".devflow/env", "A=1"),
            (".devflow/admin.json", "{}"),
            (".env", "A=1"),
            (".env.local", "A=1"),
            (".npmrc", "x"),
            (".git/config", "x"),
            (".git/index", "x"),
            ("apps/web/.env.production", "A=1"),
            ("apps/web/.devflow/env", "A=1"),
            ("apps/web/.nvmrc", "22"),
            (".nvmrc", "22"),
            ("deep/inner/.env", "A=1"),
        ] {
            write(root, rel, content);
        }
        let repo = Repo::open(root).expect("open");
        let known = |rel: &str| repo.protected_ids.borrow().contains(&identity(root, rel));

        for rel in [
            ".devflow/env",
            ".devflow/admin.json",
            ".env",
            ".env.local",
            ".npmrc",
            ".git/config",
            ".git/index",
        ] {
            assert!(known(rel), "{rel} is known before anything is read");
        }
        for rel in [".nvmrc", "apps/web/.nvmrc"] {
            assert!(!known(rel), "{rel} is not protected");
        }
        // Directories the scan has not listed yet are met when it lists them.
        assert!(!known("apps/web/.env.production"));
        assert!(!known("deep/inner/.env"));
        repo.list("apps/web");
        assert!(known("apps/web/.env.production"));
        assert!(known("apps/web/.devflow/env"));
        assert!(!known("deep/inner/.env"));
    }

    /// Whatever name a descriptor was reached through, a general reader refuses
    /// it when it is a file the scan knows to be protected.
    #[cfg(unix)]
    #[test]
    fn r2_finding_1_a_descriptor_that_is_a_protected_file_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, ".nvmrc", "22\n");
        write(root, ".tool-versions", "node 22\n");
        let repo = Repo::open(root).expect("open");
        assert_eq!(repo.read_text(".nvmrc").as_deref(), Some("22\n"));

        // Say the identity of `.nvmrc` turned out to be a protected file's.
        repo.protected_ids
            .borrow_mut()
            .insert(identity(root, ".nvmrc"));
        assert_eq!(repo.read_text(".nvmrc"), None);
        assert_eq!(repo.read_head(".nvmrc", 100), None);
        assert_eq!(
            repo.read_text(".tool-versions").as_deref(),
            Some("node 22\n")
        );
        let warnings = repo.take_warnings();
        assert!(
            warnings
                .iter()
                .any(|w| w.path == ".nvmrc" && w.message.contains("only its variable names")),
            "{warnings:?}"
        );
        // The keys-only reader is unaffected.
        write(root, ".env", "A=1\n");
        assert!(repo.open_env(".env").is_some());
    }

    /// A directory `shared` of dangling links, and `count` directories whose
    /// `.devflow` is a link to it.
    #[cfg(unix)]
    fn dangling_links(root: &Path, links: usize, count: usize) {
        use std::os::unix::fs::symlink;
        std::fs::create_dir(root.join("shared")).expect("mkdir");
        for i in 0..links {
            symlink("nowhere", root.join(format!("shared/l{i}"))).expect("symlink");
        }
        for i in 0..count {
            std::fs::create_dir(root.join(format!("d{i}"))).expect("mkdir");
            symlink("../shared", root.join(format!("d{i}/.devflow"))).expect("symlink");
        }
    }

    /// Review round 3, finding 3: registering a protected directory is charged
    /// for every entry it examines and every lookup it attempts, not only for
    /// the files it finds. Links that lead nowhere find nothing.
    #[cfg(unix)]
    #[test]
    fn r3_finding_3_registering_protected_files_is_charged_for_what_it_examines() {
        let dir = tempfile::tempdir().expect("tempdir");
        dangling_links(dir.path(), 300, 2);
        let repo = Repo::open(dir.path()).expect("open");

        let before = repo.work_spent();
        repo.list("d0");
        let spent = repo.work_spent() - before;
        // 300 entries examined and 300 lookups made, and nothing found.
        assert!(spent >= 600, "charged {spent} units");
        assert!(repo.take_warnings().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn r3_finding_3_registration_stops_with_a_warning_when_the_budget_runs_out() {
        let dir = tempfile::tempdir().expect("tempdir");
        dangling_links(dir.path(), 300, 2);
        let repo = Repo::open(dir.path()).expect("open");

        // 100 units are left, the directory needs about 600.
        repo.work.set(MAX_WORK - 100);
        repo.list("d1");
        assert!(repo.work_spent() <= MAX_WORK + 1, "{}", repo.work_spent());
        let warnings = repo.take_warnings();
        let said = |needle: &str| warnings.iter().any(|w| w.message.contains(needle));
        assert!(said("before every file here was checked"), "{warnings:?}");
        assert!(
            said("work budget for this scan is exhausted"),
            "{warnings:?}"
        );

        // Nothing is examined once the budget is gone.
        let spent = repo.work_spent();
        repo.list("d0");
        assert!(repo.work_spent() <= spent + 4);
    }

    #[cfg(unix)]
    #[test]
    fn r2_finding_2_files_with_several_names_are_not_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "one.txt", "x");
        write(root, "two.txt", "x");
        std::fs::hard_link(root.join("one.txt"), root.join("one-again.txt")).expect("link");
        let repo = Repo::open(root).expect("open");

        assert_eq!(repo.read_text("two.txt").as_deref(), Some("x"));
        assert_eq!(repo.read_text("one.txt"), None);
        assert_eq!(repo.read_text("one-again.txt"), None);
        assert!(
            repo.take_warnings()
                .iter()
                .any(|w| w.path == "one.txt" && w.message.contains("hard links"))
        );
    }

    #[test]
    fn plain_relative_paths() {
        assert!(is_plain_relative("a/b"));
        assert!(is_plain_relative("./a"));
        assert!(!is_plain_relative("../a"));
        assert!(!is_plain_relative("/etc"));
        assert!(!is_plain_relative("a\\b"));
    }
}
