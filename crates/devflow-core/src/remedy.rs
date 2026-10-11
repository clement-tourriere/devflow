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
    /// Run setup again in a workspace (the checkout at `workspace`): it
    /// resumes what did not finish, or sets up a checkout devflow has not.
    Setup { workspace: PathBuf },
    /// Review and approve what a workspace asks to run (hooks, `[env]`
    /// recipes, pitchfork processes) so that setup may apply it.
    Trust { workspace: PathBuf },
    /// Start the project's managed PostgreSQL server.
    StartDatabase { root: PathBuf },
    /// Start the container of service `key` (the branch `branch`, when the
    /// problem is about one).
    StartService {
        root: PathBuf,
        key: String,
        branch: Option<Uuid>,
    },
    /// Measure again whether the volumes of the Docker engine that holds
    /// service `key` share blocks (the measurement is per engine; `key` names
    /// the service to run the helper container for).
    MeasureServices { root: PathBuf, key: String },
    /// Finish or roll back the operations of service `key` that did not
    /// finish.
    ReconcileService { root: PathBuf, key: String },
    /// Remove everything recorded for service `key`, which the configuration
    /// no longer has (its containers and volumes included).
    ForgetService { root: PathBuf, key: String },
    /// Remove the branch `branch` of service `key` by its recorded identity.
    DropServiceBranch {
        root: PathBuf,
        key: String,
        branch: Uuid,
    },
    /// Finish PostgreSQL work that was interrupted (branches, scratch
    /// databases).
    ReconcileDatabases { root: PathBuf },
    /// Drop the databases the configuration no longer lists and that devflow
    /// kept.
    DropOrphanDatabases { root: PathBuf },
    /// Install the Git hook that sets up worktrees other tools create.
    InstallGitHook { root: PathBuf },
    /// Install pitchfork (the plan is core's; the person is asked first).
    InstallPitchfork,
    /// Forget the Git registrations of worktrees whose directory is gone.
    PruneWorktrees { root: PathBuf },
    /// Remove the workspace `name` of the project at `root`.
    RemoveWorkspace { root: PathBuf, name: String },
    /// Switch to the workspace `name` of the project at `root`.
    SwitchTo { root: PathBuf, name: String },
    /// Name a preset in `[env]` of the project at `root`.
    AddEnvPreset { root: PathBuf, preset: String },
}

impl Remedy {
    /// The `kind` this remedy serializes with (`forget_project`, ...): a stable
    /// name for element ids and for matching on data.
    pub fn kind(&self) -> &'static str {
        match self {
            Remedy::Retry => "retry",
            Remedy::ForgetProject { .. } => "forget_project",
            Remedy::LocateProject { .. } => "locate_project",
            Remedy::MigrateConfig { .. } => "migrate_config",
            Remedy::Init { .. } => "init",
            Remedy::OpenFile { .. } => "open_file",
            Remedy::Reveal { .. } => "reveal",
            Remedy::OpenGuide { .. } => "open_guide",
            Remedy::Setup { .. } => "setup",
            Remedy::Trust { .. } => "trust",
            Remedy::StartDatabase { .. } => "start_database",
            Remedy::StartService { .. } => "start_service",
            Remedy::MeasureServices { .. } => "measure_services",
            Remedy::ReconcileService { .. } => "reconcile_service",
            Remedy::ForgetService { .. } => "forget_service",
            Remedy::DropServiceBranch { .. } => "drop_service_branch",
            Remedy::ReconcileDatabases { .. } => "reconcile_databases",
            Remedy::DropOrphanDatabases { .. } => "drop_orphan_databases",
            Remedy::InstallGitHook { .. } => "install_git_hook",
            Remedy::InstallPitchfork => "install_pitchfork",
            Remedy::PruneWorktrees { .. } => "prune_worktrees",
            Remedy::RemoveWorkspace { .. } => "remove_workspace",
            Remedy::SwitchTo { .. } => "switch_to",
            Remedy::AddEnvPreset { .. } => "add_env_preset",
        }
    }

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
            // Several workspaces may be listed side by side: name which.
            Remedy::Setup { workspace } => {
                return format!("Run setup in {}", file_label(workspace));
            }
            Remedy::Trust { .. } => "Review and approve…",
            Remedy::StartDatabase { .. } => "Start the database server",
            Remedy::StartService { key, .. } => {
                return format!("Start {key}");
            }
            Remedy::MeasureServices { .. } => "Measure again",
            Remedy::ReconcileService { key, .. } => {
                return format!("Finish or roll back {key}");
            }
            Remedy::ForgetService { key, .. } => {
                return format!("Forget service {key}…");
            }
            Remedy::DropServiceBranch { key, branch, .. } => {
                let id: String = branch.to_string().chars().take(8).collect();
                return format!("Drop branch {id} of {key}…");
            }
            Remedy::ReconcileDatabases { .. } => "Finish interrupted work",
            Remedy::DropOrphanDatabases { .. } => "Drop unused databases…",
            Remedy::InstallGitHook { .. } => "Install the Git hook",
            Remedy::InstallPitchfork => "Install pitchfork…",
            Remedy::PruneWorktrees { .. } => "Clean up missing worktrees",
            Remedy::RemoveWorkspace { name, .. } => {
                return format!("Remove {name}…");
            }
            Remedy::SwitchTo { name, .. } => {
                return format!("Switch to {name}");
            }
            Remedy::AddEnvPreset { preset, .. } => {
                return format!("Add the {preset} preset");
            }
        }
        .to_string()
    }

    /// Whether the action takes something away (it is confirmed first). A
    /// forgotten project leaves the list; nothing on disk changes.
    pub fn destructive(&self) -> bool {
        matches!(
            self,
            Remedy::ForgetProject { .. }
                | Remedy::ForgetService { .. }
                | Remedy::DropServiceBranch { .. }
                | Remedy::DropOrphanDatabases { .. }
                | Remedy::RemoveWorkspace { .. }
        )
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
            Remedy::Setup { .. } => Some("devflow setup".to_string()),
            Remedy::Trust { .. } => Some("devflow trust".to_string()),
            Remedy::StartDatabase { .. } => Some("devflow db up".to_string()),
            Remedy::StartService { key, .. } => Some(format!("devflow service {} up", word(key))),
            Remedy::MeasureServices { .. } => Some("devflow service status --probe".to_string()),
            Remedy::ReconcileService { key, .. } => {
                Some(format!("devflow service {} reconcile", word(key)))
            }
            Remedy::ForgetService { key, .. } => {
                Some(format!("devflow service {} forget", word(key)))
            }
            Remedy::DropServiceBranch { key, branch, .. } => {
                Some(format!("devflow service {} drop {branch}", word(key)))
            }
            Remedy::ReconcileDatabases { .. } => Some("devflow db reconcile".to_string()),
            Remedy::DropOrphanDatabases { .. } => {
                Some("devflow db reconcile --drop-orphans".to_string())
            }
            Remedy::InstallGitHook { .. } => Some("devflow hook install".to_string()),
            Remedy::InstallPitchfork => Some("devflow pitchfork install".to_string()),
            // devflow has no command of its own for it; Git does it.
            Remedy::PruneWorktrees { .. } => Some("git worktree prune".to_string()),
            Remedy::RemoveWorkspace { name, .. } => Some(format!("devflow remove {}", word(name))),
            Remedy::SwitchTo { name, .. } => Some(format!("devflow switch {}", word(name))),
            Remedy::AddEnvPreset { preset, .. } => {
                Some(format!("devflow env preset add {}", word(preset)))
            }
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
            Remedy::Setup { workspace } | Remedy::Trust { workspace } => Some(format!(
                "run `{}` in {}",
                self.command()?,
                workspace.display()
            )),
            Remedy::DropServiceBranch { .. }
            | Remedy::DropOrphanDatabases { .. }
            | Remedy::ForgetService { .. }
            | Remedy::RemoveWorkspace { .. } => Some(format!(
                "run `{}` if you are sure (it removes data)",
                self.command()?
            )),
            Remedy::PruneWorktrees { .. } => Some(format!(
                "run `{}` to forget the worktrees whose directory is gone",
                self.command()?
            )),
            Remedy::StartDatabase { .. }
            | Remedy::StartService { .. }
            | Remedy::MeasureServices { .. }
            | Remedy::ReconcileService { .. }
            | Remedy::ReconcileDatabases { .. }
            | Remedy::InstallGitHook { .. }
            | Remedy::InstallPitchfork
            | Remedy::SwitchTo { .. }
            | Remedy::AddEnvPreset { .. } => Some(format!("run `{}`", self.command()?)),
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
    if let Some(stale) = error.downcast_ref::<crate::error::StalePlan>()
        && !stale.root.as_os_str().is_empty()
    {
        return vec![Remedy::Init {
            root: stale.root.clone(),
        }];
    }
    if let Some(stale) = error.downcast_ref::<crate::error::StalePlan>()
        && !stale.root.as_os_str().is_empty()
    {
        return vec![Remedy::Init {
            root: stale.root.clone(),
        }];
    }
    if let Some(forgetting) = error.downcast_ref::<crate::services::catalog::Forgetting>()
        && !forgetting.root.as_os_str().is_empty()
    {
        return vec![Remedy::ForgetService {
            root: forgetting.root.clone(),
            key: forgetting.key.clone(),
        }];
    }
    if let Some(refused) = error.downcast_ref::<crate::services::catalog::ForgetRefused>()
        && !refused.root.as_os_str().is_empty()
    {
        return vec![Remedy::ReconcileService {
            root: refused.root.clone(),
            key: refused.key.clone(),
        }];
    }
    if let Some(lost) = error.downcast_ref::<crate::error::LockLost>() {
        return vec![Remedy::ReconcileDatabases {
            root: lost.root.clone(),
        }];
    }
    if let Some(protected) = error.downcast_ref::<crate::error::Protected>() {
        return protected.remedies.clone();
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
    word(&path.display().to_string())
}

/// Text as one shell word.
fn word(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+=:@%,~".contains(c));
    if plain {
        text.to_string()
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
            Remedy::Setup {
                workspace: "/p.wt/a".into(),
            },
            Remedy::Trust {
                workspace: "/p.wt/a".into(),
            },
            Remedy::StartDatabase { root: "/p".into() },
            Remedy::StartService {
                root: "/p".into(),
                key: "cache".into(),
                branch: Some(Uuid::nil()),
            },
            Remedy::MeasureServices {
                root: "/p".into(),
                key: "cache".into(),
            },
            Remedy::ReconcileService {
                root: "/p".into(),
                key: "cache".into(),
            },
            Remedy::ForgetService {
                root: "/p".into(),
                key: "cache".into(),
            },
            Remedy::DropServiceBranch {
                root: "/p".into(),
                key: "cache".into(),
                branch: Uuid::nil(),
            },
            Remedy::ReconcileDatabases { root: "/p".into() },
            Remedy::DropOrphanDatabases { root: "/p".into() },
            Remedy::InstallGitHook { root: "/p".into() },
            Remedy::InstallPitchfork,
            Remedy::PruneWorktrees { root: "/p".into() },
            Remedy::RemoveWorkspace {
                root: "/p".into(),
                name: "a".into(),
            },
            Remedy::SwitchTo {
                root: "/p".into(),
                name: "a".into(),
            },
            Remedy::AddEnvPreset {
                root: "/p".into(),
                preset: "prisma".into(),
            },
        ];
        let mut kinds = std::collections::BTreeSet::new();
        for remedy in &all {
            let serialized = serde_json::to_value(remedy).unwrap();
            assert_eq!(serialized["kind"], remedy.kind());
            assert!(kinds.insert(remedy.kind()), "{} twice", remedy.kind());
            let label = remedy.label();
            assert!(
                !label.contains("devflow ") && !label.contains('`') && !label.contains("--"),
                "{label}"
            );
            if let Some(command) = remedy.command() {
                // `PruneWorktrees` is Git's own command: devflow has none.
                assert!(
                    command.starts_with("devflow ") || command == "git worktree prune",
                    "{command}"
                );
                // What the CLI prints after `hint:` carries the command.
                let hint = remedy.hint().unwrap();
                assert!(hint.contains(&format!("`{command}`")), "{hint}");
            }
            // It survives the trip through JSON.
            let back: Remedy = serde_json::from_value(serialized).unwrap();
            assert_eq!(&back, remedy);
        }
        let by = |kind: &str| all.iter().find(|r| r.kind() == kind).unwrap();
        for kind in [
            "forget_project",
            "forget_service",
            "drop_service_branch",
            "drop_orphan_databases",
            "remove_workspace",
        ] {
            assert!(by(kind).destructive(), "{kind}");
        }
        for kind in ["setup", "reconcile_service", "switch_to", "start_database"] {
            assert!(!by(kind).destructive(), "{kind}");
        }
        assert_eq!(
            by("forget_service").command().unwrap(),
            "devflow service cache forget"
        );
        assert_eq!(
            by("trust").hint().unwrap(),
            "run `devflow trust` in /p.wt/a"
        );
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
    fn the_typed_errors_of_core_know_their_remedies() {
        use crate::error::{LockLost, Protected, StalePlan};
        use crate::services::catalog::{ForgetRefused, Forgetting};
        let root = PathBuf::from("/shop");
        let errors: Vec<(anyhow::Error, Vec<Remedy>)> = vec![
            (
                anyhow::Error::new(Protected {
                    workspace: "a".into(),
                    reason: "teardown hooks are not trusted".into(),
                    forceable: true,
                    remedies: vec![Remedy::Trust {
                        workspace: "/shop.wt/a".into(),
                    }],
                }),
                vec![Remedy::Trust {
                    workspace: "/shop.wt/a".into(),
                }],
            ),
            (
                anyhow::Error::new(LockLost { root: root.clone() }),
                vec![Remedy::ReconcileDatabases { root: root.clone() }],
            ),
            (
                anyhow::Error::new(StalePlan {
                    message: "the repository changed".into(),
                    root: root.clone(),
                }),
                vec![Remedy::Init { root: root.clone() }],
            ),
            (
                anyhow::Error::new(Forgetting {
                    key: "cache".into(),
                    root: root.clone(),
                }),
                vec![Remedy::ForgetService {
                    root: root.clone(),
                    key: "cache".into(),
                }],
            ),
            (
                anyhow::Error::new(ForgetRefused {
                    key: "cache".into(),
                    found: vec!["branch x".into()],
                    root: root.clone(),
                }),
                vec![Remedy::ReconcileService {
                    root: root.clone(),
                    key: "cache".into(),
                }],
            ),
        ];
        for (error, wanted) in errors {
            let message = format!("{error:#}");
            assert!(
                !message.contains("`devflow") && !message.contains("--force"),
                "{message}"
            );
            // Found through a layer of context, too.
            assert_eq!(remedies_of(&error.context("while working")), wanted);
        }
        // An error that was raised where the project is not known has none.
        let unknown = anyhow::Error::new(Forgetting {
            key: "cache".into(),
            root: PathBuf::new(),
        });
        assert!(remedies_of(&unknown).is_empty());
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
