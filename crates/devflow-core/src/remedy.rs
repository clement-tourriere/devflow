//! What a person can do about a problem, as data.
//!
//! A [`Remedy`] is one action devflow can take (or point at) to get past a
//! problem: forget a project whose folder is gone, locate it again, rewrite a
//! configuration an earlier build wrote, set a project up. Core names the
//! remedies; every frontend renders them its own way and runs them through
//! core, so none of them keeps a list of fixes of its own:
//!
//! - the desktop app draws [`Remedy::label`] as a button and calls the core
//!   function behind it (`registry::remove`, `registry::relocate`,
//!   `config_edit::migrate::apply`, ...);
//! - the CLI prints [`Remedy::hint`] as a `hint:` line and the JSON envelope
//!   carries [`Remedy::describe`] under `remedies`, so an agent can pick one;
//! - [`Remedy::command`] is the CLI spelling of the action. It is for the CLI
//!   and the JSON document only: no message, label or page of a graphical
//!   frontend ever shows it.
//!
//! A [`Problem`] is a typed error that carries remedies. Its message says what
//! is wrong and never contains a command line.
use crate::error::{ConfigShape, NotInitialized, ShapeKind};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// One thing to do about a problem.
///
/// Serialized with a `kind` tag in snake case, so a new variant is a new
/// `kind`; readers must ignore the kinds they do not know.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Remedy {
    /// Try again: the problem may have been transient, or fixed elsewhere.
    Retry,
    /// Remove a project from devflow's list of projects. Its folder, Git
    /// repository and data are not touched.
    ForgetProject { root: PathBuf },
    /// Point a registered project at the folder it moved to.
    LocateProject { id: Uuid, root: PathBuf },
    /// Rewrite an earlier 1.0 build's configuration (`[postgres]`) in the
    /// current format; see [`crate::config_edit::migrate`].
    MigrateConfig { root: PathBuf },
    /// Set devflow up in a repository that has no `devflow.toml`.
    Init { root: PathBuf },
    /// Open a file for editing.
    OpenFile { path: PathBuf },
    /// Show a file or folder in the file manager.
    Reveal { path: PathBuf },
    /// Open a documentation page.
    OpenGuide { url: String },
}

impl Remedy {
    /// The button text: a verb phrase, in plain words.
    pub fn label(&self) -> String {
        match self {
            Remedy::Retry => "Retry",
            Remedy::ForgetProject { .. } => "Remove from list",
            Remedy::LocateProject { .. } => "Locate…",
            Remedy::MigrateConfig { .. } => "Migrate configuration…",
            Remedy::Init { .. } => "Set up devflow…",
            Remedy::OpenFile { path } => {
                return format!("Open {}", file_label(path));
            }
            Remedy::Reveal { .. } => "Show in file manager",
            Remedy::OpenGuide { .. } => "Read the migration guide",
        }
        .to_string()
    }

    /// Whether the action takes something away (it is confirmed first). A
    /// forgotten project leaves the list; nothing on disk changes.
    pub fn destructive(&self) -> bool {
        matches!(self, Remedy::ForgetProject { .. })
    }

    /// The CLI line that does this, when there is one. Remedies that act on
    /// "the project in the current directory" (`Init`, `MigrateConfig`) give
    /// the command without a directory; [`Remedy::hint`] adds where.
    pub fn command(&self) -> Option<String> {
        match self {
            Remedy::ForgetProject { root } => {
                Some(format!("devflow project forget {}", quote(root)))
            }
            Remedy::LocateProject { root, .. } => {
                Some(format!("devflow project relocate {} NEW_PATH", quote(root)))
            }
            Remedy::MigrateConfig { .. } => Some("devflow config migrate".to_string()),
            Remedy::Init { .. } => Some("devflow init".to_string()),
            Remedy::Retry
            | Remedy::OpenFile { .. }
            | Remedy::Reveal { .. }
            | Remedy::OpenGuide { .. } => None,
        }
    }

    /// The sentence the CLI prints after `hint:`. `None` for remedies that
    /// only make sense in a graphical frontend (retry, reveal).
    pub fn hint(&self) -> Option<String> {
        match self {
            Remedy::Retry | Remedy::Reveal { .. } => None,
            Remedy::ForgetProject { .. } => Some(format!(
                "run `{}` to remove it from the list (nothing on disk is touched)",
                self.command()?
            )),
            Remedy::LocateProject { .. } => {
                Some(format!("if the folder moved, run `{}`", self.command()?))
            }
            Remedy::MigrateConfig { root } => Some(format!(
                "run `{}` in {} (`--dry-run` shows the change first)",
                self.command()?,
                root.display()
            )),
            Remedy::Init { root } => Some(format!(
                "run `{}` in {} (`devflow init --plan` shows the proposed setup first)",
                self.command()?,
                root.display()
            )),
            Remedy::OpenFile { path } => Some(format!("fix {}", path.display())),
            Remedy::OpenGuide { url } => Some(format!("see {url}")),
        }
    }

    /// The remedy with its text, for a JSON document.
    pub fn describe(&self) -> RemedyInfo {
        RemedyInfo {
            remedy: self.clone(),
            label: self.label(),
            command: self.command(),
            destructive: self.destructive(),
        }
    }
}

/// A [`Remedy`] with the texts a reader needs (`devflow --json` documents).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemedyInfo {
    #[serde(flatten)]
    pub remedy: Remedy,
    pub label: String,
    /// The CLI line; absent for actions that only a graphical frontend does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub destructive: bool,
}

/// Describe each remedy of `remedies` for a JSON document.
pub fn describe(remedies: &[Remedy]) -> Vec<RemedyInfo> {
    remedies.iter().map(Remedy::describe).collect()
}

/// `serialize_with` for a list of remedies: each with its label and command.
pub fn serialize_remedies<S: serde::Serializer>(
    remedies: &[Remedy],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    describe(remedies).serialize(serializer)
}

/// A problem with what a person can do about it.
///
/// `message` states what is wrong, in words that are right in any frontend:
/// never a command line (that is [`Remedy::command`]'s job). It travels inside
/// `anyhow::Error`; read it with `error.downcast_ref::<Problem>()` or
/// [`remedies_of`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    pub message: String,
    #[serde(serialize_with = "serialize_remedies")]
    pub remedies: Vec<Remedy>,
}

impl Problem {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            remedies: Vec::new(),
        }
    }

    /// Add a remedy (the most useful first).
    #[must_use]
    pub fn with(mut self, remedy: Remedy) -> Self {
        if !self.remedies.contains(&remedy) {
            self.remedies.push(remedy);
        }
        self
    }

    /// This problem as an `anyhow` error.
    pub fn into_error(self) -> anyhow::Error {
        anyhow::Error::new(self)
    }
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Problem {}

/// The remedies for an error: those of a [`Problem`] in its chain, or the ones
/// core knows for the typed errors it raised before this module existed (a
/// missing `devflow.toml`, a configuration shape it refuses). Empty for an
/// error nobody knows a fix for; the wording of a message never decides.
pub fn remedies_of(error: &anyhow::Error) -> Vec<Remedy> {
    if let Some(problem) = error.downcast_ref::<Problem>() {
        return problem.remedies.clone();
    }
    if let Some(missing) = error.downcast_ref::<NotInitialized>() {
        return vec![Remedy::Init {
            root: missing.root.clone(),
        }];
    }
    if let Some(shape) = error.downcast_ref::<ConfigShape>() {
        return shape_remedies(shape);
    }
    Vec::new()
}

/// The remedies for a configuration file devflow refuses by shape.
pub(crate) fn shape_remedies(shape: &ConfigShape) -> Vec<Remedy> {
    match shape.kind {
        ShapeKind::PreRelease => {
            // `path` is the file with the table; a bare name (text that was
            // never read from a folder) says nothing about where the project is.
            let root = shape.path.parent().filter(|p| !p.as_os_str().is_empty());
            root.map(|root| Remedy::MigrateConfig {
                root: root.to_path_buf(),
            })
            .into_iter()
            .collect()
        }
        ShapeKind::Legacy | ShapeKind::Mixed => vec![Remedy::OpenGuide {
            url: crate::config::MIGRATION_GUIDE.to_string(),
        }],
    }
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// A path as one shell word.
fn quote(path: &Path) -> String {
    let text = path.display().to_string();
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+=:@%,~".contains(c));
    if plain {
        text
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remedies_serialize_with_a_snake_case_kind() {
        let remedy = Remedy::LocateProject {
            id: Uuid::nil(),
            root: PathBuf::from("/a b"),
        };
        let json = serde_json::to_value(&remedy).unwrap();
        assert_eq!(json["kind"], "locate_project");
        assert_eq!(json["root"], "/a b");
        let back: Remedy = serde_json::from_value(json).unwrap();
        assert_eq!(back, remedy);
        assert_eq!(
            serde_json::to_value(Remedy::Retry).unwrap(),
            serde_json::json!({"kind": "retry"})
        );
    }

    #[test]
    fn a_label_is_plain_words_and_a_command_is_for_the_cli() {
        let all = [
            Remedy::Retry,
            Remedy::ForgetProject { root: "/p".into() },
            Remedy::LocateProject {
                id: Uuid::nil(),
                root: "/p".into(),
            },
            Remedy::MigrateConfig { root: "/p".into() },
            Remedy::Init { root: "/p".into() },
            Remedy::OpenFile {
                path: "/p/devflow.toml".into(),
            },
            Remedy::Reveal { path: "/p".into() },
            Remedy::OpenGuide {
                url: "https://example.com/guide".into(),
            },
        ];
        for remedy in &all {
            let label = remedy.label();
            assert!(
                !label.contains("devflow ") && !label.contains('`'),
                "{label}"
            );
            if let Some(command) = remedy.command() {
                assert!(command.starts_with("devflow "), "{command}");
            }
        }
        assert_eq!(all[6].label(), "Show in file manager");
        assert_eq!(all[5].label(), "Open devflow.toml");
        assert!(all[1].destructive());
        assert!(!all[3].destructive());
    }

    #[test]
    fn paths_with_spaces_and_quotes_stay_one_word_in_a_command() {
        let remedy = Remedy::ForgetProject {
            root: "/Users/me/my 'app'".into(),
        };
        assert_eq!(
            remedy.command().unwrap(),
            "devflow project forget '/Users/me/my '\\''app'\\'''"
        );
        let plain = Remedy::ForgetProject {
            root: "/tmp/x".into(),
        };
        assert_eq!(plain.command().unwrap(), "devflow project forget /tmp/x");
    }

    #[test]
    fn the_json_description_adds_the_texts() {
        let info = Remedy::MigrateConfig { root: "/p".into() }.describe();
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["kind"], "migrate_config");
        assert_eq!(json["root"], "/p");
        assert_eq!(json["label"], "Migrate configuration…");
        assert_eq!(json["command"], "devflow config migrate");
        assert_eq!(json["destructive"], false);
        let retry = serde_json::to_value(Remedy::Retry.describe()).unwrap();
        assert!(retry.get("command").is_none());
    }

    #[test]
    fn remedies_come_from_the_type_of_an_error_not_its_wording() {
        let problem = Problem::new("it broke")
            .with(Remedy::Retry)
            .with(Remedy::Retry);
        let error = problem.clone().into_error().context("while loading");
        assert_eq!(remedies_of(&error), vec![Remedy::Retry]);

        let missing = anyhow::Error::new(NotInitialized {
            root: "/repo".into(),
        });
        assert_eq!(
            remedies_of(&missing),
            vec![Remedy::Init {
                root: "/repo".into()
            }]
        );
        assert!(!missing.to_string().contains("devflow init"), "{missing}");
        assert!(remedies_of(&anyhow::anyhow!("run `devflow init` first")).is_empty());
    }

    #[test]
    fn a_refused_configuration_shape_has_its_remedy() {
        let shape = |kind, path: &str| ConfigShape {
            kind,
            path: path.into(),
            message: "refused".into(),
        };
        assert_eq!(
            shape_remedies(&shape(ShapeKind::PreRelease, "/p/devflow.toml")),
            vec![Remedy::MigrateConfig { root: "/p".into() }]
        );
        assert!(shape_remedies(&shape(ShapeKind::PreRelease, "devflow.toml")).is_empty());
        assert!(matches!(
            shape_remedies(&shape(ShapeKind::Legacy, "/p/.devflow.yml"))[..],
            [Remedy::OpenGuide { .. }]
        ));
    }
}
