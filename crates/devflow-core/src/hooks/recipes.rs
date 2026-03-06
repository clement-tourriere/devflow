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
    vec![
        sync_ai_configs_recipe(),
        install_deps_recipe(),
        docker_compose_recipe(),
        local_dev_setup_recipe(),
        db_migrate_recipe(),
        multiplexer_session_recipe(),
    ]
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

fn install_deps_recipe() -> HookRecipe {
    let make_phase = || {
        let mut phase = IndexMap::new();
        for (name, condition, command) in [
            ("install-deps-npm", "file_exists:package-lock.json", "npm ci"),
            ("install-deps-bun", "file_exists:bun.lockb", "bun install --frozen-lockfile"),
            ("install-deps-pnpm", "file_exists:pnpm-lock.yaml", "pnpm install --frozen-lockfile"),
            ("install-deps-yarn", "file_exists:yarn.lock", "yarn install --frozen-lockfile"),
            ("install-deps-uv", "file_exists:uv.lock", "uv sync"),
            ("install-deps-cargo", "file_exists:Cargo.lock", "cargo build"),
        ] {
            phase.insert(
                name.to_string(),
                HookEntry::Extended(ExtendedHookEntry {
                    command: command.to_string(),
                    working_dir: None,
                    continue_on_error: Some(true),
                    condition: Some(condition.to_string()),
                    environment: None,
                    background: false,
                }),
            );
        }
        phase
    };

    let mut hooks = IndexMap::new();
    hooks.insert(HookPhase::PostCreate, make_phase());
    hooks.insert(HookPhase::PostSwitch, make_phase());

    HookRecipe {
        name: "install-deps",
        description: "Auto-detect and install dependencies (npm, bun, pnpm, yarn, uv, cargo)",
        category: "Setup",
        hooks,
    }
}

fn docker_compose_recipe() -> HookRecipe {
    let compose_files = [
        ("", "docker-compose.yml"),
        ("-v2", "compose.yml"),
        ("-yaml", "compose.yaml"),
    ];

    let make_hooks = |suffix: &str, command: &str| {
        let mut phase = IndexMap::new();
        for (variant, file) in &compose_files {
            phase.insert(
                format!("compose-{}{}", suffix, variant),
                HookEntry::Extended(ExtendedHookEntry {
                    command: command.to_string(),
                    working_dir: None,
                    continue_on_error: None,
                    condition: Some(format!("file_exists:{}", file)),
                    environment: None,
                    background: false,
                }),
            );
        }
        phase
    };

    let mut hooks = IndexMap::new();
    hooks.insert(HookPhase::PostCreate, make_hooks("up", "docker compose up -d"));
    hooks.insert(HookPhase::PostSwitch, make_hooks("restart", "docker compose up -d --build"));
    hooks.insert(HookPhase::PreRemove, make_hooks("down", "docker compose down"));

    HookRecipe {
        name: "docker-compose",
        description: "Manage Docker Compose lifecycle (up on create, restart on switch, down on remove)",
        category: "Docker",
        hooks,
    }
}

fn local_dev_setup_recipe() -> HookRecipe {
    use super::{ActionHookEntry, HookAction};

    let mut post_create = IndexMap::new();
    post_create.insert(
        "copy-env".to_string(),
        HookEntry::Action(ActionHookEntry {
            action: HookAction::Copy {
                from: ".env.example".to_string(),
                to: ".env.local".to_string(),
                overwrite: false,
            },
            working_dir: None,
            continue_on_error: None,
            condition: Some("file_exists:.env.example".to_string()),
            environment: None,
            background: false,
        }),
    );
    post_create.insert(
        "mise-trust".to_string(),
        HookEntry::Extended(ExtendedHookEntry {
            command: "mise trust".to_string(),
            working_dir: None,
            continue_on_error: None,
            condition: Some("file_exists:.mise.toml".to_string()),
            environment: None,
            background: false,
        }),
    );
    post_create.insert(
        "direnv-allow".to_string(),
        HookEntry::Extended(ExtendedHookEntry {
            command: "direnv allow".to_string(),
            working_dir: None,
            continue_on_error: None,
            condition: Some("file_exists:.envrc".to_string()),
            environment: None,
            background: false,
        }),
    );

    let mut hooks = IndexMap::new();
    hooks.insert(HookPhase::PostCreate, post_create);

    HookRecipe {
        name: "local-dev-setup",
        description: "Copy .env.example, trust mise, and allow direnv on workspace creation",
        category: "Setup",
        hooks,
    }
}

fn db_migrate_recipe() -> HookRecipe {
    let make_phase = || {
        let mut phase = IndexMap::new();
        for (name, condition, command) in [
            ("migrate-prisma", "file_exists:prisma/schema.prisma", "npx prisma migrate deploy"),
            ("migrate-rails", "dir_exists:db/migrate", "bin/rails db:migrate"),
            ("migrate-django", "file_exists:manage.py", "python manage.py migrate"),
        ] {
            phase.insert(
                name.to_string(),
                HookEntry::Extended(ExtendedHookEntry {
                    command: command.to_string(),
                    working_dir: None,
                    continue_on_error: Some(true),
                    condition: Some(condition.to_string()),
                    environment: None,
                    background: false,
                }),
            );
        }
        phase
    };

    let mut hooks = IndexMap::new();
    hooks.insert(HookPhase::PostCreate, make_phase());
    hooks.insert(HookPhase::PostSwitch, make_phase());

    HookRecipe {
        name: "db-migrate",
        description: "Run database migrations (Prisma, Rails, Django) after workspace creation and switch",
        category: "Database",
        hooks,
    }
}

fn multiplexer_session_recipe() -> HookRecipe {
    let mut post_create = IndexMap::new();
    post_create.insert(
        "open-session".to_string(),
        HookEntry::Extended(ExtendedHookEntry {
            command: "devflow switch --open {{ workspace }}".to_string(),
            working_dir: None,
            continue_on_error: Some(true),
            condition: None,
            environment: None,
            background: true,
        }),
    );

    let mut hooks = IndexMap::new();
    hooks.insert(HookPhase::PostCreate, post_create);

    HookRecipe {
        name: "multiplexer-session",
        description: "Auto-open a tmux/zellij session in the worktree after workspace creation",
        category: "Workflow",
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

    #[test]
    fn test_install_deps_recipe() {
        let recipe = install_deps_recipe();
        assert_eq!(recipe.name, "install-deps");
        assert_eq!(recipe.category, "Setup");
        assert_eq!(recipe.hooks.len(), 2); // post-create + post-switch
        assert!(recipe.hooks.contains_key(&HookPhase::PostCreate));
        assert!(recipe.hooks.contains_key(&HookPhase::PostSwitch));
        // 6 package managers per phase
        assert_eq!(recipe.hooks[&HookPhase::PostCreate].len(), 6);
        assert_eq!(recipe.hooks[&HookPhase::PostSwitch].len(), 6);
        // All should have conditions
        for (_phase, hooks) in &recipe.hooks {
            for (_name, entry) in hooks {
                match entry {
                    HookEntry::Extended(ext) => {
                        assert!(ext.condition.is_some());
                        assert_eq!(ext.continue_on_error, Some(true));
                    }
                    _ => panic!("Expected Extended entry"),
                }
            }
        }
    }

    #[test]
    fn test_docker_compose_recipe() {
        let recipe = docker_compose_recipe();
        assert_eq!(recipe.name, "docker-compose");
        assert_eq!(recipe.category, "Docker");
        assert_eq!(recipe.hooks.len(), 3); // post-create, post-switch, pre-remove
        assert!(recipe.hooks.contains_key(&HookPhase::PostCreate));
        assert!(recipe.hooks.contains_key(&HookPhase::PostSwitch));
        assert!(recipe.hooks.contains_key(&HookPhase::PreRemove));
        // 3 compose file variants per phase
        for (_phase, hooks) in &recipe.hooks {
            assert_eq!(hooks.len(), 3);
        }
        let info = recipe.to_info();
        assert_eq!(info.hooks_preview.len(), 9); // 3 phases * 3 variants
    }

    #[test]
    fn test_local_dev_setup_recipe() {
        let recipe = local_dev_setup_recipe();
        assert_eq!(recipe.name, "local-dev-setup");
        assert_eq!(recipe.category, "Setup");
        assert_eq!(recipe.hooks.len(), 1); // post-create only
        let post_create = &recipe.hooks[&HookPhase::PostCreate];
        assert_eq!(post_create.len(), 3); // copy-env, mise-trust, direnv-allow
        assert!(post_create.contains_key("copy-env"));
        assert!(post_create.contains_key("mise-trust"));
        assert!(post_create.contains_key("direnv-allow"));
        // copy-env should be an Action
        match &post_create["copy-env"] {
            HookEntry::Action(act) => {
                assert_eq!(act.action.type_name(), "copy");
                assert_eq!(act.condition.as_deref(), Some("file_exists:.env.example"));
            }
            _ => panic!("Expected Action entry for copy-env"),
        }
    }

    #[test]
    fn test_db_migrate_recipe() {
        let recipe = db_migrate_recipe();
        assert_eq!(recipe.name, "db-migrate");
        assert_eq!(recipe.category, "Database");
        assert_eq!(recipe.hooks.len(), 2); // post-create + post-switch
        for (_phase, hooks) in &recipe.hooks {
            assert_eq!(hooks.len(), 3); // prisma, rails, django
        }
        let info = recipe.to_info();
        assert_eq!(info.hooks_preview.len(), 6); // 2 phases * 3 frameworks
    }

    #[test]
    fn test_builtin_recipes_count() {
        let recipes = builtin_recipes();
        assert_eq!(recipes.len(), 6);
        let names: Vec<&str> = recipes.iter().map(|r| r.name).collect();
        assert!(names.contains(&"sync-ai-configs"));
        assert!(names.contains(&"install-deps"));
        assert!(names.contains(&"docker-compose"));
        assert!(names.contains(&"local-dev-setup"));
        assert!(names.contains(&"db-migrate"));
    }

    #[test]
    fn test_find_new_recipes() {
        assert!(find_recipe("install-deps").is_some());
        assert!(find_recipe("docker-compose").is_some());
        assert!(find_recipe("local-dev-setup").is_some());
        assert!(find_recipe("db-migrate").is_some());
    }
}
