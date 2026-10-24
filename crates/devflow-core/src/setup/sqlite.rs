//! `sqlite` services: the database file of a workspace.
//!
//! A SQLite database is a file in the worktree, so there is nothing to start,
//! no server to branch, no port or password, and nothing outside the worktree
//! for a removal to account for: the worktree takes the file with it. What the
//! pipeline decides is where the file's *data* comes from, and what the
//! receipt records about it:
//!
//! - **The default workspace** uses the file already in the main checkout. It
//!   is the project's own database, not something devflow made: the binding is
//!   not owned, and no setup, reset or drop ever changes it.
//! - **Another workspace** gets a consistent copy of its source's file: the
//!   code source's by default, `--data-from workspace`, a snapshot, or nothing
//!   (`empty`: no file, the application creates one). The source is chosen
//!   once and pinned before the copy ([`Resolved::pin`]); a retry makes the
//!   same choice. The copy is staged next to its destination under the
//!   creation's identity (journaled in `receipt.creating` before the first
//!   byte) and installed without replacing anything, so an interrupted run
//!   leaves a partial file under a name that is removed by that identity.
//! - **A file that is already there** (a `[worktree] copy` entry cloned it, or
//!   Git tracks it) is left alone and not owned: devflow never overwrites or
//!   removes what it did not make, and says so.
//!
//! The operations on a file in place (snapshot, reset, drop) take the
//! workspace's receipt lock where they change the receipt, and SQLite's own
//! locks where they touch the file ([`crate::sqlite`]).
use super::*;
use crate::config::{ServiceConfig, SqliteSection};
use crate::receipt::CloneReport;
use crate::services::catalog::CloneMethod;
use crate::sqlite::{
    self, CaptureError, FileStatus, InUse, Limits, Provisioned, Snapshot, Source, Store,
    capture::{self, Options, Share},
};

/// A data source resolved for a `sqlite` service.
struct Resolved {
    source: Source,
    /// What the receipt shows: `empty`, `workspace:NAME`, `snapshot:NAME@vN`.
    label: String,
    /// What the receipt pins: `empty`, `checkout:PATH` or `snapshot:ID`.
    /// Resolving it again never looks a name up.
    pin: String,
    /// Something worth saying about the choice.
    warning: Option<String>,
}

fn section_of(project: &Project, key: &str) -> Result<SqliteSection> {
    project
        .config
        .services
        .get(key)
        .and_then(ServiceConfig::as_sqlite)
        .cloned()
        .with_context(|| {
            format!(
                "{} has no sqlite service '{key}'",
                project.config_path.display()
            )
        })
}

/// The database file of `workspace`.
fn file_of(workspace: &Workspace, section: &SqliteSection) -> PathBuf {
    sqlite::file_in(&workspace.path, section)
}

/// `1.5 MiB`.
fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Turn what the file operations report into a [`Problem`] with a way on.
fn problem(project: &Project, key: &str, relative: &str, error: anyhow::Error) -> anyhow::Error {
    let retry = |problem: Problem| problem.with(Remedy::Retry);
    if let Some(capture) = error.downcast_ref::<CaptureError>() {
        return match capture {
            CaptureError::Locked { waited } => retry(Problem::new(format!(
                "the database {relative} of service '{key}' is locked for writing by another \
                 process; waited {:.1} s and copied nothing. Let whatever writes to it finish, \
                 or raise busy_wait",
                waited.as_secs_f64()
            )))
            .with(Remedy::OpenFile {
                path: project.config_path.clone(),
            })
            .into_error(),
            CaptureError::CannotShare { why } => retry(Problem::new(format!(
                "the database {relative} of service '{key}' cannot be cloned copy-on-write \
                 ({why}), and require_cow does not allow a full copy"
            )))
            .with(Remedy::OpenFile {
                path: project.config_path.clone(),
            })
            .into_error(),
            CaptureError::NotADatabase => Problem::new(format!(
                "{relative} is not a SQLite database (service '{key}'); devflow does not copy it"
            ))
            .into_error(),
            CaptureError::Missing => Problem::new(format!(
                "there is no database file {relative} to copy (service '{key}')"
            ))
            .into_error(),
            _ => error,
        };
    }
    if let Some(cannot) = error.downcast_ref::<sqlite::ops::CannotLock>() {
        return retry(Problem::new(format!(
            "{cannot}: {relative} (service '{key}') was left as it is. Make sure it is a \
             database this account can write, then try again"
        )))
        .with(Remedy::Reveal {
            path: project.root.clone(),
        })
        .into_error();
    }
    if let Some(in_use) = error.downcast_ref::<InUse>() {
        return retry(Problem::new(format!(
            "{in_use}: {relative} (service '{key}') is replaced or removed only while nothing \
             has it open. Stop the program that uses it, then try again"
        )))
        .into_error();
    }
    error
}

/// A reset or a drop acts on the database the receipt's binding names, and
/// only on that one: the path the configuration gives the service now may be
/// another file (an existing one, somebody's own) that the binding does not
/// cover, and the binding is not moved onto it by a change of configuration.
fn ensure_bound_path(
    workspace: &Workspace,
    key: &str,
    binding: &ServiceBinding,
    section: &SqliteSection,
    what: &str,
) -> Result<()> {
    if binding.database == section.path {
        return Ok(());
    }
    Err(Problem::new(format!(
        "service '{key}' is configured at {} now, but the database of workspace '{}' was made \
         at {}, so a {what} would act on a file this workspace's binding does not name. Set the \
         workspace up to bind the new path first; nothing was changed",
        section.path, workspace.name, binding.database
    ))
    .with(Remedy::Setup {
        workspace: workspace.path.clone(),
    })
    .into_error())
}

/// The clone report a receipt keeps.
fn report(made: &Provisioned) -> CloneReport {
    CloneReport {
        method: made.method,
        captured_at: made.method.map(|_| chrono::Utc::now()),
        allocated_bytes: None,
        source_downtime_ms: None,
    }
}

/// How a clone is worded: `(reflink, 1.5 MiB)`.
fn how(made: &Provisioned) -> String {
    match made.method {
        Some(method) => format!(" ({method}, {})", human(made.bytes)),
        None => " (empty)".to_string(),
    }
}

fn checkout_pin(path: &Path) -> String {
    format!("checkout:{}", path.display())
}

impl Runner<'_> {
    /// The `sqlite` service `key`: keep, adopt or provision its binding.
    pub(super) fn sqlite(&mut self, key: &str) -> Result<String> {
        let section = section_of(self.project, key)?;
        let relative = section.path.clone();
        let dest = sqlite::file_in(&self.workspace.path, &section);
        let store = Store::open(self.project.id(), key)?;
        // For what is only said about a file that is there (a binding kept, the
        // default workspace's own): its path as the system resolves it.
        let present = std::fs::symlink_metadata(&dest).is_ok();

        // A binding recorded before: the same file is kept.
        if let Some(binding) = self.receipt.services.get(key).cloned() {
            ensure!(
                binding.engine == Engine::Sqlite,
                "service '{key}' is bound as {} and configured as sqlite; their data is not \
                 interchangeable. Use another key for the new engine",
                binding.engine
            );
            if binding.database == relative {
                // A creation or reset of this file that died after the file
                // was in place left its intent behind: settle it.
                if let Some(stale) = self
                    .receipt
                    .creating
                    .clone()
                    .filter(|p| p.service == key && p.engine == Some(Engine::Sqlite))
                {
                    sqlite::ops::clear_staged(&self.workspace.path, &dest, stale.branch_id);
                    self.receipt.creating = None;
                }
                if !present
                    && binding.owned
                    && binding.cloned.as_ref().is_some_and(|c| c.method.is_some())
                {
                    self.warnings.push(format!(
                        "the database {relative} of service '{key}' is gone; its data is lost, \
                         and setup does not clone it again (a reset does)"
                    ));
                }
                self.stamp(key)?;
                return Ok(if present {
                    format!("{relative} is in place")
                } else {
                    format!("{relative} does not exist yet")
                });
            }
            self.warnings.push(format!(
                "service '{key}' now uses {relative} (it was {}); the file at the old path is \
                 left as it is",
                binding.database
            ));
            self.receipt.services.remove(key);
        }

        let intent = self
            .receipt
            .creating
            .clone()
            .filter(|p| p.service == key && p.engine == Some(Engine::Sqlite));
        let pending = intent.as_ref().map(|p| p.branch_id);
        let url = sqlite::url_of(&dest);

        // The default workspace's file is the project's own.
        if self.workspace.is_default {
            if self.options.data_from.is_some() {
                self.warnings.push(format!(
                    "--data-from does not apply to the default workspace's database {relative}: \
                     it is the file in the main checkout, which setup never replaces"
                ));
            }
            let detail = if present {
                format!("using {relative} in the main checkout")
            } else {
                format!("{relative} does not exist yet in the main checkout")
            };
            self.bind_sqlite(
                key,
                (uuid::Uuid::new_v4(), &relative, url),
                ("existing".into(), false),
                CloneReport::default(),
            )?;
            return Ok(detail);
        }

        // A file in the way. Reached without following a link: a `db` that is
        // a link to another checkout's is an error, not a file to adopt.
        let place =
            sqlite::confined::Place::open(&self.workspace.path, Path::new(&relative), false)
                .map_err(|error| problem(self.project, key, &relative, error))?;
        let there = match &place {
            Some(place) => place
                .identity()
                .map_err(|error| problem(self.project, key, &relative, error))?,
            None => None,
        };
        if let Some(found) = there {
            // Ours only if it is the very file an earlier run staged and
            // installed (its identity was recorded before the install). A
            // file that merely appeared while that run was cloning, or one
            // with an intent that never got as far as staging, is somebody
            // else's, whatever its header says.
            let installed = intent
                .as_ref()
                .and_then(|p| p.file)
                .is_some_and(|staged| staged.is_same_file(&found));
            if let (true, Some(id)) = (installed, pending) {
                let label = self
                    .receipt
                    .service_sources
                    .get(key)
                    .cloned()
                    .unwrap_or_else(|| "empty".into());
                self.bind_sqlite(
                    key,
                    (id, &relative, url),
                    (label, true),
                    CloneReport::default(),
                )?;
                return Ok(format!(
                    "recovered {relative} after an interrupted run ({})",
                    human(found.len)
                ));
            }
            if let Some(stale) = &intent {
                // The intent is settled, and what that run staged goes.
                sqlite::ops::clear_staged(&self.workspace.path, &dest, stale.branch_id);
                self.receipt.creating = None;
                self.warnings.push(format!(
                    "{relative} (service '{key}') appeared while an earlier run was cloning it \
                     and is not the file that run installed, so devflow left it alone and does \
                     not own it"
                ));
            } else {
                self.warnings.push(format!(
                    "{relative} (service '{key}') was already in the workspace (copied by \
                     [worktree] copy, or tracked by Git), so devflow left it alone: it is not \
                     cloned consistently, and a reset, a drop or a removal of the service never \
                     touches it. Take it out of [worktree] copy to have it cloned"
                ));
            }
            self.bind_sqlite(
                key,
                (uuid::Uuid::new_v4(), &relative, url),
                ("existing".into(), false),
                CloneReport::default(),
            )?;
            return Ok(format!(
                "{relative} was already there ({}); left alone",
                human(found.len)
            ));
        }

        // Provision it: the source is chosen once and pinned before the copy.
        let id = pending.unwrap_or_else(uuid::Uuid::new_v4);
        let resolved = match self.receipt.service_source_ids.get(key).cloned() {
            Some(pin) => {
                let label = self
                    .receipt
                    .service_sources
                    .get(key)
                    .cloned()
                    .unwrap_or_else(|| pin.clone());
                if let Some(requested) = &self.options.data_from
                    && encode_source(requested) != label
                {
                    self.warnings.push(format!(
                        "the data source of service '{key}' was already chosen as {label}; \
                         ignoring --data-from {requested}"
                    ));
                }
                resolve_pin(self.project, &self.workspace, &section, &store, &pin, label)?
            }
            None => {
                let requested = self.sqlite_requested(key, &store)?;
                let resolved =
                    resolve(self.project, &self.workspace, &section, &store, &requested)?;
                self.receipt
                    .service_source_ids
                    .insert(key.to_string(), resolved.pin.clone());
                resolved
            }
        };
        self.warnings.extend(resolved.warning.clone());
        self.receipt
            .service_sources
            .insert(key.to_string(), resolved.label.clone());
        self.receipt.creating = Some(PendingBranch {
            service: key.to_string(),
            branch_id: id,
            engine: Some(Engine::Sqlite),
            file: None,
        });
        self.receipt.save(&self.workspace.git_dir)?;
        let prepared = sqlite::ops::prepare(
            &self.workspace.path,
            &dest,
            &resolved.source,
            Limits::of(&section),
            id,
        )
        .map_err(|error| problem(self.project, key, &relative, error))?;
        // The staged file's identity is recorded before it is installed: it is
        // what a recovery compares the destination with.
        if let Some(pending) = self.receipt.creating.as_mut() {
            pending.file = prepared.identity();
        }
        if let Err(error) = self.receipt.save(&self.workspace.git_dir) {
            prepared.discard();
            return Err(error);
        }
        let made = match prepared.install() {
            Ok(made) => made,
            Err(error) if error.downcast_ref::<sqlite::ops::Collision>().is_some() => {
                // Somebody created the file meanwhile: it stays theirs. The
                // intent is settled (the next setup finds it already there and
                // leaves it alone), never carried on to adopt it.
                self.receipt.creating = None;
                self.receipt.save(&self.workspace.git_dir)?;
                return Err(Problem::new(format!(
                    "{relative} (service '{key}') appeared while it was being cloned; devflow \
                     left it alone and installed nothing. Set the workspace up again to leave \
                     it as it is, or remove it first to have it cloned"
                ))
                .with(Remedy::Setup {
                    workspace: self.workspace.path.clone(),
                })
                .into_error());
            }
            Err(error) => return Err(problem(self.project, key, &relative, error)),
        };
        self.bind_sqlite(
            key,
            (id, &relative, url),
            (resolved.label.clone(), true),
            report(&made),
        )?;
        Ok(match resolved.source {
            Source::Empty => {
                format!("{relative} starts without a file (the application creates it)")
            }
            _ => format!("{relative} from {}{}", resolved.label, how(&made)),
        })
    }

    /// Where a new database's data comes from: what was chosen before (the
    /// pin, handled by the caller), then `--data-from`, then the configured
    /// seed snapshot of a run that cannot prompt, then the code source's file.
    fn sqlite_requested(&mut self, key: &str, store: &Store) -> Result<DataSource> {
        if let Some(source) = &self.options.data_from {
            return Ok(source.clone());
        }
        if let Some((name, version)) = seed::requested(
            &self.project.config,
            self.options,
            self.workspace.is_default,
        )
        .map_err(Problem::into_error)?
        {
            let found = store.find(&name, version)?.map(|s| seed::Candidate {
                version: s.version,
                taken_at: s.captured_at.unwrap_or(s.created_at),
            });
            let choice = seed::judge(
                &self.project.config,
                &self.project.root,
                (key, Engine::Sqlite),
                &name,
                found,
                chrono::Utc::now(),
            )
            .map_err(Problem::into_error)?;
            self.warnings.extend(choice.warning);
            return Ok(choice.source);
        }
        let source = source_workspace(self.project, &self.workspace, &self.receipt)?
            .context("cannot find the workspace this one was created from")?;
        // Named by its checkout, which cannot be ambiguous.
        Ok(DataSource::Workspace {
            name: source.path.display().to_string(),
        })
    }

    /// Record the binding of `key`, with the creation settled.
    fn bind_sqlite(
        &mut self,
        key: &str,
        (id, relative, url): (uuid::Uuid, &str, String),
        (source, owned): (String, bool),
        cloned: CloneReport,
    ) -> Result<()> {
        let applied = self.project.config.service_generation(key);
        self.receipt.services.insert(
            key.to_string(),
            ServiceBinding::sqlite(
                id,
                relative.to_string(),
                url,
                source,
                owned,
                cloned,
                applied,
            ),
        );
        // The file is in place (or was never to be): the creation this binding
        // resolves is settled, and one it does not resolve (another service's)
        // is left where it is.
        self.settle_creation(key, Engine::Sqlite, Some(id));
        self.receipt.save(&self.workspace.git_dir)
    }
}

/// What a `--data-from` request means for sqlite service `key`.
fn resolve(
    project: &Project,
    workspace: &Workspace,
    section: &SqliteSection,
    store: &Store,
    requested: &DataSource,
) -> Result<Resolved> {
    match requested {
        DataSource::Empty => Ok(Resolved {
            source: Source::Empty,
            label: "empty".into(),
            pin: "empty".into(),
            warning: None,
        }),
        DataSource::Workspace { name } => {
            let other = worktree::find(project, name)?.with_context(|| {
                format!("no workspace named '{name}' to take the database from")
            })?;
            ensure!(
                !other.missing,
                "workspace '{}' is missing on disk",
                other.name
            );
            ensure!(
                other.path != workspace.path,
                "a workspace cannot take its database from itself"
            );
            from_checkout(&other, section, format!("workspace:{}", other.name))
        }
        DataSource::Snapshot { name, version } => {
            let version = version.and_then(|v| u32::try_from(v).ok());
            let snapshot = store.find(name, version)?.with_context(|| match version {
                Some(v) => format!("this service has no snapshot {name}@v{v}"),
                None => format!("this service has no snapshot named '{name}'"),
            })?;
            from_snapshot(store, &snapshot)
        }
        DataSource::Branch { .. } | DataSource::BranchPrefix { .. } => bail!(
            "a SQLite database has no branches to take data from: use a workspace, \
             snapshot:NAME[@vN] or empty"
        ),
    }
}

fn from_checkout(other: &Workspace, section: &SqliteSection, label: String) -> Result<Resolved> {
    // Reached without following a link: a `db` that is a link to another
    // checkout's is refused, never read as this one's.
    let found = sqlite::ops::source_file(&other.path, Path::new(&section.path))?;
    let (source, warning) = if let Some(path) = found {
        (Source::File { path }, None)
    } else {
        (
            Source::Empty,
            Some(format!(
                "workspace '{}' has no database file {} yet, so this workspace starts without \
                 one",
                other.name, section.path
            )),
        )
    };
    Ok(Resolved {
        source,
        label,
        pin: checkout_pin(&other.path),
        warning,
    })
}

fn from_snapshot(store: &Store, snapshot: &Snapshot) -> Result<Resolved> {
    Ok(Resolved {
        source: Source::Snapshot {
            file: store.verified_file(snapshot)?,
        },
        label: format!("snapshot:{}", snapshot.label()),
        pin: format!("snapshot:{}", snapshot.id),
        warning: None,
    })
}

/// The source a receipt pinned, resolved again without looking a name up: a
/// checkout or snapshot that is gone is an error, never another one with the
/// same name.
fn resolve_pin(
    project: &Project,
    workspace: &Workspace,
    section: &SqliteSection,
    store: &Store,
    pin: &str,
    label: String,
) -> Result<Resolved> {
    if pin == "empty" {
        return Ok(Resolved {
            source: Source::Empty,
            label,
            pin: pin.to_string(),
            warning: None,
        });
    }
    if let Some(path) = pin.strip_prefix("checkout:") {
        let path = PathBuf::from(path);
        let other = worktree::list(project)?
            .into_iter()
            .find(|w| w.path == path && !w.missing)
            .with_context(|| {
                format!(
                    "the data source was chosen as {label} (the checkout at {}) and is gone; \
                     devflow copies that exact checkout's database or none (set the workspace \
                     up with another data source)",
                    path.display()
                )
            })?;
        ensure!(
            other.path != workspace.path,
            "a workspace cannot take its database from itself"
        );
        return from_checkout(&other, section, label);
    }
    if let Some(id) = pin.strip_prefix("snapshot:") {
        let id: uuid::Uuid = id
            .parse()
            .with_context(|| format!("'{pin}' is not a recorded data source"))?;
        let snapshot = store
            .by_id(id)?
            .filter(|s| s.state == sqlite::store::State::Ready)
            .with_context(|| {
                format!(
                    "the data source was chosen as {label} and is gone; devflow clones that \
                     exact snapshot or none, and does not look the name up again"
                )
            })?;
        return from_snapshot(store, &snapshot);
    }
    bail!("'{pin}' is not a recorded data source")
}

// --- operations on a workspace's database -------------------------------------

/// A sqlite service's database in one workspace, as listings show it.
#[derive(Debug, Clone, Serialize)]
pub struct SqliteDatabase {
    /// The identity the receipt gave the database (shortened in listings).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<uuid::Uuid>,
    pub workspace: String,
    /// The workspace's checkout.
    pub workspace_path: PathBuf,
    /// The database file in it.
    pub path: PathBuf,
    pub is_default: bool,
    #[serde(flatten)]
    pub file: FileStatus,
    /// The receipt's binding of the service, when the workspace has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub owned: bool,
    /// How the data was cloned (`reflink` or `copy`); `None` for an empty
    /// start, a file that was already there and a workspace not bound yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloned: Option<CloneMethod>,
}

/// The sqlite service `key` in every workspace of the project: where its file
/// is, what it is like and what the receipt says about it. Reads local files
/// only (no database is opened).
pub fn sqlite_databases(project: &Project, key: &str) -> Result<Vec<SqliteDatabase>> {
    let section = section_of(project, key)?;
    let mut all = Vec::new();
    for workspace in worktree::list(project)? {
        if workspace.missing {
            continue;
        }
        let path = file_of(&workspace, &section);
        let binding = receipt(project, &workspace)
            .ok()
            .flatten()
            .and_then(|r| r.services.get(key).cloned())
            .filter(|b| b.engine == Engine::Sqlite);
        all.push(SqliteDatabase {
            id: binding.as_ref().and_then(|b| b.branch_id),
            workspace: workspace.name.clone(),
            workspace_path: workspace.path.clone(),
            is_default: workspace.is_default,
            file: sqlite::status(&path),
            path,
            source: binding.as_ref().map(|b| b.source.clone()),
            owned: binding.as_ref().is_some_and(|b| b.owned),
            cloned: binding.and_then(|b| b.cloned).and_then(|c| c.method),
        });
    }
    Ok(all)
}

/// Whether the catalog records snapshots for sqlite service `key` (an error
/// when `key` is not a service key).
pub fn sqlite_recorded(project: &Project, key: &str) -> Result<bool> {
    Store::open(project.id(), key)?.is_recorded()
}

/// The sqlite services the project recorded snapshots for and the
/// configuration no longer has: their snapshots are kept until they are
/// forgotten explicitly.
pub fn sqlite_orphans(project: &Project) -> Result<Vec<String>> {
    let configured: Vec<&str> = project.config.sqlite_services().map(|(k, _)| k).collect();
    let mut keys = Store::recorded_keys(project.id())?;
    keys.retain(|key| !configured.contains(&key.as_str()));
    Ok(keys)
}

/// Remove the snapshots recorded for sqlite service `key`, which the
/// configuration no longer has. Refused for a service that is configured
/// (read again from disk). Returns what was removed.
pub fn sqlite_forget(project: &Project, key: &str) -> Result<Vec<String>> {
    // A key is checked before the configuration is read, and the catalog has
    // to record the service before anything is removed.
    let store = Store::open(project.id(), key)?;
    let fresh = project.reload_same()?;
    ensure!(
        fresh.config.sqlite_services().all(|(k, _)| k != key),
        "service '{key}' is configured again in {}; remove it from the configuration first \
         (devflow never deletes the data of a configured service)",
        fresh.config_path.display()
    );
    let removed = store.forget()?;
    Ok(if removed == 0 {
        Vec::new()
    } else {
        vec![format!(
            "{removed} snapshot{} of service '{key}'",
            if removed == 1 { "" } else { "s" }
        )]
    })
}

/// The snapshots of sqlite service `key` that are ready, oldest first.
pub fn sqlite_snapshots(project: &Project, key: &str) -> Result<Vec<Snapshot>> {
    section_of(project, key)?;
    Store::open(project.id(), key)?.list()
}

/// Delete snapshot `name`@v`version` of sqlite service `key`.
pub fn sqlite_snapshot_delete(
    project: &Project,
    key: &str,
    name: &str,
    version: u32,
) -> Result<Snapshot> {
    section_of(project, key)?;
    Store::open(project.id(), key)?.delete(name, version)
}

/// Settle what an interrupted snapshot or delete of sqlite service `key` left.
pub fn sqlite_reconcile(project: &Project, key: &str) -> Result<sqlite::Reconciled> {
    section_of(project, key)?;
    Store::open(project.id(), key)?.reconcile()
}

/// A new version of snapshot `name` of sqlite service `key`, from the
/// database of `workspace`: a consistent copy taken under SQLite's own locks,
/// a reflink of the quiesced file where blocks are shared and a backup where
/// they are not. A source a writer holds is waited for within `busy_wait` and
/// refused after that.
///
/// With `expect`, only if the workspace still holds the instance and database
/// the caller showed ([`WorkspaceChanged`] otherwise).
pub fn sqlite_snapshot(
    project: &Project,
    workspace: &Workspace,
    key: &str,
    name: &str,
    expect: Option<BranchExpect>,
) -> Result<Snapshot> {
    let section = section_of(project, key)?;
    let relative = section.path.clone();
    if let Some(expect) = expect {
        let receipt = load_receipt(project, workspace)?.ok_or_else(|| needs_setup(workspace))?;
        expect.verify(workspace, key, &receipt)?;
    }
    let file = sqlite::ops::source_file(&workspace.path, Path::new(&relative))
        .map_err(|error| problem(project, key, &relative, error))?
        .with_context(|| {
            format!(
                "workspace '{}' has no database file {relative} yet, so there is nothing to \
                 snapshot",
                workspace.name
            )
        })?;
    let store = Store::open(project.id(), key)?;
    let begun = store.begin(name, &format!("workspace:{}", workspace.name))?;
    let result = (|| -> Result<_> {
        let partial = store.partial_path(&begun)?;
        capture::capture(
            &file,
            &partial,
            &Options {
                wait: section.busy_wait(),
                share: Share::Try,
            },
        )
        .map_err(anyhow::Error::new)
    })();
    match result {
        Ok(made) => store.finish(&begun, made.bytes, made.how),
        Err(error) => {
            store.abort(&begun);
            Err(problem(project, key, &relative, error))
        }
    }
}

/// What a reset did.
#[derive(Debug, Clone, Serialize)]
pub struct SqliteReset {
    pub path: PathBuf,
    pub source: String,
    pub method: Option<CloneMethod>,
    pub bytes: u64,
}

/// Replace the database of sqlite service `key` in `workspace` with a fresh
/// copy of `to`, or of the source it was created from. Only a database devflow
/// made for this workspace is replaced; it must not be open in another process.
/// With `expect`, the instance and database the caller showed are verified
/// under the receipt lock, before any effect ([`WorkspaceChanged`] otherwise).
pub fn sqlite_reset(
    project: &Project,
    workspace: &Workspace,
    key: &str,
    to: Option<DataSource>,
    expect: Option<BranchExpect>,
) -> Result<SqliteReset> {
    let section = section_of(project, key)?;
    let relative = section.path.clone();
    ensure!(
        !workspace.is_default,
        "the default workspace's database is the project's own file; a reset does not replace it"
    );
    let lock = lock(workspace)?;
    let mut receipt =
        load_receipt_locked(project, workspace, &lock)?.ok_or_else(|| needs_setup(workspace))?;
    if let Some(expect) = expect {
        expect.verify(workspace, key, &receipt)?;
    }
    let binding = receipt
        .services
        .get(key)
        .filter(|b| b.engine == Engine::Sqlite)
        .cloned()
        .with_context(|| {
            format!(
                "workspace '{}' has no database of service '{key}' bound",
                workspace.name
            )
        })?;
    ensure_bound_path(workspace, key, &binding, &section, "reset")?;
    ensure!(
        binding.owned,
        "{relative} was already in this workspace, not made by devflow, so a reset does not \
         replace it"
    );
    let store = Store::open(project.id(), key)?;
    let resolved = match &to {
        Some(requested) => resolve(project, workspace, &section, &store, requested)?,
        None => {
            let pin = receipt
                .service_source_ids
                .get(key)
                .cloned()
                .context("the receipt records no data source for this database")?;
            let label = receipt
                .service_sources
                .get(key)
                .cloned()
                .unwrap_or_else(|| binding.source.clone());
            resolve_pin(project, workspace, &section, &store, &pin, label)?
        }
    };
    let dest = file_of(workspace, &section);
    let id = uuid::Uuid::new_v4();
    // Recorded before the effect: a leftover staging file is removed by it.
    // The slot holds one intent: another service's unfinished creation is not
    // erased for this, setup settles it first.
    if let Some(other) = receipt
        .creating
        .as_ref()
        .filter(|p| !(p.service == key && p.engine == Some(Engine::Sqlite)))
    {
        return Err(Problem::new(format!(
            "the creation of service '{}' did not finish in workspace '{}'; set the workspace up \
             to settle it before resetting the database of '{key}'",
            other.service, workspace.name
        ))
        .with(Remedy::Setup {
            workspace: workspace.path.clone(),
        })
        .into_error());
    }
    receipt.creating = Some(PendingBranch {
        service: key.to_string(),
        branch_id: id,
        engine: Some(Engine::Sqlite),
        file: None,
    });
    receipt.save(&workspace.git_dir)?;
    let made = sqlite::ops::replace(
        &workspace.path,
        &dest,
        &resolved.source,
        Limits::of(&section),
        id,
    )
    .map_err(|error| {
        // Nothing was replaced: the intent is settled.
        problem(project, key, &relative, error)
    });
    receipt.creating = None;
    let made = match made {
        Ok(made) => made,
        Err(error) => {
            receipt.save(&workspace.git_dir)?;
            return Err(error);
        }
    };
    if to.is_some() {
        receipt
            .service_sources
            .insert(key.to_string(), resolved.label.clone());
        receipt
            .service_source_ids
            .insert(key.to_string(), resolved.pin.clone());
    }
    if let Some(bound) = receipt.services.get_mut(key) {
        bound.source = resolved.label.clone();
        bound.cloned = Some(report(&made));
    }
    receipt.updated_at = chrono::Utc::now();
    receipt.save(&workspace.git_dir)?;
    Ok(SqliteReset {
        path: dest,
        source: resolved.label,
        method: made.method,
        bytes: made.bytes,
    })
}

/// Remove the database file of sqlite service `key` from `workspace` (and the
/// binding, so that the next setup provisions a fresh one from the same
/// source). Only a database devflow made for this workspace is removed, and
/// only while nothing has it open. `true` when a file was removed. With
/// `expect`, the instance and database the caller showed are verified under
/// the receipt lock, before any effect ([`WorkspaceChanged`] otherwise).
pub fn sqlite_drop(
    project: &Project,
    workspace: &Workspace,
    key: &str,
    expect: Option<BranchExpect>,
) -> Result<bool> {
    let section = section_of(project, key)?;
    let relative = section.path.clone();
    ensure!(
        !workspace.is_default,
        "the default workspace's database is the project's own file; it is not removed"
    );
    let lock = lock(workspace)?;
    let mut receipt =
        load_receipt_locked(project, workspace, &lock)?.ok_or_else(|| needs_setup(workspace))?;
    if let Some(expect) = expect {
        expect.verify(workspace, key, &receipt)?;
    }
    let binding = receipt
        .services
        .get(key)
        .filter(|b| b.engine == Engine::Sqlite)
        .cloned()
        .with_context(|| {
            format!(
                "workspace '{}' has no database of service '{key}' bound",
                workspace.name
            )
        })?;
    ensure_bound_path(workspace, key, &binding, &section, "drop")?;
    ensure!(
        binding.owned,
        "{relative} was not made by devflow for this workspace, so it is not removed"
    );
    let dest = file_of(workspace, &section);
    let removed = sqlite::ops::remove(&workspace.path, &dest, section.busy_wait())
        .map_err(|error| problem(project, key, &relative, error))?;
    receipt.services.remove(key);
    receipt.updated_at = chrono::Utc::now();
    receipt.save(&workspace.git_dir)?;
    Ok(removed)
}
