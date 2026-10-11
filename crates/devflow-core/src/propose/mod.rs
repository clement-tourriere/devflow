//! Turning detected facts into a proposed configuration, and applying it.
//!
//! `devflow init` is two separate steps with a document between them:
//!
//! 1. [`plan`] reads the facts of [`crate::detect`] and the configuration
//!    that already exists, and returns a [`Proposal`]: a list of [`Item`]s,
//!    each with a stable id, a human title, a structured `value`, a
//!    confidence, the evidence behind it, the risks, the items it depends on,
//!    what it conflicts with and the [`FilePatch`] that would write it.
//!    Planning writes nothing, executes no repository code and never puts a
//!    `.env` value or any credential into the proposal.
//! 2. [`apply`] takes a proposal and the ids a person (or an agent) accepted,
//!    validates dependencies and conflicts, checks that nothing it depends on
//!    changed since planning, records an apply journal and writes the files
//!    atomically. A retry resumes where an interrupted apply stopped.
//!
//! Applying only writes configuration. It never installs tools, trusts hooks
//! or runs a command: `devflow trust` and `devflow switch` are separate steps.
//!
//! # Rules
//!
//! - **Existing files are preserved.** Patches are additive and keep the
//!   formatting and comments of what is there. A key that exists with a
//!   different value is a reported conflict, never an overwrite; such an item
//!   cannot be applied.
//! - **Item ids are stable.** `copy:node_modules`, `service:db`,
//!   `hook:setup:install`, `hook:setup:migrate`, `env:preset:prisma`,
//!   `daemon:web`, `mise:env-file`: the same facts give the same ids, so a
//!   plan can be reviewed and its ids passed to `--accept` and `--reject`.
//! - **Confidence decides the default.** `high` items are applied unless
//!   rejected; `medium` and `low` ones only when accepted by id.
//! - **Commands are correct by construction.** mise applies its `[env]` over
//!   the environment of a command that goes through it, so a hook run with
//!   `mise exec` (installs and code generation as much as migrations: a
//!   `postinstall` script can reach the database) and a daemon with
//!   `mise = true` would get whatever database mise's configuration names.
//!   The plan builds them as `mise exec -- devflow exec -- CMD` and
//!   `run = "devflow exec -- CMD"`: `devflow exec` sets the workspace's
//!   variables after mise has set its own, so nothing here depends on how mise
//!   is configured, a `cd DIR &&` ahead of it included. The one thing that
//!   cannot be wrapped is a mise *task* (`mise run migrate`): its own `env` is
//!   applied after anything set before it, so it keeps depending on
//!   `mise:env-file`.
//! - **The wiring is for shells and editors.** `mise:env-file` makes mise load
//!   `.devflow/env`, so interactive shells and editors in the worktree see the
//!   branch's variables. It is applied by default when nothing blocks it;
//!   when something does, it carries a warning only ("shells in the worktree
//!   won't see the branch's variables through mise; commands devflow runs are
//!   unaffected") and holds back nothing but a mise task.
//! - **The wiring is judged on the layered configuration.** mise applies its
//!   files in a fixed order (`mise.toml`, `.mise.toml`, `mise.<env>.toml`,
//!   `mise.local.toml`, `.mise.local.toml`, ...) and the entries of each in
//!   the order they are listed, repeated ones included; later wins. The item
//!   is blocked, naming the file, when an entry comes after `.devflow/env` in
//!   that order: later in the same list, in a file of higher precedence, or
//!   (for a wiring not written yet) in any file above the one it would go to.
//!   A `mise.<env>.toml` only applies when its `MISE_ENV` is selected, which
//!   a scan cannot tell: it is a warning. This reading is an early warning;
//!   `devflow setup` and `devflow doctor` ask mise itself what it resolves.
//! - **`mise trust` is not proposed.** A worktree whose mise configuration
//!   is not trusted is stopped before the first hook runs, so a hook cannot
//!   trust it, and devflow never trusts a configuration for the person;
//!   `devflow doctor` offers the choices instead.
//!
//! # Input hashes
//!
//! A proposal records the SHA-256 of every configuration file it reads or
//! could write (`null` for one that does not exist), of every other mise
//! configuration file detection discovers (`conf.d`, `config.local.toml`,
//! `mise.<env>.toml`, ...; a marker, [`MISE_LAYERS_KEY`], covers the set of
//! their paths so one that appears later counts too), and of the detection
//! facts, both all of them and those the generated configuration does not
//! change. [`apply`] refuses a proposal whose inputs changed, naming them; a
//! retry of an interrupted apply keeps checking the second set and every mise
//! layer, leaving out only the files the journaled apply writes.
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

mod apply;
mod autostart;
mod compose;
mod containers;
mod daemons;
mod existing;
mod hooks;
mod patch;
mod plan;
mod postgres;
mod preview;
mod stack;

#[cfg(test)]
mod tests;

pub use apply::{ApplyReport, FileOutcome, FileStatus, apply, apply_interrupted_after};
pub use existing::{ExistingFiles, MISE_LAYERS_KEY, sha256_hex};
pub use patch::{Effect, FilePatch, PatchOp, TARGET_FILES};
pub use plan::{PlanOptions, PostgresChoice, plan, plan_with, propose};
pub use preview::{
    DiffKind, DiffLine, FileChange, FilePreview, diff_lines, inputs_changed, preview,
};

/// The version of the proposal document format. `apply` refuses other versions.
pub const PROPOSAL_VERSION: u32 = 1;
/// The version of the `devflow.toml` schema the patches are written for
/// (devflow 1.0: `[services.<key>]`, named hook tables, `[env]` presets).
pub const SCHEMA_VERSION: u32 = 1;

/// The key of the detection facts in [`Proposal::input_hashes`].
pub const DETECTION_KEY: &str = "detection";
/// The key of the detection facts that applying cannot change, in
/// [`Proposal::input_hashes`]: all of them except the ones derived from the
/// files `apply` writes (devflow's project file, mise's `[env]`, pitchfork's
/// daemons). An interrupted apply checks these on a retry, when the files it
/// already wrote have changed the rest.
pub const REPOSITORY_KEY: &str = "repository";

/// An item's stable identifier.
pub type ItemId = String;

/// How sure the proposal is that an item is right and wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// A guess worth showing; asks the person to decide.
    Low,
    /// Likely right, but depends on something detection cannot see.
    Medium,
    /// Applied by default.
    High,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::High => "high",
            Confidence::Medium => "medium",
            Confidence::Low => "low",
        }
    }
}

/// What an item configures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    /// `[project]` and `[worktree]` of a new `devflow.toml`.
    Project,
    /// A path in `[worktree] copy`.
    Copy,
    /// A `[services.<key>]` table.
    Service,
    /// A command in `[hooks]`.
    Hook,
    /// A preset in `[env]`.
    Env,
    /// A daemon in `pitchfork.toml`, or devflow's `[pitchfork] autostart`.
    Daemon,
    /// mise configuration.
    Mise,
}

impl ItemKind {
    /// The heading frontends group items under.
    pub fn heading(self) -> &'static str {
        match self {
            ItemKind::Project => "Project",
            ItemKind::Copy => "Files cloned into new worktrees",
            ItemKind::Service => "Services",
            ItemKind::Hook => "Hooks",
            ItemKind::Env => "Environment",
            ItemKind::Daemon => "Processes (pitchfork)",
            ItemKind::Mise => "mise",
        }
    }
}

/// One proposed change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    /// Stable id, e.g. `copy:node_modules` or `hook:setup:migrate`.
    pub id: ItemId,
    pub kind: ItemKind,
    /// One human sentence.
    pub title: String,
    /// The proposed content in structured form (never a secret).
    pub value: Value,
    pub confidence: Confidence,
    /// Files, and after a colon the keys, that support the item.
    pub evidence: Vec<String>,
    /// What can go wrong, or what the proposal could not see.
    pub risks: Vec<String>,
    /// Items that must be accepted with this one.
    pub depends_on: Vec<ItemId>,
    /// Item ids this one cannot be combined with, or descriptions of
    /// existing configuration that stands in the way. A description makes
    /// the item impossible to apply: the file has to be edited by hand.
    pub conflicts: Vec<String>,
    /// What to write. No operations: nothing devflow can write.
    pub patch: FilePatch,
}

impl Item {
    /// Whether there is something to write.
    pub fn is_actionable(&self) -> bool {
        !self.patch.ops.is_empty()
    }

    /// Descriptions of existing configuration that stands in the way.
    pub fn blocking_conflicts<'a>(&'a self, ids: &BTreeSet<&str>) -> Vec<&'a str> {
        self.conflicts
            .iter()
            .map(String::as_str)
            .filter(|conflict| !ids.contains(conflict))
            .collect()
    }
}

/// A proposed configuration for a repository.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proposal {
    /// [`PROPOSAL_VERSION`].
    pub proposal_version: u32,
    /// [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// SHA-256 of each file the proposal reads or could write, by path
    /// (`null`: absent), and of the detection facts under `detection`.
    pub input_hashes: BTreeMap<String, Option<String>>,
    pub items: Vec<Item>,
    /// Ids of items that were not proposed because the configuration already
    /// has them.
    #[serde(default)]
    pub already_configured: Vec<ItemId>,
    pub warnings: Vec<String>,
}

/// Which items to apply, after defaults, `--accept` and `--reject`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Selection {
    /// Accepted ids in plan order.
    pub accepted: Vec<ItemId>,
    /// Default items left out, with the reason.
    pub dropped: Vec<Dropped>,
}

/// A default item that was not selected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Dropped {
    pub id: ItemId,
    pub reason: String,
}

impl Proposal {
    /// The item with this id.
    pub fn item(&self, id: &str) -> Option<&Item> {
        self.items.iter().find(|item| item.id == id)
    }

    /// SHA-256 of the proposal's canonical JSON; names a plan in the apply journal.
    pub fn digest(&self) -> String {
        // Plain data to JSON cannot fail; the map keys are sorted.
        let json = serde_json::to_vec(self).unwrap_or_default();
        existing::sha256_bytes_hex(&json)
    }

    /// The plan as it reads for the `accepted` ids.
    ///
    /// A service devflow only proposes to run itself is a choice between two
    /// ways to start it: its own container when `service:<key>` is accepted,
    /// the compose hooks when it is not. The compose hook items are therefore
    /// rendered from the final selection (see `compose`): they start every
    /// compose service whose branching item is not accepted, and an accepted
    /// hook with nothing left to start drops out. Nothing else changes, and
    /// the input hashes are the plan's own, so this is never a stale plan.
    /// [`apply`] and [`preview`] work on this.
    pub fn resolved(&self, accepted: &[ItemId]) -> Proposal {
        let mut plan = self.clone();
        plan.items = std::mem::take(&mut plan.items)
            .into_iter()
            .filter_map(|item| compose::resolve(item, accepted))
            .collect();
        plan
    }

    /// Check the document is one `apply` can act on: known versions, unique
    /// ids, dependencies that exist, patches that name a configuration file.
    pub fn validate(&self) -> Result<()> {
        if self.proposal_version != PROPOSAL_VERSION {
            bail!(
                "the plan has proposal_version {}, this devflow reads {PROPOSAL_VERSION}; \
                 run `devflow init --plan` again",
                self.proposal_version
            );
        }
        if self.schema_version != SCHEMA_VERSION {
            bail!(
                "the plan targets configuration schema {}, this devflow writes {SCHEMA_VERSION}; \
                 run `devflow init --plan` again",
                self.schema_version
            );
        }
        let mut seen = BTreeSet::new();
        for item in &self.items {
            if !seen.insert(item.id.as_str()) {
                bail!("the plan lists item \"{}\" twice", item.id);
            }
            if !TARGET_FILES.contains(&item.patch.file.as_str()) {
                bail!(
                    "item \"{}\" would write \"{}\", which devflow init does not edit",
                    item.id,
                    item.patch.file
                );
            }
            for op in &item.patch.ops {
                if let Err(why) = op.check_allowed(&item.patch.file) {
                    bail!("item \"{}\": {why}", item.id);
                }
            }
        }
        for item in &self.items {
            for dependency in &item.depends_on {
                if !seen.contains(dependency.as_str()) {
                    bail!(
                        "item \"{}\" depends on \"{dependency}\", which is not in the plan",
                        item.id
                    );
                }
            }
        }
        Ok(())
    }

    /// Decide what to apply.
    ///
    /// With `defaults`, every `high` item that has something to write, no
    /// conflict and all its dependencies selected starts selected; items whose
    /// dependencies are not selected are dropped and reported. `accept` adds
    /// items whatever their confidence, `reject` removes items (and, among the
    /// defaults, whatever depended on them). Explicitly accepted items whose
    /// dependencies are missing stay selected: [`apply`] refuses them, naming
    /// the dependency.
    pub fn select(
        &self,
        accept: &[String],
        reject: &[String],
        defaults: bool,
    ) -> Result<Selection> {
        let known: BTreeSet<&str> = self.items.iter().map(|item| item.id.as_str()).collect();
        for id in accept.iter().chain(reject) {
            if !known.contains(id.as_str()) {
                bail!("the plan has no item \"{id}\"");
            }
        }
        for id in accept {
            if reject.contains(id) {
                bail!("\"{id}\" is both accepted and rejected");
            }
        }
        let mut chosen: BTreeSet<&str> = BTreeSet::new();
        let mut implicit: BTreeSet<&str> = BTreeSet::new();
        if defaults {
            for item in &self.items {
                let blocked = !item.blocking_conflicts(&known).is_empty();
                if item.confidence == Confidence::High
                    && item.is_actionable()
                    && !blocked
                    && !reject.contains(&item.id)
                {
                    chosen.insert(item.id.as_str());
                    implicit.insert(item.id.as_str());
                }
            }
        }
        for id in accept {
            chosen.insert(id.as_str());
            implicit.remove(id.as_str());
        }
        let mut dropped = Vec::new();
        loop {
            let mut changed = false;
            for item in &self.items {
                if !implicit.contains(item.id.as_str()) {
                    continue;
                }
                if let Some(missing) = item
                    .depends_on
                    .iter()
                    .find(|dependency| !chosen.contains(dependency.as_str()))
                {
                    chosen.remove(item.id.as_str());
                    implicit.remove(item.id.as_str());
                    dropped.push(Dropped {
                        id: item.id.clone(),
                        reason: format!("depends on \"{missing}\", {}", self.unmet(missing)),
                    });
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        Ok(Selection {
            accepted: self
                .items
                .iter()
                .filter(|item| chosen.contains(item.id.as_str()))
                .map(|item| item.id.clone())
                .collect(),
            dropped,
        })
    }

    /// Why an item cannot count on `dependency`: it cannot be applied (naming
    /// the first thing that stands in the way, never a value from the files),
    /// or simply is not accepted.
    pub(crate) fn unmet(&self, dependency: &str) -> String {
        let known: BTreeSet<&str> = self.items.iter().map(|item| item.id.as_str()).collect();
        match self.item(dependency) {
            Some(item) => match item.blocking_conflicts(&known).first() {
                Some(conflict) => format!("which cannot be applied: {conflict}"),
                None if !item.is_actionable() => {
                    "which has nothing devflow can write, so it asks for work by hand".to_string()
                }
                None => "which is not accepted".to_string(),
            },
            None => "which is not in the plan".to_string(),
        }
    }

    /// `ids` and everything they depend on, in plan order.
    pub fn with_dependencies(&self, ids: &[String]) -> Vec<ItemId> {
        let mut wanted: BTreeSet<&str> = ids.iter().map(String::as_str).collect();
        loop {
            let before = wanted.len();
            for item in &self.items {
                if wanted.contains(item.id.as_str()) {
                    wanted.extend(item.depends_on.iter().map(String::as_str));
                }
            }
            if wanted.len() == before {
                break;
            }
        }
        self.items
            .iter()
            .filter(|item| wanted.contains(item.id.as_str()))
            .map(|item| item.id.clone())
            .collect()
    }
}

/// The project root's `.git` common directory, where the apply journal lives.
pub(crate) fn journal_dir(root: &Path) -> Result<std::path::PathBuf> {
    let repo = git2::Repository::open(root).map_err(|error| {
        anyhow::anyhow!(
            "{} is not the top of a Git checkout ({}); `devflow init` records its journal in the \
             repository's .git directory",
            root.display(),
            error.message()
        )
    })?;
    Ok(repo.commondir().join("devflow"))
}
