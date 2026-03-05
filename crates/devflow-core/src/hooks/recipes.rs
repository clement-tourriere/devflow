use super::{ExtendedHookEntry, HookEntry, HookPhase, HooksConfig, IndexMap};
use serde::Serialize;

/// A pre-built hook recipe that users can install into their project.
#[derive(Debug, Clone)]
pub struct HookRecipe {
    pub name: &'static str,
    pub description: &'static str,
    pub category: &'static str,
    pub hooks: HooksConfig,
}

/// Serializable preview of a recipe's hooks.
#[derive(Debug, Clone, Serialize)]
pub struct RecipeHookPreview {
    pub phase: String,
    pub hook_name: String,
    pub command_summary: String,
}

/// Serializable recipe info for GUI/JSON output.
#[derive(Debug, Clone, Serialize)]
pub struct RecipeInfo {
    pub name: String,
    pub description: String,
    pub category: String,
    pub hooks_preview: Vec<RecipeHookPreview>,
}

impl HookRecipe {
    /// Convert to a serializable `RecipeInfo`.
    pub fn to_info(&self) -> RecipeInfo {
        let mut hooks_preview = Vec::new();
        for (phase, phase_hooks) in &self.hooks {
            for (name, entry) in phase_hooks {
                let summary = match entry {
                    HookEntry::Simple(cmd) => cmd.clone(),
                    HookEntry::Extended(ext) => ext.command.clone(),
                    HookEntry::Action(act) => format!("action: {}", act.action.type_name()),
                };
                hooks_preview.push(RecipeHookPreview {
                    phase: phase.to_string(),
                    hook_name: name.clone(),
                    command_summary: summary,
                });
            }
        }
        RecipeInfo {
            name: self.name.to_string(),
            description: self.description.to_string(),
            category: self.category.to_string(),
            hooks_preview,
        }
    }
}

/// Result of installing a recipe.
#[derive(Debug, Clone, Serialize)]
pub struct InstallRecipeResult {
    pub hooks_added: usize,
    pub hooks_skipped: usize,
}

/// Return all built-in hook recipes.
pub fn builtin_recipes() -> Vec<HookRecipe> {
    vec![sync_ai_configs_recipe()]
}

/// Find a built-in recipe by name.
pub fn find_recipe(name: &str) -> Option<HookRecipe> {
    builtin_recipes().into_iter().find(|r| r.name == name)
}

/// Merge a recipe's hooks into an existing hooks config.
///
/// Only adds hooks that don't already exist (by phase + name).
/// Returns the number of hooks added and skipped.
pub fn merge_recipe_into_config(
    existing: &mut HooksConfig,
    recipe: &HookRecipe,
) -> InstallRecipeResult {
    let mut added = 0;
    let mut skipped = 0;

    for (phase, recipe_hooks) in &recipe.hooks {
        let phase_map = existing
            .entry(phase.clone())
            .or_default();
        for (name, entry) in recipe_hooks {
            if phase_map.contains_key(name) {
                skipped += 1;
            } else {
                phase_map.insert(name.clone(), entry.clone());
                added += 1;
            }
        }
    }

    InstallRecipeResult {
        hooks_added: added,
        hooks_skipped: skipped,
    }
}

// ── Built-in Recipes ──────────────────────────────────────────────────

fn sync_ai_configs_recipe() -> HookRecipe {
    let mut hooks = IndexMap::new();

    // pre-remove: sync AI configs back before workspace deletion
    let mut pre_remove = IndexMap::new();
    pre_remove.insert(
        "sync-ai-configs".to_string(),
        HookEntry::Extended(ExtendedHookEntry {
            command: "devflow sync-ai-configs".to_string(),
            working_dir: None,
            continue_on_error: Some(true),
            condition: None,
            environment: None,
            background: false,
        }),
    );
    hooks.insert(HookPhase::PreRemove, pre_remove);

    // post-merge: sync AI configs after merging
    let mut post_merge = IndexMap::new();
    post_merge.insert(
        "sync-ai-configs".to_string(),
        HookEntry::Extended(ExtendedHookEntry {
            command: "devflow sync-ai-configs".to_string(),
            working_dir: None,
            continue_on_error: Some(true),
            condition: None,
            environment: None,
            background: false,
        }),
    );
    hooks.insert(HookPhase::PostMerge, post_merge);

    HookRecipe {
        name: "sync-ai-configs",
        description: "Sync AI tool configs (.claude, .cursor, etc.) back to main worktree on workspace removal and merge",
        category: "AI Tools",
        hooks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_recipes_not_empty() {
        let recipes = builtin_recipes();
        assert!(!recipes.is_empty());
    }

    #[test]
    fn test_find_recipe() {
        assert!(find_recipe("sync-ai-configs").is_some());
        assert!(find_recipe("nonexistent").is_none());
    }

    #[test]
    fn test_merge_recipe_adds_hooks() {
        let mut config = IndexMap::new();
        let recipe = sync_ai_configs_recipe();
        let result = merge_recipe_into_config(&mut config, &recipe);
        assert_eq!(result.hooks_added, 2);
        assert_eq!(result.hooks_skipped, 0);
        assert!(config.contains_key(&HookPhase::PreRemove));
        assert!(config.contains_key(&HookPhase::PostMerge));
    }

    #[test]
    fn test_merge_recipe_skips_existing() {
        let recipe = sync_ai_configs_recipe();
        let mut config = IndexMap::new();
        // First install
        merge_recipe_into_config(&mut config, &recipe);
        // Second install should skip
        let result = merge_recipe_into_config(&mut config, &recipe);
        assert_eq!(result.hooks_added, 0);
        assert_eq!(result.hooks_skipped, 2);
    }

    #[test]
    fn test_recipe_to_info() {
        let recipe = sync_ai_configs_recipe();
        let info = recipe.to_info();
        assert_eq!(info.name, "sync-ai-configs");
        assert_eq!(info.category, "AI Tools");
        assert_eq!(info.hooks_preview.len(), 2);
    }
}
