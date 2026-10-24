//! A database file, reached from its worktree without following a link.
//!
//! A worktree's `db` directory can be a link somebody else committed (a branch
//! that ships `db -> ../shop/db` makes `db/app.sqlite3` the main checkout's
//! own database). Anything that removes, replaces or stages a file there has to
//! be sure the file is in the worktree it was asked about, so a path is never
//! resolved for it:
//!
//! - [`Place::open`] holds the worktree open as a directory descriptor and opens
//!   every directory on the way from its parent (cap-std, relative to the
//!   parent's descriptor). Each one is looked at with `lstat` before and after
//!   the open and confirmed to be the very object that was opened; a link
//!   anywhere on the way is refused, the file's own name included.
//! - What devflow then does to the file is done through that directory
//!   descriptor and the file's name (`unlinkat`, `linkat`, `renameat`): a
//!   directory replaced by a link after the walk cannot redirect it.
//! - SQLite itself opens by path, so the path it was given is confirmed to
//!   reach the same inode the descriptor holds ([`Place::confirm`]) once its
//!   lock is taken.
use crate::remedy::{Problem, Remedy};
use anyhow::{Context, Result, bail};
use cap_std::ambient_authority;
#[cfg(unix)]
use cap_std::fs::MetadataExt;
use cap_std::fs::{Dir, Metadata};
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

/// The files SQLite keeps beside a database.
pub const SIDECARS: [&str; 3] = ["-wal", "-shm", "-journal"];

/// Which file an inode is: device, inode and size. Recorded in the creation
/// intent for the file that was staged, so a recovery adopts that file and no
/// other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub dev: u64,
    pub ino: u64,
    pub len: u64,
}

impl Identity {
    /// The same inode (the size changes as the database is used). Only a
    /// platform that has inodes can say so: elsewhere nothing is the same file.
    pub fn is_same_file(&self, other: &Identity) -> bool {
        cfg!(unix) && self.dev == other.dev && self.ino == other.ino
    }

    fn of(metadata: &Metadata) -> Identity {
        #[cfg(unix)]
        {
            Identity {
                dev: metadata.dev(),
                ino: metadata.ino(),
                len: metadata.len(),
            }
        }
        #[cfg(not(unix))]
        {
            Identity {
                dev: 0,
                ino: 0,
                len: metadata.len(),
            }
        }
    }

    fn of_std(metadata: &std::fs::Metadata) -> Identity {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            Identity {
                dev: metadata.dev(),
                ino: metadata.ino(),
                len: metadata.len(),
            }
        }
        #[cfg(not(unix))]
        {
            Identity {
                dev: 0,
                ino: 0,
                len: metadata.len(),
            }
        }
    }
}

/// The platform has no inode to tell one file from another by, so nothing
/// that removes, replaces or adopts a database file is done: devflow ships for
/// macOS and Linux, and a platform it was not built for is refused rather than
/// guessed at.
#[cfg_attr(unix, allow(dead_code))]
pub(crate) fn unsupported_platform() -> anyhow::Error {
    Problem::new(
        "sqlite services are not supported on this platform: devflow cannot tell which file is \
         which here, so it does not remove, replace or adopt a database file",
    )
    .into_error()
}

fn link_refused(path: &Path) -> anyhow::Error {
    Problem::new(format!(
        "{} is a symbolic link; devflow does not reach a database through it",
        path.display()
    ))
    .with(Remedy::Reveal {
        path: path.to_path_buf(),
    })
    .into_error()
}

pub(crate) fn changed(path: &Path) -> anyhow::Error {
    Problem::new(format!(
        "{} changed while devflow was looking at it; nothing was changed",
        path.display()
    ))
    .with(Remedy::Retry)
    .into_error()
}

/// A database file in a worktree, with the directory that holds it open.
pub struct Place {
    dir: Dir,
    name: OsString,
    path: PathBuf,
}

impl std::fmt::Debug for Place {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Place({})", self.path.display())
    }
}

impl Place {
    /// The place of `relative` (a path below the worktree `root`, validated:
    /// no `..`, nothing absolute). With `create` the directories on the way
    /// are made; without it `None` says one does not exist. A link anywhere on
    /// the way is an error.
    pub fn open(root: &Path, relative: &Path, create: bool) -> Result<Option<Place>> {
        #[cfg(not(unix))]
        {
            let _ = (root, relative, create);
            return Err(unsupported_platform());
        }
        #[cfg(unix)]
        {
            Place::open_on_unix(root, relative, create)
        }
    }

    #[cfg(unix)]
    fn open_on_unix(root: &Path, relative: &Path, create: bool) -> Result<Option<Place>> {
        let mut names = Vec::new();
        for component in relative.components() {
            match component {
                Component::Normal(name) => names.push(name),
                _ => bail!(
                    "{} is not a path below the worktree: it has a part that is not a name",
                    relative.display()
                ),
            }
        }
        let Some((file, parents)) = names.split_last() else {
            bail!("the database path is empty");
        };
        // A file SQLite keeps beside a database is that database's, whatever
        // case the name is spelled in (a disk may not tell them apart), and
        // the directories devflow and Git keep are not for databases.
        if crate::config::names_a_sqlite_sidecar(&file.to_string_lossy()) {
            bail!(
                "{} names a file SQLite keeps beside a database, not a database",
                relative.display()
            );
        }
        if names.first().is_some_and(|first| {
            crate::config::names_a_reserved_directory(&first.to_string_lossy())
        }) {
            bail!(
                "{} is inside a directory of Git's or devflow's own",
                relative.display()
            );
        }
        let mut current = Dir::open_ambient_dir(root, ambient_authority())
            .with_context(|| format!("cannot open {}", root.display()))?;
        let mut shown = root.to_path_buf();
        for name in parents {
            shown.push(name);
            let entry = match current.symlink_metadata(name) {
                Ok(entry) => entry,
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    if !create {
                        return Ok(None);
                    }
                    match current.create_dir(name) {
                        Ok(()) => {}
                        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                        Err(e) => {
                            return Err(e)
                                .with_context(|| format!("cannot create {}", shown.display()));
                        }
                    }
                    current
                        .symlink_metadata(name)
                        .with_context(|| format!("cannot look at {}", shown.display()))?
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("cannot look at {}", shown.display()));
                }
            };
            if entry.file_type().is_symlink() {
                return Err(link_refused(&shown));
            }
            anyhow::ensure!(entry.is_dir(), "{} is not a directory", shown.display());
            let child = current
                .open_dir(name)
                .with_context(|| format!("cannot open {}", shown.display()))?;
            let opened = child
                .dir_metadata()
                .with_context(|| format!("cannot look at {}", shown.display()))?;
            // The name still leads to what was opened, and is still no link.
            let now = current
                .symlink_metadata(name)
                .map_err(|_| changed(&shown))?;
            if now.file_type().is_symlink() || !same_object(&now, &opened) {
                return Err(changed(&shown));
            }
            current = child;
        }
        Ok(Some(Place {
            dir: current,
            name: (*file).to_os_string(),
            // Spelled from the names that were checked, not from the text given
            // (`db/./x` and `db/x` are one place).
            path: names
                .iter()
                .fold(root.to_path_buf(), |path, name| path.join(name)),
        }))
    }

    /// The path SQLite is given for the file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn name(&self) -> &OsStr {
        &self.name
    }

    /// What `name` is in the directory, without following a link: `None` when
    /// it is not there, an error for a link or anything but a regular file.
    fn lookup(&self, name: &OsStr) -> Result<Option<Metadata>> {
        let shown = self.path.with_file_name(name);
        match self.dir.symlink_metadata(name) {
            Ok(entry) if entry.file_type().is_symlink() => Err(link_refused(&shown)),
            Ok(entry) if entry.is_file() => Ok(Some(entry)),
            Ok(_) => bail!(
                "{} is not a regular file; devflow left it alone",
                shown.display()
            ),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("cannot look at {}", shown.display())),
        }
    }

    /// The database file's identity; `None` when there is no file.
    pub fn identity(&self) -> Result<Option<Identity>> {
        Ok(self.lookup(&self.name)?.map(|m| Identity::of(&m)))
    }

    /// A file next to the database (a staged file, a sidecar).
    pub fn identity_of(&self, name: &OsStr) -> Result<Option<Identity>> {
        Ok(self.lookup(name)?.map(|m| Identity::of(&m)))
    }

    /// Whether the path SQLite was given reaches the file this directory
    /// holds under the database's name. Called once SQLite's lock is held: a
    /// path that was pointed elsewhere meanwhile is refused.
    pub fn confirm(&self) -> Result<()> {
        let held = self
            .identity()?
            .with_context(|| format!("{} is gone", self.path.display()))?;
        let reached = std::fs::metadata(&self.path)
            .map(|m| Identity::of_std(&m))
            .map_err(|_| changed(&self.path))?;
        if !held.is_same_file(&reached) {
            return Err(changed(&self.path));
        }
        Ok(())
    }

    /// The same for the staged file called `name`: the path it was written
    /// under is the file this directory holds.
    pub fn confirm_staged(&self, name: &OsStr) -> Result<Identity> {
        let shown = self.path.with_file_name(name);
        let held = self
            .identity_of(name)?
            .with_context(|| format!("{} is gone", shown.display()))?;
        let reached = std::fs::metadata(&shown)
            .map(|m| Identity::of_std(&m))
            .map_err(|_| changed(&shown))?;
        if !held.is_same_file(&reached) {
            return Err(changed(&shown));
        }
        Ok(held)
    }

    /// The path of a file next to the database.
    pub fn sibling(&self, name: &OsStr) -> PathBuf {
        self.path.with_file_name(name)
    }

    /// Remove `name` from the directory; a file that is not there is fine.
    pub fn remove(&self, name: &OsStr) -> Result<()> {
        match self.dir.remove_file(name) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| {
                format!("cannot remove {}", self.path.with_file_name(name).display())
            }),
        }
    }

    /// Remove the database and the files SQLite keeps beside it.
    pub fn remove_database(&self) -> Result<()> {
        self.remove(&self.name)?;
        self.remove_sidecars()
    }

    /// Remove the files SQLite keeps beside the database.
    pub fn remove_sidecars(&self) -> Result<()> {
        for suffix in SIDECARS {
            let mut name = self.name.clone();
            name.push(suffix);
            self.remove(&name)?;
        }
        Ok(())
    }

    /// Remove a staged file and what SQLite may have kept beside it. Best
    /// effort: it is the caller's own leftover.
    pub fn remove_staged(&self, staged: &OsStr) {
        let _ = self.remove(staged);
        for suffix in SIDECARS {
            let mut name = staged.to_os_string();
            name.push(suffix);
            let _ = self.remove(&name);
        }
    }

    /// Whether the path SQLite and path-based writers are given still leads
    /// to the directory this place holds (it was not swapped for a link, or
    /// for another directory, since the walk). Refused with "changed" when not.
    pub fn confirm_dir(&self) -> Result<()> {
        let held = self
            .dir
            .dir_metadata()
            .with_context(|| format!("cannot look at the directory of {}", self.path.display()))?;
        let parent = self.path.parent().unwrap_or(Path::new("."));
        let reached = std::fs::metadata(parent).map_err(|_| changed(parent))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if held.dev() != reached.dev() || held.ino() != reached.ino() {
                return Err(changed(parent));
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (held, reached);
            return Err(changed(parent));
        }
        Ok(())
    }

    /// Clone the file at `from` into a new file called `name` in the held
    /// directory, through its descriptor: blocks shared where the file system
    /// shares them (`fclonefileat`, `FICLONE`), else copied. `true` when
    /// shared. The file is created exclusively and nothing is followed, so a
    /// directory swapped for a link after the walk changes nothing about
    /// where it lands.
    pub fn clone_new(&self, from: &Path, name: &OsStr) -> std::io::Result<bool> {
        use std::os::fd::AsRawFd as _;
        let source = std::fs::File::open(from)?;
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::ffi::OsStrExt as _;
            let cname = std::ffi::CString::new(name.as_bytes())
                .map_err(|_| std::io::Error::from(ErrorKind::InvalidInput))?;
            // SAFETY: both descriptors are open for the call, and the name is a
            // NUL-terminated string that outlives it.
            let rc = unsafe {
                libc::fclonefileat(source.as_raw_fd(), self.dir.as_raw_fd(), cname.as_ptr(), 0)
            };
            if rc == 0 {
                return Ok(true);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == ErrorKind::AlreadyExists {
                return Err(error);
            }
        }
        let mut options = cap_std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = self.dir.open_with(name, &options)?.into_std();
        #[cfg(target_os = "linux")]
        {
            /// `_IOW(0x94, 9, int)`.
            const FICLONE: libc::c_ulong = 0x4004_9409;
            // SAFETY: both descriptors are open for the call.
            if unsafe { libc::ioctl(file.as_raw_fd(), FICLONE as _, source.as_raw_fd()) } == 0 {
                return Ok(true);
            }
        }
        let mut source = source;
        std::io::copy(&mut source, &mut file)?;
        file.sync_all()?;
        Ok(false)
    }

    /// Give the file called `name` in the held directory `mode`, make it
    /// durable and report its size, through the descriptor.
    #[cfg(unix)]
    pub fn finish_file(&self, name: &OsStr, mode: u32) -> std::io::Result<u64> {
        use std::os::unix::fs::PermissionsExt as _;
        let file = self.dir.open(name)?;
        file.set_permissions(cap_std::fs::Permissions::from_std(
            std::fs::Permissions::from_mode(mode),
        ))?;
        file.sync_all()?;
        Ok(file.metadata()?.len())
    }

    #[cfg(not(unix))]
    pub fn finish_file(&self, name: &OsStr, _mode: u32) -> std::io::Result<u64> {
        let file = self.dir.open(name)?;
        file.sync_all()?;
        Ok(file.metadata()?.len())
    }

    /// Remove what a path-based writer left at the path of the staged file
    /// `name` when the directory was swapped under it: if the held directory
    /// does not have it, the file at the path is this operation's own (the
    /// name carries its identity) and goes, with what SQLite kept beside it.
    pub fn remove_stray(&self, name: &OsStr) {
        if !matches!(self.identity_of(name), Ok(None)) {
            return;
        }
        for suffix in std::iter::once("").chain(SIDECARS) {
            let mut stray = name.to_os_string();
            stray.push(suffix);
            let path = self.sibling(&stray);
            if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    /// Take the database out of the way, but only if it is `expect`, the
    /// inode SQLite's lock was taken on: the file is renamed through the
    /// directory descriptor to a private name, that name's identity is checked
    /// and only then is it a file devflow may remove. If another file was put
    /// at the name since (so the lock covers a different one), it is put back
    /// where it was and the operation is refused, the other file untouched.
    /// Returns the private name; the caller removes it ([`Place::remove`]) or,
    /// when it cannot go on, puts it back ([`Place::restore`]).
    pub fn displace(&self, expect: &Identity, tag: &str) -> Result<OsString> {
        let mut private = OsString::from(".");
        private.push(&self.name);
        private.push(format!(".devflow-{tag}.displaced"));
        self.dir
            .rename(&self.name, &self.dir, &private)
            .with_context(|| format!("cannot move {} aside", self.path.display()))?;
        match self.identity_of(&private) {
            Ok(Some(now)) if now.is_same_file(expect) => Ok(private),
            _ => {
                // Not what was locked: whatever it is goes back to its name.
                if self.restore(&private).is_err() {
                    return Err(Problem::new(format!(
                        "{} was replaced by another file while devflow held the database, and \
                         that file could not be put back; it is at {}",
                        self.path.display(),
                        self.sibling(&private).display()
                    ))
                    .into_error());
                }
                Err(changed(&self.path))
            }
        }
    }

    /// Put a displaced file back at the database's name, if the name is free
    /// (a hard link that fails when it is taken, then the private name goes).
    pub fn restore(&self, private: &OsStr) -> std::io::Result<()> {
        self.dir.hard_link(private, &self.dir, &self.name)?;
        let _ = self.dir.remove_file(private);
        Ok(())
    }

    /// Put the staged file in place without replacing anything: a hard link
    /// that fails when the name is taken, then the staged name goes. The error
    /// is [`ErrorKind::AlreadyExists`] when something is there.
    pub fn install_new(&self, staged: &OsStr) -> std::io::Result<()> {
        self.dir.hard_link(staged, &self.dir, &self.name)?;
        let _ = self.dir.remove_file(staged);
        Ok(())
    }

    /// Put the staged file in place of the database, atomically.
    pub fn replace_with(&self, staged: &OsStr) -> std::io::Result<()> {
        self.dir.rename(staged, &self.dir, &self.name)
    }

    /// Make the directory entries durable (best effort).
    pub fn sync(&self) {
        if let Ok(dir) = self.dir.try_clone() {
            let _ = dir.into_std_file().sync_all();
        }
    }
}

#[cfg(unix)]
fn same_object(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(not(unix))]
fn same_object(_: &Metadata, _: &Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests;
