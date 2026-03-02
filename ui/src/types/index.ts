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
}

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
