//! Blocking work that runs on GPUI's background executor: panic containment,
//! progress forwarding, and the read-only refresh of a project's models.
use crate::{
    model::{
        DatabaseModel, LogFile, LogsModel, PitchforkModel, PitchforkOffer, Section,
        WorkspaceSummary, WorkspaceTarget, workspace_summaries,
    },
    redaction::{self, RefusalView},
    services::{ServiceBackend, ServicesModel},
};
use anyhow::Result;
use devflow_core::{
    config::{ConfigDocument, PostgresMode},
    config_edit::migrate::{self, Migration},
    db::Server,
    error::{Busy, ConfigChanged, NotInitialized, Protected},
    health::{self, ProjectProblem},
    pitchfork::{self, Detection, InstallPlan, Pitchfork},
    processes,
    project::{self, InitOptions, Project},
    receipt::{Phase, PhaseState, Receipt},
    redact,
    registry::{self, Entry},
    setup::{self, Progress, WorkspaceStatus},
    trust::{self, TrustState},
    worktree::Workspace,
};
use std::{
    cell::Cell,
    collections::HashMap,
    io::Read as _,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
};
use uuid::Uuid;

thread_local! {
    static IN_CORE_CALL: Cell<bool> = const { Cell::new(false) };
}

/// Panics inside core calls are reported as one stderr line (the default hook
/// would print a multi-line report on every poll while core is unfinished).
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if IN_CORE_CALL.with(Cell::get) {
            let at = info.location().map(|l| l.to_string()).unwrap_or_default();
            eprintln!(
                "devflow-gui: core call panicked at {at}: {}",
                redact::text(&payload_message(info.payload()))
            );
        } else {
            default(info);
        }
    }));
}

fn payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

/// What a frontend may act on besides the error text. Taken from core's typed
/// errors (`devflow_core::error`), never from the message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    Other,
    NotInitialized,
    /// Another setup run holds the workspace.
    Busy,
    /// A removal protection refused the operation; `forceable` says whether
    /// `force` would override it.
    Protected {
        forceable: bool,
    },
    /// The configuration file changed since the editor loaded it.
    ConfigChanged,
}

impl FailureKind {
    pub fn of(error: &anyhow::Error) -> Self {
        if let Some(protected) = error.downcast_ref::<Protected>() {
            Self::Protected {
                forceable: protected.forceable,
            }
        } else if error.downcast_ref::<NotInitialized>().is_some() {
            Self::NotInitialized
        } else if error.downcast_ref::<Busy>().is_some() {
            Self::Busy
        } else if error.downcast_ref::<ConfigChanged>().is_some() {
            Self::ConfigChanged
        } else {
            Self::Other
        }
    }
}

/// A failed core call: credential-free display text plus its kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub message: String,
    pub kind: FailureKind,
    /// Core refused the call with a typed refusal (a start that may not
    /// happen): its kind, sentence and hint, for a view to keep.
    pub refusal: Option<RefusalView>,
}

impl Failure {
    pub fn other(message: impl AsRef<str>) -> Self {
        Self {
            message: redact::text(message.as_ref()),
            kind: FailureKind::Other,
            refusal: None,
        }
    }

    pub fn of(error: &anyhow::Error) -> Self {
        Self {
            message: redact::text(&format!("{error:#}")),
            kind: FailureKind::of(error),
            refusal: RefusalView::from_error(error),
        }
    }
}

/// Run a core call, keeping the error's kind. Panics become failures too: a
/// panic must never take the UI down. Messages are redacted here, so nothing
/// that reaches a view carries a credential.
pub fn catch_typed<T>(call: impl FnOnce() -> Result<T>) -> Result<T, Failure> {
    let previous = IN_CORE_CALL.with(|flag| flag.replace(true));
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(call));
    IN_CORE_CALL.with(|flag| flag.set(previous));
    match outcome {
        Ok(result) => result.map_err(|e| Failure::of(&e)),
        Err(payload) => Err(Failure::other(format!(
            "internal error in devflow-core: {}",
            payload_message(payload.as_ref())
        ))),
    }
}

/// [`catch_typed`] flattened to display text.
pub fn catch<T>(call: impl FnOnce() -> Result<T>) -> Section<T> {
    catch_typed(call).map_err(|failure| failure.message)
}

/// Like [`catch`] for infallible calls.
pub fn catch_value<T>(call: impl FnOnce() -> T) -> Section<T> {
    catch(|| Ok(call()))
}

// ---- progress from a running mutation --------------------------------------

/// What a finished mutation reports to the UI.
#[derive(Debug, Default, Clone)]
pub struct Done {
    pub summary: String,
    pub warnings: Vec<String>,
    pub needs_attention: bool,
    pub select_workspace: Option<String>,
    /// What this did to a workspace's processes, so the view can keep or
    /// clear a refusal.
    pub process: Option<ProcessNote>,
}

/// The outcome of asking core to start a workspace's processes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessNote {
    /// The workspace's directory.
    pub path: PathBuf,
    /// Why core would not start them (a typed refusal); `None` when it did.
    pub refusal: Option<RefusalView>,
}

impl Done {
    /// The summary and its warnings as one paragraph.
    pub fn line(&self) -> String {
        let mut text = self.summary.clone();
        for warning in &self.warnings {
            text.push_str(&format!(" ({warning})"));
        }
        text
    }

    /// The same report with credentials masked (core's warnings may quote URLs).
    pub fn redacted(mut self) -> Self {
        self.summary = redact::text(&self.summary);
        for warning in &mut self.warnings {
            *warning = redact::text(warning);
        }
        self
    }
}

pub enum Event {
    Phase {
        phase: Phase,
        state: PhaseState,
        detail: Option<String>,
    },
    Line(String),
    Finished(Result<Done, Failure>),
}

/// Events a running job may have queued before it waits for the UI.
pub const EVENT_QUEUE: usize = 4096;
/// Events the UI applies per tick, so a flood cannot monopolize its executor.
pub const MAX_EVENTS_PER_TICK: usize = 500;
/// A single output line is shortened to this many bytes before display.
pub const MAX_LINE_BYTES: usize = 8 * 1024;
/// Lines longer than this are cut before masking, only to bound the work.
const MAX_RAW_LINE_BYTES: usize = 64 * 1024;

/// Cut `text` to at most `max` bytes on a character boundary, marking the cut.
pub fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… (line truncated)", &text[..end])
}

/// One line of hook output as the UI may show it: masked, then shortened.
/// Masking comes first so a cut can never leave half a password behind.
pub fn sanitize_line(line: &str) -> String {
    let raw = if line.len() > MAX_RAW_LINE_BYTES {
        clip(line, MAX_RAW_LINE_BYTES)
    } else {
        line.to_string()
    };
    clip(&redact::text(&raw), MAX_LINE_BYTES)
}

/// Forwards core progress to the UI task over a bounded queue. Phase changes
/// and the final result are never dropped (the job waits for the UI to catch
/// up); output lines are skipped, and counted, when the UI is behind.
pub struct ChannelProgress {
    tx: mpsc::SyncSender<Event>,
    skipped: usize,
}

impl ChannelProgress {
    pub fn new(tx: mpsc::SyncSender<Event>) -> Self {
        Self { tx, skipped: 0 }
    }

    fn skipped_notice(&self) -> String {
        format!(
            "… {} output lines skipped (the display fell behind)",
            self.skipped
        )
    }

    /// Report lines that were skipped and not yet announced.
    pub fn flush(&mut self) {
        if self.skipped > 0 {
            let _ = self.tx.send(Event::Line(self.skipped_notice()));
            self.skipped = 0;
        }
    }
}

impl Progress for ChannelProgress {
    fn phase(&mut self, phase: Phase, state: PhaseState, detail: Option<&str>) {
        let _ = self.tx.send(Event::Phase {
            phase,
            state,
            detail: detail.map(sanitize_line),
        });
    }

    fn output(&mut self, line: &str) {
        if self.skipped > 0 && self.tx.try_send(Event::Line(self.skipped_notice())).is_ok() {
            self.skipped = 0;
        }
        if let Err(mpsc::TrySendError::Full(_)) = self.tx.try_send(Event::Line(sanitize_line(line)))
        {
            self.skipped += 1;
        }
    }
}

/// What one UI tick collected from a running job.
pub struct Tick {
    pub events: Vec<Event>,
    /// Set once the job's result arrived (or the job vanished without one).
    pub finished: Option<Result<Done, Failure>>,
}

/// Take at most `max` queued events. The result is reported only when it was
/// actually received: an empty queue means "still running", not "finished".
pub fn drain(rx: &mpsc::Receiver<Event>, max: usize) -> Tick {
    let mut tick = Tick {
        events: Vec::new(),
        finished: None,
    };
    for _ in 0..max {
        match rx.try_recv() {
            Ok(Event::Finished(result)) => {
                tick.finished = Some(result);
                break;
            }
            Ok(event) => tick.events.push(event),
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => {
                // The job was cancelled before it could report; release its lock.
                tick.finished = Some(Err(Failure::other("The operation ended unexpectedly.")));
                break;
            }
        }
    }
    tick
}

// ---- registry --------------------------------------------------------------

pub struct Entries {
    pub entries: Vec<Entry>,
    /// Project to select once loaded (from `--project`).
    pub select: Option<Uuid>,
    /// A folder shown without being registered (not a devflow project yet).
    pub adhoc: Option<PathBuf>,
    /// The registered projects devflow cannot open as they are, by id: what is
    /// wrong and what can be done (core's `health`, read from the disk).
    pub problems: HashMap<Uuid, ProjectProblem>,
}

/// Registered projects plus, optionally, one folder that is not (yet) a
/// devflow project. `preselect` is registered when it already is one.
pub fn load_entries(adhoc: Option<PathBuf>, preselect: Option<PathBuf>) -> Section<Entries> {
    let mut loaded = catch(|| {
        let mut entries = registry::list()?;
        let mut select = None;
        let mut adhoc = adhoc;
        if let Some(root) = preselect {
            let root = root.canonicalize().unwrap_or(root);
            if let Some(known) = entries.iter().find(|e| e.root == root) {
                select = Some(known.id);
            } else if let Ok(project) = Project::discover(&root) {
                registry::add(&project)?;
                entries = registry::list()?;
                select = Some(project.id());
            } else {
                select = Some(Uuid::nil());
                adhoc = Some(root);
            }
        }
        let problems = check_projects(&entries);
        Ok(Entries {
            entries,
            select,
            adhoc,
            problems,
        })
    })?;
    // Once the folder is registered it no longer needs a placeholder.
    if let Some(root) = loaded.adhoc.clone() {
        if loaded.entries.iter().any(|e| e.root == root) {
            loaded.adhoc = None;
        } else {
            add_adhoc(&mut loaded.entries, root);
        }
    }
    Ok(loaded)
}

/// Which of `entries` have a problem, by id. Reads the disk (folder, Git
/// repository, `devflow.toml`), changes nothing.
pub fn check_projects(entries: &[Entry]) -> HashMap<Uuid, ProjectProblem> {
    entries
        .iter()
        .filter(|entry| !entry.id.is_nil())
        .filter_map(|entry| Some((entry.id, health::check(entry).problem()?)))
        .collect()
}

/// Point a registered project at the folder it moved to (core checks that the
/// folder holds that project).
pub fn relocate_project(id: Uuid, new_root: PathBuf) -> Section<Entry> {
    catch(|| registry::relocate(id, &new_root))
}

/// Forget the project whose main checkout is `root`: the list entry only.
pub fn forget_project(root: PathBuf) -> Section<()> {
    catch(|| registry::remove(&root).map(|_| ()))
}

/// Forget every project whose folder no longer exists; the entries removed.
pub fn forget_missing_projects() -> Section<Vec<Entry>> {
    catch(registry::prune_missing)
}

/// What migrating the earlier build's configuration of the project at `root`
/// would change (nothing is written).
pub fn plan_migration(root: PathBuf) -> Section<Migration> {
    catch(|| migrate::plan(&root))
}

/// Write a planned migration. The files it replaces must still be what the
/// plan read.
pub fn apply_migration(migration: Migration) -> Section<()> {
    catch(|| migrate::apply(&migration).map(|_| ()))
}

/// A folder opened without being registered or initialized gets a placeholder
/// entry (nil id) so the UI can offer to initialize it.
pub fn add_adhoc(entries: &mut Vec<Entry>, root: PathBuf) {
    if entries.iter().any(|e| e.root == root) {
        return;
    }
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string());
    entries.push(Entry {
        id: Uuid::nil(),
        name,
        root,
    });
}

pub enum Probe {
    Added(Uuid),
    NotInitialized(PathBuf),
}

/// Register the project containing `path`, or report that it has no
/// `devflow.toml` so the UI can offer to initialize it.
pub fn probe_project(path: PathBuf) -> Section<Probe> {
    match catch_typed(|| Project::discover(&path)) {
        Ok(project) => {
            catch(|| registry::add(&project))?;
            Ok(Probe::Added(project.id()))
        }
        Err(failure) if failure.kind == FailureKind::NotInitialized => {
            Ok(Probe::NotInitialized(path))
        }
        Err(failure) => Err(failure.message),
    }
}

/// `devflow init` for the checkout containing `start`; also registers it.
pub fn init_project(start: PathBuf, managed_postgres: bool) -> Section<Uuid> {
    catch(|| {
        let options = InitOptions {
            name: None,
            postgres: managed_postgres.then_some(PostgresMode::Managed),
        };
        Ok(project::init(&start, options)?.id())
    })
}

// ---- refresh -----------------------------------------------------------------

/// Where the doctor's checks come from: core, or (tests) a fixed list.
#[derive(Clone, Debug, Default)]
pub enum DoctorSource {
    #[default]
    Core,
    #[cfg(test)]
    Fixed(Vec<devflow_core::doctor::Check>),
}

/// Which pitchfork core is asked: the one on `PATH`, or (tests) a fixed one.
#[derive(Clone, Debug, Default)]
pub enum PitchforkSource {
    #[default]
    OnPath,
    #[cfg(test)]
    Fixed {
        pitchfork: Option<Pitchfork>,
        /// How to install it; the machine's plan when `None`.
        install: Option<InstallPlan>,
    },
}

impl PitchforkSource {
    /// A fixed pitchfork for tests.
    #[cfg(test)]
    pub fn fixed(pitchfork: Option<Pitchfork>) -> Self {
        Self::Fixed {
            pitchfork,
            install: None,
        }
    }

    /// How to get a usable pitchfork.
    pub fn install_plan(&self) -> InstallPlan {
        match self {
            Self::OnPath => pitchfork::install_plan(),
            #[cfg(test)]
            Self::Fixed { install, .. } => install.clone().unwrap_or_else(pitchfork::install_plan),
        }
    }

    /// What running it finds. The machine's is cached for a moment; a fixed
    /// one (tests) is asked every time.
    pub fn detection(&self) -> Detection {
        match self {
            Self::OnPath => detection_on_path(),
            #[cfg(test)]
            Self::Fixed {
                pitchfork: Some(pitchfork),
                ..
            } => pitchfork.detect(),
            #[cfg(test)]
            Self::Fixed {
                pitchfork: None, ..
            } => missing(None),
        }
    }

    pub fn resolve(&self) -> Option<Pitchfork> {
        match self {
            Self::OnPath => Pitchfork::locate(),
            #[cfg(test)]
            Self::Fixed { pitchfork, .. } => pitchfork.clone(),
        }
    }
}

/// How much of the processes picture a refresh reads. Asking pitchfork runs
/// a few commands, so the tabs that do not show processes ask for less.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProcessWant {
    /// Not at all.
    #[default]
    No,
    /// Only if some workspace of the project has pitchfork configuration (the
    /// Workspaces tab: the overview and the detail's processes).
    WhenUsed,
    /// Always, so the tab can say pitchfork is missing (the Daemons tab).
    Always,
}

/// How the container services are read: through which backend, and whether
/// in detail (which asks Docker) or only their counts (the catalog file).
#[derive(Clone)]
pub struct ServicesRead {
    pub backend: Arc<dyn ServiceBackend>,
    pub full: bool,
}

#[cfg(test)]
impl ServicesRead {
    /// Only the counts, through core: for tests that call `refresh` directly.
    pub fn counts() -> Self {
        Self {
            backend: Arc::new(crate::services::CoreServices),
            full: false,
        }
    }
}

pub struct RefreshRequest {
    pub root: PathBuf,
    /// The id and name the project is listed under, when it is listed: a
    /// folder that holds another project is a problem, and locating a moved
    /// project needs the id.
    pub listed: Option<(Uuid, String)>,
    pub load_config: bool,
    pub workspace: Option<WorkspaceTarget>,
    pub processes: ProcessWant,
    pub want_logs: bool,
    pub selected_log: Option<PathBuf>,
    pub pitchfork: PitchforkSource,
    pub services: ServicesRead,
}

pub enum LoadError {
    NotInitialized,
    /// The project cannot be opened: what is wrong and what can be done.
    Problem(ProjectProblem),
}

pub struct Refreshed {
    pub project: Arc<Project>,
    pub workspaces: Section<Vec<WorkspaceStatus>>,
    pub database: DatabaseModel,
    pub services: ServicesModel,
    pub trust: Section<TrustState>,
    pub pitchfork: Option<PitchforkModel>,
    pub config: Option<Section<ConfigDocument>>,
    pub logs: Option<LogsModel>,
}

pub fn refresh(request: RefreshRequest) -> Result<Refreshed, LoadError> {
    let listed = request
        .listed
        .as_ref()
        .map(|(id, name)| (*id, name.as_str()));
    let project = Project::open(&request.root).map_err(|error| {
        if error.downcast_ref::<NotInitialized>().is_some() {
            LoadError::NotInitialized
        } else {
            let mut problem = health::explain(&request.root, listed, &error);
            problem.message = redact::text(&problem.message);
            LoadError::Problem(problem)
        }
    })?;
    // A folder that holds another project than the listed one is not this project.
    if listed.is_some_and(|(id, _)| !id.is_nil() && id != project.id())
        && let health::Health::Problem(mut problem) = health::check_root(&request.root, listed)
    {
        problem.message = redact::text(&problem.message);
        return Err(LoadError::Problem(problem));
    }
    let workspaces = catch(|| setup::list(&project));
    let database = load_database(&project);
    let services = request.services.backend.load(
        &project,
        workspaces.as_deref().unwrap_or_default(),
        request.services.full,
    );
    // What is trusted is judged for the workspace being looked at: a linked
    // worktree has its own pitchfork files. With none selected, the main checkout.
    let trust_dir = request
        .workspace
        .as_ref()
        .map_or_else(|| project.root.clone(), |target| target.path.clone());
    let trust = catch(|| trust::state_at(&project, &trust_dir));
    let pitchfork = load_processes(
        &request.pitchfork,
        &project,
        workspaces.as_deref().unwrap_or_default(),
        request
            .workspace
            .as_ref()
            .map(|target| target.path.as_path()),
        request.processes,
    );
    let config = request
        .load_config
        .then(|| catch(|| ConfigDocument::open(&request.root)));
    let logs = request
        .want_logs
        .then(|| load_logs(request.workspace.as_ref(), request.selected_log.as_deref()));
    Ok(Refreshed {
        project: Arc::new(project),
        workspaces,
        database,
        services,
        trust,
        pitchfork,
        config,
        logs,
    })
}

/// A light read of one project's workspaces, for the sidebar tree of a project
/// that is not the selected one (the selected project's refresh covers its own).
pub fn summarize(root: &Path) -> Section<Vec<WorkspaceSummary>> {
    catch(|| {
        let project = Project::open(root)?;
        Ok(workspace_summaries(&setup::list(&project)?))
    })
}

/// Never fails: problems are recorded in the model so the tab can show them.
fn load_database(project: &Project) -> DatabaseModel {
    let mut model = DatabaseModel {
        configured: project.config.postgres().is_some(),
        ..Default::default()
    };
    let server = match catch(|| Server::for_project(project)) {
        Ok(Some(server)) => server,
        Ok(None) => return model,
        Err(e) => {
            model.error = Some(e);
            return model;
        }
    };
    let status = match catch_value(|| server.status()) {
        Ok(mut status) => {
            // An external server's endpoint or problem text may quote its URL.
            status.endpoint = status.endpoint.map(|e| redact::text(&e));
            status.problem = status.problem.map(|p| redact::text(&p));
            status
        }
        Err(e) => {
            model.error = Some(e);
            return model;
        }
    };
    if status.reachable {
        match catch(|| server.list_branches()) {
            Ok(branches) => model.branches = branches,
            Err(e) => model.error = Some(e),
        }
        match catch(|| server.list_snapshots()) {
            Ok(snapshots) => model.snapshots = snapshots,
            Err(e) => {
                model.error.get_or_insert(e);
            }
        }
    }
    model.status = Some(status);
    redaction::database(&mut model);
    model
}

/// How long a feature check of the installed pitchfork is trusted: it runs
/// several commands, and the answer only changes when somebody installs.
const DETECTION_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// A detection that found nothing, with the reason when there is one.
fn missing(problem: Option<String>) -> Detection {
    Detection {
        path: None,
        installed: false,
        version: None,
        missing_features: Vec::new(),
        problem,
    }
}

/// Moves when something may have changed what is installed.
static DETECTION_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The machine's pitchfork was installed or changed just now: look again.
pub fn forget_detection() {
    DETECTION_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// The machine's pitchfork, checked at most every [`DETECTION_TTL`] and again
/// after [`forget_detection`].
fn detection_on_path() -> Detection {
    use std::sync::{Mutex, atomic::Ordering};
    use std::time::Instant;
    static CACHE: Mutex<Option<(Instant, u64, Detection)>> = Mutex::new(None);
    let epoch = DETECTION_EPOCH.load(Ordering::Relaxed);
    if let Ok(cache) = CACHE.lock()
        && let Some((at, when, found)) = cache.as_ref()
        && *when == epoch
        && at.elapsed() < DETECTION_TTL
    {
        return found.clone();
    }
    let found = pitchfork::detect();
    if let Ok(mut cache) = CACHE.lock() {
        *cache = Some((Instant::now(), epoch, found.clone()));
    }
    found
}

/// How long installing may take (mise may download and build).
const INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Run the install `plan` the person was shown (core's
/// [`pitchfork::run_install`], which runs that command and nothing else), then
/// look at pitchfork again. The detection after it, which may still say
/// pitchfork is not there (the app's `PATH` may not include mise's shims).
pub fn install_pitchfork(source: &PitchforkSource, plan: &InstallPlan) -> Result<Detection> {
    let ran = pitchfork::run_install(plan, INSTALL_TIMEOUT)?;
    forget_detection();
    anyhow::ensure!(
        !ran.timed_out,
        "installing pitchfork took longer than {} and was stopped",
        devflow_core::text::counted(INSTALL_TIMEOUT.as_secs() / 60, "minute")
    );
    anyhow::ensure!(
        ran.ok,
        "installing pitchfork failed: {}",
        ran.message
            .as_deref()
            .unwrap_or("the command exited with an error")
    );
    let mut detection = source.detection();
    redaction::detection(&mut detection);
    Ok(detection)
}

/// What the wizard shows about pitchfork for the project at `dir`: never
/// fails, and asks pitchfork (not the supervisor) only if it is usable.
pub fn load_offer(source: &PitchforkSource, dir: &Path) -> PitchforkOffer {
    let mut detection =
        catch_value(|| source.detection()).unwrap_or_else(|message| missing(Some(message)));
    redaction::detection(&mut detection);
    let proxy = if detection.usable() {
        source
            .resolve()
            .and_then(|found| catch_value(|| found.proxy_status(dir)).ok()?.ok())
            .map(|mut proxy| {
                redaction::proxy(&mut proxy);
                proxy
            })
    } else {
        None
    };
    PitchforkOffer {
        install: (!detection.usable()).then(|| source.install_plan()),
        detection: Some(detection),
        proxy,
    }
}

/// Never fails: problems are recorded in the model so the tab can show them.
/// `None` when the refresh did not ask, or nothing in the project uses
/// pitchfork and the caller only wanted it if something did.
fn load_processes(
    source: &PitchforkSource,
    project: &Project,
    workspaces: &[WorkspaceStatus],
    selected: Option<&Path>,
    want: ProcessWant,
) -> Option<PitchforkModel> {
    let present: Vec<Workspace> = workspaces
        .iter()
        .map(|w| &w.workspace)
        .filter(|w| !w.missing)
        .cloned()
        .collect();
    match want {
        ProcessWant::No => return None,
        ProcessWant::WhenUsed => {
            if !present.iter().any(|w| processes::uses_pitchfork(&w.path)) {
                // Nothing to supervise: say so without asking pitchfork.
                return Some(PitchforkModel {
                    loaded: true,
                    ..Default::default()
                });
            }
        }
        ProcessWant::Always => {}
    }
    let mut detection =
        catch_value(|| source.detection()).unwrap_or_else(|message| missing(Some(message)));
    redaction::detection(&mut detection);
    let mut model = PitchforkModel {
        loaded: true,
        install: (!detection.usable()).then(|| source.install_plan()),
        ..Default::default()
    };
    if detection.installed {
        let located = source.resolve();
        match catch_value(|| processes::status_all(located.as_ref(), project, &present)) {
            Ok(mut all) => {
                for status in &mut all {
                    redaction::process_status(status);
                }
                model.status = selected
                    .and_then(|path| all.iter().find(|status| status.path == path))
                    .cloned();
                model.all = all;
            }
            Err(e) => model.error = Some(e),
        }
    }
    model.detection = Some(detection);
    Some(model)
}

// ---- logs --------------------------------------------------------------------

/// Setup logs of one workspace, newest first. A missing directory means none yet.
pub fn list_logs(git_dir: &Path) -> Result<Vec<LogFile>> {
    let dir = Receipt::logs_dir(git_dir);
    let reader = match std::fs::read_dir(&dir) {
        Ok(reader) => reader,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut files: Vec<LogFile> = reader
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_file())
        .map(|entry| LogFile {
            name: entry.file_name().to_string_lossy().into_owned(),
            modified: entry.metadata().and_then(|m| m.modified()).ok(),
            path: entry.path(),
        })
        .collect();
    files.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| b.name.cmp(&a.name))
    });
    Ok(files)
}

/// At most the last 512 KiB: setup logs can hold long install output.
pub fn read_log(path: &Path) -> Result<String> {
    const MAX: u64 = 512 * 1024;
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut prefix = "";
    if len > MAX {
        std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(len - MAX))?;
        prefix = "… (earlier output omitted)\n";
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(redact::text(&format!(
        "{prefix}{}",
        String::from_utf8_lossy(&bytes)
    )))
}

fn load_logs(workspace: Option<&WorkspaceTarget>, selected: Option<&Path>) -> LogsModel {
    let mut model = LogsModel::default();
    let Some(ws) = workspace else {
        return model;
    };
    match catch(|| list_logs(&ws.git_dir)) {
        Ok(files) => model.files = files,
        Err(e) => {
            model.error = Some(e);
            return model;
        }
    }
    model.selected = selected
        .and_then(|p| model.files.iter().find(|f| f.path == p))
        .or_else(|| model.files.first())
        .map(|f| f.path.clone());
    if let Some(path) = &model.selected {
        model.content = Some(catch(|| read_log(path)));
    }
    model
}

/// Tests that point `DEVFLOW_CONFIG_DIR` at a scratch registry take this lock:
/// the variable is process-wide and tests run in parallel.
#[cfg(test)]
pub(crate) static REGISTRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Scratch `DEVFLOW_CONFIG_DIR` and `DEVFLOW_DATA_DIR` for one test, held
/// under [`REGISTRY_LOCK`] until it is dropped. Every test that reaches core's
/// registry, servers or trust file starts with this: core refuses the real
/// directories in a test binary, and a test must never write to the user's
/// project list (an earlier one left its scratch projects there).
#[cfg(test)]
pub(crate) struct ScratchDirs {
    _lock: std::sync::MutexGuard<'static, ()>,
    _config: tempfile::TempDir,
    _data: tempfile::TempDir,
}

#[cfg(test)]
impl ScratchDirs {
    pub(crate) fn new() -> Self {
        let lock = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let config = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        // SAFETY: the lock serializes every test that changes them.
        unsafe {
            std::env::set_var("DEVFLOW_CONFIG_DIR", config.path());
            std::env::set_var("DEVFLOW_DATA_DIR", data.path());
        }
        Self {
            _lock: lock,
            _config: config,
            _data: data,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catch_turns_panics_and_errors_into_text() {
        let panicked: Section<()> = catch(|| todo!());
        assert!(
            panicked
                .unwrap_err()
                .contains("internal error in devflow-core")
        );

        let failed: Section<()> = catch(|| Err(anyhow::anyhow!("inner").context("outer")));
        assert_eq!(failed.unwrap_err(), "outer: inner");

        assert_eq!(catch_value(|| 7), Ok(7));
        // The flag is restored, also after a nested panic.
        let nested: Section<i32> = catch(|| Ok(catch_value(|| 1).unwrap() + 1));
        assert_eq!(nested, Ok(2));
    }

    #[test]
    fn failure_kind_follows_typed_errors_not_message_text() {
        use devflow_core::error::{Busy, NotInitialized, Protected};
        let protected = |forceable| {
            anyhow::Error::new(Protected {
                workspace: "w".into(),
                reason: "'w' has uncommitted changes".into(),
                forceable,
            })
            .context("while removing")
        };
        assert_eq!(
            FailureKind::of(&protected(true)),
            FailureKind::Protected { forceable: true }
        );
        assert_eq!(
            FailureKind::of(&protected(false)),
            FailureKind::Protected { forceable: false }
        );
        assert_eq!(
            FailureKind::of(&anyhow::Error::new(NotInitialized { root: "/x".into() })),
            FailureKind::NotInitialized
        );
        assert_eq!(
            FailureKind::of(&anyhow::Error::new(Busy::setup("w"))),
            FailureKind::Busy
        );
        // Wording alone never makes a failure overridable or "uninitialized".
        for text in [
            "something failed, or pass --force",
            "no devflow.toml in /x; run `devflow init` first",
        ] {
            assert_eq!(FailureKind::of(&anyhow::anyhow!(text)), FailureKind::Other);
        }
        let failed = catch_typed::<()>(|| Err(protected(true)));
        assert_eq!(
            failed.unwrap_err().kind,
            FailureKind::Protected { forceable: true }
        );
    }

    #[test]
    fn errors_and_panics_are_masked_before_any_view_sees_them() {
        let failed: Section<()> = catch(|| {
            Err(anyhow::anyhow!(
                "connect failed: postgres://app:hunter2@db:5432/x" // kingfisher:ignore
            ))
        });
        let message = failed.unwrap_err();
        assert!(!message.contains("hunter2"), "{message}");
        assert!(message.contains("app:***@db"), "{message}");

        let panicked: Section<()> = catch(|| panic!("bad url postgres://u:topsecret@h/d")); // kingfisher:ignore
        assert!(!panicked.unwrap_err().contains("topsecret"));

        let done = Done {
            summary: "attached postgres://u:pw1234@h/d".into(), // kingfisher:ignore
            warnings: vec!["PGPASSWORD=pw5678 psql".into()],
            ..Default::default()
        }
        .redacted();
        assert!(!done.summary.contains("pw1234"));
        assert!(!done.warnings[0].contains("pw5678"));
    }

    #[test]
    fn output_lines_are_masked_then_shortened_on_a_char_boundary() {
        assert_eq!(
            sanitize_line("psql postgres://u:pw@h/db"), // kingfisher:ignore
            "psql postgres://u:***@h/db"
        );
        let long = format!("postgres://u:hunter2@h/db {}", "€".repeat(20_000)); // kingfisher:ignore
        let line = sanitize_line(&long);
        assert!(!line.contains("hunter2"));
        assert!(line.len() <= MAX_LINE_BYTES + 32, "{} bytes", line.len());
        assert!(line.ends_with("… (line truncated)"));
        // A cut can never leave the front of a password behind.
        let boundary = format!(
            "{}postgres://u:abcdefgh@h/d", // kingfisher:ignore
            "x".repeat(MAX_LINE_BYTES - 14)
        );
        assert!(!sanitize_line(&boundary).contains("abcd"));
        assert_eq!(clip("short", 100), "short");
    }

    #[test]
    fn a_flood_of_output_neither_blocks_the_job_nor_loses_phases_or_the_result() {
        let (tx, rx) = mpsc::sync_channel(8);
        let mut progress = ChannelProgress::new(tx.clone());
        // No one is draining: lines past the queue are skipped, not waited for.
        for i in 0..1000 {
            progress.output(&format!("line {i}"));
        }
        let first = drain(&rx, 100);
        assert_eq!(first.events.len(), 8);
        assert!(first.finished.is_none());

        // Phases are never dropped, and the skipped lines are announced.
        progress.phase(Phase::Services, PhaseState::Done, Some("ready"));
        progress.flush();
        tx.send(Event::Finished(Ok(Done::default()))).unwrap();
        let rest = drain(&rx, 100);
        assert!(rest.finished.is_some());
        assert!(rest.events.iter().any(|e| matches!(
            e,
            Event::Phase {
                phase: Phase::Services,
                ..
            }
        )));
        let notice = rest.events.iter().find_map(|e| match e {
            Event::Line(l) if l.contains("output lines skipped") => Some(l.clone()),
            _ => None,
        });
        assert!(
            notice.as_ref().is_some_and(|n| n.contains("992")),
            "{notice:?}"
        );
    }

    #[test]
    fn a_tick_applies_a_bounded_number_of_events_and_never_drops_the_result() {
        let (tx, rx) = mpsc::sync_channel(EVENT_QUEUE);
        for i in 0..1200 {
            tx.send(Event::Line(format!("{i}"))).unwrap();
        }
        tx.send(Event::Finished(Ok(Done::default()))).unwrap();

        let mut seen = 0;
        let mut ticks = 0;
        loop {
            ticks += 1;
            let tick = drain(&rx, MAX_EVENTS_PER_TICK);
            assert!(tick.events.len() <= MAX_EVENTS_PER_TICK);
            seen += tick.events.len();
            if tick.finished.is_some() {
                break;
            }
            assert!(ticks < 10, "the result must arrive");
        }
        assert_eq!(seen, 1200, "no output was lost on the way");
        assert_eq!(ticks, 3, "1200 events at 500 per tick");
    }

    #[test]
    fn an_empty_queue_means_running_and_a_vanished_job_is_released() {
        let (tx, rx) = mpsc::sync_channel(4);
        let idle = drain(&rx, 10);
        assert!(idle.events.is_empty());
        assert!(idle.finished.is_none(), "no result yet is not a result");

        tx.send(Event::Line("still going".into())).unwrap();
        drop(tx);
        let tick = drain(&rx, 10);
        assert_eq!(tick.events.len(), 1);
        let failure = tick
            .finished
            .expect("the lock must be released")
            .unwrap_err();
        assert!(failure.message.contains("ended unexpectedly"));
    }

    #[test]
    fn displayed_logs_are_masked() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("hooks.log");
        std::fs::write(
            &log,
            "$ psql postgres://u:hunter2@h/db\nPGPASSWORD=hunter2\n", // kingfisher:ignore
        )
        .unwrap();
        let text = read_log(&log).unwrap();
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("u:***@h"));
    }

    #[test]
    fn logs_are_listed_newest_first_and_tail_limited() {
        let dir = tempfile::tempdir().unwrap();
        let logs = Receipt::logs_dir(dir.path());
        assert!(list_logs(dir.path()).unwrap().is_empty());
        std::fs::create_dir_all(&logs).unwrap();
        let old = logs.join("1-hooks.log");
        let new = logs.join("2-hooks.log");
        std::fs::write(&old, "old").unwrap();
        std::fs::write(&new, "x".repeat(600 * 1024)).unwrap();
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(past)
            .unwrap();

        let files = list_logs(dir.path()).unwrap();
        assert_eq!(files[0].name, "2-hooks.log");
        assert_eq!(files[1].name, "1-hooks.log");

        let text = read_log(&new).unwrap();
        assert!(text.starts_with("… (earlier output omitted)"));
        assert!(text.len() < 520 * 1024);
        assert_eq!(read_log(&old).unwrap(), "old");
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git is installed");
        assert!(status.success(), "git {args:?}");
    }

    fn request(root: &Path) -> RefreshRequest {
        RefreshRequest {
            root: root.to_path_buf(),
            listed: None,
            load_config: true,
            workspace: None,
            processes: ProcessWant::No,
            want_logs: false,
            selected_log: None,
            pitchfork: PitchforkSource::OnPath,
            services: ServicesRead::counts(),
        }
    }

    /// The whole project lifecycle the GUI drives, against the real core in a
    /// scratch repository and an isolated registry.
    #[test]
    fn add_initialize_and_refresh_a_real_project() {
        let _dirs = ScratchDirs::new();
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q", "-b", "main"]);
        git(
            repo.path(),
            &["commit", "-q", "--allow-empty", "-m", "init"],
        );
        let root = repo.path().canonicalize().unwrap();

        assert!(matches!(
            refresh(request(&root)),
            Err(LoadError::NotInitialized)
        ));
        match probe_project(root.clone()) {
            Ok(Probe::NotInitialized(path)) => assert_eq!(path, root),
            other => panic!("unexpected probe result: {:?}", other.map(|_| ())),
        }

        let id = init_project(root.clone(), false).unwrap();
        let loaded = refresh(request(&root))
            .ok()
            .expect("project loads once initialized");
        let workspaces = loaded.workspaces.unwrap();
        assert!(workspaces.iter().any(|w| w.workspace.is_default));
        assert!(!loaded.database.configured);
        assert!(
            loaded
                .config
                .unwrap()
                .unwrap()
                .content
                .contains("[project]")
        );

        let entries = load_entries(None, Some(root.clone())).unwrap();
        assert_eq!(entries.select, Some(id));
        assert!(entries.entries.iter().any(|e| e.root == root));
        assert!(entries.adhoc.is_none());

        // An unregistered, uninitialized folder is shown as a placeholder.
        let other = tempfile::tempdir().unwrap();
        let other_root = other.path().canonicalize().unwrap();
        let entries = load_entries(None, Some(other_root.clone())).unwrap();
        assert_eq!(entries.select, Some(Uuid::nil()));
        assert_eq!(entries.adhoc, Some(other_root));
    }

    #[test]
    fn adhoc_entries_do_not_duplicate_registered_roots() {
        let mut entries = vec![Entry {
            id: Uuid::new_v4(),
            name: "a".into(),
            root: PathBuf::from("/x/a"),
        }];
        add_adhoc(&mut entries, PathBuf::from("/x/a"));
        assert_eq!(entries.len(), 1);
        add_adhoc(&mut entries, PathBuf::from("/x/b"));
        assert_eq!(entries[1].name, "b");
        assert!(entries[1].id.is_nil());
    }
}
