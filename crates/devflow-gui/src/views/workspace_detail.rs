//! The detail pane of the selected workspace (Workspaces tab): where it is,
//! how its setup went phase by phase, how to connect to its data, and what it
//! costs in time and disk, said honestly.
//!
//! Everything comes from the workspace's receipt and the loaded project; the
//! only work of its own is measuring the payload paths, off the UI thread.
//! No lifecycle decision is made here: "Retry setup" calls core through the
//! same operation as the table's row action.
use super::{
    dialogs, processes, remedies, ui,
    ui::Tone,
    widgets::{banner, caption, card, chip, empty_row, hoverable, mono, section_header},
};
use crate::{
    jobs,
    model::{
        PitchforkModel, ProcessTarget, ProjectModel, Tab, WorkspaceState, cow_text, format_bytes,
        format_when, shorten_home,
    },
    redaction::{RefusalView, shown},
    state::{AppState, TrustPurpose},
};
use devflow_core::{
    config::CowCapability,
    cow::{self, Measure},
    receipt::{PayloadOutcome, Phase, PhaseState, Receipt, ServiceBinding},
    redact,
    remedy::Remedy,
    setup::WorkspaceStatus,
};
use gpui_kit::{
    assets::IconName as Lucide,
    component::{
        ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _,
        button::{Button, ButtonVariants as _},
        clipboard::Clipboard,
        h_flex,
        spinner::Spinner,
        v_flex,
    },
    prelude::*,
    *,
};
use std::{path::PathBuf, time::Duration};
use uuid::Uuid;

// ---- plain data ------------------------------------------------------------------

/// One phase of the setup pipeline as the receipt remembers it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhaseRow {
    pub phase: Phase,
    pub title: &'static str,
    pub state: PhaseState,
    /// What the phase reported (credentials masked).
    pub detail: Option<String>,
    /// How long the attempt took, when the receipt recorded it.
    pub duration: Option<Duration>,
    /// A skip that leaves the workspace needing attention.
    pub required_skip: bool,
    /// A skip someone asked for (`--no-db`, `--no-hooks`).
    pub deliberate: bool,
}

/// One scratch database of a binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScratchRow {
    pub name: String,
    pub database: String,
    /// The real URL (copy and reveal only).
    pub url: Option<String>,
}

/// How to reach one service's data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connection {
    pub service: String,
    pub engine: String,
    pub database: String,
    pub role: String,
    /// A container engine's branch (no database or role of its own).
    pub container: bool,
    /// The line under the name: the database and role, or how the branch
    /// was cloned.
    pub facts: String,
    /// `host:port` the host reaches.
    pub host_endpoint: Option<String>,
    /// `host:port` containers on the same Docker networks reach (OrbStack).
    pub container_endpoint: Option<String>,
    /// The real URL: copied and revealed on request, masked everywhere else.
    pub url: String,
    pub container_url: Option<String>,
    pub source: String,
    /// What the workspace may do to the branch.
    pub ownership: &'static str,
    pub scratch: Vec<ScratchRow>,
}

impl Connection {
    pub fn masked_url(&self) -> String {
        redact::text(&self.url)
    }

    pub fn masked_container_url(&self) -> Option<String> {
        self.container_url.as_deref().map(redact::text)
    }
}

/// What the processes section reads besides the receipt: pitchfork's picture
/// of the project (the selected workspace's status among it), a start core
/// refused, and whether a job is running.
struct ProcessesData {
    pitchfork: PitchforkModel,
    /// The workspace `pitchfork.status` describes: what its buttons act on.
    target: Option<ProcessTarget>,
    refusal: Option<RefusalView>,
    busy: bool,
}

/// What the pane shows about one workspace.
#[derive(Clone, Debug)]
pub struct Detail {
    pub name: String,
    pub branch: String,
    pub path: PathBuf,
    pub state: WorkspaceState,
    pub is_default: bool,
    pub phases: Vec<PhaseRow>,
    /// The receipt records durations for at least one phase.
    pub total: Option<Duration>,
    pub set_up_at: Option<String>,
    pub payload: Vec<(String, PayloadOutcome)>,
    pub connections: Vec<Connection>,
    /// The PostgreSQL branch's logical size and the server's copy-on-write
    /// state.
    pub database: Option<DatabaseDisk>,
    /// How each container service's branch was cloned.
    pub clones: Vec<ServiceClone>,
    /// What core says can be done about the workspace's state: approve what
    /// it asks to run, run setup. Empty when it is ready, or busy.
    pub remedies: Vec<Remedy>,
}

/// How a container service's branch was cloned, as its receipt binding
/// reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceClone {
    pub service: String,
    pub engine: String,
    /// `reflink` or `copy`; `None` for an empty root.
    pub method: Option<String>,
    pub captured: Option<String>,
    pub allocated: Option<i64>,
    /// Milliseconds the source was stopped for the copy (MySQL).
    pub source_downtime_ms: Option<u64>,
}

impl ServiceClone {
    fn of(key: &str, binding: &ServiceBinding) -> Option<Self> {
        let cloned = binding.cloned.as_ref()?;
        Some(Self {
            service: key.to_string(),
            engine: binding.engine.to_string(),
            method: cloned.method.map(|m| m.to_string()),
            captured: cloned.captured_at.map(format_when),
            allocated: cloned.allocated_bytes,
            source_downtime_ms: cloned.source_downtime_ms,
        })
    }

    /// One sentence: how, when and what it took.
    pub fn text(&self) -> String {
        let mut parts = vec![match &self.method {
            Some(method) => format!("cloned by {method}"),
            None => "started empty (nothing was cloned)".to_string(),
        }];
        if let Some(captured) = &self.captured {
            parts.push(format!("captured {captured}"));
        }
        if let Some(bytes) = self.allocated {
            parts.push(format!(
                "about {} allocated in the Docker VM",
                format_bytes(bytes)
            ));
        }
        if let Some(ms) = self.source_downtime_ms {
            parts.push(format!(
                "its source was stopped for {}",
                crate::model::format_millis(ms)
            ));
        }
        parts.join(", ")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseDisk {
    pub size_bytes: Option<i64>,
    pub cow: Option<(CowCapability, Option<devflow_core::db::CowSource>)>,
}

fn phase_title(phase: Phase) -> &'static str {
    match phase {
        Phase::Materialize => "Worktree",
        Phase::Payload => "Payload",
        Phase::Services => "Services",
        Phase::Env => "Environment",
        Phase::Hooks => "Hooks",
    }
}

/// The phases of a receipt in pipeline order. A workspace with no receipt has
/// none to show.
pub fn phase_rows(receipt: &Receipt) -> Vec<PhaseRow> {
    Phase::ALL
        .iter()
        .map(|&phase| {
            let record = receipt.phase(phase);
            PhaseRow {
                phase,
                title: phase_title(phase),
                state: record.state,
                detail: record
                    .detail
                    .as_deref()
                    .map(|d| redact::text(devflow_core::receipt::plain_detail(d))),
                duration: record.duration(),
                required_skip: record.required_skip,
                deliberate: record.deliberate,
            }
        })
        .collect()
}

/// The hint of the Processes card: the daemons running of those declared, or
/// that this is not known when pitchfork's state could not be read (the
/// declared daemons then carry a fallback "not started").
pub fn process_hint(
    running: usize,
    declared: usize,
    namespace: &str,
    unread: Option<&str>,
) -> String {
    match unread {
        None => format!("{running} of {declared} running · {namespace}"),
        Some(_) => format!("{declared} declared · running state unknown · {namespace}"),
    }
}

/// `host:port` of the authority of a connection URL, without credentials.
pub fn host_endpoint(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let endpoint = authority.rsplit_once('@').map_or(authority, |(_, e)| e);
    (!endpoint.is_empty()).then(|| endpoint.to_string())
}

/// `url` with another `host:port`, the login and password kept.
pub fn url_with_endpoint(url: &str, endpoint: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let user = authority.rsplit_once('@').map(|(user, _)| user);
    Some(match user {
        Some(user) => format!("{scheme}://{user}@{endpoint}{tail}"),
        None => format!("{scheme}://{endpoint}{tail}"),
    })
}

/// `postgres_container` is where containers reach the shared PostgreSQL
/// server (OrbStack); a container engine's own endpoint is in its binding.
fn connection(key: &str, binding: &ServiceBinding, postgres_container: Option<&str>) -> Connection {
    let container = binding.engine.is_container();
    let container_endpoint = if container {
        binding.endpoint.as_deref()
    } else {
        postgres_container
    };
    let scratch = binding
        .extras
        .iter()
        .map(|(name, extra)| ScratchRow {
            name: name.clone(),
            database: extra.database.clone(),
            url: binding.extra_url(name),
        })
        .collect();
    Connection {
        service: key.to_string(),
        engine: binding.engine.to_string(),
        database: binding.database.clone(),
        role: binding.role.clone(),
        container,
        facts: redact::text(&match ServiceClone::of(key, binding) {
            Some(clone) if container => {
                // A MySQL branch has a database of its own; Valkey has none.
                if binding.database.is_empty() {
                    clone.text()
                } else {
                    format!("database {} · {}", binding.database, clone.text())
                }
            }
            _ => format!("database {} · role {}", binding.database, binding.role),
        }),
        host_endpoint: host_endpoint(&binding.url),
        container_endpoint: container_endpoint.map(str::to_string),
        url: binding.url.clone(),
        container_url: container_endpoint.and_then(|e| url_with_endpoint(&binding.url, e)),
        // "workspace:NAME", "snapshot:NAME@vN": a name can look like a credential.
        source: redact::text(&binding.source),
        ownership: if binding.is_url_only() {
            "attached by URL: the workspace cannot reset or drop it"
        } else if binding.owned {
            "created by this workspace"
        } else {
            "shared: another workspace created it"
        },
        scratch,
    }
}

/// `hours:minutes`-free duration text: `340 ms`, `1.2 s`, `2m 05s`.
pub fn format_duration(duration: Duration) -> String {
    let ms = duration.as_millis();
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        let secs = duration.as_secs();
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}

pub fn outcome_text(outcome: PayloadOutcome) -> &'static str {
    match outcome {
        PayloadOutcome::Cloned => "cloned (copy-on-write)",
        PayloadOutcome::Mixed => "partly cloned, partly copied",
        PayloadOutcome::Copied => "copied (no copy-on-write between these locations)",
        PayloadOutcome::AlreadyPresent => "already there",
        PayloadOutcome::Missing => "not in the source",
    }
}

impl Detail {
    /// The workspace's name as the pane's header draws it; `name` is the
    /// identity.
    pub fn title(&self) -> String {
        shown(&self.name)
    }

    pub fn of(model: &ProjectModel, status: &WorkspaceStatus) -> Self {
        let receipt = status.receipt.as_ref();
        let phases = receipt.map(phase_rows).unwrap_or_default();
        let durations: Vec<Duration> = phases.iter().filter_map(|p| p.duration).collect();
        let container = model
            .database
            .status
            .as_ref()
            .and_then(|s| s.container_endpoint.as_deref());
        let connections: Vec<Connection> = receipt
            .map(|r| {
                r.services
                    .iter()
                    .filter(|(_, binding)| !binding.orphaned)
                    .map(|(key, binding)| connection(key, binding, container))
                    .collect()
            })
            .unwrap_or_default();
        let clones = receipt
            .map(|r| {
                r.services
                    .iter()
                    .filter(|(_, binding)| !binding.orphaned && binding.engine.is_container())
                    .filter_map(|(key, binding)| ServiceClone::of(key, binding))
                    .collect()
            })
            .unwrap_or_default();
        let database = receipt.and_then(|r| {
            let (_, binding) = r
                .services
                .iter()
                .find(|(_, b)| !b.orphaned && !b.engine.is_container())?;
            let branch = binding
                .branch_id
                .and_then(|id| model.database.branches.iter().find(|b| b.id == id));
            Some(DatabaseDisk {
                size_bytes: branch.and_then(|b| b.size_bytes),
                cow: model
                    .database
                    .status
                    .as_ref()
                    .map(|s| (s.cow, s.cow_source)),
            })
        });
        Self {
            name: status.workspace.name.clone(),
            // Drawn only: the Git branch can look like a credential too.
            branch: status
                .workspace
                .branch
                .as_deref()
                .map(shown)
                .unwrap_or_else(|| "(detached)".into()),
            path: status.workspace.path.clone(),
            state: WorkspaceState::of(status),
            is_default: status.workspace.is_default,
            total: (!durations.is_empty()).then(|| durations.iter().sum()),
            set_up_at: receipt.map(|r| format_when(r.created_at)),
            payload: receipt
                .map(|r| {
                    r.payload
                        .iter()
                        .map(|p| (p.path.clone(), p.outcome))
                        .collect()
                })
                .unwrap_or_default(),
            phases,
            connections,
            database,
            clones,
            remedies: model
                .project
                .as_ref()
                .map(|project| status.remedies(project))
                .unwrap_or_default(),
        }
    }

    /// The remedy of this kind the workspace has, if any.
    pub fn remedy(&self, kind: &str) -> Option<&Remedy> {
        self.remedies.iter().find(|r| r.kind() == kind)
    }

    /// Phases that went wrong or were left for someone to resolve.
    pub fn problems(&self) -> Vec<&PhaseRow> {
        self.phases
            .iter()
            .filter(|p| p.state == PhaseState::Failed || (p.required_skip && !p.deliberate))
            .collect()
    }
}

/// What the file system says about one payload path.
#[derive(Clone, Debug)]
pub struct PathSize {
    pub path: String,
    pub outcome: PayloadOutcome,
    pub measure: Result<Measure, String>,
}

/// What a measurement says, without claiming more than it measured: the sum
/// of the file lengths ("apparent"), the blocks the file system reports for
/// them ("allocated", shared blocks counted in full, directory metadata not
/// included) and whether some entries could not be read.
pub fn measure_text(m: &Measure) -> String {
    let mut text = format!(
        "{}, {} apparent (the sum of the file lengths)",
        devflow_core::text::counted(m.entries, "item"),
        format_bytes(m.apparent_bytes.min(i64::MAX as u64) as i64),
    );
    if let Some(allocated) = m.allocated_bytes {
        text.push_str(&format!(
            ", {} allocated (blocks as the file system reports them; blocks shared with the \
             source are counted in full, directory metadata is not included)",
            format_bytes(allocated.min(i64::MAX as u64) as i64)
        ));
    }
    if m.partial {
        text.push_str(
            "; incomplete: some entries could not be read, so these figures are lower than the \
             truth",
        );
    }
    text
}

/// The lines under "Disk", given what was measured: figures as the file
/// system reports them, and what they do and do not say.
pub fn disk_lines(
    sizes: &[PathSize],
    database: Option<&DatabaseDisk>,
    clones: &[ServiceClone],
) -> Vec<String> {
    let mut lines = Vec::new();
    let mut shared = false;
    for size in sizes {
        let how = outcome_text(size.outcome);
        match &size.measure {
            Ok(m) if size.outcome.was_written() => {
                shared |= matches!(size.outcome, PayloadOutcome::Cloned | PayloadOutcome::Mixed);
                lines.push(format!("{}: {how}; {}", size.path, measure_text(m)));
            }
            Ok(_) => lines.push(format!("{}: {how}", size.path)),
            Err(error) => lines.push(format!("{}: {how}; cannot measure it ({error})", size.path)),
        }
    }
    if shared {
        lines.push(
            "Cloned files share blocks with their source until one of them changes. The figures \
             above count shared blocks in full, so they say what a plain copy would hold, not \
             what this workspace costs: whatever it changes is written anew, so the cost is \
             never zero."
                .to_string(),
        );
    }
    if let Some(db) = database {
        let size = db
            .size_bytes
            .map(|b| format!("{} logical", format_bytes(b)))
            .unwrap_or_else(|| "size unknown".into());
        let cow = match db.cow {
            Some((cow, source)) => format!("copy-on-write {}", cow_text(cow, source)),
            None => "copy-on-write unknown (the server is not reachable)".into(),
        };
        lines.push(format!("Database branch: {size}; {cow}."));
    }
    for clone in clones {
        lines.push(format!(
            "{} ({}): {}.",
            clone.service,
            clone.engine,
            clone.text()
        ));
    }
    lines
}

// ---- the view ----------------------------------------------------------------------

/// What a measurement was for: a different workspace, a recreated one or a
/// different payload makes the figures stale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MeasureKey {
    pub path: PathBuf,
    pub instance: Option<Uuid>,
    pub payload: Vec<(String, PayloadOutcome)>,
}

impl MeasureKey {
    /// What measuring this workspace's payload is about; `None` before it has
    /// a receipt.
    pub fn of(status: &WorkspaceStatus) -> Option<Self> {
        let receipt = status.receipt.as_ref()?;
        Some(Self {
            path: status.workspace.path.clone(),
            instance: Some(receipt.instance_id),
            payload: receipt
                .payload
                .iter()
                .map(|p| (p.path.clone(), p.outcome))
                .collect(),
        })
    }
}

/// Measure the paths a payload wrote (blocking: run it off the UI thread).
pub(super) fn measure_paths(key: &MeasureKey) -> Vec<PathSize> {
    key.payload
        .iter()
        .map(|(path, outcome)| PathSize {
            path: path.clone(),
            outcome: *outcome,
            measure: if outcome.was_written() {
                jobs::catch(|| cow::measure(&key.path.join(path)))
            } else {
                Ok(Measure::default())
            },
        })
        .collect()
}

pub struct WorkspaceDetailView {
    state: Entity<AppState>,
    reveal: bool,
    measured: Option<(MeasureKey, Vec<PathSize>)>,
    measuring: Option<MeasureKey>,
    _subscriptions: Vec<Subscription>,
}

impl WorkspaceDetailView {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![cx.observe_in(&state, window, |this, _, window, cx| {
            this.sync(window, cx);
            cx.notify();
        })];
        let mut this = Self {
            state,
            reveal: false,
            measured: None,
            measuring: None,
            _subscriptions: subscriptions,
        };
        this.sync(window, cx);
        this
    }

    fn key_of(&self, cx: &App) -> Option<MeasureKey> {
        let state = self.state.read(cx);
        let model = state.selection.current()?;
        MeasureKey::of(model.workspace(model.selected_workspace.as_ref()?)?)
    }

    /// Measure the payload paths of the selected workspace when they have not
    /// been measured yet.
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.key_of(cx) else {
            self.measured = None;
            return;
        };
        if self.measured.as_ref().is_some_and(|(k, _)| *k == key)
            || self.measuring.as_ref() == Some(&key)
        {
            return;
        }
        self.measuring = Some(key.clone());
        let work = key.clone();
        cx.spawn_in(window, async move |this, cx| {
            let sizes = cx
                .background_spawn(async move { measure_paths(&work) })
                .await;
            this.update(cx, |this, cx| {
                if this.measuring.as_ref() == Some(&key) {
                    this.measuring = None;
                }
                // Only the figures of what is selected now are kept.
                if this.key_of(cx).as_ref() == Some(&key) {
                    this.measured = Some((key, sizes));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn header(&self, detail: &Detail, busy: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let path = detail.path.clone();
        let open = |id: &'static str, icon: IconName, tip: &'static str| {
            Button::new(id).icon(icon).ghost().small().tooltip(tip)
        };
        let finder = {
            let path = path.clone();
            open("detail-open-finder", IconName::FolderOpen, "Open in Finder")
                .on_click(move |_, _, cx| cx.reveal_path(&path))
        };
        let terminal = {
            let (state, path) = (self.state.clone(), path.clone());
            open(
                "detail-open-terminal",
                IconName::SquareTerminal,
                "Open in terminal",
            )
            .on_click(move |_, window, cx| {
                state.update(cx, |s, cx| s.open_in_terminal(&path, window, cx))
            })
        };
        let editor = {
            let (state, path) = (self.state.clone(), path.clone());
            open("detail-open-editor", IconName::File, "Open in editor").on_click(
                move |_, window, cx| state.update(cx, |s, cx| s.open_in_editor(&path, window, cx)),
            )
        };
        let retry = {
            let state = self.state.clone();
            let name = detail.name.clone();
            let needs = matches!(
                detail.state,
                WorkspaceState::NeedsAttention
                    | WorkspaceState::InProgress
                    | WorkspaceState::NotSetUp
            );
            let button = Button::new("detail-retry-setup")
                .small()
                .icon(IconName::RotateCw)
                .label(if detail.state == WorkspaceState::NotSetUp {
                    "Set up"
                } else {
                    "Retry setup"
                })
                .tooltip("Run the setup phases that did not finish")
                .disabled(
                    busy || matches!(detail.state, WorkspaceState::Missing | WorkspaceState::Busy),
                );
            let button = if needs {
                button.primary()
            } else {
                button.ghost()
            };
            button.on_click(move |_, window, cx| {
                let workspace = state
                    .read(cx)
                    .selection
                    .current()
                    .and_then(|m| m.workspace(&name))
                    .map(|w| w.workspace.clone());
                let Some(workspace) = workspace else {
                    return;
                };
                let app = state.clone();
                let path = workspace.path.clone();
                dialogs::with_trust(
                    &state,
                    &path,
                    TrustPurpose::Setup,
                    window,
                    cx,
                    move |trust, window, cx| {
                        let workspace = workspace.clone();
                        app.update(cx, |s, cx| s.rerun_setup(workspace, trust, window, cx));
                    },
                );
            })
        };
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .min_w_0()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(detail.title()),
                            )
                            .child(ui::workspace_tag(detail.state))
                            .when(detail.is_default, |row| {
                                row.child(ui::pill(
                                    "default-workspace",
                                    "default",
                                    Tone::Neutral,
                                    "The main checkout. It cannot be removed.",
                                ))
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(retry)
                            .child(finder)
                            .child(terminal)
                            .child(editor),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .text_sm()
                    .text_color(ui::muted(cx))
                    .child(
                        Icon::new(Lucide::GitBranch)
                            .size_3p5()
                            .text_color(ui::muted(cx)),
                    )
                    .child(detail.branch.clone())
                    .child("·")
                    .child(div().min_w_0().truncate().child(shorten_home(&detail.path)))
                    .child(
                        Clipboard::new("detail-copy-path")
                            .value(detail.path.display().to_string())
                            .tooltip("Copy the path"),
                    ),
            )
    }

    fn timeline(&self, detail: &Detail, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<AnyElement> = detail
            .phases
            .iter()
            .map(|row| {
                let tone = match row.state {
                    PhaseState::Done => Tone::Success,
                    PhaseState::Failed => Tone::Danger,
                    PhaseState::Skipped if row.required_skip => Tone::Warning,
                    PhaseState::Skipped => Tone::Neutral,
                    PhaseState::Started => Tone::Info,
                    PhaseState::Pending => Tone::Neutral,
                };
                let color = ui::tone_color(tone, cx);
                let mark: AnyElement = match row.state {
                    PhaseState::Done => Icon::new(IconName::CircleCheck)
                        .size_4()
                        .text_color(color)
                        .into_any_element(),
                    PhaseState::Failed => Icon::new(IconName::CircleX)
                        .size_4()
                        .text_color(color)
                        .into_any_element(),
                    PhaseState::Skipped if row.required_skip => Icon::new(IconName::TriangleAlert)
                        .size_4()
                        .text_color(color)
                        .into_any_element(),
                    PhaseState::Skipped => Icon::new(IconName::Minus)
                        .size_4()
                        .text_color(color)
                        .into_any_element(),
                    PhaseState::Started => Spinner::new().small().color(color).into_any_element(),
                    PhaseState::Pending => div()
                        .size(px(10.))
                        .m(px(3.))
                        .rounded_full()
                        .border_1()
                        .border_color(cx.theme().border)
                        .into_any_element(),
                };
                let summary = match (row.state, &row.detail) {
                    (PhaseState::Skipped, Some(d)) if row.deliberate => {
                        format!("skipped on request ({d})")
                    }
                    (PhaseState::Skipped, Some(d)) => format!("skipped: {d}"),
                    (PhaseState::Skipped, None) => "skipped".into(),
                    (PhaseState::Pending, _) => "not started".into(),
                    (PhaseState::Started, _) => "running or interrupted".into(),
                    (_, Some(d)) => d.clone(),
                    (_, None) => String::new(),
                };
                hoverable(
                    h_flex()
                        .id(SharedString::from(format!("phase-{}", row.phase.as_str())))
                        .w_full()
                        .px_3()
                        .py_1p5()
                        .gap_3()
                        .items_center(),
                    cx,
                )
                .child(mark)
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().child(row.title))
                        .child(
                            div()
                                .text_xs()
                                .truncate()
                                .text_color(ui::muted(cx))
                                .child(summary),
                        ),
                )
                .children(row.duration.map(|d| {
                    mono(format_duration(d), cx)
                        .text_xs()
                        .text_color(ui::muted(cx))
                }))
                .test_support()
                .into_any_element()
            })
            .collect();
        let hint = detail
            .total
            .map(|total| SharedString::from(format!("{} in total", format_duration(total))));
        card(cx)
            .id("detail-timeline")
            .child(section_header(Lucide::ListChecks, "Setup", hint, None, cx))
            .children(rows)
            .when(detail.phases.is_empty(), |card| {
                card.child(empty_row(
                    "This workspace has not been set up by devflow yet.",
                    None,
                    cx,
                ))
            })
            .test_support()
    }

    fn connections(&self, detail: &Detail, cx: &mut Context<Self>) -> impl IntoElement {
        let reveal = self.reveal;
        let toggle = Button::new("detail-reveal")
            .ghost()
            .xsmall()
            .icon(if reveal {
                IconName::EyeOff
            } else {
                IconName::Eye
            })
            .tooltip(if reveal {
                "Hide passwords"
            } else {
                "Show passwords"
            })
            .on_click(cx.listener(|this, _, _, cx| {
                this.reveal = !this.reveal;
                cx.notify();
            }))
            .into_any_element();
        let mut body = v_flex().w_full();
        for conn in &detail.connections {
            let shown = |url: &str, masked: String| if reveal { url.to_string() } else { masked };
            let url_row = |id: String,
                           label: &'static str,
                           endpoint: &Option<String>,
                           url: &str,
                           masked: String| {
                v_flex()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(caption(label, cx))
                            .children(endpoint.clone().map(|e| chip(e, Tone::Neutral, cx))),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                mono(shown(url, masked), cx)
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs(),
                            )
                            .child(
                                Clipboard::new(SharedString::from(id))
                                    .value(url.to_string())
                                    .tooltip("Copy the connection URL"),
                            ),
                    )
            };
            let mut block = v_flex()
                .gap_2()
                .p_3()
                .border_b_1()
                .border_color(cx.theme().border)
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(conn.service.clone()),
                        )
                        .child(ui::pill(
                            "connection-engine",
                            conn.engine.clone(),
                            Tone::Neutral,
                            "The engine of this service.",
                        ))
                        .child(
                            div()
                                .text_xs()
                                .text_color(ui::muted(cx))
                                .child(conn.facts.clone()),
                        ),
                )
                .child(url_row(
                    format!("connection-copy-{}", conn.service),
                    "From this machine",
                    &conn.host_endpoint,
                    &conn.url,
                    conn.masked_url(),
                ));
            if let (Some(url), Some(masked)) = (&conn.container_url, conn.masked_container_url()) {
                block = block.child(url_row(
                    format!("connection-copy-container-{}", conn.service),
                    "From containers",
                    &conn.container_endpoint,
                    url,
                    masked,
                ));
            }
            block = block.child(
                div()
                    .text_xs()
                    .text_color(ui::muted(cx))
                    .child(format!("From {} · {}", conn.source, conn.ownership)),
            );
            if !conn.scratch.is_empty() {
                block = block.child(
                    v_flex()
                        .gap_1()
                        .child(caption("Scratch databases", cx))
                        .children(conn.scratch.iter().map(|scratch| {
                            let masked = scratch.url.as_deref().map(redact::text);
                            h_flex()
                                .id(SharedString::from(format!(
                                    "scratch-{}-{}",
                                    conn.service, scratch.name
                                )))
                                .gap_2()
                                .items_center()
                                .child(chip(redact::text(&scratch.name), Tone::Info, cx))
                                .child(
                                    mono(
                                        match (&scratch.url, &masked) {
                                            (Some(url), _) if reveal => url.clone(),
                                            (_, Some(masked)) => masked.clone(),
                                            _ => scratch.database.clone(),
                                        },
                                        cx,
                                    )
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs(),
                                )
                                .children(scratch.url.clone().map(|url| {
                                    Clipboard::new(SharedString::from(format!(
                                        "scratch-copy-{}-{}",
                                        conn.service, scratch.name
                                    )))
                                    .value(url)
                                    .tooltip("Copy the connection URL")
                                }))
                        })),
                );
            }
            body = body.child(block);
        }
        card(cx)
            .id("detail-connections")
            .child(section_header(
                Lucide::Database,
                "Connections",
                Some("passwords are masked until you ask".into()),
                (!detail.connections.is_empty()).then_some(toggle),
                cx,
            ))
            .child(body)
            .when(detail.connections.is_empty(), |card| {
                card.child(empty_row(
                    "This workspace has no database. Add a PostgreSQL service in the Setup wizard to give each workspace its own.",
                    None,
                    cx,
                ))
            })
            .test_support()
    }

    fn disk(&self, detail: &Detail, cx: &mut Context<Self>) -> impl IntoElement {
        let sizes: Vec<PathSize> = match &self.measured {
            Some((_, sizes)) => sizes.clone(),
            None => detail
                .payload
                .iter()
                .map(|(path, outcome)| PathSize {
                    path: path.clone(),
                    outcome: *outcome,
                    measure: Ok(Measure::default()),
                })
                .collect(),
        };
        let measuring = self.measured.is_none() && !detail.payload.is_empty();
        let lines = if measuring {
            Vec::new()
        } else {
            disk_lines(&sizes, detail.database.as_ref(), &detail.clones)
        };
        let speed = match detail.total {
            Some(total) => format!(
                "Set up in {} across the phases the receipt timed{}.",
                format_duration(total),
                detail
                    .set_up_at
                    .as_ref()
                    .map(|at| format!(" ({at})"))
                    .unwrap_or_default()
            ),
            None if detail.phases.is_empty() => "Not set up yet.".to_string(),
            None => "No phase durations were recorded for this workspace (it was set up before devflow kept them).".to_string(),
        };
        card(cx)
            .id("detail-disk")
            .child(section_header(
                Lucide::Gauge,
                "Disk and speed",
                None,
                None,
                cx,
            ))
            .child(
                v_flex()
                    .gap_1()
                    .p_3()
                    .text_sm()
                    .child(speed)
                    .when(
                        detail.payload.is_empty() && !detail.phases.is_empty(),
                        |col| {
                            col.child(
                                div()
                                    .text_color(ui::muted(cx))
                                    .child("No payload paths are recorded for this workspace."),
                            )
                        },
                    )
                    .when(measuring, |col| {
                        col.child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .text_color(ui::muted(cx))
                                .child(Spinner::new().small())
                                .child("Measuring the payload…"),
                        )
                    })
                    .children(lines.into_iter().enumerate().map(|(index, line)| {
                        div()
                            .id(SharedString::from(format!("disk-line-{index}")))
                            .text_color(ui::muted(cx))
                            .child(line)
                    })),
            )
            .test_support()
    }

    /// The processes of this workspace, from core's status of its pitchfork
    /// namespace: each daemon with its state, port and URL, the buttons
    /// gated exactly like the Daemons tab's, what autostart does and why it
    /// is blocked, and a start that core refused with its explanation.
    fn processes_card(&self, data: &ProcessesData, cx: &mut Context<Self>) -> impl IntoElement {
        let pitchfork = &data.pitchfork;
        let status = pitchfork.status.as_ref().filter(|s| s.configured);
        let running = status.map_or(0, |s| s.daemons.iter().filter(|d| d.is_active()).count());
        let unread = status.and_then(processes::unread_reason);
        let hint = status.map(|s| {
            SharedString::from(process_hint(
                running,
                s.daemons.len(),
                &s.namespace.value,
                unread.as_deref(),
            ))
        });
        let controls = status.zip(data.target.as_ref()).map(|(status, target)| {
            h_flex()
                .gap_1()
                .children(
                    processes::buttons("detail-daemons-", &self.state, status, target, data.busy)
                        .map(|button| button.small().into_any_element()),
                )
                .into_any_element()
        });
        let mut body = v_flex().w_full();
        match status {
            None if !pitchfork.loaded => {
                body = body.child(
                    h_flex()
                        .px_3()
                        .py_3()
                        .gap_2()
                        .items_center()
                        .text_sm()
                        .text_color(ui::muted(cx))
                        .child(Spinner::new().small())
                        .child("Looking for pitchfork…"),
                );
            }
            None => {
                let unusable = pitchfork.detection.as_ref().is_some_and(|d| !d.usable());
                body = body.child(if unusable {
                    empty_row(
                        "pitchfork is not installed or lacks what devflow uses, so this project's processes cannot run. The Daemons tab shows how to get it.",
                        Some(
                            Button::new("detail-open-daemons")
                                .label("Open Daemons")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.state.update(cx, |s, cx| s.go_to_tab(Tab::Daemons, cx))
                                })),
                        ),
                        cx,
                    )
                } else {
                    empty_row(
                        "This workspace declares no processes. A pitchfork.toml with daemons runs the project's servers per workspace, each with its own URL.",
                        Some(
                            Button::new("detail-detect-processes")
                                .label("Detect processes…")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.state.update(cx, |s, cx| s.request_setup_wizard(cx))
                                })),
                        ),
                        cx,
                    )
                });
            }
            Some(status) => {
                // The read failure is shown where the states would be: nothing
                // below it is an observation.
                if let Some(why) = &unread {
                    body = body.child(
                        div().px_3().pt_2().child(
                            ui::error_banner(
                                format!("pitchfork's state could not be read: {why}"),
                                cx,
                            )
                            .id("detail-processes-unread")
                            .test_support(),
                        ),
                    );
                }
                for (ix, daemon) in status.daemons.iter().enumerate() {
                    let url = daemon.url.clone();
                    let name = daemon.name.clone();
                    let target = data.target.clone();
                    let can_start = processes::row_can_start(daemon, data.busy) && target.is_some();
                    let state = self.state.clone();
                    body = body.child(
                        hoverable(
                            h_flex()
                                .id(("detail-daemon-row", ix))
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
                            h_flex()
                                .gap_2()
                                .items_center()
                                .w(px(150.))
                                .min_w_0()
                                .child(
                                    div()
                                        .truncate()
                                        .text_sm()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(processes::daemon_label(daemon)),
                                )
                                .when(!daemon.triggers.is_empty(), |row| {
                                    row.child(ui::pill(
                                        "detail-daemon-triggers",
                                        "on its own",
                                        Tone::Warning,
                                        format!(
                                            "pitchfork starts it without devflow: {}",
                                            daemon.triggers.join(", ")
                                        ),
                                    ))
                                }),
                        )
                        .child({
                            let (word, tone, help) =
                                processes::state_view(daemon, unread.as_deref());
                            ui::pill("detail-daemon-state", word, tone, help)
                        })
                        .child(
                            mono(
                                if daemon.ports.is_empty() {
                                    "—".to_string()
                                } else {
                                    daemon
                                        .ports
                                        .iter()
                                        .map(u16::to_string)
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                },
                                cx,
                            )
                            .text_xs()
                            .w(px(60.))
                            .text_color(ui::muted(cx)),
                        )
                        .child(
                            div().flex_1().min_w_0().child(match url {
                                Some(url) => {
                                    let target = url.clone();
                                    Button::new(("detail-daemon-url", ix))
                                        .ghost()
                                        .xsmall()
                                        .label(url)
                                        .tooltip("Open in the browser")
                                        .on_click(move |_, _, cx| cx.open_url(&target))
                                        .into_any_element()
                                }
                                None => div()
                                    .text_xs()
                                    .text_color(ui::muted(cx))
                                    .child("no URL")
                                    .into_any_element(),
                            }),
                        )
                        .child(
                            Button::new(("detail-daemon-start", ix))
                                .icon(IconName::Play)
                                .ghost()
                                .xsmall()
                                .tooltip(if daemon.startable {
                                    "Start this daemon"
                                } else {
                                    "Cannot be started now (see the notes below)"
                                })
                                .disabled(!can_start)
                                .on_click(move |_, window, cx| {
                                    if let Some(target) = target.clone() {
                                        processes::start_one(&state, target, &name, window, cx)
                                    }
                                }),
                        )
                        .test_support(),
                    );
                }
                let autostart_help = if status.autostart {
                    "devflow starts this workspace's daemons after setup and switch ([pitchfork] autostart in devflow.toml)."
                } else {
                    "Daemons start only when you ask; [pitchfork] autostart is off in devflow.toml."
                };
                body = body.child(
                    v_flex()
                        .gap_1()
                        .px_3()
                        .py_2()
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(caption("Autostart", cx))
                                .child(ui::pill(
                                    "detail-autostart",
                                    if status.autostart { "on" } else { "off" },
                                    if status.autostart && status.autostart_blocked.is_none() {
                                        Tone::Success
                                    } else if status.autostart {
                                        Tone::Warning
                                    } else {
                                        Tone::Neutral
                                    },
                                    autostart_help,
                                )),
                        )
                        .children(processes::notes(status).into_iter().map(|(id, text)| {
                            div()
                                .id(SharedString::from(format!("detail-{id}")))
                                .test_support()
                                .text_xs()
                                .text_color(ui::muted(cx))
                                .child(text)
                        })),
                );
                // A start core refused after its gates allowed it (the mise
                // delivery check), unless the gate already says as much.
                if let (Some(refusal), Some(target)) = (
                    data.refusal
                        .as_ref()
                        .filter(|r| status.can_start.reason.as_deref() != Some(r.text.as_str())),
                    data.target.as_ref(),
                ) {
                    body = body.child(div().px_3().pb_3().child(processes::refusal_banner(
                        "detail-start-refusal",
                        refusal,
                        target,
                        &self.state,
                        cx,
                    )));
                }
            }
        }
        card(cx)
            .id("detail-processes")
            .child(section_header(
                Lucide::Server,
                "Processes",
                hint,
                controls,
                cx,
            ))
            .child(body)
            .test_support()
    }

    fn problems(&self, detail: &Detail, cx: &mut Context<Self>) -> Vec<AnyElement> {
        detail
            .problems()
            .into_iter()
            .map(|row| {
                let failed = row.state == PhaseState::Failed;
                let text = match &row.detail {
                    Some(d) if failed => format!("{} failed: {d}", row.title),
                    Some(d) => format!("{} was skipped: {d}", row.title),
                    None if failed => format!("{} failed.", row.title),
                    None => format!("{} was skipped.", row.title),
                };
                // What core says fixes it, next to the reason: approving what
                // setup skipped for want of approval, else running setup again.
                let review = (!failed && matches!(row.phase, Phase::Env | Phase::Hooks))
                    .then(|| detail.remedy("trust"))
                    .flatten();
                let action = match (review, detail.remedy("setup")) {
                    (Some(trust), _) => Some(remedies::button_with_id(
                        &self.state,
                        format!("problem-review-{}", row.phase.as_str()).into(),
                        trust,
                        false,
                    )),
                    (None, Some(setup)) => Some(remedies::button_with_id(
                        &self.state,
                        format!("problem-fix-{}", row.phase.as_str()).into(),
                        setup,
                        false,
                    )),
                    (None, None) => None,
                };
                banner(
                    if failed { Tone::Danger } else { Tone::Warning },
                    if failed {
                        Lucide::CircleAlert
                    } else {
                        Lucide::TriangleAlert
                    },
                    text,
                    action,
                    cx,
                )
                .id(SharedString::from(format!(
                    "problem-{}",
                    row.phase.as_str()
                )))
                .test_support()
                .into_any_element()
            })
            .collect()
    }
}

impl Render for WorkspaceDetailView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (detail, busy, processes) = {
            let state = self.state.read(cx);
            let Some(model) = state.selection.current() else {
                return div().into_any_element();
            };
            let status = model
                .selected_workspace
                .as_ref()
                .and_then(|name| model.workspace(name));
            let busy = state.busy().is_some();
            let processes = status.map(|s| ProcessesData {
                pitchfork: model.pitchfork.clone(),
                target: model
                    .pitchfork
                    .status
                    .as_ref()
                    .map(|status| ProcessTarget::of(model, status)),
                refusal: model.refusals.get(&s.workspace.path).cloned(),
                busy,
            });
            (status.map(|s| Detail::of(model, s)), busy, processes)
        };
        let (Some(detail), Some(processes)) = (detail, processes) else {
            return div().into_any_element();
        };
        let problems = self.problems(&detail, cx);
        v_flex()
            .id("workspace-detail")
            .size_full()
            .overflow_y_scroll()
            .px_4()
            .py_3()
            .gap_3()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(self.header(&detail, busy, cx))
            .children(problems)
            .child(
                h_flex()
                    .w_full()
                    .gap_3()
                    .items_start()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_3()
                            .child(self.timeline(&detail, cx))
                            .child(self.disk(&detail, cx)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_3()
                            .child(self.connections(&detail, cx))
                            .child(self.processes_card(&processes, cx)),
                    ),
            )
            .test_support()
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `gpui_kit::*` also exports a `test` attribute; these are plain tests.
    use chrono::Utc;
    use devflow_core::{
        db::{BranchInfo, RecordStatus, ServerStatus},
        receipt::{DataBinding, ExtraBinding, PayloadRecord},
        worktree::Workspace,
    };
    use std::collections::BTreeMap;
    use std::prelude::v1::test;

    pub const URL: &str = "postgres://dfl_abc:s3cretpw@127.0.0.1:55432/df_feature"; // kingfisher:ignore

    pub fn binding() -> ServiceBinding {
        let mut binding = ServiceBinding::postgres(
            DataBinding {
                branch_id: Some(Uuid::new_v4()),
                database: "df_feature".into(),
                role: "dfl_abc".into(),
                url: URL.into(),
                source: "workspace:main".into(),
                owned: true,
            },
            None,
        );
        binding.extras = BTreeMap::from([(
            "test".to_string(),
            ExtraBinding {
                database: "df_x_1".into(),
            },
        )]);
        binding
    }

    #[test]
    fn endpoints_are_read_from_and_put_into_a_url_without_touching_the_login() {
        assert_eq!(host_endpoint(URL).as_deref(), Some("127.0.0.1:55432"));
        assert_eq!(
            url_with_endpoint(URL, "db.shop-1234abcd.devflow.orb.local:5432").as_deref(),
            Some("postgres://dfl_abc:s3cretpw@db.shop-1234abcd.devflow.orb.local:5432/df_feature") // kingfisher:ignore
        );
        assert_eq!(host_endpoint("postgres://h/db").as_deref(), Some("h"));
        assert_eq!(
            url_with_endpoint("postgres://h/db?sslmode=disable", "c:1").as_deref(),
            Some("postgres://c:1/db?sslmode=disable")
        );
        assert_eq!(host_endpoint("not a url"), None);
    }

    #[test]
    fn durations_read_at_a_glance() {
        assert_eq!(format_duration(Duration::from_millis(340)), "340 ms");
        assert_eq!(format_duration(Duration::from_millis(1234)), "1.2 s");
        assert_eq!(format_duration(Duration::from_secs(125)), "2m 05s");
    }

    #[test]
    fn a_connection_masks_the_password_and_keeps_the_real_url_for_copying() {
        let conn = connection("db", &binding(), Some("db.shop.devflow.orb.local:5432"));
        assert_eq!(conn.host_endpoint.as_deref(), Some("127.0.0.1:55432"));
        assert!(
            !conn.masked_url().contains("s3cretpw"),
            "{}",
            conn.masked_url()
        );
        assert!(conn.masked_url().contains("127.0.0.1:55432"));
        assert!(conn.url.contains("s3cretpw"));
        let container = conn.container_url.as_deref().unwrap();
        assert!(
            container.contains("db.shop.devflow.orb.local:5432") && container.contains("s3cretpw")
        );
        assert!(!conn.masked_container_url().unwrap().contains("s3cretpw"));
        assert_eq!(conn.scratch.len(), 1);
        assert!(conn.scratch[0].url.as_deref().unwrap().ends_with("/df_x_1"));
        assert_eq!(conn.ownership, "created by this workspace");
        let without = connection("db", &binding(), None);
        assert!(without.container_url.is_none() && without.container_endpoint.is_none());
    }

    fn fixture() -> (ProjectModel, WorkspaceStatus) {
        let entry = devflow_core::registry::Entry {
            id: Uuid::new_v4(),
            name: "shop".into(),
            root: PathBuf::from("/tmp/shop"),
        };
        let key = crate::model::RequestKey {
            project: entry.id,
            epoch: 1,
        };
        let mut model = ProjectModel::new(entry, key);
        let mut receipt = Receipt::new(Uuid::new_v4(), "feature", Some("main".into()));
        let binding = binding();
        let branch_id = binding.branch_id.unwrap();
        receipt.services.insert("db".into(), binding);
        for phase in Phase::ALL {
            receipt.set_phase(phase, PhaseState::Started, None);
            std::thread::sleep(Duration::from_millis(2));
            receipt.set_phase(phase, PhaseState::Done, Some(format!("{phase:?} done")));
        }
        receipt.skip_required(Phase::Hooks, "the hooks are not approved".into());
        receipt.payload = vec![
            PayloadRecord {
                path: "node_modules".into(),
                outcome: PayloadOutcome::Cloned,
            },
            PayloadRecord {
                path: ".env".into(),
                outcome: PayloadOutcome::Copied,
            },
        ];
        model.database.branches = vec![BranchInfo {
            id: branch_id,
            name: "feature".into(),
            db_name: "df_feature".into(),
            role_name: "dfl_abc".into(),
            source_kind: "branch".into(),
            source_id: None,
            instance_id: None,
            status: RecordStatus::Ready,
            cow: CowCapability::Verified,
            error: None,
            created_at: Utc::now(),
            size_bytes: Some(392 * 1024 * 1024),
            generation: 0,
            extras: Vec::new(),
            warnings: Vec::new(),
        }];
        model.database.status = Some(ServerStatus {
            mode: devflow_core::config::PostgresMode::Managed,
            running: true,
            reachable: true,
            endpoint: Some("127.0.0.1:55432".into()),
            container_endpoint: Some("db.shop.devflow.orb.local:5432".into()),
            server_version: Some("18.1".into()),
            file_copy_method: Some("clone".into()),
            cow: CowCapability::Verified,
            cow_source: Some(devflow_core::db::CowSource::Measured),
            problem: None,
        });
        let status = WorkspaceStatus {
            workspace: Workspace {
                name: "feature".into(),
                branch: Some("feature".into()),
                path: PathBuf::from("/tmp/shop.worktrees/feature"),
                git_dir: PathBuf::from("/tmp/shop/.git/worktrees/feature"),
                is_default: false,
                locked: false,
                missing: false,
                head: None,
                fingerprint: None,
            },
            status: Some(receipt.status()),
            receipt: Some(receipt),
            busy: false,
        };
        (model, status)
    }

    #[test]
    fn a_connection_draws_its_provenance_masked_and_keeps_the_real_url_for_copying() {
        // A legal workspace name that is also a credential.
        // kingfisher:ignore
        let name = "password=hunter2";
        let binding = ServiceBinding::container(
            devflow_core::config::Engine::Valkey,
            Uuid::new_v4(),
            "redis://:cache-password@127.0.0.1:6381/0".into(), // kingfisher:ignore
            None,
            format!("workspace:{name}"),
            devflow_core::receipt::CloneReport {
                method: None,
                captured_at: None,
                allocated_bytes: None,
                source_downtime_ms: None,
            },
            None,
        );
        let conn = connection("cache", &binding, None);
        let drawn = [
            conn.source.clone(),
            conn.facts.clone(),
            conn.masked_url(),
            conn.ownership.to_string(),
        ]
        .join("\n");
        assert!(drawn.contains("workspace:password=***"), "{drawn}");
        assert!(!drawn.contains("hunter2") && !drawn.contains("cache-password"));
        // Copying still gives the real URL.
        assert!(conn.url.contains("cache-password"));
    }

    #[test]
    fn the_detail_is_read_from_the_receipt() {
        let (model, status) = fixture();
        let detail = Detail::of(&model, &status);
        assert_eq!(detail.name, "feature");
        assert_eq!(detail.state, WorkspaceState::NeedsAttention);
        assert_eq!(detail.phases.len(), 5);
        let titles: Vec<_> = detail.phases.iter().map(|p| p.title).collect();
        assert_eq!(
            titles,
            ["Worktree", "Payload", "Services", "Environment", "Hooks"]
        );
        // Four phases have a recorded duration; the skipped one does too (it
        // was started first in this fixture).
        assert!(detail.phases[0].duration.is_some());
        assert!(detail.total.is_some());
        let problems = detail.problems();
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].phase, Phase::Hooks);
        assert_eq!(detail.connections.len(), 1);
        assert_eq!(
            detail.connections[0].container_endpoint.as_deref(),
            Some("db.shop.devflow.orb.local:5432")
        );
        assert_eq!(detail.payload.len(), 2);
        let db = detail.database.as_ref().unwrap();
        assert_eq!(db.size_bytes, Some(392 * 1024 * 1024));
    }

    #[test]
    fn disk_figures_say_what_they_measured_and_never_claim_a_bound_or_zero() {
        let sizes = vec![
            PathSize {
                path: "node_modules".into(),
                outcome: PayloadOutcome::Cloned,
                measure: Ok(Measure {
                    apparent_bytes: 412 * 1024 * 1024,
                    allocated_bytes: Some(420 * 1024 * 1024),
                    entries: 9000,
                    partial: false,
                }),
            },
            PathSize {
                path: ".env".into(),
                outcome: PayloadOutcome::Copied,
                measure: Ok(Measure {
                    apparent_bytes: 300,
                    allocated_bytes: Some(4096),
                    entries: 1,
                    partial: false,
                }),
            },
            PathSize {
                path: "dist".into(),
                outcome: PayloadOutcome::Missing,
                measure: Ok(Measure::default()),
            },
        ];
        let db = DatabaseDisk {
            size_bytes: Some(392 * 1024 * 1024),
            cow: Some((
                CowCapability::Verified,
                Some(devflow_core::db::CowSource::Measured),
            )),
        };
        let lines = disk_lines(&sizes, Some(&db), &[]);
        assert!(
            lines[0].starts_with(
                "node_modules: cloned (copy-on-write); 9000 items, 412.0 MB apparent \
                 (the sum of the file lengths), 420.0 MB allocated"
            ),
            "{lines:?}"
        );
        assert!(
            lines[0].contains("shared with the source are counted in full")
                && lines[0].contains("directory metadata is not included"),
            "{lines:?}"
        );
        assert!(
            lines[1].starts_with(
                ".env: copied (no copy-on-write between these locations); 1 item, 300 B apparent"
            ),
            "{lines:?}"
        );
        assert!(lines[1].contains("4.0 kB allocated"), "{lines:?}");
        assert_eq!(lines[2], "dist: not in the source");
        let all = lines.join("\n");
        assert!(all.contains("never zero"), "{all}");
        assert!(
            all.contains("392.0 MB logical") && all.contains("verified (measured)"),
            "{all}"
        );
        let all = all.to_lowercase();
        assert!(
            !all.contains("upper bound") && !all.contains("zero disk") && !all.contains("up to"),
            "no claim the figures cannot back: {all}"
        );
        // Copies only: no claim about sharing.
        let copies = disk_lines(&sizes[1..2], None, &[]);
        assert!(!copies.join(" ").contains("share blocks"), "{copies:?}");
    }

    #[test]
    fn a_one_byte_clone_is_not_called_a_bound_and_an_incomplete_read_is_marked() {
        // One byte of content takes a 4 kB block: the apparent size is no bound.
        let m = Measure {
            apparent_bytes: 1,
            allocated_bytes: Some(4096),
            entries: 1,
            partial: false,
        };
        let text = measure_text(&m);
        assert!(
            text.contains("1 B apparent") && text.contains("4.0 kB allocated"),
            "{text}"
        );
        assert!(!text.to_lowercase().contains("bound") && !text.contains("incomplete"));
        // A tree that could not be read in full says so.
        let text = measure_text(&Measure { partial: true, ..m });
        assert!(
            text.contains("incomplete") && text.contains("lower than the truth"),
            "{text}"
        );
        // Where the platform reports no blocks, none are claimed.
        let text = measure_text(&Measure {
            allocated_bytes: None,
            ..m
        });
        assert!(!text.contains("allocated"), "{text}");
        let lines = disk_lines(
            &[PathSize {
                path: "dist".into(),
                outcome: PayloadOutcome::Cloned,
                measure: Ok(Measure { partial: true, ..m }),
            }],
            None,
            &[],
        );
        assert!(lines[0].contains("incomplete"), "{lines:?}");
    }

    // kingfisher:ignore
    const REDIS_URL: &str = "redis://:cache-password@127.0.0.1:6381/0";

    fn container_binding() -> ServiceBinding {
        ServiceBinding::container(
            devflow_core::config::Engine::Valkey,
            Uuid::new_v4(),
            REDIS_URL.into(),
            Some("main.cache.shop-0b3c9d2e.devflow.orb.local:6379".into()),
            "workspace:main".into(),
            devflow_core::receipt::CloneReport {
                method: Some(devflow_core::services::catalog::CloneMethod::Reflink),
                captured_at: Some(Utc::now()),
                allocated_bytes: Some(12 * 1024 * 1024),
                source_downtime_ms: None,
            },
            None,
        )
    }

    #[test]
    fn a_container_binding_has_its_own_endpoints_masked_password_and_clone_report() {
        // The PostgreSQL server's OrbStack name is not the cache's.
        let conn = connection(
            "cache",
            &container_binding(),
            Some("db.shop.devflow.orb.local:5432"),
        );
        assert!(conn.container);
        assert_eq!(conn.host_endpoint.as_deref(), Some("127.0.0.1:6381"));
        assert_eq!(
            conn.container_endpoint.as_deref(),
            Some("main.cache.shop-0b3c9d2e.devflow.orb.local:6379")
        );
        let container_url = conn.container_url.as_deref().unwrap();
        // kingfisher:ignore
        assert!(container_url.starts_with("redis://:cache-password@main.cache.shop"));
        // Shown masked; the real URLs are kept for the copy buttons.
        assert_eq!(conn.masked_url(), "redis://:***@127.0.0.1:6381/0");
        assert!(
            !conn
                .masked_container_url()
                .unwrap()
                .contains("cache-password")
        );
        assert!(conn.url.contains("cache-password"));
        assert!(
            conn.facts.starts_with("cloned by reflink, captured "),
            "{}",
            conn.facts
        );
        assert!(conn.facts.contains("12.0 MB allocated"), "{}", conn.facts);
        assert!(!conn.facts.contains("database"), "no database or role here");
        assert_eq!(conn.engine, "valkey");
        assert_eq!(conn.ownership, "created by this workspace");
    }

    #[test]
    fn a_mysql_binding_is_a_container_binding_with_a_database_and_its_source_downtime() {
        // kingfisher:ignore
        let url = "mysql://df_main:mysql-password@127.0.0.1:3307/df_main";
        let mut binding = ServiceBinding::container(
            devflow_core::config::Engine::Mysql,
            Uuid::new_v4(),
            url.into(),
            Some("main.shop-data.shop-0b3c9d2e.devflow.orb.local:3306".into()),
            "workspace:main".into(),
            devflow_core::receipt::CloneReport {
                method: Some(devflow_core::services::catalog::CloneMethod::Reflink),
                captured_at: None,
                allocated_bytes: None,
                source_downtime_ms: Some(1800),
            },
            None,
        );
        binding.database = "df_main".into();
        let conn = connection(
            "shop-data",
            &binding,
            Some("db.shop.devflow.orb.local:5432"),
        );
        assert!(conn.container);
        assert_eq!(conn.engine, "mysql");
        assert_eq!(conn.host_endpoint.as_deref(), Some("127.0.0.1:3307"));
        assert_eq!(
            conn.container_endpoint.as_deref(),
            Some("main.shop-data.shop-0b3c9d2e.devflow.orb.local:3306")
        );
        assert_eq!(
            conn.masked_url(),
            "mysql://df_main:***@127.0.0.1:3307/df_main"
        );
        assert!(
            conn.facts
                .starts_with("database df_main · cloned by reflink"),
            "{}",
            conn.facts
        );
        assert!(
            conn.facts.contains("its source was stopped for 1.8 s"),
            "{}",
            conn.facts
        );
    }

    #[test]
    fn container_clones_are_reported_beside_the_database_branch_in_the_disk_section() {
        let (model, mut status) = fixture();
        let binding = container_binding();
        status
            .receipt
            .as_mut()
            .unwrap()
            .services
            .insert("cache".into(), binding);
        let detail = Detail::of(&model, &status);
        // PostgreSQL stays the database branch; the cache is its own line.
        let keys: Vec<_> = detail
            .connections
            .iter()
            .map(|c| c.service.as_str())
            .collect();
        assert_eq!(keys, ["cache", "db"]);
        assert_eq!(
            detail.database.as_ref().unwrap().size_bytes,
            Some(392 * 1024 * 1024)
        );
        assert_eq!(detail.clones.len(), 1);
        let lines = disk_lines(&[], detail.database.as_ref(), &detail.clones);
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("cache (valkey): cloned by reflink")),
            "{lines:?}"
        );
        // An empty root says nothing was cloned instead of claiming a method.
        let mut empty = ServiceClone::of("cache", &container_binding()).unwrap();
        empty.method = None;
        assert!(
            empty.text().starts_with("started empty"),
            "{}",
            empty.text()
        );
    }
}
