//! The `isolation` and `dotenv` checks: does the current workspace get its own
//! variables, and only those?
//!
//! `isolation` compares the process environment with the current workspace's
//! `.devflow/env` ([`crate::env_owner`]): every name the workspace owns that
//! the environment carries with another value (or that the workspace does not
//! set at all) is a variable another workspace's file, a shell profile or CI
//! put there, and a command started without `devflow exec` would use it. The
//! check names the variables, and the workspace they came from when the
//! environment says so. With a reachable server it also asks the database the
//! workspace's URL reaches (`SELECT current_database()`) and compares it with
//! the one the receipt binds.
//!
//! `dotenv` reports the files `[worktree] copy` carries into the workspace
//! (`.env`, `.env.*`) that set an owned name to another value than
//! `.devflow/env`: the tools that read such a file themselves ignore the
//! workspace's value.
//!
//! Both name variables, databases and files, never values. Silent outside a
//! workspace and in one with no generated file. They judge the workspace
//! containing the directory the command was started in (the one the desktop
//! app shows), against the environment of the process that runs them.
use super::Check;
use crate::env_file;
use crate::env_owner::{self, ChildEnv, Inherited, Leak, Stale};
use crate::project::Project;
use crate::remedy::Remedy;
use crate::setup::{self, WorkspaceStatus};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

/// How long the database question may take in all.
const ASK_TIMEOUT: Duration = Duration::from_secs(6);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Most variable names listed in one line.
const LISTED: usize = 8;

/// What the checks ask of the process they run in, so tests can answer for it.
pub(super) struct Process<'a> {
    /// What the environment says about where it came from.
    pub inherited: Inherited,
    /// The environment's value of a name.
    pub lookup: &'a dyn Fn(&str) -> Option<String>,
    /// Whether the project's PostgreSQL server answers.
    pub reachable: &'a dyn Fn(&Project) -> bool,
    /// The database a URL reaches, or why that could not be asked.
    pub ask: &'a dyn Fn(&str) -> Result<String, String>,
}

/// The checks for the workspace containing `cwd`, against this process's own
/// environment.
pub(super) fn checks(project: &Project, cwd: &Path) -> Vec<Check> {
    checks_in(
        project,
        cwd,
        &Process {
            inherited: Inherited::from_process(),
            lookup: &|name| std::env::var(name).ok(),
            reachable: &|project| {
                crate::db::Server::for_project(project)
                    .ok()
                    .flatten()
                    .is_some_and(|server| server.status().reachable)
            },
            ask: &current_database,
        },
    )
}

/// The workspace of `list` that contains `cwd`: the deepest checkout, one
/// whose directory exists.
pub(super) fn current_workspace<'a>(
    list: &'a [WorkspaceStatus],
    cwd: &Path,
) -> Option<&'a WorkspaceStatus> {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    list.iter()
        .filter(|item| !item.workspace.missing)
        .filter(|item| cwd.starts_with(&item.workspace.path))
        .max_by_key(|item| item.workspace.path.components().count())
}

pub(super) fn checks_in(project: &Project, cwd: &Path, process: &Process) -> Vec<Check> {
    let Ok(list) = setup::list(project) else {
        return Vec::new();
    };
    let Some(current) = current_workspace(&list, cwd) else {
        return Vec::new();
    };
    let (path, name) = (&current.workspace.path, &current.workspace.name);
    if !env_file::path(path).is_file() {
        return Vec::new();
    }
    let Ok(contents) = env_file::read_contents(path) else {
        return Vec::new();
    };
    if contents.lacks_manifest() {
        return vec![
            Check::warn(
                "isolation",
                format!(
                    "{name}: .devflow/env was written by an earlier devflow and does not list the \
                     names the workspace owns, so it is not compared and nothing runs from it"
                ),
                "setting the workspace up again writes the list",
            )
            .fix(Remedy::Setup {
                workspace: path.to_path_buf(),
            }),
        ];
    }
    let owned: BTreeSet<String> = contents
        .declared
        .unwrap_or_default()
        .into_iter()
        .chain(contents.former)
        .collect();
    // A source that cannot be resolved is reported, not fatal here: the
    // comparison goes on with the workspace's own names.
    let (env, stale) = match ChildEnv::resolve(contents.vars.clone(), &owned, &process.inherited) {
        Ok(env) => (env, None),
        Err(stale) => (ChildEnv::standalone(contents.vars, &owned), Some(stale)),
    };
    let leak = env_owner::leaks(&env, path, process.lookup, &process.inherited);
    let database = bound_database(project, current, &env, process);
    let mut checks = vec![isolation(name, path, &env, &leak, stale, database)];
    checks.extend(dotenv(project, name, path, &env));
    checks
}

fn listed(names: &[String]) -> String {
    let shown = names.iter().take(LISTED).cloned().collect::<Vec<_>>();
    let more = names.len().saturating_sub(LISTED);
    if more == 0 {
        shown.join(", ")
    } else {
        format!("{} and {more} more", shown.join(", "))
    }
}

/// The `isolation` check: the worst of the environment comparison and the
/// database question.
fn isolation(
    workspace: &str,
    path: &Path,
    env: &ChildEnv,
    leak: &Leak,
    stale: Option<Stale>,
    database: Option<Result<String, String>>,
) -> Check {
    let mut said = if leak.names.is_empty() {
        format!(
            "no inherited variable differs from .devflow/env ({} owned)",
            env.owned().len()
        )
    } else {
        let origin = match &leak.from {
            Some((from, Some(name))) => format!(
                "; the environment comes from workspace {name} ({})",
                from.display()
            ),
            Some((from, None)) => format!("; the environment comes from {}", from.display()),
            None => String::new(),
        };
        format!(
            "{} in the process environment {} from .devflow/env{origin}",
            listed(&leak.names),
            if leak.names.len() == 1 {
                "differs"
            } else {
                "differ"
            }
        )
    };
    if let Some(stale) = &stale {
        said = format!("{stale}; {said}");
    }
    let mut check = if stale.is_some() {
        Check::warn(
            "isolation",
            String::new(),
            "commands devflow starts refuse to run while the environment carries the variables of \
             a workspace that cannot be resolved",
        )
        .fix(Remedy::ClearInherited)
    } else if leak.names.is_empty() {
        Check::ok("isolation", String::new())
    } else {
        Check::warn(
            "isolation",
            String::new(),
            "commands devflow starts (hooks, daemons and its exec wrapper) get the workspace's \
             own values; a shell that loaded another workspace's .devflow/env (mise, direnv) \
             has to be reloaded in this directory, or the commands run through the exec wrapper",
        )
        .fix(Remedy::Reveal {
            path: env_file::path(path),
        })
    };
    match database {
        Some(Ok(sentence)) => said = format!("{said}; {sentence}"),
        Some(Err(problem)) => {
            said = format!("{problem}; {said}");
            check = Check::fail(
                "isolation",
                String::new(),
                "the workspace's .devflow/env points at another database than the one it owns; \
                 setting the workspace up again writes its own",
            )
            .fix(Remedy::Setup {
                workspace: path.to_path_buf(),
            })
            .fix(Remedy::Reveal {
                path: env_file::path(path),
            });
        }
        None => {}
    }
    check.detail = format!("{workspace}: {said}");
    check
}

/// What the workspace's URL reaches, against what the receipt binds: `Ok`
/// with a sentence when it matches or could not be asked, `Err` with the
/// problem when it reaches another database. `None` without a branch the
/// project's reachable server provisioned.
fn bound_database(
    project: &Project,
    current: &WorkspaceStatus,
    env: &ChildEnv,
    process: &Process,
) -> Option<Result<String, String>> {
    let receipt = current.receipt.as_ref()?;
    let (_, binding) = receipt.postgres(&project.config)?;
    // Only a branch devflow's own server provisioned: nothing is connected to
    // that devflow did not make.
    binding.branch_id?;
    if binding.database.is_empty() {
        return None;
    }
    let url = env
        .vars()
        .iter()
        .find(|(name, _)| name == "DATABASE_URL")
        .map(|(_, url)| url)?;
    if !(process.reachable)(project) {
        return None;
    }
    Some(match (process.ask)(url) {
        Ok(found) if found == binding.database => Ok(format!(
            "its database answers as {found}, the one the receipt binds"
        )),
        Ok(found) => Err(format!(
            "its URL reaches database {found}, not {} which the receipt binds",
            binding.database
        )),
        Err(why) => Ok(format!(
            "the database it binds ({}) was not asked: {why}",
            binding.database
        )),
    })
}

fn dotenv(project: &Project, workspace: &str, path: &Path, env: &ChildEnv) -> Option<Check> {
    let conflicts = env_owner::dotenv_conflicts(&project.config, path, env);
    if conflicts.is_empty() {
        return None;
    }
    let mut check = Check::warn(
        "dotenv",
        format!(
            "{workspace}: {}",
            conflicts
                .iter()
                .map(env_owner::describe_conflict)
                .collect::<Vec<_>>()
                .join("; ")
        ),
        "remove the variables from the copied file (or drop it from `[worktree] copy`) so that \
         the workspace's own values are the only ones",
    );
    for conflict in &conflicts {
        check = check.fix(Remedy::OpenFile {
            path: path.join(&conflict.file),
        });
    }
    Some(check)
}

/// The name of the database `url` reaches, asked within [`ASK_TIMEOUT`]. Says
/// nothing of the URL when it cannot.
fn current_database(url: &str) -> Result<String, String> {
    let config: postgres::Config = url
        .parse()
        .map_err(|_| "the URL is not a PostgreSQL connection string".to_string())?;
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut config = config;
        config
            .connect_timeout(CONNECT_TIMEOUT)
            .application_name("devflow-doctor");
        let answer = config
            .connect(postgres::NoTls)
            .map_err(|_| "the server did not accept the connection".to_string())
            .and_then(|mut client| {
                client
                    .query_one("SELECT current_database()", &[])
                    .map(|row| row.get::<_, String>(0))
                    .map_err(|_| "the server did not answer the question".to_string())
            });
        let _ = sender.send(answer);
    });
    receiver
        .recv_timeout(ASK_TIMEOUT)
        .unwrap_or_else(|_| Err("the server did not answer in time".to_string()))
}

#[cfg(test)]
mod tests;
