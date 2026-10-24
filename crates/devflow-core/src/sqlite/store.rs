//! Snapshots of SQLite databases: a catalog and the files it names.
//!
//! A snapshot is a consistent copy of a database ([`super::capture`]), kept
//! under the data directory so that it outlives the workspace it was taken
//! from, and immutable: a new version of a name is a new file. A workspace
//! cloned from a snapshot gets a reflink of the file, so a seed of any size
//! costs what a metadata operation costs where the file system shares blocks.
//!
//! ```text
//! <data dir>/sqlite/<project id>/catalog.json        (0600, versioned, atomic)
//! <data dir>/sqlite/<project id>/catalog.lock
//! <data dir>/sqlite/<project id>/<service key>/<snapshot id>.sqlite   (read-only)
//! ```
//!
//! The catalog is the authority; a file is only ever removed because a record
//! names it. Every change is recorded before its first effect: a snapshot is
//! recorded `creating` (with the process that makes it) before its file
//! exists and `ready` once the file is complete and in place, and a delete is
//! recorded `deleting` before the file goes. A crash leaves a record of what
//! was going on, and [`Store::reconcile`] (which every change also runs first)
//! finishes it: an abandoned `creating` record and its partial file are
//! removed, a `deleting` record is completed. Versions of a name are never
//! reused, even after a delete.
use crate::fsutil;
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use super::capture::How;

/// Format version of the catalog file. A newer file is refused.
pub const VERSION: u32 = 1;

/// A `creating` record whose process is gone, or that is older than this, was
/// abandoned.
const ABANDONED_AFTER: chrono::Duration = chrono::Duration::hours(6);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Catalog {
    version: u32,
    project_id: Uuid,
    #[serde(default)]
    services: BTreeMap<String, Records>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Records {
    #[serde(default)]
    snapshots: Vec<Snapshot>,
    /// The highest version ever given to each name; it outlives the
    /// snapshots, so a deleted version is never given again to other data.
    #[serde(default)]
    versions: BTreeMap<String, u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Being taken: the file may be partial.
    Creating,
    Ready,
    /// Being removed: the file may be gone.
    Deleting,
}

/// One snapshot version of a service's database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: Uuid,
    pub name: String,
    /// Counts from 1 for each name.
    pub version: u32,
    pub state: State,
    pub created_at: DateTime<Utc>,
    /// When the data was captured; set when the snapshot is ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub how: Option<How>,
    /// What it was taken from: `workspace:NAME`.
    pub source: String,
    /// The process that was making it, while `creating`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

impl Snapshot {
    /// `NAME@vN`.
    pub fn label(&self) -> String {
        format!("{}@v{}", self.name, self.version)
    }
}

/// What [`Store::reconcile`] found and settled.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Reconciled {
    /// Snapshots whose making was abandoned: record and partial file removed.
    pub abandoned: Vec<String>,
    /// Snapshots whose removal had been started: finished.
    pub removed: Vec<String>,
    /// Ready snapshots whose file is gone or has another size than recorded:
    /// reported, never repaired (the data is lost).
    pub damaged: Vec<String>,
}

impl Reconciled {
    pub fn is_empty(&self) -> bool {
        self.abandoned.is_empty() && self.removed.is_empty() && self.damaged.is_empty()
    }
}

/// The snapshots of one service of one project.
#[derive(Debug, Clone)]
pub struct Store {
    project_id: Uuid,
    key: String,
    dir: PathBuf,
}

impl Store {
    /// The store of service `key` of project `project_id` under the data
    /// directory. Nothing is created until something is recorded.
    ///
    /// The key becomes a directory name and the name of a record, so it has to
    /// be a service key: a path, `..` or an absolute path is refused before
    /// anything is looked at, whether or not the configuration has it.
    pub fn open(project_id: Uuid, key: &str) -> Result<Store> {
        ensure!(
            crate::config::valid_service_key(key),
            "'{}' is not a service key: use a lowercase letter, then lowercase letters, digits, \
             '_' or '-' (31 characters at most)",
            printable(key)
        );
        Ok(Store::at(Store::root_dir(project_id)?, project_id, key))
    }

    fn root_dir(project_id: Uuid) -> Result<PathBuf> {
        Ok(crate::paths::data_dir()?
            .join("sqlite")
            .join(project_id.to_string()))
    }

    fn at(root: PathBuf, project_id: Uuid, key: &str) -> Store {
        Store {
            project_id,
            key: key.to_string(),
            dir: root,
        }
    }

    fn catalog_path(&self) -> PathBuf {
        self.dir.join("catalog.json")
    }

    fn lock_path(&self) -> PathBuf {
        self.dir.join("catalog.lock")
    }

    /// The directory the files of this service's snapshots live in.
    pub fn files_dir(&self) -> PathBuf {
        self.dir.join(&self.key)
    }

    /// Where the file of `snapshot` is.
    pub fn file_of(&self, snapshot: &Snapshot) -> PathBuf {
        self.files_dir().join(format!("{}.sqlite", snapshot.id))
    }

    fn partial_of(&self, snapshot: &Snapshot) -> PathBuf {
        self.files_dir()
            .join(format!(".{}.sqlite.partial", snapshot.id))
    }

    fn read(&self) -> Result<Catalog> {
        let path = self.catalog_path();
        match std::fs::read(&path) {
            Ok(bytes) => {
                let catalog: Catalog = serde_json::from_slice(&bytes)
                    .with_context(|| format!("{} is not a snapshot catalog", path.display()))?;
                ensure!(
                    catalog.version <= VERSION,
                    "{} was written by a newer devflow (catalog version {}, this one reads up to \
                     {VERSION})",
                    path.display(),
                    catalog.version
                );
                ensure!(
                    catalog.project_id == self.project_id,
                    "{} belongs to another project",
                    path.display()
                );
                Ok(catalog)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Catalog {
                version: VERSION,
                project_id: self.project_id,
                services: BTreeMap::new(),
            }),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    fn write(&self, catalog: &Catalog) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(catalog)?;
        fsutil::write_atomic(&self.catalog_path(), &bytes, true)
    }

    /// Change the catalog under its lock. `change` sees this service's
    /// records; the catalog is written back when it returns `Ok`.
    fn change<T>(&self, change: impl FnOnce(&mut Records) -> Result<T>) -> Result<T> {
        fsutil::with_lock(&self.lock_path(), || {
            let mut catalog = self.read()?;
            let records = catalog.services.entry(self.key.clone()).or_default();
            let value = change(records)?;
            self.write(&catalog)?;
            Ok(value)
        })
    }

    fn records(&self) -> Result<Records> {
        Ok(self.read()?.services.remove(&self.key).unwrap_or_default())
    }

    /// The snapshots that are ready, oldest first.
    pub fn list(&self) -> Result<Vec<Snapshot>> {
        let mut ready: Vec<Snapshot> = self
            .records()?
            .snapshots
            .into_iter()
            .filter(|s| s.state == State::Ready)
            .collect();
        ready.sort_by(|a, b| {
            (a.created_at, &a.name, a.version).cmp(&(b.created_at, &b.name, b.version))
        });
        Ok(ready)
    }

    /// How many snapshots are ready.
    pub fn count(&self) -> usize {
        self.list().map(|l| l.len()).unwrap_or(0)
    }

    /// Snapshots being made or removed.
    pub fn unfinished(&self) -> Result<Vec<Snapshot>> {
        Ok(self
            .records()?
            .snapshots
            .into_iter()
            .filter(|s| s.state != State::Ready)
            .collect())
    }

    /// The ready snapshot `name` at `version`, or its latest version.
    pub fn find(&self, name: &str, version: Option<u32>) -> Result<Option<Snapshot>> {
        Ok(self
            .records()?
            .snapshots
            .into_iter()
            .filter(|s| s.state == State::Ready && s.name == name)
            .filter(|s| version.is_none_or(|v| s.version == v))
            .max_by_key(|s| s.version))
    }

    /// A snapshot by its id.
    pub fn by_id(&self, id: Uuid) -> Result<Option<Snapshot>> {
        Ok(self.records()?.snapshots.into_iter().find(|s| s.id == id))
    }

    /// Record that a new version of `name` is being taken from `source`, before
    /// its file exists, and return the record. The file is
    /// [`Store::partial_path`] until [`Store::finish`] puts it in place.
    pub fn begin(&self, name: &str, source: &str) -> Result<Snapshot> {
        validate_name(name)?;
        self.reconcile()?;
        self.change(|records| {
            let version = records.versions.get(name).copied().unwrap_or(0) + 1;
            records.versions.insert(name.to_string(), version);
            let snapshot = Snapshot {
                id: Uuid::new_v4(),
                name: name.to_string(),
                version,
                state: State::Creating,
                created_at: Utc::now(),
                captured_at: None,
                bytes: None,
                how: None,
                source: source.to_string(),
                pid: Some(std::process::id()),
            };
            records.snapshots.push(snapshot.clone());
            Ok(snapshot)
        })
    }

    /// Where the file of a snapshot being made is written.
    pub fn partial_path(&self, snapshot: &Snapshot) -> Result<PathBuf> {
        let dir = self.files_dir();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        // A snapshot is a copy of somebody's data: the directories are theirs.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for private in [&dir, &self.dir] {
                let _ = std::fs::set_permissions(private, std::fs::Permissions::from_mode(0o700));
            }
        }
        Ok(self.partial_of(snapshot))
    }

    /// The partial file is complete: put it in place read-only and record the
    /// snapshot as ready.
    pub fn finish(&self, snapshot: &Snapshot, bytes: u64, how: How) -> Result<Snapshot> {
        let partial = self.partial_of(snapshot);
        let ready = self.file_of(snapshot);
        crate::cow::rename_noreplace(&partial, &ready)
            .with_context(|| format!("cannot put the snapshot in place at {}", ready.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&ready, std::fs::Permissions::from_mode(0o400));
        }
        self.change(|records| {
            let record = records
                .snapshots
                .iter_mut()
                .find(|s| s.id == snapshot.id)
                .context("the snapshot's record is gone")?;
            record.state = State::Ready;
            record.bytes = Some(bytes);
            record.how = Some(how);
            record.captured_at = Some(Utc::now());
            record.pid = None;
            Ok(record.clone())
        })
    }

    /// The snapshot could not be taken: remove its record and what it wrote.
    pub fn abort(&self, snapshot: &Snapshot) {
        let _ = std::fs::remove_file(self.partial_of(snapshot));
        let _ = std::fs::remove_file(self.file_of(snapshot));
        let _ = self.change(|records| {
            records
                .snapshots
                .retain(|s| !(s.id == snapshot.id && s.state == State::Creating));
            Ok(())
        });
    }

    /// Delete the ready snapshot `name` at `version`: recorded first, then the
    /// file, then the record.
    pub fn delete(&self, name: &str, version: u32) -> Result<Snapshot> {
        self.reconcile()?;
        let snapshot = self.change(|records| {
            let record = records
                .snapshots
                .iter_mut()
                .find(|s| s.state == State::Ready && s.name == name && s.version == version)
                .with_context(|| {
                    format!("there is no snapshot {name}@v{version} of this service")
                })?;
            record.state = State::Deleting;
            Ok(record.clone())
        })?;
        self.finish_delete(&snapshot)?;
        Ok(snapshot)
    }

    fn finish_delete(&self, snapshot: &Snapshot) -> Result<()> {
        let file = self.file_of(snapshot);
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("cannot remove {}", file.display()));
            }
        }
        let _ = std::fs::remove_file(self.partial_of(snapshot));
        self.change(|records| {
            records
                .snapshots
                .retain(|s| !(s.id == snapshot.id && s.state == State::Deleting));
            Ok(())
        })
    }

    /// Settle what a crash left: abandoned snapshots are removed, removals
    /// started are finished, and ready snapshots whose file is gone or has
    /// another size are reported.
    pub fn reconcile(&self) -> Result<Reconciled> {
        if !self.catalog_path().exists() {
            return Ok(Reconciled::default());
        }
        let mut done = Reconciled::default();
        let now = Utc::now();
        let pending: Vec<Snapshot> = self
            .records()?
            .snapshots
            .into_iter()
            .filter(|s| match s.state {
                State::Creating => {
                    s.pid.is_none_or(|pid| !process_alive(pid))
                        || now - s.created_at > ABANDONED_AFTER
                }
                State::Deleting => true,
                State::Ready => false,
            })
            .collect();
        for snapshot in pending {
            match snapshot.state {
                State::Creating => {
                    let _ = std::fs::remove_file(self.partial_of(&snapshot));
                    let _ = std::fs::remove_file(self.file_of(&snapshot));
                    self.change(|records| {
                        records
                            .snapshots
                            .retain(|s| !(s.id == snapshot.id && s.state == State::Creating));
                        Ok(())
                    })?;
                    done.abandoned.push(snapshot.label());
                }
                State::Deleting => {
                    self.finish_delete(&snapshot)?;
                    done.removed.push(snapshot.label());
                }
                State::Ready => {}
            }
        }
        done.damaged = self.damaged()?;
        Ok(done)
    }

    /// The ready snapshots whose file is gone or has another size than
    /// recorded (their data is lost). Reads only.
    pub fn damaged(&self) -> Result<Vec<String>> {
        let mut damaged = Vec::new();
        for snapshot in self.list()? {
            let size = std::fs::metadata(self.file_of(&snapshot))
                .map(|m| m.len())
                .ok();
            if size.is_none() || snapshot.bytes.is_some_and(|b| Some(b) != size) {
                damaged.push(snapshot.label());
            }
        }
        Ok(damaged)
    }

    /// A ready snapshot's file, verified to be the one recorded (it exists,
    /// has the recorded size and starts like a SQLite database): what a clone
    /// may read.
    pub fn verified_file(&self, snapshot: &Snapshot) -> Result<PathBuf> {
        let file = self.file_of(snapshot);
        let metadata = std::fs::symlink_metadata(&file).with_context(|| {
            format!(
                "the file of snapshot {} of service '{}' is gone: its data is lost",
                snapshot.label(),
                self.key
            )
        })?;
        ensure!(
            metadata.is_file() && snapshot.bytes.is_none_or(|b| b == metadata.len()),
            "the file of snapshot {} of service '{}' is not the one that was recorded: it has \
             been changed",
            snapshot.label(),
            self.key
        );
        Ok(file)
    }

    /// The service keys the project's catalog records snapshots for.
    pub fn recorded_keys(project_id: Uuid) -> Result<Vec<String>> {
        // Only the catalog is read; no service's files are touched.
        let store = Store::at(Store::root_dir(project_id)?, project_id, "");
        Ok(store
            .read()?
            .services
            .into_iter()
            .filter(|(_, records)| !records.snapshots.is_empty())
            .map(|(key, _)| key)
            .collect())
    }

    /// Whether the catalog records snapshots for this service. Reads only.
    pub fn is_recorded(&self) -> Result<bool> {
        Ok(!self.records()?.snapshots.is_empty())
    }

    /// Remove everything recorded for this service (the configuration no
    /// longer has it): the files its records name, then the records. Returns
    /// how many snapshots were recorded.
    ///
    /// Nothing is removed that the catalog does not record: a service the
    /// catalog has no snapshot of is refused before any effect, and what goes
    /// is the file of each record (and its partial file), never a directory
    /// and never what else the service's directory holds. Every record is
    /// marked `deleting` first, so an interruption is finished by
    /// [`Store::reconcile`]; the service's own directory goes last, and only
    /// when it is empty.
    pub fn forget(&self) -> Result<usize> {
        ensure!(
            crate::config::valid_service_key(&self.key),
            "'{}' is not a service key",
            printable(&self.key)
        );
        let recorded = self.change(|records| {
            ensure!(
                !records.snapshots.is_empty(),
                "the catalog records no snapshot of service '{}'; nothing was removed",
                self.key
            );
            for snapshot in &mut records.snapshots {
                snapshot.state = State::Deleting;
            }
            Ok(records.snapshots.clone())
        })?;
        for snapshot in &recorded {
            for file in [self.file_of(snapshot), self.partial_of(snapshot)] {
                match std::fs::remove_file(&file) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        return Err(e).with_context(|| format!("cannot remove {}", file.display()));
                    }
                }
            }
        }
        self.change(|records| {
            *records = Records::default();
            Ok(())
        })?;
        // Only an empty directory: anything else in it is not recorded.
        let _ = std::fs::remove_dir(self.files_dir());
        Ok(recorded.len())
    }
}

/// A key as it can be shown: control characters would rewrite a terminal.
fn printable(key: &str) -> String {
    key.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// Names appear in `snapshot:NAME@vN` on command lines, so `@` and whitespace
/// are reserved (the rule PostgreSQL and container snapshots follow).
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.chars().count() > 100 {
        bail!("snapshot names must be 1-100 characters");
    }
    if name.contains('@') || name.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("snapshot names cannot contain '@', spaces or control characters");
    }
    Ok(())
}

/// Whether process `pid` exists.
fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: signal 0 delivers nothing; it only asks whether the process exists.
        let rc = unsafe { libc::kill(pid as i32, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// Where a store's files are, for tests and diagnostics.
pub fn root_of(store: &Store) -> &Path {
    &store.dir
}

#[cfg(test)]
mod tests;
