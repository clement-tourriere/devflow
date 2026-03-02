import { invoke } from "@tauri-apps/api/core";
import type {
  ProjectEntry,
  ProjectDetail,
  BranchesResponse,
  ServiceEntry,
  ServiceBranchStatus,
  AddServiceRequest,
  HookPhaseEntry,
  ProxyStatus,
  ContainerEntry,
  CertificateStatus,
  AppSettings,
  OrchestrationResult,
  CreateBranchResult,
  DestroyResult,
  OrphanProjectEntry,
  OrphanCleanupResult,
} from "../types";

// Projects
export const listProjects = () => invoke<ProjectEntry[]>("list_projects");
export const addProject = (path: string, name?: string) =>
  invoke<ProjectEntry>("add_project", { path, name });
export const removeProject = (path: string) =>
  invoke<void>("remove_project", { path });
export const getProjectDetail = (projectPath: string) =>
  invoke<ProjectDetail>("get_project_detail", { projectPath });
export const initProject = (path: string, name?: string) =>
  invoke<ProjectEntry>("init_project", { path, name });

// Branches
export const listBranches = (projectPath: string) =>
  invoke<BranchesResponse>("list_branches", { projectPath });
export const getConnectionInfo = (
  projectPath: string,
  branchName: string,
  serviceName?: string
) =>
  invoke<Record<string, unknown>>("get_connection_info", {
    projectPath,
    branchName,
    serviceName,
  });
export const createBranch = (
  projectPath: string,
  branchName: string,
  fromBranch?: string
) =>
  invoke<CreateBranchResult>("create_branch", {
    projectPath,
    branchName,
    fromBranch,
  });
export const deleteBranch = (projectPath: string, branchName: string) =>
  invoke<OrchestrationResult[]>("delete_branch", {
    projectPath,
    branchName,
  });

// Services
export const addService = (projectPath: string, request: AddServiceRequest) =>
  invoke<ServiceEntry>("add_service", { projectPath, request });
export const listServices = (projectPath: string) =>
  invoke<ServiceEntry[]>("list_services", { projectPath });
export const startService = (
  projectPath: string,
  serviceName: string,
  branchName: string
) => invoke<void>("start_service", { projectPath, serviceName, branchName });
export const stopService = (
  projectPath: string,
  serviceName: string,
  branchName: string
) => invoke<void>("stop_service", { projectPath, serviceName, branchName });
export const runDoctor = (projectPath: string) =>
  invoke<unknown[]>("run_doctor", { projectPath });
export const getServiceLogs = (
  projectPath: string,
  serviceName: string,
  branchName: string
) =>
  invoke<string>("get_service_logs", {
    projectPath,
    serviceName,
    branchName,
  });
export const resetService = (
  projectPath: string,
  serviceName: string,
  branchName: string
) =>
  invoke<void>("reset_service", { projectPath, serviceName, branchName });
export const getServiceStatus = (
  projectPath: string,
  serviceName: string,
  branchName: string
) =>
  invoke<ServiceBranchStatus>("get_service_status", {
    projectPath,
    serviceName,
    branchName,
  });

// Hooks
export const listHooks = (projectPath: string) =>
  invoke<HookPhaseEntry[]>("list_hooks", { projectPath });
export const renderTemplate = (
  projectPath: string,
  template: string,
  branchName?: string
) => invoke<string>("render_template", { projectPath, template, branchName });
export const getHookVariables = (
  projectPath: string,
  branchName?: string
) =>
  invoke<Record<string, unknown>>("get_hook_variables", {
    projectPath,
    branchName,
  });

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
