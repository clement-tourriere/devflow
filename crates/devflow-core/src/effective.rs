//! The effective configuration of one workspace.
//!
//! A project's configuration has two kinds of parts, and they do not come from
//! the same place:
//!
//! - **Shared** (stable, read from the main checkout's working copy):
//!   `[project]`, `[services.*]`, `[data]`, `[integrations]`,
//!   `[pitchfork] autostart` and the `[worktree]` keys other than `copy`
//!   (`dir`, `port_range`, and the keys that arrive with them). They decide
//!   what exists on the machine: a branch never reconfigures a shared server,
//!   and never acquires a resource by renaming a service.
//! - **Recipes** (versioned with the code, read from the workspace's own
//!   `devflow.toml`): `[hooks]`, `[env]` recipes and presets and
//!   `[worktree] copy`. The pitchfork files, which are not devflow's, already
//!   come from the workspace.
//!
//! The main checkout's `devflow.local.toml` overlays both, as it always did
//! (it is checkout-wide); one inside a linked worktree is ignored. The main
//! checkout is both sources at once.
//!
//! A workspace whose own file is missing, invalid, a 0.x shape or another
//! project's has no recipes. That is an explicit condition
//! ([`RecipesSource::Unavailable`]), not a fallback: nothing of the main
//! checkout's recipes runs, is copied or is exported for it, until a person
//! chooses to use the default workspace's recipes
//! ([`RecipesSource::DefaultChosen`], recorded in the receipt), a choice that
//! holds only while the file stays unusable.
//!
//! [`Project::at`](crate::project::Project::at) resolves one [`Effective`]
//! configuration and an operation keeps it for its whole run, so a hook that
//! rewrites `devflow.toml` changes nothing for that run.
use crate::config::{Config, LOCAL_CONFIG_FILE};
use crate::error::{ConfigShape, NotInitialized, ShapeKind};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Why a workspace's own `devflow.toml` cannot supply its recipes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProblemKind {
    /// The checkout has no `devflow.toml` (the branch predates devflow).
    Missing,
    /// The checkout has none, and the main checkout's is not committed: the
    /// workspace's start point predates it, because it is not in any commit
    /// yet (untracked, staged, ignored). `file` is the main checkout's.
    NotCommitted,
    /// A 0.x file or a mixture of 0.x and 1.0 keys.
    Legacy,
    /// It does not parse, fails validation or conflicts with the project's
    /// services (including a pre-release `[postgres]` table).
    Invalid,
    /// It names another project's id.
    OtherProject,
}

impl ProblemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProblemKind::Missing => "missing",
            ProblemKind::NotCommitted => "not_committed",
            ProblemKind::Legacy => "legacy",
            ProblemKind::Invalid => "invalid",
            ProblemKind::OtherProject => "other_project",
        }
    }
}

/// A workspace's own recipes cannot be used, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecipesProblem {
    pub kind: ProblemKind,
    /// The file the recipes would come from (the expected one when missing).
    pub file: PathBuf,
    /// What is wrong, naming the file and the key, never a value.
    pub message: String,
}

/// Where a workspace's recipes (hooks, `[env]`, `[worktree] copy`) come from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum RecipesSource {
    /// The main checkout's own file: the default workspace.
    Main { file: PathBuf },
    /// The workspace's own `devflow.toml`.
    Workspace { file: PathBuf },
    /// The default workspace's recipes, because a person chose them for a
    /// workspace whose own file is unusable (`because`).
    DefaultChosen {
        file: PathBuf,
        because: RecipesProblem,
    },
    /// No usable recipes and no choice: none applies.
    Unavailable { because: RecipesProblem },
}

impl RecipesSource {
    /// `main`, `workspace`, `default_chosen` or `unavailable`.
    pub fn label(&self) -> &'static str {
        match self {
            RecipesSource::Main { .. } => "main",
            RecipesSource::Workspace { .. } => "workspace",
            RecipesSource::DefaultChosen { .. } => "default_chosen",
            RecipesSource::Unavailable { .. } => "unavailable",
        }
    }

    /// The file the recipes are read from, when there is one.
    pub fn file(&self) -> Option<&Path> {
        match self {
            RecipesSource::Main { file }
            | RecipesSource::Workspace { file }
            | RecipesSource::DefaultChosen { file, .. } => Some(file),
            RecipesSource::Unavailable { .. } => None,
        }
    }

    /// Why the workspace's own recipes are not used, when they are not.
    pub fn problem(&self) -> Option<&RecipesProblem> {
        match self {
            RecipesSource::DefaultChosen { because, .. }
            | RecipesSource::Unavailable { because } => Some(because),
            _ => None,
        }
    }

    /// No recipes apply: the condition that needs a person's choice.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, RecipesSource::Unavailable { .. })
    }

    /// One line for a person: where the recipes come from.
    pub fn describe(&self) -> String {
        match self {
            RecipesSource::Main { file } => format!("the main checkout's {}", file.display()),
            RecipesSource::Workspace { file } => {
                format!("this workspace's own {}", file.display())
            }
            RecipesSource::DefaultChosen { file, .. } => format!(
                "the default workspace's {} (chosen: this workspace has no usable file of its own)",
                file.display()
            ),
            RecipesSource::Unavailable { because } => format!(
                "none ({}): no hooks, [env] recipes or copy list apply until the workspace has \
                 a valid devflow.toml or the default workspace's recipes are chosen",
                because.message
            ),
        }
    }
}

/// A shared section the workspace's own file has with another content. It is
/// never applied; it is reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Differing {
    pub file: PathBuf,
    /// The section as written in the file (`[services.db]`, `[worktree] dir`),
    /// never its value.
    pub section: String,
}

impl Differing {
    /// The warning setup, switch and `doctor` show: names the file and the
    /// section, never a value.
    pub fn warning(&self) -> String {
        format!(
            "{} has a {} that differs from the main checkout's; a branch never changes the \
             project's shared sections, so it is not applied",
            self.file.display(),
            self.section
        )
    }
}

/// What one workspace's configuration is made of, and where each part is read
/// from. Resolved once per operation and kept (see the module documentation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Effective {
    /// The workspace directory this was resolved for.
    pub dir: PathBuf,
    /// Where `[hooks]`, `[env]` and `[worktree] copy` come from.
    pub recipes: RecipesSource,
    /// The main checkout's project file: `[project]`, `[services.*]`,
    /// `[data]`, `[integrations]`, `[pitchfork]` and the shared `[worktree]`
    /// keys come from it.
    pub shared: PathBuf,
    /// The main checkout's `devflow.local.toml` when it exists: it overlays
    /// both parts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local: Option<PathBuf>,
    /// A `devflow.local.toml` inside a linked worktree: ignored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignored_local: Option<PathBuf>,
    /// Shared sections the workspace's own file has with another content.
    pub differing: Vec<Differing>,
    /// SHA-256 of the recipes that apply (hooks, `[env]`, copy list); the
    /// digest of what the workspace runs, whichever file it came from.
    pub digest: String,
}

impl Effective {
    /// What a person should hear about this configuration (not an error): the
    /// shared sections a branch file differs in.
    pub fn warnings(&self) -> Vec<String> {
        self.differing.iter().map(Differing::warning).collect()
    }

    /// The warning for a `devflow.local.toml` inside a linked worktree.
    pub fn ignored_local_warning(&self) -> Option<String> {
        self.ignored_local.as_ref().map(|file| {
            format!(
                "{} is ignored: devflow.local.toml is read from the main checkout and applies \
                 to every workspace",
                file.display()
            )
        })
    }
}

/// What happened to the choice to use the default workspace's recipes during
/// a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChoiceChange {
    /// The workspace has no usable file of its own and the default
    /// workspace's recipes were chosen (and recorded in the receipt) now.
    Recorded,
    /// The workspace's own file is valid again: its recipes apply and the
    /// recorded choice was cleared.
    Cleared,
    /// The workspace was set up before recipes were per workspace, when every
    /// workspace ran the default workspace's, and its branch has none of its
    /// own: it keeps them, and that is now recorded as the choice.
    Migrated,
}

/// Where a workspace's configuration comes from, as setup, `list` and
/// `doctor` report it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecipesReport {
    #[serde(flatten)]
    pub source: RecipesSource,
    /// SHA-256 of the recipes that apply (hooks, `[env]`, copy list).
    pub digest: String,
    /// The main checkout's project file, where the shared sections are read.
    pub shared: PathBuf,
    /// The main checkout's `devflow.local.toml`, when it exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local: Option<PathBuf>,
    /// A `devflow.local.toml` inside the worktree, which is ignored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignored_local: Option<PathBuf>,
    /// Shared sections the workspace's own file has with another content;
    /// they are not applied.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub differing: Vec<Differing>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub choice: Option<ChoiceChange>,
}

impl Effective {
    /// The report for this configuration.
    pub fn report(&self, choice: Option<ChoiceChange>) -> RecipesReport {
        RecipesReport {
            source: self.recipes.clone(),
            digest: self.digest.clone(),
            shared: self.shared.clone(),
            local: self.local.clone(),
            ignored_local: self.ignored_local.clone(),
            differing: self.differing.clone(),
            choice,
        }
    }
}

/// The configuration `dir` runs: `main` (the main checkout's, read from
/// `main_file` with its local overlay) with the recipes replaced by the
/// workspace's own, as described in the module documentation. `use_default`
/// is the choice to use the default workspace's recipes where the workspace's
/// own are unusable.
pub(crate) fn resolve(
    main_root: &Path,
    main_file: &Path,
    main: &Config,
    dir: &Path,
    use_default: bool,
) -> (Config, Effective) {
    let local = Some(main_root.join(LOCAL_CONFIG_FILE)).filter(|path| path.is_file());
    let mut effective = Effective {
        dir: dir.to_path_buf(),
        recipes: RecipesSource::Main {
            file: main_file.to_path_buf(),
        },
        shared: main_file.to_path_buf(),
        local,
        ignored_local: None,
        differing: Vec::new(),
        digest: String::new(),
    };
    if dir == main_root {
        effective.digest = main.recipes_digest();
        return (main.clone(), effective);
    }
    effective.ignored_local = Some(dir.join(LOCAL_CONFIG_FILE)).filter(|path| path.is_file());
    let expected =
        Config::find_file(dir).unwrap_or_else(|| dir.join(crate::config::CONFIG_FILES[0]));
    // The workspace's file with the recipe fields of the main checkout's
    // overlay over it: the overlay's shared fields are already in `main`.
    let problem = match Config::load_branch(dir, main_root) {
        Ok(branch) if branch.own.project.id != main.project.id => RecipesProblem {
            kind: ProblemKind::OtherProject,
            message: format!(
                "{} is project {}, not this project ({})",
                branch.path.display(),
                branch.own.project.id,
                main.project.id
            ),
            file: branch.path,
        },
        Ok(branch) => match with_recipes_of(main, &branch.own) {
            Ok(config) => {
                let file = branch.path;
                effective.differing = shared_differences(
                    main,
                    branch.shared_view.as_ref().unwrap_or(&branch.own),
                    &file,
                );
                effective.digest = config.recipes_digest();
                effective.recipes = RecipesSource::Workspace { file };
                return (config, effective);
            }
            Err(message) => RecipesProblem {
                kind: ProblemKind::Invalid,
                file: branch.path,
                message,
            },
        },
        Err(error) => refine(classify(&error, &expected, dir), main_root, main_file, dir),
    };
    if use_default {
        effective.digest = main.recipes_digest();
        effective.recipes = RecipesSource::DefaultChosen {
            file: main_file.to_path_buf(),
            because: problem,
        };
        return (main.clone(), effective);
    }
    // No recipes: the project's shared sections, and nothing that runs.
    let mut config = main.clone();
    clear_recipes(&mut config);
    effective.digest = config.recipes_digest();
    effective.recipes = RecipesSource::Unavailable { because: problem };
    (config, effective)
}

/// `main` with the recipes of `own`, validated as a whole: the recipes must
/// make sense next to the project's services (an `[env]` recipe may not set a
/// variable a service exports, whichever file the service is in).
/// [`with_recipes_of`] as an error that carries the validator's message.
pub(crate) fn combined(main: &Config, own: &Config) -> anyhow::Result<Config> {
    with_recipes_of(main, own).map_err(anyhow::Error::msg)
}

pub(crate) fn with_recipes_of(main: &Config, own: &Config) -> Result<Config, String> {
    let mut config = main.clone();
    config.hooks = own.hooks.clone();
    config.env = own.env.clone();
    config.worktree.copy = own.worktree.copy.clone();
    config.validate().map_err(|error| format!("{error:#}"))?;
    Ok(config)
}

fn clear_recipes(config: &mut Config) {
    config.hooks = Default::default();
    config.env = Default::default();
    config.worktree.copy = Vec::new();
}

/// Turn the error of reading a workspace's file into the condition it is.
fn classify(error: &anyhow::Error, expected: &Path, dir: &Path) -> RecipesProblem {
    if error.downcast_ref::<NotInitialized>().is_some() {
        return RecipesProblem {
            kind: ProblemKind::Missing,
            file: expected.to_path_buf(),
            message: format!(
                "{} does not exist in {}",
                file_name(expected),
                dir.display()
            ),
        };
    }
    if let Some(shape) = error.downcast_ref::<ConfigShape>() {
        let kind = match shape.kind {
            ShapeKind::Legacy | ShapeKind::Mixed => ProblemKind::Legacy,
            ShapeKind::PreRelease => ProblemKind::Invalid,
        };
        return RecipesProblem {
            kind,
            file: shape.path.clone(),
            message: shape.message.clone(),
        };
    }
    RecipesProblem {
        kind: ProblemKind::Invalid,
        file: expected.to_path_buf(),
        // The parser's own text: it names the file and the key path and never
        // repeats a value (see `crate::diag`).
        message: format!("{error:#}"),
    }
}

/// A missing file is a different problem, and has a different way out, when the
/// main checkout has one that no commit holds: the workspace's start point
/// cannot have it, however it was made.
fn refine(
    problem: RecipesProblem,
    main_root: &Path,
    main_file: &Path,
    dir: &Path,
) -> RecipesProblem {
    if problem.kind != ProblemKind::Missing || !not_committed(main_root, main_file) {
        return problem;
    }
    let name = file_name(main_file);
    let start = start_point(dir).map_or_else(
        || "the start point of this workspace".to_string(),
        |rev| format!("the start point of this workspace ({rev})"),
    );
    RecipesProblem {
        kind: ProblemKind::NotCommitted,
        file: main_file.to_path_buf(),
        message: format!(
            "{start} has no {name}, and the main checkout's {} is not committed",
            main_file.display()
        ),
    }
}

/// Whether the main checkout's project file is absent from the commit its
/// `HEAD` names (untracked, only staged, ignored). A repository whose `HEAD`
/// cannot be read says nothing.
fn not_committed(main_root: &Path, main_file: &Path) -> bool {
    let Ok(relative) = main_file.strip_prefix(main_root) else {
        return false;
    };
    let Ok(repo) = git2::Repository::open(main_root) else {
        return false;
    };
    let Ok(tree) = repo.head().and_then(|head| head.peel_to_tree()) else {
        return false;
    };
    matches!(
        tree.get_path(relative),
        Err(ref error) if error.code() == git2::ErrorCode::NotFound
    )
}

/// The commit the workspace's checkout is at, shortened: its start point.
fn start_point(dir: &Path) -> Option<String> {
    let repo = git2::Repository::open(dir).ok()?;
    let commit = repo.head().ok()?.peel_to_commit().ok()?;
    Some(commit.id().to_string().chars().take(8).collect())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// The shared sections in which `own` (a workspace's file, read with the same
/// overlay) differs from `main`. Sections are named, never their values.
fn shared_differences(main: &Config, own: &Config, file: &Path) -> Vec<Differing> {
    let mut sections = Vec::new();
    if main.project.name != own.project.name {
        sections.push("[project] name".to_string());
    }
    let keys: BTreeSet<&String> = main.services.keys().chain(own.services.keys()).collect();
    for key in keys {
        let same = match (main.services.get(key), own.services.get(key)) {
            (Some(a), Some(b)) => a.generation() == b.generation(),
            _ => false,
        };
        if !same {
            sections.push(format!("[services.{}]", crate::diag::key_label(key)));
        }
    }
    if main.data != own.data {
        sections.push("[data]".into());
    }
    if main.integrations != own.integrations {
        sections.push("[integrations]".into());
    }
    if main.pitchfork != own.pitchfork {
        sections.push("[pitchfork]".into());
    }
    // Every `[worktree]` key but the copy list is shared, so a key that
    // arrives later is compared without changing this.
    let worktree = |config: &Config| {
        let mut value = serde_json::to_value(&config.worktree).unwrap_or_default();
        if let Some(map) = value.as_object_mut() {
            map.remove("copy");
        }
        value
    };
    let (a, b) = (worktree(main), worktree(own));
    if a != b {
        let (a, b) = (a.as_object().cloned(), b.as_object().cloned());
        let (a, b) = (a.unwrap_or_default(), b.unwrap_or_default());
        let names: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
        for name in names {
            if a.get(name) != b.get(name) {
                sections.push(format!("[worktree] {name}"));
            }
        }
    }
    sections
        .into_iter()
        .map(|section| Differing {
            file: file.to_path_buf(),
            section,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str =
        "[project]\nname = \"shop\"\nid = \"0b3c9d2e-1111-4222-8333-444455556666\"\n";
    const DB: &str = "[services.db]\nengine = \"postgres\"\nmode = \"external\"\n";

    fn parse(extra: &str) -> Config {
        Config::parse(&format!("{HEAD}{extra}")).unwrap()
    }

    fn sections(main: &str, own: &str) -> Vec<String> {
        shared_differences(&parse(main), &parse(own), Path::new("/ws/devflow.toml"))
            .into_iter()
            .map(|d| d.section)
            .collect()
    }

    #[test]
    fn recipes_are_not_shared_sections() {
        let own = "[hooks]\nsetup = [\"a\"]\n[env]\nA = \"1\"\n[worktree]\ncopy = [\".env\"]\n";
        assert!(sections("", own).is_empty(), "{:?}", sections("", own));
        assert!(sections(own, "").is_empty());
    }

    #[test]
    fn a_shared_section_that_differs_is_named_never_valued() {
        assert_eq!(sections(DB, ""), ["[services.db]"]);
        assert_eq!(sections("", DB), ["[services.db]"]);
        let other = DB.replace("external", "managed");
        assert_eq!(sections(DB, &other), ["[services.db]"]);
        assert_eq!(
            sections("", "[data]\nnoninteractive_from = \"snapshot:seed\"\n"),
            ["[data]"]
        );
        assert_eq!(
            sections("", "[integrations]\ngit_hook = \"background\"\n"),
            ["[integrations]"]
        );
        assert_eq!(
            sections("", "[pitchfork]\nautostart = true\n"),
            ["[pitchfork]"]
        );
        assert_eq!(
            sections(
                "",
                "[worktree]\ndir = \"../elsewhere\"\nport_range = [41000, 41999]\n"
            ),
            ["[worktree] dir", "[worktree] port_range"]
        );
        let renamed = Config::parse(&HEAD.replace("shop", "other")).unwrap();
        let found = shared_differences(&parse(""), &renamed, Path::new("/ws/devflow.toml"));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].section, "[project] name");
        // The warning names the file and the section.
        let warning = found[0].warning();
        assert!(warning.contains("/ws/devflow.toml") && warning.contains("[project] name"));
        assert!(!warning.contains("other"), "{warning}");
    }

    #[test]
    fn a_problem_is_classified_by_what_it_is() {
        let expected = Path::new("/ws/devflow.toml");
        let dir = Path::new("/ws");
        let missing = anyhow::Error::new(NotInitialized { root: dir.into() });
        assert_eq!(classify(&missing, expected, dir).kind, ProblemKind::Missing);
        for (kind, expected_kind) in [
            (ShapeKind::Legacy, ProblemKind::Legacy),
            (ShapeKind::Mixed, ProblemKind::Legacy),
            (ShapeKind::PreRelease, ProblemKind::Invalid),
        ] {
            let shape = anyhow::Error::new(ConfigShape {
                kind,
                path: "/ws/.devflow.yml".into(),
                message: "x".into(),
            });
            let problem = classify(&shape, expected, dir);
            assert_eq!(problem.kind, expected_kind);
            assert_eq!(problem.file, Path::new("/ws/.devflow.yml"));
        }
        let other = anyhow::anyhow!("invalid configuration in /ws/devflow.toml: hooks.nope");
        let problem = classify(&other, expected, dir);
        assert_eq!(problem.kind, ProblemKind::Invalid);
        assert!(problem.message.contains("hooks.nope"));
    }

    #[test]
    fn recipes_that_clash_with_the_projects_services_are_invalid_for_that_project() {
        // A recipe may not set a variable a service of the project exports,
        // whichever file the service is in.
        let main = parse("[services.cache]\nengine = \"valkey\"\nenv = \"CACHE_URL\"\n");
        let own = parse("[env]\nCACHE_URL = \"x\"\n");
        let error = with_recipes_of(&main, &own).unwrap_err();
        assert!(error.contains("CACHE_URL"), "{error}");
        assert!(with_recipes_of(&main, &parse("[env]\nOTHER = \"1\"\n")).is_ok());
    }

    #[test]
    fn the_digest_is_of_the_recipes_that_apply() {
        let a = parse("[hooks]\nsetup = [\"a\"]\n");
        let b = parse("[hooks]\nsetup = [\"a\"]\n");
        let c = parse("[hooks]\nsetup = [\"c\"]\n");
        assert_eq!(a.recipes_digest(), b.recipes_digest());
        assert_ne!(a.recipes_digest(), c.recipes_digest());
        // The services are not part of it.
        assert_eq!(
            parse(&format!("{DB}[hooks]\nsetup = [\"a\"]\n")).recipes_digest(),
            a.recipes_digest()
        );
    }
}
