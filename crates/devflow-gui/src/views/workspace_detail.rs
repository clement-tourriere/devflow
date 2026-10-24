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
    widgets::{
        banner, banner_with, caption, card, chip, empty_row, hoverable, mono, section_header,
    },
};
use crate::{
    jobs,
    model::{
        PitchforkModel, ProcessTarget, ProjectModel, Tab, WorkspaceState, cow_text, format_bytes,
        format_when, shorten_home,
    },
    redaction::{RefusalView, shown},
    services::{FileOwner, sqlite_source_text},
    state::{AppState, TrustPurpose},
};
use devflow_core::{
    config::{CowCapability, Engine},
    cow::{self, Measure},
    receipt::{PayloadOutcome, Phase, PhaseState, Receipt, ServiceBinding},
    redact,
    remedy::Remedy,
    setup::{ServiceRun, WorkspaceStatus},
    worktree::Workspace,
};
use gpui_kit::{
    assets::IconName as Lucide,
    component::{
        ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _,
        button::{Button, ButtonVariants as _},
        clipboard::Clipboard,
        h_flex,
        spinner::Spinner,
        v_flex,
    },
    prelude::*,
    *,
};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
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
    /// A `sqlite` database: the absolute path of its file in this checkout.
    /// There is no host, port, role or password; `url` is the `sqlite:` URL of
    /// that path, which is not a secret.
    pub file: Option<PathBuf>,
    /// The line under the name: the database and role, how the branch was
    /// cloned, or the SQLite file and how it got its data.
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

/// Where a workspace's hooks, `[env]` recipes and copy list come from, as the
/// pane says it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecipesView {
    /// The sentence: whose file they are read from (or why none applies).
    pub source: String,
    /// The file they are read from, when there is one.
    pub file: Option<PathBuf>,
    /// The workspace has no usable recipes and none were chosen for it: it needs
    /// a choice (the remedy), or a valid `devflow.toml` committed in it.
    pub unavailable: bool,
    /// The default workspace's recipes were chosen for it.
    pub chosen: bool,
    /// Shared sections of its own file that are not applied: the project's
    /// services and the rest are the main checkout's.
    pub differing: Vec<String>,
}

impl RecipesView {
    pub fn of(report: &devflow_core::effective::RecipesReport) -> Self {
        use devflow_core::effective::RecipesSource;
        let mut sections: Vec<String> = Vec::new();
        for differing in &report.differing {
            if !sections.contains(&differing.section) {
                sections.push(differing.section.clone());
            }
        }
        Self {
            source: shown(&report.source.describe()),
            file: report.source.file().map(Path::to_path_buf),
            unavailable: report.source.is_unavailable(),
            chosen: matches!(report.source, RecipesSource::DefaultChosen { .. }),
            differing: sections,
        }
    }
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
    /// "now on branch X; its data still belongs to workspace Y": the checkout
    /// holds another branch than the workspace was created for.
    pub drift: Option<String>,
    /// `.devflow/env` is missing or unreadable on a workspace that is set up.
    pub env_file: Option<String>,
    /// How the container services stood when the workspace was last entered
    /// in this app (empty before that).
    pub services: Vec<ServiceNote>,
    /// The workspace's own port (`DEVFLOW_PORT`), once it has one.
    pub app_port: Option<u16>,
    /// The workspace is bound to container services (Valkey, Redis, MySQL):
    /// the header offers to stop and start them.
    pub container_services: bool,
    /// Where the workspace's hooks, `[env]` and copy list come from.
    pub recipes: Option<RecipesView>,
    /// When it was suspended, as the header says it.
    pub suspended: Option<String>,
    /// Kept out of the idle suspension.
    pub pinned: bool,
    /// Whether the project's focus points here.
    pub focus: FocusView,
}

/// Where the project's focus (the canonical loopback ports) points, as seen
/// from one workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FocusView {
    /// Nobody asked for it, or it could not be read.
    Off,
    /// It points at this workspace: the ports it forwards.
    Here(Vec<String>),
    /// It points at another workspace, by name.
    Elsewhere(String),
}

impl FocusView {
    pub fn of(focus: Option<&devflow_core::focus::Status>, path: &std::path::Path) -> Self {
        let Some(focus) = focus.filter(|f| f.running) else {
            return Self::Off;
        };
        match &focus.workspace {
            Some(workspace) if workspace.path == path => Self::Here(
                focus
                    .forwards
                    .iter()
                    .map(|f| format!("{} → {}", f.listen, f.to))
                    .collect(),
            ),
            Some(workspace) => Self::Elsewhere(shown(&workspace.name)),
            None => Self::Off,
        }
    }
}

/// One container service as the last entry of the workspace left it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceNote {
    pub service: String,
    pub state: ServiceRun,
    /// Why it did not start.
    pub reason: Option<String>,
    /// Starting it again, for a service that failed.
    pub remedy: Option<Remedy>,
}

impl ServiceNote {
    pub fn label(&self) -> &'static str {
        match self.state {
            ServiceRun::Running => "running",
            ServiceRun::Started => "started",
            ServiceRun::Failed => "not running",
        }
    }
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
        file: None,
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

/// A `sqlite` service's database in this workspace: a file, with no host,
/// port, role or password. Its path and URL are recomputed from where the
/// checkout is now (the receipt's URL is only what it was when the file was
/// made), and are not secrets.
fn sqlite_connection(key: &str, binding: &ServiceBinding, workspace: &Workspace) -> Connection {
    let file = workspace.path.join(&binding.database);
    let owner = FileOwner::of(workspace.is_default, binding.owned, true);
    let how = match binding.cloned.as_ref().and_then(|c| c.method) {
        Some(method) => format!("cloned by {method}"),
        None if binding.owned => "nothing was cloned".to_string(),
        None if workspace.is_default => "the project's own file, left alone".to_string(),
        None => "already in the workspace, left alone".to_string(),
    };
    Connection {
        service: key.to_string(),
        engine: binding.engine.to_string(),
        database: binding.database.clone(),
        role: String::new(),
        container: false,
        facts: redact::text(&format!("SQLite file {} · {how}", binding.database)),
        host_endpoint: None,
        container_endpoint: None,
        url: devflow_core::sqlite::url_of(&file),
        container_url: None,
        file: Some(file),
        // "workspace:NAME", "snapshot:NAME@vN": a name can look like a credential.
        source: redact::text(&sqlite_source_text(&binding.source)),
        ownership: owner.label(),
        scratch: Vec::new(),
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
                    .map(|(key, binding)| {
                        if binding.engine.is_file() {
                            sqlite_connection(key, binding, &status.workspace)
                        } else {
                            connection(key, binding, container)
                        }
                    })
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
                .find(|(_, b)| !b.orphaned && b.engine == Engine::Postgres)?;
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
            app_port: receipt.and_then(|r| r.app_port),
            container_services: receipt.is_some_and(|r| {
                r.services
                    .values()
                    .any(|binding| !binding.orphaned && binding.engine.is_container())
            }),
            recipes: status.recipes.as_ref().map(RecipesView::of),
            suspended: receipt.and_then(|r| r.suspended_at).map(format_when),
            pinned: receipt.is_some_and(|r| r.pinned),
            focus: FocusView::of(model.focus.as_ref(), &status.workspace.path),
            drift: status.drift.as_ref().map(|drift| shown(&drift.describe())),
            env_file: status
                .env_file
                .map(|problem| problem.describe().to_string()),
            services: model
                .entered
                .get(&status.workspace.path)
                .map(|entered| {
                    entered
                        .iter()
                        .map(|service| ServiceNote {
                            service: service.key.clone(),
                            state: service.state,
                            reason: service.reason.clone(),
                            remedy: (service.state == ServiceRun::Failed).then(|| {
                                Remedy::StartService {
                                    root: model.entry.root.clone(),
                                    key: service.key.clone(),
                                    branch: receipt
                                        .and_then(|r| r.services.get(&service.key))
                                        .and_then(|binding| binding.branch_id),
                                    workspace: Some(status.workspace.path.clone()),
                                }
                            }),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// The remedy of this kind the workspace has, if any.
    pub fn remedy(&self, kind: &str) -> Option<&Remedy> {
        self.remedies.iter().find(|r| r.kind() == kind)
    }

    /// Phases that went wrong or were left for someone to resolve. A workspace
    /// without recipes has one problem, which the recipes banner says (its
    /// payload, `[env]` and hooks phases all wait for the same choice).
    pub fn problems(&self) -> Vec<&PhaseRow> {
        let no_recipes = self.recipes.as_ref().is_some_and(|r| r.unavailable);
        self.phases
            .iter()
            .filter(|p| {
                !(no_recipes && matches!(p.phase, Phase::Payload | Phase::Env | Phase::Hooks))
            })
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
        MeasureKey::of(model.selected_status()?)
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
        // Stop (or start) the workspace's own containers; the data stays.
        let services = |start: bool| {
            // Found again by its checkout when clicked: its name is a label.
            let (state, at) = (self.state.clone(), detail.path.clone());
            Button::new(if start {
                "detail-start-services"
            } else {
                "detail-stop-services"
            })
            .icon(if start {
                IconName::Play
            } else {
                IconName::Square
            })
            .ghost()
            .small()
            .tooltip(if start {
                "Start the workspace's containers"
            } else {
                "Stop the workspace's containers; their data is kept"
            })
            .disabled(
                busy || matches!(detail.state, WorkspaceState::Missing | WorkspaceState::Busy),
            )
            .on_click(move |_, window, cx| {
                let found = state.read(cx).selection.current().and_then(|model| {
                    Some((
                        model.project_ref(),
                        model.workspace_at(&at)?.workspace.clone(),
                    ))
                });
                let Some((owner, workspace)) = found else {
                    return;
                };
                state.update(cx, |s, cx| {
                    s.workspace_services(owner, workspace, start, window, cx)
                });
            })
        };
        // Rest the workspace (or enter it again), keep it out of the idle
        // suspension, point the focus at it: each calls core through a job.
        let idle = busy
            || matches!(
                detail.state,
                WorkspaceState::Missing | WorkspaceState::Busy | WorkspaceState::NotSetUp
            );
        let found = |state: &Entity<AppState>, at: &PathBuf, cx: &App| {
            state.read(cx).selection.current().and_then(|model| {
                Some((
                    model.project_ref(),
                    model.workspace_at(at)?.workspace.clone(),
                ))
            })
        };
        let rest = {
            let (state, at) = (self.state.clone(), detail.path.clone());
            let suspended = detail.suspended.is_some();
            Button::new(if suspended {
                "detail-resume"
            } else {
                "detail-suspend"
            })
            .icon(if suspended {
                IconName::Play
            } else {
                IconName::Moon
            })
            .ghost()
            .small()
            .tooltip(if suspended {
                "Resume: enter the workspace again, which starts its containers"
            } else {
                "Suspend: stop the workspace's own containers and daemons; its data is kept"
            })
            .disabled(idle)
            .on_click(move |_, window, cx| {
                let Some((owner, workspace)) = found(&state, &at, cx) else {
                    return;
                };
                if suspended {
                    let (app, path) = (state.clone(), workspace.path.clone());
                    dialogs::with_trust(
                        &state,
                        &path,
                        TrustPurpose::Setup,
                        window,
                        cx,
                        move |trust, window, cx| {
                            let (owner, workspace) = (owner.clone(), workspace.clone());
                            app.update(cx, |s, cx| {
                                s.resume_workspace(owner, workspace, trust, window, cx)
                            });
                        },
                    );
                } else {
                    state.update(cx, |s, cx| {
                        s.suspend_workspace(owner, workspace, window, cx)
                    });
                }
            })
        };
        let pin = {
            let (state, at) = (self.state.clone(), detail.path.clone());
            let pinned = detail.pinned;
            Button::new("detail-pin")
                .icon(Lucide::Pin)
                .ghost()
                .small()
                .selected(pinned)
                .tooltip(if pinned {
                    "Pinned: the idle suspension leaves this workspace alone. Click to unpin"
                } else {
                    "Pin: keep this workspace out of the idle suspension"
                })
                .disabled(idle)
                .on_click(move |_, window, cx| {
                    let Some((owner, workspace)) = found(&state, &at, cx) else {
                        return;
                    };
                    state.update(cx, |s, cx| {
                        s.pin_workspace(owner, workspace, !pinned, window, cx)
                    });
                })
        };
        let focus = {
            let (state, at) = (self.state.clone(), detail.path.clone());
            let here = matches!(detail.focus, FocusView::Here(_));
            Button::new("detail-focus")
                .icon(Lucide::Crosshair)
                .ghost()
                .small()
                .selected(here)
                .tooltip(match &detail.focus {
                    FocusView::Here(_) => {
                        "Focused: the canonical ports reach this workspace. Click to stop the focus"
                            .to_string()
                    }
                    FocusView::Elsewhere(name) => {
                        format!("The focus is on {name}. Click to point it here")
                    }
                    FocusView::Off => {
                        "Focus: make the canonical ports (3000, …) reach this workspace".to_string()
                    }
                })
                .disabled(idle)
                .on_click(move |_, window, cx| {
                    let Some((owner, workspace)) = found(&state, &at, cx) else {
                        return;
                    };
                    state.update(cx, |s, cx| {
                        if here {
                            s.stop_focus(owner, window, cx)
                        } else {
                            s.focus_workspace(owner, workspace, window, cx)
                        }
                    });
                })
        };
        let retry = {
            let state = self.state.clone();
            // Found again by its checkout when clicked: its name is a label.
            let at = detail.path.clone();
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
                    .and_then(|m| m.workspace_at(&at))
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
                            })
                            .when_some(detail.suspended.clone(), |row, since| {
                                row.child(ui::pill(
                                    "detail-suspended",
                                    "suspended",
                                    Tone::Info,
                                    format!(
                                        "Suspended since {since}: its containers and daemons are stopped and its data is kept"
                                    ),
                                ))
                            })
                            .when(detail.pinned, |row| {
                                row.child(ui::pill(
                                    "detail-pinned",
                                    "pinned",
                                    Tone::Neutral,
                                    "Kept out of the idle suspension",
                                ))
                            })
                            .when_some(
                                match &detail.focus {
                                    FocusView::Here(ports) => Some(ports.join(", ")),
                                    _ => None,
                                },
                                |row, ports| {
                                    row.child(ui::pill(
                                        "detail-focused",
                                        "focus",
                                        Tone::Success,
                                        format!("The canonical ports reach this workspace: {ports}"),
                                    ))
                                },
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(retry)
                            .child(rest)
                            .child(pin)
                            .child(focus)
                            .when(detail.container_services, |row| {
                                row.child(services(false)).child(services(true))
                            })
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
                    .when_some(detail.app_port, |row, port| {
                        row.child("·").child(ui::pill(
                            "detail-app-port",
                            format!("port {port}"),
                            Tone::Neutral,
                            "This workspace's own port, for its dev server (DEVFLOW_PORT)",
                        ))
                    })
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
        // A SQLite file has no password: only the others are worth revealing.
        let only_files =
            !detail.connections.is_empty() && detail.connections.iter().all(|c| c.file.is_some());
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
                                    .tooltip(match label {
                                        "File" => "Copy the path of the file",
                                        "URL" => "Copy the URL",
                                        _ => "Copy the connection URL",
                                    }),
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
                        .children(
                            detail
                                .services
                                .iter()
                                .find(|note| note.service == conn.service)
                                .map(|note| {
                                    ui::pill(
                                        "connection-state",
                                        note.label(),
                                        match note.state {
                                            ServiceRun::Failed => Tone::Danger,
                                            ServiceRun::Started => Tone::Info,
                                            ServiceRun::Running => Tone::Success,
                                        },
                                        "How this service stood when the workspace was last \
                                         entered.",
                                    )
                                }),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(ui::muted(cx))
                                .child(conn.facts.clone()),
                        ),
                );
            block = match &conn.file {
                // A SQLite file: its path and its URL, neither a secret; no
                // host, port, role or password exist.
                Some(file) => {
                    let path = file.display().to_string();
                    block
                        .child(url_row(
                            format!("connection-copy-path-{}", conn.service),
                            "File",
                            &None,
                            &path,
                            redact::text(&shorten_home(file)),
                        ))
                        .child(url_row(
                            format!("connection-copy-{}", conn.service),
                            "URL",
                            &None,
                            &conn.url,
                            conn.masked_url(),
                        ))
                }
                None => block.child(url_row(
                    format!("connection-copy-{}", conn.service),
                    "From this machine",
                    &conn.host_endpoint,
                    &conn.url,
                    conn.masked_url(),
                )),
            };
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
                Some(if only_files {
                    "files have no password".into()
                } else {
                    "passwords are masked until you ask".into()
                }),
                (!detail.connections.is_empty() && !only_files).then_some(toggle),
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

    /// Where the workspace's hooks, `[env]` and copy list come from: its own
    /// branch's `devflow.toml`, the main checkout's, or none. Which file is
    /// shown, never what is in it.
    fn recipes_card(&self, detail: &Detail, cx: &mut Context<Self>) -> impl IntoElement {
        let recipes = detail.recipes.clone();
        card(cx)
            .id("detail-recipes")
            .child(section_header(
                Lucide::GitBranch,
                "Recipes",
                Some("hooks, [env] and the copy list".into()),
                None,
                cx,
            ))
            .child(
                v_flex()
                    .gap_1()
                    .p_3()
                    .text_sm()
                    .when_some(recipes, |col, recipes| {
                        col.child(
                            div()
                                .id("recipes-source")
                                .child(if recipes.unavailable {
                                    recipes.source.clone()
                                } else {
                                    format!("Read from {}.", recipes.source)
                                })
                                .test_support(),
                        )
                        .when(!recipes.differing.is_empty(), |col| {
                            col.child(
                                div()
                                    .id("recipes-differing")
                                    .text_color(ui::muted(cx))
                                    .child(format!(
                                        "{} in this workspace's own file {} not applied: the \
                                         project's services, data and integrations come from the \
                                         main checkout.",
                                        recipes.differing.join(", "),
                                        if recipes.differing.len() == 1 {
                                            "is"
                                        } else {
                                            "are"
                                        },
                                    )),
                            )
                        })
                    })
                    .when(detail.recipes.is_none(), |col| {
                        col.child(
                            div()
                                .text_color(ui::muted(cx))
                                .child("This checkout's directory is missing."),
                        )
                    }),
            )
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
                        "pitchfork is not installed or is not a release devflow supports, so this project's processes cannot run. The Daemons tab shows how to get it.",
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
        // The checkout holds another branch than the workspace was created
        // for: nothing is wrong with the data, but the name on the branch is
        // not the workspace's.
        // It only informs: there is nothing to run about it.
        let drift = detail.drift.iter().map(|text| {
            banner(Tone::Info, Lucide::GitBranch, text.clone(), None, cx)
                .id("problem-branch-drift")
                .test_support()
                .into_any_element()
        });
        // A derived file is gone: setup writes it again (and the workspace
        // stays ready; the status is the receipt's).
        let env_file = detail.env_file.iter().map(|text| {
            banner(
                Tone::Warning,
                Lucide::TriangleAlert,
                text.clone(),
                detail.remedy("setup").map(|remedy| {
                    remedies::button_with_id(
                        &self.state,
                        "problem-fix-env-file".into(),
                        remedy,
                        false,
                    )
                }),
                cx,
            )
            .id("problem-env-file")
            .test_support()
            .into_any_element()
        });
        // A service the last entry could not start, with the way to start it.
        let services: Vec<AnyElement> = detail
            .services
            .iter()
            .filter(|note| note.state == ServiceRun::Failed)
            .map(|note| {
                let text = match &note.reason {
                    Some(reason) => format!("{} did not start: {reason}", note.service),
                    None => format!("{} did not start.", note.service),
                };
                banner(
                    Tone::Danger,
                    Lucide::CircleAlert,
                    text,
                    note.remedy.as_ref().map(|remedy| {
                        remedies::button_with_id(
                            &self.state,
                            format!("problem-start-{}", note.service).into(),
                            remedy,
                            false,
                        )
                    }),
                    cx,
                )
                .id(SharedString::from(format!(
                    "problem-service-{}",
                    note.service
                )))
                .test_support()
                .into_any_element()
            })
            .collect();
        // No recipes of its own and none chosen: what core offers is the choice
        // of the default workspace's (a button), not "run setup again".
        let recipes: Vec<AnyElement> = detail
            .recipes
            .iter()
            .filter(|recipes| recipes.unavailable)
            .map(|recipes| {
                // Core's remedies, in its order: the choice, and when the
                // problem is a file nobody committed, that file.
                banner_with(
                    Tone::Warning,
                    Lucide::TriangleAlert,
                    recipes.source.clone(),
                    [
                        ("problem-use-default-recipes", "use_default_recipes"),
                        ("problem-open-recipes-file", "open_file"),
                    ]
                    .into_iter()
                    .filter_map(|(id, kind)| {
                        detail.remedy(kind).map(|remedy| {
                            remedies::button_with_id(&self.state, id.into(), remedy, false)
                        })
                    })
                    .collect(),
                    cx,
                )
                .id("problem-recipes")
                .test_support()
                .into_any_element()
            })
            .collect();
        let phases = detail.problems().into_iter().map(|row| {
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
        });
        drift
            .chain(env_file)
            .chain(recipes)
            .chain(phases)
            .chain(services)
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
            let status = model.selected_status();
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
                            .child(self.recipes_card(&detail, cx))
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
            clone_strategy: None,
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
            drift: None,
            env_file: None,
            recipes: None,
        };
        (model, status)
    }

    /// The fixture's workspace with a SQLite binding of service `app` (and the
    /// PostgreSQL one too, unless `only` says otherwise).
    fn with_sqlite(
        only: bool,
        owned: bool,
        is_default: bool,
        cloned: Option<devflow_core::services::catalog::CloneMethod>,
        source: &str,
    ) -> (ProjectModel, WorkspaceStatus) {
        use devflow_core::receipt::CloneReport;
        let (model, mut status) = fixture();
        status.workspace.is_default = is_default;
        let receipt = status.receipt.as_mut().unwrap();
        if only {
            receipt.services.clear();
        }
        receipt.services.insert(
            "app".into(),
            ServiceBinding::sqlite(
                Uuid::new_v4(),
                "db/app.sqlite3".into(),
                // Where the file was when it was made: not where it is now.
                "sqlite:///somewhere/else/db/app.sqlite3".into(),
                source.into(),
                owned,
                CloneReport {
                    method: cloned,
                    ..CloneReport::default()
                },
                None,
            ),
        );
        (model, status)
    }

    #[test]
    fn a_sqlite_binding_reads_as_a_file_with_no_server_role_or_password() {
        use devflow_core::services::catalog::CloneMethod;
        let (model, status) = with_sqlite(
            true,
            true,
            false,
            Some(CloneMethod::Reflink),
            "workspace:main",
        );
        let detail = Detail::of(&model, &status);
        let [conn] = &detail.connections[..] else {
            panic!("{:?}", detail.connections);
        };
        assert_eq!(conn.service, "app");
        assert_eq!(conn.engine, "sqlite");
        assert!(!conn.container && conn.role.is_empty());
        assert_eq!(conn.facts, "SQLite file db/app.sqlite3 · cloned by reflink");
        // No host, port or container endpoint exist for a file.
        assert!(conn.host_endpoint.is_none() && conn.container_endpoint.is_none());
        assert!(conn.container_url.is_none() && conn.scratch.is_empty());
        // The path and the URL follow the checkout's present location, not the
        // receipt's text, and are not secrets.
        let file = PathBuf::from("/tmp/shop.worktrees/feature/db/app.sqlite3");
        assert_eq!(conn.file.as_deref(), Some(file.as_path()));
        assert_eq!(
            conn.url,
            "sqlite:///tmp/shop.worktrees/feature/db/app.sqlite3"
        );
        assert_eq!(conn.masked_url(), conn.url);
        assert_eq!(conn.source, "workspace main");
        assert_eq!(conn.ownership, "created by this workspace");
        // Nothing here sizes a PostgreSQL branch or starts a container.
        assert!(detail.database.is_none());
        assert!(detail.clones.is_empty());
        assert!(!detail.container_services);
    }

    #[test]
    fn a_sqlite_file_says_whether_it_was_cloned_or_left_alone() {
        let facts = |owned, is_default, source| {
            let (model, status) = with_sqlite(true, owned, is_default, None, source);
            let detail = Detail::of(&model, &status);
            (
                detail.connections[0].facts.clone(),
                detail.connections[0].ownership,
            )
        };
        assert_eq!(
            facts(true, false, "empty"),
            (
                "SQLite file db/app.sqlite3 · nothing was cloned".to_string(),
                "created by this workspace"
            )
        );
        assert_eq!(
            facts(false, false, "existing"),
            (
                "SQLite file db/app.sqlite3 · already in the workspace, left alone".to_string(),
                "was already in the workspace"
            )
        );
        assert_eq!(
            facts(false, true, "existing"),
            (
                "SQLite file db/app.sqlite3 · the project's own file, left alone".to_string(),
                "the project's own file"
            )
        );
    }

    #[test]
    fn a_sqlite_service_does_not_stand_in_for_the_postgres_branch() {
        // `app` sorts before `db`: it used to be taken for "the database".
        let (model, status) = with_sqlite(false, true, false, None, "empty");
        let detail = Detail::of(&model, &status);
        let disk = detail
            .database
            .as_ref()
            .expect("the PostgreSQL branch is sized");
        assert_eq!(disk.size_bytes, Some(392 * 1024 * 1024));
        let engines: Vec<_> = detail
            .connections
            .iter()
            .map(|c| c.engine.as_str())
            .collect();
        assert_eq!(engines, ["sqlite", "postgres"]);
        // The line for the PostgreSQL branch is the only one that talks about
        // a database branch.
        let lines = disk_lines(&[], detail.database.as_ref(), &detail.clones);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("Database branch: 392.0 MB logical"));
    }

    #[test]
    fn a_source_that_looks_like_a_credential_is_drawn_masked_for_a_file_too() {
        // kingfisher:ignore
        let (model, status) = with_sqlite(true, true, false, None, "workspace:password=hunter2");
        let detail = Detail::of(&model, &status);
        let conn = &detail.connections[0];
        assert_eq!(conn.source, "workspace password=***");
        assert!(!format!("{conn:?}").contains("hunter2"));
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
    fn a_workspace_without_recipes_has_one_problem_and_the_choice_as_its_button() {
        use devflow_core::effective::{
            Differing, ProblemKind, RecipesProblem, RecipesReport, RecipesSource,
        };
        let (mut model, mut status) = fixture();
        let file = PathBuf::from("/tmp/shop.worktrees/feature/devflow.toml");
        let detail = Detail::of(&model, &status);
        assert!(
            detail.recipes.is_none(),
            "a listing without the report says nothing"
        );

        // Payload, `[env]` and hooks all wait for the same choice.
        let why = "no recipes of its own (devflow.toml does not exist)";
        for phase in [Phase::Payload, Phase::Env, Phase::Hooks] {
            status
                .receipt
                .as_mut()
                .unwrap()
                .skip_required(phase, why.into());
        }
        let report = |source| RecipesReport {
            source,
            digest: String::new(),
            shared: PathBuf::from("/tmp/shop/devflow.toml"),
            local: None,
            ignored_local: None,
            differing: Vec::new(),
            choice: None,
        };
        status.recipes = Some(report(RecipesSource::Unavailable {
            because: RecipesProblem {
                kind: ProblemKind::Missing,
                file: file.clone(),
                message: "devflow.toml does not exist".into(),
            },
        }));
        let detail = Detail::of(&model, &status);
        let recipes = detail.recipes.as_ref().unwrap();
        assert!(recipes.unavailable && !recipes.chosen && recipes.file.is_none());
        assert!(
            detail.problems().is_empty(),
            "one banner says it, not one per phase: {:?}",
            detail
                .problems()
                .iter()
                .map(|p| p.phase)
                .collect::<Vec<_>>()
        );
        assert!(
            !recipes.source.contains("`devflow"),
            "the app never shows a command line: {}",
            recipes.source
        );

        // The choice is core's remedy, drawn as a button.
        let project = crate::ops::tests_support::project_with_hooks(&[]);
        model.project = Some(std::sync::Arc::new(project));
        let remedies = status.remedies(model.project.as_ref().unwrap());
        assert_eq!(
            remedies.first(),
            Some(&Remedy::UseDefaultRecipes {
                workspace: status.workspace.path.clone()
            })
        );
        assert_eq!(
            Remedy::UseDefaultRecipes {
                workspace: PathBuf::new()
            }
            .kind(),
            "use_default_recipes"
        );

        // The choice made: the recipes come from the default workspace's file.
        status.recipes = Some(report(RecipesSource::DefaultChosen {
            file: PathBuf::from("/tmp/shop/devflow.toml"),
            because: RecipesProblem {
                kind: ProblemKind::Missing,
                file,
                message: "devflow.toml does not exist".into(),
            },
        }));
        let detail = Detail::of(&model, &status);
        let recipes = detail.recipes.as_ref().unwrap();
        assert!(recipes.chosen && !recipes.unavailable);
        assert_eq!(
            recipes.file.as_deref(),
            Some(Path::new("/tmp/shop/devflow.toml"))
        );

        // A branch file's shared sections are named, once each, never valued.
        let mut own = report(RecipesSource::Workspace {
            file: PathBuf::from("/tmp/shop.worktrees/feature/devflow.toml"),
        });
        own.differing = vec![
            Differing {
                file: PathBuf::from("/tmp/shop.worktrees/feature/devflow.toml"),
                section: "[services.db]".into(),
            },
            Differing {
                file: PathBuf::from("/tmp/shop.worktrees/feature/devflow.toml"),
                section: "[services.db]".into(),
            },
        ];
        status.recipes = Some(own);
        let detail = Detail::of(&model, &status);
        assert_eq!(detail.recipes.unwrap().differing, ["[services.db]"]);
    }

    #[test]
    fn a_moved_checkout_and_a_service_that_did_not_start_are_said_with_their_remedy() {
        use crate::jobs::EnteredService;
        use devflow_core::setup::BranchDrift;
        let (mut model, mut status) = fixture();
        let detail = Detail::of(&model, &status);
        assert!(detail.drift.is_none() && detail.services.is_empty() && detail.env_file.is_none());

        status.drift = Some(BranchDrift {
            workspace: "feature".into(),
            branch: Some("other".into()),
            head: None,
        });
        let path = status.workspace.path.clone();
        model.entered.insert(
            path,
            vec![
                EnteredService {
                    key: "db".into(),
                    engine: "postgres".into(),
                    state: ServiceRun::Running,
                    reason: None,
                },
                EnteredService {
                    key: "cache".into(),
                    engine: "valkey".into(),
                    state: ServiceRun::Failed,
                    reason: Some("port 6400 is taken".into()),
                },
            ],
        );
        let detail = Detail::of(&model, &status);
        assert_eq!(
            detail.drift.as_deref(),
            Some("now on branch other; its data still belongs to workspace feature")
        );
        // The note about the branch is plain words: nothing to type.
        assert!(!detail.drift.as_deref().unwrap().contains("devflow"));

        // A file derived from the receipt is gone: the pane says so (naming
        // the file, no command) and offers the remedy core gives.
        status.env_file = Some(devflow_core::setup::EnvFileProblem::Missing);
        let detail = Detail::of(&model, &status);
        let text = detail.env_file.as_deref().unwrap();
        assert!(
            text.contains(".devflow/env") && !text.contains("`devflow"),
            "{text}"
        );
        assert_eq!(detail.services.len(), 2);
        assert_eq!(detail.services[0].label(), "running");
        assert!(detail.services[0].remedy.is_none());
        let failed = &detail.services[1];
        assert_eq!(failed.label(), "not running");
        assert_eq!(failed.reason.as_deref(), Some("port 6400 is taken"));
        assert!(
            matches!(
                &failed.remedy,
                Some(Remedy::StartService { key, workspace: Some(w), .. })
                    if key == "cache" && *w == status.workspace.path
            ),
            "{:?}",
            failed.remedy
        );
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
