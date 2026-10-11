//! One container service on the Data tab: its engine, image and Docker
//! engine, the copy-on-write result with what it means, each branch with its
//! owner, state and endpoints, the snapshots and the orphans.
//!
//! The model is `services::ServiceDetail`, built from core's reports; the
//! buttons call `AppState`'s service actions (jobs with progress in the
//! activity log). Nothing here decides how a clone works or whether a reset
//! may happen: core refuses what it must, and the view shows its error.
use super::{
    dialogs, service_dialogs, ui,
    ui::Tone,
    widgets::{card, chip, empty_row, hoverable, mono, section_header},
};
use crate::{
    model::ProjectRef,
    model::format_bytes,
    redaction::RawName,
    services::{
        BranchRow, BranchState, Cow, OrphanBranch, Orphans, Ownership, ServiceDetail,
        ServiceTarget, SnapshotRow,
    },
    state::AppState,
};
use gpui_kit::{
    assets::IconName as Lucide,
    component::{
        ActiveTheme as _, Disableable as _, IconName, Sizable as _,
        button::{Button, ButtonVariants as _},
        h_flex, v_flex,
    },
    prelude::*,
    *,
};

/// What a branch is called on the page: its workspace, else its (masked)
/// name. Never the recorded workspace name itself.
pub fn branch_label(row: &BranchRow) -> String {
    row.workspace
        .as_ref()
        .map(RawName::shown)
        .unwrap_or_else(|| row.name.clone())
}

/// What the pill and the buttons of a branch no workspace carries say. Only a
/// confirmed orphan says there is no workspace; an owner that could not be
/// resolved says so, with why, and is never offered for dropping.
pub fn ownership_help(ownership: &Ownership) -> String {
    match ownership {
        Ownership::Workspace => String::new(),
        Ownership::Orphan => "No workspace of this project carries the instance that owns this branch (core read every receipt). It can be dropped under Orphans below.".to_string(),
        Ownership::Unowned => "The catalog records no owner for this branch, so no workspace owns it. It is not listed under Orphans.".to_string(),
        Ownership::Unknown(why) => format!(
            "Who owns this branch is unknown: {why}. It is not offered for dropping."
        ),
    }
}

/// The header of the branch list: how many, and how many run as far as Docker
/// said (a branch whose state could not be read makes the count a lower bound).
pub fn branches_hint(detail: &ServiceDetail) -> String {
    use super::data::{Running, running_word};
    let Some((running, reading)) = Running::of(Some(detail)).counted() else {
        return String::new();
    };
    if matches!(reading, super::reading::Reading::Unknown { .. }) {
        return "unavailable: the error above says why".to_string();
    }
    let reading = match reading {
        super::reading::Reading::Partial { .. } => super::reading::Reading::Partial {
            unread: detail
                .branches
                .iter()
                .filter(|b| matches!(b.state, BranchState::Unknown))
                .count(),
            total: detail.branches.len(),
            reason: "Docker could not say what their containers are doing".to_string(),
        },
        other => other,
    };
    let mut text = format!(
        "{} · {}",
        detail.branches.len(),
        running_word(running, &reading)
    );
    if let Some(note) = reading.note("Docker", "branches") {
        text.push_str(&format!(" ({note})"));
    }
    text
}

/// The line under a branch: where it came from, when, how, and what it cost.
pub fn branch_facts(row: &BranchRow) -> String {
    let mut facts = vec![format!("from {}", row.source)];
    if let Some(database) = &row.database {
        facts.push(format!("database {database}"));
    }
    if let Some(captured) = &row.captured {
        facts.push(format!(
            "captured {captured}{}{}",
            if row.capture_last_save {
                " (the source's last save; nothing had changed since)"
            } else {
                ""
            },
            if row.capture_exact {
                ""
            } else {
                " (the source saved again while it was copied, so the clone may be newer)"
            }
        ));
    }
    if let Some(method) = &row.clone {
        facts.push(format!("cloned by {method}"));
    }
    if let Some(bytes) = row.allocated {
        facts.push(format!("{} allocated", format_bytes(bytes)));
    }
    if let Some(ms) = row.source_downtime_ms {
        facts.push(format!(
            "source stopped for {}",
            crate::model::format_millis(ms)
        ));
    }
    facts.join(" · ")
}

/// Everything the view draws, copied out of the state so no borrow of it
/// lives through the frame.
struct Shown {
    detail: Option<ServiceDetail>,
    orphans: Vec<OrphanBranch>,
    /// Receipts that could not be read: who owns which branch is unknown.
    ownership_unknown: Vec<String>,
    /// Core's ownership scan, which says whether "no workspace" is known.
    scan: Orphans,
    busy: bool,
    key: String,
    /// The project this page was drawn for: every action captures it.
    project: ProjectRef,
}

impl Shown {
    fn target(&self) -> ServiceTarget {
        ServiceTarget {
            project: self.project.clone(),
            key: self.key.clone(),
        }
    }
}

pub struct ServiceView {
    state: Entity<AppState>,
    _subscription: Subscription,
}

impl ServiceView {
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
            detail: model.services.detail(&key).cloned(),
            orphans: model
                .services
                .orphans
                .branches
                .iter()
                .filter(|o| o.service == key)
                .cloned()
                .collect(),
            ownership_unknown: model.services.orphans.incomplete.clone(),
            scan: model.services.orphans.clone(),
            busy: state.busy().is_some(),
            key,
            project: model.project_ref(),
        })
    }

    fn header(
        &self,
        target: &ServiceTarget,
        detail: &ServiceDetail,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let docker = match &detail.docker {
            Some(docker) => format!(
                "{} {}",
                if docker.orbstack {
                    "OrbStack"
                } else {
                    "Docker"
                },
                docker.id
            ),
            None => "Docker is not available".to_string(),
        };
        let (measure_state, reconcile_state) = (self.state.clone(), self.state.clone());
        let (measure_target, reconcile_target) = (target.clone(), target.clone());
        card(cx)
            .id("service-header")
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
                                            .child(detail.key.clone()),
                                    )
                                    .child(ui::pill(
                                        "service-engine",
                                        detail.engine.clone(),
                                        Tone::Info,
                                        format!(
                                            "A {} service: each workspace gets a branch of its own, \
                                             a container with its own port and password, cloned from \
                                             its source.",
                                            detail.engine
                                        ),
                                    ))
                                    .child(mono(detail.image.clone(), cx).text_xs().text_color(ui::muted(cx))),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(ui::muted(cx))
                                    .child(format!("Docker engine: {docker}")),
                            ),
                    )
                    .child(div().flex_1())
                    .child(ui::cow_pill("service-cow", &detail.cow))
                    .when(detail.require_cow, |row| {
                        row.child(ui::pill(
                            "service-require-cow",
                            "copy refused",
                            Tone::Neutral,
                            "require_cow is on in the configuration: devflow refuses a clone \
                             that would not share blocks with its source.",
                        ))
                    })
                    .child(
                        Button::new("service-measure")
                            .ghost()
                            .small()
                            .icon(Lucide::Ruler)
                            .label(if detail.cow.measured_at.is_some() {
                                "Measure again"
                            } else {
                                "Measure"
                            })
                            .tooltip("Write 64 MiB into a scratch volume in the Docker VM and clone it, to see whether the clone shares blocks")
                            .disabled(busy || detail.docker.is_none())
                            .on_click(move |_, window, cx| {
                                let target = measure_target.clone();
                                measure_state.update(cx, |s, cx| s.service_measure(target, window, cx))
                            }),
                    )
                    .child(
                        Button::new("service-reconcile")
                            .ghost()
                            .small()
                            .icon(Lucide::Wrench)
                            .label("Reconcile")
                            .tooltip("Finish or roll back operations a crash interrupted")
                            .disabled(busy || detail.docker.is_none())
                            .on_click(move |_, window, cx| {
                                let target = reconcile_target.clone();
                                reconcile_state
                                    .update(cx, |s, cx| s.service_reconcile(target, window, cx))
                            }),
                    ),
            )
            .children(detail.last_clone.as_ref().map(|last| {
                div()
                    .id("service-last-clone")
                    .px_3()
                    .pb_2()
                    .text_xs()
                    .text_color(ui::muted(cx))
                    .child(last.text())
                    .test_support()
            }))
            .when(detail.unfinished > 0, |card| {
                card.child(
                    div()
                        .id("service-unfinished")
                        .px_3()
                        .pb_2()
                        .text_sm()
                        .text_color(cx.theme().warning)
                        .child(format!(
                            "{} operation(s) did not finish (a clone, snapshot or reset that was interrupted). Reconcile completes or rolls them back.",
                            detail.unfinished
                        ))
                        .test_support(),
                )
            })
            .test_support()
    }

    fn branch_row(
        &self,
        target: &ServiceTarget,
        ix: usize,
        row: &BranchRow,
        ownership: &Ownership,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = branch_label(row);
        let no_owner = ownership_help(ownership);
        let transient = matches!(row.state, BranchState::Creating | BranchState::Deleting);
        let state = self.state.clone();
        let toggle = {
            let (state, target, id, label) = (state.clone(), target.clone(), row.id, label.clone());
            let running = row.state.running();
            Button::new(if running {
                ("service-stop", ix)
            } else {
                ("service-start", ix)
            })
            .ghost()
            .xsmall()
            .icon(if running {
                IconName::Square
            } else {
                IconName::Play
            })
            .tooltip(if running {
                "Stop the container; the data stays in its volume"
            } else {
                "Start the container"
            })
            .disabled(busy || transient)
            .on_click(move |_, window, cx| {
                let (target, label) = (target.clone(), label.clone());
                state.update(cx, |s, cx| {
                    if running {
                        s.service_stop(target, id, label, window, cx)
                    } else {
                        s.service_start(target, id, label, window, cx)
                    }
                })
            })
        };
        let copy = {
            let (state, target, id) = (state.clone(), target.clone(), row.id);
            Button::new(("service-copy-url", ix))
                .ghost()
                .xsmall()
                .icon(IconName::Copy)
                .tooltip("Copy the connection URL with its password")
                .disabled(transient)
                .on_click(move |_, window, cx| {
                    let target = target.clone();
                    state.update(cx, |s, cx| s.copy_service_url(target, id, window, cx))
                })
        };
        let snapshot = {
            let (state, target, row) = (state.clone(), target.clone(), row.clone());
            Button::new(("service-snapshot", ix))
                .ghost()
                .xsmall()
                .icon(Lucide::Camera)
                .tooltip(if row.workspace.is_some() {
                    "Seal a snapshot of this branch's data".to_string()
                } else {
                    no_owner.clone()
                })
                .disabled(busy || transient || row.workspace.is_none())
                .on_click(move |_, window, cx| {
                    service_dialogs::open_snapshot(
                        &state,
                        target.clone(),
                        Some(row.clone()),
                        window,
                        cx,
                    )
                })
        };
        let reset = {
            let (state, target, row) = (state.clone(), target.clone(), row.clone());
            Button::new(("service-reset", ix))
                .ghost()
                .xsmall()
                .icon(IconName::RotateCw)
                .tooltip(if row.workspace.is_some() {
                    "Replace this branch's data with a fresh clone".to_string()
                } else {
                    no_owner.clone()
                })
                .disabled(busy || transient || row.workspace.is_none())
                .on_click(move |_, window, cx| {
                    service_dialogs::open_reset(&state, target.clone(), row.clone(), window, cx)
                })
        };
        hoverable(
            v_flex()
                .id(("service-branch", ix))
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
                .child(ui::pill(
                    ("service-branch-state", ix),
                    row.state.label(),
                    ui::branch_tone(row.state),
                    row.state.help(),
                ))
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(label),
                )
                .children(match ownership {
                    Ownership::Workspace => None,
                    Ownership::Orphan => Some(ui::pill(
                        ("service-branch-orphan", ix),
                        "no workspace",
                        Tone::Warning,
                        no_owner.clone(),
                    )),
                    Ownership::Unowned => Some(ui::pill(
                        ("service-branch-unowned", ix),
                        "no owner recorded",
                        Tone::Neutral,
                        no_owner.clone(),
                    )),
                    Ownership::Unknown(_) => Some(ui::pill(
                        ("service-branch-owner-unknown", ix),
                        "owner unknown",
                        Tone::Warning,
                        no_owner.clone(),
                    )),
                })
                .children(row.endpoint.clone().map(|endpoint| {
                    div()
                        .id(("service-endpoint", ix))
                        .child(chip(endpoint, Tone::Neutral, cx))
                        .tooltip(|window, cx| {
                            gpui_kit::component::tooltip::Tooltip::new(
                                "Where this machine reaches the branch (loopback).",
                            )
                            .build(window, cx)
                        })
                        .test_support()
                }))
                .children(row.container_endpoint.clone().map(|endpoint| {
                    div()
                        .id(("service-container-endpoint", ix))
                        .child(chip(endpoint, Tone::Info, cx))
                        .tooltip(|window, cx| {
                            gpui_kit::component::tooltip::Tooltip::new(
                                "Where containers on the same Docker networks reach it (OrbStack).",
                            )
                            .build(window, cx)
                        })
                        .test_support()
                }))
                .child(div().flex_1())
                .child(
                    h_flex()
                        .gap_0p5()
                        .child(toggle)
                        .child(copy)
                        .child(snapshot)
                        .child(reset),
                ),
        )
        .child(
            div()
                .id(("service-branch-facts", ix))
                .text_xs()
                .text_color(ui::muted(cx))
                .child(branch_facts(row))
                .test_support(),
        )
        .children(row.masked_url.clone().map(|url| {
            mono(url, cx)
                .id(("service-masked-url", ix))
                .text_xs()
                .text_color(ui::muted(cx))
                .test_support()
        }))
        .test_support()
        .into_any_element()
    }

    fn branches(
        &self,
        shown: &Shown,
        detail: &ServiceDetail,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let rows: Vec<AnyElement> = detail
            .branches
            .iter()
            .enumerate()
            .map(|(ix, row)| {
                let ownership = row.ownership(&shown.scan);
                self.branch_row(&shown.target(), ix, row, &ownership, shown.busy, cx)
            })
            .collect();
        let unavailable = detail.docker.is_none() || detail.error.is_some();
        let hint = branches_hint(detail);
        card(cx)
            .id("service-branches")
            .child(section_header(
                Lucide::GitBranch,
                "Branches",
                Some(hint.into()),
                None,
                cx,
            ))
            .children(rows)
            .when(detail.branches.is_empty() && !unavailable, |card| {
                let state = self.state.clone();
                card.child(empty_row(
                    "No workspace has a branch of this service yet. Creating a workspace gives it one, cloned from its data source.",
                    Some(
                        Button::new("service-new-workspace")
                            .primary()
                            .label("New workspace")
                            .disabled(shown.busy)
                            .on_click(move |_, window, cx| {
                                dialogs::open_new_workspace(&state, window, cx)
                            }),
                    ),
                    cx,
                ))
            })
            .test_support()
    }

    fn snapshot_row(
        &self,
        target: &ServiceTarget,
        ix: usize,
        row: &SnapshotRow,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (state, target, row) = (self.state.clone(), target.clone(), row.clone());
        let mut facts = vec![format!("from {}", row.source)];
        if let Some(captured) = &row.captured {
            facts.push(format!("captured {captured}"));
        }
        if let Some(method) = &row.clone {
            facts.push(format!("cloned by {method}"));
        }
        if let Some(bytes) = row.allocated {
            facts.push(format!("{} allocated", format_bytes(bytes)));
        }
        if let Some(ms) = row.source_downtime_ms {
            facts.push(format!(
                "source stopped for {}",
                crate::model::format_millis(ms)
            ));
        }
        facts.push(format!("created {}", row.created));
        let label = row.label();
        hoverable(
            h_flex()
                .id(("service-snapshot-row", ix))
                .w_full()
                .px_3()
                .py_1p5()
                .gap_3()
                .items_center()
                .border_b_1()
                .border_color(cx.theme().border.opacity(0.5)),
            cx,
        )
        .child(mono(label.clone(), cx).w(px(160.)).truncate())
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_xs()
                .text_color(ui::muted(cx))
                .child(facts.join(" · ")),
        )
        .child(
            Button::new(("service-snapshot-delete", ix))
                .ghost()
                .xsmall()
                .icon(IconName::Delete)
                .tooltip("Delete this snapshot")
                .disabled(busy)
                .on_click(move |_, window, cx| {
                    // The snapshot is named by its id; the name is only drawn,
                    // masked, in `label`.
                    let (state, target, id, label) =
                        (state.clone(), target.clone(), row.id, row.label());
                    let key = target.key.clone();
                    dialogs::open_confirm(
                        window,
                        cx,
                        "Delete snapshot",
                        vec![
                            format!("Delete {label} of {key}?"),
                            "Workspaces already cloned from it keep their data. Version numbers are never reused."
                                .into(),
                        ],
                        "Delete",
                        move |window, cx| {
                            let (target, label) = (target.clone(), label.clone());
                            state.update(cx, |s, cx| {
                                s.service_delete_snapshot(target, id, label, window, cx)
                            });
                        },
                    );
                }),
        )
        .test_support()
        .into_any_element()
    }

    fn snapshots(
        &self,
        shown: &Shown,
        detail: &ServiceDetail,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let state = self.state.clone();
        let target = shown.target();
        let unavailable = detail.docker.is_none() || detail.error.is_some();
        let create = Button::new("service-snapshot-create")
            .small()
            .icon(IconName::Plus)
            .label("Create snapshot…")
            .disabled(shown.busy || detail.branches.iter().all(|b| b.workspace.is_none()))
            .on_click(move |_, window, cx| {
                service_dialogs::open_snapshot(&state, target.clone(), None, window, cx)
            })
            .into_any_element();
        let rows: Vec<AnyElement> = detail
            .snapshots
            .iter()
            .enumerate()
            .map(|(ix, row)| self.snapshot_row(&shown.target(), ix, row, shown.busy, cx))
            .collect();
        card(cx)
            .id("service-snapshots")
            .child(section_header(
                Lucide::Camera,
                "Snapshots",
                Some(if unavailable {
                    "unavailable".into()
                } else {
                    format!("{}", detail.snapshots.len()).into()
                }),
                Some(create),
                cx,
            ))
            .children(rows)
            .when(detail.snapshots.is_empty() && !unavailable, |card| {
                card.child(empty_row(
                    "A snapshot is a sealed, read-only copy of a branch's data that new workspaces can start from.",
                    None,
                    cx,
                ))
            })
            .test_support()
    }

    fn orphans(&self, shown: &Shown, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<AnyElement> = shown
            .orphans
            .iter()
            .enumerate()
            .map(|(ix, orphan)| {
                let (state, target, orphan) = (self.state.clone(), shown.target(), orphan.clone());
                let reason = orphan.reason.clone();
                let name = orphan.name.clone();
                hoverable(
                    h_flex()
                        .id(("service-orphan", ix))
                        .w_full()
                        .px_3()
                        .py_1p5()
                        .gap_3()
                        .items_center()
                        .border_b_1()
                        .border_color(cx.theme().border.opacity(0.5)),
                    cx,
                )
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(name),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(ui::muted(cx))
                        .child(reason),
                )
                .child(
                    Button::new(("service-orphan-drop", ix))
                        .small()
                        .icon(IconName::Delete)
                        .label("Drop…")
                        .disabled(shown.busy)
                        .on_click(move |_, window, cx| {
                            service_dialogs::open_drop(
                                &state,
                                target.clone(),
                                orphan.clone(),
                                window,
                                cx,
                            )
                        }),
                )
                .test_support()
                .into_any_element()
            })
            .collect();
        card(cx)
            .id("service-orphans")
            .child(section_header(
                Lucide::TriangleAlert,
                "Orphans",
                Some("branches no workspace of this project owns".into()),
                None,
                cx,
            ))
            .children(rows)
            .test_support()
    }
}

impl Render for ServiceView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(shown) = self.shown(cx) else {
            return div().into_any_element();
        };
        let Some(detail) = shown.detail.clone() else {
            return ui::loading("Reading the service…", cx).into_any_element();
        };
        v_flex()
            .id("service-view")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_3()
            .child(self.header(&shown.target(), &detail, shown.busy, cx))
            .children(
                detail
                    .error
                    .clone()
                    .map(|e| ui::error_banner(e, cx).id("service-error").test_support()),
            )
            .children((!shown.ownership_unknown.is_empty()).then(|| {
                ui::error_banner(ownership_unknown_text(&shown.ownership_unknown), cx)
                    .id("service-ownership-unknown")
                    .test_support()
            }))
            .child(self.branches(&shown, &detail, cx))
            .child(self.snapshots(&shown, &detail, cx))
            .when(!shown.orphans.is_empty(), |page| {
                page.child(self.orphans(&shown, cx))
            })
            .test_support()
            .into_any_element()
    }
}

/// What the page says while a workspace's receipt cannot be read: no branch is
/// offered for dropping as an orphan, because the owner may be in that receipt.
pub fn ownership_unknown_text(problems: &[String]) -> String {
    format!(
        "Who owns which branch is unknown: {} receipt(s) cannot be read ({}). No branch is offered as an orphan until they are repaired.",
        problems.len(),
        problems.join("; ")
    )
}

/// What the copy-on-write pill says.
pub fn cow_text(cow: &Cow) -> String {
    match (cow.capability, cow.measured_at) {
        (devflow_core::config::CowCapability::Unknown, None) => "not measured".to_string(),
        (capability, _) => crate::model::cow_label(capability).to_string(),
    }
}
