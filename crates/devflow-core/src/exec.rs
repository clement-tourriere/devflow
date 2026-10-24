//! `devflow exec`: run a command with a workspace's variables applied last.
//!
//! Commands that need the workspace's database go through here. The variables
//! of `.devflow/env` are set on top of the inherited environment, overriding
//! whatever mise, a shell profile or anything else set before, and every other
//! name the workspace owns (declared but not set here, or set by the workspace
//! the caller came from) is taken out of it. The command replaces the process
//! (Unix `exec`), so signals, standard streams and the exit status pass
//! straight through. Inside `mise exec -- devflow exec -- CMD` mise has
//! already done its work, so what it sets cannot win.
//!
//! The fast path reads two things: it walks up from the directory to the
//! worktree root (the nearest directory with a `.git`) and reads that root's
//! `.devflow/env`, which also lists every name the workspace owns. When the
//! inherited environment names the workspace it came from, that workspace's
//! file is read too ([`crate::env_owner`]). Nothing scans the project, the
//! registry or the server.
//!
//! It never runs a command against an unknown database: no worktree, no
//! `.devflow/env` (setup has not written it), or one that is not a plain file
//! is an error, and the command does not start. Errors and this module's
//! output never include a value.
use crate::env_owner::{ChildEnv, Inherited};
use crate::remedy::{Problem, Remedy};
use anyhow::{Result, bail};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

/// What the caller asked for beyond the workspace's own rules.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// Keep the caller's value of these names unless the workspace sets one
    /// (`--inherit`; `DEVFLOW_INHERIT` says the same).
    pub inherit: Vec<String>,
    /// Go on although the environment names a workspace that cannot be
    /// resolved (`--clear-inherited`; `DEVFLOW_CLEAR_INHERITED` says the same).
    pub clear_stale: bool,
}

impl Options {
    fn inherited(&self) -> Inherited {
        let inherited = Inherited::from_process().keeping(self.inherit.iter().cloned());
        if self.clear_stale {
            inherited.clearing_stale()
        } else {
            inherited
        }
    }
}

/// A workspace whose variables a command will get.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// The worktree root that owns the variables.
    pub root: PathBuf,
    env: ChildEnv,
}

impl Prepared {
    /// The names of the variables the command will get.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.env.vars().iter().map(|(name, _)| name.as_str())
    }

    /// The workspace's environment: its variables and what is taken out of
    /// the caller's.
    pub fn env(&self) -> &ChildEnv {
        &self.env
    }

    /// Set the variables on `command`, on top of the environment it inherits.
    ///
    /// Every name the workspace owns ([`crate::env_owner`]) is taken out of
    /// the inherited environment first, so a name this workspace does not set
    /// (no database, a removed recipe) does not keep the caller's value, and
    /// an inherited `PGSERVICE` or `PGSSLROOTCERT` cannot redirect a `psql`
    /// that gets only a `DATABASE_URL`.
    pub fn apply(&self, command: &mut Command) {
        self.env.apply(command);
    }
}

/// The workspace containing `start`, found by walking up to the worktree root.
///
/// The root is the nearest directory that has a `.git` (a directory in a main
/// checkout, a file in a linked worktree). Its `.devflow/env` must exist:
/// looking further up could hand the command the variables of another
/// checkout (a worktree set up inside the main one, say) while this one's own
/// are not written yet.
pub fn prepare(start: &Path) -> Result<Prepared> {
    prepare_with(start, &Options::default())
}

/// [`prepare`] with the caller's [`Options`].
pub fn prepare_with(start: &Path, options: &Options) -> Result<Prepared> {
    let mut dir = Some(start);
    while let Some(candidate) = dir {
        if candidate.join(".git").symlink_metadata().is_ok() {
            return prepare_root_with(candidate, options);
        }
        dir = candidate.parent();
    }
    bail!(
        "{} is not inside a Git checkout, so it is not inside a workspace; run the command in \
         one, or name a workspace with --workspace",
        start.display()
    )
}

/// The workspace at `root` has no `.devflow/env`: setup has not run there.
fn no_variables(root: &Path) -> anyhow::Error {
    Problem::new(format!(
        "{} has no {}/{}: this workspace has no variables yet, so the command was not started",
        root.display(),
        crate::env_file::DIR,
        crate::env_file::FILE
    ))
    .with(Remedy::Setup {
        workspace: root.to_path_buf(),
    })
    .into_error()
}

/// The variables of the workspace whose worktree root is `root`.
pub fn prepare_root(root: &Path) -> Result<Prepared> {
    prepare_root_with(root, &Options::default())
}

/// [`prepare_root`] with the caller's [`Options`].
///
/// Refuses (typed, with its remedy) a file written without the list of owned
/// names, and an inherited environment that names a workspace that cannot be
/// resolved, unless [`Options::clear_stale`] says to go on.
pub fn prepare_root_with(root: &Path, options: &Options) -> Result<Prepared> {
    let dir = root.join(crate::env_file::DIR);
    let path = crate::env_file::path(root);
    // The file holds credentials and decides which database a command gets: a
    // link could point at another checkout's.
    match dir.symlink_metadata() {
        Ok(meta) if meta.file_type().is_dir() => {}
        Ok(_) => bail!(
            "{} is a symlink or not a directory; refusing to take the workspace's variables \
             through it",
            dir.display()
        ),
        Err(_) => return Err(no_variables(root)),
    }
    match path.symlink_metadata() {
        Ok(meta) if meta.file_type().is_file() => {}
        Ok(_) => bail!(
            "{} is a symlink or not a regular file; refusing to take the workspace's variables \
             through it",
            path.display()
        ),
        Err(_) => return Err(no_variables(root)),
    }
    let contents = match crate::env_file::read_contents(root) {
        Ok(contents) => contents,
        Err(_) => bail!("cannot read {}", path.display()),
    };
    if contents.vars.is_empty() {
        return Err(Problem::new(format!(
            "{} sets no variables, so the command was not started",
            path.display()
        ))
        .with(Remedy::Setup {
            workspace: root.to_path_buf(),
        })
        .into_error());
    }
    let env = ChildEnv::from_contents_in(contents, &options.inherited())
        .map_err(|why| why.into_error(root))?;
    Ok(Prepared {
        root: root.to_path_buf(),
        env,
    })
}

/// What happened when the command could not be started.
#[derive(Debug)]
pub struct NotStarted {
    /// `env(1)`'s convention: 127 for a program that is not found, 126 for
    /// one that cannot be run.
    pub code: u8,
    pub reason: String,
}

/// Run `program` with `args` and the workspace's variables.
///
/// On Unix this replaces the process and only returns when the program could
/// not be started. Elsewhere the program runs as a child and its exit status
/// is returned as the error-free `Ok`.
pub fn run(
    prepared: &Prepared,
    program: &OsStr,
    args: &[std::ffi::OsString],
    dir: Option<&Path>,
) -> std::result::Result<u8, NotStarted> {
    let mut command = Command::new(program);
    command.args(args);
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    prepared.apply(&mut command);
    start(command)
}

#[cfg(unix)]
fn start(mut command: Command) -> std::result::Result<u8, NotStarted> {
    use std::os::unix::process::CommandExt;
    // `exec` only returns on failure.
    let error = command.exec();
    Err(not_started(&command, &error))
}

#[cfg(not(unix))]
fn start(mut command: Command) -> std::result::Result<u8, NotStarted> {
    match command.status() {
        Ok(status) => Ok(status.code().map_or(1, |code| code.clamp(0, 255) as u8)),
        Err(error) => Err(not_started(&command, &error)),
    }
}

fn not_started(command: &Command, error: &std::io::Error) -> NotStarted {
    let code = match error.kind() {
        std::io::ErrorKind::NotFound => 127,
        _ => 126,
    };
    let reason = match error.kind() {
        std::io::ErrorKind::NotFound => "no such program (is it on PATH?)".to_string(),
        std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
        _ => error.to_string(),
    };
    NotStarted {
        code,
        reason: format!(
            "cannot run `{}`: {reason}",
            crate::redact::text(&command.get_program().to_string_lossy())
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DB: &str = "postgres://own:own-password-8@localhost/own"; // kingfisher:ignore

    /// `.devflow/env` as setup writes it: the variables, and the names a
    /// workspace with those variables owns.
    fn write_env(root: &Path, vars: &[(&str, &str)]) {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let declared = crate::env_owner::declared_for(&crate::Config::default(), &vars);
        crate::env_file::write_owned(root, &vars, &declared, &Default::default()).unwrap();
    }

    /// A directory with a `.git` and a `.devflow/env`.
    fn workspace(dir: &Path) -> PathBuf {
        let root = dir.join("ws");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        write_env(&root, &[("DATABASE_URL", DB)]);
        root
    }

    #[test]
    fn a_workspace_without_variables_says_so_and_offers_setup() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let error = prepare_root(&root).unwrap_err();
        assert!(!error.to_string().contains("`devflow"), "{error}");
        assert!(error.to_string().contains("no variables yet"), "{error}");
        assert_eq!(
            crate::remedy::remedies_of(&error),
            [Remedy::Setup {
                workspace: root.clone()
            }]
        );
        // An empty file is the same story.
        std::fs::create_dir_all(root.join(crate::env_file::DIR)).unwrap();
        std::fs::write(crate::env_file::path(&root), "").unwrap();
        let error = prepare_root(&root).unwrap_err();
        assert!(!error.to_string().contains("`devflow"), "{error}");
        assert_eq!(crate::remedy::remedies_of(&error).len(), 1);
    }

    #[test]
    fn the_workspace_is_found_from_any_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        let root = workspace(dir.path());
        let deep = root.join("apps/web/src");
        std::fs::create_dir_all(&deep).unwrap();
        for start in [&root, &root.join("apps"), &deep] {
            let prepared = prepare(start).unwrap();
            assert_eq!(prepared.root, root);
            assert_eq!(prepared.names().collect::<Vec<_>>(), ["DATABASE_URL"]);
        }
        // A linked worktree has a `.git` file.
        let linked = dir.path().join("linked");
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::write(linked.join(".git"), "gitdir: /elsewhere\n").unwrap();
        write_env(&linked, &[("A", "1")]);
        assert_eq!(prepare(&linked).unwrap().root, linked);
    }

    #[test]
    fn nothing_is_started_outside_a_workspace_or_without_its_variables() {
        let dir = tempfile::tempdir().unwrap();
        // Not in a Git checkout at all (the temporary directory has no .git
        // above it).
        let loose = dir.path().join("loose");
        std::fs::create_dir_all(&loose).unwrap();
        if loose
            .ancestors()
            .all(|a| a.join(".git").symlink_metadata().is_err())
        {
            let error = prepare(&loose).unwrap_err().to_string();
            assert!(error.contains("not inside a Git checkout"), "{error}");
        }
        // A checkout that setup never reached.
        let bare = dir.path().join("bare");
        std::fs::create_dir_all(bare.join(".git")).unwrap();
        let error = prepare(&bare).unwrap_err().to_string();
        assert!(
            error.contains("no .devflow/env") && error.contains("was not started"),
            "{error}"
        );
        // The nearest checkout decides: a worktree set up inside another one
        // never borrows its variables while its own are missing.
        let main = workspace(dir.path());
        let inner = main.join(".worktrees/feature");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join(".git"), "gitdir: /x\n").unwrap();
        let error = prepare(&inner.join("src")).unwrap_err().to_string();
        assert!(error.contains("no .devflow/env"), "{error}");
        assert!(prepare(&main.join("src")).is_ok());
        // An empty file sets nothing.
        std::fs::create_dir_all(bare.join(".devflow")).unwrap();
        std::fs::write(crate::env_file::path(&bare), "# nothing\n").unwrap();
        let error = prepare(&bare).unwrap_err().to_string();
        assert!(error.contains("sets no variables"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn links_are_refused_and_no_error_holds_a_value() {
        let dir = tempfile::tempdir().unwrap();
        let main = workspace(dir.path());
        let other = dir.path().join("other");
        std::fs::create_dir_all(other.join(".git")).unwrap();
        std::fs::create_dir_all(other.join(".devflow")).unwrap();
        std::os::unix::fs::symlink(crate::env_file::path(&main), crate::env_file::path(&other))
            .unwrap();
        let error = prepare(&other).unwrap_err().to_string();
        assert!(error.contains("symlink"), "{error}");
        let linked_dir = dir.path().join("third");
        std::fs::create_dir_all(linked_dir.join(".git")).unwrap();
        std::os::unix::fs::symlink(main.join(".devflow"), linked_dir.join(".devflow")).unwrap();
        let error = prepare(&linked_dir).unwrap_err().to_string();
        assert!(error.contains("symlink"), "{error}");
        for text in [&error, &prepare(&other).unwrap_err().to_string()] {
            assert!(!text.contains("password") && !text.contains(DB), "{text}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_variables_go_on_top_and_inherited_libpq_selectors_are_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let root = workspace(dir.path());
        let prepared = prepare(&root).unwrap();
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "printf '%s|%s' \"$DATABASE_URL\" \"${PGSERVICE-unset}\"",
            ])
            .env("DATABASE_URL", "postgres://main/other")
            .env("PGSERVICE", "main");
        prepared.apply(&mut command);
        let output = command.output().unwrap();
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("{DB}|unset")
        );
    }

    /// What a shell command sees of a few variables, inherited as listed.
    #[cfg(unix)]
    fn seen_by(prepared: &Prepared, inherited: &[(&str, &str)]) -> String {
        let mut command = Command::new("sh");
        command
            .args(["-c", "printf '%s|%s|%s|%s|%s' \"${DATABASE_URL-unset}\" \"${PGHOST-unset}\" \"${REDIS_URL-unset}\" \"${DEVFLOW_SETUP_ACTIVE-unset}\" \"${DEVFLOW_WORKSPACE-unset}\""])
            .env_clear()
            .envs(inherited.iter().copied());
        prepared.apply(&mut command);
        String::from_utf8(command.output().unwrap().stdout).unwrap()
    }

    #[cfg(unix)]
    const INHERITED: [(&str, &str); 5] = [
        ("DATABASE_URL", "postgres://main/other"),
        ("PGHOST", "main"),
        ("REDIS_URL", "redis://main"),
        ("DEVFLOW_SETUP_ACTIVE", "1"),
        ("DEVFLOW_WORKSPACE", "a"),
    ];

    /// A `--no-db` workspace of a project with PostgreSQL and Redis: the file
    /// sets neither, and lists both among the names it owns.
    #[cfg(unix)]
    fn no_db_workspace(dir: &Path, former: &[&str]) -> PathBuf {
        let root = dir.join("ws");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let declared = ["DATABASE_URL", "PGHOST", "REDIS_URL", "DEVFLOW_WORKSPACE"]
            .iter()
            .map(|name| name.to_string())
            .collect();
        let former = former.iter().map(|name| name.to_string()).collect();
        crate::env_file::write_owned(
            &root,
            &[("DEVFLOW_WORKSPACE".to_string(), "b".to_string())],
            &declared,
            &former,
        )
        .unwrap();
        root
    }

    #[cfg(unix)]
    #[test]
    fn a_name_the_workspace_declares_but_does_not_set_is_not_inherited() {
        let dir = tempfile::tempdir().unwrap();
        let root = no_db_workspace(dir.path(), &[]);
        let prepared = prepare(&root).unwrap();
        assert_eq!(seen_by(&prepared, &INHERITED), "unset|unset|unset|1|b");
        // The caller keeps what it names.
        let kept = prepare_with(
            &root,
            &Options {
                inherit: vec!["REDIS_URL".into()],
                ..Options::default()
            },
        )
        .unwrap();
        assert_eq!(seen_by(&kept, &INHERITED), "unset|unset|redis://main|1|b");
    }

    #[cfg(unix)]
    #[test]
    fn a_name_the_workspace_exported_once_is_still_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let root = no_db_workspace(dir.path(), &["OLD_RECIPE"]);
        let prepared = prepare(&root).unwrap();
        let mut command = Command::new("sh");
        command
            .args(["-c", "printf %s \"${OLD_RECIPE-unset}\""])
            .env("OLD_RECIPE", "from a shell");
        prepared.apply(&mut command);
        assert_eq!(
            String::from_utf8(command.output().unwrap().stdout).unwrap(),
            "unset"
        );
    }

    #[test]
    fn a_file_written_by_an_earlier_devflow_is_refused_with_setup_as_the_remedy() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        // The format before the list of owned names was kept.
        crate::env_file::write(&root, &[("DATABASE_URL".to_string(), DB.to_string())]).unwrap();
        let error = prepare_root(&root).unwrap_err();
        assert!(error.to_string().contains("earlier devflow"), "{error}");
        assert!(!error.to_string().contains("`devflow"), "{error}");
        assert!(!error.to_string().contains("own-password"), "{error}");
        assert_eq!(
            crate::remedy::remedies_of(&error),
            [Remedy::Setup {
                workspace: root.clone()
            }]
        );
    }

    #[test]
    fn an_environment_naming_a_workspace_that_is_gone_refuses_until_told_to_go_on() {
        let dir = tempfile::tempdir().unwrap();
        let root = workspace(dir.path());
        let gone = Inherited {
            workspace_path: Some(dir.path().join("removed")),
            workspace_id: Some("an-id".into()),
            workspace_name: Some("removed".into()),
            ..Inherited::default()
        };
        let contents = crate::env_file::read_contents(&root).unwrap();
        let refused = ChildEnv::from_contents_in(contents.clone(), &gone).unwrap_err();
        let error = refused.into_error(&root);
        assert!(error.to_string().contains("removed"), "{error}");
        assert_eq!(crate::remedy::remedies_of(&error), [Remedy::ClearInherited]);
        // The override goes on, and takes out what devflow can generate.
        let env = ChildEnv::from_contents_in(contents, &gone.clearing_stale()).unwrap();
        assert!(env.owned().contains("PGSERVICE"));
        assert!(env.owned().contains("REDIS_URL"));
    }

    #[test]
    fn a_program_that_cannot_start_reports_env_style_codes_without_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let root = workspace(dir.path());
        let prepared = prepare(&root).unwrap();
        let missing = run(
            &prepared,
            OsStr::new("devflow-no-such-program"),
            &["--token=s3cr3t-token-value".into()], // kingfisher:ignore
            None,
        )
        .unwrap_err();
        assert_eq!(missing.code, 127);
        assert!(
            missing.reason.contains("devflow-no-such-program"),
            "{}",
            missing.reason
        );
        assert!(!missing.reason.contains("s3cr3t"), "{}", missing.reason);
        #[cfg(unix)]
        {
            let plain = dir.path().join("not-executable");
            std::fs::write(&plain, "x").unwrap();
            let denied = run(&prepared, plain.as_os_str(), &[], None).unwrap_err();
            assert_eq!(denied.code, 126, "{}", denied.reason);
        }
    }
}
