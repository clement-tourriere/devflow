//! One `sqlite` service on the Data tab: a database that is a file in each
//! workspace. There is no container, no server, no port and no password, so
//! the page has no Start or Stop: it shows the file of every workspace (its
//! size, journal mode, where its data came from, how it was cloned, who owns
//! it) and the project's snapshots, and offers the four things core can do to
//! them: take a snapshot, delete one, reset a database from a source, drop it.
//!
//! The model is `services::SqliteService`, built from local files on every
//! refresh. The buttons call `AppState`'s sqlite jobs (progress in the
//! activity log). Nothing here decides whether a reset may happen: a button
//! that core would refuse is disabled with the reason as its tooltip, and
//! core has the last word anyway (its error comes back with its remedies).
use super::{
    remedies, sqlite_dialogs, ui,
    ui::Tone,
    widgets::{caption, card, empty_row, hoverable, mono, section_header},
};
use crate::{
    model::{ProjectRef, shorten_home},
    redaction::shown,
    services::{ServiceTarget, SqliteDb, SqliteService, SqliteSnapshotRow},
    state::AppState,
};
use devflow_core::remedy::Remedy;
use gpui_kit::{
    assets::IconName as Lucide,
    component::{
        ActiveTheme as _, Disableable as _, IconName, Sizable as _,
        button::{Button, ButtonVariants as _},
        clipboard::Clipboard,
        h_flex, v_flex,
    },
    prelude::*,
    *,
};
use std::path::PathBuf;

// ---- what the tables say ------------------------------------------------------------

/// The cells of one database row, in words. Plain data, so what the page says
/// for a fixture can be checked without drawing it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseCells {
    /// The workspace, masked.
    pub workspace: String,
    /// "5.2 MB", "not created yet".
    pub size: String,
    /// "WAL", "rollback journal", "—".
    pub mode: String,
    /// "workspace main", "snapshot seed@v2", "empty", …
    pub source: String,
    /// "cloned by reflink", "nothing cloned".
    pub clone: String,
    /// Who owns the file, in words.
    pub owner: &'static str,
}

pub fn database_cells(db: &SqliteDb) -> DatabaseCells {
    DatabaseCells {
        workspace: db.workspace.shown(),
        size: db.size_text(),
        mode: db.mode_text(),
        source: shown(&db.source_text()),
        clone: db.clone_text(),
        owner: db.owner().label(),
    }
}

/// The cells of one snapshot row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotCells {
    /// `NAME@vN`, masked.
    pub label: String,
    pub size: String,
    /// "taken by reflink (blocks shared)", "taken by backup (a new file)".
    pub how: String,
    /// "from workspace main".
    pub from: String,
    pub created: String,
}

pub fn snapshot_cells(row: &SqliteSnapshotRow) -> SnapshotCells {
    SnapshotCells {
        label: row.label(),
        size: row.size_text(),
        how: row.how_text(),
        from: format!(
            "from {}",
            shown(&crate::services::sqlite_source_text(&row.source))
        ),
        created: row.created.clone(),
    }
}

/// The sentence under the page's title: what the library is and what this
/// kind of service does not have.
pub fn library_text(service: &SqliteService) -> String {
    format!("SQLite {} (bundled)", service.library)
}

/// The header of the databases section: how many files exist, as far as it is
/// known.
pub fn databases_hint(service: &SqliteService) -> String {
    match &service.databases {
        Ok(all) => {
            let existing = all.iter().filter(|d| d.exists).count();
            format!(
                "{existing} of {} workspace{} {}",
                all.len(),
                if all.len() == 1 { "" } else { "s" },
                if existing == 1 { "has one" } else { "have one" }
            )
        }
        Err(_) => "unavailable: the error above says why".to_string(),
    }
}

/// The header of the snapshots section.
pub fn snapshots_hint(service: &SqliteService) -> String {
    match &service.snapshots {
        Ok(snapshots) => snapshots.ready.len().to_string(),
        Err(_) => "unavailable: the error above says why".to_string(),
    }
}

/// The warning under the header when snapshot operations did not finish.
pub fn unfinished_text(count: usize) -> String {
    format!(
        "{count} snapshot operation(s) did not finish (a snapshot or a delete that was interrupted). Finishing them removes what is half made.",
    )
}

/// The warning when snapshots lost their file.
pub fn damaged_text(labels: &[String]) -> String {
    format!(
        "The file of {} is gone or is not the one that was recorded, so its data is lost. A snapshot like that cannot seed a workspace; delete it.",
        labels.join(", ")
    )
}

/// The remedies for what is wrong with the snapshots of `service`: the one
/// core offers for an interrupted operation.
pub fn snapshot_remedies(project: &ProjectRef, service: &SqliteService) -> Vec<Remedy> {
    if service.unfinished() > 0 {
        vec![Remedy::ReconcileService {
            root: project.root.clone(),
            key: service.key.clone(),
        }]
    } else {
        Vec::new()
    }
}

// ---- the view -------------------------------------------------------------------------

/// The widths of the databases table's columns (the "Data from" column takes
/// the rest), shared by its header and its rows so they line up.
const COL_WORKSPACE: f32 = 130.;
const COL_SIZE: f32 = 80.;
const COL_JOURNAL: f32 = 110.;
const COL_COPY: f32 = 120.;
const COL_OWNER: f32 = 200.;
const COL_ACTIONS: f32 = 84.;

/// Everything the view draws, copied out of the state so no borrow of it
/// lives through the frame.
struct Shown {
    service: SqliteService,
    busy: bool,
    /// The project this page was drawn for: every action captures it.
    project: ProjectRef,
}

impl Shown {
    fn target(&self) -> ServiceTarget {
        ServiceTarget {
            project: self.project.clone(),
            key: self.service.key.clone(),
        }
    }
}

pub struct SqliteView {
    state: Entity<AppState>,
    _subscription: Subscription,
}

impl SqliteView {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&state, |_, _, cx| cx.notify());
        Self {
            state,
            _subscription: subscription,
        }
    }

    fn shown(&self, cx: &App) -> Option<Shown> {
        let state = self.state.read(cx);
        let model = state.selection.current()?;
        let key = state.selected_data_service()?;
        Some(Shown {
            service: model.services.sqlite_service(&key)?.clone(),
            busy: state.busy().is_some(),
            project: model.project_ref(),
        })
    }

    fn header(&self, shown_: &Shown, cx: &mut Context<Self>) -> impl IntoElement {
        let service = &shown_.service;
        card(cx)
            .id("sqlite-header")
            .child(
                h_flex()
                    .p_3()
                    .gap_4()
                    .items_center()
                    .child(
                        v_flex()
                            .gap_0p5()
                            .min_w_0()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        div()
                                            .text_base()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(shown(&service.key)),
                                    )
                                    .child(ui::pill(
                                        "sqlite-engine",
                                        "sqlite",
                                        Tone::Info,
                                        "A sqlite service: the database is a file in each workspace. Nothing runs, so there is no container, server, port or password.",
                                    ))
                                    .child(
                                        mono(shown(&service.path), cx)
                                            .id("sqlite-path")
                                            .text_xs()
                                            .text_color(ui::muted(cx))
                                            .test_support(),
                                    ),
                            )
                            .child(
                                div()
                                    .id("sqlite-library")
                                    .text_xs()
                                    .text_color(ui::muted(cx))
                                    .child(library_text(service))
                                    .test_support(),
                            ),
                    )
                    .child(div().flex_1())
                    .when(service.require_cow, |row| {
                        row.child(ui::pill(
                            "sqlite-require-cow",
                            "copy refused",
                            Tone::Neutral,
                            "require_cow is on in the configuration: devflow refuses a copy that would not share blocks with its source.",
                        ))
                    }),
            )
            .test_support()
    }

    fn database_row(
        &self,
        shown_: &Shown,
        ix: usize,
        db: &SqliteDb,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cells = database_cells(db);
        let owner = db.owner();
        let refusal = owner.refusal();
        let busy = shown_.busy;
        let checkout = db.checkout.clone();
        let snapshot = {
            let (state, target, checkout) = (self.state.clone(), shown_.target(), checkout.clone());
            Button::new(("sqlite-db-snapshot", ix))
                .ghost()
                .xsmall()
                .icon(Lucide::Camera)
                .tooltip(if db.exists {
                    "Take a snapshot of this database".to_string()
                } else {
                    "This workspace has no database file yet, so there is nothing to snapshot"
                        .to_string()
                })
                .disabled(busy || !db.exists)
                .on_click(move |_, window, cx| {
                    sqlite_dialogs::open_snapshot(
                        &state,
                        target.clone(),
                        Some(checkout.clone()),
                        window,
                        cx,
                    )
                })
        };
        let reset = {
            let (state, target, checkout) = (self.state.clone(), shown_.target(), checkout.clone());
            Button::new(("sqlite-db-reset", ix))
                .ghost()
                .xsmall()
                .icon(IconName::RotateCw)
                .tooltip(
                    refusal
                        .unwrap_or("Replace this database with a fresh copy of a source")
                        .to_string(),
                )
                .disabled(busy || refusal.is_some())
                .on_click(move |_, window, cx| {
                    sqlite_dialogs::open_reset(&state, target.clone(), checkout.clone(), window, cx)
                })
        };
        let drop = {
            let (state, target, checkout) = (self.state.clone(), shown_.target(), checkout);
            Button::new(("sqlite-db-drop", ix))
                .ghost()
                .xsmall()
                .icon(IconName::Delete)
                .tooltip(
                    refusal
                        .unwrap_or("Remove this database file from the workspace")
                        .to_string(),
                )
                .disabled(busy || refusal.is_some())
                .on_click(move |_, window, cx| {
                    sqlite_dialogs::open_drop(&state, target.clone(), checkout.clone(), window, cx)
                })
        };
        let path = shown(&shorten_home(&db.file));
        let sidecars = (!db.sidecars.is_empty()).then(|| {
            format!(
                "{} beside it (a process may have it open)",
                db.sidecars.join(", ")
            )
        });
        hoverable(
            v_flex()
                .id(("sqlite-db", ix))
                .w_full()
                .px_3()
                .py_2()
                .gap_1()
                .border_b_1()
                .border_color(cx.theme().border.opacity(0.5)),
            cx,
        )
        .child(
            h_flex()
                .gap_3()
                .items_center()
                .child(
                    h_flex()
                        .w(px(COL_WORKSPACE))
                        .gap_1p5()
                        .items_center()
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(cells.workspace.clone()),
                        )
                        .when(db.is_default, |row| {
                            row.child(ui::pill(
                                ("sqlite-db-default", ix),
                                "default",
                                Tone::Neutral,
                                "The main checkout: its file is the project's own.",
                            ))
                        }),
                )
                .child(
                    div()
                        .id(("sqlite-db-size", ix))
                        .w(px(COL_SIZE))
                        .text_sm()
                        .child(cells.size.clone())
                        .test_support(),
                )
                .child(
                    div()
                        .id(("sqlite-db-mode", ix))
                        .w(px(COL_JOURNAL))
                        .text_sm()
                        .child(cells.mode.clone())
                        .test_support(),
                )
                .child(
                    div()
                        .id(("sqlite-db-source", ix))
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .child(cells.source.clone())
                        .test_support(),
                )
                .child(
                    div()
                        .id(("sqlite-db-clone", ix))
                        .w(px(COL_COPY))
                        .text_sm()
                        .child(cells.clone.clone())
                        .test_support(),
                )
                .child(
                    div()
                        .id(("sqlite-db-owner", ix))
                        .w(px(COL_OWNER))
                        .truncate()
                        .text_sm()
                        .text_color(ui::muted(cx))
                        .child(cells.owner)
                        .test_support(),
                )
                .child(
                    h_flex()
                        .w(px(COL_ACTIONS))
                        .gap_0p5()
                        .child(snapshot)
                        .child(reset)
                        .child(drop),
                ),
        )
        .child(
            h_flex()
                .gap_1()
                .items_center()
                .text_xs()
                .text_color(ui::muted(cx))
                .child(
                    div()
                        .id(("sqlite-db-path", ix))
                        .min_w_0()
                        .truncate()
                        .child(path)
                        .test_support(),
                )
                .child(
                    Clipboard::new(("sqlite-db-copy-path", ix))
                        .value(db.file.display().to_string())
                        .tooltip("Copy the path of the file"),
                )
                .children(sidecars.map(|text| {
                    div()
                        .id(("sqlite-db-sidecars", ix))
                        .child(text)
                        .test_support()
                })),
        )
        .test_support()
        .into_any_element()
    }

    fn databases(&self, shown_: &Shown, cx: &mut Context<Self>) -> impl IntoElement {
        let service = &shown_.service;
        let rows: Vec<AnyElement> = match &service.databases {
            Ok(all) => all
                .iter()
                .enumerate()
                .map(|(ix, db)| self.database_row(shown_, ix, db, cx))
                .collect(),
            Err(_) => Vec::new(),
        };
        let empty = matches!(&service.databases, Ok(all) if all.is_empty());
        let header = h_flex()
            .px_3()
            .py_1()
            .gap_3()
            .border_b_1()
            .border_color(cx.theme().border.opacity(0.5))
            .child(div().w(px(COL_WORKSPACE)).child(caption("Workspace", cx)))
            .child(div().w(px(COL_SIZE)).child(caption("Size", cx)))
            .child(div().w(px(COL_JOURNAL)).child(caption("Journal", cx)))
            .child(div().flex_1().min_w_0().child(caption("Data from", cx)))
            .child(div().w(px(COL_COPY)).child(caption("Copy", cx)))
            .child(div().w(px(COL_OWNER)).child(caption("Owner", cx)))
            .child(div().w(px(COL_ACTIONS)));
        card(cx)
            .id("sqlite-databases")
            .child(section_header(
                Lucide::File,
                "Databases",
                Some(databases_hint(service).into()),
                None,
                cx,
            ))
            .when(!rows.is_empty(), |card| card.child(header))
            .children(rows)
            .when(empty, |card| {
                card.child(empty_row(
                    "No workspace has a checkout yet. Creating one gives it its own copy of this file, taken from its data source.",
                    None,
                    cx,
                ))
            })
            .test_support()
    }

    fn snapshot_row(
        &self,
        shown_: &Shown,
        ix: usize,
        row: &SqliteSnapshotRow,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cells = snapshot_cells(row);
        let (state, target) = (self.state.clone(), shown_.target());
        // The snapshot is named by what was recorded; the label is only drawn.
        let (name, version, label) = (
            row.name.as_str().to_string(),
            row.version,
            cells.label.clone(),
        );
        hoverable(
            h_flex()
                .id(("sqlite-snapshot", ix))
                .w_full()
                .px_3()
                .py_1p5()
                .gap_3()
                .items_center()
                .border_b_1()
                .border_color(cx.theme().border.opacity(0.5)),
            cx,
        )
        .child(mono(cells.label.clone(), cx).w(px(180.)).truncate())
        .child(
            div()
                .id(("sqlite-snapshot-size", ix))
                .w(px(90.))
                .text_sm()
                .child(cells.size.clone())
                .test_support(),
        )
        .child(
            div()
                .id(("sqlite-snapshot-facts", ix))
                .flex_1()
                .min_w_0()
                .truncate()
                .text_xs()
                .text_color(ui::muted(cx))
                .child(format!(
                    "{} · {} · created {}",
                    cells.how, cells.from, cells.created
                ))
                .test_support(),
        )
        .children(row.damaged.then(|| {
            ui::pill(
                ("sqlite-snapshot-damaged", ix),
                "damaged",
                Tone::Warning,
                "The file of this snapshot is gone or is not the one that was recorded: its data is lost.",
            )
        }))
        .child(
            Button::new(("sqlite-snapshot-delete", ix))
                .ghost()
                .xsmall()
                .icon(IconName::Delete)
                .tooltip("Delete this snapshot")
                .disabled(shown_.busy)
                .on_click(move |_, window, cx| {
                    sqlite_dialogs::open_delete_snapshot(
                        &state,
                        target.clone(),
                        name.clone(),
                        version,
                        label.clone(),
                        window,
                        cx,
                    )
                }),
        )
        .test_support()
        .into_any_element()
    }

    fn snapshots(&self, shown_: &Shown, cx: &mut Context<Self>) -> impl IntoElement {
        let service = &shown_.service;
        let (state, target) = (self.state.clone(), shown_.target());
        let any_file = matches!(&service.databases, Ok(all) if all.iter().any(|d| d.exists));
        let create = Button::new("sqlite-snapshot-create")
            .small()
            .icon(IconName::Plus)
            .label("Take snapshot…")
            .tooltip(if any_file {
                "Keep a consistent copy of a workspace's database, which new workspaces can start from"
            } else {
                "No workspace has a database file yet, so there is nothing to snapshot"
            })
            .disabled(shown_.busy || !any_file)
            .on_click(move |_, window, cx| {
                sqlite_dialogs::open_snapshot(&state, target.clone(), None, window, cx)
            })
            .into_any_element();
        let rows: Vec<AnyElement> = match &service.snapshots {
            Ok(snapshots) => snapshots
                .ready
                .iter()
                .enumerate()
                .map(|(ix, row)| self.snapshot_row(shown_, ix, row, cx))
                .collect(),
            Err(_) => Vec::new(),
        };
        let empty = matches!(&service.snapshots, Ok(s) if s.ready.is_empty());
        card(cx)
            .id("sqlite-snapshots")
            .child(section_header(
                Lucide::Camera,
                "Snapshots",
                Some(snapshots_hint(service).into()),
                Some(create),
                cx,
            ))
            .children(rows)
            .when(empty, |card| {
                card.child(empty_row(
                    "A snapshot is a sealed, consistent copy of one workspace's database that new workspaces can start from.",
                    None,
                    cx,
                ))
            })
            .test_support()
    }
}

impl Render for SqliteView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(shown_) = self.shown(cx) else {
            return div().into_any_element();
        };
        let service = shown_.service.clone();
        let errors: Vec<(&'static str, String)> = [
            ("sqlite-databases-error", service.databases.as_ref().err()),
            ("sqlite-snapshots-error", service.snapshots.as_ref().err()),
        ]
        .into_iter()
        .filter_map(|(id, why)| Some((id, why?.clone())))
        .collect();
        let remedies_list = snapshot_remedies(&shown_.project, &service);
        let damaged: Vec<String> = service
            .snapshots
            .as_ref()
            .map(|s| {
                s.ready
                    .iter()
                    .filter(|r| r.damaged)
                    .map(|r| r.label())
                    .collect()
            })
            .unwrap_or_default();
        v_flex()
            .id("sqlite-view")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_3()
            .child(self.header(&shown_, cx))
            .children(
                errors
                    .into_iter()
                    .map(|(id, why)| ui::error_banner(why, cx).id(id).test_support()),
            )
            .when(service.unfinished() > 0, |page| {
                page.child(
                    v_flex()
                        .id("sqlite-unfinished")
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().warning)
                                .child(unfinished_text(service.unfinished())),
                        )
                        .child(remedies::row(
                            &self.state,
                            "sqlite-remedy",
                            &remedies_list,
                            false,
                        ))
                        .test_support(),
                )
            })
            .when(!damaged.is_empty(), |page| {
                page.child(
                    div()
                        .id("sqlite-damaged")
                        .text_sm()
                        .text_color(cx.theme().warning)
                        .child(damaged_text(&damaged))
                        .test_support(),
                )
            })
            .child(self.databases(&shown_, cx))
            .child(self.snapshots(&shown_, cx))
            .test_support()
            .into_any_element()
    }
}

/// The checkout the dialogs preselect: the workspace selected in the app.
pub fn preselected(state: &AppState) -> Option<PathBuf> {
    state.selection.current()?.selected_workspace.clone()
}
