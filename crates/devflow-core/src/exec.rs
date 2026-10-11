//! `devflow exec`: run a command with a workspace's variables applied last.
//!
//! Commands that need the workspace's database go through here. The variables
//! of `.devflow/env` are set on top of the inherited environment, overriding
//! whatever mise, a shell profile or anything else set before, and the command
//! replaces the process (Unix `exec`), so signals, standard streams and the
//! exit status pass straight through. Inside `mise exec -- devflow exec --
//! CMD` mise has already done its work, so what it sets cannot win.
//!
//! The fast path reads two things: it walks up from the directory to the
//! worktree root (the nearest directory with a `.git`) and reads that root's
//! `.devflow/env`. Nothing scans the project, the registry or the server.
//!
//! It never runs a command against an unknown database: no worktree, no
//! `.devflow/env` (setup has not written it), or one that is not a plain file
//! is an error, and the command does not start. Errors and this module's
//! output never include a value.
use crate::remedy::{Problem, Remedy};
use anyhow::{Result, bail};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A workspace whose variables a command will get.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// The worktree root that owns the variables.
    pub root: PathBuf,
    vars: Vec<(String, String)>,
}

impl Prepared {
    /// The names of the variables the command will get.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.vars.iter().map(|(name, _)| name.as_str())
    }

    /// Set the variables on `command`, on top of the environment it inherits.
    ///
    /// As for hooks, a workspace with a database also loses every inherited
    /// libpq variable that selects or authenticates a connection before the
    /// generated ones are set: an inherited `PGSERVICE` or `PGSSLROOTCERT`
    /// would otherwise redirect a `psql` that gets only a `DATABASE_URL`.
    pub fn apply(&self, command: &mut Command) {
        if self.vars.iter().any(|(name, _)| name == "DATABASE_URL") {
            for var in crate::conn::libpq_env_vars() {
                command.env_remove(var);
            }
        }
        command.envs(self.vars.iter().map(|(name, value)| (name, value)));
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
    let mut dir = Some(start);
    while let Some(candidate) = dir {
        if candidate.join(".git").symlink_metadata().is_ok() {
            return prepare_root(candidate);
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
    let vars = match crate::env_file::read(root) {
        Ok(vars) => vars,
        Err(_) => bail!("cannot read {}", path.display()),
    };
    if vars.is_empty() {
        return Err(Problem::new(format!(
            "{} sets no variables, so the command was not started",
            path.display()
        ))
        .with(Remedy::Setup {
            workspace: root.to_path_buf(),
        })
        .into_error());
    }
    Ok(Prepared {
        root: root.to_path_buf(),
        vars,
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

    /// A directory with a `.git` and a `.devflow/env`.
    fn workspace(dir: &Path) -> PathBuf {
        let root = dir.join("ws");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        crate::env_file::write(&root, &[("DATABASE_URL".to_string(), DB.to_string())]).unwrap();
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
        crate::env_file::write(&linked, &[("A".to_string(), "1".to_string())]).unwrap();
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
