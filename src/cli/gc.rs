//! `devflow gc`: workspaces that Git or an agent host removed, and what they
//! still own. Core classifies (`devflow_core::gc`); this prints the inventory,
//! asks, and passes the instances the person named, with the generation they
//! were shown, back to core. Nothing is removed unless `--remove` names it.
use super::output::{self, Palette, note, say};
use super::{Ctx, ProjectInfo};
use anyhow::Result;
use clap::Args;
use devflow_core::gc::{
    self, Branch, Cause, Class, Entry, Inventory, Presence, RebindRequest, RemoveRequest, Target,
};
use devflow_core::ids::IdRef;
use devflow_core::redact;
use devflow_core::remedy::Remedy;
use serde_json::json;
use std::process::ExitCode;

#[derive(Args)]
#[command(group = clap::ArgGroup::new("act").args(["remove", "rebind"]))]
pub struct GcArgs {
    /// Remove these workspaces (the ids shown, or at least 8 characters of one)
    #[arg(long, value_name = "ID", num_args = 1..)]
    pub remove: Vec<String>,
    /// Confirm that the records of these workspaces belong to this repository
    /// (written for a copy of it, a restored backup, or a move nothing proves),
    /// so that they can be cleaned up; nothing is removed by it
    #[arg(long, value_name = "ID", num_args = 1..)]
    pub rebind: Vec<String>,
    /// Act without asking; the selection stays exactly what --remove or --rebind names
    #[arg(short = 'y', long, requires = "act")]
    pub yes: bool,
    /// Snapshot each data branch first (named gc-WORKSPACE-DATE)
    #[arg(long, requires = "remove")]
    pub snapshot: bool,
    /// Go on past what cannot be verified (daemons whose state cannot be read, a drop
    /// that fails); it stays recorded
    #[arg(long, requires = "remove")]
    pub force: bool,
    /// List every workspace, not only those that own something or need a decision
    #[arg(long)]
    pub all: bool,
}

pub fn run(ctx: &Ctx, args: GcArgs) -> Result<ExitCode> {
    let project = ctx.project()?;
    let inventory = gc::inventory(&project, &gc::Options::default())?;
    if args.remove.is_empty() && args.rebind.is_empty() {
        if ctx.json {
            let body = json!({ "project": ProjectInfo::of(&project), "inventory": inventory });
            output::emit(ctx, "gc", &body)?;
        } else {
            print_inventory(&inventory, args.all);
        }
        return Ok(ExitCode::SUCCESS);
    }

    if !args.rebind.is_empty() {
        return rebind_records(ctx, &project, &inventory, &args);
    }

    // The selection: exactly the ids named, resolved against what is shown.
    let mut chosen: Vec<&Entry> = Vec::new();
    for typed in &args.remove {
        let id: IdRef = typed.parse()?;
        let instance = inventory.resolve(&id)?;
        let entry = inventory
            .entry(instance)
            .expect("an id resolved from the inventory names an entry");
        if !chosen.iter().any(|c| c.instance == entry.instance) {
            chosen.push(entry);
        }
    }
    let (removable, refused): (Vec<&Entry>, Vec<&Entry>) = chosen
        .iter()
        .copied()
        .partition(|e| e.removable && e.target().is_some());

    if !ctx.json {
        // What is about to go is shown before it is asked about; a run that was
        // told to go ahead says what it did instead.
        if !args.yes {
            print_inventory(&inventory, args.all);
        }
        for entry in &refused {
            note(format!(
                "! not removing {} ({}): {}",
                entry.workspace,
                entry.short_id().unwrap_or_default(),
                redact::text(
                    entry
                        .not_removable
                        .as_deref()
                        .unwrap_or("it cannot be removed")
                )
            ));
            for remedy in &entry.remedies {
                if let Some(hint) = remedy_hint(remedy) {
                    note(format!("  hint: {}", redact::text(&hint)));
                }
            }
        }
    }
    let confirmed = if removable.is_empty() {
        false
    } else if args.yes {
        true
    } else if ctx.interactive() {
        confirm(&confirmation(&removable, args.snapshot))?
    } else {
        false
    };
    let targets: Vec<Target> = removable.iter().filter_map(|e| e.target()).collect();
    let outcome = if confirmed {
        let request = RemoveRequest {
            targets,
            snapshot: args.snapshot,
            force: args.force,
            pitchfork: None,
        };
        gc::remove(&project, &request, &mut |line| {
            if !ctx.json {
                note(line)
            }
        })?
    } else {
        gc::Outcome::default()
    };

    let kept_back = !refused.is_empty() || !outcome.skipped.is_empty();
    if ctx.json {
        let mut body = json!({
            "project": ProjectInfo::of(&project),
            "inventory": inventory,
            "confirmed": confirmed,
            "removed": outcome.removed,
            "skipped": outcome.skipped,
            "not_removable": refused
                .iter()
                .map(|e| json!({
                    "instance": e.instance,
                    "workspace": e.workspace,
                    "reason": e.not_removable,
                    "remedies": devflow_core::remedy::describe(&e.remedies),
                }))
                .collect::<Vec<_>>(),
        });
        if kept_back {
            let mut reasons: Vec<String> = outcome
                .skipped
                .iter()
                .map(|s| {
                    format!(
                        "{}: {}",
                        s.workspace.as_deref().unwrap_or("(unknown)"),
                        s.reason
                    )
                })
                .collect();
            reasons.extend(refused.iter().map(|e| {
                format!(
                    "{}: {}",
                    e.workspace,
                    e.not_removable.as_deref().unwrap_or("it cannot be removed")
                )
            }));
            body["error"] = serde_json::Value::String(format!("kept: {}", reasons.join("; ")));
        }
        output::emit(ctx, "gc", &body)?;
    } else {
        let palette = Palette::stdout();
        for removed in &outcome.removed {
            say(format!(
                "{} Removed {}",
                palette.green("✓"),
                removed.workspace
            ));
            for line in &removed.snapshots {
                say(format!("  snapshot {line}"));
            }
            for line in &removed.dropped {
                say(format!("  {line}"));
            }
            for warning in &removed.warnings {
                note(format!("  warning: {}", redact::text(warning)));
            }
            if let Some(may) = &removed.may_still_run {
                note(format!(
                    "  warning: daemons may still be running{}{}",
                    may.namespace
                        .as_deref()
                        .map(|namespace| format!(" under the namespace {namespace}"))
                        .unwrap_or_default(),
                    if may.ids.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", may.ids.join(", "))
                    }
                ));
            }
            if let Some(released) = &removed.released_focus {
                say(format!("  {}", released.describe()));
            }
            if !removed.still_recorded.is_empty() {
                note(format!(
                    "  kept on record for a later clean-up: {}",
                    removed.still_recorded.join(", ")
                ));
            }
        }
        for skipped in &outcome.skipped {
            say(format!(
                "{} kept {}: {}",
                palette.yellow("!"),
                skipped.workspace.as_deref().unwrap_or("(unknown)"),
                redact::text(&skipped.reason)
            ));
            if !skipped.done.is_empty() {
                note(format!("  already done: {}", skipped.done.join("; ")));
            }
            if skipped.forceable && !args.force {
                note(
                    "hint: pass --force to go on past this; whatever cannot be verified stays recorded",
                );
            }
        }
        if !confirmed && !removable.is_empty() {
            note("hint: pass --yes to remove them without being asked");
        }
    }
    Ok(if kept_back {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// `gc --rebind`: the person confirms that records written for another
/// repository directory belong to this one. Nothing is removed.
fn rebind_records(
    ctx: &Ctx,
    project: &devflow_core::project::Project,
    inventory: &Inventory,
    args: &GcArgs,
) -> Result<ExitCode> {
    let mut chosen: Vec<&Entry> = Vec::new();
    for typed in &args.rebind {
        let id: IdRef = typed.parse()?;
        let instance = inventory.resolve(&id)?;
        let entry = inventory
            .entry(instance)
            .expect("an id resolved from the inventory names an entry");
        if !chosen.iter().any(|c| c.instance == entry.instance) {
            chosen.push(entry);
        }
    }
    let (bindable, refused): (Vec<&Entry>, Vec<&Entry>) = chosen
        .iter()
        .copied()
        .partition(|e| e.rebindable() && e.target().is_some());
    if !ctx.json {
        if !args.yes {
            print_inventory(inventory, args.all);
        }
        for entry in &refused {
            note(format!(
                "! not binding {} ({}): its record is not waiting to be bound to this repository",
                entry.workspace,
                entry.short_id().unwrap_or_default(),
            ));
        }
    }
    let confirmed = if bindable.is_empty() {
        false
    } else if args.yes {
        true
    } else if ctx.interactive() {
        confirm(&rebind_question(&bindable))?
    } else {
        false
    };
    let outcome = if confirmed {
        let request = RebindRequest {
            targets: bindable.iter().filter_map(|e| e.target()).collect(),
        };
        gc::rebind(project, &request, &mut |line| {
            if !ctx.json {
                note(line)
            }
        })?
    } else {
        gc::RebindOutcome::default()
    };
    let kept_back = !refused.is_empty() || !outcome.skipped.is_empty();
    if ctx.json {
        let mut body = json!({
            "project": ProjectInfo::of(project),
            "inventory": inventory,
            "confirmed": confirmed,
            "rebound": outcome.rebound,
            "skipped": outcome.skipped,
            "not_bindable": refused
                .iter()
                .map(|e| json!({ "instance": e.instance, "workspace": e.workspace }))
                .collect::<Vec<_>>(),
        });
        if kept_back {
            body["error"] = serde_json::Value::String(
                "some records were not bound (see skipped and not_bindable)".to_string(),
            );
        }
        output::emit(ctx, "gc", &body)?;
    } else {
        let palette = Palette::stdout();
        for rebound in &outcome.rebound {
            say(format!(
                "{} Bound the record of {} to this repository ({})",
                palette.green("✓"),
                rebound.workspace,
                rebound.class.label()
            ));
        }
        for skipped in &outcome.skipped {
            say(format!(
                "{} kept {}: {}",
                palette.yellow("!"),
                skipped.workspace.as_deref().unwrap_or("(unknown)"),
                redact::text(&skipped.reason)
            ));
        }
        if !confirmed && !bindable.is_empty() {
            note("hint: pass --yes to bind them without being asked");
        }
        if !outcome.rebound.is_empty() {
            note("hint: they can be cleaned up like any other record now (`devflow gc`)");
        }
    }
    Ok(if kept_back {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// The question: whose record it is, in words.
fn rebind_question(entries: &[&Entry]) -> String {
    let written_for = |entry: &Entry| match &entry.cause {
        Some(Cause::OtherCheckout { common_dir }) => common_dir.display().to_string(),
        _ => "another repository".to_string(),
    };
    let what = match entries {
        [one] => format!(
            "the record of workspace {} (written for {})",
            one.workspace,
            written_for(one)
        ),
        many => format!("{} records", many.len()),
    };
    format!(
        "Bind {what} to this repository? It may then be cleaned up, which drops the data it names; \
         bind it only if it is yours and the repository it was written for no longer uses that \
         data. Binding removes nothing."
    )
}

/// The question: what goes, in words.
fn confirmation(entries: &[&Entry], snapshot: bool) -> String {
    let branches: usize = entries.iter().map(|e| e.branches.len()).sum();
    let daemons: usize = entries.iter().map(|e| e.daemons.len()).sum();
    let owned = match branches {
        0 => "it owns no data branch".to_string(),
        1 => "drop 1 data branch".to_string(),
        n => format!("drop {n} data branches"),
    };
    format!(
        "Remove {} ({owned}{}){}? {}",
        match entries {
            [one] => format!("workspace {}", one.workspace),
            many => format!("{} workspaces", many.len()),
        },
        // Daemons are only ever asked about: pitchfork would run their
        // lifecycle hooks from a configuration nobody approved.
        if daemons > 0 {
            "; their daemons are not stopped, and must not be running"
        } else {
            ""
        },
        if snapshot { ", snapshotting first" } else { "" },
        if snapshot {
            "The snapshots are kept."
        } else {
            "Dropped data cannot be recovered."
        }
    )
}

// --- the inventory ------------------------------------------------------

fn print_inventory(inventory: &Inventory, all: bool) {
    let palette = Palette::stdout();
    let shown: Vec<&Entry> = inventory
        .entries
        .iter()
        .filter(|e| all || e.class != Class::Active || e.holds_resources() && e.removing)
        .collect();
    if shown.is_empty() {
        say("No workspace was removed outside devflow, and none is missing or unknown.");
        let hidden = inventory.entries.len();
        if hidden > 0 {
            note(format!(
                "({hidden} active workspace{} not listed; --all shows them)",
                if hidden == 1 { "" } else { "s" }
            ));
        }
    } else {
        let rows: Vec<Vec<String>> = shown.iter().map(|e| row(e)).collect();
        say(output::table(
            &["ID", "WORKSPACE", "STATE", "LAST ACTIVE", "OWNS", "MERGED"],
            &rows,
        ));
        for entry in &shown {
            details(entry, palette);
        }
    }
    for line in &inventory.notes {
        note(format!("note: {}", redact::text(line)));
    }
    if inventory.removable().next().is_some() {
        note(
            "hint: remove the ones you do not need with `devflow gc --remove ID…` (`--snapshot` keeps a snapshot of each data branch first)",
        );
    }
}

fn row(entry: &Entry) -> Vec<String> {
    let mut state = entry.class.label().to_string();
    if entry.removing {
        state.push_str(", removal unfinished");
    }
    vec![
        entry.short_id().unwrap_or_else(|| "-".to_string()),
        entry.workspace.clone(),
        state,
        entry
            .last_active_at
            .or(entry.created_at)
            .map(devflow_core::text::ago)
            .unwrap_or_else(|| "-".to_string()),
        owns(entry),
        entry
            .merge
            .as_ref()
            .map(|m| m.summary())
            .unwrap_or_else(|| "-".to_string()),
    ]
}

fn owns(entry: &Entry) -> String {
    let mut parts = Vec::new();
    let branches = entry.branches.len();
    if branches > 0 {
        parts.push(format!(
            "{branches} branch{}",
            if branches == 1 { "" } else { "es" }
        ));
    }
    if !entry.daemons.is_empty() {
        parts.push(format!(
            "{} daemon{}",
            entry.daemons.len(),
            if entry.daemons.len() == 1 { "" } else { "s" }
        ));
    }
    if parts.is_empty() {
        "nothing".to_string()
    } else {
        parts.join(", ")
    }
}

fn details(entry: &Entry, palette: Palette) {
    let id = entry.short_id().unwrap_or_else(|| "-".to_string());
    if entry.branches.is_empty()
        && entry.daemons.is_empty()
        && entry.reason.is_none()
        && entry.class == Class::Active
    {
        return;
    }
    say(format!(
        "\n{} {}  {}",
        palette.dim(&id),
        entry.workspace,
        palette.dim(&output::tilde(&entry.path))
    ));
    if let Some(reason) = &entry.reason {
        say(format!("  {}", redact::text(reason)));
    }
    if let Some(recorded) = &entry.recorded_path {
        say(format!(
            "  the record has it at {}",
            output::tilde(recorded)
        ));
    }
    for branch in &entry.branches {
        say(format!("  {}", describe(branch)));
    }
    for daemon in &entry.daemons {
        say(format!("  daemon {daemon}"));
    }
    for remedy in &entry.remedies {
        if let Some(hint) = remedy_hint(remedy) {
            say(format!("  hint: {}", redact::text(&hint)));
        }
    }
}

/// Only the remedies whose hint names something a person can do from here.
fn remedy_hint(remedy: &Remedy) -> Option<String> {
    match remedy {
        Remedy::Retry | Remedy::Reveal { .. } => None,
        other => other.hint(),
    }
}

fn describe(branch: &Branch) -> String {
    let mut line = branch.describe();
    if branch.pending {
        line.push_str(" (being created when setup stopped)");
    }
    let mut facts: Vec<String> = Vec::new();
    match &branch.presence {
        Presence::Owned => {}
        Presence::Foreign => facts.push("another instance's, left alone".to_string()),
        Presence::Absent => facts.push("not there any more".to_string()),
        Presence::Unreachable { reason } => facts.push(format!("cannot be asked: {reason}")),
        Presence::NotChecked => {}
    }
    if let Some(size) = branch.size_bytes {
        facts.push(output::bytes(size));
    }
    if let Some(state) = &branch.container {
        facts.push(state.clone());
    }
    if let Some(memory) = branch.memory_bytes {
        facts.push(format!("{} memory", output::bytes(memory as i64)));
    }
    if !facts.is_empty() {
        line.push_str(&format!(" — {}", facts.join(", ")));
    }
    redact::text(&line)
}

fn confirm(prompt: &str) -> Result<bool> {
    match inquire::Confirm::new(prompt).with_default(false).prompt() {
        Ok(answer) => Ok(answer),
        Err(
            inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted,
        ) => anyhow::bail!("cancelled"),
        Err(error) => Err(error.into()),
    }
}
