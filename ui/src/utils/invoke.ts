import { invoke } from "@tauri-apps/api/core";
import type {
  ProjectEntry,
  ProjectDetail,
  WorkspacesResponse,
  ServiceEntry,
  ServiceWorkspaceStatus,
  ServiceWorkspaceInfo,
  AddServiceRequest,
  DiscoveredContainer,
  DestroyServiceResult,
  AgentSkillsStatus,
  InstalledSkillInfo,
  SkillSearchResult,
  SkillSearchDetail,
  SkillDetail,
  UserSkillInfo,
  HookPhaseEntry,
  VcsHooksActionResult,
  ActionTypeInfo,
  HookRunResult,
  HookPreview,
  TriggerMapping,
  RecipeInfo,
  InstallRecipeResult,
  ProxyStatus,
  ContainerEntry,
  CertificateStatus,
  AppSettings,
  OrchestrationResult,
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
} from "../types";

// Projects
export const listProjects = () => invoke<ProjectEntry[]>("list_projects");
export const addProject = (path: string, name?: string) =>
  invoke<ProjectEntry>("add_project", { path, name });
export const removeProject = (path: string) =>
  invoke<void>("remove_project", { path });
export const getProjectDetail = (projectPath: string) =>
  invoke<ProjectDetail>("get_project_detail", { projectPath });
export const initProject = (path: string, name?: string, vcsPreference?: string, worktreeEnabled?: boolean) =>
  invoke<ProjectEntry>("init_project", { path, name, vcsPreference, worktreeEnabled });
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
  copyIgnored?: boolean,
  sandboxed?: boolean
) =>
  invoke<CreateWorkspaceResult>("create_workspace", {
    projectPath,
    workspaceName,
    fromWorkspace,
    creationMode,
    copyFiles,
    copyIgnored,
    sandboxed,
  });
export const switchWorkspace = (projectPath: string, workspaceName: string) =>
  invoke<SwitchWorkspaceResult>("switch_workspace", {
    projectPath,
    workspaceName,
  });
export const deleteWorkspace = (projectPath: string, workspaceName: string) =>
  invoke<OrchestrationResult[]>("delete_workspace", {
    projectPath,
    workspaceName,
  });
export const pruneWorktrees = (projectPath: string) =>
  invoke<PruneResult>("prune_worktrees", { projectPath });

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
export const getServiceStatus = (
  projectPath: string,
  serviceName: string,
  workspaceName: string
) =>
  invoke<ServiceWorkspaceStatus>("get_service_status", {
    projectPath,
    serviceName,
    workspaceName,
  });
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
export const uninstallAgentSkills = (projectPath: string) =>
  invoke<void>("uninstall_agent_skills", { projectPath });
export const checkAgentSkills = (projectPath: string) =>
  invoke<AgentSkillsStatus>("check_agent_skills", { projectPath });

// Skills management
export const skillList = (projectPath: string) =>
  invoke<InstalledSkillInfo[]>("skill_list", { projectPath });
export const skillSearch = (query: string, limit: number) =>
  invoke<SkillSearchResult[]>("skill_search", { query, limit });
export const skillSearchDetail = (source: string, name: string) =>
  invoke<SkillSearchDetail>("skill_search_detail", { source, name });
export const skillInstall = (projectPath: string, identifier: string) =>
  invoke<string[]>("skill_install", { projectPath, identifier });
export const skillRemove = (projectPath: string, name: string) =>
  invoke<void>("skill_remove", { projectPath, name });
export const skillUpdate = (projectPath: string, name?: string) =>
  invoke<string[]>("skill_update", { projectPath, name: name ?? null });
export const skillShow = (projectPath: string, name: string) =>
  invoke<SkillDetail>("skill_show", { projectPath, name });
export const skillCheckUpdates = (projectPath: string) =>
  invoke<string[]>("skill_check_updates", { projectPath });

// User-scope skills
export const userSkillList = () =>
  invoke<UserSkillInfo[]>("user_skill_list");
export const userSkillInstall = (identifier: string) =>
  invoke<string[]>("user_skill_install", { identifier });
export const userSkillRemove = (name: string) =>
  invoke<void>("user_skill_remove", { name });
export const userSkillUpdate = (name?: string) =>
  invoke<string[]>("user_skill_update", { name: name ?? null });
export const userSkillShow = (name: string) =>
  invoke<SkillDetail>("user_skill_show", { name });
export const userSkillCheckUpdates = () =>
  invoke<string[]>("user_skill_check_updates");

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
export const validateHook = (
  projectPath: string,
  hook: unknown,
  workspaceName?: string
) =>
  invoke<{ valid: boolean }>("validate_hook", {
    projectPath,
    hook,
    workspaceName,
  });
export const previewHook = (
  projectPath: string,
  hook: unknown,
  workspaceName?: string
) =>
  invoke<HookPreview>("preview_hook", {
    projectPath,
    hook,
    workspaceName,
  });
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
export const installRecipe = (projectPath: string, recipeName: string) =>
  invoke<InstallRecipeResult>("install_recipe", { projectPath, recipeName });
export const installRecipes = (projectPath: string, recipeNames: string[]) =>
  invoke<InstallRecipeResult>("install_recipes", { projectPath, recipeNames });

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
  workspaceName?: string
) =>
  invoke<TerminalSessionInfo>("create_terminal", {
    projectPath,
    workspaceName,
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

// Merge & Train
export const mergeCheck = (
  projectPath: string,
  source: string,
  target?: string
) =>
  invoke<import("../types").MergeReadinessReport>("merge_check", {
    projectPath,
    source,
    target,
  });

export const mergeWorkspace = (
  projectPath: string,
  source: string,
  target?: string,
  cleanup?: boolean
) =>
  invoke<import("../types").MergeResult>("merge_workspace", {
    projectPath,
    source,
    target,
    cleanup,
  });

export const rebaseWorkspace = (
  projectPath: string,
  workspace: string,
  target?: string
) =>
  invoke<import("../types").RebaseResult>("rebase_workspace", {
    projectPath,
    workspace,
    target,
  });

export const trainAdd = (
  projectPath: string,
  workspace?: string,
  target?: string
) => invoke<void>("train_add", { projectPath, workspace, target });

export const trainRemove = (projectPath: string, workspace: string) =>
  invoke<void>("train_remove", { projectPath, workspace });

export const trainStatus = (projectPath: string, target?: string) =>
  invoke<import("../types").MergeTrain | null>("train_status", {
    projectPath,
    target,
  });

export const trainRun = (
  projectPath: string,
  target?: string,
  stopOnFailure?: boolean,
  cleanup?: boolean
) =>
  invoke<import("../types").MergeTrainEntry[]>("train_run", {
    projectPath,
    target,
    stopOnFailure,
    cleanup,
  });

export const trainPause = (projectPath: string, target?: string) =>
  invoke<void>("train_pause", { projectPath, target });

export const trainResume = (projectPath: string, target?: string) =>
  invoke<void>("train_resume", { projectPath, target });
