//! The Clean up page: the workspaces Git or an agent host removed, and what
//! they still own.
//!
//! A worktree removed by `git worktree remove`, or by a tool that deletes
//! worktrees itself, takes its receipt with it and leaves the data branches,
//! containers and daemons behind. Core keeps a record of what each workspace
//! owns (`devflow_core::ledger`) and classifies them (`devflow_core::gc`); this
//! page shows that inventory and nothing else decides here:
//!
//! - an entry has a checkbox only when core says it is removable, and removing
//!   asks core for exactly the picked instances with the generation of the
//!   record that was shown (core refuses one that changed since);
//! - an entry core cannot identify is shown with why and with core's remedies
//!   as buttons, never removable;
//! - removing is confirmed first, in words: what is dropped, which daemons are
//!   only checked (devflow never stops a daemon of a workspace with no checkout:
//!   one that runs keeps its workspace), and that dropped data cannot be
//!   recovered (or that a snapshot is taken first).
//!
//! The text of an entry and of the confirmation is built by plain functions
//! ([`facts`], [`confirmation_lines`]) so that it can be tested as text. No
//! command line appears anywhere on the page.
use super::{
    dialogs::open_confirm,
    remedies, ui,
    ui::Tone,
    widgets::{banner, caption, card, chip, section_header},
};
use crate::{model::format_bytes, redaction::shown, state::AppState};
use devflow_core::{
    gc::{Branch, Class, Entry, Inventory, Presence},
    remedy::Remedy,
    text::ago,
    wide::Merged,
    worktree::Merge,
};
use gpui_kit::{
    assets::IconName as Lucide,
    component::{
        ActiveTheme as _, Disableable as _, Sizable as _,
        button::{Button, ButtonVariants as _},
        checkbox::Checkbox,
        h_flex, v_flex,
    },
    prelude::*,
    *,
};

pub struct CleanupView {
    state: Entity<AppState>,
    _subscription: Subscription,
}

impl CleanupView {
    pub fn new(state: Entity<AppState>, _: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&state, |_, _, cx| cx.notify());
        Self {
            state,
            _subscription: subscription,
        }
    }
}

// ---- text ---------------------------------------------------------------

/// What a row says about an entry, as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Facts {
    pub name: String,
    pub branch: Option<String>,
    pub path: String,
    /// `3d ago`, when the workspace was last entered (else created).
    pub activity: Option<String>,
    /// A removal began and did not finish.
    pub unfinished: bool,
    /// What the workspace owns, one line each.
    pub owns: Vec<String>,
    /// The merge evidence and how strongly it is stated.
    pub merge: Option<(Tone, String)>,
    /// Why it is what it is (an unknown entry's reason, a moved one's note).
    pub reason: Option<String>,
}

impl Facts {
    /// Every line, for a test that looks for what must not be there.
    #[cfg(test)]
    pub(super) fn lines(&self) -> Vec<String> {
        let mut all = vec![self.name.clone(), self.path.clone()];
        all.extend(self.branch.clone());
        all.extend(self.activity.clone());
        all.extend(self.owns.clone());
        all.extend(self.merge.as_ref().map(|(_, text)| text.clone()));
        all.extend(self.reason.clone());
        all
    }
}

pub(super) fn facts(entry: &Entry) -> Facts {
    let merge = entry.merge.as_ref().map(|evidence| {
        let tone = match &evidence.merge {
            Merge::Merged {
                how: Merged::LooksSquashed,
            } => Tone::Info,
            Merge::Merged { .. } => Tone::Success,
            Merge::NotMerged { .. } => Tone::Warning,
            Merge::Unknown { .. } => Tone::Neutral,
        };
        (tone, evidence.summary())
    });
    let mut owns: Vec<String> = entry.branches.iter().map(branch_line).collect();
    owns.extend(entry.daemons.iter().map(|id| format!("daemon {id}")));
    if entry.daemons_unverified
        && entry.daemons.is_empty()
        && let Some(namespace) = &entry.namespace
    {
        owns.push(format!(
            "processes in the pitchfork namespace {namespace} (they could not be ruled out)"
        ));
    }
    Facts {
        name: shown(&entry.workspace),
        branch: entry.branch.as_deref().map(shown),
        path: shown(&crate::model::shorten_home(&entry.path)),
        activity: entry
            .last_active_at
            .or(entry.created_at)
            .map(|at| format!("last active {}", ago(at))),
        unfinished: entry.removing,
        owns: owns.iter().map(|line| shown(line)).collect(),
        merge,
        reason: entry.reason.as_deref().map(shown),
    }
}

/// One branch the workspace owns, with what is measurable about it.
pub(super) fn branch_line(branch: &Branch) -> String {
    let mut line = branch.describe();
    if branch.pending {
        line.push_str(" (being created when setup stopped)");
    }
    let mut facts: Vec<String> = Vec::new();
    match &branch.presence {
        Presence::Foreign => facts.push("another instance's, left alone".to_string()),
        Presence::Absent => facts.push("not there any more".to_string()),
        Presence::Unreachable { reason } => facts.push(format!("cannot be asked: {reason}")),
        Presence::Owned | Presence::NotChecked => {}
    }
    if let Some(size) = branch.size_bytes {
        facts.push(format_bytes(size));
    }
    if let Some(state) = &branch.container {
        facts.push(state.clone());
    }
    if let Some(memory) = branch.memory_bytes {
        facts.push(format!("{} memory", format_bytes(memory as i64)));
    }
    if !facts.is_empty() {
        line.push_str(&format!(": {}", facts.join(", ")));
    }
    line
}

/// The question before removing: per workspace what is dropped and checked,
/// then what happens to the data.
pub(super) fn confirmation_lines(entries: &[&Entry], snapshot: bool) -> Vec<String> {
    let mut lines = vec![match entries {
        [one] => format!("Remove the workspace {}?", shown(&one.workspace)),
        many => format!("Remove these {} workspaces?", many.len()),
    }];
    for entry in entries {
        let mut actions: Vec<String> = Vec::new();
        for branch in &entry.branches {
            let size = branch
                .size_bytes
                .map(|bytes| format!(" ({})", format_bytes(bytes)))
                .unwrap_or_default();
            actions.push(match branch.presence {
                Presence::Absent => format!("{} is not there any more", branch.describe()),
                Presence::Foreign => {
                    format!("leaves {} alone (another instance's)", branch.describe())
                }
                _ => format!("drops {}{size}", branch.describe()),
            });
        }
        // Daemons are asked about, never stopped: devflow lets the record go
        // only once pitchfork shows none of them running.
        actions.extend(
            entry
                .daemons
                .iter()
                .map(|id| format!("checks that the daemon {id} is not running")),
        );
        if entry.daemons_unverified && entry.daemons.is_empty() {
            actions.push("checks that nothing runs in its pitchfork namespace".to_string());
        }
        if entry.class == Class::Missing {
            actions.push("forgets its Git registration".to_string());
        }
        let what = if actions.is_empty() {
            "owns nothing; only its record goes".to_string()
        } else {
            actions.join(", ")
        };
        lines.push(shown(&format!("- {}: {what}", entry.workspace)));
    }
    if entries
        .iter()
        .any(|entry| !entry.daemons.is_empty() || entry.daemons_unverified)
    {
        lines.push(
            "A daemon that is running is not stopped here: that workspace is kept until it is \
             stopped."
                .to_string(),
        );
    }
    lines.push("Git branches are kept.".to_string());
    lines.push(if snapshot {
        "A snapshot of each data branch is taken first and kept.".to_string()
    } else {
        "Dropped data cannot be recovered.".to_string()
    });
    lines
}

/// Ask before removing what is picked; confirming starts the removal.
pub(super) fn open_removal(state: &Entity<AppState>, window: &mut Window, cx: &mut App) {
    let (lines, any) = {
        let app = state.read(cx);
        let picked = app.cleanup.picked();
        (
            confirmation_lines(&picked, app.cleanup.snapshot),
            !picked.is_empty(),
        )
    };
    if !any {
        return;
    }
    let app = state.clone();
    open_confirm(
        window,
        cx,
        "Clean up workspaces",
        lines,
        "Remove",
        move |window, cx| {
            app.update(cx, |s, cx| s.remove_cleanup(window, cx));
        },
    );
}

// ---- the page -----------------------------------------------------------

/// How the page groups entries, in the order it shows them.
const GROUPS: [(Class, &str, &str, Lucide); 5] = [
    (
        Class::RemovedElsewhere,
        "Removed elsewhere",
        "Git or an agent host removed the worktree; its data and processes are still here",
        Lucide::Trash,
    ),
    (
        Class::Missing,
        "Missing directory",
        "still registered with Git, but the directory is gone",
        Lucide::FolderGit2,
    ),
    (
        Class::Unknown,
        "Cannot be identified",
        "never removed until devflow can tell what they are",
        Lucide::ShieldAlert,
    ),
    (
        Class::Moved,
        "Moved",
        "the checkout is there, at another place",
        Lucide::GitBranch,
    ),
    (
        Class::Active,
        "Active",
        "nothing to clean up here",
        Lucide::Layers,
    ),
];

impl Render for CleanupView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.clone();
        let app = self.state.read(cx);
        let selected_project = app.selection.current().map(|m| m.key.project);
        let cleanup = &app.cleanup;
        let current = cleanup.project.is_some() && cleanup.project == selected_project;
        let busy = app.busy().is_some();
        let picked = if current { cleanup.selected.len() } else { 0 };
        let snapshot = cleanup.snapshot;
        let loading = !current || cleanup.loading;
        let removable = current
            && cleanup
                .read()
                .is_some_and(|inventory| inventory.removable().next().is_some());

        let header = v_flex()
            .gap_2()
            .px_4()
            .pt_3()
            .pb_2()
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .justify_between()
                    .child(ui::heading("Clean up"))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child({
                                let state = state.clone();
                                Checkbox::new("cleanup-snapshot")
                                    .label("Snapshot each data branch first")
                                    .checked(snapshot)
                                    .on_click(move |value: &bool, _, cx| {
                                        let value = *value;
                                        state.update(cx, |s, cx| s.set_cleanup_snapshot(value, cx));
                                    })
                            })
                            .child({
                                let state = state.clone();
                                Button::new("cleanup-select-all")
                                    .ghost()
                                    .small()
                                    .label(if picked > 0 {
                                        "Select none"
                                    } else {
                                        "Select all"
                                    })
                                    .disabled(!removable || busy)
                                    .on_click(move |_, _, cx| {
                                        state.update(cx, |s, cx| {
                                            let all = s.cleanup.selected.is_empty();
                                            s.pick_all_cleanup(all, cx)
                                        });
                                    })
                            })
                            .child({
                                let state = state.clone();
                                Button::new("cleanup-refresh")
                                    .outline()
                                    .small()
                                    .label("Refresh")
                                    .disabled(loading)
                                    .on_click(move |_, window, cx| {
                                        state.update(cx, |s, cx| s.load_cleanup(window, cx));
                                    })
                            })
                            .child({
                                let state = state.clone();
                                Button::new("cleanup-remove")
                                    .danger()
                                    .small()
                                    .label(format!("Remove selected ({picked})…"))
                                    .disabled(picked == 0 || busy)
                                    .on_click(move |_, window, cx| open_removal(&state, window, cx))
                            }),
                    ),
            )
            .child(div().text_sm().text_color(ui::muted(cx)).child(
                "A worktree removed by Git or by an agent host leaves its data \
                         branches, containers and daemons behind. Nothing is removed until \
                         you pick it here and confirm.",
            ));

        let body: AnyElement = match (&cleanup.inventory, current) {
            (Some(Ok(inventory)), true) => page(inventory, &state, cleanup, busy, cx),
            (Some(Err(message)), true) => v_flex()
                .gap_3()
                .child(ui::error_banner(shown(message), cx))
                .child({
                    let state = state.clone();
                    Button::new("cleanup-retry")
                        .outline()
                        .small()
                        .label("Retry")
                        .on_click(move |_, window, cx| {
                            state.update(cx, |s, cx| s.load_cleanup(window, cx));
                        })
                })
                .into_any_element(),
            _ => ui::loading("Looking at the workspaces…", cx).into_any_element(),
        };

        v_flex().size_full().child(header).child(
            div()
                .id("cleanup-scroll")
                .test_support()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .px_4()
                .pb_4()
                .child(body),
        )
    }
}

/// The inventory: a notice per note core made, then a card per group.
fn page(
    inventory: &Inventory,
    state: &Entity<AppState>,
    cleanup: &crate::cleanup::Cleanup,
    busy: bool,
    cx: &App,
) -> AnyElement {
    let nothing_to_do = inventory
        .entries
        .iter()
        .all(|e| e.class == Class::Active || e.class == Class::Moved);
    v_flex()
        .gap_3()
        .children(
            inventory
                .notes
                .iter()
                .map(|note| banner(Tone::Info, Lucide::CircleAlert, shown(note), None, cx)),
        )
        .when(nothing_to_do, |col| {
            col.child(
                div()
                    .id("cleanup-nothing")
                    .test_support()
                    .text_sm()
                    .text_color(ui::muted(cx))
                    .child(
                        "No workspace was removed outside devflow, and none is missing or \
                         unknown.",
                    ),
            )
        })
        .children(GROUPS.iter().filter_map(|(class, title, hint, icon)| {
            let entries: Vec<(usize, &Entry)> = inventory
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| e.class == *class)
                .collect();
            if entries.is_empty() {
                return None;
            }
            // An active workspace owns nothing to clean up: listed compactly.
            let compact = *class == Class::Active;
            Some(
                card(cx)
                    .id(SharedString::from(format!(
                        "cleanup-group-{}",
                        class.label()
                    )))
                    .test_support()
                    .child(section_header(
                        *icon,
                        *title,
                        Some((*hint).into()),
                        Some(caption(format!("{}", entries.len()), cx).into_any_element()),
                        cx,
                    ))
                    .children(
                        entries.into_iter().map(|(index, entry)| {
                            row(entry, index, compact, state, cleanup, busy, cx)
                        }),
                    )
                    .into_any_element(),
            )
        }))
        .into_any_element()
}

/// One entry.
fn row(
    entry: &Entry,
    index: usize,
    compact: bool,
    state: &Entity<AppState>,
    cleanup: &crate::cleanup::Cleanup,
    busy: bool,
    cx: &App,
) -> AnyElement {
    let key = entry.short_id().unwrap_or_else(|| format!("n{index}"));
    let facts = facts(entry);
    let picked = entry.instance.is_some_and(|i| cleanup.is_selected(i));
    let selectable = entry.removable && entry.target().is_some();

    let marker: AnyElement = match (selectable, entry.instance) {
        (true, Some(instance)) => {
            let state = state.clone();
            Checkbox::new(SharedString::from(format!("cleanup-select-{key}")))
                .checked(picked)
                .disabled(busy)
                .on_click(move |value: &bool, _, cx| {
                    let value = *value;
                    state.update(cx, |s, cx| s.pick_cleanup(instance, value, cx));
                })
                .into_any_element()
        }
        _ => div().w(px(16.)).into_any_element(),
    };

    let mut title = h_flex()
        .gap_2()
        .items_center()
        .flex_wrap()
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child(facts.name.clone()),
        )
        .children(
            facts
                .branch
                .clone()
                .filter(|branch| *branch != facts.name)
                .map(|branch| chip(branch, Tone::Neutral, cx)),
        );
    if facts.unfinished {
        title = title.child(chip("removal unfinished", Tone::Warning, cx));
    }
    if let Some(activity) = &facts.activity {
        title = title.child(
            div()
                .text_xs()
                .text_color(ui::muted(cx))
                .child(activity.clone()),
        );
    }

    let mut details = v_flex().gap_1().min_w_0().flex_1().child(title);
    if !compact {
        details = details.child(
            div()
                .text_xs()
                .text_color(ui::muted(cx))
                .truncate()
                .child(facts.path.clone()),
        );
        details = details.children(
            facts
                .owns
                .iter()
                .map(|line| div().text_sm().child(line.clone()).into_any_element()),
        );
        if let Some((tone, text)) = facts.merge.clone() {
            details = details.child(h_flex().child(chip(text, tone, cx)));
        }
        if let Some(reason) = facts.reason.clone() {
            details = details.child(div().text_sm().text_color(ui::muted(cx)).child(reason));
        }
    }
    // What can be done about it, from core: an unknown entry's way on, an
    // unfinished removal's.
    if !entry.remedies.is_empty() && (!compact || entry.removing) {
        details = details.child(remedies::row(
            state,
            &format!("cleanup-remedy-{key}"),
            &entry.remedies,
            false,
        ));
    }

    h_flex()
        .id(SharedString::from(format!("cleanup-row-{key}")))
        .test_support()
        .w_full()
        .gap_3()
        .px_3()
        .py_2()
        .items_start()
        .border_b_1()
        .border_color(cx.theme().border)
        .child(div().pt_0p5().child(marker))
        .child(details)
        .into_any_element()
}

/// The banner on the Workspaces tab: workspaces Git forgot that still own
/// something, with the one way to deal with them.
pub(super) fn banner_of(
    state: &Entity<AppState>,
    summary: &devflow_core::gc::Summary,
    root: &std::path::Path,
    cx: &App,
) -> Option<Div> {
    let sentence = summary.sentence()?;
    let remedy = Remedy::CleanUp {
        root: root.to_path_buf(),
    };
    Some(banner(
        Tone::Warning,
        Lucide::Trash,
        shown(&sentence),
        Some(remedies::button_with_id(
            state,
            "cleanup-banner-open".into(),
            &remedy,
            false,
        )),
        cx,
    ))
}
