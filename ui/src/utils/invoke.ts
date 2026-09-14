import { invoke } from "@tauri-apps/api/core";
import type {
  ProjectEntry,
  ProjectDetail,
  WorkspacesResponse,
  ConnectionInfo,
  ServiceEntry,
  ServiceWorkspaceInfo,
  AddServiceRequest,
  DiscoveredContainer,
  DestroyServiceResult,
  HookPhaseEntry,
  VcsHooksActionResult,
  ProxyStatus,
  ContainerEntry,
  CertificateStatus,
  AppSettings,
  CreateWorkspaceResult,
  DestroyResult,
  DoctorReport,
  OrphanProjectEntry,
  OrphanCleanupResult,
  VcsInfo,
  VcsWorkspaceInfo,
  TerminalSessionInfo,
  PruneResult,
  SwitchWorkspaceResult,
  DeleteWorkspaceResult,
  DeleteWorkspacePreflight,
  ProcessStatus,
  ProcessOperationResponse,
} from "../types";

// Projects
export const listProjects = () => invoke<ProjectEntry[]>("list_projects");
export const removeProject = (path: string) =>
  invoke<void>("remove_project", { path });
export const getProjectDetail = (projectPath: string) =>
  invoke<ProjectDetail>("get_project_detail", { projectPath });
export const addOrInitProject = (path: string, name?: string, vcsPreference?: string, mainWorkspace?: string) =>
  invoke<ProjectEntry>("add_or_init_project", { path, name, vcsPreference, mainWorkspace });

// VCS
export const detectVcsInfo = (path: string) =>
  invoke<VcsInfo>("detect_vcs_info", { path });
export const detectVcsWorkspaces = (path: string) =>
  invoke<VcsWorkspaceInfo>("detect_vcs_workspaces", { path });

// Workspaces
export const listWorkspaces = (projectPath: string) =>
  invoke<WorkspacesResponse>("list_workspaces", { projectPath });
export const getConnectionInfo = (
  projectPath: string,
  workspaceName: string,
  serviceName?: string
) =>
  invoke<ConnectionInfo>("get_connection_info", {
    projectPath,
    workspaceName,
    serviceName,
  });
export const createWorkspace = (
  projectPath: string,
  workspaceName: string,
  fromWorkspace?: string,
  copyFiles?: string[],
  copyIgnored?: boolean,
  dataFromWorkspace?: string
) =>
  invoke<CreateWorkspaceResult>("create_workspace", {
    projectPath,
    workspaceName,
    fromWorkspace,
    dataFromWorkspace,
    copyFiles,
    copyIgnored,
  });
export const switchWorkspace = (projectPath: string, workspaceName: string) =>
  invoke<SwitchWorkspaceResult>("switch_workspace", {
    projectPath,
    workspaceName,
  });
export const preflightDeleteWorkspace = (projectPath: string, workspaceName: string) =>
  invoke<DeleteWorkspacePreflight>("preflight_delete_workspace", {
    projectPath,
    workspaceName,
  });
export const deleteWorkspace = (projectPath: string, workspaceName: string, force = false) =>
  invoke<DeleteWorkspaceResult>("delete_workspace", {
    projectPath,
    workspaceName,
    force,
  });
export const pruneWorktrees = (projectPath: string) =>
  invoke<PruneResult>("prune_worktrees", { projectPath });

// Processes
export const listProcesses = (projectPath: string, workspaceName?: string) =>
  invoke<ProcessStatus[]>("list_processes", { projectPath, workspaceName });
export const startProcesses = (
  projectPath: string,
  workspaceName: string | undefined,
  names: string[],
  force = false,
) =>
  invoke<ProcessOperationResponse>("start_processes", {
    projectPath,
    workspaceName,
    names,
    force,
  });
export const stopProcesses = (
  projectPath: string,
  workspaceName: string | undefined,
  names: string[],
) =>
  invoke<ProcessOperationResponse>("stop_processes", {
    projectPath,
    workspaceName,
    names,
  });
export const restartProcesses = (
  projectPath: string,
  workspaceName: string | undefined,
  names: string[],
) =>
  invoke<ProcessOperationResponse>("restart_processes", {
    projectPath,
    workspaceName,
    names,
  });
export const getProcessLogs = (
  projectPath: string,
  workspaceName: string,
  name: string,
  tail?: number,
) =>
  invoke<string>("get_process_logs", {
    projectPath,
    workspaceName,
    name,
    tail,
  });
export const forgetProcessRecord = (
  projectPath: string,
  workspaceName: string,
  name: string,
) =>
  invoke<boolean>("forget_process_record", {
    projectPath,
    workspaceName,
    name,
  });

// Services
export const addService = (projectPath: string, request: AddServiceRequest) =>
  invoke<ServiceEntry>("add_service", { projectPath, request });
export const listServices = (projectPath: string) =>
  invoke<ServiceEntry[]>("list_services", { projectPath });
export const startService = (
  projectPath: string,
  serviceName: string,
  workspaceName: string
) => invoke<void>("start_service", { projectPath, serviceName, workspaceName });
export const stopService = (
  projectPath: string,
  serviceName: string,
  workspaceName: string
) => invoke<void>("stop_service", { projectPath, serviceName, workspaceName });
export const runDoctor = (projectPath: string) =>
  invoke<DoctorReport>("run_doctor", { projectPath });
export const getServiceLogs = (
  projectPath: string,
  serviceName: string,
  workspaceName: string
) =>
  invoke<string>("get_service_logs", {
    projectPath,
    serviceName,
    workspaceName,
  });
export const resetService = (
  projectPath: string,
  serviceName: string,
  workspaceName: string
) =>
  invoke<void>("reset_service", { projectPath, serviceName, workspaceName });
export const listServiceWorkspaces = (
  projectPath: string,
  serviceName: string
) =>
  invoke<ServiceWorkspaceInfo[]>("list_service_workspaces", {
    projectPath,
    serviceName,
  });
export const deleteServiceWorkspace = (
  projectPath: string,
  serviceName: string,
  workspaceName: string
) =>
  invoke<void>("delete_service_workspace", {
    projectPath,
    serviceName,
    workspaceName,
  });
export const destroyService = (projectPath: string, serviceName: string) =>
  invoke<DestroyServiceResult>("destroy_service", {
    projectPath,
    serviceName,
  });
export const discoverDockerContainers = (
  serviceType?: string,
  options?: { projectPath?: string; global?: boolean }
) =>
  invoke<DiscoveredContainer[]>("discover_docker_containers", {
    serviceType,
    projectPath: options?.projectPath,
    global: options?.global,
  });
export const generateAgentGuides = (projectPath: string) =>
  invoke<string[]>("generate_agent_guides", { projectPath });

// Hooks
export const listHooks = (projectPath: string) =>
  invoke<HookPhaseEntry[]>("list_hooks", { projectPath });
export const renderTemplate = (
  projectPath: string,
  template: string,
  workspaceName?: string
) => invoke<string>("render_template", { projectPath, template, workspaceName });
export const getHookVariables = (
  projectPath: string,
  workspaceName?: string
) =>
  invoke<Record<string, unknown>>("get_hook_variables", {
    projectPath,
    workspaceName,
  });
export const installVcsHooks = (projectPath: string) =>
  invoke<VcsHooksActionResult>("install_vcs_hooks", { projectPath });
export const uninstallVcsHooks = (projectPath: string) =>
  invoke<VcsHooksActionResult>("uninstall_vcs_hooks", { projectPath });
// Proxy
export const startProxy = () => invoke<ProxyStatus>("start_proxy");
export const stopProxy = () => invoke<void>("stop_proxy");
export const getProxyStatus = () => invoke<ProxyStatus>("get_proxy_status");
export const listContainers = () =>
  invoke<ContainerEntry[]>("list_containers");
export const getCertificateStatus = () =>
  invoke<CertificateStatus>("get_certificate_status");

// Config
export const getConfigDocument = (projectPath: string) =>
  invoke<{path: string; format: string; content: string}>("get_config_document", {projectPath});
export const saveConfigYaml = (projectPath: string, content: string, original?: string) =>
  invoke<void>("save_config_yaml", { projectPath, content, original });
export const validateConfigYaml = (content: string, format = "yaml") =>
  invoke<{ valid: boolean; error?: string; services?: number; hooks?: number }>(
    "validate_config_yaml",
    { content, format }
  );

// Destroy
export const destroyProject = (projectPath: string) =>
  invoke<DestroyResult>("destroy_project", { projectPath });

// Orphan detection & cleanup
export const detectOrphanProjects = () =>
  invoke<OrphanProjectEntry[]>("detect_orphan_projects");
export const cleanupOrphanProject = (projectName: string) =>
  invoke<OrphanCleanupResult>("cleanup_orphan_project", { projectName });

// Settings
export const getSettings = () => invoke<AppSettings>("get_settings");
export const saveSettings = (settings: AppSettings) =>
  invoke<void>("save_settings", { settings });

// Terminal
export const createTerminal = (
  projectPath?: string,
  workspaceName?: string,
  initialCommand?: string
) =>
  invoke<TerminalSessionInfo>("create_terminal", {
    projectPath,
    workspaceName,
    initialCommand,
  });
export const listTerminals = () =>
  invoke<TerminalSessionInfo[]>("list_terminals");
export const writeTerminal = (sessionId: string, data: string) =>
  invoke<void>("write_terminal", { sessionId, data });
export const resizeTerminal = (
  sessionId: string,
  rows: number,
  cols: number
) => invoke<void>("resize_terminal", { sessionId, rows, cols });
export const closeTerminal = (sessionId: string) =>
  invoke<void>("close_terminal", { sessionId });

export const providerDescriptors = () => invoke<import("../types").ProviderDescriptor[]>("provider_descriptors");

export const listBaselines = (projectPath: string) => invoke<Array<{reference: string; service: string; status: string; engine_fingerprint: string | null}>>("list_baselines", {projectPath});
export const listOperations = (projectPath: string) => invoke<import("../types").WorkspaceOperation[]>("list_operations", {projectPath});
export const resumeOperation = (projectPath: string, id: string) => invoke<Record<string, unknown>>("resume_operation", {projectPath, id});
export const cancelOperation = (projectPath: string, id: string) => invoke<import("../types").WorkspaceOperation>("cancel_operation", {projectPath, id});
