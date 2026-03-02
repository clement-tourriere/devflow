/// All possible actions that flow through the TUI.
///
/// Components emit actions in response to events; the App dispatches
/// them back to all components via `update()`.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum Action {
    // ── Navigation ──
    Quit,
    NextTab,
    PrevTab,
    SelectTab(usize),
    ToggleHelp,

    // ── Data refresh ──
    Refresh,
    /// Data has been loaded in the background
    DataLoaded(DataPayload),
    /// An async operation completed
    OperationComplete {
        success: bool,
        message: String,
    },
    /// An error occurred
    Error(String),

    // ── Branch actions ──
    SwitchServices(String),
    OpenBranchAndExit(String),
    CreateBranch {
        name: String,
        from: Option<String>,
    },
    DeleteBranch(String),
    /// Internal: delete the VCS branch after service branches are cleaned up.
    /// Sent by background tasks back to the main thread.
    DeleteVcsBranch(String),

    // ── Service actions ──
    StartService {
        service: String,
        branch: String,
    },
    StopService {
        service: String,
        branch: String,
    },
    ResetService {
        service: String,
        branch: String,
    },
    DeleteServiceBranch {
        service: String,
        branch: String,
    },
    ViewLogs {
        service: String,
        branch: String,
    },
    RunDoctor,

    // ── Environments tree actions ──
    /// Toggle collapse/expand of a tree node
    CollapseToggle(String),
    /// Start all services for a branch
    StartAllServices(String),
    /// Stop all services for a branch
    StopAllServices(String),

    // ── System tab actions ──
    /// Switch sub-section within the System tab (0=Config, 1=Hooks, 2=Doctor, 3=Capabilities)
    SelectSubSection(usize),

    // ── Confirmation dialog ──
    ShowConfirm {
        title: String,
        message: String,
        on_confirm: Box<Action>,
    },
    ConfirmYes,
    ConfirmNo,

    // ── Input dialog ──
    ShowInput {
        title: String,
        on_submit: InputTarget,
    },
    SubmitInput(String),
    CancelInput,

    // ── Misc ──
    Tick,
    None,
}

/// Where to send input dialog results.
#[derive(Debug, Clone)]
pub enum InputTarget {
    CreateBranch { from: Option<String> },
    FilterBranches,
    FilterLogsPicker,
}

/// Async data payloads that come back from background tasks.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum DataPayload {
    Branches(BranchesData),
    Services(ServicesData),
    Capabilities(CapabilitiesData),
    ConnectionInfo(Vec<ConnectionInfoEntry>),
    DoctorResults(Vec<DoctorEntry>),
    Logs { service: String, content: String },
    ConfigYaml(String),
    HooksData(HooksData),
}

/// Enriched branch info combining VCS + service data.
#[derive(Debug, Clone)]
pub struct EnrichedBranch {
    pub name: String,
    pub is_current: bool,
    pub is_default: bool,
    pub worktree_path: Option<String>,
    pub services: Vec<BranchServiceState>,
    /// Parent branch name from the devflow branch registry.
    pub parent: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BranchServiceState {
    pub service_name: String,
    pub state: Option<String>,
    pub database_name: Option<String>,
    pub parent_branch: Option<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct BranchesData {
    pub branches: Vec<EnrichedBranch>,
    pub current_branch: Option<String>,
    pub default_branch: Option<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ServiceEntry {
    pub name: String,
    pub provider_type: String,
    pub service_type: String,
    pub auto_branch: bool,
    pub is_default: bool,
    pub branches: Vec<ServiceBranchEntry>,
    pub project_info: Option<ProjectInfoEntry>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ServiceBranchEntry {
    pub name: String,
    pub state: Option<String>,
    pub parent_branch: Option<String>,
    pub database_name: String,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ProjectInfoEntry {
    pub name: String,
    pub storage_driver: Option<String>,
    pub image: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ServicesData {
    pub services: Vec<ServiceEntry>,
}

#[derive(Debug, Clone)]
pub struct CapabilitiesData {
    pub vcs_provider: Option<String>,
    pub worktree_cow: String,
    pub services: Vec<ServiceCapabilityEntry>,
}

#[derive(Debug, Clone)]
pub struct ServiceCapabilityEntry {
    pub service_name: String,
    pub provider_name: String,
    pub capabilities: devflow_core::services::ServiceCapabilities,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ConnectionInfoEntry {
    pub service_name: String,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: Option<String>,
    pub connection_string: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DoctorEntry {
    pub service_name: String,
    pub checks: Vec<DoctorCheckEntry>,
}

#[derive(Debug, Clone)]
pub struct DoctorCheckEntry {
    pub name: String,
    pub available: bool,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct HooksData {
    pub phases: Vec<HookPhaseEntry>,
}

#[derive(Debug, Clone)]
pub struct HookPhaseEntry {
    pub phase: String,
    pub hooks: Vec<HookEntryInfo>,
}

#[derive(Debug, Clone)]
pub struct HookEntryInfo {
    pub name: String,
    pub command: String,
    pub is_extended: bool,
    pub background: bool,
    pub condition: Option<String>,
}
