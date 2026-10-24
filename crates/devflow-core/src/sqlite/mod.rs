//! The `sqlite` engine: a database that is a file in the workspace.
//!
//! Nothing runs, so there is no server to branch, no container, no port and no
//! password. What devflow does is the part the others get from copy-on-write
//! storage: a workspace's database starts as a *consistent* copy of its
//! source's file (another workspace's, or a snapshot), cloned with a reflink
//! where the file system shares blocks, and reported as `reflink` or `copy`
//! like every engine. The file is the workspace's own: removing the worktree
//! removes it.
//!
//! - [`capture`] is the consistency: a copy made under SQLite's own locks, by
//!   cloning a quiesced file or through the backup API, waiting for a writer
//!   within a bound and never copying while the file can change.
//! - [`store`] keeps snapshots: immutable files under the data directory and a
//!   catalog with an intent recorded before every change.
//! - [`ops`] stages, installs, replaces and removes database files, and
//!   refuses to touch one another process has open.
//!
//! The workspace side (which file a service uses, where its data comes from,
//! the receipt's binding, `--data-from`, reset, the variables) is in
//! `setup::sqlite`.
//!
//! The library is SQLite itself, linked through `rusqlite` with the bundled
//! amalgamation: the version is known, and neither a `libsqlite3` nor a
//! `sqlite3` program has to be installed. The `-wal`, `-shm` and `-journal`
//! files SQLite keeps beside a database are never copied; a consistent copy
//! has none.
pub mod capture;
pub mod confined;
pub mod ops;
pub mod store;

pub use capture::{Error as CaptureError, How, Share};
pub use confined::Identity;
pub use ops::{FileStatus, InUse, Limits, Provisioned, Source, file_in, status, url_of};
pub use store::{Reconciled, Snapshot, Store};

/// The version of the SQLite library devflow links.
pub fn library_version() -> &'static str {
    rusqlite::version()
}
