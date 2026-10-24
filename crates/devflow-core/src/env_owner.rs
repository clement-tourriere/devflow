//! Whose variables a child process gets: the workspace's own, and nobody
//! else's.
//!
//! Hooks, `devflow exec` and the mise checks start a process in a workspace
//! with that workspace's variables on top of the environment they inherit. The
//! inherited environment may still carry another workspace's (a shell that
//! sourced its `.devflow/env`, a mise `_.file`, a hook that runs `devflow
//! switch`), and a name this workspace does not set (a `--no-db` workspace
//! has no `DATABASE_URL`, a removed recipe no value) would keep the caller's
//! value and point the command at the wrong database. One rule decides what
//! happens to every name, and every place that builds such an environment
//! applies it through [`ChildEnv`]:
//!
//! - **Owned names** are taken out of the inherited environment, then the
//!   workspace's values are set. They are the union of
//!   - the **target set**: every name the target's configuration declares,
//!     bound or not ([`declared_names`]): identity (`DEVFLOW_PROJECT`, …,
//!     `COMPOSE_PROJECT_NAME`), the connection outputs of each configured
//!     service (PostgreSQL: `DATABASE_URL`, the libpq family, each
//!     `<NAME>_DATABASE_URL`; Valkey and Redis: the `env` name; MySQL: its URL
//!     variable and `MYSQL_*`) and the `[env]` recipes and presets. A
//!     `--no-db` workspace of a project with PostgreSQL still owns
//!     `DATABASE_URL`. With them go the **former names**: every name the
//!     workspace instance has ever exported (the receipt keeps them,
//!     `.devflow/env` lists the ones the configuration no longer declares), so
//!     a shell that still holds a removed recipe's variable is cleared of it
//!     after the file was written again;
//!   - the **source set**: when the inherited environment names the workspace
//!     it came from (`DEVFLOW_WORKSPACE_PATH` and `DEVFLOW_WORKSPACE_ID`) and
//!     that workspace's `.devflow/env` is *authenticated*, every name in that
//!     file, and the former names it lists, another project's included.
//!     Authenticated means a plain file in a
//!     plain `.devflow` directory (nothing reached through a symlink) that
//!     records the same `DEVFLOW_WORKSPACE_ID` as the inherited one. Matching
//!     the path and the id only decides what to clear: it confers neither
//!     ownership nor approval of anything.
//! - **A source that cannot be resolved is not "no source".** When the
//!   inherited environment carries `DEVFLOW_WORKSPACE_PATH` or
//!   `DEVFLOW_WORKSPACE_ID` and the workspace they name cannot be resolved (its
//!   directory is gone, it has no file, the file is reached through a symlink
//!   or records another id), the variables in the environment may be that
//!   workspace's and nothing says which: nothing is started ([`Stale`]).
//!   `DEVFLOW_CLEAR_INHERITED=1` (or `devflow exec --clear-inherited`) goes
//!   on anyway, taking out the target set and every name devflow can generate
//!   ([`generatable_names`]).
//! - **Execution controls are never owned**: `DEVFLOW_SETUP_ACTIVE`,
//!   `DEVFLOW_PHASE`, `DEVFLOW_TRUST_HOOKS`, `DEVFLOW_NON_INTERACTIVE`,
//!   `DEVFLOW_CD_FILE`, `DEVFLOW_BIN`, `DEVFLOW_CONFIG_DIR`,
//!   `DEVFLOW_DATA_DIR`, `DEVFLOW_INSTANCE_ID`, `DEVFLOW_INHERIT`,
//!   `DEVFLOW_CLEAR_INHERITED` and every other `DEVFLOW_*` that is not
//!   identity: a devflow started by a hook depends on them to know it is
//!   nested. Nor are the names a process needs to find programs and its home
//!   (`PATH`, `HOME`, …): a recipe that sets one still sets it, but nothing is
//!   ever taken away from the caller because another workspace's file
//!   mentioned it.
//! - **Explicit inheritance**: names the caller listed (`devflow exec
//!   --inherit NAME`, or `DEVFLOW_INHERIT=NAME,NAME` in the environment) keep
//!   the caller's value when the target sets no value of its own. A value the
//!   target sets always wins.
//!
//! `devflow exec` reads the target's names from the versioned manifest that
//! setup records in `.devflow/env` ([`crate::env_file::write_owned`]), so it
//! still needs only that file (and, when the inherited environment names a
//! source workspace, that one's file): no project scan, no registry, no
//! server. A file without the manifest, written by an earlier devflow, is not
//! guessed at: it needs to be written again ([`NotReady::NoManifest`]).
//!
//! Messages and diagnostics here name variables and files, never values.
use crate::config::{Config, EnvPreset, MYSQL_FIELD_ENVS, valid_env_name};
use crate::env_file::{self, Contents};
use crate::remedy::{Problem, Remedy};
use crate::{config, conn};
use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The names a workspace carries to say which one it is, and the port it was
/// given (`DEVFLOW_PORT`: unlike the other `DEVFLOW_*` names it is the
/// workspace's, never an execution control).
pub const IDENTITY: [&str; 8] = [
    "DEVFLOW_PROJECT",
    "DEVFLOW_PROJECT_ROOT",
    "DEVFLOW_WORKSPACE",
    "DEVFLOW_WORKSPACE_PATH",
    "DEVFLOW_SOURCE_PATH",
    "DEVFLOW_WORKSPACE_ID",
    "COMPOSE_PROJECT_NAME",
    "DEVFLOW_PORT",
];

/// The environment variable that lists the names to inherit: comma separated.
pub const INHERIT: &str = "DEVFLOW_INHERIT";

/// The environment variable that goes on without the variables of a workspace
/// that cannot be resolved: `1`, `true` or `yes`.
pub const CLEAR_INHERITED: &str = "DEVFLOW_CLEAR_INHERITED";

/// Names a process needs to run at all. Never taken away.
const PROCESS: [&str; 9] = [
    "PATH", "HOME", "PWD", "OLDPWD", "SHELL", "USER", "LOGNAME", "TMPDIR", "TERM",
];

/// The largest `.devflow/env` read from another workspace.
const MAX_SOURCE_BYTES: u64 = 1 << 20;

/// Whether `name` controls how devflow runs (and must reach nested devflow
/// processes as it is): `DEVFLOW_*` other than the identity names.
pub fn is_execution_control(name: &str) -> bool {
    name.starts_with("DEVFLOW_") && !IDENTITY.contains(&name)
}

/// Whether a workspace's environment may ever take `name` away from a caller.
fn may_clear(name: &str) -> bool {
    valid_env_name(name) && !is_execution_control(name) && !PROCESS.contains(&name)
}

fn names(list: impl IntoIterator<Item = impl AsRef<str>>) -> BTreeSet<String> {
    list.into_iter()
        .map(|name| name.as_ref().to_string())
        .collect()
}

/// Every name `config` declares for a workspace, whether this workspace has a
/// value for it or not. See the module documentation for the categories.
pub fn declared_names(config: &Config) -> BTreeSet<String> {
    let mut found = names(IDENTITY);
    if config.postgres().is_some() {
        found.insert("DATABASE_URL".into());
        found.extend(names(conn::libpq_env_vars()));
        found.extend(names(conn::pg_env_names()));
    }
    // Scratch database URLs, the URL of each container service and a mysql
    // service's typed fields.
    found.extend(
        config
            .exported_variables()
            .into_iter()
            .map(|(name, _)| name),
    );
    for preset in &config.env.presets {
        found.extend(names(preset.keys()));
    }
    found.extend(config.env.vars.keys().cloned());
    found.retain(|name| may_clear(name));
    found
}

/// [`declared_names`] for a workspace whose variables are `vars`: also
/// PostgreSQL's family when it has a `DATABASE_URL`. An attachment made with
/// no PostgreSQL service configured (`setup --database-url`) exports a
/// `DATABASE_URL` the configuration does not declare, and its libpq selectors
/// are as much the workspace's as any other's.
pub fn declared_for(config: &Config, vars: &[(String, String)]) -> BTreeSet<String> {
    let mut found = declared_names(config);
    if vars.iter().any(|(name, _)| name == "DATABASE_URL") {
        found.extend(names(conn::libpq_env_vars()));
        found.extend(names(conn::pg_env_names()));
    }
    found
}

/// Every name devflow can generate for some workspace, whichever its
/// configuration: identity, `DATABASE_URL` and the libpq family, the default
/// URL variables of the container services, `MYSQL_*`, the scratch databases'
/// reserved URLs and the variables every preset sets. What the override for an
/// unresolved source takes out, besides the workspace's own names.
pub fn generatable_names() -> BTreeSet<String> {
    let mut found = names(IDENTITY);
    found.insert("DATABASE_URL".into());
    found.extend(names(conn::libpq_env_vars()));
    found.extend(names(conn::pg_env_names()));
    found.extend(names(MYSQL_FIELD_ENVS));
    found.extend(names([
        config::DEFAULT_CACHE_ENV,
        config::MYSQL_URL_ENV,
        config::SQLITE_URL_ENV,
        config::DEFAULT_SQLITE_PATH_ENV,
        "TEST_DATABASE_URL",
        "SHADOW_DATABASE_URL",
    ]));
    for preset in EnvPreset::ALL {
        found.extend(names(preset.keys()));
    }
    found.retain(|name| may_clear(name));
    found
}

/// What the environment a process inherited says about where it came from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inherited {
    /// `DEVFLOW_WORKSPACE_PATH`.
    pub workspace_path: Option<PathBuf>,
    /// `DEVFLOW_WORKSPACE_ID`.
    pub workspace_id: Option<String>,
    /// `DEVFLOW_WORKSPACE`, to name the source in diagnostics.
    pub workspace_name: Option<String>,
    /// Names listed in `DEVFLOW_INHERIT`.
    pub keep: BTreeSet<String>,
    /// Go on although the workspace the environment names cannot be resolved
    /// (`DEVFLOW_CLEAR_INHERITED`).
    pub clear_stale: bool,
}

impl Inherited {
    /// Read the process's own environment.
    pub fn from_process() -> Inherited {
        let get = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
        let text = |name: &str| get(name).and_then(|value| value.into_string().ok());
        Inherited {
            workspace_path: get("DEVFLOW_WORKSPACE_PATH").map(PathBuf::from),
            workspace_id: text("DEVFLOW_WORKSPACE_ID"),
            workspace_name: text("DEVFLOW_WORKSPACE"),
            keep: text(INHERIT)
                .map(|list| inherit_list(&list))
                .unwrap_or_default(),
            clear_stale: text(CLEAR_INHERITED)
                .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "yes")),
        }
    }

    /// Also keep `names`.
    pub fn keeping(mut self, names: impl IntoIterator<Item = String>) -> Inherited {
        self.keep.extend(names);
        self
    }

    /// Go on although the source cannot be resolved.
    pub fn clearing_stale(mut self) -> Inherited {
        self.clear_stale = true;
        self
    }
}

/// The valid names in a comma-separated `DEVFLOW_INHERIT` value.
pub fn inherit_list(list: &str) -> BTreeSet<String> {
    list.split(',')
        .map(str::trim)
        .filter(|name| valid_env_name(name))
        .map(str::to_string)
        .collect()
}

/// The workspace the inherited environment came from, when its file proves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub path: PathBuf,
    pub name: Option<String>,
    /// The variables its `.devflow/env` sets and the former names it lists
    /// (names only matter here).
    pub names: BTreeSet<String>,
}

/// Why the workspace the inherited environment names cannot be resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleReason {
    /// The environment has an id and no path (or the path is not absolute).
    NoPath,
    /// The environment has a path and no id: nothing says which instance.
    NoId,
    /// The directory is gone, or is not one.
    Gone,
    /// The directory has no `.devflow/env`.
    NoFile,
    /// `.devflow` or `.devflow/env` is a symlink or not the plain thing, or
    /// too large: it is never followed.
    NotPlain,
    /// The file cannot be read.
    Unreadable,
    /// The file records another instance: the workspace was set up again, or
    /// the path is another workspace's.
    OtherInstance,
}

impl StaleReason {
    fn describe(self) -> &'static str {
        match self {
            StaleReason::NoPath => "the environment has a workspace id and no usable path",
            StaleReason::NoId => "the environment does not say which instance of it it came from",
            StaleReason::Gone => "its directory is gone",
            StaleReason::NoFile => "it has no .devflow/env",
            StaleReason::NotPlain => {
                "its .devflow/env is not a plain file in a plain directory (a symlink is never followed)"
            }
            StaleReason::Unreadable => "its .devflow/env cannot be read",
            StaleReason::OtherInstance => {
                "its .devflow/env belongs to another instance (the workspace was set up again, or the path is another workspace's)"
            }
        }
    }
}

/// The inherited environment names a workspace that cannot be resolved: its
/// variables may be in the environment, and nothing says which. A typed
/// problem, with [`Remedy::ClearInherited`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stale {
    pub path: Option<PathBuf>,
    pub name: Option<String>,
    pub reason: StaleReason,
}

impl fmt::Display for Stale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let which = match (&self.name, &self.path) {
            (Some(name), Some(path)) => format!("workspace {name} ({})", path.display()),
            (Some(name), None) => format!("workspace {name}"),
            (None, Some(path)) => format!("a workspace at {}", path.display()),
            (None, None) => "a workspace".to_string(),
        };
        write!(
            f,
            "the environment carries variables of {which}, which cannot be resolved: {}. They \
             could be taken for this workspace's, so nothing was run",
            self.reason.describe()
        )
    }
}

impl std::error::Error for Stale {}

impl Stale {
    /// This as an error that carries its remedy.
    pub fn into_error(self) -> anyhow::Error {
        Problem::new(self.to_string())
            .with(Remedy::ClearInherited)
            .into_error()
    }
}

/// What the inherited environment says about the workspace it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provenance {
    /// It names none (`DEVFLOW_WORKSPACE_PATH` and `DEVFLOW_WORKSPACE_ID` are
    /// both unset).
    Absent,
    /// It names one and the workspace's file proves it.
    Resolved(Source),
    /// It names one that cannot be resolved.
    Unresolved(Stale),
}

/// Resolve the workspace `inherited` names, if it names one. The path and the
/// id decide what is cleared, never what is owned or approved.
pub fn provenance(inherited: &Inherited) -> Provenance {
    if inherited.workspace_path.is_none() && inherited.workspace_id.is_none() {
        return Provenance::Absent;
    }
    let stale = |reason| {
        Provenance::Unresolved(Stale {
            path: inherited.workspace_path.clone(),
            name: inherited.workspace_name.clone(),
            reason,
        })
    };
    let Some(path) = inherited
        .workspace_path
        .as_ref()
        .filter(|p| p.is_absolute())
    else {
        return stale(StaleReason::NoPath);
    };
    let Some(id) = inherited
        .workspace_id
        .as_deref()
        .filter(|id| !id.is_empty())
    else {
        return stale(StaleReason::NoId);
    };
    let contents = match read_plain(path) {
        Ok(contents) => contents,
        Err(reason) => return stale(reason),
    };
    let recorded = contents
        .vars
        .iter()
        .find(|(name, _)| name == "DEVFLOW_WORKSPACE_ID")
        .map(|(_, value)| value.as_str());
    if recorded != Some(id) {
        return stale(StaleReason::OtherInstance);
    }
    // What the file sets, and what it exported once and no longer does: a
    // shell that loaded an earlier version of the file still holds those.
    let names = contents
        .vars
        .into_iter()
        .map(|(name, _)| name)
        .chain(contents.former)
        .collect();
    Provenance::Resolved(Source {
        path: path.clone(),
        name: inherited.workspace_name.clone(),
        names,
    })
}

/// `worktree`'s `.devflow/env`, read only when it is reached without following
/// a single symlink: every component of the path, from the root of the
/// filesystem down to the file, is looked at with `symlink_metadata`, and a
/// symlink anywhere (an `alias` above the worktree included), a `.` or a `..`
/// refuses it ([`StaleReason::NotPlain`]). On Unix the file is then opened with
/// `O_NOFOLLOW` and must be the very file that was looked at.
///
/// Devflow writes the canonical path of a worktree to `DEVFLOW_WORKSPACE_PATH`,
/// so what it wrote is always accepted; a path it did not write may not be.
pub fn read_plain(worktree: &Path) -> Result<Contents, StaleReason> {
    use std::path::Component;
    let mut current = PathBuf::new();
    for component in worktree.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => current.push(component),
            Component::Normal(part) => {
                current.push(part);
                match current.symlink_metadata() {
                    Ok(meta) if meta.file_type().is_symlink() => return Err(StaleReason::NotPlain),
                    Ok(meta) if meta.is_dir() => {}
                    _ => return Err(StaleReason::Gone),
                }
            }
            Component::CurDir | Component::ParentDir => return Err(StaleReason::NotPlain),
        }
    }
    let dir = worktree.join(env_file::DIR);
    match dir.symlink_metadata() {
        Ok(meta) if meta.file_type().is_dir() => {}
        Ok(_) => return Err(StaleReason::NotPlain),
        Err(_) => return Err(StaleReason::NoFile),
    }
    let file = env_file::path(worktree);
    let looked_at = match file.symlink_metadata() {
        Ok(meta) if meta.file_type().is_file() && meta.len() <= MAX_SOURCE_BYTES => meta,
        Ok(_) => return Err(StaleReason::NotPlain),
        Err(_) => return Err(StaleReason::NoFile),
    };
    let text = open_plain(&file, &looked_at).map_err(|_| StaleReason::Unreadable)?;
    Ok(env_file::parse(&text))
}

/// The text of `file`, opened without following a symlink and checked to be
/// the file `looked_at` described.
fn open_plain(file: &Path, looked_at: &std::fs::Metadata) -> std::io::Result<String> {
    use std::io::Read;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let opened = options.open(file)?;
    let meta = opened.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.dev() != looked_at.dev() || meta.ino() != looked_at.ino() {
            return Err(std::io::Error::other("the file changed while it was read"));
        }
    }
    if !meta.is_file() || meta.len() > MAX_SOURCE_BYTES {
        return Err(std::io::Error::other("not a plain file"));
    }
    let mut text = String::new();
    opened.take(MAX_SOURCE_BYTES).read_to_string(&mut text)?;
    Ok(text)
}

/// Every name the `.devflow/env` of the worktree at `worktree` exported (its
/// variables) or lists as exported once (its former names), read the way a
/// source file is: never through a symlink. Empty when there is no such file
/// or it cannot be trusted as one. What setup keeps of a file before it writes
/// another over it.
pub fn previous_names(worktree: &Path) -> BTreeSet<String> {
    read_plain(worktree)
        .map(|contents| {
            contents
                .vars
                .into_iter()
                .map(|(name, _)| name)
                .chain(contents.former)
                .filter(|name| may_clear(name))
                .collect()
        })
        .unwrap_or_default()
}

/// Why a workspace's `.devflow/env` cannot give an environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotReady {
    /// It was written without the list of owned names, by an earlier devflow
    /// ([`Contents::lacks_manifest`], the one predicate for this).
    NoManifest,
    /// The inherited environment names a workspace that cannot be resolved.
    Stale(Stale),
}

impl NotReady {
    /// The typed problem for the workspace at `root`: setting it up again
    /// writes the file, or the override goes on without the stale variables.
    pub fn into_error(self, root: &Path) -> anyhow::Error {
        match self {
            NotReady::NoManifest => Problem::new(format!(
                "{} was written by an earlier devflow and does not list the names the workspace \
                 owns, so nothing was run from it",
                env_file::path(root).display()
            ))
            .with(Remedy::Setup {
                workspace: root.to_path_buf(),
            })
            .into_error(),
            NotReady::Stale(stale) => stale.into_error(),
        }
    }
}

/// The environment of a process started in a workspace: the workspace's
/// variables and the names that are taken out of the inherited environment
/// first.
#[derive(Debug, Clone)]
pub struct ChildEnv {
    vars: Vec<(String, String)>,
    owned: BTreeSet<String>,
    source: Option<Source>,
    keep: BTreeSet<String>,
}

impl ChildEnv {
    /// `vars` of a workspace that owns `names` (what its configuration
    /// declares and what it exported once), against the process's own
    /// inherited environment. Refuses when that names a workspace that cannot
    /// be resolved, unless `DEVFLOW_CLEAR_INHERITED` says to go on.
    pub fn new(vars: Vec<(String, String)>, names: &BTreeSet<String>) -> Result<ChildEnv, Stale> {
        ChildEnv::resolve(vars, names, &Inherited::from_process())
    }

    /// [`ChildEnv::new`] against an explicit inherited environment.
    pub fn resolve(
        vars: Vec<(String, String)>,
        names: &BTreeSet<String>,
        inherited: &Inherited,
    ) -> Result<ChildEnv, Stale> {
        let source = match provenance(inherited) {
            Provenance::Absent => None,
            Provenance::Resolved(source) => Some(source),
            Provenance::Unresolved(_) if inherited.clear_stale => None,
            Provenance::Unresolved(stale) => return Err(stale),
        };
        let mut owned: BTreeSet<String> = names.clone();
        owned.extend(vars.iter().map(|(name, _)| name.clone()));
        if let Some(source) = &source {
            owned.extend(source.names.iter().cloned());
        }
        if inherited.clear_stale {
            owned.extend(generatable_names());
        }
        owned.retain(|name| may_clear(name));
        Ok(ChildEnv {
            vars,
            owned,
            source,
            keep: inherited.keep.clone(),
        })
    }

    /// The environment of a workspace whose names are known, without looking
    /// at where the process came from: for what does not depend on the
    /// caller's environment (comparing a copied dotenv file with it).
    pub fn standalone(vars: Vec<(String, String)>, names: &BTreeSet<String>) -> ChildEnv {
        ChildEnv::resolve(vars, names, &Inherited::default())
            .expect("an environment that names no workspace is always resolved")
    }

    /// A workspace known by its variables alone, for tests: the identity names
    /// and, with a `DATABASE_URL`, PostgreSQL's family.
    #[cfg(test)]
    pub fn plain(vars: Vec<(String, String)>) -> ChildEnv {
        let owned = declared_for(&Config::default(), &vars);
        ChildEnv::standalone(vars, &owned)
    }

    /// The environment of the workspace whose worktree root is `root`, from
    /// its `.devflow/env` alone. A typed problem when the file has no list of
    /// owned names or the inherited environment is stale.
    pub fn from_file(root: &Path) -> anyhow::Result<ChildEnv> {
        ChildEnv::from_contents(env_file::read_contents(root)?).map_err(|why| why.into_error(root))
    }

    /// [`ChildEnv::from_file`] for contents already read.
    pub fn from_contents(contents: Contents) -> Result<ChildEnv, NotReady> {
        ChildEnv::from_contents_in(contents, &Inherited::from_process())
    }

    /// [`ChildEnv::from_contents`] against an explicit inherited environment.
    pub fn from_contents_in(
        contents: Contents,
        inherited: &Inherited,
    ) -> Result<ChildEnv, NotReady> {
        if contents.lacks_manifest() {
            return Err(NotReady::NoManifest);
        }
        let mut owned = contents.declared.unwrap_or_default();
        owned.extend(contents.former);
        ChildEnv::resolve(contents.vars, &owned, inherited).map_err(NotReady::Stale)
    }

    /// Also keep the caller's value of `names` (`--inherit`) unless the
    /// workspace sets one.
    pub fn keeping(mut self, names: impl IntoIterator<Item = String>) -> ChildEnv {
        self.keep.extend(names);
        self
    }

    /// The workspace's variables.
    pub fn vars(&self) -> &[(String, String)] {
        &self.vars
    }

    /// The names taken out of the inherited environment (before `--inherit`).
    pub fn owned(&self) -> &BTreeSet<String> {
        &self.owned
    }

    /// The owned names the workspace sets no value for and the caller did not
    /// ask to keep: a process started with this environment must not have
    /// them. What the mise checks verify besides the values.
    pub fn absent(&self) -> impl Iterator<Item = &str> {
        self.owned
            .iter()
            .filter(|name| !self.keep.contains(*name))
            .filter(|name| !self.vars.iter().any(|(key, _)| key == *name))
            .map(String::as_str)
    }

    /// The workspace the inherited environment came from, when it is proven.
    pub fn source(&self) -> Option<&Source> {
        self.source.as_ref()
    }

    /// Take the owned names out of what `command` inherits, then set the
    /// workspace's variables. Execution controls are untouched.
    pub fn apply(&self, command: &mut Command) {
        for name in &self.owned {
            if !self.keep.contains(name) {
                command.env_remove(name);
            }
        }
        command.envs(self.vars.iter().map(|(name, value)| (name, value)));
    }
}

/// How the process environment differs from a workspace's: which owned names
/// carry another value (or one where the workspace sets none), and which
/// workspace they came from when that is known.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Leak {
    pub names: Vec<String>,
    /// The workspace the process environment names, when it is not this one.
    pub from: Option<(PathBuf, Option<String>)>,
}

/// Compare `env`'s owned names with `lookup` (the process environment): a name
/// whose inherited value differs from the workspace's, or that the workspace
/// does not set at all, is a leak. `root` is the workspace itself.
pub fn leaks(
    env: &ChildEnv,
    root: &Path,
    lookup: impl Fn(&str) -> Option<String>,
    inherited: &Inherited,
) -> Leak {
    let mut names = Vec::new();
    for name in &env.owned {
        let want = env
            .vars
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value);
        // A name the caller asked to keep is theirs only while the workspace
        // sets no value for it: its own value wins over the inherited one.
        if want.is_none() && env.keep.contains(name) {
            continue;
        }
        let Some(have) = lookup(name) else { continue };
        if want != Some(&have) {
            names.push(name.clone());
        }
    }
    let from = inherited
        .workspace_path
        .as_ref()
        .filter(|path| !same_dir(path, root))
        .map(|path| (path.clone(), inherited.workspace_name.clone()));
    Leak { names, from }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || a.canonicalize()
            .ok()
            .zip(b.canonicalize().ok())
            .is_some_and(|(a, b)| a == b)
}

/// A copied dotenv file that sets an owned name to another value than
/// `.devflow/env`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotenvConflict {
    /// Relative to the worktree, as `[worktree] copy` names it.
    pub file: String,
    pub names: Vec<String>,
}

/// Whether a `[worktree] copy` entry is a dotenv file: `.env` or `.env.*`.
fn is_dotenv(entry: &str) -> bool {
    Path::new(entry)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == ".env" || name.starts_with(".env."))
}

/// The copied dotenv files in `worktree` (the `[worktree] copy` entries named
/// `.env` or `.env.*`) that define a name `env` owns with a value other than
/// the one in `.devflow/env`. Frameworks and dotenv loaders read such a file
/// themselves, so a database URL copied from the main checkout beats the
/// workspace's whenever the command does not go through devflow. Names and
/// paths only; a file that is a symlink or too large is not read.
pub fn dotenv_conflicts(config: &Config, worktree: &Path, env: &ChildEnv) -> Vec<DotenvConflict> {
    dotenv_conflicts_among(config, worktree, env, &[])
}

/// [`dotenv_conflicts`] that also looks at `copied`: the paths the payload
/// phase recorded, which include the files `.worktreeinclude` selected.
pub fn dotenv_conflicts_among(
    config: &Config,
    worktree: &Path,
    env: &ChildEnv,
    copied: &[String],
) -> Vec<DotenvConflict> {
    let mut found = Vec::new();
    let mut entries: Vec<&String> = config.worktree.copy.iter().collect();
    for entry in copied {
        if !entries.contains(&entry) {
            entries.push(entry);
        }
    }
    for entry in entries.into_iter().filter(|e| is_dotenv(e)) {
        let path = worktree.join(entry);
        let Ok(meta) = path.symlink_metadata() else {
            continue;
        };
        if !meta.file_type().is_file() || meta.len() > MAX_SOURCE_BYTES {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut names: Vec<String> = dotenv_vars(&text)
            .into_iter()
            .filter(|(name, value)| {
                env.owned.contains(name)
                    && env
                        .vars
                        .iter()
                        .find(|(key, _)| key == name)
                        .is_none_or(|(_, ours)| ours != value)
            })
            .map(|(name, _)| name)
            .collect();
        names.sort();
        names.dedup();
        if !names.is_empty() {
            found.push(DotenvConflict {
                file: entry.clone(),
                names,
            });
        }
    }
    found
}

/// The sentence setup and `doctor` say for a conflict. Names the file and the
/// variables, never a value.
pub fn describe_conflict(conflict: &DotenvConflict) -> String {
    format!(
        "{} sets {} to another value than the workspace's .devflow/env: tools that read the file \
         themselves use it, not the workspace's",
        conflict.file,
        conflict.names.join(", ")
    )
}

/// `NAME=value` lines of a dotenv file: an optional `export `, one pair of
/// matching quotes around the value, `#` comments, and an unquoted value
/// cut at ` #`. Enough to compare values, not a full dotenv implementation.
fn dotenv_vars(text: &str) -> Vec<(String, String)> {
    let mut vars = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").map_or(line, str::trim_start);
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if !valid_env_name(name) {
            continue;
        }
        let value = value.trim();
        let value = match value.chars().next() {
            Some(quote @ ('"' | '\'')) => value[1..]
                .find(quote)
                .map_or(&value[1..], |end| &value[1..=end]),
            _ => value.split(" #").next().unwrap_or(value).trim_end(),
        };
        vars.push((name.to_string(), value.to_string()));
    }
    vars
}

#[cfg(test)]
mod tests;
