//! What applying part of a proposal would do to each file, without writing
//! anything: the text after, and a line diff for a person to read.
use super::existing::ExistingFiles;
use super::patch::{self, Effect, PatchOp};
use super::plan::detection_hash;
use super::{DETECTION_KEY, ItemId, Proposal};
use crate::detect;
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::Path;

/// What would happen to a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChange {
    /// The file does not exist and would be written.
    Created,
    /// The file exists and would get additions.
    Modified,
    /// The accepted items are already in the file.
    Unchanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    /// A line the file has now and keeps.
    Context,
    Added,
    Removed,
    /// Lines left out between two changes.
    Gap,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffLine {
    pub kind: DiffKind,
    /// The line without its newline (empty for a [`DiffKind::Gap`]).
    pub text: String,
}

/// One file of the preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FilePreview {
    pub file: String,
    pub change: FileChange,
    /// The text the file would have (the file as it is when unchanged).
    pub after: String,
    /// Changed lines with two lines of context around them.
    pub diff: Vec<DiffLine>,
    pub added: usize,
    pub removed: usize,
    /// Paths (`hooks.setup`, `services.db.image`) the file already sets to
    /// something else; applying is refused while any is listed.
    pub conflicts: Vec<String>,
}

/// Preview applying `accepted` items of `plan` to the project at `root`.
///
/// Reads the files as they are now and writes nothing. Items that only ask
/// for work by hand have no patch and do not appear.
pub fn preview(root: &Path, plan: &Proposal, accepted: &[ItemId]) -> Result<Vec<FilePreview>> {
    let existing = ExistingFiles::read(root)?;
    let mut targets: Vec<(String, Vec<PatchOp>)> = Vec::new();
    let resolved = plan.resolved(accepted);
    for item in resolved.items.iter().filter(|i| accepted.contains(&i.id)) {
        if !item.is_actionable() {
            continue;
        }
        match targets
            .iter_mut()
            .find(|(file, _)| *file == item.patch.file)
        {
            Some((_, ops)) => ops.extend(item.patch.ops.clone()),
            None => targets.push((item.patch.file.clone(), item.patch.ops.clone())),
        }
    }
    let mut previews = Vec::new();
    for (file, ops) in targets {
        let before = existing.text(&file);
        let (after, effects) =
            patch::render(before, &ops).with_context(|| format!("cannot edit {file}"))?;
        let conflicts = effects
            .iter()
            .zip(&ops)
            .filter(|(effect, _)| matches!(effect, Effect::Conflict(_)))
            .map(|(_, op)| op.target())
            .collect();
        let change = match before {
            None => FileChange::Created,
            Some(text) if text == after => FileChange::Unchanged,
            Some(_) => FileChange::Modified,
        };
        let diff = diff_lines(before.unwrap_or(""), &after, 2);
        let count = |kind| diff.iter().filter(|l| l.kind == kind).count();
        previews.push(FilePreview {
            added: count(DiffKind::Added),
            removed: count(DiffKind::Removed),
            file,
            change,
            after,
            diff,
            conflicts,
        });
    }
    Ok(previews)
}

/// What changed since `plan` was made: the configuration files it read or
/// could write, and the repository files detection reads. Empty when the plan
/// still describes the checkout, so applying it will not be refused for stale
/// inputs. Reads only.
///
/// This is the check [`super::apply`] makes before its first write, asked
/// ahead of time so a frontend can offer to plan again instead of showing a
/// failed apply.
pub fn inputs_changed(root: &Path, plan: &Proposal) -> Result<Vec<String>> {
    let existing = ExistingFiles::read(root)?;
    let mut changed = super::apply::changed_inputs(plan, &existing.hashes(), &BTreeSet::new());
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
    Ok(changed)
}

/// Above this many line pairs the diff is not computed; the whole file is
/// shown as replaced (configuration files are far smaller).
const MAX_PAIRS: usize = 4_000_000;

/// A line diff of `before` against `after`, showing `context` unchanged lines
/// around each change and a gap where more were left out.
pub fn diff_lines(before: &str, after: &str, context: usize) -> Vec<DiffLine> {
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();
    let all = if old.len().saturating_mul(new.len()) > MAX_PAIRS {
        old.iter()
            .map(|l| (DiffKind::Removed, *l))
            .chain(new.iter().map(|l| (DiffKind::Added, *l)))
            .collect()
    } else {
        lcs_diff(&old, &new)
    };
    // Keep the changes and `context` lines around them.
    let mut keep = vec![false; all.len()];
    for (index, (kind, _)) in all.iter().enumerate() {
        if *kind != DiffKind::Context {
            let from = index.saturating_sub(context);
            let to = (index + context + 1).min(all.len());
            keep[from..to].iter_mut().for_each(|k| *k = true);
        }
    }
    let mut out = Vec::new();
    let mut gap = false;
    for (index, (kind, text)) in all.into_iter().enumerate() {
        if keep[index] {
            if gap && !out.is_empty() {
                out.push(DiffLine {
                    kind: DiffKind::Gap,
                    text: String::new(),
                });
            }
            gap = false;
            out.push(DiffLine {
                kind,
                text: text.to_string(),
            });
        } else {
            gap = true;
        }
    }
    out
}

/// Longest-common-subsequence diff of two line lists, in order.
fn lcs_diff<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<(DiffKind, &'a str)> {
    let (n, m) = (old.len(), new.len());
    // lengths[i][j]: LCS of old[i..] and new[j..].
    let mut lengths = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lengths[i][j] = if old[i] == new[j] {
                lengths[i + 1][j + 1] + 1
            } else {
                lengths[i + 1][j].max(lengths[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::with_capacity(n.max(m));
    while i < n && j < m {
        if old[i] == new[j] {
            out.push((DiffKind::Context, old[i]));
            i += 1;
            j += 1;
        } else if lengths[i + 1][j] >= lengths[i][j + 1] {
            out.push((DiffKind::Removed, old[i]));
            i += 1;
        } else {
            out.push((DiffKind::Added, new[j]));
            j += 1;
        }
    }
    out.extend(old[i..].iter().map(|l| (DiffKind::Removed, *l)));
    out.extend(new[j..].iter().map(|l| (DiffKind::Added, *l)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(diff: &[DiffLine]) -> String {
        diff.iter()
            .map(|l| match l.kind {
                DiffKind::Context => format!(" {}", l.text),
                DiffKind::Added => format!("+{}", l.text),
                DiffKind::Removed => format!("-{}", l.text),
                DiffKind::Gap => "…".to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_new_file_is_all_additions() {
        let diff = diff_lines("", "a\nb\n", 2);
        assert_eq!(show(&diff), "+a\n+b");
    }

    #[test]
    fn changes_keep_two_lines_of_context_and_a_gap_between_distant_ones() {
        let before = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
        let after = "1\nONE\n2\n3\n4\n5\n6\n7\n8\n9\n10\nELEVEN\n";
        let diff = diff_lines(before, after, 2);
        assert_eq!(show(&diff), " 1\n+ONE\n 2\n 3\n…\n 9\n 10\n+ELEVEN");
        assert_eq!(diff_lines("a\n", "a\n", 2), vec![]);
        assert_eq!(show(&diff_lines("a\nb\n", "a\nc\n", 2)), " a\n-b\n+c");
    }
}
