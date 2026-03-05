export type { DevflowConfig } from "./config";

export interface ProjectEntry {
  path: string;
  name: string;
}

export interface ProjectDetail {
  name: string;
  path: string;
  has_config: boolean;
  current_workspace: string | null;
  service_count: number;
  workspace_count: number;
  hook_count: number;
  worktree_enabled: boolean;
  worktree_copy_files: string[];
  worktree_copy_ignored: boolean;
  vcs_type: string | null;
}

export interface WorkspaceEntry {
  name: string;
  is_current: boolean;
  is_default: boolean;
  worktree_path: string | null;
  parent: string | null;
  created_at: string | null;
  agent_tool: string | null;
  agent_status: string | null;
}

export interface WorkspacesResponse {
  workspaces: WorkspaceEntry[];
  current: string | null;
}

export interface ServiceEntry {
  name: string;
  service_type: string;
  provider_type: string;
  auto_workspace: boolean;
}

export interface ServiceWorkspaceStatus {
  service_name: string;
  workspace_name: string;
  state: string | null;
}

export interface ServiceWorkspaceInfo {
  name: string;
  created_at: string | null;
  parent_workspace: string | null;
  database_name: string;
  state: string | null;
}

export interface PruneResult {
  pruned: number;
  details: string[];
}

export interface DoctorCheck {
  name: string;
  available: boolean;
  detail: string;
}

export interface DoctorServiceReport {
  service: string;
  checks: DoctorCheck[];
}

export interface DoctorReport {
  general: DoctorCheck[];
  services: DoctorServiceReport[];
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

export interface CreateWorkspaceResult {
  services: OrchestrationResult[];
  worktree_path: string | null;
}

export type WorkspaceCreationMode = "default" | "worktree" | "branch";

export interface HookPhaseEntry {
  phase: string;
  hooks: HookInfo[];
}

export interface HookInfo {
  name: string;
  command: string;
  is_extended: boolean;
  action_type?: string;
  condition?: string;
  background: boolean;
  raw: unknown;
}

export interface VcsHooksActionResult {
  installed: boolean;
  detail: string;
}

export interface ActionTypeInfo {
  type: string;
  label: string;
  description: string;
  requires_approval: boolean;
  fields: ActionFieldInfo[];
}

export interface ActionFieldInfo {
  name: string;
  label: string;
  field_type: string; // "string" | "text" | "bool" | "select" | "key-value"
  required: boolean;
  default_value?: string;
  options?: string[];
  template: boolean;
}

export interface HookRunResult {
  succeeded: number;
  failed: number;
  skipped: number;
  background: number;
  errors: string[];
}

export interface HookPreview {
  type: string;
  rendered_command?: string;
  action_type?: string;
  requires_approval?: boolean;
}

export interface TriggerMapping {
  vcs_event: string;
  phases: string[];
}

export interface RecipeInfo {
  name: string;
  description: string;
  category: string;
  hooks_preview: RecipeHookPreview[];
}

export interface RecipeHookPreview {
  phase: string;
  hook_name: string;
  command_summary: string;
}

export interface InstallRecipeResult {
  hooks_added: number;
  hooks_skipped: number;
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
  workspace: string | null;
  https_url: string;
}

export interface CertificateStatus {
  exists: boolean;
  installed: boolean;
  path: string;
  info: string;
}

export interface DiscoveredContainer {
  container_id: string;
  container_name: string;
  image: string;
  service_type: string;
  host: string;
  port: number;
  username: string | null;
  password: string | null;
  database: string | null;
  connection_url: string;
  is_compose: boolean;
  compose_project: string | null;
  compose_service: string | null;
}

export interface AddServiceRequest {
  name: string;
  service_type: string;
  provider_type: string;
  auto_workspace?: boolean;
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
  workspaces_destroyed: string[];
  error: string | null;
}

export interface DestroyServiceResult {
  service_name: string;
  destroyed_workspaces: string[];
}

export interface OrphanProjectEntry {
  project_name: string;
  project_path: string | null;
  sources: string[];
  sqlite_project_id: string | null;
  sqlite_workspace_count: number;
  container_names: string[];
  local_state_service_count: number;
  local_state_workspace_count: number;
}

export interface OrphanCleanupResult {
  project_name: string;
  containers_removed: number;
  sqlite_rows_deleted: boolean;
  local_state_cleared: boolean;
  data_dirs_removed: number;
  errors: string[];
}

export interface AgentSkillsStatus {
  installed: boolean;
  installed_skills: string[];
  missing_skills: string[];
  update_available: boolean;
  stale_skills: string[];
}

export interface VcsInfo {
  existing_vcs: string | null;
  available_tools: string[];
}

export interface TerminalSessionInfo {
  id: string;
  label: string;
  project_path: string | null;
  workspace_name: string | null;
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
