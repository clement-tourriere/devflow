//! Directory-relative file access built on `cap-std`.
//!
//! A [`Sandbox`] owns an open directory descriptor. Every lookup is resolved
//! from that descriptor one component at a time (`openat` with `O_NOFOLLOW`,
//! or `openat2(RESOLVE_BENEATH)` where the kernel has it), and symlinks are
//! followed only while they stay beneath the directory. There is no
//! "canonicalize, then open" window: the path a swap races against is the
//! same one the kernel resolves, so an attacker replacing a file or an
//! intermediate directory with a link to `/etc` or `~/.ssh` while the scan
//! runs gets an error instead of the target.
//!
//! Files are opened with `O_NONBLOCK`, so a FIFO swapped in for a manifest
//! opens (and is then rejected) instead of blocking the scan, and the opened
//! descriptor is validated with `fstat` (regular file, size, link count)
//! rather than trusting an earlier `stat` of the name.
//!
//! [`Sandbox::open_strict`] goes one step further for readers that decided
//! what a path *is* from its resolved name: it walks that name one component
//! at a time and, after each open, confirms with `lstat` that the entry is not
//! a symlink and is the very object that was opened. A path that was a plain
//! file or directory when it was inspected and became a link before it was
//! opened is refused instead of followed.
use cap_std::ambient_authority;
use cap_std::fs::{Dir, File, Metadata};
#[cfg(unix)]
use cap_std::fs::{MetadataExt, OpenOptionsExt};
use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};

/// What a directory entry is, after following symlinks that stay inside the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    File,
    Dir,
    Other,
}

/// What `readdir` reported for an entry, without following it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryType {
    File,
    Dir,
    Symlink,
    Other,
}

/// Why a lookup produced no usable result.
#[derive(Debug)]
pub(crate) enum Denied {
    /// The path (or one of its parents) does not exist.
    Missing,
    /// Following symlinks led outside the sandbox.
    Escapes,
    /// The path exists but is not a regular file.
    NotRegular,
    /// An entry on the way was swapped for a link, or for something else,
    /// while the path was being opened.
    Changed,
    /// Anything else (permissions, symlink loops, I/O).
    Other(io::Error),
}

impl Denied {
    fn from_io(error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => Denied::Missing,
            // cap-std reports an escaping path as a permission error without
            // an OS error number; a real `EACCES` carries one.
            io::ErrorKind::PermissionDenied if error.raw_os_error().is_none() => Denied::Escapes,
            _ => Denied::Other(error),
        }
    }
}

/// The facts about an object that the scan uses, taken from one `stat`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Meta {
    pub kind: Kind,
    pub len: u64,
    /// Device and inode, to recognize one file under several names.
    pub id: Option<(u64, u64)>,
    /// Number of names (hard links) the object has. `1` where the platform
    /// does not say.
    pub links: u64,
}

impl Meta {
    fn of(meta: &Metadata) -> Self {
        let kind = if meta.is_file() {
            Kind::File
        } else if meta.is_dir() {
            Kind::Dir
        } else {
            Kind::Other
        };
        #[cfg(unix)]
        let (id, links) = (Some((meta.dev(), meta.ino())), meta.nlink());
        #[cfg(not(unix))]
        let (id, links) = (None, 1);
        Self {
            kind,
            len: meta.len(),
            id,
            links,
        }
    }
}

/// The entries of one directory as `readdir` returned them.
#[derive(Debug, Default)]
pub(crate) struct DirListing {
    /// UTF-8 names with their type; entries that could not be read are absent.
    pub entries: Vec<(String, EntryType)>,
    /// Entries examined, including the ones skipped.
    pub examined: usize,
    /// More entries existed than the limit allowed.
    pub truncated: bool,
}

/// The identities of the files in one directory and the work that took.
#[derive(Debug, Default)]
pub(crate) struct FileIds {
    /// `(device, inode)` of each regular file found.
    pub ids: Vec<(u64, u64)>,
    /// Directory entries examined, including the ones skipped.
    pub examined: usize,
    /// Metadata lookups attempted, whether or not they found a file.
    pub lookups: usize,
    /// The directory has more entries than the entry limit.
    pub truncated: bool,
    /// The work allowance ran out before every entry was looked at.
    pub out_of_work: bool,
}

/// An open directory that all access is confined to.
pub(crate) struct Sandbox {
    dir: Dir,
}

impl Sandbox {
    /// Open `path` as a sandbox root. This is the only use of ambient
    /// authority: everything after it is relative to the descriptor.
    pub(crate) fn open_ambient(path: &Path) -> io::Result<Self> {
        Ok(Self {
            dir: Dir::open_ambient_dir(path, ambient_authority())?,
        })
    }

    fn path(rel: &str) -> &Path {
        Path::new(if rel.is_empty() { "." } else { rel })
    }

    /// Metadata of `rel`, following symlinks that stay inside the sandbox.
    pub(crate) fn metadata(&self, rel: &str) -> Result<Meta, Denied> {
        self.dir
            .metadata(Self::path(rel))
            .map(|m| Meta::of(&m))
            .map_err(Denied::from_io)
    }

    /// Whether the last component of `rel` is itself a symlink.
    pub(crate) fn is_symlink(&self, rel: &str) -> bool {
        self.dir
            .symlink_metadata(Self::path(rel))
            .is_ok_and(|m| m.file_type().is_symlink())
    }

    /// `rel` with every symlink resolved, relative to the sandbox root.
    ///
    /// Resolved here with `lstat` and `readlink` only. `Dir::canonicalize`
    /// would do, except that where it is implemented by opening the path (macOS
    /// and the BSDs) it opens the last component with a plain `open`, which
    /// blocks forever on a FIFO. This function never opens anything.
    ///
    /// The answer is only used to decide what a file *is called* (is it a
    /// dotenv file?); containment is enforced by the confined open that
    /// follows, so a swap between the two cannot reach outside the sandbox.
    pub(crate) fn canonical(&self, rel: &str) -> Result<PathBuf, Denied> {
        const MAX_LINKS: usize = 40;
        enum Part {
            Name(OsString),
            Up,
        }
        fn push_parts(path: &Path, todo: &mut Vec<Part>) -> Result<(), Denied> {
            for component in path.components().rev() {
                match component {
                    Component::Normal(name) => todo.push(Part::Name(name.to_owned())),
                    Component::ParentDir => todo.push(Part::Up),
                    Component::CurDir => {}
                    Component::RootDir | Component::Prefix(_) => return Err(Denied::Escapes),
                }
            }
            Ok(())
        }

        let mut todo = Vec::new();
        push_parts(Self::path(rel), &mut todo)?;
        let mut resolved = PathBuf::new();
        let mut links = 0;
        while let Some(part) = todo.pop() {
            match part {
                Part::Up => {
                    if !resolved.pop() {
                        return Err(Denied::Escapes);
                    }
                }
                Part::Name(name) => {
                    let candidate = resolved.join(name);
                    let meta = self
                        .dir
                        .symlink_metadata(&candidate)
                        .map_err(Denied::from_io)?;
                    if meta.file_type().is_symlink() {
                        links += 1;
                        if links > MAX_LINKS {
                            return Err(Denied::Other(io::Error::other(
                                "too many levels of symbolic links",
                            )));
                        }
                        let target = self
                            .dir
                            .read_link_contents(&candidate)
                            .map_err(Denied::from_io)?;
                        push_parts(&target, &mut todo)?;
                    } else {
                        resolved = candidate;
                    }
                }
            }
        }
        if resolved.as_os_str().is_empty() {
            resolved.push(".");
        }
        Ok(resolved)
    }

    /// Open `rel` for reading and check the descriptor that was opened: only
    /// a regular file is returned. `O_NONBLOCK` keeps a FIFO from blocking
    /// the open; the `fstat` below rejects it.
    pub(crate) fn open_regular(&self, rel: &str) -> Result<(File, Meta), Denied> {
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NONBLOCK);
        let file = self
            .dir
            .open_with(Self::path(rel), &options)
            .map_err(Denied::from_io)?;
        let meta = file.metadata().map_err(Denied::Other)?;
        let meta = Meta::of(&meta);
        if meta.kind != Kind::File {
            return Err(Denied::NotRegular);
        }
        Ok((file, meta))
    }

    /// Open the regular file `rel`, a path with every symlink already
    /// resolved (see [`Sandbox::canonical`]), refusing to follow any link on
    /// the way.
    ///
    /// Each directory is opened from its parent and each open is confirmed
    /// with `lstat`: the entry must not be a symlink and must be the object
    /// that was opened. If the name was a plain file when it was inspected
    /// and a symlink to somewhere else by the time it is opened, the open
    /// either fails or lands on an object whose identity differs from what
    /// `lstat` reports, and the result is [`Denied::Changed`].
    pub(crate) fn open_strict(&self, rel: &Path) -> Result<(File, Meta), Denied> {
        let names: Vec<&std::ffi::OsStr> = rel
            .components()
            .filter_map(|c| match c {
                Component::Normal(name) => Some(name),
                _ => None,
            })
            .collect();
        let Some((last, parents)) = names.split_last() else {
            return Err(Denied::NotRegular);
        };
        let mut owned: Option<Dir> = None;
        for name in parents {
            let current = owned.as_ref().unwrap_or(&self.dir);
            let child = current.open_dir(name).map_err(Denied::from_io)?;
            let opened = child.dir_metadata().map_err(Denied::Other)?;
            Self::confirm(current, name, &Meta::of(&opened))?;
            owned = Some(child);
        }
        let current = owned.as_ref().unwrap_or(&self.dir);
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NONBLOCK);
        let file = current.open_with(last, &options).map_err(Denied::from_io)?;
        let meta = Meta::of(&file.metadata().map_err(Denied::Other)?);
        Self::confirm(current, last, &meta)?;
        if meta.kind != Kind::File {
            return Err(Denied::NotRegular);
        }
        Ok((file, meta))
    }

    /// `name` in `dir` must be, right now, the object `opened` describes and
    /// not a symlink.
    fn confirm(dir: &Dir, name: &std::ffi::OsStr, opened: &Meta) -> Result<(), Denied> {
        match dir.symlink_metadata(name) {
            Ok(entry) if !entry.file_type().is_symlink() && Meta::of(&entry).id == opened.id => {
                Ok(())
            }
            Ok(_) => Err(Denied::Changed),
            Err(error) => Err(Denied::from_io(error)),
        }
    }

    /// Identities of the files directly inside `rel` (symlinks followed within
    /// the sandbox). Unreadable entries are skipped.
    ///
    /// Looks at no more than `entry_limit` entries, and the entries examined
    /// and the lookups made together never exceed `allowance` units of work.
    /// The result says what was done, found or not, so that the caller can
    /// charge for it: a directory of dangling links costs a lookup per link
    /// and yields nothing.
    pub(crate) fn file_ids(
        &self,
        rel: &str,
        entry_limit: usize,
        allowance: usize,
    ) -> Result<FileIds, Denied> {
        let listing = self.read_dir(rel, entry_limit.min(allowance))?;
        let mut found = FileIds {
            examined: listing.examined,
            // Hitting the work allowance is not the same as hitting the
            // entry limit: the first means the scan is out of budget.
            truncated: listing.truncated && allowance >= entry_limit,
            out_of_work: listing.truncated && allowance < entry_limit,
            ..FileIds::default()
        };
        let mut left = allowance - listing.examined;
        for (name, entry) in &listing.entries {
            if !matches!(entry, EntryType::File | EntryType::Symlink) {
                continue;
            }
            if left == 0 {
                found.out_of_work = true;
                break;
            }
            left -= 1;
            found.lookups += 1;
            let path = format!("{}/{name}", rel.trim_end_matches('/'));
            if let Ok(meta) = self.metadata(&path)
                && meta.kind == Kind::File
                && let Some(id) = meta.id
            {
                found.ids.push(id);
            }
        }
        Ok(found)
    }

    /// Open the directory `rel` as a sandbox of its own.
    pub(crate) fn open_dir(&self, rel: &str) -> Result<Sandbox, Denied> {
        self.dir
            .open_dir(Self::path(rel))
            .map(|dir| Sandbox { dir })
            .map_err(Denied::from_io)
    }

    /// List `rel`, examining at most `limit` entries.
    pub(crate) fn read_dir(&self, rel: &str, limit: usize) -> Result<DirListing, Denied> {
        let dir = self
            .dir
            .open_dir(Self::path(rel))
            .map_err(Denied::from_io)?;
        let mut listing = DirListing::default();
        for entry in dir.entries().map_err(Denied::Other)? {
            if listing.examined >= limit {
                listing.truncated = true;
                break;
            }
            listing.examined += 1;
            let Ok(entry) = entry else { continue };
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            // An entry whose type cannot be read is still there: `Other`
            // keeps it from vanishing from the listing.
            let Ok(file_type) = entry.file_type() else {
                listing.entries.push((name, EntryType::Other));
                continue;
            };
            let kind = if file_type.is_symlink() {
                EntryType::Symlink
            } else if file_type.is_dir() {
                EntryType::Dir
            } else if file_type.is_file() {
                EntryType::File
            } else {
                EntryType::Other
            };
            listing.entries.push((name, kind));
        }
        Ok(listing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[cfg(unix)]
    #[test]
    fn a_fifo_opens_without_blocking_and_is_rejected() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = CString::new(dir.path().join("pipe").as_os_str().as_bytes()).expect("path");
        // SAFETY: `path` is a valid NUL-terminated string that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let sandbox = Sandbox::open_ambient(dir.path()).expect("open");

        assert!(matches!(
            sandbox.open_regular("pipe"),
            Err(Denied::NotRegular)
        ));
        assert_eq!(sandbox.metadata("pipe").expect("stat").kind, Kind::Other);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_leaving_the_sandbox_are_refused_at_every_component() {
        use std::os::unix::fs::symlink;
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret"), "x").expect("write");
        let dir = tempfile::tempdir().expect("tempdir");
        symlink(outside.path(), dir.path().join("dir")).expect("symlink");
        symlink(outside.path().join("secret"), dir.path().join("file")).expect("symlink");
        symlink("../../../../../../etc/passwd", dir.path().join("rel")).expect("symlink");
        let sandbox = Sandbox::open_ambient(dir.path()).expect("open");

        for rel in ["dir/secret", "file", "rel", "dir"] {
            assert!(
                matches!(sandbox.open_regular(rel), Err(Denied::Escapes)),
                "{rel} must not resolve outside the sandbox"
            );
        }
        assert!(matches!(sandbox.canonical("dir"), Err(Denied::Escapes)));
        assert!(matches!(
            sandbox.metadata("dir/secret"),
            Err(Denied::Escapes)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn internal_symlinks_resolve_and_canonicalize() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("real")).expect("mkdir");
        std::fs::write(dir.path().join("real/file"), "hello").expect("write");
        symlink("real", dir.path().join("alias")).expect("symlink");
        let sandbox = Sandbox::open_ambient(dir.path()).expect("open");

        let (mut file, meta) = sandbox.open_regular("alias/file").expect("open");
        let mut text = String::new();
        file.read_to_string(&mut text).expect("read");
        assert_eq!((text.as_str(), meta.len), ("hello", 5));
        assert_eq!(
            sandbox.canonical("alias/file").expect("canonical"),
            Path::new("real/file")
        );
        assert!(sandbox.is_symlink("alias"));
        assert!(!sandbox.is_symlink("real"));
    }

    #[test]
    fn listing_counts_every_entry_against_the_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        for i in 0..10 {
            std::fs::write(dir.path().join(format!("f{i}")), "").expect("write");
        }
        let sandbox = Sandbox::open_ambient(dir.path()).expect("open");

        let listing = sandbox.read_dir(".", 4).expect("list");
        assert_eq!(listing.examined, 4);
        assert!(listing.truncated);
        assert!(listing.entries.len() <= 4);
        assert!(!sandbox.read_dir("", 100).expect("list").truncated);
        assert!(matches!(sandbox.read_dir("nope", 4), Err(Denied::Missing)));
    }

    /// `open_strict` is given a name that was inspected earlier. If a link
    /// stands there now, in the name or on the way to it, it must not be
    /// followed. (This is the state a swap between the check and the open
    /// leaves behind.)
    #[cfg(unix)]
    #[test]
    fn r2_finding_1_strict_open_refuses_a_link_at_any_component() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("real/inner")).expect("mkdir");
        std::fs::write(dir.path().join("real/inner/file"), "hello").expect("write");
        std::fs::write(dir.path().join("plain"), "plain").expect("write");
        symlink("plain", dir.path().join("file-link")).expect("symlink");
        symlink("real", dir.path().join("dir-link")).expect("symlink");
        symlink("inner", dir.path().join("real/deeper-link")).expect("symlink");
        let sandbox = Sandbox::open_ambient(dir.path()).expect("open");

        let (mut file, meta) = sandbox
            .open_strict(Path::new("real/inner/file"))
            .expect("a plain path opens");
        let mut text = String::new();
        file.read_to_string(&mut text).expect("read");
        assert_eq!((text.as_str(), meta.len, meta.links), ("hello", 5, 1));
        assert!(sandbox.open_strict(Path::new("plain")).is_ok());

        for rel in ["file-link", "dir-link/inner/file", "real/deeper-link/file"] {
            assert!(
                matches!(sandbox.open_strict(Path::new(rel)), Err(Denied::Changed)),
                "{rel} must not be followed"
            );
        }
        assert!(matches!(
            sandbox.open_strict(Path::new("real")),
            Err(Denied::NotRegular)
        ));
        assert!(matches!(
            sandbox.open_strict(Path::new("nope")),
            Err(Denied::Missing)
        ));
        assert!(matches!(
            sandbox.open_strict(Path::new("real/nope/file")),
            Err(Denied::Missing)
        ));
        assert!(matches!(
            sandbox.open_strict(Path::new(".")),
            Err(Denied::NotRegular)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn r2_finding_1_strict_open_does_not_block_on_a_fifo() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = CString::new(dir.path().join("pipe").as_os_str().as_bytes()).expect("path");
        // SAFETY: `path` is a valid NUL-terminated string that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let sandbox = Sandbox::open_ambient(dir.path()).expect("open");

        assert!(matches!(
            sandbox.open_strict(Path::new("pipe")),
            Err(Denied::NotRegular)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn r2_finding_2_the_link_count_of_the_opened_file_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a"), "x").expect("write");
        std::fs::write(dir.path().join("lonely"), "x").expect("write");
        std::fs::hard_link(dir.path().join("a"), dir.path().join("b")).expect("link");
        let sandbox = Sandbox::open_ambient(dir.path()).expect("open");

        for name in ["a", "b"] {
            let (_, meta) = sandbox.open_strict(Path::new(name)).expect("open");
            assert_eq!(meta.links, 2, "{name}");
        }
        let (_, lonely) = sandbox.open_strict(Path::new("lonely")).expect("open");
        assert_eq!(lonely.links, 1);
        assert_eq!(
            sandbox.metadata("a").expect("stat").id,
            sandbox.metadata("b").expect("stat").id
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_ids_lists_regular_files_and_the_targets_of_links() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("secrets/sub")).expect("mkdir");
        std::fs::write(dir.path().join("secrets/env"), "x").expect("write");
        std::fs::write(dir.path().join("secrets/sub/nested"), "x").expect("write");
        std::fs::write(dir.path().join("other"), "x").expect("write");
        symlink("../other", dir.path().join("secrets/alias")).expect("symlink");
        symlink("missing", dir.path().join("secrets/dangling")).expect("symlink");
        let sandbox = Sandbox::open_ambient(dir.path()).expect("open");

        let found = sandbox.file_ids("secrets", 100, 1000).expect("ids");
        let ids = &found.ids;
        let id = |rel: &str| sandbox.metadata(rel).expect("stat").id.expect("id");
        assert_eq!(ids.len(), 2, "env and the target of alias: {ids:?}");
        assert!(ids.contains(&id("secrets/env")));
        assert!(ids.contains(&id("other")));
        assert!(!ids.contains(&id("secrets/sub/nested")), "not recursive");
        // Four entries examined; the file and the two links are looked up.
        assert_eq!((found.examined, found.lookups), (4, 3));
        assert!(!found.truncated && !found.out_of_work);
        assert!(matches!(
            sandbox.file_ids("nope", 100, 1000),
            Err(Denied::Missing)
        ));
    }

    /// Review round 3, finding 3: a link that leads nowhere finds no file and
    /// still costs a lookup, so the cost is reported whatever was found, and a
    /// work allowance stops the examination and the lookups alike.
    #[cfg(unix)]
    #[test]
    fn r3_finding_3_every_entry_and_lookup_is_reported_and_the_allowance_is_enforced() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("links")).expect("mkdir");
        for i in 0..50 {
            symlink("missing", dir.path().join(format!("links/l{i}"))).expect("symlink");
        }
        let sandbox = Sandbox::open_ambient(dir.path()).expect("open");
        let run = |entry_limit, allowance| {
            let found = sandbox
                .file_ids("links", entry_limit, allowance)
                .expect("ids");
            assert!(found.ids.is_empty());
            (
                found.examined,
                found.lookups,
                found.truncated,
                found.out_of_work,
            )
        };

        // Nothing found, all of it paid for.
        assert_eq!(run(100, 1000), (50, 50, false, false));
        // Out of allowance in the middle of the lookups.
        assert_eq!(run(100, 80), (50, 30, false, true));
        // Out of allowance before the listing is over.
        assert_eq!(run(100, 20), (20, 0, false, true));
        assert_eq!(run(100, 0), (0, 0, false, true));
        // The entry limit is a different stop: the budget is not out.
        assert_eq!(run(10, 1000), (10, 10, true, false));
        // Exactly enough is not out of work.
        assert_eq!(run(100, 100), (50, 50, false, false));
    }
}
