/// All possible actions that flow through the TUI.
///
/// Components emit actions in response to events; the App dispatches
/// them back to all components via `update()`.
#[derive(Debug, Clone)]
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

    // ── Workspace actions ──
    SwitchServices(String),
    OpenBranchAndExit(String),
    CreateBranch {
        name: String,
        from: Option<String>,
    },
    DeleteBranch(String),
    /// Internal: delete the VCS workspace after service workspaces are cleaned up.
    /// Sent by background tasks back to the main thread.
    DeleteVcsBranch(String),
    /// Merge a workspace into a target.
    MergeWorkspace {
        source: String,
        target: String,
    },
    /// Rebase a workspace onto a target.
    RebaseWorkspace {
        source: String,
        target: String,
    },
    /// Merge checks completed — show results in overlay.
    MergeChecksComplete(devflow_core::merge::MergeReadinessReport),
    /// Rebase completed — show results.
    RebaseComplete(devflow_core::merge::RebaseResult),

    // ── Merge train actions ──
    TrainAdd {
        workspace: String,
        target: String,
    },
    TrainRun {
        target: String,
    },
    TrainStatus {
        target: String,
    },
    MergeTrainProgress(devflow_core::merge::train::MergeTrainEntry),

    // ── Service config actions ──
    /// Add a new service configuration (triggers wizard flow)
    AddServiceConfig {
        service_type: String,
        name: String,
    },
    /// Remove a service configuration
    RemoveServiceConfig(String),

    // ── Service actions ──
    StartService {
        service: String,
        workspace: String,
    },
    StopService {
        service: String,
        workspace: String,
    },
    ResetService {
        service: String,
        workspace: String,
    },
    ViewLogs {
        service: String,
        workspace: String,
    },
    RunDoctor,
    InstallAgentSkills,

    // ── Skill tab actions ──
    SkillSearch(String),
    SkillSearchResults(Vec<SkillSearchEntry>),
    SkillInstall(String),
    SkillRemove(String),
    SkillUpdate(Option<String>),
    /// Toggle skills tab between project and user scope
    SkillToggleScope,
    /// User-scope variants
    UserSkillInstall(String),
    UserSkillRemove(String),
    UserSkillUpdate(Option<String>),

    // ── Environments tree actions ──
    /// Start all services for a workspace
    StartAllServices(String),
    /// Stop all services for a workspace
    StopAllServices(String),

    // ── Proxy actions ──
    StartProxy,
    StopProxy,

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

    // ── Select dialog ──
    ShowSelect {
        title: String,
        options: Vec<String>,
        on_select: SelectTarget,
    },
    SelectOption(usize),
    CancelSelect,

    // ── Misc ──
    None,
}

/// Where to send input dialog results.
#[derive(Debug, Clone)]
pub enum InputTarget {
    CreateBranch {
        from: Option<String>,
    },
    FilterBranches,
    FilterLogsPicker,
    /// Name input for a new service (service_type already selected)
    AddServiceName {
        service_type: String,
    },
    SkillSearch,
}

/// Where to send select dialog results.
#[derive(Debug, Clone)]
pub enum SelectTarget {
    /// User is picking a service type to add
    AddServiceType,
}

/// Async data payloads that come back from background tasks.
#[derive(Debug, Clone)]
pub enum DataPayload {
    Branches(BranchesData),
    Services(ServicesData),
    Capabilities(CapabilitiesData),
    DoctorResults(Vec<DoctorEntry>),
    Logs { service: String, content: String },
    ConfigYaml(String),
    HooksData(HooksData),
    ProxyStatus(super::components::proxy_tab::ProxyStatusData),
    ProxyTargets(Vec<super::components::proxy_tab::ProxyTargetEntry>),
    Skills(SkillsTabData),
    UserSkills(SkillsTabData),
}

/// Enriched workspace info combining VCS + service data.
#[derive(Debug, Clone)]
pub struct EnrichedBranch {
    pub name: String,
    pub is_current: bool,
    pub is_default: bool,
    pub worktree_path: Option<String>,
    pub services: Vec<BranchServiceState>,
    /// Parent workspace name from the devflow workspace registry.
    pub parent: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BranchServiceState {
    pub service_name: String,
    pub state: Option<String>,
    pub database_name: Option<String>,
    pub parent_workspace: Option<String>,
    /// Whether this service supports lifecycle operations (start/stop/reset/logs).
    /// Only true for local Docker-based providers.
    pub supports_lifecycle: bool,
}

#[derive(Debug, Clone)]
pub struct BranchesData {
    pub workspaces: Vec<EnrichedBranch>,
}

#[derive(Debug, Clone)]
pub struct ServiceEntry {
    pub name: String,
    pub provider_type: String,
    pub service_type: String,
    pub workspaces: Vec<ServiceWorkspaceEntry>,
    pub project_info: Option<ProjectInfoEntry>,
}

#[derive(Debug, Clone)]
pub struct ServiceWorkspaceEntry {
    pub name: String,
    pub state: Option<String>,
    pub parent_workspace: Option<String>,
    pub database_name: String,
}

#[derive(Debug, Clone)]
pub struct ProjectInfoEntry {
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

/// Data for the Skills tab.
#[derive(Debug, Clone)]
pub struct SkillsTabData {
    pub installed: Vec<SkillEntry>,
    pub updates_available: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SkillEntry {
    pub name: String,
    pub source_label: String,
    pub content_hash: String,
    pub installed_at: String,
    pub content: Option<String>,
    /// Whether this skill is managed by devflow (true) or discovered externally (false).
    pub managed: bool,
}

#[derive(Debug, Clone)]
pub struct SkillSearchEntry {
    pub name: String,
    pub source: String,
    pub installs: u64,
}
