//! Planning: facts and existing files in, a [`Proposal`] out.
use super::existing::{LOCAL_FILE, sha256_bytes_hex};
use super::patch::{self, Effect, FilePatch, PatchOp};
use super::{
    Confidence, DETECTION_KEY, ExistingFiles, Item, ItemId, ItemKind, PROPOSAL_VERSION, Proposal,
    REPOSITORY_KEY, SCHEMA_VERSION,
};
use crate::config::Config;
use crate::detect::{self, ArtifactKind, EnvCategory, Facts};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use toml_edit::DocumentMut;

/// The item that makes mise load `.devflow/env`.
pub(super) const MISE_WIRING: &str = "mise:env-file";

/// What a blocked wiring costs. Only that: the hooks and daemons devflow
/// proposes go through `devflow exec`.
const WIRING_BLOCKED: &str = "shells in the worktree won't see the branch's variables through \
     mise; commands devflow runs are unaffected";

/// What the person asked for on the command line, applied while planning.
#[derive(Debug, Clone, Default)]
pub struct PlanOptions {
    /// Project name for a new `devflow.toml` (default: the directory name).
    pub name: Option<String>,
    /// Override the PostgreSQL decision.
    pub postgres: Option<PostgresChoice>,
    /// The id for a new project (default: a fresh random one). Tests pin it.
    pub project_id: Option<uuid::Uuid>,
}

/// `--postgres`: overrides what detection would propose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostgresChoice {
    /// Propose a managed server whatever the evidence.
    Managed,
    /// Propose an external server whatever the evidence.
    External,
    /// Propose no PostgreSQL service, and nothing that depends on one.
    None,
}

/// Scan `root` and plan with the files that exist there.
pub fn propose(root: &Path, options: &PlanOptions) -> Result<Proposal> {
    let facts = detect::scan(root)?;
    let existing = ExistingFiles::read(root)?;
    plan_with(root, &facts, &existing, options)
}

/// [`plan_with`] with default options.
pub fn plan(root: &Path, facts: &Facts, existing: &ExistingFiles) -> Result<Proposal> {
    plan_with(root, facts, existing, &PlanOptions::default())
}

/// Build the proposal for `root` from `facts` and `existing`.
///
/// Pure: nothing is written, no command runs. Fails only when the project's
/// existing configuration cannot be read (an invalid `devflow.toml`, a 0.x
/// file, a `pitchfork.toml` or `mise.toml` that is not TOML where a proposal
/// would have to edit it is reported as a warning instead).
pub fn plan_with(
    root: &Path,
    facts: &Facts,
    existing: &ExistingFiles,
    options: &PlanOptions,
) -> Result<Proposal> {
    let mut builder = Builder::new(root, facts, existing, options)?;
    builder.project()?;
    builder.copies();
    builder.postgres();
    builder.containers();
    builder.mise_hooks();
    builder.compose();
    builder.installs();
    builder.codegen();
    builder.migrations();
    builder.presets();
    builder.mise_env_file();
    builder.daemons();
    builder.link_mise_wiring();
    builder.autostart();
    builder.notes();
    Ok(builder.finish())
}

/// A proposed item before its operations are checked against the files.
#[derive(Clone)]
pub(super) struct Spec {
    pub id: String,
    pub kind: ItemKind,
    pub title: String,
    pub value: Value,
    pub confidence: Confidence,
    pub evidence: Vec<String>,
    pub risks: Vec<String>,
    pub depends_on: Vec<String>,
    pub conflicts: Vec<String>,
    pub file: String,
    pub ops: Vec<PatchOp>,
}

impl Spec {
    pub fn new(
        id: impl Into<String>,
        kind: ItemKind,
        file: &str,
        title: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            title: title.into(),
            value: Value::Null,
            confidence: Confidence::High,
            evidence: Vec::new(),
            risks: Vec::new(),
            depends_on: Vec::new(),
            conflicts: Vec::new(),
            file: file.to_string(),
            ops: Vec::new(),
        }
    }

    pub fn value(mut self, value: Value) -> Self {
        self.value = value;
        self
    }

    pub fn confidence(mut self, confidence: Confidence) -> Self {
        self.confidence = confidence;
        self
    }

    pub fn evidence(mut self, evidence: impl IntoIterator<Item = String>) -> Self {
        for line in evidence {
            if !self.evidence.contains(&line) {
                self.evidence.push(line);
            }
        }
        self
    }

    pub fn risks(mut self, risks: impl IntoIterator<Item = String>) -> Self {
        for line in risks {
            if !self.risks.contains(&line) {
                self.risks.push(line);
            }
        }
        self
    }

    pub fn depends_on(mut self, ids: impl IntoIterator<Item = String>) -> Self {
        for id in ids {
            if !self.depends_on.contains(&id) {
                self.depends_on.push(id);
            }
        }
        self
    }

    pub fn ops(mut self, ops: Vec<PatchOp>) -> Self {
        self.ops = ops;
        self
    }
}

/// What happened to a [`Spec`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Pushed {
    /// An item that can be applied.
    Item,
    /// The file already has it.
    Satisfied,
    /// Something in the file stands in the way; the item is only a report.
    Blocked,
    /// The target file cannot be edited at all.
    Unusable,
}

pub(super) struct Builder<'a> {
    pub root: &'a Path,
    pub facts: &'a Facts,
    pub existing: &'a ExistingFiles,
    pub options: &'a PlanOptions,
    /// The merged existing configuration, when a project file exists.
    pub cfg: Option<Config>,
    docs: BTreeMap<String, DocumentMut>,
    /// Target files that are not valid TOML, with the parser's complaint.
    unusable: BTreeMap<String, String>,
    pub items: Vec<Item>,
    pub satisfied: Vec<ItemId>,
    pub warnings: Vec<String>,
    /// Id of the PostgreSQL service item when this plan adds one.
    pub pg_item: Option<ItemId>,
    /// A PostgreSQL service is configured or proposed.
    pub has_pg: bool,
    /// A compose hook that starts services devflow does not branch is proposed.
    pub compose_hooks: bool,
    /// Compose services devflow branches (or the configuration already does)
    /// as `[services.<key>]`: the compose hooks leave them out.
    pub branched_compose: BTreeSet<String>,
    /// The compose services among those that are only *proposed* as a service
    /// item (not configured yet), with the item: declining it leaves the
    /// service to the compose hooks (see `compose`).
    pub branch_fallback: BTreeMap<String, ItemId>,
    /// Compose services devflow recognises but does not branch, with why
    /// (`not proposed automatically: …` or `unsupported: …`).
    pub unbranched_compose: BTreeMap<String, String>,
    /// `.env` files that will be cloned into new worktrees.
    pub copies_env: bool,
    /// Ids of proposed hooks and daemons that run a mise task and need the
    /// workspace's variables from `.devflow/env`: mise, not devflow, hands
    /// them to the task, and a mise `[env]` that does not load the file (or
    /// loads another one over it) replaces them. Commands that `devflow exec`
    /// wraps are not here.
    pub mise_env_users: Vec<ItemId>,
}

impl<'a> Builder<'a> {
    fn new(
        root: &'a Path,
        facts: &'a Facts,
        existing: &'a ExistingFiles,
        options: &'a PlanOptions,
    ) -> Result<Self> {
        let cfg = match existing.text(existing.devflow_file()) {
            Some(text) => Some(
                Config::parse_layers(
                    text,
                    existing.devflow_file(),
                    existing.text(LOCAL_FILE),
                    LOCAL_FILE,
                )
                .with_context(|| {
                    format!(
                        "cannot plan: the existing {} is not a valid devflow 1.0 configuration",
                        existing.devflow_file()
                    )
                })?,
            ),
            None => None,
        };
        let mut unusable = BTreeMap::new();
        let mut docs = BTreeMap::new();
        for file in super::TARGET_FILES {
            match patch::parse(existing.text(file)) {
                Ok(doc) => {
                    docs.insert(file.to_string(), doc);
                }
                Err(error) => {
                    unusable.insert(file.to_string(), error.to_string());
                }
            }
        }
        Ok(Self {
            root,
            facts,
            existing,
            options,
            cfg,
            docs,
            unusable,
            items: Vec::new(),
            satisfied: Vec::new(),
            warnings: Vec::new(),
            pg_item: None,
            has_pg: false,
            compose_hooks: false,
            branched_compose: BTreeSet::new(),
            branch_fallback: BTreeMap::new(),
            unbranched_compose: BTreeMap::new(),
            copies_env: false,
            mise_env_users: Vec::new(),
        })
    }

    /// Check `spec` against the file it patches and add it to the plan.
    pub fn push(&mut self, spec: Spec) -> Pushed {
        let Spec {
            id,
            kind,
            title,
            value,
            confidence,
            evidence,
            risks,
            depends_on,
            mut conflicts,
            file,
            ops,
        } = spec;
        if self.items.iter().any(|item| item.id == id) || self.satisfied.contains(&id) {
            return Pushed::Satisfied;
        }
        let Some(doc) = self.docs.get(&file) else {
            let why = self
                .unusable
                .get(&file)
                .cloned()
                .unwrap_or_else(|| "not a file devflow edits".into());
            self.warnings.push(format!(
                "{file} {why}: not proposing {id}; fix the file or edit it by hand"
            ));
            return Pushed::Unusable;
        };
        // Operations are checked in order on a scratch copy, so a `Set`
        // below a `Table` created by an earlier operation sees that table.
        let mut scratch: DocumentMut = doc.clone();
        let mut kept = Vec::new();
        for op in ops {
            match patch::apply(&mut scratch, &op) {
                Ok(Effect::Apply) => kept.push(op),
                Ok(Effect::Satisfied) => {}
                Ok(Effect::Conflict(why)) => conflicts.push(why),
                Err(error) => {
                    self.warnings.push(format!("not proposing {id}: {error:#}"));
                    return Pushed::Unusable;
                }
            }
        }
        let blocked = !conflicts.is_empty();
        if kept.is_empty() && !blocked {
            self.satisfied.push(id);
            return Pushed::Satisfied;
        }
        let (confidence, kept) = if blocked {
            (Confidence::Low, Vec::new())
        } else {
            (confidence, kept)
        };
        self.items.push(Item {
            id,
            kind,
            title,
            value,
            confidence,
            evidence,
            risks,
            depends_on,
            conflicts,
            patch: FilePatch { file, ops: kept },
        });
        if blocked {
            Pushed::Blocked
        } else {
            Pushed::Item
        }
    }

    /// Add a conflict report that does not come from a patch operation.
    pub fn report_conflict(&mut self, spec: Spec, why: String) {
        let Spec {
            id,
            kind,
            title,
            value,
            evidence,
            risks,
            depends_on,
            mut conflicts,
            file,
            ..
        } = spec;
        conflicts.push(why);
        self.items.push(Item {
            id,
            kind,
            title,
            value,
            confidence: Confidence::Low,
            evidence,
            risks,
            depends_on,
            conflicts,
            patch: FilePatch::none(&file),
        });
    }

    /// Add an item that only asks for work by hand: nothing devflow can write.
    pub fn note(&mut self, spec: Spec) {
        let Spec {
            id,
            kind,
            title,
            value,
            evidence,
            risks,
            depends_on,
            conflicts,
            file,
            ..
        } = spec;
        if self.items.iter().any(|item| item.id == id) {
            return;
        }
        self.items.push(Item {
            id,
            kind,
            title,
            value,
            confidence: Confidence::Low,
            evidence,
            risks,
            depends_on,
            conflicts,
            patch: FilePatch::none(&file),
        });
    }

    /// The text of a read-only or target file, for looking things up.
    pub fn text(&self, file: &str) -> Option<&str> {
        self.existing.text(file)
    }

    pub fn warn(&mut self, message: impl Into<String>) {
        let message = message.into();
        if !self.warnings.contains(&message) {
            self.warnings.push(message);
        }
    }

    pub fn devflow_file(&self) -> &'static str {
        self.existing.devflow_file()
    }

    // -- project ---------------------------------------------------------------------

    fn project(&mut self) -> Result<()> {
        for file in &self.facts.wiring.legacy_devflow_files {
            self.warn(format!(
                "{file} is a devflow 0.x file; devflow 1.0 does not read it and never touches 0.x \
                 resources (see docs/MIGRATING.md)"
            ));
        }
        let file = self.devflow_file();
        if let Some(cfg) = &self.cfg {
            if let Some(name) = &self.options.name
                && name != &cfg.project.name
            {
                let spec = Spec::new(
                    "project:name",
                    ItemKind::Project,
                    file,
                    format!("Rename the project to \"{name}\""),
                )
                .value(json!({"name": name}))
                .evidence([format!("{file}:project.name")]);
                let why = format!(
                    "{file} already names the project; edit project.name by hand if you want \
                     \"{name}\""
                );
                self.report_conflict(spec, why);
            }
            return Ok(());
        }
        let name = match &self.options.name {
            Some(name) => name.clone(),
            None => self
                .root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .context("cannot derive a project name from the directory; pass --name")?,
        };
        if name.trim().is_empty() || name.chars().any(char::is_control) {
            bail!("the project name must not be empty or contain control characters");
        }
        let id = self.options.project_id.unwrap_or_else(uuid::Uuid::new_v4);
        let path = |parts: &[&str]| parts.iter().map(|p| (*p).to_string()).collect::<Vec<_>>();
        let spec = Spec::new(
            "project:init",
            ItemKind::Project,
            file,
            format!("Create {file} for project \"{name}\""),
        )
        .value(json!({"file": file, "name": name, "id": id}))
        .evidence([if self.facts.git.repository {
            ".git".to_string()
        } else {
            "no Git repository was detected".to_string()
        }])
        .ops(vec![
            PatchOp::Table {
                path: path(&["project"]),
                comment: Some(
                    "Project identity. Keep the id stable and committed: it ties trust, the \
                     registry and the database server to this project across machines."
                        .into(),
                ),
                dotted: false,
            },
            PatchOp::Set {
                path: path(&["project", "name"]),
                value: json!(name),
                comment: None,
            },
            PatchOp::Set {
                path: path(&["project", "id"]),
                value: json!(id.to_string()),
                comment: None,
            },
            PatchOp::Table {
                path: path(&["worktree"]),
                comment: None,
                dotted: false,
            },
            PatchOp::Set {
                path: path(&["worktree", "dir"]),
                value: json!("../{repo}.worktrees"),
                comment: Some(
                    "Where new worktrees go ({repo} is this checkout's directory name).".into(),
                ),
            },
        ]);
        self.push(spec);
        Ok(())
    }

    // -- worktree.copy ---------------------------------------------------------------

    fn copies(&mut self) {
        let file = self.devflow_file();
        let mut seen = Vec::new();
        for artifact in &self.facts.artifacts {
            if !artifact.is_copy_candidate() || never_copied(&artifact.path, artifact.kind) {
                continue;
            }
            if seen.contains(&artifact.path) {
                continue;
            }
            seen.push(artifact.path.clone());
            let secret = matches!(
                artifact.kind,
                ArtifactKind::EnvFile | ArtifactKind::SecretKey
            );
            let confidence = match artifact.kind {
                ArtifactKind::GeneratedCode | ArtifactKind::AssetBuilds => Confidence::Medium,
                _ => Confidence::High,
            };
            let mut risks = Vec::new();
            if secret {
                risks.push(
                    "Holds secrets. It is cloned on this machine only and is not part of the \
                     plan; keep it out of Git."
                        .to_string(),
                );
            }
            if artifact.kind == ArtifactKind::EnvFile {
                self.copies_env = true;
            }
            let spec = Spec::new(
                format!("copy:{}", artifact.path),
                ItemKind::Copy,
                file,
                format!("Clone `{}` into new worktrees", artifact.path),
            )
            .value(json!({"path": artifact.path, "why": artifact.reason}))
            .confidence(confidence)
            .evidence(artifact.evidence.clone())
            .evidence(["ignored by Git".to_string()])
            .risks(risks)
            .ops(vec![PatchOp::Append {
                path: vec!["worktree".into(), "copy".into()],
                values: vec![json!(artifact.path)],
                front: false,
            }]);
            self.push(spec);
        }
        // A local env file or key that Git does not ignore is a smell worth
        // mentioning; `.env.example` and friends are meant to be committed.
        for artifact in &self.facts.artifacts {
            let name = artifact.path.rsplit('/').next().unwrap_or("");
            let local = match artifact.kind {
                ArtifactKind::EnvFile => name == ".env" || name.ends_with(".local"),
                ArtifactKind::SecretKey => true,
                _ => false,
            };
            if local && artifact.git_ignored == Some(false) {
                let message = format!(
                    "`{}` exists but Git does not ignore it, so it is not proposed for \
                     worktree.copy; add it to .gitignore first (it may hold secrets)",
                    artifact.path
                );
                self.warn(message);
            }
        }
    }

    // -- mise ------------------------------------------------------------------------

    /// `mise install` runs before everything else.
    ///
    /// `mise trust` is not proposed: a worktree whose configuration mise does
    /// not trust is stopped before the first hook runs (the check of what mise
    /// hands the hooks needs mise to read it), so a hook could not trust it
    /// anyway, and devflow never trusts a configuration for the person.
    /// `devflow doctor` offers the choices instead.
    fn mise_hooks(&mut self) {
        let file = self.devflow_file();
        let Some(mise) = &self.facts.toolchain.mise else {
            return;
        };
        if mise.tools.is_empty() {
            return;
        }
        if self.hook_covered("setup", &super::hooks::Purpose::MiseInstall, "") {
            self.satisfied.push("hook:setup:mise".into());
            return;
        }
        let values = vec![json!({"name": "mise-install", "run": "mise install"})];
        let spec = Spec::new(
            "hook:setup:mise",
            ItemKind::Hook,
            file,
            format!(
                "Install the mise tools of each new worktree ({})",
                mise.tools.join(", ")
            ),
        )
        .value(json!({"phase": "setup", "entries": values}))
        .evidence(mise.evidence.clone())
        .risks([
            "mise has to trust the new worktree's configuration before this can run (a new \
             worktree is a new path); setup stops there and the doctor checks list the ways to \
             trust it. `mise install` needs network access the first time."
                .to_string(),
        ])
        .ops(vec![PatchOp::Append {
            path: vec!["hooks".into(), "setup".into()],
            values,
            front: true,
        }]);
        self.push(spec);
    }

    /// Whether the existing hooks of `phase` already do what `purpose` does.
    pub fn hook_covered(&self, phase: &str, purpose: &super::hooks::Purpose, run: &str) -> bool {
        let Some(cfg) = &self.cfg else {
            return false;
        };
        let entries = match phase {
            "setup" => &cfg.hooks.setup,
            "switch" => &cfg.hooks.switch,
            _ => &cfg.hooks.teardown,
        };
        super::hooks::covered(entries, purpose, run)
    }

    /// A managed service or `[env]` content is configured or proposed:
    /// `.devflow/env` carries variables the project's commands need, so its
    /// daemons run through `devflow exec` and mise loads the file. The engine
    /// does not matter: PostgreSQL, Valkey, Redis and MySQL services export
    /// their URLs the same way, and `[env]` recipes and presets are written to
    /// the same file (as is `COMPOSE_PROJECT_NAME`, for the compose hooks). A
    /// proposal counts whether or not it ends up accepted: `.devflow/env`
    /// always exists, so wrapping a command that then has little to add costs
    /// nothing.
    pub fn needs_workspace_env(&self) -> bool {
        self.has_pg || self.compose_hooks || self.has_container_service() || self.has_env_content()
    }

    /// A service other than the PostgreSQL one (Valkey, Redis, MySQL) is
    /// configured, or proposed with something to write.
    fn has_container_service(&self) -> bool {
        self.cfg
            .as_ref()
            .is_some_and(|cfg| cfg.containers().next().is_some())
            || self.items.iter().any(|item| {
                item.kind == ItemKind::Service
                    && item.is_actionable()
                    && self.pg_item.as_deref() != Some(item.id.as_str())
            })
    }

    /// `[env]` recipes or presets are configured, or a preset is proposed.
    fn has_env_content(&self) -> bool {
        self.cfg
            .as_ref()
            .is_some_and(|cfg| !cfg.env.vars.is_empty() || !cfg.env.presets.is_empty())
            || self
                .items
                .iter()
                .any(|item| item.kind == ItemKind::Env && item.is_actionable())
    }

    fn mise_env_file(&mut self) {
        if !self.needs_workspace_env() {
            return;
        }
        let file = self.existing.mise_file();
        let creating = !self.existing.has_mise_file();
        let wiring = &self.facts.wiring;
        let overridden = wiring.mise_loads_devflow_env && !wiring.mise_env_overridden_by.is_empty();
        let selected = describe_entries(wiring.mise_env_overridden_when_selected.iter());
        if wiring.mise_loads_devflow_env && !overridden {
            self.satisfied.push(MISE_WIRING.into());
            if !selected.is_empty() {
                self.warn(selected_override_warning(&selected));
            }
            return;
        }
        let mut evidence = Vec::new();
        if let Some(mise) = &self.facts.toolchain.mise {
            evidence.extend(mise.config_files.clone());
        } else {
            evidence.push("no mise configuration found; this creates mise.toml".to_string());
        }
        if self.has_pg {
            evidence.push(
                "devflow writes DATABASE_URL and the PG* variables to .devflow/env".to_string(),
            );
        }
        if self.has_container_service() {
            evidence.push(
                "devflow writes the URL of each container service (REDIS_URL, MYSQL_URL, ...) \
                 to .devflow/env"
                    .to_string(),
            );
        }
        if self.has_env_content() {
            evidence.push("devflow writes the [env] recipes and presets to .devflow/env".into());
        }
        if self.compose_hooks {
            evidence.push("devflow writes COMPOSE_PROJECT_NAME to .devflow/env".to_string());
        }
        let confidence = if self.facts.toolchain.mise.is_some() {
            Confidence::High
        } else {
            Confidence::Medium
        };
        let mut risks = vec![
            "Without this, shells and editors in a worktree see the values of the main \
             checkout's .env (its DATABASE_URL, REDIS_URL, ...), not the workspace's own \
             services. Commands devflow runs (setup hooks, pitchfork daemons) go through \
             `devflow exec` and are unaffected."
                .to_string(),
        ];
        if creating {
            risks.push(
                "Needs mise on PATH (https://mise.jdx.dev); without it .devflow/env is not \
                 loaded and you load it yourself."
                    .into(),
            );
        }
        let mut spec = Spec::new(
            MISE_WIRING,
            ItemKind::Mise,
            file,
            if creating {
                format!("Create {file} so mise loads .devflow/env in every worktree")
            } else {
                format!("Load .devflow/env through mise ({file} [env] _.file)")
            },
        )
        .value(json!({"file": file, "env_file": ".devflow/env", "creates_file": creating}))
        .confidence(confidence)
        .evidence(evidence)
        .risks(risks)
        .ops(vec![
            PatchOp::Table {
                path: vec!["env".into()],
                comment: Some(
                    "devflow writes the workspace's DATABASE_URL and other variables to \
                     .devflow/env; mise loads it for shells and editors in the worktree."
                        .into(),
                ),
                dotted: false,
            },
            PatchOp::Table {
                path: vec!["env".into(), "_".into()],
                comment: None,
                dotted: true,
            },
            PatchOp::Set {
                path: vec!["env".into(), "_".into(), "file".into()],
                value: json!(".devflow/env"),
                comment: None,
            },
        ]);
        // Conflicts name the key and the file, never what the existing file
        // holds there.
        if overridden {
            let files = describe_entries(self.facts.wiring.mise_env_overridden_by.iter());
            spec.risks.push(
                "mise lists other files after .devflow/env and applies later entries over \
                 earlier ones, so their values (a .env with the main checkout's DATABASE_URL) \
                 replace the workspace's."
                    .to_string(),
            );
            spec.ops.clear();
            let why = format!(
                "Entries in {files} are applied after .devflow/env; mise lets later entries win, \
                 so the database would come from them, not from this worktree. Move \
                 .devflow/env to the end of the last list by hand"
            );
            self.blocked_wiring(spec, why);
            return;
        }
        // The effective configuration after the patch: `.devflow/env` joins the
        // `_.file` of `file`, and every file mise reads after it is applied
        // over it, whatever that file lists.
        if let Some(mise) = &self.facts.toolchain.mise {
            let (over, sometimes) = mise.entries_over(file);
            if !over.is_empty() {
                let files = describe_entries(over.into_iter());
                spec.risks.push(
                    "A mise file read after the one this edits lists env files, and mise applies \
                     later files over earlier ones: they would replace the workspace's \
                     variables."
                        .to_string(),
                );
                spec.ops.clear();
                let why = format!(
                    "Entries in {files} are applied after {file}, where .devflow/env would be \
                     loaded; mise lets later files win, so the database would come from them, \
                     not from this worktree. Load .devflow/env there instead, last in its \
                     `_.file` list"
                );
                self.blocked_wiring(spec, why);
                return;
            }
            let sometimes = describe_entries(sometimes.into_iter());
            if !sometimes.is_empty() {
                self.warn(selected_override_warning(&sometimes));
                spec.risks.push(selected_override_warning(&sometimes));
            }
        }
        if !self.facts.wiring.mise_env_unresolved.is_empty() {
            spec.risks.push(
                "mise already lists a .devflow/env that is not this worktree's (an absolute \
                 path names one checkout)."
                    .to_string(),
            );
            spec.ops.clear();
            let why = "[env] _.file already mentions .devflow/env through a path that is not \
                       anchored to this worktree; use the relative `.devflow/env` so every \
                       worktree loads its own file, or the database would come from the main \
                       checkout"
                .to_string();
            self.blocked_wiring(spec, why);
            return;
        }
        match self.push(spec.clone()) {
            Pushed::Unusable => {
                spec.ops.clear();
                let why = format!(
                    "{file} is not valid TOML, so devflow cannot add `[env] _.file` to it; fix \
                     the file by hand"
                );
                self.blocked_wiring(spec, why);
            }
            // The file has an `_.file` of its own that the patch would
            // replace: reported as a conflict, with the same consequence.
            Pushed::Blocked => {
                if let Some(item) = self.items.iter_mut().find(|item| item.id == MISE_WIRING) {
                    item.risks.push(WIRING_BLOCKED.to_string());
                }
                self.warn(format!("{MISE_WIRING}: {WIRING_BLOCKED}"));
            }
            Pushed::Item | Pushed::Satisfied => {}
        }
    }

    /// The wiring cannot be applied. Nothing else depends on it (commands
    /// devflow runs get the workspace's variables from `devflow exec`), so
    /// this is a warning about shells and editors, not a blocker.
    fn blocked_wiring(&mut self, mut spec: Spec, why: String) {
        spec.risks.push(WIRING_BLOCKED.to_string());
        self.warn(format!("{MISE_WIRING}: {WIRING_BLOCKED}"));
        self.report_conflict(spec, why);
    }

    /// Make every hook and daemon that needs mise to deliver `.devflow/env`
    /// depend on the item that arranges it. Commands the plan builds go
    /// through `devflow exec`, which sets the workspace's variables after
    /// mise has set its own, so they depend on nothing; what is left is a
    /// mise *task* (`mise run migrate`), whose own `env` is applied after
    /// anything devflow sets and which cannot be wrapped. When the wiring
    /// cannot be applied (the file wires something else, or lists another env
    /// file over `.devflow/env`) or is not accepted, such an item is not
    /// selected by default and `apply` refuses it: run through mise, it would
    /// get another database's `DATABASE_URL`, and a migration would run
    /// against it.
    ///
    /// Nothing to link when mise already loads `.devflow/env` and it wins.
    pub fn link_mise_wiring(&mut self) {
        let users = std::mem::take(&mut self.mise_env_users);
        let Some(wiring) = self.items.iter().find(|item| item.id == MISE_WIRING) else {
            return;
        };
        let note = if wiring.is_actionable() {
            format!(
                "Runs a mise task, which `devflow exec` cannot wrap: it gets the workspace's \
                 DATABASE_URL only once {MISE_WIRING} is applied; without that file it may get \
                 the main checkout's."
            )
        } else {
            format!(
                "Runs a mise task, which `devflow exec` cannot wrap, and mise cannot be made to \
                 deliver this workspace's variables yet ({MISE_WIRING} has a conflict to \
                 resolve by hand): it would be handed the main checkout's DATABASE_URL instead \
                 of the workspace's. Not applied by default."
            )
        };
        for item in &mut self.items {
            if !users.contains(&item.id) || item.id == MISE_WIRING {
                continue;
            }
            if !item.depends_on.iter().any(|d| d == MISE_WIRING) {
                item.depends_on.push(MISE_WIRING.to_string());
            }
            if !item.risks.contains(&note) {
                item.risks.push(note.clone());
            }
        }
    }

    // -- warnings and assembly -------------------------------------------------------

    fn notes(&mut self) {
        let facts = self.facts;
        for warning in &facts.warnings {
            self.warn(format!("detection: {}: {}", warning.path, warning.message));
        }
        if let Some(local) = self.existing.text(LOCAL_FILE)
            && let Ok(table) = local.parse::<toml::Table>()
            && let Some(hooks) = table.get("hooks").and_then(toml::Value::as_table)
        {
            let replaced: Vec<&str> = ["setup", "switch", "teardown"]
                .into_iter()
                .filter(|phase| hooks.contains_key(*phase))
                .collect();
            if !replaced.is_empty() {
                let message = format!(
                    "{LOCAL_FILE} sets hooks.{}: a list there replaces the committed one on this \
                     machine, so hook proposals written to {} are overridden here",
                    replaced.join(", hooks."),
                    self.existing.devflow_file()
                );
                self.warn(message);
            }
        }
        if facts
            .env
            .keys
            .iter()
            .any(|k| k.category == EnvCategory::Port && matches!(k.name.as_str(), "PORT"))
            && self.items.iter().any(|i| i.kind == ItemKind::Daemon)
        {
            self.warn(
                "A `.env` file sets PORT. pitchfork assigns each daemon's PORT, so remove the \
                 fixed value or workspaces started together will collide",
            );
        }
    }

    fn finish(mut self) -> Proposal {
        // Every item that edits a project file that does not exist yet needs
        // the item that creates it.
        let creates_project = self.items.iter().any(|item| item.id == "project:init");
        if creates_project {
            let project_file = self.devflow_file();
            for item in &mut self.items {
                if item.id != "project:init"
                    && item.patch.file == project_file
                    && item.is_actionable()
                    && !item.depends_on.iter().any(|d| d == "project:init")
                {
                    item.depends_on.insert(0, "project:init".into());
                }
            }
        }
        let ids: Vec<String> = self.items.iter().map(|item| item.id.clone()).collect();
        for item in &mut self.items {
            item.depends_on
                .retain(|dependency| ids.contains(dependency));
        }
        let mut input_hashes = self.existing.hashes();
        input_hashes.insert(DETECTION_KEY.to_string(), Some(detection_hash(self.facts)));
        input_hashes.insert(
            REPOSITORY_KEY.to_string(),
            Some(repository_hash(self.facts)),
        );
        // Every message is written without copying what the existing files
        // hold; this is the net under that rule, so a credential that still
        // slipped into one is masked before it reaches a document that is
        // saved, shared and printed.
        for item in &mut self.items {
            scrub(&mut item.title);
            item.evidence.iter_mut().for_each(scrub);
            item.risks.iter_mut().for_each(scrub);
            item.conflicts.iter_mut().for_each(scrub);
        }
        self.warnings.iter_mut().for_each(scrub);
        Proposal {
            proposal_version: PROPOSAL_VERSION,
            schema_version: SCHEMA_VERSION,
            input_hashes,
            items: self.items,
            already_configured: self.satisfied,
            warnings: self.warnings,
        }
    }
}

/// The files (with the directive) that hold `entries`, once each and in the
/// order mise applies them: `mise.local.toml ([env] _.file)`. Never a path or
/// a value from the file.
fn describe_entries<'a>(entries: impl Iterator<Item = &'a detect::MiseEnvEntry>) -> String {
    let mut seen: Vec<String> = Vec::new();
    for entry in entries {
        let text = match &entry.env {
            Some(env) => format!("{} (when MISE_ENV={env})", entry.describe()),
            None => entry.describe(),
        };
        if !seen.contains(&text) {
            seen.push(text);
        }
    }
    seen.join(", ")
}

fn selected_override_warning(files: &str) -> String {
    format!(
        "{files} list env files that mise applies after .devflow/env when their MISE_ENV is \
         selected; a scan cannot tell whether it is. setup and the doctor checks ask \
         mise what it really resolves."
    )
}

/// Mask credentials in free text ([`crate::redact::text`]).
pub(super) fn scrub(text: &mut String) {
    let masked = crate::redact::text(text);
    if masked != *text {
        *text = masked;
    }
}

/// SHA-256 of the facts, without the checkout's path: moving the checkout
/// does not change what was detected.
pub fn detection_hash(facts: &Facts) -> String {
    let mut value = serde_json::to_value(facts).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut value {
        map.remove("root");
    }
    sha256_bytes_hex(&serde_json::to_vec(&value).unwrap_or_default())
}

/// SHA-256 of the facts that applying a proposal cannot change.
///
/// `apply` writes devflow's project file, mise's `[env]` and pitchfork's
/// daemons, and detection reads those files back: the wiring facts, mise's
/// config files and env entries, the pitchfork processes, the evidence and
/// warnings that cite those files. After the first file is written those no
/// longer match the plan, and expectedly so. Everything else (package
/// scripts, lockfiles, frameworks, migrations, compose, `.env` key names,
/// ignored directories, mise tools and tasks, the legacy and local devflow
/// files) is what the plan was made from and what the rest of an interrupted
/// apply still relies on, so it must be exactly as planned.
pub fn repository_hash(facts: &Facts) -> String {
    /// The files `apply` writes.
    const WRITTEN: [&str; 5] = [
        "devflow.toml",
        ".devflow.toml",
        "mise.toml",
        ".mise.toml",
        "pitchfork.toml",
    ];
    let cites_written = |text: &str| {
        WRITTEN.iter().any(|file| {
            text == *file || text.strip_prefix(file).is_some_and(|r| r.starts_with(':'))
        })
    };
    /// Drop the evidence entries that cite a written file, wherever they are.
    fn strip(value: &mut Value, cites_written: &dyn Fn(&str) -> bool) {
        match value {
            Value::Object(map) => {
                for (key, entry) in map.iter_mut() {
                    if key == "evidence"
                        && let Value::Array(items) = entry
                    {
                        items.retain(|item| !item.as_str().is_some_and(cites_written));
                    } else {
                        strip(entry, cites_written);
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|item| strip(item, cites_written)),
            _ => {}
        }
    }

    let mut value = serde_json::to_value(facts).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut value {
        map.remove("root");
        // Wiring: only the files apply never writes.
        if let Some(Value::Object(wiring)) = map.get_mut("wiring") {
            wiring.retain(|key, _| {
                matches!(
                    key.as_str(),
                    "legacy_devflow_files" | "devflow_local_config"
                )
            });
        }
        // mise: what it pins and runs, not which files say so or load what.
        if let Some(Value::Object(toolchain)) = map.get_mut("toolchain") {
            let mise = toolchain.get("mise").and_then(Value::as_object);
            let pick = |key: &str| {
                mise.and_then(|m| m.get(key))
                    .cloned()
                    .unwrap_or_else(|| json!([]))
            };
            let kept = json!({"tools": pick("tools"), "tasks": pick("tasks")});
            toolchain.insert("mise".into(), kept);
        }
        // Daemons already in pitchfork.toml are read back as processes.
        if let Some(Value::Array(processes)) = map.get_mut("processes") {
            processes.retain(|process| process.get("source") != Some(&json!("pitchfork")));
        }
        if let Some(Value::Array(warnings)) = map.get_mut("warnings") {
            warnings.retain(|warning| {
                !warning
                    .get("path")
                    .and_then(Value::as_str)
                    .is_some_and(|path| WRITTEN.contains(&path))
            });
        }
    }
    strip(&mut value, &cites_written);
    sha256_bytes_hex(&serde_json::to_vec(&value).unwrap_or_default())
}

/// Paths that are never worth cloning, whatever detection advises: caches
/// keyed to absolute paths, virtual environments, build output, logs.
fn never_copied(path: &str, kind: ArtifactKind) -> bool {
    if matches!(
        kind,
        ArtifactKind::PythonVenv
            | ArtifactKind::NextCache
            | ArtifactKind::CargoTarget
            | ArtifactKind::Tmp
            | ArtifactKind::Log
            | ArtifactKind::BootstrapCache
            | ArtifactKind::Storage
    ) {
        return true;
    }
    path.split('/').any(|part| {
        matches!(
            part,
            ".venv" | "venv" | ".next" | "target" | "tmp" | "log" | "logs" | ".git" | ".devflow"
        )
    })
}
