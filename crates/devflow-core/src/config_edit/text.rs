//! Shared plumbing of the edits: the editing context, the final text, and a
//! model of a TOML array that keeps each entry's comments with the entry.
use super::{ConfigFiles, Target};
use toml_edit::{Array, DocumentMut, Value};

/// What an edit knows about the files it works on.
pub(super) struct Context<'a> {
    pub target: Target,
    files: &'a ConfigFiles,
    project_doc: Option<Option<DocumentMut>>,
    pub notes: Vec<String>,
}

impl<'a> Context<'a> {
    pub fn new(target: Target, files: &'a ConfigFiles) -> Self {
        Self {
            target,
            files,
            project_doc: None,
            notes: Vec::new(),
        }
    }

    pub fn note(&mut self, text: impl Into<String>) {
        self.notes.push(text.into());
    }

    /// The name of the file being edited.
    pub fn file_name(&self) -> String {
        super::file_name(&self.files.file(self.target).path)
    }

    pub fn project_name(&self) -> String {
        super::file_name(&self.files.project.path)
    }

    /// The project file, parsed for editing (formatting kept); `None` when
    /// it cannot be parsed.
    pub fn project_doc(&mut self) -> Option<&DocumentMut> {
        let files = self.files;
        self.project_doc
            .get_or_insert_with(|| files.project.text.as_deref()?.parse().ok())
            .as_ref()
    }

    /// The local file, parsed (empty when it does not exist).
    pub fn local_doc(&self) -> Option<DocumentMut> {
        self.files.local.text.as_deref().unwrap_or("").parse().ok()
    }
}

/// The text of an edited document: no leading blank line in a new file, one
/// trailing newline.
pub(super) fn finish(doc: &DocumentMut, original: Option<&str>) -> String {
    let rendered = doc.to_string();
    if original.is_none_or(|text| text.trim().is_empty()) {
        let trimmed = rendered.trim_start_matches('\n').to_string();
        if trimmed.is_empty() || trimmed.ends_with('\n') {
            trimmed
        } else {
            format!("{trimmed}\n")
        }
    } else if rendered.ends_with('\n') {
        rendered
    } else {
        format!("{rendered}\n")
    }
}

// -- arrays -----------------------------------------------------------------------

/// One element of an array together with the comments that belong to it.
///
/// In a parsed array the text between a comma and the next value is that
/// value's prefix, so a comment written after an entry's comma
/// (`"a", # note`) is stored in the *next* entry's prefix. Edits that remove
/// or reorder entries would then take the note along with the wrong entry.
/// Splitting each prefix into the part on the previous entry's line (its
/// `trail`) and the rest (`lead`: comment lines above the entry and the
/// indentation) gives every entry what is its own.
pub(super) struct Slot {
    pub value: Value,
    pub lead: String,
    pub trail: String,
}

/// An array taken apart: see [`Slot`].
pub(super) struct Slots {
    pub entries: Vec<Slot>,
    /// What follows the last entry's trail before `]` (usually a newline).
    rest: String,
    /// Written over several lines.
    pub multiline: bool,
    trailing_comma: bool,
    indent: String,
}

impl Slots {
    pub fn read(array: &Array) -> Slots {
        let prefixes: Vec<String> = array
            .iter()
            .map(|value| raw(value.decor().prefix()))
            .collect();
        let trailing = raw(Some(array.trailing()));
        let multiline = prefixes.iter().any(|p| p.contains('\n')) || trailing.contains('\n');
        let split: Vec<(String, String)> = prefixes
            .iter()
            .enumerate()
            .map(|(index, prefix)| {
                if index == 0 {
                    (String::new(), prefix.clone())
                } else {
                    split_prefix(prefix)
                }
            })
            .collect();
        let (last_trail, rest) = if array.is_empty() {
            (String::new(), trailing)
        } else {
            split_prefix(&trailing)
        };
        let indent = split
            .first()
            .and_then(|(_, lead)| lead.rsplit_once('\n'))
            .map(|(_, indent)| indent)
            .filter(|indent| indent.chars().all(|c| c == ' ' || c == '\t'))
            .map_or_else(|| "  ".to_string(), str::to_string);
        let mut entries = Vec::with_capacity(array.len());
        for (index, value) in array.iter().enumerate() {
            let trail = match split.get(index + 1) {
                Some((trail, _)) => trail.clone(),
                None => last_trail.clone(),
            };
            let mut value = value.clone();
            value.decor_mut().set_prefix("");
            entries.push(Slot {
                value,
                lead: split[index].1.clone(),
                trail,
            });
        }
        Slots {
            entries,
            rest,
            multiline,
            trailing_comma: array.trailing_comma(),
            indent,
        }
    }

    /// A slot for a new entry, written the way the others are.
    pub fn new_slot(&self, value: Value) -> Slot {
        Slot {
            value,
            lead: if self.multiline {
                format!("\n{}", self.indent)
            } else {
                String::new()
            },
            trail: String::new(),
        }
    }

    /// Write the entries back. `force_multiline` switches a one-line array
    /// to one entry per line (used when a table is added to it).
    pub fn write(mut self, array: &mut Array, force_multiline: bool) {
        if force_multiline && !self.multiline {
            self.multiline = true;
            self.trailing_comma = true;
            self.rest = "\n".to_string();
            for slot in &mut self.entries {
                slot.lead = format!("\n{}", self.indent);
                slot.trail.clear();
            }
        }
        let count = self.entries.len();
        let mut trail_before = String::new();
        let mut values = Vec::with_capacity(count);
        for (index, mut slot) in self.entries.into_iter().enumerate() {
            let prefix = if self.multiline {
                let mut prefix = trail_before;
                prefix.push_str(&slot.lead);
                prefix
            } else if index == 0 {
                String::new()
            } else {
                " ".to_string()
            };
            slot.value.decor_mut().set_prefix(prefix);
            trail_before = slot.trail;
            values.push(slot.value);
        }
        let trailing = if self.multiline {
            let mut trailing = trail_before;
            trailing.push_str(&self.rest);
            if count == 0 && trailing.trim().is_empty() {
                trailing.clear();
            }
            trailing
        } else {
            String::new()
        };
        array.clear();
        for value in values {
            array.push_formatted(value);
        }
        array.set_trailing(trailing);
        array.set_trailing_comma(self.multiline && count > 0 && self.trailing_comma);
    }
}

fn raw(raw: Option<&toml_edit::RawString>) -> String {
    raw.and_then(|r| r.as_str()).unwrap_or("").to_string()
}

/// Split the text before an entry into the comment on the previous entry's
/// line and the rest (which starts with that line's newline).
fn split_prefix(prefix: &str) -> (String, String) {
    match prefix.find('\n') {
        Some(newline) if prefix[..newline].contains('#') => {
            (prefix[..newline].to_string(), prefix[newline..].to_string())
        }
        _ => (String::new(), prefix.to_string()),
    }
}
