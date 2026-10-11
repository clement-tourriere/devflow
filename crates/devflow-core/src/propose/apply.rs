//! Applying an accepted part of a proposal.
//!
//! The order is fixed: validate the plan and the accepted ids (dependencies,
//! conflicts), check that nothing the plan depends on changed, render every
//! file in memory and validate the results, record the intended hashes in a
//! journal, and only then write, one file at a time, atomically.
//!
//! Each file is read again immediately before it is replaced and must still
//! be exactly what its patch was rendered from; a file edited in the meantime
//! (by a person, an editor, another tool) is refused with an "edited since the
//! plan was made" error and never overwritten. That check cannot cover a save
//! that lands after it, so the replacement itself is conditional
//! (`fsutil::replace_if_unchanged`): the new content is staged, atomically
//! exchanged with the file, and the displaced content is compared with what
//! the patch was rendered from. A save in that window is put back or, if it
//! cannot be, kept next to the file as `<name>.devflow-conflict-<timestamp>`;
//! either way the apply stops and names the files. That guarantee needs the
//! atomic exchange (APFS, Linux filesystems with `RENAME_EXCHANGE`); on other
//! filesystems a narrow window between the last check and the rename remains,
//! and only the hard-link backup of the displaced file is kept.
//!
//! The journal (`<git common dir>/devflow/init-apply.json`, mode 0600) names
//! the plan by digest and the accepted ids, and lists each target file with
//! the hashes it must have before and after. A retry of the same plan with the
//! same ids recognizes a file already at its *after* hash and continues with
//! the next; a file at neither hash is somebody else's change and stops the
//! apply. A retry also keeps checking everything the plan depended on that the
//! apply itself does not change: the facts detection read from the repository
//! (scripts, lockfiles, compose, ...), as opposed to the ones the generated
//! configuration changes, and every mise configuration file (a
//! `.config/mise/config.local.toml` or `conf.d` file that appeared since
//! changes what the wiring means), by content and as a set. Only the files
//! the journal lists are left out of those. Applying again after it finished
//! is a no-op.
use super::existing::{DEVFLOW_FILES, LOCAL_FILE, MISE_LAYERS_KEY, read_current, sha256_hex};
use super::patch::{self, Effect, PatchOp};
use super::plan::{detection_hash, repository_hash};
use super::{DETECTION_KEY, ExistingFiles, ItemId, Proposal, REPOSITORY_KEY, journal_dir};
use crate::config::Config;
use crate::detect;
use crate::error::StalePlan;
use crate::fsutil::{
    ConflictKind, ReplaceError, ReplaceSeams, replace_if_unchanged_with, write_atomic,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const JOURNAL_FILE: &str = "init-apply.json";
const JOURNAL_VERSION: u32 = 1;

/// What happened to one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    /// The file did not exist and was written.
    Created,
    /// The file existed and the additions were written.
    Modified,
    /// The file was already in its final state (a retry or a second apply).
    AlreadyApplied,
}

/// One file of an apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileOutcome {
    pub file: String,
    pub status: FileStatus,
    /// SHA-256 before (`null`: the file did not exist).
    pub before: Option<String>,
    /// SHA-256 after.
    pub after: String,
}

/// The result of [`apply`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApplyReport {
    /// Accepted items whose changes are now in the files.
    pub applied: Vec<ItemId>,
    /// Accepted items that had nothing to write: they ask for work by hand.
    pub manual: Vec<ItemId>,
    pub files: Vec<FileOutcome>,
    /// The apply continued an interrupted one.
    pub resumed: bool,
    /// Every file was already in its final state; nothing was written.
    pub already_applied: bool,
    /// The apply journal.
    pub journal: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Journal {
    version: u32,
    plan_digest: String,
    accepted: Vec<ItemId>,
    state: JournalState,
    started_at: String,
    files: Vec<JournalFile>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JournalState {
    Applying,
    Complete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalFile {
    file: String,
    before: Option<String>,
    after: String,
    written: bool,
}

/// Apply the `accepted` items of `plan` to the project at `root`.
///
/// Writes configuration only: nothing is installed, trusted or run. A plan
/// that no longer describes the checkout is refused with a [`StalePlan`]
/// (also when an interrupted apply of it cannot continue); anything else that
/// goes wrong is another error.
pub fn apply(root: &Path, plan: &Proposal, accepted: &[ItemId]) -> Result<ApplyReport> {
    apply_with(root, plan, accepted, &Probes::default())
}

/// Apply and stop after `written` files, as a crash would: for tests of the
/// frontends that resume an interrupted apply.
#[doc(hidden)]
pub fn apply_interrupted_after(
    root: &Path,
    plan: &Proposal,
    accepted: &[ItemId],
    written: usize,
) -> Result<ApplyReport> {
    let probes = Probes {
        stop_after: Some(written),
        ..Probes::default()
    };
    apply_with(root, plan, accepted, &probes)
}

/// A refusal because the plan is out of date.
fn stale(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(StalePlan {
        message: message.into(),
        root: PathBuf::new(),
    })
}

/// Seams for tests: failing part-way, and acting between a file's last check
/// and its replacement.
#[derive(Default)]
pub(super) struct Probes<'a> {
    /// Fail after this many files were written, to test resuming.
    pub stop_after: Option<usize>,
    /// Called with the name of each file right before it is checked again and
    /// replaced: the moment a concurrent edit would land.
    pub before_replace: Option<&'a dyn Fn(&str)>,
    /// Called with the name of each file after its replacement is staged and
    /// before it is exchanged with the file: the last moment a concurrent
    /// edit would land.
    pub after_staging: Option<&'a dyn Fn(&str)>,
}

/// [`apply`] with test seams.
pub(super) fn apply_with(
    root: &Path,
    plan: &Proposal,
    accepted: &[ItemId],
    probes: &Probes,
) -> Result<ApplyReport> {
    apply_checked(root, plan, accepted, probes).map_err(|error| {
        // The refusal is raised far from the repository's path; say which one.
        match error.downcast::<StalePlan>() {
            Ok(stale) => anyhow::Error::new(StalePlan {
                root: root.to_path_buf(),
                ..stale
            }),
            Err(error) => error,
        }
    })
}

fn apply_checked(
    root: &Path,
    plan: &Proposal,
    accepted: &[ItemId],
    probes: &Probes,
) -> Result<ApplyReport> {
    plan.validate()?;
    let accepted = check_selection(plan, accepted)?;
    let dir = journal_dir(root)?;
    // Two `init --apply` at once would race on the files and the journal.
    crate::fsutil::with_lock(&dir.join("init-apply.lock"), || {
        apply_locked(root, plan, accepted, dir.join(JOURNAL_FILE), probes)
    })
}

fn apply_locked(
    root: &Path,
    plan: &Proposal,
    accepted: Vec<ItemId>,
    journal_path: PathBuf,
    probes: &Probes,
) -> Result<ApplyReport> {
    let existing = ExistingFiles::read(root)?;
    let current = existing.hashes();
    let digest = plan.digest();

    let previous = read_journal(&journal_path);
    let matching = previous
        .as_ref()
        .filter(|journal| journal.plan_digest == digest && journal.accepted == accepted);

    // The operations of each target file, in the order the plan lists them.
    let mut targets: Vec<(String, Vec<PatchOp>)> = Vec::new();
    let mut applied = Vec::new();
    let mut manual = Vec::new();
    // The compose hooks start what the accepted service items do not.
    let resolved = plan.resolved(&accepted);
    for item in &resolved.items {
        if !accepted.contains(&item.id) {
            continue;
        }
        if !item.is_actionable() {
            manual.push(item.id.clone());
            continue;
        }
        applied.push(item.id.clone());
        match targets
            .iter_mut()
            .find(|(file, _)| *file == item.patch.file)
        {
            Some((_, ops)) => ops.extend(item.patch.ops.clone()),
            None => targets.push((item.patch.file.clone(), item.patch.ops.clone())),
        }
    }

    if targets.is_empty() {
        // Only items that ask for work by hand (or nothing): nothing to write
        // and nothing to journal.
        return Ok(ApplyReport {
            applied,
            manual,
            files: Vec::new(),
            resumed: false,
            already_applied: false,
            journal: journal_path,
        });
    }

    if let Some(journal) = matching {
        return resume(
            root,
            plan,
            &existing,
            &current,
            journal.clone(),
            &targets,
            ApplyParts {
                applied,
                manual,
                path: journal_path,
                probes,
            },
        );
    }

    // A fresh apply: every input must be exactly what the plan saw.
    let mut changed = changed_inputs(plan, &current, &BTreeSet::new());
    if changed.is_empty() {
        let facts = detect::scan(root).context("cannot re-check the repository")?;
        if plan.input_hashes.get(DETECTION_KEY) != Some(&Some(detection_hash(&facts))) {
            changed.push(
                "the repository files detection reads (manifests, lockfiles, compose, .env key \
                 names, ...)"
                    .to_string(),
            );
        }
    }
    if !changed.is_empty() {
        let mut message = format!(
            "the repository changed since the plan was made; nothing was written. Changed: {}. \
             Make a new plan",
            changed.join(", ")
        );
        if let Some(journal) = previous.filter(|j| j.state == JournalState::Applying) {
            message.push_str(&format!(
                ". An earlier apply (plan {}) did not finish; retry it with the same plan and \
                 --accept, or review the files and delete {}",
                &journal.plan_digest[..12.min(journal.plan_digest.len())],
                journal_path.display()
            ));
        }
        return Err(stale(message));
    }

    // Render every file in memory and validate it before the first write.
    let mut rendered = Vec::new();
    for (file, ops) in &targets {
        let before = existing.text(file);
        let (after, effects) =
            patch::render(before, ops).with_context(|| format!("cannot edit {file}"))?;
        let conflicts: Vec<String> = effects
            .iter()
            .zip(ops)
            .filter_map(|(effect, op)| match effect {
                Effect::Conflict(why) => Some(format!("{} ({why})", op.target())),
                _ => None,
            })
            .collect();
        if !conflicts.is_empty() {
            bail!(
                "{file} has content that conflicts with the plan: {}; nothing was written",
                conflicts.join("; ")
            );
        }
        validate_rendered(&existing, file, &after)?;
        rendered.push((file.clone(), before.map(str::to_string), after));
    }

    let files: Vec<JournalFile> = rendered
        .iter()
        .filter(|(_, before, after)| before.as_deref() != Some(after.as_str()))
        .map(|(file, before, after)| JournalFile {
            file: file.clone(),
            before: before.as_deref().map(sha256_hex),
            after: sha256_hex(after),
            written: false,
        })
        .collect();
    let mut journal = Journal {
        version: JOURNAL_VERSION,
        plan_digest: digest,
        accepted,
        state: JournalState::Applying,
        started_at: chrono::Utc::now().to_rfc3339(),
        files,
    };
    save_journal(&journal_path, &journal)?;

    let mut outcomes = Vec::new();
    let mut written = 0;
    for (file, before, after) in &rendered {
        let Some(entry) = journal.files.iter().position(|f| f.file == *file) else {
            continue;
        };
        if let Some(probe) = probes.before_replace {
            probe(file);
        }
        let already: Vec<&str> = outcomes
            .iter()
            .map(|o: &FileOutcome| o.file.as_str())
            .collect();
        ensure_unedited(
            root,
            file,
            before.as_deref().map(sha256_hex).as_deref(),
            &journal_path,
            &already,
        )?;
        replace_file(
            root,
            file,
            before.as_deref().map(sha256_hex).as_deref(),
            after,
            &journal_path,
            &already,
            probes,
        )?;
        journal.files[entry].written = true;
        save_journal(&journal_path, &journal)?;
        written += 1;
        outcomes.push(FileOutcome {
            file: file.clone(),
            status: if before.is_some() {
                FileStatus::Modified
            } else {
                FileStatus::Created
            },
            before: before.as_deref().map(sha256_hex),
            after: sha256_hex(after),
        });
        if probes.stop_after.is_some_and(|limit| written >= limit) && written < journal.files.len()
        {
            bail!("interrupted after writing {written} file(s) (simulated)");
        }
    }
    journal.state = JournalState::Complete;
    save_journal(&journal_path, &journal)?;
    Ok(ApplyReport {
        applied,
        manual,
        files: outcomes,
        resumed: false,
        already_applied: false,
        journal: journal_path,
    })
}

struct ApplyParts<'a> {
    applied: Vec<ItemId>,
    manual: Vec<ItemId>,
    path: PathBuf,
    probes: &'a Probes<'a>,
}

/// Continue the apply that `journal` records.
fn resume(
    root: &Path,
    plan: &Proposal,
    existing: &ExistingFiles,
    current: &BTreeMap<String, Option<String>>,
    mut journal: Journal,
    targets: &[(String, Vec<PatchOp>)],
    parts: ApplyParts,
) -> Result<ApplyReport> {
    let ApplyParts {
        applied,
        manual,
        path,
        probes,
    } = parts;
    let journaled: BTreeSet<String> = journal.files.iter().map(|f| f.file.clone()).collect();
    // Files the apply does not write must still be what the plan saw.
    let mut problems = changed_inputs(plan, current, &journaled);
    let mut done = Vec::new();
    let mut todo = Vec::new();
    for entry in &journal.files {
        let now = current.get(&entry.file).cloned().flatten();
        if now.as_deref() == Some(entry.after.as_str()) {
            done.push(entry.clone());
        } else if now == entry.before {
            todo.push(entry.clone());
        } else {
            problems.push(format!(
                "{} (neither as planned nor as written by the earlier apply)",
                entry.file
            ));
        }
    }
    if !problems.is_empty() {
        if journal.state == JournalState::Complete {
            return Err(stale(format!(
                "this plan was already applied, and the files changed since: {}. Nothing was \
                 written. Make a new plan",
                problems.join(", ")
            )));
        }
        return Err(stale(format!(
            "an earlier apply of this plan is unfinished, but the files changed since: {}. \
             Nothing was written. Review the files, then delete {} to start over with a new \
             plan",
            problems.join(", "),
            path.display()
        )));
    }
    // Whatever is still to be written is written on the strength of the facts
    // the plan saw. Nothing written yet: all of them must match. Some files
    // written: those the generated configuration changes (wiring, mise's
    // `[env]`, pitchfork's daemons) no longer match, expectedly, but every
    // other fact (scripts, lockfiles, compose, `.env` key names, ...) must.
    if !todo.is_empty() {
        let facts = detect::scan(root).context("cannot re-check the repository")?;
        let unchanged = if done.is_empty() {
            plan.input_hashes.get(DETECTION_KEY) == Some(&Some(detection_hash(&facts)))
        } else {
            plan.input_hashes.get(REPOSITORY_KEY) == Some(&Some(repository_hash(&facts)))
        };
        if !unchanged {
            if done.is_empty() {
                return Err(stale(
                    "the repository files detection reads changed since the plan was made; \
                     nothing was written. Make a new plan",
                ));
            }
            return Err(stale(format!(
                "an earlier apply of this plan is unfinished, but the repository changed since \
                 the plan was made (the files detection reads: manifests, lockfiles, compose, \
                 .env key names, ...). Nothing more was written. Review the files, then delete \
                 {} to start over with a new plan",
                path.display()
            )));
        }
    }

    let resumed = !done.is_empty();
    let mut outcomes: Vec<FileOutcome> = done
        .iter()
        .map(|entry| FileOutcome {
            file: entry.file.clone(),
            status: FileStatus::AlreadyApplied,
            before: entry.before.clone(),
            after: entry.after.clone(),
        })
        .collect();

    let mut written = 0;
    for entry in &todo {
        let (_, ops) = targets
            .iter()
            .find(|(file, _)| *file == entry.file)
            .context("the journal names a file the accepted items do not edit")?;
        let before = existing.text(&entry.file);
        let (after, effects) = patch::render(before, ops)?;
        if let Some(Effect::Conflict(why)) =
            effects.iter().find(|e| matches!(e, Effect::Conflict(_)))
        {
            bail!(
                "{} has content that conflicts with the plan: {why}",
                entry.file
            );
        }
        if sha256_hex(&after) != entry.after {
            bail!(
                "{} would not come out as journaled; nothing more was written. Review it and \
                 delete {} to start over",
                entry.file,
                path.display()
            );
        }
        validate_rendered(existing, &entry.file, &after)?;
        if let Some(probe) = probes.before_replace {
            probe(&entry.file);
        }
        let already: Vec<&str> = outcomes.iter().map(|o| o.file.as_str()).collect();
        ensure_unedited(root, &entry.file, entry.before.as_deref(), &path, &already)?;
        replace_file(
            root,
            &entry.file,
            entry.before.as_deref(),
            &after,
            &path,
            &already,
            probes,
        )?;
        if let Some(slot) = journal.files.iter_mut().find(|f| f.file == entry.file) {
            slot.written = true;
        }
        save_journal(&path, &journal)?;
        written += 1;
        outcomes.push(FileOutcome {
            file: entry.file.clone(),
            status: if entry.before.is_some() {
                FileStatus::Modified
            } else {
                FileStatus::Created
            },
            before: entry.before.clone(),
            after: entry.after.clone(),
        });
        if probes.stop_after.is_some_and(|limit| written >= limit) && written < todo.len() {
            bail!("interrupted after writing {written} file(s) (simulated)");
        }
    }
    let already_applied = todo.is_empty();
    if journal.state != JournalState::Complete {
        journal.state = JournalState::Complete;
        save_journal(&path, &journal)?;
    }
    Ok(ApplyReport {
        applied,
        manual,
        files: outcomes,
        resumed: resumed && !already_applied,
        already_applied,
        journal: path,
    })
}

/// Read `file` again, right before it is replaced, and refuse when it is not
/// exactly what the patch was rendered from (`expected`: its hash, `None` for
/// a file that did not exist). The apply holds the lock against another
/// `init --apply`, not against an editor or any other tool. This is the early,
/// readable check; the replacement itself ([`replace_file`]) is the one that
/// holds against a save landing after it. `already` are the files this apply
/// wrote before this one.
fn ensure_unedited(
    root: &Path,
    file: &str,
    expected: Option<&str>,
    journal: &Path,
    already: &[&str],
) -> Result<()> {
    let current = read_current(root, file)?.map(|text| sha256_hex(&text));
    if current.as_deref() == expected {
        return Ok(());
    }
    let how = match (expected, current.as_deref()) {
        (Some(_), None) => "deleted",
        (None, Some(_)) => "created",
        _ => "changed",
    };
    Err(edited_error(
        &format!(
            "{file} was edited since the plan was made (it was {how} after devflow read it); it \
             was not overwritten"
        ),
        already,
        journal,
    ))
}

/// The error for a file that is not what the plan saw.
fn edited_error(what: &str, already: &[&str], journal: &Path) -> anyhow::Error {
    let mut message = format!("{what}. Make a new plan");
    if !already.is_empty() {
        message.push_str(&format!(
            ". Already in place from this apply: {}; review the files, then delete {} to start \
             over",
            already.join(", "),
            journal.display()
        ));
    }
    stale(message)
}

/// Replace `file` with `after` unless it changed since the patch was rendered
/// from `expected`, even at the last moment: see
/// `fsutil::replace_if_unchanged`. With an atomic exchange (APFS, Linux
/// filesystems with `RENAME_EXCHANGE`) a save that landed in between is never
/// overwritten or lost and the error says where it is; elsewhere a narrow
/// window remains and the hard-link backup is kept.
fn replace_file(
    root: &Path,
    file: &str,
    expected: Option<&str>,
    after: &str,
    journal: &Path,
    already: &[&str],
    probes: &Probes,
) -> Result<()> {
    let staged = probes.after_staging.map(|probe| move || probe(file));
    let seams = ReplaceSeams {
        after_staging: staged.as_ref().map(|probe| probe as &dyn Fn()),
        ..ReplaceSeams::default()
    };
    match replace_if_unchanged_with(&root.join(file), after.as_bytes(), false, expected, &seams) {
        Ok(()) => Ok(()),
        Err(ReplaceError::Failed(error)) => Err(error),
        Err(ReplaceError::Conflict(conflict)) => {
            let what = match conflict.kind {
                ConflictKind::Edited | ConflictKind::Deleted | ConflictKind::Created => {
                    let how = match conflict.kind {
                        ConflictKind::Deleted => "deleted",
                        ConflictKind::Created => "created",
                        _ => "changed",
                    };
                    format!(
                        "{file} was edited since the plan was made (it was {how} while devflow \
                         was replacing it); it was not overwritten"
                    )
                }
                // A version of the file was moved aside: say where.
                ConflictKind::NotRestored | ConflictKind::EditedAgain => format!(
                    "{file} was edited while devflow was replacing it, and no edit was lost: \
                     {conflict}"
                ),
            };
            Err(edited_error(&what, already, journal))
        }
    }
}

/// The accepted ids in plan order, after checking they exist and that their
/// dependencies and conflicts hold.
fn check_selection(plan: &Proposal, accepted: &[ItemId]) -> Result<Vec<ItemId>> {
    let known: BTreeSet<&str> = plan.items.iter().map(|item| item.id.as_str()).collect();
    let mut chosen = BTreeSet::new();
    for id in accepted {
        if !known.contains(id.as_str()) {
            bail!("the plan has no item \"{id}\"");
        }
        chosen.insert(id.as_str());
    }
    let mut problems = Vec::new();
    for item in plan.items.iter().filter(|i| chosen.contains(i.id.as_str())) {
        for dependency in &item.depends_on {
            if !chosen.contains(dependency.as_str()) {
                problems.push(format!(
                    "\"{}\" needs \"{dependency}\", {}",
                    item.id,
                    plan.unmet(dependency)
                ));
            }
        }
        for conflict in &item.conflicts {
            if known.contains(conflict.as_str()) {
                if chosen.contains(conflict.as_str()) {
                    problems.push(format!(
                        "\"{}\" conflicts with \"{conflict}\"; accept only one of them",
                        item.id
                    ));
                }
            } else {
                problems.push(format!(
                    "\"{}\" cannot be applied: {conflict}; resolve it by hand",
                    item.id
                ));
            }
        }
    }
    if !problems.is_empty() {
        bail!(
            "cannot apply the accepted items; nothing was written:\n  - {}",
            problems.join("\n  - ")
        );
    }
    Ok(plan
        .items
        .iter()
        .filter(|item| chosen.contains(item.id.as_str()))
        .map(|item| item.id.clone())
        .collect())
}

/// The files whose current hash differs from the plan's, skipping the
/// detection keys and the files in `except`.
pub(super) fn changed_inputs(
    plan: &Proposal,
    current: &BTreeMap<String, Option<String>>,
    except: &BTreeSet<String>,
) -> Vec<String> {
    plan.input_hashes
        .iter()
        .filter(|(key, _)| {
            key.as_str() != DETECTION_KEY
                && key.as_str() != REPOSITORY_KEY
                && !except.contains(key.as_str())
        })
        .filter(|(key, hash)| current.get(key.as_str()) != Some(*hash))
        .map(|(key, hash)| {
            if key.as_str() == MISE_LAYERS_KEY {
                "the mise configuration files (one was added or removed)".to_string()
            } else if hash.is_none() {
                format!("{key} (created)")
            } else if current.get(key.as_str()).cloned().flatten().is_none() {
                format!("{key} (deleted)")
            } else {
                key.clone()
            }
        })
        .collect()
}

/// A rendered file must still be a valid file of its kind.
fn validate_rendered(existing: &ExistingFiles, file: &str, text: &str) -> Result<()> {
    if DEVFLOW_FILES.contains(&file) {
        Config::parse_layers(text, file, existing.text(LOCAL_FILE), LOCAL_FILE)
            .with_context(|| format!("the result for {file} would not be a valid configuration"))?;
    } else {
        text.parse::<toml::Table>()
            .with_context(|| format!("the result for {file} would not be valid TOML"))?;
    }
    Ok(())
}

fn read_journal(path: &Path) -> Option<Journal> {
    let text = std::fs::read_to_string(path).ok()?;
    let journal: Journal = serde_json::from_str(&text).ok()?;
    (journal.version == JOURNAL_VERSION).then_some(journal)
}

fn save_journal(path: &Path, journal: &Journal) -> Result<()> {
    let json = serde_json::to_vec_pretty(journal)?;
    write_atomic(path, &json, true)
}
