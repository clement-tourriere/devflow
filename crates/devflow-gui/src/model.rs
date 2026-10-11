//! Plain data the views render, plus the pure logic around it (labels, rows,
//! stale-result filtering). Nothing here touches GPUI or blocks.
use chrono::{DateTime, Local, Utc};
use devflow_core::{
    config::{ConfigDocument, CowCapability, PostgresMode},
    db::{BranchInfo, CowSource, DataSource, RecordStatus, ServerStatus, SnapshotInfo},
    pitchfork::Daemon,
    project::Project,
    receipt::Status,
    registry::Entry,
    setup::{ExpectedWorkspace, WorkspaceStatus},
    trust::TrustState,
    worktree::Workspace,
};
use std::{
    collections::HashSet,
    hash::{Hash as _, Hasher as _},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};
use uuid::Uuid;

/// Errors from core calls are flattened to text on the background thread.
pub type Section<T> = Result<T, String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Project,
    Settings,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Workspaces,
    Database,
    Daemons,
    Config,
    Logs,
}

impl Tab {
    pub const ALL: [Tab; 5] = [
        Tab::Workspaces,
        Tab::Database,
        Tab::Daemons,
        Tab::Config,
        Tab::Logs,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Workspaces => "Workspaces",
            Tab::Database => "Database",
            Tab::Daemons => "Daemons",
            Tab::Config => "Config",
            Tab::Logs => "Logs",
        }
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    /// Tabs whose data is tied to one workspace (shows the workspace picker).
    pub fn needs_workspace(self) -> bool {
        matches!(self, Tab::Daemons | Tab::Logs)
    }
}

// ---- selection and stale-result filtering ---------------------------------

/// Identifies one selection of a project. Every background request carries it;
/// a result whose key no longer matches is dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestKey {
    pub project: Uuid,
    pub epoch: u64,
}

/// Whether a notification's "Reload" still applies to the selected project.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReloadTarget {
    /// The selection and the document it was offered for are still current.
    Current,
    /// Another project is selected now.
    OtherProject,
    /// The project was reselected, or its file already reloaded or saved.
    Outdated,
}

#[derive(Default)]
pub struct Selection {
    model: Option<ProjectModel>,
    epoch: u64,
}

impl Selection {
    /// Replace the selected project; results for the previous one become stale.
    pub fn select(&mut self, entry: Entry) -> RequestKey {
        self.epoch += 1;
        let key = RequestKey {
            project: entry.id,
            epoch: self.epoch,
        };
        self.model = Some(ProjectModel::new(entry, key));
        key
    }

    pub fn clear(&mut self) {
        self.epoch += 1;
        self.model = None;
    }

    pub fn current(&self) -> Option<&ProjectModel> {
        self.model.as_ref()
    }

    pub fn current_mut(&mut self) -> Option<&mut ProjectModel> {
        self.model.as_mut()
    }

    #[cfg(test)]
    pub fn is_current(&self, key: RequestKey) -> bool {
        self.model.as_ref().is_some_and(|m| m.key == key)
    }

    /// The model, only if `key` still identifies the selected project.
    pub fn accept(&mut self, key: RequestKey) -> Option<&mut ProjectModel> {
        self.model.as_mut().filter(|m| m.key == key)
    }

    /// Does a "Reload" offered for the document `loaded` of the selection
    /// `key` still apply to what is selected and in the editor?
    pub fn reload_target(&self, key: RequestKey, loaded: &ConfigIdentity) -> ReloadTarget {
        match self.model.as_ref() {
            Some(model) if model.key == key => match &model.config {
                Some(config) if config.identity == *loaded => ReloadTarget::Current,
                _ => ReloadTarget::Outdated,
            },
            Some(model) if model.key.project == key.project => ReloadTarget::Outdated,
            _ => ReloadTarget::OtherProject,
        }
    }

    pub fn current_key(&self) -> Option<RequestKey> {
        self.model.as_ref().map(|m| m.key)
    }
}

// ---- per-project model -----------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum LoadPhase {
    Loading,
    Ready,
    /// The folder is a project root candidate without `devflow.toml`.
    NotInitialized,
    Failed(String),
}

#[derive(Clone, Debug, Default)]
pub struct DatabaseModel {
    pub configured: bool,
    pub status: Option<ServerStatus>,
    pub branches: Vec<BranchInfo>,
    pub snapshots: Vec<SnapshotInfo>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct PitchforkModel {
    /// Pitchfork was queried at least once (distinguishes "not installed" from "not asked yet").
    pub loaded: bool,
    pub available: bool,
    pub daemons: Vec<Daemon>,
    pub error: Option<String>,
}

/// Which loaded document an editor holds. The generation comes from one
/// process-wide counter, so two documents never share it, even of different
/// projects; the project and root are kept so a stale editor is recognizable
/// and Save can be bound to the document its text was loaded from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigIdentity {
    pub project: Uuid,
    pub root: PathBuf,
    pub generation: u64,
}

static NEXT_CONFIG_GENERATION: AtomicU64 = AtomicU64::new(1);

pub struct ConfigModel {
    pub doc: ConfigDocument,
    pub identity: ConfigIdentity,
}

impl ConfigModel {
    /// A freshly (re)loaded document of the project rooted at `root`.
    pub fn new(doc: ConfigDocument, project: Uuid, root: &Path) -> Self {
        Self {
            doc,
            identity: ConfigIdentity {
                project,
                root: root.to_path_buf(),
                generation: NEXT_CONFIG_GENERATION.fetch_add(1, Ordering::Relaxed),
            },
        }
    }
}

#[derive(Clone, Debug)]
pub struct LogFile {
    pub name: String,
    pub path: PathBuf,
    pub modified: Option<SystemTime>,
}

#[derive(Default)]
pub struct LogsModel {
    pub files: Vec<LogFile>,
    pub selected: Option<PathBuf>,
    pub content: Option<Section<String>>,
    pub error: Option<String>,
}

/// What a background refresh needs to know about the selected workspace.
#[derive(Clone, Debug)]
pub struct WorkspaceTarget {
    pub path: PathBuf,
    pub git_dir: PathBuf,
}

/// A workspace by name and location: a worktree recreated elsewhere under the
/// same name is a different workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceId {
    pub name: String,
    pub path: PathBuf,
}

/// What a background refresh was started for. Its result is applied only if
/// every part still describes the model: same selection, same workspace, and
/// no mutation, selection change or newer refresh since.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefreshStamp {
    pub key: RequestKey,
    pub generation: u64,
    pub workspace: Option<WorkspaceId>,
}

/// A workspace of one specific project, for actions started from a
/// notification or dialog that may outlive the selection they were made in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceRef {
    pub project: Uuid,
    pub root: PathBuf,
    pub name: String,
    pub path: PathBuf,
    /// The receipt's instance id when the reference was made; `None` for a
    /// workspace that had no receipt. A workspace recreated under the same
    /// name gets a new one.
    pub instance_id: Option<Uuid>,
    /// The checkout directory's identity when the list was loaded, so a
    /// directory recreated in its place is refused even without a receipt.
    pub fingerprint: Option<String>,
}

impl WorkspaceRef {
    /// What core must find under `name` before it removes anything: exactly
    /// the workspace this reference was made from.
    pub fn expected(&self) -> ExpectedWorkspace {
        ExpectedWorkspace {
            path: self.path.clone(),
            instance_id: self.instance_id,
            fingerprint: self.fingerprint.clone(),
        }
    }
}

pub struct ProjectModel {
    pub key: RequestKey,
    pub entry: Entry,
    pub phase: LoadPhase,
    pub project: Option<Arc<Project>>,
    pub workspaces: Vec<WorkspaceStatus>,
    pub workspaces_error: Option<String>,
    pub database: DatabaseModel,
    pub trust: Option<TrustState>,
    pub pitchfork: PitchforkModel,
    pub config: Option<ConfigModel>,
    pub config_error: Option<String>,
    pub logs: LogsModel,
    pub selected_workspace: Option<String>,
    pub refreshing: bool,
    /// A refresh was requested while one was running.
    pub refresh_again: bool,
    /// Moves on every refresh start and every invalidation; only a result
    /// stamped with the current value is applied.
    pub refresh_generation: u64,
}

impl ProjectModel {
    pub fn new(entry: Entry, key: RequestKey) -> Self {
        Self {
            key,
            entry,
            phase: LoadPhase::Loading,
            project: None,
            workspaces: Vec::new(),
            workspaces_error: None,
            database: DatabaseModel::default(),
            trust: None,
            pitchfork: PitchforkModel::default(),
            config: None,
            config_error: None,
            logs: LogsModel::default(),
            selected_workspace: None,
            refreshing: false,
            refresh_again: false,
            refresh_generation: 0,
        }
    }

    pub fn selected_target(&self) -> Option<WorkspaceTarget> {
        let ws = self.selected_workspace_entry()?;
        Some(WorkspaceTarget {
            path: ws.path.clone(),
            git_dir: ws.git_dir.clone(),
        })
    }

    fn selected_workspace_entry(&self) -> Option<&Workspace> {
        let name = self.selected_workspace.as_ref()?;
        self.workspaces
            .iter()
            .map(|w| &w.workspace)
            .find(|w| &w.name == name)
    }

    pub fn selected_workspace_id(&self) -> Option<WorkspaceId> {
        let ws = self.selected_workspace_entry()?;
        Some(WorkspaceId {
            name: ws.name.clone(),
            path: ws.path.clone(),
        })
    }

    /// Start a refresh: older ones still in flight become stale.
    pub fn begin_refresh(&mut self) -> RefreshStamp {
        self.refresh_generation += 1;
        RefreshStamp {
            key: self.key,
            generation: self.refresh_generation,
            workspace: self.selected_workspace_id(),
        }
    }

    /// Something the in-flight refresh did not see has changed (a mutation
    /// started or ended, another workspace or log was chosen).
    pub fn invalidate_refresh(&mut self) {
        self.refresh_generation += 1;
    }

    /// Does a refresh started with `stamp` still describe this model?
    pub fn accepts(&self, stamp: &RefreshStamp) -> bool {
        stamp.key == self.key
            && stamp.generation == self.refresh_generation
            && stamp.workspace == self.selected_workspace_id()
    }

    pub fn workspace_ref(&self, name: &str) -> Option<WorkspaceRef> {
        let status = self.workspace(name)?;
        Some(WorkspaceRef {
            project: self.entry.id,
            root: self.entry.root.clone(),
            name: status.workspace.name.clone(),
            path: status.workspace.path.clone(),
            instance_id: instance_id(status),
            fingerprint: status.workspace.fingerprint.clone(),
        })
    }

    /// The workspace `target` names, if this model is still the same project
    /// and the workspace still lives at the same place with the same identity.
    pub fn resolve(&self, target: &WorkspaceRef) -> Option<&WorkspaceStatus> {
        if self.entry.id != target.project || self.entry.root != target.root {
            return None;
        }
        self.workspace(&target.name).filter(|w| {
            w.workspace.path == target.path
                && instance_id(w) == target.instance_id
                && w.workspace.fingerprint == target.fingerprint
        })
    }

    pub fn workspace(&self, name: &str) -> Option<&WorkspaceStatus> {
        self.workspaces.iter().find(|w| w.workspace.name == name)
    }

    pub fn default_workspace(&self) -> Option<&Workspace> {
        self.workspaces
            .iter()
            .map(|w| &w.workspace)
            .find(|w| w.is_default)
    }

    /// Keep the selected workspace valid after the list changed.
    pub fn fix_selected_workspace(&mut self) {
        let valid = self
            .selected_workspace
            .as_ref()
            .is_some_and(|name| self.workspace(name).is_some());
        if !valid {
            self.selected_workspace = self.default_workspace().map(|w| w.name.clone());
        }
    }

    pub fn server_badge(&self) -> ServerBadge {
        server_badge(&self.database)
    }
}

// ---- workspaces ------------------------------------------------------------

/// The receipt's instance id: the identity of this incarnation of a workspace.
fn instance_id(status: &WorkspaceStatus) -> Option<Uuid> {
    status.receipt.as_ref().map(|r| r.instance_id)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceState {
    Ready,
    NeedsAttention,
    InProgress,
    NotSetUp,
    Busy,
    Missing,
}

impl WorkspaceState {
    pub fn of(ws: &WorkspaceStatus) -> Self {
        if ws.workspace.missing {
            return Self::Missing;
        }
        if ws.busy {
            return Self::Busy;
        }
        match (ws.status, &ws.receipt) {
            (Some(Status::Ready), _) => Self::Ready,
            (Some(Status::NeedsAttention), _) => Self::NeedsAttention,
            (Some(Status::InProgress), _) | (None, Some(_)) => Self::InProgress,
            (None, None) => Self::NotSetUp,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::NeedsAttention => "needs attention",
            Self::InProgress => "in progress",
            Self::NotSetUp => "not set up",
            Self::Busy => "busy",
            Self::Missing => "missing",
        }
    }
}

#[derive(Clone, Debug)]
pub struct WorkspaceRow {
    pub workspace: Workspace,
    pub state: WorkspaceState,
    pub branch: String,
    pub database: String,
    pub path: String,
}

pub fn workspace_rows(list: &[WorkspaceStatus]) -> Vec<WorkspaceRow> {
    list.iter()
        .map(|ws| WorkspaceRow {
            state: WorkspaceState::of(ws),
            branch: ws
                .workspace
                .branch
                .clone()
                .unwrap_or_else(|| "(detached)".into()),
            database: database_label(ws),
            path: shorten_home(&ws.workspace.path),
            workspace: ws.workspace.clone(),
        })
        .collect()
}

fn database_label(ws: &WorkspaceStatus) -> String {
    match ws.receipt.as_ref().and_then(|r| r.data.as_ref()) {
        Some(data) if data.branch_id.is_none() => format!("{} (url)", data.database),
        Some(data) => data.database.clone(),
        None => "none".into(),
    }
}

pub fn shorten_home(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME")
        && let Ok(rest) = path.strip_prefix(Path::new(&home))
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

/// Branch ids that some local worktree receipt is bound to.
pub fn attached_branch_ids(workspaces: &[WorkspaceStatus]) -> HashSet<Uuid> {
    workspaces
        .iter()
        .filter_map(|w| w.receipt.as_ref()?.data.as_ref()?.branch_id)
        .collect()
}

// ---- database --------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerBadge {
    Running,
    Stopped,
    Unreachable,
    NotConfigured,
}

impl ServerBadge {
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Unreachable => "unreachable",
            Self::NotConfigured => "not configured",
        }
    }
}

pub fn server_badge(db: &DatabaseModel) -> ServerBadge {
    if !db.configured {
        return ServerBadge::NotConfigured;
    }
    match &db.status {
        Some(s) if s.reachable => ServerBadge::Running,
        Some(s) if s.mode == PostgresMode::Managed && !s.running => ServerBadge::Stopped,
        _ => ServerBadge::Unreachable,
    }
}

/// `verified`, `copy` or `unknown`, with how it was established when known.
pub fn cow_text(cow: CowCapability, source: Option<CowSource>) -> String {
    match source {
        Some(CowSource::Measured) => format!("{} (measured)", cow_label(cow)),
        Some(CowSource::Declared) => format!("{} (declared)", cow_label(cow)),
        None => cow_label(cow).to_string(),
    }
}

pub fn cow_label(cow: CowCapability) -> &'static str {
    match cow {
        CowCapability::Verified => "verified",
        CowCapability::Copy => "copy",
        CowCapability::Unknown => "unknown",
    }
}

fn record_status_label(status: RecordStatus) -> &'static str {
    match status {
        RecordStatus::Creating => "creating",
        RecordStatus::Ready => "ready",
        RecordStatus::Deleting => "deleting",
        RecordStatus::Failed => "failed",
    }
}

pub fn format_bytes(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut value = bytes.max(0) as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} B", bytes.max(0))
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Identifies the text a log view shows. The tail window of a long log keeps a
/// constant length while its content moves, so the length alone is not enough.
pub fn fingerprint(text: &str) -> (usize, u64) {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    (text.len(), hasher.finish())
}

pub fn format_when(when: DateTime<Utc>) -> String {
    when.with_timezone(&Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

#[derive(Clone, Debug, PartialEq)]
pub struct BranchRow {
    pub id: Uuid,
    pub name: String,
    pub database: String,
    pub source: String,
    pub status: String,
    pub size: String,
    pub cow: &'static str,
    /// No local worktree receipt references this branch.
    pub unattached: bool,
}

/// `empty`, `branch <name>` or `snapshot <name@vN>`, resolved from what we know.
fn describe_source(kind: &str, id: Option<Uuid>, db: &DatabaseModel) -> String {
    match (kind, id) {
        ("branch", Some(id)) => match db.branches.iter().find(|b| b.id == id) {
            Some(b) => format!("branch {}", b.name),
            None => "branch (deleted)".into(),
        },
        ("snapshot", Some(id)) => match db.snapshots.iter().find(|s| s.id == id) {
            Some(s) => format!("snapshot {}@v{}", s.name, s.version),
            None => "snapshot (deleted)".into(),
        },
        (kind, _) => kind.to_string(),
    }
}

pub fn branch_rows(db: &DatabaseModel, workspaces: &[WorkspaceStatus]) -> Vec<BranchRow> {
    let attached = attached_branch_ids(workspaces);
    db.branches
        .iter()
        .map(|b| BranchRow {
            id: b.id,
            name: b.name.clone(),
            database: b.db_name.clone(),
            source: describe_source(&b.source_kind, b.source_id, db),
            status: record_status_label(b.status).into(),
            size: b.size_bytes.map(format_bytes).unwrap_or_else(|| "—".into()),
            cow: cow_label(b.cow),
            unattached: !attached.contains(&b.id),
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotRow {
    pub name: String,
    pub version: i32,
    pub source: String,
    pub created: String,
}

impl SnapshotRow {
    pub fn label(&self) -> String {
        format!("{}@v{}", self.name, self.version)
    }
}

pub fn snapshot_rows(db: &DatabaseModel) -> Vec<SnapshotRow> {
    db.snapshots
        .iter()
        .map(|s| SnapshotRow {
            name: s.name.clone(),
            version: s.version,
            source: describe_source("branch", s.source_branch_id, db),
            created: format_when(s.created_at),
        })
        .collect()
}

/// One choice in a "data from" select.
#[derive(Clone, Debug, PartialEq)]
pub struct DataOption {
    pub label: String,
    /// `None` means the caller's default (code source, or the recorded source).
    pub source: Option<DataSource>,
}

pub fn data_options(
    default_label: &str,
    workspaces: &[WorkspaceStatus],
    snapshots: &[SnapshotInfo],
) -> Vec<DataOption> {
    let mut options = vec![
        DataOption {
            label: default_label.into(),
            source: None,
        },
        DataOption {
            label: "Empty database".into(),
            source: Some(DataSource::Empty),
        },
    ];
    for ws in workspaces {
        if ws.receipt.as_ref().is_some_and(|r| r.data.is_some()) {
            options.push(DataOption {
                label: format!("Workspace {}", ws.workspace.name),
                source: Some(DataSource::Workspace {
                    name: ws.workspace.name.clone(),
                }),
            });
        }
    }
    for snap in snapshots.iter().filter(|s| s.status == RecordStatus::Ready) {
        options.push(DataOption {
            label: format!("Snapshot {}@v{}", snap.name, snap.version),
            source: Some(DataSource::Snapshot {
                name: snap.name.clone(),
                version: Some(snap.version),
            }),
        });
    }
    options
}

/// Workspaces whose database branch can be snapshotted or reset.
pub fn data_workspaces(workspaces: &[WorkspaceStatus]) -> Vec<Workspace> {
    workspaces
        .iter()
        .filter(|w| w.receipt.as_ref().is_some_and(|r| r.data.is_some()))
        .map(|w| w.workspace.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use devflow_core::receipt::{DataBinding, Receipt};

    fn workspace(name: &str, is_default: bool) -> Workspace {
        Workspace {
            name: name.into(),
            branch: Some(name.into()),
            path: PathBuf::from(format!("/tmp/{name}")),
            git_dir: PathBuf::from(format!("/tmp/{name}/.git")),
            is_default,
            locked: false,
            missing: false,
            head: None,
            fingerprint: None,
        }
    }

    fn status(
        name: &str,
        receipt: Option<Receipt>,
        status: Option<Status>,
        busy: bool,
    ) -> WorkspaceStatus {
        WorkspaceStatus {
            workspace: workspace(name, false),
            receipt,
            status,
            busy,
        }
    }

    fn entry(name: &str) -> Entry {
        Entry {
            id: Uuid::new_v4(),
            name: name.into(),
            root: PathBuf::from(format!("/tmp/{name}")),
        }
    }

    #[test]
    fn stale_results_are_dropped_after_switching_projects() {
        let mut selection = Selection::default();
        let (a, b) = (entry("a"), entry("b"));
        let key_a = selection.select(a.clone());
        assert!(selection.accept(key_a).is_some());

        let key_b = selection.select(b);
        assert!(
            selection.accept(key_a).is_none(),
            "a's result must not touch b"
        );
        assert!(selection.accept(key_b).is_some());

        // Re-selecting the same project invalidates requests from before.
        let key_a2 = selection.select(a);
        assert_eq!(key_a.project, key_a2.project);
        assert!(!selection.is_current(key_a));
        assert!(selection.is_current(key_a2));

        selection.clear();
        assert!(selection.accept(key_a2).is_none());
    }

    #[test]
    fn workspace_state_labels() {
        let receipt = || Receipt::new(Uuid::new_v4(), "w", None);
        let cases = [
            (status("w", None, None, false), "not set up"),
            (
                status("w", Some(receipt()), Some(Status::Ready), false),
                "ready",
            ),
            (
                status("w", Some(receipt()), Some(Status::NeedsAttention), false),
                "needs attention",
            ),
            (
                status("w", Some(receipt()), Some(Status::InProgress), false),
                "in progress",
            ),
            (
                status("w", Some(receipt()), Some(Status::Ready), true),
                "busy",
            ),
        ];
        for (ws, label) in cases {
            assert_eq!(WorkspaceState::of(&ws).label(), label);
        }
        let mut missing = status("w", None, None, false);
        missing.workspace.missing = true;
        assert_eq!(WorkspaceState::of(&missing), WorkspaceState::Missing);
    }

    fn server_status(mode: PostgresMode, running: bool, reachable: bool) -> ServerStatus {
        ServerStatus {
            mode,
            running,
            reachable,
            endpoint: None,
            server_version: None,
            file_copy_method: None,
            cow: CowCapability::Unknown,
            cow_source: None,
            problem: None,
        }
    }

    #[test]
    fn server_badge_distinguishes_stopped_from_unreachable() {
        let mut db = DatabaseModel::default();
        assert_eq!(server_badge(&db), ServerBadge::NotConfigured);
        db.configured = true;
        assert_eq!(server_badge(&db), ServerBadge::Unreachable);
        db.status = Some(server_status(PostgresMode::Managed, true, true));
        assert_eq!(server_badge(&db), ServerBadge::Running);
        db.status = Some(server_status(PostgresMode::Managed, false, false));
        assert_eq!(server_badge(&db), ServerBadge::Stopped);
        db.status = Some(server_status(PostgresMode::External, true, false));
        assert_eq!(server_badge(&db), ServerBadge::Unreachable);
    }

    #[test]
    fn branches_are_unattached_without_a_local_receipt() {
        let attached_id = Uuid::new_v4();
        let mut receipt = Receipt::new(Uuid::new_v4(), "w", None);
        receipt.data = Some(DataBinding {
            branch_id: Some(attached_id),
            database: "df_x".into(),
            role: "dfl_x".into(),
            url: "postgres://".into(),
            source: "empty".into(),
            owned: true,
        });
        let workspaces = vec![status("w", Some(receipt), Some(Status::Ready), false)];
        let branch = |id: Uuid| BranchInfo {
            id,
            name: "n".into(),
            db_name: "df_x".into(),
            role_name: "dfl_x".into(),
            source_kind: "empty".into(),
            source_id: None,
            instance_id: None,
            status: RecordStatus::Ready,
            cow: CowCapability::Verified,
            error: None,
            created_at: Utc::now(),
            size_bytes: Some(3 * 1024 * 1024),
            generation: 0,
            warnings: Vec::new(),
        };
        let db = DatabaseModel {
            configured: true,
            branches: vec![branch(attached_id), branch(Uuid::new_v4())],
            ..Default::default()
        };
        let rows = branch_rows(&db, &workspaces);
        assert!(!rows[0].unattached);
        assert!(rows[1].unattached);
        assert_eq!(rows[0].size, "3.0 MB");
        assert_eq!(rows[0].cow, "verified");
    }

    #[test]
    fn cow_text_says_how_it_was_established() {
        assert_eq!(
            cow_text(CowCapability::Verified, Some(CowSource::Measured)),
            "verified (measured)"
        );
        assert_eq!(cow_text(CowCapability::Unknown, None), "unknown");
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 kB");
        assert_eq!(format_bytes(392 * 1024 * 1024), "392.0 MB");
    }

    #[test]
    fn data_options_start_with_defaults_and_skip_unbound_workspaces() {
        let options = data_options(
            "Same as code source",
            &[status("w", None, None, false)],
            &[],
        );
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].source, None);
        assert_eq!(options[1].source, Some(DataSource::Empty));
    }

    #[test]
    fn selected_workspace_falls_back_to_default() {
        let mut model = ProjectModel::new(
            entry("p"),
            RequestKey {
                project: Uuid::new_v4(),
                epoch: 1,
            },
        );
        model.workspaces = vec![WorkspaceStatus {
            workspace: workspace("main", true),
            receipt: None,
            status: None,
            busy: false,
        }];
        model.selected_workspace = Some("gone".into());
        model.fix_selected_workspace();
        assert_eq!(model.selected_workspace.as_deref(), Some("main"));
    }

    fn model_with(names: &[&str]) -> ProjectModel {
        let entry = entry("p");
        let key = RequestKey {
            project: entry.id,
            epoch: 1,
        };
        let mut model = ProjectModel::new(entry, key);
        model.workspaces = names
            .iter()
            .enumerate()
            .map(|(i, name)| WorkspaceStatus {
                workspace: workspace(name, i == 0),
                receipt: None,
                status: None,
                busy: false,
            })
            .collect();
        model.fix_selected_workspace();
        model
    }

    #[test]
    fn refresh_results_are_dropped_unless_they_describe_the_current_state() {
        let mut model = model_with(&["main", "feature"]);
        let for_main = model.begin_refresh();
        assert!(model.accepts(&for_main));

        // The user picked another workspace while that refresh was in flight:
        // main's daemons and logs must not be shown under feature.
        model.selected_workspace = Some("feature".into());
        assert!(!model.accepts(&for_main), "result for the old workspace");
        let for_feature = model.begin_refresh();
        assert!(model.accepts(&for_feature));
        assert!(!model.accepts(&for_main), "an older generation never wins");

        // A mutation starting (or ending) invalidates whatever is in flight.
        model.invalidate_refresh();
        assert!(!model.accepts(&for_feature));
        let after = model.begin_refresh();
        assert!(after.generation > for_feature.generation);
        assert!(model.accepts(&after));

        // The same workspace name at another location is another workspace.
        let mut moved = after.clone();
        moved.workspace.as_mut().unwrap().path = PathBuf::from("/elsewhere/feature");
        assert!(!model.accepts(&moved));

        // Another selection of the project never matches either.
        let mut other_epoch = after;
        other_epoch.key.epoch += 1;
        assert!(!model.accepts(&other_epoch));
    }

    #[test]
    fn config_generations_are_unique_across_projects() {
        let doc = |content: &str| ConfigDocument {
            path: PathBuf::from("/x/devflow.toml"),
            content: content.into(),
        };
        let (a, b) = (entry("a"), entry("b"));
        let first = ConfigModel::new(doc("a"), a.id, &a.root);
        let second = ConfigModel::new(doc("b"), b.id, &b.root);
        let again = ConfigModel::new(doc("a"), a.id, &a.root);
        assert_ne!(first.identity, second.identity);
        assert_ne!(first.identity.generation, second.identity.generation);
        assert_ne!(
            first.identity, again.identity,
            "a reload of the same project is a new document"
        );
        assert_eq!(second.identity.project, b.id);
        assert_eq!(second.identity.root, b.root);
    }

    #[test]
    fn workspace_refs_are_bound_to_their_project_and_location() {
        let mut model = model_with(&["main", "feature"]);
        let target = model.workspace_ref("feature").unwrap();
        assert!(model.resolve(&target).is_some());

        // Same workspace name in another project: not the same workspace.
        let mut other = model_with(&["main", "feature"]);
        other.entry.id = Uuid::new_v4();
        assert!(other.resolve(&target).is_none());
        other.entry = model.entry.clone();
        other.entry.root = PathBuf::from("/tmp/elsewhere");
        assert!(other.resolve(&target).is_none());

        // Removed and recreated somewhere else under the same name.
        model.workspaces[1].workspace.path = PathBuf::from("/tmp/p/new-feature");
        assert!(model.resolve(&target).is_none());
        model.workspaces.remove(1);
        assert!(model.resolve(&target).is_none());
        assert!(model.workspace_ref("feature").is_none());
    }

    #[test]
    fn workspace_refs_carry_the_receipt_instance_and_resolve_only_to_it() {
        let mut model = model_with(&["main", "feature"]);
        let receipt = Receipt::new(Uuid::new_v4(), "feature", None);
        let instance = receipt.instance_id;
        model.workspaces[1].receipt = Some(receipt);
        let target = model.workspace_ref("feature").unwrap();
        assert_eq!(target.instance_id, Some(instance));
        let expected = target.expected();
        assert_eq!(expected.path, target.path);
        assert_eq!(expected.instance_id, Some(instance));
        assert!(model.resolve(&target).is_some());

        // Recreated at the same place: a new incarnation with another receipt.
        model.workspaces[1].receipt = Some(Receipt::new(Uuid::new_v4(), "feature", None));
        assert!(model.resolve(&target).is_none());
        // Or one that was never set up.
        model.workspaces[1].receipt = None;
        assert!(model.resolve(&target).is_none());
        assert_eq!(model.workspace_ref("feature").unwrap().instance_id, None);
    }

    fn config_model(entry: &Entry) -> ConfigModel {
        let doc = ConfigDocument {
            path: entry.root.join("devflow.toml"),
            content: "[project]\n".into(),
        };
        ConfigModel::new(doc, entry.id, &entry.root)
    }

    #[test]
    fn a_reload_offer_applies_only_to_its_own_project_and_document() {
        let mut selection = Selection::default();
        let (a, b) = (entry("a"), entry("b"));
        let key_a = selection.select(a.clone());
        let config_a = config_model(&a);
        let offered = config_a.identity.clone();
        selection.current_mut().unwrap().config = Some(config_a);
        assert_eq!(
            selection.reload_target(key_a, &offered),
            ReloadTarget::Current
        );

        // The document was reloaded or saved since: nothing left to reload.
        selection.current_mut().unwrap().config = Some(config_model(&a));
        assert_eq!(
            selection.reload_target(key_a, &offered),
            ReloadTarget::Outdated
        );
        selection.current_mut().unwrap().config = None;
        assert_eq!(
            selection.reload_target(key_a, &offered),
            ReloadTarget::Outdated
        );

        // Another project is selected: its editor is not this offer's business.
        selection.select(b.clone());
        selection.current_mut().unwrap().config = Some(config_model(&b));
        assert_eq!(
            selection.reload_target(key_a, &offered),
            ReloadTarget::OtherProject
        );
        selection.clear();
        assert_eq!(
            selection.reload_target(key_a, &offered),
            ReloadTarget::OtherProject
        );

        // Back to A: a new selection, a new document.
        selection.select(a.clone());
        selection.current_mut().unwrap().config = Some(config_model(&a));
        assert_eq!(
            selection.reload_target(key_a, &offered),
            ReloadTarget::Outdated
        );
    }

    #[test]
    fn fingerprint_follows_the_text_not_just_its_length() {
        // A capped tail keeps its length while the content scrolls.
        let a = format!("{}a", "x".repeat(1000));
        let b = format!("{}b", "x".repeat(1000));
        assert_eq!(a.len(), b.len());
        assert_ne!(fingerprint(&a), fingerprint(&b));
        assert_eq!(fingerprint(&a), fingerprint(&a.clone()));
    }
}
