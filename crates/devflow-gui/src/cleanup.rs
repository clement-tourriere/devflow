//! What the Clean up page shows and what the person picked on it.
//!
//! Core classifies every workspace record and registered worktree
//! (`devflow_core::gc::inventory`) and removes the instances it is asked to
//! (`gc::remove`); this holds the last inventory read for the open project, the
//! instances picked in it and the "snapshot first" choice. Nothing here decides
//! what may be removed: an entry is selectable only when core said it is
//! `removable` and gave a target, and the target a removal is asked for carries
//! the generation of the record the person was shown, so core refuses an entry
//! that changed since.
use devflow_core::gc::{Entry, Inventory, Target};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Default)]
pub struct Cleanup {
    /// The project the inventory and the selection are about.
    pub project: Option<Uuid>,
    /// The last inventory read, or why it could not be.
    pub inventory: Option<Result<Box<Inventory>, String>>,
    /// An inventory is being read (the previous one, when there is one, stays
    /// on the page meanwhile).
    pub loading: bool,
    /// The instances picked, as core described them when they were picked: the
    /// generation of the record and what was seen of each branch's data.
    pub selected: BTreeMap<Uuid, Target>,
    /// Take a snapshot of every data branch first.
    pub snapshot: bool,
    /// Moves on every read; only the newest read's result is applied.
    request: u64,
}

impl Cleanup {
    /// A read for `project` starts. What was read for another project is
    /// forgotten, including the selection. Returns the read's number.
    pub fn begin(&mut self, project: Uuid) -> u64 {
        if self.project != Some(project) {
            *self = Self {
                project: Some(project),
                snapshot: self.snapshot,
                ..Self::default()
            };
        }
        self.request += 1;
        self.loading = true;
        self.request
    }

    /// The result of read number `request` for `project`. A result that is not
    /// the newest read's, or is about another project, changes nothing and
    /// returns `false`. The selection keeps only the entries that are still
    /// removable and whose record and data are the ones that were picked.
    pub fn finish(
        &mut self,
        request: u64,
        project: Uuid,
        result: Result<Inventory, String>,
    ) -> bool {
        if request != self.request || self.project != Some(project) {
            return false;
        }
        self.loading = false;
        match &result {
            Ok(inventory) => self.selected.retain(|instance, picked| {
                inventory
                    .entry(*instance)
                    .is_some_and(|entry| entry.removable && entry.target().as_ref() == Some(picked))
            }),
            Err(_) => self.selected.clear(),
        }
        self.inventory = Some(result.map(Box::new));
        true
    }

    /// The inventory, when one was read.
    pub fn read(&self) -> Option<&Inventory> {
        self.inventory.as_ref()?.as_ref().ok().map(|b| &**b)
    }

    /// Pick or unpick `instance`. Only an entry core says is removable, with a
    /// target, can be picked.
    pub fn select(&mut self, instance: Uuid, on: bool) {
        if !on {
            self.selected.remove(&instance);
            return;
        }
        let target = self
            .read()
            .and_then(|inventory| inventory.entry(instance))
            .filter(|entry| entry.removable)
            .and_then(Entry::target);
        if let Some(target) = target {
            self.selected.insert(target.instance, target);
        }
    }

    /// Pick every removable entry (or none).
    pub fn select_all(&mut self, on: bool) {
        self.selected.clear();
        if !on {
            return;
        }
        let all: Vec<Target> = self
            .read()
            .map(|inventory| {
                inventory
                    .removable()
                    .filter_map(Entry::target)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for target in all {
            self.selected.insert(target.instance, target);
        }
    }

    pub fn is_selected(&self, instance: Uuid) -> bool {
        self.selected.contains_key(&instance)
    }

    /// The entries picked, in the order the inventory lists them.
    pub fn picked(&self) -> Vec<&Entry> {
        self.read()
            .map(|inventory| {
                inventory
                    .entries
                    .iter()
                    .filter(|entry| {
                        entry
                            .instance
                            .is_some_and(|i| self.selected.contains_key(&i))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Exactly what a removal is asked for: the picked instances, each as it
    /// was shown (record generation and branch data).
    pub fn targets(&self) -> Vec<Target> {
        self.selected.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devflow_core::gc::Class;

    fn entry(n: u128, class: Class, removable: bool, generation: &str) -> Entry {
        Entry {
            instance: Some(Uuid::from_u128(n)),
            workspace: format!("w{n}"),
            class,
            cause: None,
            reason: None,
            path: format!("/tmp/w{n}").into(),
            recorded_path: None,
            branch: None,
            has_record: true,
            removing: false,
            created_at: None,
            last_active_at: None,
            branches: Vec::new(),
            namespace: None,
            daemons: Vec::new(),
            app_port: None,
            daemons_unverified: false,
            merge: None,
            generation: Some(generation.to_string()),
            removable,
            not_removable: (!removable).then(|| "no".to_string()),
            remedies: Vec::new(),
        }
    }

    fn inventory(entries: Vec<Entry>) -> Inventory {
        Inventory {
            project: "shop".into(),
            base: None,
            entries,
            notes: Vec::new(),
        }
    }

    #[test]
    fn only_what_core_calls_removable_can_be_picked() {
        let project = Uuid::new_v4();
        let mut cleanup = Cleanup::default();
        let read = cleanup.begin(project);
        assert!(cleanup.finish(
            read,
            project,
            Ok(inventory(vec![
                entry(1, Class::RemovedElsewhere, true, "g1"),
                entry(2, Class::Unknown, false, "g2"),
                entry(3, Class::Active, false, "g3"),
            ])),
        ));
        for n in 1..=3 {
            cleanup.select(Uuid::from_u128(n), true);
        }
        assert_eq!(
            cleanup.targets(),
            [Target {
                instance: Uuid::from_u128(1),
                generation: "g1".into(),
                backend: Vec::new(),
            }]
        );
        cleanup.select_all(true);
        assert_eq!(cleanup.targets().len(), 1);
        cleanup.select_all(false);
        assert!(cleanup.targets().is_empty());
    }

    #[test]
    fn a_read_keeps_the_picks_whose_record_is_unchanged_and_drops_the_rest() {
        let project = Uuid::new_v4();
        let mut cleanup = Cleanup::default();
        let read = cleanup.begin(project);
        cleanup.finish(
            read,
            project,
            Ok(inventory(vec![
                entry(1, Class::RemovedElsewhere, true, "g1"),
                entry(2, Class::Missing, true, "g2"),
                entry(3, Class::Missing, true, "g3"),
            ])),
        );
        cleanup.select_all(true);
        let read = cleanup.begin(project);
        assert!(cleanup.loading);
        cleanup.finish(
            read,
            project,
            Ok(inventory(vec![
                // unchanged
                entry(1, Class::RemovedElsewhere, true, "g1"),
                // its record changed since it was picked
                entry(2, Class::Missing, true, "g2-changed"),
                // not removable any more
                entry(3, Class::Unknown, false, "g3"),
            ])),
        );
        assert_eq!(
            cleanup.targets(),
            [Target {
                instance: Uuid::from_u128(1),
                generation: "g1".into(),
                backend: Vec::new(),
            }]
        );
    }

    #[test]
    fn a_stale_or_foreign_result_changes_nothing_and_another_project_starts_clean() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let mut cleanup = Cleanup::default();
        let first = cleanup.begin(a);
        let second = cleanup.begin(a);
        assert!(!cleanup.finish(first, a, Err("old".into())), "superseded");
        assert!(cleanup.loading && cleanup.inventory.is_none());
        assert!(!cleanup.finish(second, b, Err("wrong project".into())));
        assert!(cleanup.finish(
            second,
            a,
            Ok(inventory(vec![entry(1, Class::Missing, true, "g")]))
        ));
        cleanup.select(Uuid::from_u128(1), true);
        cleanup.snapshot = true;

        // Another project: the inventory and the picks are forgotten, the
        // person's choice about snapshots is not.
        let read = cleanup.begin(b);
        assert!(cleanup.inventory.is_none() && cleanup.selected.is_empty());
        assert!(cleanup.snapshot);
        assert!(!cleanup.finish(read - 1, b, Err("late".into())));
        assert!(cleanup.finish(read, b, Err("no server".into())));
        assert!(cleanup.read().is_none());
        assert!(matches!(&cleanup.inventory, Some(Err(m)) if m == "no server"));
    }
}
