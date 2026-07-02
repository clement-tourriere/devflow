import { invoke } from "@tauri-apps/api/core";
import type {
  ProjectEntry,
  ProjectDetail,
  WorkspacesResponse,
  ServiceEntry,
  ServiceWorkspaceInfo,
  AddServiceRequest,
  DiscoveredContainer,
  DestroyServiceResult,
  HookPhaseEntry,
  VcsHooksActionResult,
  ActionTypeInfo,
  HookRunResult,
  TriggerMapping,
  RecipeInfo,
  RecipeDetectionInfo,
  RecipeHookPreview,
  InstallRecipeResult,
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
  GitBranchInfo,
  TerminalSessionInfo,
  WorkspaceCreationMode,
  PruneResult,
  SwitchWorkspaceResult,
  DeleteWorkspaceResult,
  ProcessStatus,
  ProcessOperationResponse,
  PitchforkBridgeInfo,
} from "../types";

// Projects
export const listProjects = () => invoke<ProjectEntry[]>("list_projects");
export const removeProject = (path: string) =>
  invoke<void>("remove_project", { path });
export const getProjectDetail = (projectPath: string) =>
  invoke<ProjectDetail>("get_project_detail", { projectPath });
export const addOrInitProject = (path: string, name?: string, vcsPreference?: string, worktreeEnabled?: boolean, mainBranch?: string) =>
  invoke<ProjectEntry>("add_or_init_project", { path, name, vcsPreference, worktreeEnabled, mainBranch });

// VCS
export const detectVcsInfo = (path: string) =>
  invoke<VcsInfo>("detect_vcs_info", { path });
export const detectGitBranches = (path: string) =>
  invoke<GitBranchInfo>("detect_git_branches", { path });

// Workspaces
export const listWorkspaces = (projectPath: string) =>
  invoke<WorkspacesResponse>("list_workspaces", { projectPath });
export const getConnectionInfo = (
  projectPath: string,
  workspaceName: string,
  serviceName?: string
) =>
  invoke<Record<string, unknown>>("get_connection_info", {
    projectPath,
    workspaceName,
    serviceName,
  });
export const createWorkspace = (
  projectPath: string,
  workspaceName: string,
  fromWorkspace?: string,
  creationMode?: WorkspaceCreationMode,
  copyFiles?: string[],
  copyIgnored?: boolean
) =>
  invoke<CreateWorkspaceResult>("create_workspace", {
    projectPath,
    workspaceName,
    fromWorkspace,
    creationMode,
    copyFiles,
    copyIgnored,
  });
export const switchWorkspace = (projectPath: string, workspaceName: string) =>
  invoke<SwitchWorkspaceResult>("switch_workspace", {
    projectPath,
    workspaceName,
  });
export const deleteWorkspace = (projectPath: string, workspaceName: string) =>
  invoke<DeleteWorkspaceResult>("delete_workspace", {
    projectPath,
    workspaceName,
  });
export const pruneWorktrees = (projectPath: string) =>
  invoke<PruneResult>("prune_worktrees", { projectPath });

// Processes
export const getPitchforkBridgeInfo = (projectPath: string) =>
  invoke<PitchforkBridgeInfo>("get_pitchfork_bridge_info", { projectPath });
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
export const installAgentSkills = (projectPath: string) =>
  invoke<string[]>("install_agent_skills", { projectPath });

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
export const getActionTypes = () =>
  invoke<ActionTypeInfo[]>("get_action_types");
export const saveHooks = (projectPath: string, hooks: unknown) =>
  invoke<void>("save_hooks", { projectPath, hooks });
export const runHook = (
  projectPath: string,
  phase: string,
  hookName: string,
  workspaceName?: string
) =>
  invoke<HookRunResult>("run_hook", {
    projectPath,
    phase,
    hookName,
    workspaceName,
  });
export const getTriggerMappings = (projectPath: string) =>
  invoke<TriggerMapping[]>("get_trigger_mappings", { projectPath });
export const getRecipes = () =>
  invoke<RecipeInfo[]>("get_recipes");
export const detectRecipes = (projectPath: string) =>
  invoke<RecipeDetectionInfo[]>("detect_recipes", { projectPath });
export const previewRecipe = (
  projectPath: string,
  recipeName: string,
  params: Record<string, string>
) =>
  invoke<RecipeHookPreview[]>("preview_recipe", {
    projectPath,
    recipeName,
    params,
  });
export const installRecipe = (
  projectPath: string,
  recipeName: string,
  params: Record<string, string>
) =>
  invoke<InstallRecipeResult>("install_recipe", {
    projectPath,
    recipeName,
    params,
  });
export const installRecipes = (
  projectPath: string,
  selections: { name: string; params: Record<string, string> }[]
) => invoke<InstallRecipeResult>("install_recipes", { projectPath, selections });

// Proxy
export const startProxy = () => invoke<ProxyStatus>("start_proxy");
export const stopProxy = () => invoke<void>("stop_proxy");
export const getProxyStatus = () => invoke<ProxyStatus>("get_proxy_status");
export const listContainers = () =>
  invoke<ContainerEntry[]>("list_containers");
export const getCertificateStatus = () =>
  invoke<CertificateStatus>("get_certificate_status");
export const installCertificate = () => invoke<void>("install_certificate");
export const removeCertificate = () => invoke<void>("remove_certificate");

// Config
export const getConfigJson = (projectPath: string) =>
  invoke<import("../types/config").DevflowConfig>("get_config_json", {
    projectPath,
  });
export const saveConfigJson = (
  projectPath: string,
  config: import("../types/config").DevflowConfig
) => invoke<void>("save_config_json", { projectPath, config });
export const getConfigYaml = (projectPath: string) =>
  invoke<string>("get_config_yaml", { projectPath });
export const saveConfigYaml = (projectPath: string, content: string) =>
  invoke<void>("save_config_yaml", { projectPath, content });
export const validateConfigYaml = (content: string) =>
  invoke<{ valid: boolean; error?: string }>("validate_config_yaml", {
    content,
  });

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
