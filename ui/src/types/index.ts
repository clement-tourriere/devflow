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
  worktree_copy_files: string[];
  worktree_copy_ignored: boolean;
  vcs_type: string | null;
}

export interface WorkspaceInventoryProject {
  name: string;
  root: string;
  vcs_provider: string;
}

export interface WorkspaceServiceStatus {
  name: string;
  provider: string;
  state: string | null;
  database_name: string | null;
  parent_workspace: string | null;
  provisioned: boolean;
  supports_lifecycle: boolean;
}

export interface WorkspaceEntry {
  name: string;
  service_key: string;
  canonical_service_key: string;
  identity_status: "canonical" | "legacy_adopted" | "legacy_unresolved";
  parent: string | null;
  parent_state: string | null;
  children: string[];
  is_default: boolean;
  is_context: boolean;
  worktree_path: string | null;
  health: string;
  created_at: string;
  executed_command: string | null;
  execution_status: string | null;
  services: WorkspaceServiceStatus[];
  processes: ProcessStatus[];
}

export interface WorkspacesResponse {
  schema_version: number;
  project: WorkspaceInventoryProject;
  context_workspace: string | null;
  default_workspace: string;
  roots: string[];
  workspaces: WorkspaceEntry[];
  warnings: string[];
}

export interface ServiceEntry {
  name: string;
  service_type: string;
  provider_type: string;
  auto_workspace: boolean;
  source: "config" | "local_state" | string;
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

export interface ProcessResult {
  process: string;
  success: boolean;
  message: string;
  required: boolean;
  pid?: number;
  ports?: number[];
}

export interface ProcessStatus {
  process: string;
  workspace: string;
  pid: number | null;
  status: string;
  required: boolean;
  ports: number[];
  urls: string[];
  command: string;
  workdir: string;
  log_path: string;
  retry_count: number;
  last_error?: string | null;
  started_at?: string | null;
  desired_state?: string | null;
  runtime?: string | null;
  pitchfork_id?: string | null;
  configured: boolean;
  source: "config" | "config+runtime" | "runtime_state" | string;
}

export interface ProcessOperationResponse {
  workspace: string;
  results: ProcessResult[];
}

export interface PitchforkBridgeInfo {
  provider: string;
  enabled: boolean;
  web_ui_enabled: boolean;
  web_ui_url: string;
  web_ui_reachable: boolean;
  cli_available: boolean;
  config_policy: string;
  external_daemons: string;
  edit_mode: string;
}

export interface CreateWorkspaceResult {
  workspace: string;
  service_key: string;
  parent: string | null;
  vcs_ref_created: boolean;
  services: OrchestrationResult[];
  processes: ProcessResult[];
  worktree_path: string | null;
  hooks: HookRunResult[];
}

export interface SwitchWorkspaceResult {
  workspace: string;
  service_key: string;
  parent: string | null;
  worktree_path: string | null;
  vcs_ref_created: boolean;
  services: OrchestrationResult[];
  processes: ProcessResult[];
  hooks: HookRunResult[];
}

export interface DeleteWorkspaceResult {
  workspace: string;
  service_key: string;
  worktree_removed: boolean;
  worktree_path: string | null;
  vcs_ref_deleted: boolean;
  services: OrchestrationResult[];
  processes: ProcessResult[];
  hooks: HookRunResult[];
}

export interface DeleteWorkspacePreflightIssue {
  code: string;
  message: string;
  force_overridable: boolean;
}

export interface DeleteWorkspacePreflight {
  workspace: string;
  service_key: string;
  worktree_path: string | null;
  vcs_ref_exists: boolean;
  issues: DeleteWorkspacePreflightIssue[];
}

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
  // Present on lifecycle results (HookRunResultDto); absent on `run_hook`'s
  // hand-built payload, which reuses this shape without a phase.
  phase?: string;
  succeeded: number;
  failed: number;
  skipped: number;
  background: number;
  errors: string[];
}

export interface TriggerMapping {
  vcs_event: string;
  phases: string[];
}

export interface RecipeParamInfo {
  key: string;
  label: string;
  help: string;
  kind: "string" | "text" | "bool";
  default?: string | null;
  required: boolean;
}

export interface RecipeInfo {
  name: string;
  description: string;
  category: string;
  phases: string[];
  repeatable: boolean;
  params: RecipeParamInfo[];
}

export interface RecipeDetectionInfo {
  recipe: RecipeInfo;
  applicable: boolean;
  suggested: boolean;
  installed: boolean;
  reasons: string[];
  suggested_params: Record<string, string>;
  param_options: Record<string, string[]>;
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
  source: "devflow-service" | "docker-compose" | "standalone-container" | string;
  /** Reachable endpoint: `https://<domain>` for web, `postgresql://<domain>:5432` etc. for databases. */
  endpoint_url: string;
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
  command: string[];
  extra_env: Record<string, string>;
  restart_policy: string | null;
}

export interface AddServiceRequest {
  name: string;
  service_type: string;
  provider_type: string;
  auto_workspace?: boolean;
  image?: string;
  seed_from?: string;
  docker_command?: string[];
  docker_environment?: Record<string, string>;
  docker_restart_policy?: string;
}

export interface AppSettings {
  projects: ProjectEntry[];
  proxy_auto_start: boolean;
  proxy_config: {
    https_port: number;
    http_port: number;
    api_port: number;
    domain_suffix: string;
    auto_network?: boolean;
    mdns?: boolean;
    bind_address?: string;
  } | null;
  terminal_renderer: TerminalRenderer;
  terminal_font_size: number;
}

export type TerminalRenderer = "auto" | "webgpu" | "webgl2";

export interface DestroyResult {
  project_name: string;
  processes_stopped: number;
  process_results: ProcessResult[];
  services_destroyed: ServiceDestroyResult[];
  worktrees_removed: number;
  hooks_uninstalled: boolean;
  state_cleared: boolean;
  config_deleted: boolean;
  local_config_deleted: boolean;
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

export interface VcsInfo {
  existing_vcs: string | null;
  available_tools: string[];
}

export interface VcsWorkspaceInfo {
  workspaces: string[];
  default_workspace: string | null;
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
