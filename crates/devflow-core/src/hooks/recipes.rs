//! Intent-based hook recipes.
//!
//! A recipe pairs an intent (write service URLs into an env file, run the
//! project's migrations, ...) with install-time detection and parameters.
//! Detection probes the project — files on disk AND binaries actually
//! installed — so installing generates ONE lean hook set matching the real
//! stack, instead of a guarded variant per possible tool.
//!
//! Nothing about a recipe persists after install: the generated hooks are
//! plain entries in `.devflow.yml`, edited like any other hook. Generated
//! hook names are stable so re-installing dedupes (skip-if-exists) and
//! frontends can tell whether a recipe is already installed.

use anyhow::{bail, Context, Result};
use serde::Serialize;

use super::detect::DetectContext;
use super::{
    ActionHookEntry, EnvWriteMode, ExtendedHookEntry, HookAction, HookEntry, HookPhase,
    HooksConfig, IndexMap,
};

/// Recipe parameters travel as strings end-to-end (CLI `--param k=v`,
/// GUI JSON maps); `build()` parses them per [`ParamKind`].
pub type RecipeParams = IndexMap<String, String>;

/// The built-in recipes. Closed set — every variant implements
/// `detect`/`build` via exhaustive matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecipeId {
    EnvFile,
    PatchConfig,
    DbMigrate,
    InstallDeps,
    WorkspaceSetup,
    SyncAiConfigs,
    MultiplexerSession,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecipeMeta {
    pub name: &'static str,
    pub description: &'static str,
    pub category: &'static str,
    /// Phases the generated hooks target (stable metadata for UI filtering).
    pub phases: &'static [&'static str],
    /// Repeatable recipes can be installed several times with different
    /// params (e.g. patch-config per file) and never report "installed".
    pub repeatable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ParamKind {
    /// Single-line value.
    String,
    /// Multi-line value (one entry per line).
    Text,
    /// "true" / "false".
    Bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecipeParam {
    pub key: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub kind: ParamKind,
    /// Static fallback; detection may propose a better value.
    pub default: Option<&'static str>,
    pub required: bool,
}

/// What install-time detection found for a recipe in a given project.
#[derive(Debug, Clone, Serialize)]
pub struct RecipeDetection {
    /// Whether the recipe can be installed right now.
    pub applicable: bool,
    /// Whether detection found concrete evidence the project wants this.
    pub suggested: bool,
    /// Human-readable evidence lines.
    pub reasons: Vec<String>,
    /// Detection-derived parameter defaults.
    pub suggested_params: RecipeParams,
    /// Alternative values per param when several candidates were found.
    pub param_options: IndexMap<String, Vec<String>>,
}

/// Static recipe listing shape (usable outside a project).
#[derive(Debug, Clone, Serialize)]
pub struct RecipeInfo {
    pub name: String,
    pub description: String,
    pub category: String,
    pub phases: Vec<String>,
    pub repeatable: bool,
    pub params: Vec<RecipeParam>,
}

/// Serializable preview of generated hooks.
#[derive(Debug, Clone, Serialize)]
pub struct RecipeHookPreview {
    pub phase: String,
    pub hook_name: String,
    pub command_summary: String,
}

/// Recipe listing enriched with per-project detection results.
#[derive(Debug, Clone, Serialize)]
pub struct RecipeDetectionInfo {
    pub recipe: RecipeInfo,
    pub applicable: bool,
    pub suggested: bool,
    pub installed: bool,
    pub reasons: Vec<String>,
    pub suggested_params: RecipeParams,
    pub param_options: IndexMap<String, Vec<String>>,
    /// Hooks that would be generated from the suggested params
    /// (empty when suggestions don't satisfy the required params).
    pub hooks_preview: Vec<RecipeHookPreview>,
}

/// Result of installing a recipe.
#[derive(Debug, Clone, Serialize)]
pub struct InstallRecipeResult {
    pub hooks_added: usize,
    pub hooks_skipped: usize,
}

impl RecipeId {
    pub const ALL: [RecipeId; 7] = [
        RecipeId::EnvFile,
        RecipeId::PatchConfig,
        RecipeId::DbMigrate,
        RecipeId::InstallDeps,
        RecipeId::WorkspaceSetup,
        RecipeId::SyncAiConfigs,
        RecipeId::MultiplexerSession,
    ];

    pub fn name(self) -> &'static str {
        self.meta().name
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|id| id.name() == name)
    }

    pub fn meta(self) -> RecipeMeta {
        match self {
            RecipeId::EnvFile => RecipeMeta {
                name: "env-file",
                description: "Write per-workspace service URLs into an env file (merged — your other keys are kept)",
                category: "Environment",
                phases: &["post-create", "post-switch"],
                repeatable: false,
            },
            RecipeId::PatchConfig => RecipeMeta {
                name: "patch-config",
                description: "Find-and-replace a value in any config file after create/switch",
                category: "Environment",
                phases: &["post-create", "post-switch"],
                repeatable: true,
            },
            RecipeId::DbMigrate => RecipeMeta {
                name: "db-migrate",
                description: "Run database migrations after workspace creation and switch",
                category: "Database",
                phases: &["post-create", "post-switch"],
                repeatable: false,
            },
            RecipeId::InstallDeps => RecipeMeta {
                name: "install-deps",
                description: "Install dependencies with the package manager this project actually uses",
                category: "Setup",
                phases: &["post-create", "post-switch"],
                repeatable: false,
            },
            RecipeId::WorkspaceSetup => RecipeMeta {
                name: "workspace-setup",
                description: "Set up new worktrees: copy .env.example, trust mise, allow direnv",
                category: "Setup",
                phases: &["post-create"],
                repeatable: false,
            },
            RecipeId::SyncAiConfigs => RecipeMeta {
                name: "sync-ai-configs",
                description: "Sync AI tool configs (.claude, .cursor, etc.) back to the main worktree before workspace removal",
                category: "AI Tools",
                phases: &["pre-remove"],
                repeatable: false,
            },
            RecipeId::MultiplexerSession => RecipeMeta {
                name: "multiplexer-session",
                description: "Auto-open a tmux/zellij session in the worktree after workspace creation",
                category: "Workflow",
                phases: &["post-create"],
                repeatable: false,
            },
        }
    }

    pub fn params(self) -> Vec<RecipeParam> {
        match self {
            RecipeId::EnvFile => vec![
                RecipeParam {
                    key: "file",
                    label: "Env file",
                    help: "Path relative to the worktree",
                    kind: ParamKind::String,
                    default: Some(".env.local"),
                    required: true,
                },
                RecipeParam {
                    key: "vars",
                    label: "Variables",
                    help: "One KEY=VALUE per line; values are MiniJinja templates (e.g. {{ service['db'].url }})",
                    kind: ParamKind::Text,
                    default: None,
                    required: true,
                },
            ],
            RecipeId::PatchConfig => vec![
                RecipeParam {
                    key: "file",
                    label: "File",
                    help: "Config file to patch, relative to the worktree",
                    kind: ParamKind::String,
                    default: None,
                    required: true,
                },
                RecipeParam {
                    key: "pattern",
                    label: "Pattern",
                    help: "Text (or regex) to find",
                    kind: ParamKind::String,
                    default: None,
                    required: true,
                },
                RecipeParam {
                    key: "replacement",
                    label: "Replacement",
                    help: "Replacement text; MiniJinja templates allowed (e.g. {{ service['db'].url }})",
                    kind: ParamKind::String,
                    default: None,
                    required: true,
                },
                RecipeParam {
                    key: "regex",
                    label: "Regex",
                    help: "Treat the pattern as a regular expression",
                    kind: ParamKind::Bool,
                    default: Some("false"),
                    required: false,
                },
            ],
            RecipeId::DbMigrate => vec![RecipeParam {
                key: "command",
                label: "Migration command",
                help: "Shell command that applies migrations in the worktree",
                kind: ParamKind::String,
                default: None,
                required: true,
            }],
            RecipeId::InstallDeps => vec![RecipeParam {
                key: "command",
                label: "Install command",
                help: "Shell command that installs dependencies in the worktree",
                kind: ParamKind::String,
                default: None,
                required: true,
            }],
            RecipeId::WorkspaceSetup => vec![
                RecipeParam {
                    key: "copy-env",
                    label: "Copy .env.example → .env.local",
                    help: "Copy the example env file into new worktrees (never overwrites)",
                    kind: ParamKind::Bool,
                    default: Some("false"),
                    required: false,
                },
                RecipeParam {
                    key: "mise-trust",
                    label: "Run mise trust",
                    help: "Trust the worktree's mise config",
                    kind: ParamKind::Bool,
                    default: Some("false"),
                    required: false,
                },
                RecipeParam {
                    key: "direnv-allow",
                    label: "Run direnv allow",
                    help: "Allow the worktree's .envrc",
                    kind: ParamKind::Bool,
                    default: Some("false"),
                    required: false,
                },
            ],
            RecipeId::SyncAiConfigs | RecipeId::MultiplexerSession => vec![],
        }
    }

    /// Stable (phase, hook-name) markers used to tell whether this recipe is
    /// already installed, independent of the params it was installed with.
    fn marker_hooks(self) -> &'static [(&'static str, &'static str)] {
        match self {
            RecipeId::EnvFile => &[("post-create", "env-file")],
            RecipeId::PatchConfig => &[],
            RecipeId::DbMigrate => &[("post-create", "db-migrate")],
            RecipeId::InstallDeps => &[("post-create", "install-deps")],
            RecipeId::WorkspaceSetup => &[
                ("post-create", "copy-env"),
                ("post-create", "mise-trust"),
                ("post-create", "direnv-allow"),
            ],
            RecipeId::SyncAiConfigs => &[("pre-remove", "sync-ai-configs")],
            RecipeId::MultiplexerSession => &[("post-create", "open-session")],
        }
    }

    /// Probe the project for evidence and parameter suggestions.
    pub fn detect(self, ctx: &DetectContext) -> RecipeDetection {
        match self {
            RecipeId::EnvFile => detect_env_file(ctx),
            RecipeId::PatchConfig => detect_patch_config(),
            RecipeId::DbMigrate => detect_db_migrate(ctx),
            RecipeId::InstallDeps => detect_install_deps(ctx),
            RecipeId::WorkspaceSetup => detect_workspace_setup(ctx),
            RecipeId::SyncAiConfigs => detect_sync_ai_configs(ctx),
            RecipeId::MultiplexerSession => detect_multiplexer_session(ctx),
        }
    }

    /// Generate the hooks for the given resolved params. Pure function of
    /// the params — detection only influences the values passed in.
    pub fn build(self, params: &RecipeParams) -> Result<HooksConfig> {
        match self {
            RecipeId::EnvFile => build_env_file(params),
            RecipeId::PatchConfig => build_patch_config(params),
            RecipeId::DbMigrate => build_command_recipe("db-migrate", params),
            RecipeId::InstallDeps => build_command_recipe("install-deps", params),
            RecipeId::WorkspaceSetup => build_workspace_setup(params),
            RecipeId::SyncAiConfigs => Ok(build_sync_ai_configs()),
            RecipeId::MultiplexerSession => Ok(build_multiplexer_session()),
        }
    }

    pub fn to_info(self) -> RecipeInfo {
        let meta = self.meta();
        RecipeInfo {
            name: meta.name.to_string(),
            description: meta.description.to_string(),
            category: meta.category.to_string(),
            phases: meta.phases.iter().map(|p| p.to_string()).collect(),
            repeatable: meta.repeatable,
            params: self.params(),
        }
    }

    /// Detection + installed-state + preview, for frontends.
    pub fn detect_info(
        self,
        ctx: &DetectContext,
        existing: Option<&HooksConfig>,
    ) -> RecipeDetectionInfo {
        let detection = self.detect(ctx);

        let hooks_preview = if detection.applicable {
            resolve_params(self, Some(&detection), &RecipeParams::new())
                .and_then(|params| self.build(&params))
                .map(|hooks| hooks_preview_of(&hooks))
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let installed = existing.is_some_and(|hooks| {
            self.marker_hooks().iter().any(|(phase, name)| {
                let phase: HookPhase = phase.parse().expect("HookPhase::from_str is infallible");
                hooks
                    .get(&phase)
                    .is_some_and(|phase_hooks| phase_hooks.contains_key(*name))
            })
        });

        RecipeDetectionInfo {
            recipe: self.to_info(),
            applicable: detection.applicable,
            suggested: detection.suggested,
            installed,
            reasons: detection.reasons,
            suggested_params: detection.suggested_params,
            param_options: detection.param_options,
            hooks_preview,
        }
    }
}

/// Message for recipes that no longer exist under their old name.
pub fn removed_recipe_message(name: &str) -> Option<String> {
    match name {
        "docker-compose" => Some(
            "The docker-compose recipe was removed. devflow manages app processes directly: \
             run `devflow init` to import compose app services as process daemons \
             (processes.daemons); database/cache containers are handled by devflow services. \
             See `devflow process --help`."
                .to_string(),
        ),
        "local-dev-setup" => Some(
            "The local-dev-setup recipe was renamed to `workspace-setup` and now detects \
             mise/direnv/.env.example before installing. Run: devflow hook install workspace-setup"
                .to_string(),
        ),
        _ => None,
    }
}

/// Layer parameter values: static defaults ← detection suggestions ← user.
///
/// Errors on unknown keys (catches `--param` typos), missing required
/// params, and Bool params that aren't "true"/"false".
pub fn resolve_params(
    id: RecipeId,
    detection: Option<&RecipeDetection>,
    user: &RecipeParams,
) -> Result<RecipeParams> {
    let declared = id.params();

    for key in user.keys() {
        if !declared.iter().any(|p| p.key == key) {
            let expected = declared.iter().map(|p| p.key).collect::<Vec<_>>();
            if expected.is_empty() {
                bail!("recipe '{}' takes no parameters", id.name());
            }
            bail!(
                "unknown parameter '{}' for recipe '{}' (expected: {})",
                key,
                id.name(),
                expected.join(", ")
            );
        }
    }

    let mut resolved = RecipeParams::new();
    for param in &declared {
        let value = user
            .get(param.key)
            .cloned()
            .or_else(|| detection.and_then(|d| d.suggested_params.get(param.key).cloned()))
            .or_else(|| param.default.map(String::from));

        match value {
            Some(value) => {
                if param.kind == ParamKind::Bool && value != "true" && value != "false" {
                    bail!(
                        "parameter '{}' must be true or false, got '{}'",
                        param.key,
                        value
                    );
                }
                if param.required && value.trim().is_empty() {
                    bail!(
                        "parameter '{}' is required for recipe '{}'",
                        param.key,
                        id.name()
                    );
                }
                resolved.insert(param.key.to_string(), value);
            }
            None if param.required => bail!(
                "missing required parameter '{}' for recipe '{}' (pass --param {}=...)",
                param.key,
                id.name(),
                param.key
            ),
            None => {}
        }
    }
    Ok(resolved)
}

/// Merge generated hooks into an existing hooks config.
///
/// Only adds hooks that don't already exist (by phase + name).
pub fn merge_hooks_into_config(
    existing: &mut HooksConfig,
    generated: &HooksConfig,
) -> InstallRecipeResult {
    let mut added = 0;
    let mut skipped = 0;

    for (phase, generated_hooks) in generated {
        let phase_map = existing.entry(phase.clone()).or_default();
        for (name, entry) in generated_hooks {
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

/// Flatten a hooks config into human-readable previews.
pub fn hooks_preview_of(hooks: &HooksConfig) -> Vec<RecipeHookPreview> {
    let mut preview = Vec::new();
    for (phase, phase_hooks) in hooks {
        for (name, entry) in phase_hooks {
            let summary = match entry {
                HookEntry::Simple(cmd) => cmd.clone(),
                HookEntry::Extended(ext) => ext.command.clone(),
                HookEntry::Action(act) => action_summary(&act.action),
            };
            preview.push(RecipeHookPreview {
                phase: phase.to_string(),
                hook_name: name.clone(),
                command_summary: summary,
            });
        }
    }
    preview
}

fn action_summary(action: &HookAction) -> String {
    match action {
        HookAction::WriteEnv { path, vars, .. } => {
            format!("write-env → {} ({} vars, merged)", path, vars.len())
        }
        HookAction::Copy { from, to, .. } => format!("copy {} → {}", from, to),
        HookAction::Replace { file, .. } => format!("replace in {}", file),
        other => format!("action: {}", other.type_name()),
    }
}

// ── Entry builders ────────────────────────────────────────────────────

fn shell_hook(
    command: impl Into<String>,
    condition: Option<String>,
    background: bool,
) -> HookEntry {
    HookEntry::Extended(ExtendedHookEntry {
        command: command.into(),
        working_dir: None,
        continue_on_error: Some(true),
        condition,
        environment: None,
        background,
    })
}

fn action_hook(action: HookAction, condition: Option<String>) -> HookEntry {
    HookEntry::Action(ActionHookEntry {
        action,
        working_dir: None,
        continue_on_error: None,
        condition,
        environment: None,
        background: false,
    })
}

fn create_and_switch_hooks(name: &str, entry: HookEntry) -> HooksConfig {
    let mut hooks: HooksConfig = IndexMap::new();
    for phase in [HookPhase::PostCreate, HookPhase::PostSwitch] {
        hooks
            .entry(phase)
            .or_default()
            .insert(name.to_string(), entry.clone());
    }
    hooks
}

fn required_param<'p>(params: &'p RecipeParams, key: &str) -> Result<&'p str> {
    params
        .get(key)
        .map(String::as_str)
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing required parameter '{}'", key))
}

fn bool_param(params: &RecipeParams, key: &str) -> Result<bool> {
    match params.get(key).map(String::as_str) {
        None | Some("") | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(other) => bail!("parameter '{}' must be true or false, got '{}'", key, other),
    }
}

// ── env-file ──────────────────────────────────────────────────────────

fn detect_env_file(ctx: &DetectContext) -> RecipeDetection {
    let services = ctx.config.services.as_deref().unwrap_or_default();
    if services.is_empty() {
        return RecipeDetection {
            applicable: false,
            suggested: false,
            reasons: vec![
                "no services configured — add one with `devflow init` or `devflow service add`"
                    .to_string(),
            ],
            suggested_params: RecipeParams::new(),
            param_options: IndexMap::new(),
        };
    }

    let mut reasons = Vec::new();
    let mut suggested_params = RecipeParams::new();

    let file = ctx.first_existing_file(&[".env.local", ".env", ".env.development"]);
    if let Some(existing) = &file {
        reasons.push(format!("found existing {}", existing));
    } else if ctx.file_exists(".env.example") {
        reasons.push(".env.example present — writing to .env.local".to_string());
    }
    suggested_params.insert(
        "file".to_string(),
        file.unwrap_or_else(|| ".env.local".to_string()),
    );

    // Key per service: the default service gets DATABASE_URL, well-known
    // types get their conventional key, everything else {NAME}_URL.
    let db_types = ["postgres", "mysql", "mariadb"];
    let default_name = services
        .iter()
        .find(|s| s.default)
        .map(|s| s.name.as_str())
        .or_else(|| {
            let mut dbs = services
                .iter()
                .filter(|s| db_types.contains(&s.service_type.as_str()));
            match (dbs.next(), dbs.next()) {
                (Some(only), None) => Some(only.name.as_str()),
                _ => None,
            }
        });

    let mut used_keys: Vec<String> = Vec::new();
    let mut lines = Vec::new();
    for service in services {
        let preferred = if Some(service.name.as_str()) == default_name {
            "DATABASE_URL".to_string()
        } else {
            match service.service_type.as_str() {
                "redis" => "REDIS_URL".to_string(),
                "clickhouse" => "CLICKHOUSE_URL".to_string(),
                _ => env_key_for(&service.name),
            }
        };
        let key = if used_keys.contains(&preferred) {
            env_key_for(&service.name)
        } else {
            preferred
        };
        reasons.push(format!(
            "service '{}' ({}) → {}",
            service.name, service.service_type, key
        ));
        lines.push(format!("{}={{{{ service['{}'].url }}}}", key, service.name));
        used_keys.push(key);
    }
    suggested_params.insert("vars".to_string(), lines.join("\n"));

    RecipeDetection {
        applicable: true,
        suggested: true,
        reasons,
        suggested_params,
        param_options: IndexMap::new(),
    }
}

/// "app-db" → "APP_DB_URL"
fn env_key_for(service_name: &str) -> String {
    let mut key = String::new();
    let mut last_underscore = true; // trims leading separators
    for c in service_name.chars() {
        if c.is_ascii_alphanumeric() {
            key.push(c.to_ascii_uppercase());
            last_underscore = false;
        } else if !last_underscore {
            key.push('_');
            last_underscore = true;
        }
    }
    let key = key.trim_end_matches('_');
    if key.is_empty() {
        "SERVICE_URL".to_string()
    } else {
        format!("{}_URL", key)
    }
}

fn build_env_file(params: &RecipeParams) -> Result<HooksConfig> {
    let file = required_param(params, "file")?;
    let vars = parse_env_vars(required_param(params, "vars")?)?;

    Ok(create_and_switch_hooks(
        "env-file",
        action_hook(
            HookAction::WriteEnv {
                path: file.to_string(),
                vars,
                mode: EnvWriteMode::Merge,
            },
            None,
        ),
    ))
}

/// Parse newline-separated (or literal `\n`-separated) KEY=VALUE lines.
fn parse_env_vars(raw: &str) -> Result<IndexMap<String, String>> {
    let mut vars = IndexMap::new();
    for line in raw.replace("\\n", "\n").lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .with_context(|| format!("invalid vars line '{}' (expected KEY=VALUE)", line))?;
        vars.insert(key.trim().to_string(), value.trim().to_string());
    }
    if vars.is_empty() {
        bail!("env-file: no variables given");
    }
    Ok(vars)
}

// ── patch-config ──────────────────────────────────────────────────────

fn detect_patch_config() -> RecipeDetection {
    RecipeDetection {
        applicable: true,
        suggested: false,
        reasons: vec![
            "generic — point it at any config file the workspace needs patched".to_string(),
        ],
        suggested_params: RecipeParams::new(),
        param_options: IndexMap::new(),
    }
}

fn build_patch_config(params: &RecipeParams) -> Result<HooksConfig> {
    let file = required_param(params, "file")?;
    let pattern = required_param(params, "pattern")?;
    let replacement = required_param(params, "replacement")?;
    let regex = bool_param(params, "regex")?;

    // Deterministic per-file hook name: repeat installs for the same file
    // dedupe, different files coexist.
    let hook_name = format!("patch-{}", slugify(file));
    Ok(create_and_switch_hooks(
        &hook_name,
        action_hook(
            HookAction::Replace {
                file: file.to_string(),
                pattern: pattern.to_string(),
                replacement: replacement.to_string(),
                regex,
                create_if_missing: false,
            },
            Some(format!("file_exists:{}", file)),
        ),
    ))
}

fn slugify(value: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = true; // trims leading separators
    for c in value.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "file".to_string()
    } else {
        slug.to_string()
    }
}

// ── db-migrate ────────────────────────────────────────────────────────

fn detect_db_migrate(ctx: &DetectContext) -> RecipeDetection {
    let mut candidates: Vec<(String, String)> = Vec::new(); // (command, evidence)

    if ctx.file_exists("prisma/schema.prisma") {
        candidates.push((
            format!("{} prisma migrate deploy", js_runner(ctx)),
            "prisma/schema.prisma found".to_string(),
        ));
    }
    if ctx.dir_exists("db/migrate") {
        candidates.push((
            "bin/rails db:migrate".to_string(),
            "db/migrate/ found (Rails)".to_string(),
        ));
    }
    if ctx.file_exists("manage.py") {
        candidates.push((
            uv_aware(ctx, "python manage.py migrate"),
            "manage.py found (Django)".to_string(),
        ));
    }
    if ctx.file_exists("alembic.ini") {
        candidates.push((
            uv_aware(ctx, "alembic upgrade head"),
            "alembic.ini found".to_string(),
        ));
    }
    if ctx.dir_exists("migrations") && ctx.binary_exists("sqlx") {
        candidates.push((
            "sqlx migrate run".to_string(),
            "migrations/ found and sqlx installed".to_string(),
        ));
    }
    if ctx.file_exists("diesel.toml") {
        candidates.push((
            "diesel migration run".to_string(),
            "diesel.toml found".to_string(),
        ));
    }
    if ctx.dir_exists("db/migrations") {
        candidates.push((
            "dbmate up".to_string(),
            "db/migrations/ found (dbmate)".to_string(),
        ));
    }

    let mut reasons: Vec<String> = candidates
        .iter()
        .map(|(cmd, why)| format!("{} → {}", why, cmd))
        .collect();
    let mut suggested_params = RecipeParams::new();
    let mut param_options = IndexMap::new();
    if let Some((first, _)) = candidates.first() {
        suggested_params.insert("command".to_string(), first.clone());
    }
    if candidates.len() > 1 {
        param_options.insert(
            "command".to_string(),
            candidates.iter().map(|(cmd, _)| cmd.clone()).collect(),
        );
    }

    let suggested = !candidates.is_empty();
    if !suggested {
        reasons.push(
            "no migration tool detected (prisma, rails, django, alembic, sqlx, diesel, dbmate) — a custom command works too"
                .to_string(),
        );
    }
    RecipeDetection {
        // Installable with a custom command even when nothing was detected.
        applicable: true,
        suggested,
        reasons,
        suggested_params,
        param_options,
    }
}

/// Runner for JS devDependency CLIs, picked from the project's lockfile.
/// Each resolves the project-local binary first (no ad-hoc downloads).
fn js_runner(ctx: &DetectContext) -> &'static str {
    if ctx.file_exists("bun.lock") || ctx.file_exists("bun.lockb") {
        "bunx"
    } else if ctx.file_exists("pnpm-lock.yaml") {
        "pnpm exec"
    } else if ctx.file_exists("yarn.lock") {
        "yarn"
    } else {
        "npx"
    }
}

fn uv_aware(ctx: &DetectContext, command: &str) -> String {
    if ctx.file_exists("uv.lock") {
        format!("uv run {}", command)
    } else {
        command.to_string()
    }
}

fn build_command_recipe(hook_name: &str, params: &RecipeParams) -> Result<HooksConfig> {
    let command = required_param(params, "command")?;
    Ok(create_and_switch_hooks(
        hook_name,
        shell_hook(command, None, false),
    ))
}

// ── install-deps ──────────────────────────────────────────────────────

fn detect_install_deps(ctx: &DetectContext) -> RecipeDetection {
    let mut reasons = Vec::new();
    let mut options: Vec<String> = Vec::new();
    let mut ecosystem_winners: Vec<String> = Vec::new();

    // JS: tool-specific lockfiles beat a lingering package-lock.json.
    let js_managers: [(&[&str], &str, &str); 4] = [
        (
            &["bun.lock", "bun.lockb"],
            "bun",
            "bun install --frozen-lockfile",
        ),
        (
            &["pnpm-lock.yaml"],
            "pnpm",
            "pnpm install --frozen-lockfile",
        ),
        (&["yarn.lock"], "yarn", "yarn install --frozen-lockfile"),
        (&["package-lock.json"], "npm", "npm ci"),
    ];
    let mut js_winner: Option<&str> = None;
    for (lockfiles, bin, command) in js_managers {
        let Some(lockfile) = lockfiles.iter().find(|f| ctx.file_exists(f)) else {
            continue;
        };
        if ctx.binary_exists(bin) {
            reasons.push(format!(
                "{} found and {} installed → {}",
                lockfile, bin, command
            ));
            options.push(command.to_string());
            js_winner.get_or_insert(command);
        } else {
            reasons.push(format!(
                "{} found but {} is not installed — skipped",
                lockfile, bin
            ));
        }
    }
    if let Some(command) = js_winner {
        ecosystem_winners.push(command.to_string());
    }

    for (lockfile, bin, command) in [
        ("uv.lock", "uv", "uv sync"),
        ("Cargo.lock", "cargo", "cargo build"),
    ] {
        if !ctx.file_exists(lockfile) {
            continue;
        }
        if ctx.binary_exists(bin) {
            reasons.push(format!(
                "{} found and {} installed → {}",
                lockfile, bin, command
            ));
            options.push(command.to_string());
            ecosystem_winners.push(command.to_string());
        } else {
            reasons.push(format!(
                "{} found but {} is not installed — skipped",
                lockfile, bin
            ));
        }
    }

    let mut suggested_params = RecipeParams::new();
    let mut param_options = IndexMap::new();
    if !ecosystem_winners.is_empty() {
        suggested_params.insert("command".to_string(), ecosystem_winners.join(" && "));
    }
    if options.len() > 1 {
        param_options.insert("command".to_string(), options);
    }

    let suggested = !ecosystem_winners.is_empty();
    if !suggested {
        reasons.push("no known lockfile with an installed package manager found".to_string());
    }
    RecipeDetection {
        // Installable with a custom command even when nothing was detected.
        applicable: true,
        suggested,
        reasons,
        suggested_params,
        param_options,
    }
}

// ── workspace-setup ───────────────────────────────────────────────────

fn detect_workspace_setup(ctx: &DetectContext) -> RecipeDetection {
    let mut reasons = Vec::new();
    let mut suggested_params = RecipeParams::new();

    if ctx.file_exists(".env.example") {
        suggested_params.insert("copy-env".to_string(), "true".to_string());
        reasons.push(".env.example found → copy to .env.local".to_string());
    }
    if let Some(mise_config) =
        ctx.first_existing_file(&["mise.toml", ".mise.toml", ".config/mise/config.toml"])
    {
        if ctx.binary_exists("mise") {
            suggested_params.insert("mise-trust".to_string(), "true".to_string());
            reasons.push(format!(
                "{} found and mise installed → mise trust",
                mise_config
            ));
        } else {
            reasons.push(format!(
                "{} found but mise is not installed — skipped",
                mise_config
            ));
        }
    }
    if ctx.file_exists(".envrc") {
        if ctx.binary_exists("direnv") {
            suggested_params.insert("direnv-allow".to_string(), "true".to_string());
            reasons.push(".envrc found and direnv installed → direnv allow".to_string());
        } else {
            reasons.push(".envrc found but direnv is not installed — skipped".to_string());
        }
    }

    let suggested = !suggested_params.is_empty();
    if !suggested {
        reasons.push("nothing to set up (no .env.example, mise config, or .envrc)".to_string());
    }
    RecipeDetection {
        applicable: suggested,
        suggested,
        reasons,
        suggested_params,
        param_options: IndexMap::new(),
    }
}

fn build_workspace_setup(params: &RecipeParams) -> Result<HooksConfig> {
    let copy_env = bool_param(params, "copy-env")?;
    let mise_trust = bool_param(params, "mise-trust")?;
    let direnv_allow = bool_param(params, "direnv-allow")?;
    if !copy_env && !mise_trust && !direnv_allow {
        bail!("workspace-setup: enable at least one of copy-env, mise-trust, direnv-allow");
    }

    let mut phase_hooks = IndexMap::new();
    if copy_env {
        phase_hooks.insert(
            "copy-env".to_string(),
            action_hook(
                HookAction::Copy {
                    from: ".env.example".to_string(),
                    to: ".env.local".to_string(),
                    overwrite: false,
                },
                Some("file_exists:.env.example".to_string()),
            ),
        );
    }
    if mise_trust {
        // command_exists guards teammates without the tool; the config file
        // was verified at install time and tolerates old branches.
        phase_hooks.insert(
            "mise-trust".to_string(),
            shell_hook("mise trust", Some("command_exists:mise".to_string()), false),
        );
    }
    if direnv_allow {
        phase_hooks.insert(
            "direnv-allow".to_string(),
            shell_hook(
                "direnv allow",
                Some("command_exists:direnv".to_string()),
                false,
            ),
        );
    }

    let mut hooks: HooksConfig = IndexMap::new();
    hooks.insert(HookPhase::PostCreate, phase_hooks);
    Ok(hooks)
}

// ── sync-ai-configs ───────────────────────────────────────────────────

fn detect_sync_ai_configs(ctx: &DetectContext) -> RecipeDetection {
    let found: Vec<&str> = crate::config::AI_TOOL_DIRS
        .iter()
        .copied()
        .filter(|dir| ctx.dir_exists(dir))
        .collect();
    let suggested = !found.is_empty();
    let reasons = if suggested {
        vec![format!("AI tool dirs found: {}", found.join(", "))]
    } else {
        vec!["no AI tool dirs found yet — still safe to install".to_string()]
    };
    RecipeDetection {
        applicable: true,
        suggested,
        reasons,
        suggested_params: RecipeParams::new(),
        param_options: IndexMap::new(),
    }
}

fn build_sync_ai_configs() -> HooksConfig {
    let mut pre_remove = IndexMap::new();
    pre_remove.insert(
        "sync-ai-configs".to_string(),
        shell_hook("devflow sync-ai-configs", None, false),
    );
    let mut hooks: HooksConfig = IndexMap::new();
    hooks.insert(HookPhase::PreRemove, pre_remove);
    hooks
}

// ── multiplexer-session ───────────────────────────────────────────────

fn detect_multiplexer_session(ctx: &DetectContext) -> RecipeDetection {
    let mut reasons = Vec::new();
    if ctx.binary_exists("tmux") {
        reasons.push("tmux installed".to_string());
    }
    if ctx.binary_exists("zellij") {
        reasons.push("zellij installed".to_string());
    }
    if let Some(execute) = &ctx.config.execute {
        if execute.multiplexer.is_some() || execute.detach_command.is_some() {
            reasons.push("execute multiplexer/detach_command configured".to_string());
        }
    }
    let suggested = !reasons.is_empty();
    if !suggested {
        reasons
            .push("no tmux/zellij installed and no execute.detach_command configured".to_string());
    }
    RecipeDetection {
        applicable: suggested,
        suggested,
        reasons,
        suggested_params: RecipeParams::new(),
        param_options: IndexMap::new(),
    }
}

fn build_multiplexer_session() -> HooksConfig {
    let mut post_create = IndexMap::new();
    post_create.insert(
        "open-session".to_string(),
        shell_hook("devflow switch --open {{ workspace }}", None, true),
    );
    let mut hooks: HooksConfig = IndexMap::new();
    hooks.insert(HookPhase::PostCreate, post_create);
    hooks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn config_from_yaml(yaml: &str) -> Config {
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    fn params(pairs: &[(&str, &str)]) -> RecipeParams {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn no_binaries<'a>(config: &'a Config, root: &'a std::path::Path) -> DetectContext<'a> {
        DetectContext::with_binary_checker(config, root, |_| false)
    }

    #[test]
    fn test_all_recipes_unique_and_resolvable() {
        assert_eq!(RecipeId::ALL.len(), 7);
        let names: Vec<&str> = RecipeId::ALL.iter().map(|id| id.name()).collect();
        for expected in [
            "env-file",
            "patch-config",
            "db-migrate",
            "install-deps",
            "workspace-setup",
            "sync-ai-configs",
            "multiplexer-session",
        ] {
            assert!(names.contains(&expected), "missing recipe {}", expected);
            assert_eq!(RecipeId::from_name(expected).unwrap().name(), expected);
        }
        assert!(RecipeId::from_name("docker-compose").is_none());
    }

    #[test]
    fn test_removed_recipe_messages() {
        assert!(removed_recipe_message("docker-compose")
            .unwrap()
            .contains("processes"));
        assert!(removed_recipe_message("local-dev-setup")
            .unwrap()
            .contains("workspace-setup"));
        assert!(removed_recipe_message("env-file").is_none());
    }

    #[test]
    fn test_merge_adds_and_skips() {
        let generated = RecipeId::SyncAiConfigs.build(&RecipeParams::new()).unwrap();
        let mut existing: HooksConfig = IndexMap::new();

        let first = merge_hooks_into_config(&mut existing, &generated);
        assert_eq!(first.hooks_added, 1);
        assert_eq!(first.hooks_skipped, 0);

        let second = merge_hooks_into_config(&mut existing, &generated);
        assert_eq!(second.hooks_added, 0);
        assert_eq!(second.hooks_skipped, 1);
    }

    #[test]
    fn test_env_file_detection_maps_service_keys() {
        let config = config_from_yaml(
            r#"
services:
  - name: app-db
    service_type: postgres
    default: true
  - name: cache
    service_type: redis
  - name: analytics
    service_type: clickhouse
  - name: storage
    service_type: rustfs
"#,
        );
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".env"), "").unwrap();

        let detection = RecipeId::EnvFile.detect(&no_binaries(&config, tmp.path()));
        assert!(detection.applicable);
        assert!(detection.suggested);
        assert_eq!(detection.suggested_params.get("file").unwrap(), ".env");
        let vars = detection.suggested_params.get("vars").unwrap();
        assert!(vars.contains("DATABASE_URL={{ service['app-db'].url }}"));
        assert!(vars.contains("REDIS_URL={{ service['cache'].url }}"));
        assert!(vars.contains("CLICKHOUSE_URL={{ service['analytics'].url }}"));
        assert!(vars.contains("STORAGE_URL={{ service['storage'].url }}"));
    }

    #[test]
    fn test_env_file_sole_db_service_becomes_default() {
        let config = config_from_yaml("services:\n  - name: pg\n    service_type: postgres\n");
        let tmp = tempfile::tempdir().unwrap();
        let detection = RecipeId::EnvFile.detect(&no_binaries(&config, tmp.path()));
        assert_eq!(
            detection.suggested_params.get("file").unwrap(),
            ".env.local"
        );
        assert_eq!(
            detection.suggested_params.get("vars").unwrap(),
            "DATABASE_URL={{ service['pg'].url }}"
        );
    }

    #[test]
    fn test_env_file_not_applicable_without_services() {
        let config = Config::default();
        let tmp = tempfile::tempdir().unwrap();
        let detection = RecipeId::EnvFile.detect(&no_binaries(&config, tmp.path()));
        assert!(!detection.applicable);
        assert!(!detection.suggested);
    }

    #[test]
    fn test_env_file_build() {
        let built = RecipeId::EnvFile
            .build(&params(&[
                ("file", ".env.local"),
                (
                    "vars",
                    "DATABASE_URL={{ service['db'].url }}\n# comment\n\nREDIS_URL=x",
                ),
            ]))
            .unwrap();

        for phase in [HookPhase::PostCreate, HookPhase::PostSwitch] {
            let entry = built.get(&phase).unwrap().get("env-file").unwrap();
            let HookEntry::Action(action_entry) = entry else {
                panic!("expected action hook");
            };
            let HookAction::WriteEnv { path, vars, mode } = &action_entry.action else {
                panic!("expected write-env action");
            };
            assert_eq!(path, ".env.local");
            assert_eq!(vars.len(), 2);
            assert_eq!(vars.get("DATABASE_URL").unwrap(), "{{ service['db'].url }}");
            assert!(matches!(mode, EnvWriteMode::Merge));
        }
    }

    #[test]
    fn test_env_file_vars_parsing_errors() {
        // literal \n separators (CLI --param) are accepted
        let built = RecipeId::EnvFile
            .build(&params(&[("file", ".env"), ("vars", "A=1\\nB=2")]))
            .unwrap();
        let entry = built
            .get(&HookPhase::PostCreate)
            .unwrap()
            .get("env-file")
            .unwrap();
        let HookEntry::Action(action_entry) = entry else {
            panic!("expected action hook");
        };
        let HookAction::WriteEnv { vars, .. } = &action_entry.action else {
            panic!("expected write-env action");
        };
        assert_eq!(vars.len(), 2);

        // invalid line and empty vars are rejected
        assert!(RecipeId::EnvFile
            .build(&params(&[("file", ".env"), ("vars", "NOT A PAIR")]))
            .is_err());
        assert!(RecipeId::EnvFile
            .build(&params(&[("file", ".env"), ("vars", "# only comments")]))
            .is_err());
    }

    #[test]
    fn test_patch_config_build() {
        let built = RecipeId::PatchConfig
            .build(&params(&[
                ("file", "config/settings.py"),
                ("pattern", "DB_HOST = .*"),
                ("replacement", "DB_HOST = '{{ service['db'].host }}'"),
                ("regex", "true"),
            ]))
            .unwrap();

        for phase in [HookPhase::PostCreate, HookPhase::PostSwitch] {
            let entry = built
                .get(&phase)
                .unwrap()
                .get("patch-config-settings-py")
                .unwrap();
            let HookEntry::Action(action_entry) = entry else {
                panic!("expected action hook");
            };
            assert_eq!(
                action_entry.condition.as_deref(),
                Some("file_exists:config/settings.py")
            );
            let HookAction::Replace {
                file,
                regex,
                create_if_missing,
                ..
            } = &action_entry.action
            else {
                panic!("expected replace action");
            };
            assert_eq!(file, "config/settings.py");
            assert!(*regex);
            assert!(!*create_if_missing);
        }
    }

    #[test]
    fn test_db_migrate_detection_prefers_first_match_and_lists_all() {
        let config = Config::default();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("prisma")).unwrap();
        std::fs::write(tmp.path().join("prisma/schema.prisma"), "").unwrap();
        std::fs::write(tmp.path().join("bun.lock"), "").unwrap();
        std::fs::write(tmp.path().join("manage.py"), "").unwrap();
        std::fs::write(tmp.path().join("uv.lock"), "").unwrap();

        let detection = RecipeId::DbMigrate.detect(&no_binaries(&config, tmp.path()));
        assert!(detection.applicable);
        assert!(detection.suggested);
        assert_eq!(
            detection.suggested_params.get("command").unwrap(),
            "bunx prisma migrate deploy"
        );
        let options = detection.param_options.get("command").unwrap();
        assert!(options.contains(&"uv run python manage.py migrate".to_string()));
    }

    #[test]
    fn test_db_migrate_no_detection_still_applicable() {
        let config = Config::default();
        let tmp = tempfile::tempdir().unwrap();
        let detection = RecipeId::DbMigrate.detect(&no_binaries(&config, tmp.path()));
        assert!(detection.applicable);
        assert!(!detection.suggested);
        assert!(detection.suggested_params.get("command").is_none());
    }

    #[test]
    fn test_db_migrate_build() {
        let built = RecipeId::DbMigrate
            .build(&params(&[("command", "bin/rails db:migrate")]))
            .unwrap();
        for phase in [HookPhase::PostCreate, HookPhase::PostSwitch] {
            let entry = built.get(&phase).unwrap().get("db-migrate").unwrap();
            let HookEntry::Extended(extended) = entry else {
                panic!("expected extended hook");
            };
            assert_eq!(extended.command, "bin/rails db:migrate");
            assert_eq!(extended.continue_on_error, Some(true));
            assert!(!extended.background);
        }
        assert!(RecipeId::DbMigrate.build(&RecipeParams::new()).is_err());
    }

    #[test]
    fn test_install_deps_detection_picks_installed_manager() {
        let config = Config::default();
        let tmp = tempfile::tempdir().unwrap();
        // Lingering package-lock.json next to the real bun lockfile
        std::fs::write(tmp.path().join("bun.lockb"), "").unwrap();
        std::fs::write(tmp.path().join("package-lock.json"), "").unwrap();
        std::fs::write(tmp.path().join("Cargo.lock"), "").unwrap();

        let ctx = DetectContext::with_binary_checker(&config, tmp.path(), |bin| {
            matches!(bin, "bun" | "npm" | "cargo")
        });
        let detection = RecipeId::InstallDeps.detect(&ctx);
        assert!(detection.suggested);
        assert_eq!(
            detection.suggested_params.get("command").unwrap(),
            "bun install --frozen-lockfile && cargo build"
        );
        let options = detection.param_options.get("command").unwrap();
        assert!(options.contains(&"npm ci".to_string()));
    }

    #[test]
    fn test_install_deps_skips_missing_binaries() {
        let config = Config::default();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("pnpm-lock.yaml"), "").unwrap();

        let detection = RecipeId::InstallDeps.detect(&no_binaries(&config, tmp.path()));
        assert!(detection.applicable);
        assert!(!detection.suggested);
        assert!(detection
            .reasons
            .iter()
            .any(|r| r.contains("pnpm-lock.yaml found but pnpm is not installed")));
    }

    #[test]
    fn test_workspace_setup_detection() {
        let config = Config::default();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".env.example"), "").unwrap();
        std::fs::write(tmp.path().join("mise.toml"), "").unwrap();

        // mise config present but mise not installed → only copy-env suggested
        let detection = RecipeId::WorkspaceSetup.detect(&no_binaries(&config, tmp.path()));
        assert!(detection.applicable);
        assert_eq!(detection.suggested_params.get("copy-env").unwrap(), "true");
        assert!(detection.suggested_params.get("mise-trust").is_none());
        assert!(detection
            .reasons
            .iter()
            .any(|r| r.contains("mise is not installed")));

        // Nothing to set up → not applicable
        let empty = tempfile::tempdir().unwrap();
        let detection = RecipeId::WorkspaceSetup.detect(&no_binaries(&config, empty.path()));
        assert!(!detection.applicable);
    }

    #[test]
    fn test_workspace_setup_build() {
        let built = RecipeId::WorkspaceSetup
            .build(&params(&[("copy-env", "true"), ("mise-trust", "true")]))
            .unwrap();
        let phase_hooks = built.get(&HookPhase::PostCreate).unwrap();
        assert_eq!(phase_hooks.len(), 2);

        let HookEntry::Action(copy_entry) = phase_hooks.get("copy-env").unwrap() else {
            panic!("expected action hook");
        };
        assert!(matches!(
            &copy_entry.action,
            HookAction::Copy {
                overwrite: false,
                ..
            }
        ));

        let HookEntry::Extended(mise_entry) = phase_hooks.get("mise-trust").unwrap() else {
            panic!("expected extended hook");
        };
        assert_eq!(mise_entry.condition.as_deref(), Some("command_exists:mise"));

        assert!(RecipeId::WorkspaceSetup
            .build(&RecipeParams::new())
            .is_err());
    }

    #[test]
    fn test_sync_ai_configs_detection_and_build() {
        let config = Config::default();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".claude")).unwrap();

        let detection = RecipeId::SyncAiConfigs.detect(&no_binaries(&config, tmp.path()));
        assert!(detection.applicable);
        assert!(detection.suggested);

        let built = RecipeId::SyncAiConfigs.build(&RecipeParams::new()).unwrap();
        let entry = built
            .get(&HookPhase::PreRemove)
            .unwrap()
            .get("sync-ai-configs")
            .unwrap();
        let HookEntry::Extended(extended) = entry else {
            panic!("expected extended hook");
        };
        assert_eq!(extended.command, "devflow sync-ai-configs");
        assert_eq!(extended.continue_on_error, Some(true));
    }

    #[test]
    fn test_multiplexer_session_detection_and_build() {
        let config = Config::default();
        let tmp = tempfile::tempdir().unwrap();

        let none = RecipeId::MultiplexerSession.detect(&no_binaries(&config, tmp.path()));
        assert!(!none.applicable);

        let ctx = DetectContext::with_binary_checker(&config, tmp.path(), |bin| bin == "tmux");
        let detection = RecipeId::MultiplexerSession.detect(&ctx);
        assert!(detection.applicable && detection.suggested);

        let built = RecipeId::MultiplexerSession
            .build(&RecipeParams::new())
            .unwrap();
        let entry = built
            .get(&HookPhase::PostCreate)
            .unwrap()
            .get("open-session")
            .unwrap();
        let HookEntry::Extended(extended) = entry else {
            panic!("expected extended hook");
        };
        assert_eq!(extended.command, "devflow switch --open {{ workspace }}");
        assert!(extended.background);
    }

    #[test]
    fn test_resolve_params_layering_and_validation() {
        // user > detection > static default
        let detection = RecipeDetection {
            applicable: true,
            suggested: true,
            reasons: vec![],
            suggested_params: params(&[("file", ".env"), ("vars", "A=1")]),
            param_options: IndexMap::new(),
        };
        let resolved = resolve_params(
            RecipeId::EnvFile,
            Some(&detection),
            &params(&[("vars", "B=2")]),
        )
        .unwrap();
        assert_eq!(resolved.get("file").unwrap(), ".env"); // detection beats static default
        assert_eq!(resolved.get("vars").unwrap(), "B=2"); // user beats detection

        // static default kicks in without detection
        let resolved =
            resolve_params(RecipeId::EnvFile, None, &params(&[("vars", "A=1")])).unwrap();
        assert_eq!(resolved.get("file").unwrap(), ".env.local");

        // missing required
        assert!(resolve_params(RecipeId::EnvFile, None, &RecipeParams::new()).is_err());
        // unknown key
        assert!(resolve_params(RecipeId::EnvFile, None, &params(&[("nope", "x")])).is_err());
        // recipes without params reject any param
        assert!(resolve_params(RecipeId::SyncAiConfigs, None, &params(&[("x", "y")])).is_err());
        // bool validation
        assert!(resolve_params(
            RecipeId::WorkspaceSetup,
            None,
            &params(&[("copy-env", "yes")])
        )
        .is_err());
    }

    #[test]
    fn test_detect_info_reports_installed_via_markers() {
        let config = config_from_yaml("services:\n  - name: db\n    service_type: postgres\n");
        let tmp = tempfile::tempdir().unwrap();
        let ctx = no_binaries(&config, tmp.path());

        // db-migrate hook present under its stable name → installed, even
        // though detection found no migration tool (no preview).
        let existing = RecipeId::DbMigrate
            .build(&params(&[("command", "custom migrate")]))
            .unwrap();
        let info = RecipeId::DbMigrate.detect_info(&ctx, Some(&existing));
        assert!(info.installed);
        assert!(info.hooks_preview.is_empty());

        let info = RecipeId::EnvFile.detect_info(&ctx, Some(&existing));
        assert!(!info.installed);
        assert!(!info.hooks_preview.is_empty());

        // repeatable recipes never report installed
        let patched = RecipeId::PatchConfig
            .build(&params(&[
                ("file", "app.yml"),
                ("pattern", "x"),
                ("replacement", "y"),
            ]))
            .unwrap();
        let info = RecipeId::PatchConfig.detect_info(&ctx, Some(&patched));
        assert!(!info.installed);
    }

    #[test]
    fn test_slugify_and_env_key() {
        assert_eq!(slugify("config/settings.py"), "config-settings-py");
        assert_eq!(slugify("---"), "file");
        assert_eq!(env_key_for("app-db"), "APP_DB_URL");
        assert_eq!(env_key_for("storage"), "STORAGE_URL");
        assert_eq!(env_key_for("--"), "SERVICE_URL");
    }
}
