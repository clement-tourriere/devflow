use std::path::PathBuf;

use anyhow::Result;
use devflow_core::config::Config;
use devflow_core::hooks::{
    approval::ApprovalStore, HookContext, HookEngine, HookEntry, HookPhase, IndexMap,
    TemplateEngine,
};
use devflow_core::vcs;

fn resolve_project_dir_for_hooks() -> PathBuf {
    Config::find_config_file()
        .ok()
        .flatten()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn parse_hook_phase_input(phase: &str) -> Result<HookPhase> {
    let trimmed = phase.trim();
    if trimmed.is_empty() {
        anyhow::bail!("Hook phase cannot be empty");
    }

    let parsed = match trimmed.parse::<HookPhase>() {
        Ok(phase) => phase,
        Err(never) => match never {},
    };

    Ok(parsed)
}

/// Build a `HookContext` from project config and workspace name.
async fn build_hook_context(config: &Config, workspace_name: &str) -> HookContext {
    let project_dir = resolve_project_dir_for_hooks();
    devflow_core::hooks::build_hook_context(config, &project_dir, workspace_name).await
}

/// Handle `devflow hook` subcommands.
pub(super) async fn handle_hook_command(
    action: super::HookCommands,
    config: &Config,
    json_output: bool,
    non_interactive: bool,
) -> Result<()> {
    match action {
        super::HookCommands::Show { phase } => {
            handle_hook_show(config, phase.as_deref(), json_output)?;
        }
        super::HookCommands::Run {
            phase,
            name,
            workspace,
        } => {
            handle_hook_run(
                config,
                &phase,
                name.as_deref(),
                workspace.as_deref(),
                json_output,
                non_interactive,
            )
            .await?;
        }
        super::HookCommands::Approvals { action } => {
            handle_hook_approvals(action, json_output)?;
        }
        super::HookCommands::Explain { phase } => {
            handle_hook_explain(phase.as_deref(), json_output)?;
        }
        super::HookCommands::Vars { workspace } => {
            handle_hook_vars(config, workspace.as_deref(), json_output).await?;
        }
        super::HookCommands::Render {
            template,
            workspace,
        } => {
            handle_hook_render(config, &template, workspace.as_deref(), json_output).await?;
        }
        super::HookCommands::Triggers => {
            handle_hook_triggers(config, json_output)?;
        }
        super::HookCommands::Actions => {
            handle_hook_actions(json_output)?;
        }
        super::HookCommands::Recipes => {
            handle_hook_recipes(config, json_output)?;
        }
        super::HookCommands::Install { recipe, param, yes } => {
            handle_hook_install(config, &recipe, &param, yes, json_output, non_interactive)?;
        }
        super::HookCommands::Setup => {
            handle_hook_setup(config, json_output, non_interactive)?;
        }
    }
    Ok(())
}

/// `devflow hook show [phase]` — display configured hooks.
fn handle_hook_show(config: &Config, phase_filter: Option<&str>, json_output: bool) -> Result<()> {
    let hooks = match &config.hooks {
        Some(h) if !h.is_empty() => h,
        _ => {
            if json_output {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({}))?);
            } else {
                println!("No hooks configured.");
                println!("  Add a 'hooks' section to .devflow.yml to configure lifecycle hooks.");
            }
            return Ok(());
        }
    };

    // Optionally filter to a single phase
    let phase_filter_parsed: Option<HookPhase> = match phase_filter {
        Some(s) => {
            let parsed = parse_hook_phase_input(s)?;
            if let HookPhase::Custom(ref name) = parsed {
                eprintln!(
                    "Warning: '{}' is not a built-in phase. Built-in phases: pre-switch, post-create, \
                     post-start, post-switch, pre-remove, post-remove, pre-commit, \
                     pre-service-create, post-service-create, pre-service-delete, \
                     post-service-delete, post-service-switch",
                    name
                );
            }
            Some(parsed)
        }
        None => None,
    };

    if json_output {
        let mut filtered = serde_json::Map::new();
        for (phase, phase_hooks) in hooks.iter().filter(|(phase, _)| {
            phase_filter_parsed
                .as_ref()
                .is_none_or(|parsed_phase| *phase == parsed_phase)
        }) {
            filtered.insert(phase.to_string(), serde_json::to_value(phase_hooks)?);
        }
        println!("{}", serde_json::to_string_pretty(&filtered)?);
        return Ok(());
    }

    let mut shown = false;
    for (phase, named_hooks) in hooks {
        if let Some(ref pf) = phase_filter_parsed {
            if phase != pf {
                continue;
            }
        }

        shown = true;
        println!(
            "{} ({}):",
            phase,
            if phase.is_blocking() {
                "blocking"
            } else {
                "background"
            }
        );

        for (name, entry) in named_hooks {
            match entry {
                HookEntry::Simple(cmd) => {
                    println!("  {}: {}", name, cmd);
                }
                HookEntry::Extended(ext) => {
                    println!("  {}:", name);
                    println!("    command: {}", ext.command);
                    if let Some(ref wd) = ext.working_dir {
                        println!("    working_dir: {}", wd);
                    }
                    if let Some(ref cond) = ext.condition {
                        println!("    condition: {}", cond);
                    }
                    if let Some(coe) = ext.continue_on_error {
                        println!("    continue_on_error: {}", coe);
                    }
                    if ext.background {
                        println!("    background: true");
                    }
                    if let Some(ref env) = ext.environment {
                        println!("    environment:");
                        for (k, v) in env {
                            println!("      {}: {}", k, v);
                        }
                    }
                }
                HookEntry::Action(act) => {
                    println!("  {}:", name);
                    println!("    action: {}", act.action.type_name());
                    if let Some(ref wd) = act.working_dir {
                        println!("    working_dir: {}", wd);
                    }
                    if let Some(ref cond) = act.condition {
                        println!("    condition: {}", cond);
                    }
                    if let Some(coe) = act.continue_on_error {
                        println!("    continue_on_error: {}", coe);
                    }
                    if act.background {
                        println!("    background: true");
                    }
                }
            }
        }
    }

    if !shown {
        if let Some(pf) = phase_filter {
            println!("No hooks configured for phase '{}'.", pf);
        }
    }

    Ok(())
}

/// `devflow hook explain [phase]` — show documentation about hook phases.
fn handle_hook_explain(phase: Option<&str>, json_output: bool) -> Result<()> {
    // Static phase documentation: (name, summary, blocking, category, detail)
    let phases: Vec<(&str, &str, bool, &str, &str)> = vec![
        ("pre-switch",           "Before switching workspaces/worktrees",           true,  "VCS",     "Runs before any workspace/worktree switch. Use for saving state or running checks."),
        ("post-create",          "After creating a new workspace/worktree",          true,  "VCS",     "Runs after a new workspace is created (via `switch -c`). Use for one-time setup: install dependencies, run migrations, write .env files."),
        ("post-start",           "After starting a stopped service container",    false, "VCS",     "Runs after `devflow service start`. Use for warming caches or re-applying state."),
        ("post-switch",          "After switching to a workspace/worktree",          false, "VCS",     "Runs every time you switch workspaces. Use for updating .env files, restarting dev servers."),
        ("pre-remove",           "Before removing a workspace",                      true,  "VCS",     "Runs before `devflow remove`. Use for cleanup tasks or archival."),
        ("post-remove",          "After removing a workspace",                       false, "VCS",     "Runs after workspace removal. Use for notifying external systems."),
        ("pre-commit",           "Before committing",                             true,  "VCS",     "Runs before `devflow commit`. Use for linting, formatting, or test checks."),
        ("pre-service-create",   "Before creating a service workspace",              true,  "Service", "Runs before service provisioning. Use for pre-flight checks."),
        ("post-service-create",  "After creating a service workspace",               true,  "Service", "Runs after service provisioning. THE most common hook — use for npm ci, migrations, writing .env files."),
        ("pre-service-delete",   "Before deleting a service workspace",              true,  "Service", "Runs before service teardown. Use for data export or backups."),
        ("post-service-delete",  "After deleting a service workspace",               false, "Service", "Runs after service teardown. Use for cleanup."),
        ("post-service-switch",  "After services switch to a workspace",             false, "Service", "Runs after services switch (not VCS). Use for service-specific reconnection."),
    ];

    if json_output {
        let items: Vec<serde_json::Value> = phases
            .iter()
            .map(|(name, summary, blocking, category, detail)| {
                serde_json::json!({
                    "phase": name,
                    "summary": summary,
                    "blocking": blocking,
                    "category": category,
                    "detail": detail,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }

    if let Some(phase_name) = phase {
        // Show detailed info for one phase
        if let Some((name, summary, blocking, category, detail)) =
            phases.iter().find(|(n, ..)| *n == phase_name)
        {
            println!("{}", name);
            println!("{}", "=".repeat(name.len()));
            println!();
            println!("Category:  {}", category);
            println!(
                "Blocking:  {}",
                if *blocking {
                    "Yes (waits for completion)"
                } else {
                    "No (runs in background)"
                }
            );
            println!("Summary:   {}", summary);
            println!();
            println!("{}", detail);
            println!();
            println!("Example YAML:");
            println!();
            // Show a contextual example based on the phase
            match *name {
                "post-create" | "post-service-create" => {
                    println!("  hooks:");
                    println!("    {}:", name);
                    println!("      install: \"npm ci\"");
                    println!("      env: |");
                    println!("        cat > .env.local << EOF");
                    println!("        DATABASE_URL={{{{ service['db'].url }}}}");
                    println!("        EOF");
                    println!("      migrate: \"npx prisma migrate deploy\"");
                }
                "post-switch" | "post-service-switch" => {
                    println!("  hooks:");
                    println!("    {}:", name);
                    println!("      env: |");
                    println!("        cat > .env.local << EOF");
                    println!("        DATABASE_URL={{{{ service['db'].url }}}}");
                    println!("        EOF");
                }
                "pre-commit" => {
                    println!("  hooks:");
                    println!("    {}:", name);
                    println!("      lint: \"npm run lint\"");
                    println!("      test: \"npm test\"");
                }
                _ => {
                    println!("  hooks:");
                    println!("    {}:", name);
                    println!(
                        "      example: \"echo Running {} for {{{{ workspace }}}}\"",
                        name
                    );
                }
            }
            println!();
            println!("Available template variables:");
            println!("  {{{{ workspace }}}}              Current workspace name");
            println!("  {{{{ name }}}}               Project name (from config.name)");
            println!("  {{{{ repo }}}}                Repository name");
            println!("  {{{{ default_workspace }}}}      Main workspace (e.g. main)");
            println!("  {{{{ worktree_path }}}}       Worktree directory path");
            println!("  {{{{ service['name'].url }}}} Full connection URL for a service");
            println!("  {{{{ service['name'].host/port/database/user/password }}}}");
            println!();
            println!("Available filters:");
            println!("  {{{{ workspace | sanitize }}}}     Path-safe (/ -> -)");
            println!("  {{{{ workspace | sanitize_db }}}}  DB-safe (lowercase, _, max 63 chars)");
            println!("  {{{{ workspace | hash_port }}}}    Deterministic port 10000-19999");
            println!();
            print_conditions_reference();
        } else {
            println!("Unknown phase: '{}'", phase_name);
            println!();
            println!("Built-in phases:");
            for (name, summary, blocking, ..) in &phases {
                println!(
                    "  {:<24} {} {}",
                    name,
                    if *blocking {
                        "[blocking]  "
                    } else {
                        "[background]"
                    },
                    summary
                );
            }
        }
    } else {
        // List all phases
        println!("Hook Phases");
        println!("===========");
        println!();
        println!("Which hook should I use?");
        println!("  Setting up a new workspace?     -> post-create or post-service-create");
        println!("  Updating env on switch?      -> post-switch");
        println!("  Running tests before commit? -> pre-commit");
        println!("  Custom setup per service?    -> post-service-create");
        println!();

        let mut current_category = "";
        for (name, summary, blocking, category, _) in &phases {
            if *category != current_category {
                println!();
                println!("{} Lifecycle:", category);
                current_category = category;
            }
            println!(
                "  {:<24} {} {}",
                name,
                if *blocking {
                    "[blocking]  "
                } else {
                    "[background]"
                },
                summary
            );
        }
        println!();
        print_conditions_reference();
        println!();
        println!("Use 'devflow hook explain <phase>' for detailed info and examples.");
    }

    Ok(())
}

fn print_conditions_reference() {
    println!("Available conditions:");
    println!("  Conditions control when a hook runs. Built-in conditions are safe");
    println!("  (no approval needed). Custom shell conditions require approval.");
    println!();
    println!("  Worktree:");
    println!("    is_worktree                    Only run in worktree workspaces");
    println!("    not_worktree                   Skip worktree workspaces");
    println!();
    println!("  Trigger source:");
    println!("    trigger_is:<source>            Only run for a trigger (vcs, cli, gui, auto)");
    println!("    trigger_not:<source>           Skip for a trigger source");
    println!();
    println!("  Workspace:");
    println!("    workspace_is:<name>            Only run for a specific workspace");
    println!("    workspace_not:<name>           Skip a specific workspace");
    println!("    workspace_matches:<regex>      Workspace name matches a regex");
    println!("    is_default_workspace           Only run on the default workspace (e.g. main)");
    println!("    not_default_workspace          Skip the default workspace");
    println!();
    println!("  File system:");
    println!("    file_exists:<path>             Only run if a file exists");
    println!("    dir_exists:<path>              Only run if a directory exists");
    println!("    command_exists:<bin>           Only run if a binary is on PATH (or mise shims)");
    println!();
    println!("  Environment:");
    println!("    env_set:<VAR>                  Only run if an env var is set");
    println!("    env_is:<VAR>=<value>           Only run if an env var has a specific value");
    println!();
    println!("  Boolean:");
    println!("    always / true                  Always run");
    println!("    never / false                  Never run (disable a hook)");
    println!();
    println!("  Custom:");
    println!("    <shell command>                Exit 0 = true (requires approval)");
    println!();
    println!("  Example:");
    println!("    hooks:");
    println!("      post-create:");
    println!("        trust-mise:");
    println!("          command: \"mise trust\"");
    println!("          condition: is_worktree");
}

/// `devflow hook vars` — show available template variables with current values.
async fn handle_hook_vars(
    config: &Config,
    branch_override: Option<&str>,
    json_output: bool,
) -> Result<()> {
    let workspace_name = if let Some(b) = branch_override {
        b.to_string()
    } else {
        match vcs::detect_vcs_provider(".") {
            Ok(vcs_repo) => vcs_repo
                .current_workspace()
                .ok()
                .flatten()
                .unwrap_or_else(|| "unknown".to_string()),
            Err(_) => "unknown".to_string(),
        }
    };

    let context = build_hook_context(config, &workspace_name).await;
    let engine = TemplateEngine::new();

    if json_output {
        println!("{}", serde_json::to_string_pretty(&context)?);
        return Ok(());
    }

    println!("Template Variables (current context):");
    println!();
    println!("  {{{{ workspace }}}}              = {}", context.workspace);
    println!("  {{{{ name }}}}               = {}", context.name);
    println!("  {{{{ repo }}}}                = {}", context.repo);
    println!(
        "  {{{{ default_workspace }}}}      = {}",
        context.default_workspace
    );
    if let Some(ref wt) = context.worktree_path {
        println!("  {{{{ worktree_path }}}}       = {}", wt);
    }
    if let Some(ref commit) = context.commit {
        println!("  {{{{ commit }}}}              = {}", commit);
    }
    if let Some(ref sc) = context.short_commit {
        println!("  {{{{ short_commit }}}}        = {}", sc);
    }
    println!(
        "  {{{{ trigger_source }}}}      = {}",
        context.trigger_source
    );
    if let Some(ref prev) = context.previous_workspace {
        println!("  {{{{ previous_workspace }}}}  = {}", prev);
    }
    if let Some(ref ve) = context.vcs_event {
        println!("  {{{{ vcs_event }}}}           = {}", ve);
    }

    if !context.service.is_empty() {
        println!();
        println!("  Services:");
        for (name, svc) in &context.service {
            println!();
            println!("    {{{{ service['{}'].host }}}}     = {}", name, svc.host);
            println!("    {{{{ service['{}'].port }}}}     = {}", name, svc.port);
            println!(
                "    {{{{ service['{}'].database }}}} = {}",
                name, svc.database
            );
            println!("    {{{{ service['{}'].user }}}}     = {}", name, svc.user);
            if let Some(ref pw) = svc.password {
                println!("    {{{{ service['{}'].password }}}} = {}", name, pw);
            }
            println!("    {{{{ service['{}'].url }}}}      = {}", name, svc.url);
        }
    }

    // Show filter examples
    println!();
    println!("  Filters:");
    let sanitized = engine
        .render("{{ workspace | sanitize }}", &context)
        .unwrap_or_default();
    let sanitized_db = engine
        .render("{{ workspace | sanitize_db }}", &context)
        .unwrap_or_default();
    let hash_port = engine
        .render("{{ workspace | hash_port }}", &context)
        .unwrap_or_default();
    println!("    {{{{ workspace | sanitize }}}}      = {}", sanitized);
    println!("    {{{{ workspace | sanitize_db }}}}   = {}", sanitized_db);
    println!("    {{{{ workspace | hash_port }}}}     = {}", hash_port);

    Ok(())
}

/// `devflow hook render <template>` — render a template string.
async fn handle_hook_render(
    config: &Config,
    template: &str,
    branch_override: Option<&str>,
    json_output: bool,
) -> Result<()> {
    let workspace_name = if let Some(b) = branch_override {
        b.to_string()
    } else {
        match vcs::detect_vcs_provider(".") {
            Ok(vcs_repo) => vcs_repo
                .current_workspace()
                .ok()
                .flatten()
                .unwrap_or_else(|| "unknown".to_string()),
            Err(_) => "unknown".to_string(),
        }
    };

    let context = build_hook_context(config, &workspace_name).await;
    let engine = TemplateEngine::new();
    let rendered = engine.render(template, &context)?;

    if json_output {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "template": template,
                "rendered": rendered,
            }))?
        );
    } else {
        println!("{}", rendered);
    }

    Ok(())
}

/// `devflow hook run <phase> [name]` — manually execute hooks.
async fn handle_hook_run(
    config: &Config,
    phase_str: &str,
    name_filter: Option<&str>,
    branch_override: Option<&str>,
    json_output: bool,
    _non_interactive: bool,
) -> Result<()> {
    let hooks_config = match &config.hooks {
        Some(h) if !h.is_empty() => h.clone(),
        _ => {
            anyhow::bail!("No hooks configured. Add a 'hooks' section to .devflow.yml first.");
        }
    };

    let phase = parse_hook_phase_input(phase_str)?;

    if let HookPhase::Custom(ref name) = phase {
        eprintln!(
            "Warning: '{}' is not a built-in phase. Built-in phases: pre-switch, post-create, \
             post-start, post-switch, pre-remove, post-remove, pre-commit, \
             pre-service-create, post-service-create, pre-service-delete, \
             post-service-delete, post-service-switch",
            name
        );
    }

    // Determine workspace name: use override, or try current git workspace, or fallback
    let workspace_name = if let Some(b) = branch_override {
        b.to_string()
    } else {
        match vcs::detect_vcs_provider(".") {
            Ok(vcs_repo) => vcs_repo
                .current_workspace()
                .ok()
                .flatten()
                .unwrap_or_else(|| "unknown".to_string()),
            Err(_) => "unknown".to_string(),
        }
    };

    let context = build_hook_context(config, &workspace_name).await;

    // If a specific hook name is given, build a filtered config
    let effective_config = if let Some(name) = name_filter {
        let phase_hooks = hooks_config
            .get(&phase)
            .ok_or_else(|| anyhow::anyhow!("No hooks configured for phase '{}'", phase))?;

        let entry = phase_hooks.get(name).ok_or_else(|| {
            anyhow::anyhow!(
                "Hook '{}' not found in phase '{}'. Available: {}",
                name,
                phase,
                phase_hooks
                    .keys()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

        let mut filtered = IndexMap::new();
        let mut phase_map = IndexMap::new();
        phase_map.insert(name.to_string(), entry.clone());
        filtered.insert(phase.clone(), phase_map);
        filtered
    } else {
        hooks_config
    };

    let working_dir =
        std::env::current_dir().map_err(|e| anyhow::anyhow!("Failed to get cwd: {}", e))?;

    // Manual runs don't require approval
    let engine =
        HookEngine::new_no_approval(effective_config, working_dir).with_quiet_output(json_output);
    let result = if json_output {
        engine.run_phase(&phase, &context).await?
    } else {
        engine.run_phase_verbose(&phase, &context).await?
    };

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "phase": phase.to_string(),
                "succeeded": result.succeeded,
                "failed": result.failed,
                "skipped": result.skipped,
                "background": result.background,
            }))?
        );
    } else if result.succeeded == 0 && result.background == 0 && result.skipped == 0 {
        println!("No hooks ran for phase '{}'.", phase);
    }

    Ok(())
}

/// `devflow hook approvals` — manage hook approval store.
fn handle_hook_approvals(action: super::ApprovalCommands, json_output: bool) -> Result<()> {
    let project_key = std::env::current_dir()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    match action {
        super::ApprovalCommands::List => {
            let store = ApprovalStore::load().unwrap_or_default();
            let mut approved = store.list_approved(&project_key);
            approved.sort_by(|a, b| a.command.cmp(&b.command));

            if json_output {
                let items: Vec<serde_json::Value> = approved
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "command": r.command,
                            "approved_at": r.approved_at.to_rfc3339(),
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "project": project_key,
                        "approvals": items,
                    }))?
                );
            } else if approved.is_empty() {
                println!("No approved hooks for this project.");
            } else {
                println!("Approved hooks ({}):", approved.len());
                for record in approved {
                    println!(
                        "  - {} (approved {})",
                        record.command,
                        record.approved_at.format("%Y-%m-%d %H:%M")
                    );
                }
            }
        }
        super::ApprovalCommands::Add { command } => {
            let mut store = ApprovalStore::load().unwrap_or_default();
            store.approve(&project_key, &command)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "ok",
                        "action": "approve",
                        "command": command,
                    }))?
                );
            } else {
                println!("Approved hook command: {}", command);
            }
        }
        super::ApprovalCommands::Clear => {
            let mut store = ApprovalStore::load().unwrap_or_default();
            store.clear_project(&project_key)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "ok",
                        "action": "clear_approvals",
                        "project": project_key,
                    }))?
                );
            } else {
                println!("Cleared all hook approvals for this project.");
            }
        }
    }

    Ok(())
}

/// `devflow hook triggers` — show VCS event → devflow phase mapping.
fn handle_hook_triggers(config: &Config, json_output: bool) -> Result<()> {
    let triggers = config.triggers.clone().unwrap_or_default();
    let mappings = triggers.git_mappings();

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "git": mappings,
            }))?
        );
        return Ok(());
    }

    println!("VCS Event → Devflow Phase(s)");
    println!("{}", "─".repeat(50));
    for mapping in &mappings {
        println!(
            "  git {:<18} → [{}]",
            mapping.vcs_event,
            mapping.phases.join(", ")
        );
    }
    println!();
    println!("Override in .devflow.yml:");
    println!("  triggers:");
    println!("    git:");
    for mapping in &mappings {
        println!(
            "      {}: [{}]",
            mapping.vcs_event,
            mapping.phases.join(", ")
        );
    }

    Ok(())
}

/// `devflow hook actions` — list all available built-in action types.
fn handle_hook_actions(json_output: bool) -> Result<()> {
    let actions = vec![
        ("shell", "Run a shell command", "action:\n  type: shell\n  command: \"npm ci\""),
        ("replace", "Find and replace in a file (plain or regex)", "action:\n  type: replace\n  file: config/database.yml\n  pattern: \"database: \\\\w+\"\n  replacement: \"database: {{ service['app-db'].database }}\"\n  regex: true"),
        ("write-file", "Write content to a file", "action:\n  type: write-file\n  path: config/branch.txt\n  content: \"{{ workspace }}\""),
        ("write-env", "Write a .env-style file with key=value pairs", "action:\n  type: write-env\n  path: .env.local\n  vars:\n    DATABASE_URL: \"{{ service['app-db'].url }}\"\n    WORKSPACE: \"{{ workspace }}\""),
        ("copy", "Copy a file", "action:\n  type: copy\n  from: .env.example\n  to: .env.local"),
        ("docker-exec", "Execute a command inside a Docker container", "action:\n  type: docker-exec\n  container: myapp-postgres\n  command: \"psql -U postgres -c 'CREATE EXTENSION IF NOT EXISTS pgcrypto'\""),
        ("http", "Make an HTTP request", "action:\n  type: http\n  url: \"https://hooks.slack.com/services/XXX\"\n  method: POST\n  headers:\n    Content-Type: application/json\n  body: '{\"text\": \"Workspace {{ workspace }} ready\"}'"),
        ("notify", "Send a desktop notification", "action:\n  type: notify\n  title: devflow\n  message: \"Workspace {{ workspace }} is ready\"\n  level: success"),
    ];

    if json_output {
        let items: Vec<serde_json::Value> = actions
            .iter()
            .map(|(name, desc, example)| {
                serde_json::json!({
                    "type": name,
                    "description": desc,
                    "example": example,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }

    println!("Built-in Hook Actions");
    println!("{}", "─".repeat(50));
    println!();
    println!(
        "Actions replace shell commands for common operations.\nOnly 'shell' and 'docker-exec' require approval; all others run without prompting.\n"
    );

    for (name, description, example) in &actions {
        println!("  {} — {}", name, description);
        println!();
        for line in example.lines() {
            println!("    {}", line);
        }
        println!();
    }

    println!("Usage in .devflow.yml:");
    println!("  hooks:");
    println!("    post-create:");
    println!("      my-hook:");
    println!("        action:");
    println!("          type: write-env");
    println!("          path: .env.local");
    println!("          vars:");
    println!("            DATABASE_URL: \"{{{{ service['app-db'].url }}}}\"");

    Ok(())
}

/// `devflow hook recipes` — list recipes, with detection inside a project.
fn handle_hook_recipes(config: &Config, json_output: bool) -> Result<()> {
    use devflow_core::hooks::detect::DetectContext;
    use devflow_core::hooks::recipes::{self, RecipeId};

    let Some(project_root) = config.project_root.clone() else {
        // Outside a project: static listing only.
        return print_static_recipes(json_output);
    };

    let ctx = DetectContext::new(config, &project_root);
    let infos: Vec<recipes::RecipeDetectionInfo> = RecipeId::ALL
        .iter()
        .map(|id| id.detect_info(&ctx, config.hooks.as_ref()))
        .collect();

    if json_output {
        println!("{}", serde_json::to_string_pretty(&infos)?);
        return Ok(());
    }

    println!("Hook Recipes");
    println!("{}", "─".repeat(50));

    let mut current_category = String::new();
    for info in &infos {
        if info.recipe.category != current_category {
            println!();
            println!("{}:", info.recipe.category);
            current_category = info.recipe.category.clone();
        }

        let marker = if info.installed {
            " [installed]"
        } else if info.suggested {
            " [suggested]"
        } else if !info.applicable {
            " [not applicable]"
        } else {
            ""
        };
        println!(
            "  {}{} — {}",
            info.recipe.name, marker, info.recipe.description
        );
        for reason in &info.reasons {
            println!("      {}", reason);
        }
    }

    println!();
    println!("Install one: devflow hook install <name>   Wizard: devflow hook setup");
    Ok(())
}

fn print_static_recipes(json_output: bool) -> Result<()> {
    use devflow_core::hooks::recipes::{self, RecipeId};

    let infos: Vec<recipes::RecipeInfo> = RecipeId::ALL.iter().map(|id| id.to_info()).collect();
    if json_output {
        println!("{}", serde_json::to_string_pretty(&infos)?);
        return Ok(());
    }

    println!("Hook Recipes");
    println!("{}", "─".repeat(50));
    let mut current_category = String::new();
    for info in &infos {
        if info.category != current_category {
            println!();
            println!("{}:", info.category);
            current_category = info.category.clone();
        }
        println!("  {} — {}", info.name, info.description);
    }
    println!();
    println!("Run inside a devflow project to see which recipes apply to it.");
    Ok(())
}

/// `devflow hook install <recipe>` — detect, parameterize, and install.
fn handle_hook_install(
    config: &Config,
    recipe_name: &str,
    param_flags: &[String],
    yes: bool,
    json_output: bool,
    non_interactive: bool,
) -> Result<()> {
    use devflow_core::hooks::detect::DetectContext;
    use devflow_core::hooks::recipes::{self, RecipeId};

    if let Some(message) = recipes::removed_recipe_message(recipe_name) {
        anyhow::bail!("{}", message);
    }
    let id = RecipeId::from_name(recipe_name).ok_or_else(|| {
        anyhow::anyhow!(
            "Recipe '{}' not found. Run 'devflow hook recipes' to see available recipes.",
            recipe_name
        )
    })?;

    let config_file = Config::find_config_file()?
        .ok_or_else(|| anyhow::anyhow!("No .devflow.yml found. Run 'devflow init' first."))?;
    let project_root = config
        .project_root
        .clone()
        .or_else(|| config_file.parent().map(|p| p.to_path_buf()))
        .ok_or_else(|| anyhow::anyhow!("Could not determine the project root"))?;

    let ctx = DetectContext::new(config, &project_root);
    let detection = id.detect(&ctx);
    if !detection.applicable {
        anyhow::bail!(
            "Recipe '{}' does not apply to this project:\n  {}",
            recipe_name,
            detection.reasons.join("\n  ")
        );
    }

    let user_params = parse_param_flags(param_flags)?;
    let params = if yes || non_interactive || json_output {
        recipes::resolve_params(id, Some(&detection), &user_params)?
    } else {
        for reason in &detection.reasons {
            println!("  {}", reason);
        }
        prompt_recipe_params(id, &detection, user_params)?
    };
    let generated = id.build(&params)?;

    // Merge against the raw committed config (the write-back target), not
    // the merged view that may contain local-state overlays.
    let file_config = Config::from_file(&config_file)?;
    let mut hooks_config = file_config.hooks.unwrap_or_default();

    if !json_output {
        println!();
        println!("Recipe: {} — {}", id.name(), id.meta().description);
        println!();
        println!("This will add the following hooks:");
        for preview in recipes::hooks_preview_of(&generated) {
            let phase = parse_hook_phase_input(&preview.phase)?;
            let exists = hooks_config
                .get(&phase)
                .is_some_and(|hooks| hooks.contains_key(&preview.hook_name));
            if exists {
                println!(
                    "  {} → {} — SKIP (already exists)",
                    preview.phase, preview.hook_name
                );
            } else {
                println!(
                    "  {} → {} ({})",
                    preview.phase, preview.hook_name, preview.command_summary
                );
            }
        }
        println!();

        if !yes && !non_interactive {
            let confirm = inquire::Confirm::new("Install this recipe?")
                .with_default(true)
                .prompt()
                .unwrap_or(false);
            if !confirm {
                println!("Aborted.");
                return Ok(());
            }
        }
    }

    let result = recipes::merge_hooks_into_config(&mut hooks_config, &generated);
    if result.hooks_added > 0 {
        write_hooks_to_config_file(&config_file, &hooks_config)?;
    }

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "recipe": id.name(),
                "params": params,
                "hooks_added": result.hooks_added,
                "hooks_skipped": result.hooks_skipped,
            }))?
        );
    } else if result.hooks_added == 0 {
        println!(
            "All hooks from recipe '{}' are already installed.",
            recipe_name
        );
    } else {
        println!(
            "Installed recipe '{}': {} hook(s) added, {} skipped.",
            recipe_name, result.hooks_added, result.hooks_skipped
        );
    }
    Ok(())
}

/// `devflow hook setup` — detect applicable recipes and install a selection.
fn handle_hook_setup(config: &Config, json_output: bool, non_interactive: bool) -> Result<()> {
    use devflow_core::hooks::detect::DetectContext;
    use devflow_core::hooks::recipes::{self, RecipeDetection, RecipeId};

    if json_output || non_interactive {
        anyhow::bail!(
            "hook setup is interactive; use 'devflow hook install <name> --yes' for automated runs"
        );
    }

    let config_file = Config::find_config_file()?
        .ok_or_else(|| anyhow::anyhow!("No .devflow.yml found. Run 'devflow init' first."))?;
    let project_root = config
        .project_root
        .clone()
        .or_else(|| config_file.parent().map(|p| p.to_path_buf()))
        .ok_or_else(|| anyhow::anyhow!("Could not determine the project root"))?;

    let ctx = DetectContext::new(config, &project_root);
    let mut detected: Vec<(RecipeId, RecipeDetection, bool)> = Vec::new();
    for id in RecipeId::ALL {
        let info = id.detect_info(&ctx, config.hooks.as_ref());
        if !info.applicable {
            continue;
        }
        let detection = RecipeDetection {
            applicable: info.applicable,
            suggested: info.suggested,
            reasons: info.reasons,
            suggested_params: info.suggested_params,
            param_options: info.param_options,
        };
        detected.push((id, detection, info.installed));
    }

    if detected.is_empty() {
        println!("No applicable recipes detected for this project.");
        return Ok(());
    }

    let labels: Vec<String> = detected
        .iter()
        .map(|(id, _, installed)| {
            let mut label = format!("{} — {}", id.name(), id.meta().description);
            if *installed {
                label.push_str(" [installed]");
            }
            label
        })
        .collect();
    let preselected: Vec<usize> = detected
        .iter()
        .enumerate()
        .filter(|(_, (_, detection, installed))| detection.suggested && !installed)
        .map(|(index, _)| index)
        .collect();

    let picked = inquire::MultiSelect::new("Install which recipes?", labels.clone())
        .with_default(&preselected)
        .prompt()?;
    if picked.is_empty() {
        println!("Nothing selected.");
        return Ok(());
    }

    let file_config = Config::from_file(&config_file)?;
    let mut hooks_config = file_config.hooks.unwrap_or_default();
    let mut total_added = 0;
    let mut total_skipped = 0;

    for label in &picked {
        let index = labels
            .iter()
            .position(|l| l == label)
            .expect("picked label comes from the list");
        let (id, detection, _) = &detected[index];

        println!();
        println!("── {} ──", id.name());
        for reason in &detection.reasons {
            println!("   {}", reason);
        }

        let params = prompt_recipe_params(*id, detection, Default::default())?;
        match id.build(&params) {
            Ok(generated) => {
                let result = recipes::merge_hooks_into_config(&mut hooks_config, &generated);
                total_added += result.hooks_added;
                total_skipped += result.hooks_skipped;
            }
            Err(e) => eprintln!("   Skipping {}: {}", id.name(), e),
        }
    }

    if total_added > 0 {
        write_hooks_to_config_file(&config_file, &hooks_config)?;
    }
    println!();
    println!(
        "Setup complete: {} hook(s) added, {} skipped (already present).",
        total_added, total_skipped
    );
    Ok(())
}

const CUSTOM_CHOICE: &str = "custom…";

/// Prompt for each declared param not already set via `--param`, seeded
/// with detection suggestions, then validate the full set.
fn prompt_recipe_params(
    id: devflow_core::hooks::recipes::RecipeId,
    detection: &devflow_core::hooks::recipes::RecipeDetection,
    user_params: devflow_core::hooks::recipes::RecipeParams,
) -> Result<devflow_core::hooks::recipes::RecipeParams> {
    use devflow_core::hooks::recipes::{self, ParamKind, RecipeParams};

    let mut resolved = RecipeParams::new();
    for param in id.params() {
        // --param flags are authoritative; don't prompt for those.
        if let Some(value) = user_params.get(param.key) {
            resolved.insert(param.key.to_string(), value.clone());
            continue;
        }

        let current = detection
            .suggested_params
            .get(param.key)
            .cloned()
            .or_else(|| param.default.map(String::from));

        let value = match param.kind {
            ParamKind::Bool => inquire::Confirm::new(param.label)
                .with_default(current.as_deref() == Some("true"))
                .with_help_message(param.help)
                .prompt()?
                .to_string(),
            ParamKind::Text => prompt_text_lines(&param, current.as_deref())?,
            ParamKind::String => {
                let options = detection
                    .param_options
                    .get(param.key)
                    .filter(|options| options.len() > 1);
                if let Some(options) = options {
                    // Several detected candidates → pick one or customize.
                    let mut choices: Vec<String> = Vec::new();
                    if let Some(current) = &current {
                        if !options.contains(current) {
                            choices.push(current.clone());
                        }
                    }
                    choices.extend(options.iter().cloned());
                    choices.push(CUSTOM_CHOICE.to_string());
                    let picked = inquire::Select::new(param.label, choices)
                        .with_help_message(param.help)
                        .prompt()?;
                    if picked == CUSTOM_CHOICE {
                        inquire::Text::new(param.label)
                            .with_initial_value(current.as_deref().unwrap_or(""))
                            .with_help_message(param.help)
                            .prompt()?
                    } else {
                        picked
                    }
                } else {
                    inquire::Text::new(param.label)
                        .with_initial_value(current.as_deref().unwrap_or(""))
                        .with_help_message(param.help)
                        .prompt()?
                }
            }
        };
        if !value.trim().is_empty() {
            resolved.insert(param.key.to_string(), value);
        }
    }

    recipes::resolve_params(id, Some(detection), &resolved)
}

/// Multi-line param prompt: one Text prompt per suggested line (Enter to
/// keep, edit to change, clear to drop), then extra lines until empty.
fn prompt_text_lines(
    param: &devflow_core::hooks::recipes::RecipeParam,
    current: Option<&str>,
) -> Result<String> {
    println!("{} — {}", param.label, param.help);
    let mut lines: Vec<String> = Vec::new();
    for line in current
        .unwrap_or("")
        .lines()
        .filter(|l| !l.trim().is_empty())
    {
        let edited = inquire::Text::new("entry:")
            .with_initial_value(line)
            .with_help_message("Enter to keep, edit to change, clear to drop")
            .prompt()?;
        if !edited.trim().is_empty() {
            lines.push(edited.trim().to_string());
        }
    }
    loop {
        let extra = inquire::Text::new("add entry:")
            .with_help_message("KEY=VALUE, leave empty to finish")
            .prompt()?;
        if extra.trim().is_empty() {
            break;
        }
        lines.push(extra.trim().to_string());
    }
    Ok(lines.join("\n"))
}

fn parse_param_flags(flags: &[String]) -> Result<devflow_core::hooks::recipes::RecipeParams> {
    let mut params = devflow_core::hooks::recipes::RecipeParams::new();
    for flag in flags {
        let (key, value) = flag
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("invalid --param '{}' (expected KEY=VALUE)", flag))?;
        params.insert(key.trim().to_string(), value.to_string());
    }
    Ok(params)
}

/// Rewrite only the top-level `hooks:` key of the config file.
fn write_hooks_to_config_file(
    config_file: &std::path::Path,
    hooks_config: &devflow_core::hooks::HooksConfig,
) -> Result<()> {
    if config_file.extension().and_then(|e| e.to_str()) == Some("toml") {
        anyhow::bail!(
            "Recipe install writes YAML and this project uses a TOML config ({}). \
             Add the hooks manually or migrate to .devflow.yml.",
            config_file.display()
        );
    }
    let content = std::fs::read_to_string(config_file)?;
    let mut doc: serde_yaml_ng::Value = if content.trim().is_empty() {
        serde_yaml_ng::Value::Mapping(Default::default())
    } else {
        serde_yaml_ng::from_str(&content)?
    };
    if doc.is_null() {
        // A comments-only config parses to null; still a valid empty config.
        doc = serde_yaml_ng::Value::Mapping(Default::default());
    }
    let serde_yaml_ng::Value::Mapping(ref mut map) = doc else {
        anyhow::bail!(
            "{} is not a YAML mapping — cannot insert the hooks section",
            config_file.display()
        );
    };
    map.insert(
        serde_yaml_ng::Value::String("hooks".to_string()),
        serde_yaml_ng::to_value(hooks_config)?,
    );
    std::fs::write(config_file, serde_yaml_ng::to_string(&doc)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_param_flags() {
        let params = parse_param_flags(&[
            "command=sqlx migrate run".to_string(),
            "file=.env.local".to_string(),
            // last one wins on duplicates
            "file=.env".to_string(),
            // values may contain '='
            "vars=A=1".to_string(),
        ])
        .unwrap();
        assert_eq!(params.get("command").unwrap(), "sqlx migrate run");
        assert_eq!(params.get("file").unwrap(), ".env");
        assert_eq!(params.get("vars").unwrap(), "A=1");

        assert!(parse_param_flags(&["no-equals".to_string()]).is_err());
    }
}
