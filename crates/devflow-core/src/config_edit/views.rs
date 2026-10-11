//! Read-only views of the two files for frontends: what the hooks and `[env]`
//! of the merged configuration are, and which file each part comes from.
use super::{ConfigFiles, Target, masked_template};
use crate::config::{Config, EnvPreset, HookEntry};
use crate::hooks::HookPhase;
use anyhow::Result;
use serde::Serialize;

/// One hook of a phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HookRow {
    /// Position in the list, counting from 0.
    pub index: usize,
    pub name: Option<String>,
    pub run: String,
    /// The entry's own timeout, when it has one.
    pub timeout: Option<String>,
    /// The timeout it runs under: its own, else `[hooks] timeout`.
    pub effective_timeout: String,
}

/// The hooks of one phase as they are on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PhaseView {
    pub phase: HookPhase,
    /// The file the list comes from: a list in `devflow.local.toml` replaces
    /// the committed one whole.
    pub source: Target,
    pub hooks: Vec<HookRow>,
    /// The committed hooks that the local list hides (empty unless
    /// `source` is [`Target::Local`]).
    pub shadowed: Vec<HookRow>,
}

impl PhaseView {
    /// The list an edit of `target` works on. Editing the committed file
    /// while the local file replaces the list means the committed list (not
    /// the one in effect); in every other case it is the list in effect (the
    /// first local edit starts from a copy of it).
    pub fn rows_for(&self, target: Target) -> &[HookRow] {
        if target == Target::Project && self.source == Target::Local {
            &self.shadowed
        } else {
            &self.hooks
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HooksView {
    pub setup: PhaseView,
    pub switch: PhaseView,
    pub teardown: PhaseView,
    /// The default timeout per command.
    pub timeout: String,
    pub timeout_source: Target,
}

impl HooksView {
    pub fn phase(&self, phase: HookPhase) -> &PhaseView {
        match phase {
            HookPhase::Setup => &self.setup,
            HookPhase::Switch => &self.switch,
            HookPhase::Teardown => &self.teardown,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.setup.hooks.is_empty()
            && self.switch.hooks.is_empty()
            && self.teardown.hooks.is_empty()
    }
}

/// One preset of the `[env]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PresetRow {
    pub preset: EnvPreset,
    /// The variables the preset can set.
    pub keys: Vec<&'static str>,
}

/// One `NAME = "template"` recipe.
///
/// A recipe may be a credential (a literal in `devflow.local.toml`), and a row
/// is meant to be printed, serialized and logged, so it carries the masked
/// text. The real template is behind [`RecipeRow::reveal`], for a person who
/// asked to see it or an editor that must write it back; it is neither
/// serialized nor shown by `Debug`.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct RecipeRow {
    pub key: String,
    /// The template as it may be shown (`${DATABASE_URL}` unexpanded):
    /// credentials replaced by `***` (see [`masked_template`]).
    pub template: String,
    /// `template` is not what is written in the file.
    pub masked: bool,
    pub source: Target,
    #[serde(skip)]
    raw: String,
}

impl RecipeRow {
    pub(super) fn new(key: &str, raw: &str, source: Target) -> Self {
        let template = masked_template(key, raw);
        Self {
            key: key.to_string(),
            masked: template != raw,
            template,
            source,
            raw: raw.to_string(),
        }
    }

    /// The template as written in the file. May be a credential: never log it.
    pub fn reveal(&self) -> &str {
        &self.raw
    }
}

impl std::fmt::Debug for RecipeRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecipeRow")
            .field("key", &self.key)
            .field("template", &self.template)
            .field("masked", &self.masked)
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EnvView {
    pub presets: Vec<PresetRow>,
    pub presets_source: Target,
    /// Committed presets that a local list hides.
    pub shadowed_presets: Vec<EnvPreset>,
    pub recipes: Vec<RecipeRow>,
}

fn rows(entries: &[HookEntry], default: &str) -> Vec<HookRow> {
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| HookRow {
            index,
            name: entry.name.clone(),
            run: entry.run.clone(),
            timeout: entry.timeout.clone(),
            effective_timeout: entry.timeout.clone().unwrap_or_else(|| default.to_string()),
        })
        .collect()
}

fn table(text: Option<&str>) -> Option<toml::Table> {
    text?.parse().ok()
}

impl ConfigFiles {
    /// The hooks of the merged configuration, with the file each list is from.
    pub fn hooks(&self) -> Result<HooksView> {
        let config = self.config()?;
        let local = table(self.local.text.as_deref());
        let project = table(self.project.text.as_deref());
        let default = config.hooks.timeout.clone();
        let phase = |phase: HookPhase| -> PhaseView {
            let local_defines = local
                .as_ref()
                .and_then(|t| t.get("hooks")?.get(phase.as_str()))
                .is_some();
            let shadowed = if local_defines {
                project
                    .as_ref()
                    .and_then(|t| t.get("hooks")?.get(phase.as_str()).cloned())
                    .and_then(|v| v.try_into::<Vec<HookEntry>>().ok())
                    .map(|entries| rows(&entries, &default))
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            PhaseView {
                phase,
                source: if local_defines {
                    Target::Local
                } else {
                    Target::Project
                },
                hooks: rows(config.hooks.entries(phase), &default),
                shadowed,
            }
        };
        let timeout_source = if local
            .as_ref()
            .and_then(|t| t.get("hooks")?.get("timeout"))
            .is_some()
        {
            Target::Local
        } else {
            Target::Project
        };
        Ok(HooksView {
            setup: phase(HookPhase::Setup),
            switch: phase(HookPhase::Switch),
            teardown: phase(HookPhase::Teardown),
            timeout: default.clone(),
            timeout_source,
        })
    }

    /// The `[env]` section of the merged configuration, with sources.
    pub fn env(&self) -> Result<EnvView> {
        let config: Config = self.config()?;
        let env_of =
            |text: Option<&str>| table(text).and_then(|t| t.get("env")?.as_table().cloned());
        let local = env_of(self.local.text.as_deref());
        let project = env_of(self.project.text.as_deref());
        let local_presets = local.as_ref().is_some_and(|t| t.contains_key("presets"));
        let shadowed_presets = if local_presets {
            project
                .as_ref()
                .and_then(|t| t.get("presets").cloned())
                .and_then(|v| v.try_into::<Vec<String>>().ok())
                .map(|names| names.iter().filter_map(|n| EnvPreset::parse(n)).collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        Ok(EnvView {
            presets: config
                .env
                .presets
                .iter()
                .map(|&preset| PresetRow {
                    preset,
                    keys: preset.keys().to_vec(),
                })
                .collect(),
            presets_source: if local_presets {
                Target::Local
            } else {
                Target::Project
            },
            shadowed_presets,
            recipes: config
                .env
                .vars
                .iter()
                .map(|(key, template)| {
                    let source = if local.as_ref().is_some_and(|t| t.contains_key(key)) {
                        Target::Local
                    } else {
                        Target::Project
                    };
                    RecipeRow::new(key, template, source)
                })
                .collect(),
        })
    }
}
