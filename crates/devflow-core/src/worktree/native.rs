//! The native checkout: a worktree made by Git itself.
//!
//! `[worktree] checkout = "git"` in `devflow.local.toml` hands the last step of
//! a worktree's creation to the `git` on `PATH`. devflow still decides
//! everything that is its own: it resolves the starting point (`--base`,
//! `--from`, the remote branch, the default workspace), creates the branch with
//! the upstream it should track, and only then runs `git worktree add` with
//! structured arguments. Git then checks the files out the way it always does,
//! which is the point: Git LFS and other filters, partial clones, hooks.
//!
//! What this authorizes is the person's, not the repository's, so the choice is
//! machine-local ([`crate::config::CheckoutBackend`]) and says so wherever it
//! is reported. The run is contained as far as it can be without defeating its
//! purpose:
//!
//! - no `GIT_*` from the caller (a Git hook that started devflow sets
//!   `GIT_DIR` and `GIT_INDEX_FILE`, which would point this Git at the wrong
//!   repository), and `GIT_TERMINAL_PROMPT=0`: nothing waits for a terminal;
//! - none of the variables of the workspace devflow was started from: Git runs
//!   filters and hooks, which would otherwise see another workspace's
//!   `DATABASE_URL` ([`crate::env_owner`] decides which names those are). When
//!   the environment names a workspace that cannot be resolved, which names
//!   are its own is not known, and the run is refused before anything is
//!   created, as `devflow exec` refuses ([`Stale`], with the remedy that goes
//!   on anyway: `DEVFLOW_CLEAR_INHERITED`, which also clears every name devflow
//!   can generate);
//! - `DEVFLOW_SETUP_ACTIVE=1`, so devflow's own `post-checkout` hook, which Git
//!   runs after the checkout, does not set the worktree up a second time: the
//!   setup that follows is the one the creation runs;
//! - a deadline, with the process group ended when it passes.
//!
//! A failure is never retried with libgit2: whatever Git left behind is the
//! caller's to report ([`Failure`]).
use crate::env_owner::{self, ChildEnv, Inherited, Stale};
use crate::project::Project;
use crate::remedy::{Problem, Remedy};
use crate::subprocess::{self, Limits};
use crate::tools::{self, Tool};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// How long `git worktree add` may take, filters and hooks included. A large
/// LFS checkout downloads its files here, so the bound is generous; past it
/// the process group is ended and what was written stays where it is.
const DEADLINE: Duration = Duration::from_secs(30 * 60);
/// How long `git --version` may take.
const VERSION_DEADLINE: Duration = Duration::from_secs(10);
/// Where Git is downloaded from, for the person who has none or too old a one.
pub const DOWNLOAD_URL: &str = Tool::Git.upgrade_page();

/// The `git` executable the native checkout runs.
#[derive(Debug, Clone)]
pub struct Git {
    program: PathBuf,
    /// What `git --version` printed (`git version 2.50.1 (Apple Git-155)`).
    pub version: String,
    /// The workspace variables Git does not inherit, settled once when Git
    /// was located (before anything was created) and applied to every run.
    env: ChildEnv,
}

/// Why Git's `worktree add` did not succeed.
#[derive(Debug, Clone)]
pub struct Failure {
    /// The end of what Git printed, credentials masked.
    pub message: String,
    pub timed_out: bool,
}

impl Git {
    /// Find Git on `PATH` and check that it runs. A typed problem, before any
    /// effect, when it does not: the choice of the native checkout is
    /// explicit, so a missing Git is never answered with another backend.
    pub fn locate(project: &Project) -> Result<Git> {
        Git::locate_in(project, std::env::var_os("PATH").as_deref())
    }

    /// [`Git::locate`] over the directories of `path`.
    fn locate_in(project: &Project, path: Option<&std::ffi::OsStr>) -> Result<Git> {
        Git::locate_with(project, path, Inherited::from_process())
    }

    /// [`Git::locate_in`] for a process that inherited `inherited`.
    fn locate_with(
        project: &Project,
        path: Option<&std::ffi::OsStr>,
        inherited: Inherited,
    ) -> Result<Git> {
        let local = project.root.join(crate::config::LOCAL_CONFIG_FILE);
        let way_on = |problem: Problem| {
            problem
                .with(Remedy::OpenGuide {
                    url: DOWNLOAD_URL.to_string(),
                })
                .with(Remedy::OpenFile {
                    path: local.clone(),
                })
                .with(Remedy::Retry)
                .into_error()
        };
        let Some(program) = find_on_path("git", path) else {
            return Err(way_on(Problem::new(format!(
                "{} sets worktree.checkout to \"git\", so new worktrees are checked out by Git \
                 itself, but there is no git on PATH. Install Git, or remove the key to let \
                 devflow check out with libgit2 (which refuses Git LFS and partial clones)",
                crate::config::LOCAL_CONFIG_FILE
            ))));
        };
        // Before Git runs for any reason, and before the caller creates
        // anything: an environment of a workspace that cannot be resolved
        // could carry that workspace's variables into Git's filters and hooks.
        let env = environment(project, inherited).map_err(Stale::into_error)?;
        let mut command = Command::new(&program);
        command.arg("--version");
        scrub(&mut command, &env);
        let version = subprocess::run(command, VERSION_DEADLINE)
            .map_err(|e| anyhow::anyhow!("cannot run {}: {e}", program.display()))
            .and_then(|out| {
                if out.ok && out.stdout.trim_start().starts_with("git version") {
                    Ok(out.stdout.trim().to_string())
                } else {
                    Err(anyhow::anyhow!(
                        "{} did not report a version{}",
                        program.display(),
                        out.message
                            .map(|m| format!(": {}", crate::redact::text(&m)))
                            .unwrap_or_default()
                    ))
                }
            });
        match version {
            // The pin is the release of the options the checkout passes.
            Ok(version) => match tools::judge(Tool::Git, &version) {
                Ok(_) => Ok(Git {
                    program,
                    version,
                    env,
                }),
                Err(problem) => Err(way_on(Problem::new(format!(
                    "{} sets worktree.checkout to \"git\", but {}. Upgrade Git, or remove the \
                     key to let devflow check out with libgit2",
                    crate::config::LOCAL_CONFIG_FILE,
                    problem.message()
                )))),
            },
            Err(error) => Err(way_on(Problem::new(format!(
                "{} sets worktree.checkout to \"git\", but the git on PATH does not run ({error}). \
                 Fix the installation, or remove the key to let devflow check out with libgit2",
                crate::config::LOCAL_CONFIG_FILE
            )))),
        }
    }

    /// `git worktree add -- PATH BRANCH` in the main checkout: the existing
    /// local `branch` checked out at the new `path`.
    pub fn add_worktree(
        &self,
        project: &Project,
        path: &Path,
        branch: &str,
    ) -> Result<(), Failure> {
        let mut command = Command::new(&self.program);
        command
            .current_dir(&project.root)
            .args(["worktree", "add", "--quiet", "--"])
            .arg(path)
            .arg(branch);
        scrub(&mut command, &self.env);
        // Hooks and helpers may start something that outlives Git on purpose
        // (a file system monitor): not ours to end.
        let limits = Limits::new(DEADLINE).leaving();
        match subprocess::run_bounded(command, None, limits) {
            Ok(out) if out.ok => Ok(()),
            Ok(out) => Err(Failure {
                message: crate::redact::text(&out.message.unwrap_or_else(|| match out.code {
                    Some(code) => format!("git exited with status {code}"),
                    None => "git ended without a status".to_string(),
                })),
                timed_out: out.timed_out,
            }),
            Err(error) => Err(Failure {
                message: crate::redact::text(&format!("cannot run git: {error}")),
                timed_out: false,
            }),
        }
    }
}

/// The workspace variables a native Git must not inherit: the target's names,
/// the names of the workspace the environment came from (its manifest, so a
/// variable only that workspace's recipes set is included) and nothing kept on
/// request (`DEVFLOW_INHERIT` is for commands a workspace runs, not for Git's
/// filters). A workspace the environment names and that cannot be resolved
/// is [`Stale`] unless clearing was asked for (`DEVFLOW_CLEAR_INHERITED`).
fn environment(project: &Project, mut inherited: Inherited) -> Result<ChildEnv, Stale> {
    inherited.keep.clear();
    let names = env_owner::declared_names(&project.config);
    ChildEnv::resolve(Vec::new(), &names, &inherited)
}

/// The environment a native Git runs with: see the module documentation.
fn scrub(command: &mut Command, env: &ChildEnv) {
    let present: Vec<std::ffi::OsString> = std::env::vars_os().map(|(name, _)| name).collect();
    scrub_from(command, env, &present);
}

/// [`scrub`] for a process that has the variables called `present`.
fn scrub_from(command: &mut Command, env: &ChildEnv, present: &[std::ffi::OsString]) {
    env.apply(command);
    for name in present {
        if name.to_string_lossy().starts_with("GIT_") {
            command.env_remove(name);
        }
    }
    command.env("GIT_TERMINAL_PROMPT", "0");
    command.env("DEVFLOW_SETUP_ACTIVE", "1");
}

/// The first executable called `name` in a directory of `paths` (a `PATH`).
fn find_on_path(name: &str, paths: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    std::env::split_paths(paths?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::testutil::repo;
    use std::ffi::{OsStr, OsString};

    fn envs(command: &Command) -> Vec<(String, Option<String>)> {
        command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    fn removed(command: &Command, name: &str) -> bool {
        envs(command).iter().any(|(n, v)| n == name && v.is_none())
    }

    #[test]
    fn git_is_not_found_in_relative_or_empty_path_entries() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let program = dir.path().join("git");
            std::fs::write(&program, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
            let absolute = std::env::join_paths([dir.path()]).unwrap();
            assert_eq!(find_on_path("git", Some(&absolute)), Some(program.clone()));
            // `.` and the empty entry would run whatever the working directory holds.
            for relative in [".", "", "bin"] {
                assert!(find_on_path("git", Some(OsStr::new(relative))).is_none());
            }
            // Not executable: not found.
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(find_on_path("git", Some(&absolute)).is_none());
            assert!(find_on_path("git", None).is_none());
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_git_older_than_the_pin_is_a_problem_with_ways_on_and_never_runs() {
        use std::os::unix::fs::PermissionsExt;
        let t = repo(None);
        let bin = tempfile::tempdir().unwrap();
        let git = |version: &str| {
            let path = bin.path().join("git");
            // The stand-in records that it was asked for anything but its version.
            std::fs::write(
                &path,
                format!(
                    "#!/bin/sh\n[ \"$1\" = --version ] && {{ echo '{version}'; exit 0; }}\ntouch '{}'\nexit 1\n",
                    bin.path().join("was-run").display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::env::join_paths([bin.path()]).unwrap()
        };
        for printed in ["git version 2.18.0", "git version garbled"] {
            let path = git(printed);
            let error = Git::locate_in(&t.project, Some(&path)).unwrap_err();
            let problem = error.downcast_ref::<Problem>().expect("typed");
            assert!(problem.message.contains("worktree.checkout"), "{problem:?}");
            assert!(
                problem.message.contains("2.19.0"),
                "names the pin: {}",
                problem.message
            );
            assert!(
                !problem.message.contains("git worktree"),
                "no command line: {}",
                problem.message
            );
            assert!(
                problem
                    .remedies
                    .iter()
                    .any(|r| matches!(r, Remedy::OpenGuide { url } if url == DOWNLOAD_URL)),
                "{problem:?}"
            );
            assert!(problem.remedies.contains(&Remedy::Retry));
        }
        assert!(!bin.path().join("was-run").exists());
        // The pin itself is supported.
        let path = git("git version 2.19.0");
        assert!(Git::locate_in(&t.project, Some(&path)).is_ok());
    }

    #[test]
    fn a_missing_git_is_a_problem_with_ways_on_and_no_other_backend() {
        let t = repo(None);
        let empty = tempfile::tempdir().unwrap();
        let path = std::env::join_paths([empty.path()]).unwrap();
        let error = Git::locate_in(&t.project, Some(&path)).unwrap_err();
        let problem = error.downcast_ref::<Problem>().expect("typed");
        assert!(problem.message.contains("worktree.checkout"), "{problem:?}");
        assert!(problem.message.contains("no git on PATH"), "{problem:?}");
        assert!(
            problem
                .remedies
                .iter()
                .any(|r| matches!(r, Remedy::OpenGuide { url } if url == DOWNLOAD_URL)),
            "{problem:?}"
        );
        assert!(
            problem.remedies.iter().any(
                |r| matches!(r, Remedy::OpenFile { path } if path.ends_with("devflow.local.toml"))
            ),
            "{problem:?}"
        );
        assert!(problem.remedies.contains(&Remedy::Retry));
        assert!(
            !problem.message.contains("git worktree"),
            "no command line: {}",
            problem.message
        );
    }

    /// A caller that was started from a workspace that is gone.
    fn from_a_gone_workspace() -> Inherited {
        Inherited {
            workspace_path: Some(PathBuf::from("/gone/elsewhere")),
            workspace_id: Some("not-a-real-id".into()),
            workspace_name: Some("elsewhere".into()),
            keep: ["DATABASE_URL".to_string()].into_iter().collect(),
            clear_stale: false,
        }
    }

    fn scrubbed(env: &ChildEnv, present: &[&str]) -> Command {
        let present: Vec<OsString> = present.iter().map(OsString::from).collect();
        let mut command = Command::new("git");
        scrub_from(&mut command, env, &present);
        command
    }

    #[test]
    fn git_runs_without_the_callers_git_variables_or_workspace_when_clearing_was_asked_for() {
        let t = repo(None);
        // Clearing is the explicit override of an environment that names a
        // workspace that cannot be resolved.
        let inherited = Inherited {
            clear_stale: true,
            ..from_a_gone_workspace()
        };
        let env = environment(&t.project, inherited).unwrap();
        let command = scrubbed(
            &env,
            &[
                "GIT_DIR",
                "GIT_INDEX_FILE",
                "GIT_WORK_TREE",
                "GIT_SSH_COMMAND",
                "PATH",
                "HOME",
            ],
        );
        for name in [
            "GIT_DIR",
            "GIT_INDEX_FILE",
            "GIT_WORK_TREE",
            "GIT_SSH_COMMAND",
        ] {
            assert!(removed(&command, name), "{name} reaches Git");
        }
        // What a process needs to find programs and its home is left alone.
        for name in ["PATH", "HOME"] {
            assert!(
                !envs(&command).iter().any(|(n, _)| n == name),
                "{name} was touched"
            );
        }
        // Another workspace's variables are taken out, and `DEVFLOW_INHERIT`
        // does not keep them for Git.
        for name in [
            "DEVFLOW_WORKSPACE",
            "DEVFLOW_WORKSPACE_PATH",
            "DEVFLOW_PROJECT",
            "DATABASE_URL",
        ] {
            assert!(removed(&command, name), "{name} reaches Git");
        }
        let set: std::collections::BTreeMap<_, _> = envs(&command)
            .into_iter()
            .filter_map(|(n, v)| Some((n, v?)))
            .collect();
        assert_eq!(
            set.get("GIT_TERMINAL_PROMPT").map(String::as_str),
            Some("0")
        );
        assert_eq!(
            set.get("DEVFLOW_SETUP_ACTIVE").map(String::as_str),
            Some("1")
        );
        // Execution controls are not workspace variables.
        assert!(!removed(&command, "DEVFLOW_TRUST_HOOKS"));
        assert!(!removed(&command, "DEVFLOW_INHERIT"));
    }

    #[test]
    fn an_environment_of_a_workspace_that_cannot_be_resolved_refuses_before_anything_runs() {
        let t = repo(None);
        // The environment names a workspace, and nothing proves which
        // variables are its own: without the override that is a typed problem
        // with the remedy that goes on anyway, and no environment to run with.
        let stale = environment(&t.project, from_a_gone_workspace()).unwrap_err();
        assert!(stale.to_string().contains("elsewhere"), "{stale}");
        let error = stale.into_error();
        assert_eq!(
            crate::remedy::remedies_of(&error),
            [Remedy::ClearInherited],
            "{error:#}"
        );

        // So is locating Git: this happens before the branch exists, with
        // `git` found and not run.
        let Some(git) = std::env::var_os("PATH").and_then(|p| find_on_path("git", Some(&p))) else {
            eprintln!("skipped: no git on PATH");
            return;
        };
        let only = std::env::join_paths([git.parent().unwrap()]).unwrap();
        let error = Git::locate_with(&t.project, Some(&only), from_a_gone_workspace()).unwrap_err();
        assert!(
            error.downcast_ref::<Problem>().is_some(),
            "typed: {error:#}"
        );
        assert!(
            crate::remedy::remedies_of(&error).contains(&Remedy::ClearInherited),
            "{error:#}"
        );
        // The override locates it, and the environment it settled is the one
        // every Git run applies.
        let cleared = Inherited {
            clear_stale: true,
            ..from_a_gone_workspace()
        };
        assert!(Git::locate_with(&t.project, Some(&only), cleared).is_ok());
        // Nothing to resolve: nothing to refuse.
        assert!(Git::locate_with(&t.project, Some(&only), Inherited::default()).is_ok());
    }

    #[test]
    fn a_workspace_that_resolves_has_every_name_it_set_taken_out_not_only_devflows() {
        let t = repo(None);
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        // A workspace whose recipes set a name only it knows.
        crate::env_file::write(
            &root,
            &[
                ("DEVFLOW_WORKSPACE_ID".to_string(), "abc".to_string()),
                ("STRIPE_SECRET".to_string(), "not-printed".to_string()),
            ],
        )
        .unwrap();
        let inherited = Inherited {
            workspace_path: Some(root),
            workspace_id: Some("abc".into()),
            workspace_name: Some("other".into()),
            keep: ["STRIPE_SECRET".to_string()].into_iter().collect(),
            clear_stale: false,
        };
        let env = environment(&t.project, inherited).unwrap();
        let command = scrubbed(&env, &[]);
        assert!(
            removed(&command, "STRIPE_SECRET"),
            "its own name reaches Git"
        );
        assert!(removed(&command, "DEVFLOW_WORKSPACE_ID"));
        // Names nothing says are a workspace's are not touched.
        assert!(!removed(&command, "EDITOR"));
        assert!(!envs(&command).iter().any(|(n, _)| n == "EDITOR"));
    }
}
