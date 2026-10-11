//! Edits of the `[hooks]` table: entries of `setup`, `switch` and `teardown`
//! and the default `timeout`.
use super::text::{Context, Slots};
use super::{Edit, HookRef, Placement, Target};
use crate::config::HookEntry;
use crate::hooks::HookPhase;
use anyhow::{Result, bail};
use toml_edit::{
    Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, TableLike, Value, value,
};

/// `[hooks]` as an editable table, created when the file has none.
fn hooks_table(doc: &mut DocumentMut) -> Result<&mut dyn TableLike> {
    let root = doc.as_table_mut();
    if !root.contains_key("hooks") {
        root.insert("hooks", Item::Table(Table::new()));
    }
    match root.get_mut("hooks").and_then(Item::as_table_like_mut) {
        Some(table) => Ok(table),
        None => bail!("hooks: not a table"),
    }
}

/// The entries of one phase, in a file.
enum List<'a> {
    /// `setup = ["a", { name = "b", run = "c" }]`
    Inline(&'a mut Array),
    /// `[[hooks.setup]]` tables
    Tables(&'a mut ArrayOfTables),
}

/// Whether `doc` has `hooks.<phase>`.
fn has_phase(doc: &DocumentMut, phase: HookPhase) -> bool {
    doc.get("hooks")
        .and_then(Item::as_table_like)
        .is_some_and(|t| t.contains_key(phase.as_str()))
}

/// The list of `phase`, created empty when `create` is set and the file has
/// none.
fn list_of<'a>(
    doc: &'a mut DocumentMut,
    phase: HookPhase,
    create: bool,
) -> Result<Option<List<'a>>> {
    let key = phase.as_str();
    let table = hooks_table(doc)?;
    if !table.contains_key(key) {
        if !create {
            return Ok(None);
        }
        table.insert(key, Item::Value(Value::Array(Array::new())));
    }
    match table.get_mut(key) {
        Some(Item::Value(Value::Array(array))) => Ok(Some(List::Inline(array))),
        Some(Item::ArrayOfTables(tables)) => Ok(Some(List::Tables(tables))),
        _ => bail!("hooks.{key}: not a list of commands"),
    }
}

// -- entries <-> TOML ---------------------------------------------------------------

fn string_of(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string)
}

fn entry_of_value(value: &Value) -> Option<HookEntry> {
    match value {
        Value::String(text) => Some(HookEntry::new(text.value().as_str())),
        Value::InlineTable(table) => Some(HookEntry {
            name: table.get("name").and_then(string_of),
            run: table.get("run").and_then(string_of)?,
            timeout: table.get("timeout").and_then(string_of),
        }),
        _ => None,
    }
}

fn entry_of_table(table: &Table) -> Option<HookEntry> {
    let text = |key: &str| table.get(key).and_then(Item::as_str).map(str::to_string);
    Some(HookEntry {
        name: text("name"),
        run: text("run")?,
        timeout: text("timeout"),
    })
}

/// A string when only `run` is set, `{ name, run, timeout }` otherwise.
fn value_of(entry: &HookEntry) -> Value {
    if entry.name.is_none() && entry.timeout.is_none() {
        return Value::from(entry.run.as_str());
    }
    let mut table = InlineTable::new();
    if let Some(name) = &entry.name {
        table.insert("name", Value::from(name.as_str()));
    }
    table.insert("run", Value::from(entry.run.as_str()));
    if let Some(timeout) = &entry.timeout {
        table.insert("timeout", Value::from(timeout.as_str()));
    }
    table.fmt();
    Value::InlineTable(table)
}

fn table_of(entry: &HookEntry) -> Table {
    let mut table = Table::new();
    set_table_fields(&mut table, entry);
    table
}

/// Make `table` say `entry`, keeping the comments and order of the keys it has.
fn set_table_fields(table: &mut Table, entry: &HookEntry) {
    let mut set = |key: &str, text: Option<&str>| match text {
        Some(text) => match table.get_mut(key) {
            Some(item) => {
                let decor = item.as_value().map(|v| v.decor().clone());
                let mut new = Value::from(text);
                if let Some(decor) = decor {
                    *new.decor_mut() = decor;
                }
                *item = Item::Value(new);
            }
            None => {
                table.insert(key, value(text));
            }
        },
        None => {
            table.remove(key);
        }
    };
    set("name", entry.name.as_deref());
    set("run", Some(entry.run.as_str()));
    set("timeout", entry.timeout.as_deref());
}

impl List<'_> {
    fn entries(&self) -> Vec<HookEntry> {
        match self {
            List::Inline(array) => array.iter().filter_map(entry_of_value).collect(),
            List::Tables(tables) => tables.iter().filter_map(entry_of_table).collect(),
        }
    }

    fn len(&self) -> usize {
        match self {
            List::Inline(array) => array.len(),
            List::Tables(tables) => tables.len(),
        }
    }

    fn find(&self, hook: &HookRef, phase: HookPhase) -> Result<usize> {
        match hook {
            HookRef::Index(index) => {
                if *index < self.len() {
                    Ok(*index)
                } else {
                    bail!(
                        "hooks.{} has {}; position {} does not exist",
                        phase.as_str(),
                        crate::text::counted_as(self.len() as u64, "entry", "entries"),
                        index + 1
                    )
                }
            }
            HookRef::Name(name) => {
                let found: Vec<usize> = self
                    .entries()
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| e.name.as_deref() == Some(name.as_str()))
                    .map(|(i, _)| i)
                    .collect();
                match found.as_slice() {
                    [one] => Ok(*one),
                    [] => bail!("hooks.{} has no entry with that name", phase.as_str()),
                    _ => bail!(
                        "hooks.{} has several entries with that name",
                        phase.as_str()
                    ),
                }
            }
        }
    }

    /// Where `at` puts a new entry, or, with `moving` (the entry's current
    /// index), where the moved entry ends up.
    fn index_for(&self, at: &Placement, phase: HookPhase, moving: Option<usize>) -> Result<usize> {
        let others = self.len() - usize::from(moving.is_some());
        let relative = |reference: &HookRef, after: bool| -> Result<usize> {
            let anchor = self.find(reference, phase)?;
            match moving {
                Some(from) if from == anchor => {
                    bail!(
                        "hooks.{}: an entry cannot be moved next to itself",
                        phase.as_str()
                    )
                }
                // Positions after the entry that leaves shift down by one.
                Some(from) if from < anchor => Ok(anchor - 1 + usize::from(after)),
                _ => Ok(anchor + usize::from(after)),
            }
        };
        Ok(match at {
            Placement::End => others,
            Placement::Start => 0,
            Placement::Index(index) => (*index).min(others),
            Placement::Before(reference) => relative(reference, false)?,
            Placement::After(reference) => relative(reference, true)?,
        })
    }

    fn insert(&mut self, index: usize, entry: &HookEntry) {
        match self {
            List::Inline(array) => {
                let mut slots = Slots::read(array);
                let value = value_of(entry);
                let table = matches!(value, Value::InlineTable(_));
                let slot = slots.new_slot(value);
                slots.entries.insert(index, slot);
                slots.write(array, table);
            }
            List::Tables(tables) => {
                rebuild_tables(tables, |all| all.insert(index, table_of(entry)))
            }
        }
    }

    fn replace(&mut self, index: usize, entry: &HookEntry) {
        match self {
            List::Inline(array) => {
                let mut slots = Slots::read(array);
                let mut value = value_of(entry);
                let table = matches!(value, Value::InlineTable(_));
                let old = &slots.entries[index].value;
                if let (Value::InlineTable(old), Value::InlineTable(new)) = (old, &mut value) {
                    // Keep the key order and the comments inside the braces.
                    let mut merged = old.clone();
                    let keys_before = merged.len();
                    set_inline_fields(&mut merged, entry);
                    // A key added or removed leaves the spacing around it
                    // uneven (`"x" , timeout`): write the braces afresh.
                    if merged.len() != keys_before {
                        merged.fmt();
                    }
                    *new = merged;
                }
                let suffix = old.decor().suffix().cloned();
                if let Some(suffix) = suffix {
                    value.decor_mut().set_suffix(suffix);
                }
                slots.entries[index].value = value;
                slots.write(array, table);
            }
            List::Tables(tables) => {
                if let Some(table) = tables.get_mut(index) {
                    set_table_fields(table, entry);
                }
            }
        }
    }

    fn remove(&mut self, index: usize) {
        match self {
            List::Inline(array) => {
                let mut slots = Slots::read(array);
                slots.entries.remove(index);
                slots.write(array, false);
            }
            List::Tables(tables) => rebuild_tables(tables, |all| {
                all.remove(index);
            }),
        }
    }

    fn move_to(&mut self, from: usize, to: usize) {
        match self {
            List::Inline(array) => {
                let mut slots = Slots::read(array);
                let slot = slots.entries.remove(from);
                slots.entries.insert(to, slot);
                slots.write(array, false);
            }
            List::Tables(tables) => rebuild_tables(tables, |all| {
                let table = all.remove(from);
                all.insert(to, table);
            }),
        }
    }
}

/// An inline table with the fields of `entry`, keeping what it had.
fn set_inline_fields(table: &mut InlineTable, entry: &HookEntry) {
    let mut set = |key: &str, text: Option<&str>| match text {
        Some(text) => {
            let decor = table.get(key).map(|v| v.decor().clone());
            let mut new = Value::from(text);
            if let Some(decor) = decor {
                *new.decor_mut() = decor;
            }
            table.insert(key, new);
        }
        None => {
            table.remove(key);
        }
    };
    set("name", entry.name.as_deref());
    set("run", Some(entry.run.as_str()));
    set("timeout", entry.timeout.as_deref());
}

/// Change the order or the members of an array of tables. Tables are written
/// in document order of their `position`, so the positions of the old order
/// are handed to the new one.
fn rebuild_tables(tables: &mut ArrayOfTables, change: impl FnOnce(&mut Vec<Table>)) {
    let mut all: Vec<Table> = tables.iter().cloned().collect();
    let mut positions: Vec<Option<isize>> = all.iter().map(Table::position).collect();
    change(&mut all);
    tables.clear();
    positions.resize(all.len(), None);
    for (mut table, position) in all.into_iter().zip(positions) {
        table.set_position(position);
        tables.push(table);
    }
}

// -- edits ---------------------------------------------------------------------------

/// Local file only: a list the local file does not have is first copied from
/// the project file, because a list in the local file replaces the committed
/// one whole.
fn seed_local(doc: &mut DocumentMut, ctx: &mut Context<'_>, phase: HookPhase) -> Result<()> {
    if ctx.target != Target::Local || has_phase(doc, phase) {
        return Ok(());
    }
    let key = phase.as_str();
    // A fresh list with the committed entries: the comments of the committed
    // file stay in the committed file.
    let committed = ctx.project_doc().and_then(|project| {
        let table = project.get("hooks")?.as_table_like()?;
        match table.get(key)? {
            Item::Value(Value::Array(array)) => {
                Some(array.iter().filter_map(entry_of_value).collect::<Vec<_>>())
            }
            Item::ArrayOfTables(tables) => Some(tables.iter().filter_map(entry_of_table).collect()),
            _ => None,
        }
    });
    let Some(entries) = committed else {
        return Ok(());
    };
    let mut array = Array::new();
    let mut slots = Slots::read(&array);
    for entry in &entries {
        let slot = slots.new_slot(value_of(entry));
        slots.entries.push(slot);
    }
    let tables = entries
        .iter()
        .any(|e| e.name.is_some() || e.timeout.is_some());
    slots.write(&mut array, tables);
    hooks_table(doc)?.insert(key, Item::Value(Value::Array(array)));
    let project = ctx.project_name();
    ctx.note(format!(
        "{} now holds the whole hooks.{key} list, copied from {project}: a list in the local \
         file replaces the committed one on this machine",
        ctx.file_name()
    ));
    Ok(())
}

/// Committed file only: say so when the local file overrides the list.
fn note_override(ctx: &mut Context<'_>, phase: HookPhase) {
    if ctx.target != Target::Project {
        return;
    }
    let overridden = ctx
        .local_doc()
        .is_some_and(|local| has_phase(&local, phase));
    if overridden {
        ctx.note(format!(
            "{} sets hooks.{} too, and its list replaces this one on this machine: the change \
             has no effect here until that override is removed",
            super::LOCAL_CONFIG_FILE,
            phase.as_str()
        ));
    }
}

impl Edit {
    pub(super) fn apply_hooks_edit(
        &self,
        doc: &mut DocumentMut,
        ctx: &mut Context<'_>,
    ) -> Result<bool> {
        match self {
            Edit::AddHook { phase, entry, at } => {
                seed_local(doc, ctx, *phase)?;
                note_override(ctx, *phase);
                let mut list = list_of(doc, *phase, true)?.expect("created");
                let index = list.index_for(at, *phase, None)?;
                list.insert(index, entry);
            }
            Edit::UpdateHook { phase, hook, entry } => {
                seed_local(doc, ctx, *phase)?;
                note_override(ctx, *phase);
                let Some(mut list) = list_of(doc, *phase, false)? else {
                    bail!("hooks.{} has no entries", phase.as_str());
                };
                let index = list.find(hook, *phase)?;
                list.replace(index, entry);
            }
            Edit::RemoveHook { phase, hook } => {
                seed_local(doc, ctx, *phase)?;
                note_override(ctx, *phase);
                let Some(mut list) = list_of(doc, *phase, false)? else {
                    bail!("hooks.{} has no entries", phase.as_str());
                };
                let index = list.find(hook, *phase)?;
                list.remove(index);
            }
            Edit::MoveHook { phase, hook, to } => {
                seed_local(doc, ctx, *phase)?;
                note_override(ctx, *phase);
                let Some(mut list) = list_of(doc, *phase, false)? else {
                    bail!("hooks.{} has no entries", phase.as_str());
                };
                let from = list.find(hook, *phase)?;
                let target = list.index_for(to, *phase, Some(from))?;
                if from != target {
                    list.move_to(from, target);
                }
            }
            Edit::SetHooksTimeout(timeout) => {
                let table = hooks_table(doc)?;
                match timeout {
                    Some(text) => match table.get_mut("timeout") {
                        Some(item) => {
                            let decor = item.as_value().map(|v| v.decor().clone());
                            let mut new = Value::from(text.as_str());
                            if let Some(decor) = decor {
                                *new.decor_mut() = decor;
                            }
                            *item = Item::Value(new);
                        }
                        None => {
                            table.insert("timeout", value(text.as_str()));
                        }
                    },
                    None => {
                        table.remove("timeout");
                    }
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}
