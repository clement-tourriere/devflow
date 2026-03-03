export interface ProjectEntry {
  path: string;
  name: string;
}

export interface ProjectDetail {
  name: string;
  path: string;
  has_config: boolean;
  current_branch: string | null;
  service_count: number;
  branch_count: number;
  worktree_enabled: boolean;
  vcs_type: string | null;
}

export interface BranchEntry {
  name: string;
  is_current: boolean;
  is_default: boolean;
  worktree_path: string | null;
}

export interface BranchesResponse {
  branches: BranchEntry[];
  current: string | null;
}

export interface ServiceEntry {
  name: string;
  service_type: string;
  provider_type: string;
  auto_branch: boolean;
}

export interface ServiceBranchStatus {
  service_name: string;
  branch_name: string;
  state: string | null;
}

export interface ServiceBranchInfo {
  name: string;
  created_at: string | null;
  parent_branch: string | null;
  database_name: string;
  state: string | null;
}

export interface ConnectionInfo {
  host: string;
  port: number;
  database: string;
  user: string;
  password: string | null;
  connection_string: string | null;
}

export interface OrchestrationResult {
  service_name: string;
  success: boolean;
  message: string;
}

export interface CreateBranchResult {
  services: OrchestrationResult[];
  worktree_path: string | null;
}

export interface HookPhaseEntry {
  phase: string;
  hooks: HookInfo[];
}

export interface HookInfo {
  name: string;
  command: string;
  is_extended: boolean;
}

export interface ProxyStatus {
  running: boolean;
  https_port: number;
  http_port: number;
  ca_installed: boolean;
  ca_path: string;
}

export interface ContainerEntry {
  domain: string;
  container_name: string;
  container_ip: string;
  port: number;
  project: string | null;
  service: string | null;
  branch: string | null;
  https_url: string;
}

export interface CertificateStatus {
  exists: boolean;
  installed: boolean;
  path: string;
  info: string;
}

export interface AddServiceRequest {
  name: string;
  service_type: string;
  provider_type: string;
  auto_branch?: boolean;
  image?: string;
  seed_from?: string;
}

export interface AppSettings {
  projects: ProjectEntry[];
  proxy_auto_start: boolean;
  proxy_config: {
    https_port: number;
    http_port: number;
    api_port: number;
    domain_suffix: string;
  } | null;
  terminal_renderer: TerminalRenderer;
  terminal_font_size: number;
}

export type TerminalRenderer = "auto" | "webgpu" | "webgl2";

export interface DestroyResult {
  services_destroyed: ServiceDestroyResult[];
  worktrees_removed: number;
  hooks_uninstalled: boolean;
  config_deleted: boolean;
}

export interface ServiceDestroyResult {
  name: string;
  success: boolean;
  branches_destroyed: string[];
  error: string | null;
}

export interface OrphanProjectEntry {
  project_name: string;
  project_path: string | null;
  sources: string[];
  sqlite_project_id: string | null;
  sqlite_branch_count: number;
  container_names: string[];
  local_state_service_count: number;
  local_state_branch_count: number;
}

export interface OrphanCleanupResult {
  project_name: string;
  containers_removed: number;
  sqlite_rows_deleted: boolean;
  local_state_cleared: boolean;
  data_dirs_removed: number;
  errors: string[];
}

export interface VcsInfo {
  existing_vcs: string | null;
  available_tools: string[];
}

export interface TerminalSessionInfo {
  id: string;
  label: string;
  project_path: string | null;
  branch_name: string | null;
  service_name: string | null;
  working_directory: string;
  status: "Running" | "Exited";
}

export interface TerminalOutputEvent {
  session_id: string;
  data: string; // base64
}

export interface TerminalExitEvent {
  session_id: string;
}
