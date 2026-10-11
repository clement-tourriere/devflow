//! Is a registered project in a state devflow can open, and if not, why and
//! what can be done about it.
//!
//! [`check`] classifies a registry entry by looking at the disk: the folder,
//! the Git repository, `devflow.toml` and the project's identity. It has no
//! side effects (no registration, no migration, no network) and is cheap
//! enough to run for every project in a list. A [`ProjectProblem`] carries a
//! plain-words message and the [`Remedy`] values a frontend can offer; the CLI
//! prints them as hints and the desktop app draws them as buttons.
use crate::config::Config;
use crate::error::{ConfigShape, NotInitialized, ShapeKind};
use crate::registry::Entry;
use crate::remedy::{Remedy, serialize_remedies, shape_remedies};
use serde::Serialize;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// What is wrong with a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProblemKind {
    /// The folder was not found (moved, renamed, deleted, or on a drive that is
    /// not connected), or is not a folder.
    FolderMissing,
    /// The folder is not a Git checkout (any more).
    NotGitCheckout,
    /// The checkout has no `devflow.toml`.
    NotInitialized,
    /// The configuration uses the top-level `[postgres]` table of an earlier
    /// 1.0 build. When [`ProjectProblem::remedies`] has
    /// [`Remedy::MigrateConfig`], devflow can rewrite it.
    PreReleaseConfig,
    /// A devflow 0.x configuration (or a mix of 0.x and 1.0 keys): never
    /// adopted, never rewritten.
    LegacyConfig,
    /// The configuration cannot be read or is not valid.
    InvalidConfig,
    /// The folder holds another devflow project than the one registered.
    WrongProject,
    /// The folder cannot be read for another reason (permissions, a transient
    /// error): trying again may help.
    Unreadable,
}

impl ProblemKind {
    /// A short title for the problem, for a page or a list.
    pub fn title(self) -> &'static str {
        match self {
            ProblemKind::FolderMissing => "This project's folder was not found",
            ProblemKind::NotGitCheckout => "This folder is not a Git repository",
            ProblemKind::NotInitialized => "devflow is not set up here",
            ProblemKind::PreReleaseConfig => "This project's configuration needs updating",
            ProblemKind::LegacyConfig => "This project has a devflow 0.x configuration",
            ProblemKind::InvalidConfig => "This project's configuration has a problem",
            ProblemKind::WrongProject => "A different project is in this folder",
            ProblemKind::Unreadable => "This project cannot be opened",
        }
    }
}

/// A project devflow cannot open as it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectProblem {
    pub kind: ProblemKind,
    /// What is wrong, in plain words (never a command line).
    pub message: String,
    pub root: PathBuf,
    /// What can be done, the most useful first.
    #[serde(serialize_with = "serialize_remedies")]
    pub remedies: Vec<Remedy>,
}

/// The state of a registered project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Ok,
    Problem(ProjectProblem),
}

impl Health {
    pub fn problem(self) -> Option<ProjectProblem> {
        match self {
            Health::Ok => None,
            Health::Problem(problem) => Some(problem),
        }
    }
}

/// Classify a registry entry. Reads the disk, changes nothing.
pub fn check(entry: &Entry) -> Health {
    check_root(&entry.root, Some((entry.id, &entry.name)))
}

/// Classify the project whose main checkout is `root`. `registered` is the id
/// and name it is listed under, when it is: a `devflow.toml` with another id
/// is [`ProblemKind::WrongProject`].
pub fn check_root(root: &Path, registered: Option<(Uuid, &str)>) -> Health {
    let forget = || Remedy::ForgetProject {
        root: root.to_path_buf(),
    };
    let problem = |kind, message: String, mut remedies: Vec<Remedy>| {
        remedies.push(forget());
        Health::Problem(ProjectProblem {
            kind,
            message,
            root: root.to_path_buf(),
            remedies,
        })
    };
    let locate = |remedies: &mut Vec<Remedy>| {
        if let Some((id, _)) = registered.filter(|(id, _)| !id.is_nil()) {
            remedies.push(Remedy::LocateProject {
                id,
                root: root.to_path_buf(),
            });
        }
    };

    match std::fs::metadata(root) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            let mut remedies = Vec::new();
            locate(&mut remedies);
            return problem(
                ProblemKind::FolderMissing,
                format!("{} is not a folder.", root.display()),
                remedies,
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut remedies = Vec::new();
            locate(&mut remedies);
            return problem(
                ProblemKind::FolderMissing,
                format!(
                    "The folder {} was not found. It may have been moved, renamed or deleted, or it is on a drive that is not connected.",
                    root.display()
                ),
                remedies,
            );
        }
        Err(error) => {
            return problem(
                ProblemKind::Unreadable,
                format!(
                    "devflow cannot read {}: {}.",
                    root.display(),
                    error_kind(&error)
                ),
                vec![Remedy::Retry],
            );
        }
    }

    match git2::Repository::open(root) {
        Ok(repo) if repo.is_bare() => {
            return problem(
                ProblemKind::NotGitCheckout,
                format!(
                    "{} is a bare Git repository; devflow needs a repository with a checkout.",
                    root.display()
                ),
                vec![Remedy::Reveal {
                    path: root.to_path_buf(),
                }],
            );
        }
        Ok(_) => {}
        Err(_) => {
            let mut remedies = Vec::new();
            locate(&mut remedies);
            remedies.push(Remedy::Reveal {
                path: root.to_path_buf(),
            });
            return problem(
                ProblemKind::NotGitCheckout,
                format!("{} is not a Git repository (any more).", root.display()),
                remedies,
            );
        }
    }

    // Whose project is this? Asked before the configuration is judged: a
    // folder that now holds another project is that project's to repair, not
    // the registered one's, whatever state its files are in (an earlier
    // build's `[postgres]` must not offer a migration under this entry).
    if let Some((id, _)) = registered.filter(|(id, _)| !id.is_nil())
        && let Ok((found, name)) = Config::read_identity(root)
        && found != id
    {
        return problem(
            ProblemKind::WrongProject,
            format!(
                "The folder {} now holds a different devflow project ('{name}') than the one \
                 listed here. Remove this entry; add the folder again to list the project it \
                 holds.",
                root.display(),
            ),
            vec![Remedy::Reveal {
                path: root.to_path_buf(),
            }],
        );
    }

    match Config::load(root) {
        Ok(_) => Health::Ok,
        Err(error) => classify(root, &error, &problem),
    }
}

/// Explain why opening the project in `root` (listed as `registered`, when
/// it is) failed with `error`: the classification of the disk when it finds a
/// problem, else a generic one that offers to try again (the failure was not
/// about the project's files).
pub fn explain(
    root: &Path,
    registered: Option<(Uuid, &str)>,
    error: &anyhow::Error,
) -> ProjectProblem {
    match check_root(root, registered) {
        Health::Problem(problem) => problem,
        Health::Ok => ProjectProblem {
            kind: ProblemKind::Unreadable,
            message: format!("{error:#}"),
            root: root.to_path_buf(),
            remedies: vec![
                Remedy::Retry,
                Remedy::ForgetProject {
                    root: root.to_path_buf(),
                },
            ],
        },
    }
}

fn classify(
    root: &Path,
    error: &anyhow::Error,
    problem: &dyn Fn(ProblemKind, String, Vec<Remedy>) -> Health,
) -> Health {
    if error.downcast_ref::<NotInitialized>().is_some() {
        return problem(
            ProblemKind::NotInitialized,
            format!("{} has no devflow.toml.", root.display()),
            vec![Remedy::Init {
                root: root.to_path_buf(),
            }],
        );
    }
    if let Some(shape) = error.downcast_ref::<ConfigShape>() {
        return match shape.kind {
            ShapeKind::PreRelease => pre_release(root, shape, problem),
            ShapeKind::Legacy => problem(
                ProblemKind::LegacyConfig,
                format!(
                    "{} has a devflow 0.x configuration. devflow 1.0 uses a new format and never \
                     adopts 0.x containers or data; the migration guide explains how to move to it.",
                    root.display()
                ),
                shape_remedies(shape),
            ),
            ShapeKind::Mixed => {
                let mut remedies = vec![Remedy::OpenFile {
                    path: shape.path.clone(),
                }];
                remedies.extend(shape_remedies(shape));
                problem(ProblemKind::LegacyConfig, shape.message.clone(), remedies)
            }
        };
    }
    // The parser's and validator's messages name the file and the key, never a value.
    let mut remedies = vec![Remedy::OpenFile {
        path: Config::find_file(root).unwrap_or_else(|| root.join(crate::config::CONFIG_FILES[0])),
    }];
    let local = root.join(crate::config::LOCAL_CONFIG_FILE);
    if local.is_file() {
        remedies.push(Remedy::OpenFile { path: local });
    }
    remedies.push(Remedy::Retry);
    problem(ProblemKind::InvalidConfig, format!("{error:#}"), remedies)
}

fn pre_release(
    root: &Path,
    shape: &ConfigShape,
    problem: &dyn Fn(ProblemKind, String, Vec<Remedy>) -> Health,
) -> Health {
    let file = shape
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| crate::config::CONFIG_FILES[0].to_string());
    let intro = format!(
        "{file} in {} was written by an earlier build of devflow 1.0: it still has a top-level \
         [postgres] table, which is now a service.",
        root.display()
    );
    let open = Remedy::OpenFile {
        path: shape.path.clone(),
    };
    match crate::config_edit::migrate::plan(root) {
        Ok(_) => problem(
            ProblemKind::PreReleaseConfig,
            format!(
                "{intro} devflow can rewrite the file in the current format; the database, its \
                 branches and your workspaces are not touched."
            ),
            vec![
                Remedy::MigrateConfig {
                    root: root.to_path_buf(),
                },
                open,
            ],
        ),
        Err(refusal) => problem(
            ProblemKind::PreReleaseConfig,
            format!("{intro} {refusal:#}."),
            vec![open],
        ),
    }
}

/// An I/O error's kind in words, without the OS message (which may quote a path).
fn error_kind(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
        kind => kind.to_string(),
    }
}

#[cfg(test)]
mod tests;
