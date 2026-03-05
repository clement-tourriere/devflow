// TypeScript interfaces mirroring the Rust Config structs in devflow-core

export type VcsKind = "git" | "jj";

export interface DevflowConfig {
  name?: string | null;
  default_vcs?: VcsKind | null;
  git?: GitConfig;
  behavior?: BehaviorConfig;
  services?: NamedServiceConfig[] | null;
  worktree?: WorktreeConfig | null;
  hooks?: Record<string, Record<string, unknown>> | null;
  triggers?: Record<string, unknown> | null;
  agent?: AgentConfig | null;
  commit?: CommitConfig | null;
}

export interface GitConfig {
  auto_create_on_workspace: boolean;
  auto_switch_on_workspace: boolean;
  main_workspace: string;
  auto_create_workspace_filter?: string | null;
  workspace_filter_regex?: string | null;
  exclude_workspaces: string[];
}

export interface BehaviorConfig {
  max_workspaces?: number | null;
}

export interface WorktreeConfig {
  enabled: boolean;
  path_template: string;
  copy_files: string[];
  copy_ignored: boolean;
  respect_gitignore: boolean;
}

export interface NamedServiceConfig {
  name: string;
  /** Serialized as "type" in YAML but "provider_type" in JSON due to #[serde(rename)] */
  type: string;
  service_type: string;
  auto_workspace: boolean;
  default: boolean;
  local?: LocalServiceConfig | null;
  neon?: NeonConfig | null;
  dblab?: DBLabConfig | null;
  xata?: XataConfig | null;
  clickhouse?: ClickHouseConfig | null;
  mysql?: MySQLConfig | null;
  generic?: GenericDockerConfig | null;
  plugin?: PluginConfig | null;
}

export interface LocalServiceConfig {
  image?: string | null;
  data_root?: string | null;
  storage?: string | null;
  port_range_start?: number | null;
  postgres_user?: string | null;
  postgres_password?: string | null;
  postgres_db?: string | null;
}

export interface NeonConfig {
  api_key: string;
  project_id: string;
  base_url: string;
}

export interface DBLabConfig {
  api_url: string;
  auth_token: string;
}

export interface XataConfig {
  api_key: string;
  organization_id: string;
  project_id: string;
  base_url: string;
}

export interface ClickHouseConfig {
  image: string;
  port_range_start?: number | null;
  data_root?: string | null;
  user: string;
  password?: string | null;
}

export interface MySQLConfig {
  image: string;
  port_range_start?: number | null;
  data_root?: string | null;
  root_password: string;
  database?: string | null;
  user?: string | null;
  password?: string | null;
}

export interface GenericDockerConfig {
  image: string;
  port_mapping?: string | null;
  port_range_start?: number | null;
  environment: Record<string, string>;
  volumes: string[];
  command?: string | null;
  healthcheck?: string | null;
}

export interface PluginConfig {
  path?: string | null;
  name?: string | null;
  timeout: number;
  config?: unknown | null;
}

export interface AgentConfig {
  auto_context: boolean;
}

export interface CommitConfig {
  generation?: CommitGenerationConfig | null;
}

export interface CommitGenerationConfig {
  command?: string | null;
  api_key?: string | null;
  api_url?: string | null;
  model?: string | null;
}

// Defaults matching Rust Default impls — used to fill fields omitted by skip_serializing_if

const DEFAULT_GIT: GitConfig = {
  auto_create_on_workspace: true,
  auto_switch_on_workspace: true,
  main_workspace: "main",
  auto_create_workspace_filter: null,
  workspace_filter_regex: null,
  exclude_workspaces: ["main", "master"],
};

const DEFAULT_BEHAVIOR: BehaviorConfig = {
  max_workspaces: 10,
};

/** DevflowConfig with all required fields filled in */
export type FilledConfig = DevflowConfig & {
  git: GitConfig;
  behavior: BehaviorConfig;
};

/** Fill in defaults for fields that serde may omit via skip_serializing_if */
export function withDefaults(cfg: DevflowConfig): FilledConfig {
  return {
    ...cfg,
    git: cfg.git ? { ...DEFAULT_GIT, ...cfg.git } : { ...DEFAULT_GIT },
    behavior: cfg.behavior
      ? { ...DEFAULT_BEHAVIOR, ...cfg.behavior }
      : { ...DEFAULT_BEHAVIOR },
  };
}
