//! `devflow init`: propose a setup from what the repository contains, review
//! it, then write it. Core plans and applies (`devflow_core::propose`); this
//! module parses arguments, asks the questions and renders.
//!
//! - `devflow init` shows the plan and asks what to accept (a terminal is needed).
//! - `devflow init --plan [--json]` writes nothing; with `--json` the document
//!   is the proposal itself, so it can be saved, reviewed and applied.
//! - `devflow init --apply PLAN.json [--accept ID]... [--reject ID]...` applies
//!   a saved plan: its high-confidence items unless told otherwise.
//! - `devflow init --yes` plans, accepts the high-confidence items and applies.
//!
//! Writing configuration never installs tools, trusts hooks or runs anything.
use super::output::{self, Palette, note, say};
use super::{Ctx, ProjectInfo};
use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use devflow_core::propose::{
    self, ApplyReport, Confidence, FileStatus, Item, ItemKind, PlanOptions, Proposal, Selection,
};
use devflow_core::{Project, githook, project, redact};
use inquire::InquireError;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Clone, Copy, ValueEnum)]
pub enum PostgresChoice {
    /// devflow runs a PostgreSQL container with docker compose
    Managed,
    /// An existing PostgreSQL 18+ server
    External,
    /// No PostgreSQL service
    None,
}

impl From<PostgresChoice> for propose::PostgresChoice {
    fn from(choice: PostgresChoice) -> Self {
        match choice {
            PostgresChoice::Managed => propose::PostgresChoice::Managed,
            PostgresChoice::External => propose::PostgresChoice::External,
            PostgresChoice::None => propose::PostgresChoice::None,
        }
    }
}

#[derive(Args)]
pub struct InitArgs {
    /// Project name for a new devflow.toml (default: the checkout's directory name)
    #[arg(long)]
    pub name: Option<String>,
    /// PostgreSQL service: managed, external or none (default: what detection proposes)
    #[arg(long, value_enum)]
    pub postgres: Option<PostgresChoice>,
    /// Show the proposed setup and write nothing (with --json: the proposal document)
    #[arg(long, conflicts_with_all = ["apply", "yes", "accept", "reject", "no_defaults"])]
    pub plan: bool,
    /// Apply a plan saved from `devflow --json init --plan` (`-` reads standard input)
    #[arg(
        long,
        value_name = "PLAN.json",
        conflicts_with_all = ["name", "postgres", "yes"]
    )]
    pub apply: Option<PathBuf>,
    /// Also apply this item, by id (repeatable); high-confidence items are applied anyway
    #[arg(long, value_name = "ID")]
    pub accept: Vec<String>,
    /// Do not apply this item, by id (repeatable)
    #[arg(long, value_name = "ID")]
    pub reject: Vec<String>,
    /// Apply only the items named with --accept
    #[arg(long)]
    pub no_defaults: bool,
    /// Do not ask: plan, accept the high-confidence items and apply
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Also install the post-checkout Git hook
    #[arg(long)]
    pub hook: bool,
}

pub fn run(ctx: &Ctx, args: InitArgs) -> Result<ExitCode> {
    let root = project::main_checkout(&ctx.cwd)?;
    if args.plan {
        return plan_only(ctx, &root, &args);
    }
    let writes = args.apply.is_some() || args.yes;
    if !writes && (!args.accept.is_empty() || !args.reject.is_empty() || args.no_defaults) {
        bail!("--accept, --reject and --no-defaults need --apply or --yes");
    }
    if let Some(file) = &args.apply {
        let plan = read_plan(file)?;
        let selection = plan.select(&args.accept, &args.reject, !args.no_defaults)?;
        return write(ctx, &root, &plan, selection, &args);
    }
    if args.yes {
        let plan = make_plan(&root, &args)?;
        let selection = plan.select(&args.accept, &args.reject, !args.no_defaults)?;
        return write(ctx, &root, &plan, selection, &args);
    }
    if !ctx.interactive() {
        bail!(
            "`devflow init` asks what to accept and needs a terminal. Use `devflow init --plan` \
             to see the proposal, `devflow init --apply PLAN.json` to apply a saved one, or \
             `devflow init --yes` to apply the high-confidence items"
        );
    }
    let plan = make_plan(&root, &args)?;
    say(render_plan(&plan, &root, Palette::stdout()));
    let Some(selection) = ask(&plan)? else {
        return Ok(ExitCode::SUCCESS);
    };
    write(ctx, &root, &plan, selection, &args)
}

fn make_plan(root: &Path, args: &InitArgs) -> Result<Proposal> {
    let options = PlanOptions {
        name: args.name.clone(),
        postgres: args.postgres.map(Into::into),
        project_id: None,
    };
    propose::propose(root, &options)
}

// --- --plan --------------------------------------------------------------

fn plan_only(ctx: &Ctx, root: &Path, args: &InitArgs) -> Result<ExitCode> {
    let plan = make_plan(root, args)?;
    if ctx.json {
        output::emit(ctx, "init", &plan)?;
    } else {
        say(render_plan(&plan, root, Palette::stdout()));
        note(
            "Nothing was written. Save the plan with `devflow --json init --plan > plan.json` and \
             apply it with `devflow init --apply plan.json`, or run `devflow init`.",
        );
    }
    Ok(ExitCode::SUCCESS)
}

// --- --apply -------------------------------------------------------------

/// A plan saved from `devflow --json init --plan`: the proposal inside the
/// versioned envelope.
fn read_plan(file: &Path) -> Result<Proposal> {
    let text = if file == Path::new("-") {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .context("cannot read the plan from standard input")?;
        text
    } else {
        std::fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))?
    };
    let mut value: Value =
        serde_json::from_str(&text).with_context(|| format!("{} is not JSON", file.display()))?;
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        bail!(
            "{} holds an error document, not a plan: {}",
            file.display(),
            redact::text(error)
        );
    }
    if let Value::Object(map) = &mut value {
        // The envelope of `--json`.
        map.remove("version");
        map.remove("command");
    }
    // A plan carries proposed file contents: a wrong shape is described by its
    // key path and kind, never by what the plan holds.
    let plan: Proposal = devflow_core::diag::json_from_value(
        value,
        &format!("{} is not a devflow init plan", file.display()),
    )
    .map_err(|error| anyhow::anyhow!("{error:#}; make one with `devflow --json init --plan`"))?;
    plan.validate()
        .with_context(|| format!("cannot use {}", file.display()))?;
    Ok(plan)
}

// --- writing -------------------------------------------------------------

fn write(
    ctx: &Ctx,
    root: &Path,
    plan: &Proposal,
    selection: Selection,
    args: &InitArgs,
) -> Result<ExitCode> {
    let report = propose::apply(root, plan, &selection.accepted)?;
    // The project is usable even if the optional hook cannot be installed.
    let project = Project::open(root).ok();
    if let Some(project) = &project {
        ctx.register(project);
    }
    let hook = match (&project, args.hook) {
        (Some(project), true) => Some(githook::install(project)),
        (None, true) => Some(Err(anyhow::anyhow!(
            "the project has no valid devflow.toml yet"
        ))),
        _ => None,
    };
    let steps = next_steps(&report, plan, &selection);
    if ctx.json {
        let hook = hook.as_ref().map(|result| match result {
            Ok(path) => json!({ "installed": true, "path": path }),
            Err(error) => json!({
                "installed": false,
                "error": format!("{error:#}"),
                "remedies": devflow_core::remedy::describe(&devflow_core::remedy::remedies_of(error)),
            }),
        });
        let body = json!({
            "project": project.as_ref().map(ProjectInfo::of),
            "config": project.as_ref().map(|p| &p.config_path),
            "postgres": project.as_ref().and_then(|p| p.config.postgres().map(|(_, pg)| pg.mode)),
            // Every configured service (the keys and engines only).
            "services": project.as_ref().map(|p| p.config.services.iter()
                .map(|(key, service)| json!({ "key": key, "engine": service.engine() }))
                .collect::<Vec<_>>()),
            "accepted": selection.accepted,
            "dropped": selection.dropped,
            "applied": report.applied,
            "manual": report.manual,
            "files": report.files,
            "resumed": report.resumed,
            "already_applied": report.already_applied,
            "journal": report.journal,
            "hook": hook,
            "next_steps": steps,
        });
        output::emit(ctx, "init", &body)?;
        return Ok(ExitCode::SUCCESS);
    }
    say(render_report(&report, &selection, root, Palette::stdout()));
    match hook {
        Some(Ok(path)) => note(format!(
            "Installed the post-checkout hook at {}",
            path.display()
        )),
        Some(Err(error)) => {
            note(format!(
                "warning: could not install the Git hook: {}",
                redact::text(&format!("{error:#}"))
            ));
            for hint in devflow_core::remedy::remedies_of(&error)
                .iter()
                .filter_map(devflow_core::remedy::Remedy::hint)
            {
                note(format!("hint: {}", redact::text(&hint)));
            }
        }
        None => {}
    }
    if !steps.is_empty() {
        note("Next:");
    }
    for (index, step) in steps.iter().enumerate() {
        note(format!("  {}. {step}", index + 1));
    }
    Ok(ExitCode::SUCCESS)
}

fn next_steps(report: &ApplyReport, plan: &Proposal, selection: &Selection) -> Vec<String> {
    let mut steps = Vec::new();
    let names: Vec<&str> = report.files.iter().map(|f| f.file.as_str()).collect();
    if !names.is_empty() {
        steps.push(format!(
            "Review {} and commit {}.",
            if names.len() == 1 { "it" } else { "them" },
            names.join(", ")
        ));
    }
    for id in &report.manual {
        if let Some(item) = plan.item(id) {
            steps.push(format!("By hand: {}", plain(&item.title)));
        }
    }
    let skipped = plan
        .items
        .iter()
        .filter(|item| item.is_actionable() && !selection.accepted.contains(&item.id))
        .count();
    if skipped > 0 {
        steps.push(format!(
            "{skipped} proposed item(s) were not applied; `devflow init --plan` lists them (accept \
             one with `--accept ID`)."
        ));
    }
    let containers = report.applied.iter().any(|id| {
        plan.item(id).is_some_and(|item| {
            item.kind == ItemKind::Service
                && item
                    .value
                    .get("engine")
                    .and_then(Value::as_str)
                    .is_some_and(|engine| engine != "postgres")
        })
    });
    if containers {
        steps.push(
            "Valkey, Redis and MySQL services run in Docker, one container per workspace: \
             `devflow service status` checks the engine and copy-on-write."
                .to_string(),
        );
    }
    if !names.is_empty() || report.already_applied {
        steps.push(
            "`devflow trust` shows what the hooks, [env] and pitchfork daemons run and approves \
             it."
            .to_string(),
        );
        steps.push("`devflow switch -c NAME` creates a workspace.".to_string());
    }
    steps
}

fn render_report(
    report: &ApplyReport,
    selection: &Selection,
    root: &Path,
    palette: Palette,
) -> String {
    let mut lines = Vec::new();
    if report.files.is_empty() {
        lines.push(if report.manual.is_empty() {
            "Nothing to write: the configuration already has what was accepted.".to_string()
        } else {
            "Nothing to write.".to_string()
        });
    }
    for file in &report.files {
        let (mark, verb) = match file.status {
            FileStatus::Created => (palette.green("+"), "created"),
            FileStatus::Modified => (palette.green("~"), "updated"),
            FileStatus::AlreadyApplied => (palette.dim("="), "already up to date"),
        };
        lines.push(format!(
            "{mark} {} {verb}",
            output::tilde(&root.join(&file.file))
        ));
    }
    if report.resumed {
        lines.push(palette.dim("Continued an interrupted apply."));
    }
    for dropped in &selection.dropped {
        lines.push(palette.yellow(&format!(
            "- {} left out: {}",
            plain(&dropped.id),
            plain(&dropped.reason)
        )));
    }
    lines.join("\n")
}

/// `text` for a terminal: control characters (an escape sequence in a script
/// or service name of a hostile repository, say) are shown escaped, never
/// interpreted.
fn plain(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

// --- rendering the plan ---------------------------------------------------

fn label(item: &Item, known: &[&str], palette: Palette) -> String {
    let blocked = item.conflicts.iter().any(|c| !known.contains(&c.as_str()));
    let (text, painted) = if blocked {
        ("conflict", palette.red("conflict"))
    } else if !item.is_actionable() {
        ("manual", palette.yellow("manual"))
    } else {
        match item.confidence {
            Confidence::High => ("high", palette.green("high")),
            Confidence::Medium => ("medium", palette.yellow("medium")),
            Confidence::Low => ("low", palette.dim("low")),
        }
    };
    format!("{painted}{}", " ".repeat(8usize.saturating_sub(text.len())))
}

/// The plan as people read it: items grouped by what they configure, with
/// their confidence, evidence, risks and conflicts.
pub(crate) fn render_plan(plan: &Proposal, root: &Path, palette: Palette) -> String {
    let known: Vec<&str> = plan.items.iter().map(|i| i.id.as_str()).collect();
    let mut groups: BTreeMap<ItemKind, Vec<&Item>> = BTreeMap::new();
    for item in &plan.items {
        groups.entry(item.kind).or_default().push(item);
    }
    let count = |confidence: Confidence| {
        plan.items
            .iter()
            .filter(|i| i.is_actionable() && i.confidence == confidence)
            .count()
    };
    let attention = plan.items.iter().filter(|i| !i.is_actionable()).count();
    let mut out = Vec::new();
    out.push(format!(
        "Proposed setup for {} ({} high, {} medium, {} low confidence{})",
        output::tilde(root),
        count(Confidence::High),
        count(Confidence::Medium),
        count(Confidence::Low),
        if attention > 0 {
            format!(", {attention} needing a manual edit")
        } else {
            String::new()
        }
    ));
    if plan.items.is_empty() {
        out.push(String::new());
        out.push("Nothing to propose: the configuration already has what detection found.".into());
    }
    for (kind, items) in groups {
        out.push(String::new());
        out.push(kind.heading().to_string());
        for item in items {
            out.push(format!(
                "  {} {}  {}",
                label(item, &known, palette),
                palette.dim(&plain(&item.id)),
                plain(&item.title)
            ));
            let indent = "            ";
            if !item.evidence.is_empty() {
                let evidence: Vec<String> = item.evidence.iter().map(|e| plain(e)).collect();
                let shown: Vec<&str> = evidence.iter().take(4).map(String::as_str).collect();
                let more = item.evidence.len().saturating_sub(4);
                out.push(palette.dim(&format!(
                    "{indent}evidence: {}{}",
                    shown.join(", "),
                    if more > 0 {
                        format!(" (+{more} more)")
                    } else {
                        String::new()
                    }
                )));
            }
            // Everything in a new devflow.toml needs the item that creates it;
            // saying so on every line is noise.
            let needs: Vec<String> = item
                .depends_on
                .iter()
                .filter(|id| id.as_str() != "project:init")
                .map(|id| plain(id))
                .collect();
            if !needs.is_empty() {
                out.push(palette.dim(&format!("{indent}needs: {}", needs.join(", "))));
            }
            for risk in &item.risks {
                out.push(format!(
                    "{indent}{}",
                    palette.yellow(&format!("! {}", plain(risk)))
                ));
            }
            for conflict in &item.conflicts {
                out.push(format!(
                    "{indent}{}",
                    palette.red(&format!("x {}", plain(conflict)))
                ));
            }
        }
    }
    if !plan.already_configured.is_empty() {
        out.push(String::new());
        out.push(format!(
            "Already configured: {}",
            plan.already_configured
                .iter()
                .map(|id| plain(id))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !plan.warnings.is_empty() {
        out.push(String::new());
        out.push("Warnings".to_string());
        out.extend(
            plan.warnings
                .iter()
                .map(|w| format!("  {}", palette.yellow(&format!("! {}", plain(w))))),
        );
    }
    redact::text(&out.join("\n"))
}

// --- asking ---------------------------------------------------------------

/// One choice in a multi-select.
struct Choice {
    id: String,
    text: String,
}

impl fmt::Display for Choice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

fn cancelled(error: InquireError) -> anyhow::Error {
    match error {
        InquireError::OperationCanceled | InquireError::OperationInterrupted => {
            anyhow::anyhow!("cancelled")
        }
        other => other.into(),
    }
}

fn confirm(question: &str, default: bool) -> Result<bool> {
    inquire::Confirm::new(question)
        .with_default(default)
        .prompt()
        .map_err(cancelled)
}

fn pick(question: &str, items: &[&Item], preselect: bool) -> Result<Vec<String>> {
    let choices: Vec<Choice> = items
        .iter()
        .map(|item| Choice {
            id: item.id.clone(),
            text: format!("{}  {}", plain(&item.id), plain(&item.title)),
        })
        .collect();
    let all: Vec<usize> = (0..choices.len()).collect();
    let mut prompt = inquire::MultiSelect::new(question, choices);
    if preselect {
        prompt = prompt.with_default(&all);
    }
    Ok(prompt
        .prompt()
        .map_err(cancelled)?
        .into_iter()
        .map(|c| c.id)
        .collect())
}

/// Ask what to accept: all the high-confidence items at once, then the
/// medium and low ones. `None` when the person declines to write anything.
fn ask(plan: &Proposal) -> Result<Option<Selection>> {
    let known: Vec<&str> = plan.items.iter().map(|i| i.id.as_str()).collect();
    let applicable = |confidence: Confidence| -> Vec<&Item> {
        plan.items
            .iter()
            .filter(|item| {
                item.is_actionable()
                    && item.confidence == confidence
                    && item
                        .conflicts
                        .iter()
                        .all(|conflict| known.contains(&conflict.as_str()))
            })
            .collect()
    };
    let mut chosen: Vec<String> = Vec::new();

    let high = applicable(Confidence::High);
    if !high.is_empty() {
        if confirm(
            &format!(
                "Accept {} of high confidence?",
                devflow_core::text::counted(high.len() as u64, "item")
            ),
            true,
        )? {
            chosen.extend(high.iter().map(|i| i.id.clone()));
        } else {
            chosen.extend(pick("High-confidence items to apply", &high, true)?);
        }
    }
    let medium = applicable(Confidence::Medium);
    if !medium.is_empty() {
        chosen.extend(pick(
            "Medium confidence: likely right, but they depend on something detection cannot see",
            &medium,
            false,
        )?);
    }
    let low = applicable(Confidence::Low);
    if !low.is_empty() {
        chosen.extend(pick("Low confidence: guesses", &low, false)?);
    }
    if chosen.is_empty() {
        note("Nothing selected; nothing was written.");
        return Ok(None);
    }

    // Whatever an accepted item needs comes with it.
    let closed = plan.with_dependencies(&chosen);
    let added: Vec<&String> = closed.iter().filter(|id| !chosen.contains(id)).collect();
    if !added.is_empty() {
        note(format!(
            "Also adding what the selection needs: {}",
            added
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let files: Vec<&str> = {
        let mut files: Vec<&str> = Vec::new();
        for item in plan.items.iter().filter(|i| closed.contains(&i.id)) {
            if item.is_actionable() && !files.contains(&item.patch.file.as_str()) {
                files.push(&item.patch.file);
            }
        }
        files
    };
    if !confirm(
        &format!(
            "Write {} ({})?",
            files.join(", "),
            devflow_core::text::counted(closed.len() as u64, "item")
        ),
        true,
    )? {
        note("Nothing was written.");
        return Ok(None);
    }
    Ok(Some(Selection {
        accepted: closed,
        dropped: Vec::new(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_escapes_in_a_plan_are_shown_not_interpreted() {
        assert_eq!(plain("a\u{1b}[2Jb\n"), "a\\u{1b}[2Jb\\n");
        let json = r#"{
          "proposal_version": 1, "schema_version": 1, "input_hashes": {},
          "items": [
            {"id": "daemon:x", "kind": "daemon", "title": "Run \u001b[31mred",
             "value": {}, "confidence": "low", "evidence": ["a\u001b[2Jb"], "risks": ["\u0007bell"],
             "depends_on": [], "conflicts": [],
             "patch": {"file": "pitchfork.toml", "ops": []}}
          ],
          "already_configured": [], "warnings": ["\u001b]0;title\u0007"]
        }"#;
        let plan: Proposal = serde_json::from_str(json).unwrap();
        let text = render_plan(&plan, Path::new("/work/shop"), Palette::stdout());
        assert!(
            !text.contains('\u{1b}') && !text.contains('\u{7}'),
            "{text:?}"
        );
        assert!(text.contains("\\u{1b}[31mred"), "{text}");
    }

    #[test]
    fn plans_render_grouped_with_labels_and_hide_nothing_important() {
        let json = r#"{
          "proposal_version": 1, "schema_version": 1,
          "input_hashes": {"devflow.toml": null},
          "items": [
            {"id": "project:init", "kind": "project", "title": "Create devflow.toml",
             "value": {}, "confidence": "high", "evidence": [".git"], "risks": [],
             "depends_on": [], "conflicts": [],
             "patch": {"file": "devflow.toml", "ops": [{"op": "set", "path": ["a"], "value": 1}]}},
            {"id": "mise:env-file", "kind": "mise", "title": "Load .devflow/env",
             "value": {}, "confidence": "low", "evidence": [], "risks": ["be careful"],
             "depends_on": [], "conflicts": ["env._.file is already set"],
             "patch": {"file": "mise.toml", "ops": []}}
          ],
          "already_configured": ["copy:.env"], "warnings": ["something odd"]
        }"#;
        let plan: Proposal = serde_json::from_str(json).unwrap();
        let text = render_plan(&plan, Path::new("/work/shop"), Palette::stdout());
        assert!(text.contains("1 high, 0 medium, 0 low confidence, 1 needing a manual edit"));
        assert!(text.contains("Project\n  high"), "{text}");
        assert!(text.contains("conflict"), "{text}");
        assert!(text.contains("! be careful"));
        assert!(text.contains("x env._.file is already set"));
        assert!(text.contains("Already configured: copy:.env"));
        assert!(text.contains("! something odd"));
    }
}
