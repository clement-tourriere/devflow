// TypeScript interfaces mirroring the Rust Config structs in devflow-core

export type VcsKind = "git" | "jj";

export interface DevflowConfig {
  name?: string | null;
  default_vcs?: VcsKind | null;
  git?: GitConfig;
  behavior?: BehaviorConfig;
  services?: NamedServiceConfig[] | null;
  worktree?: WorktreeConfig;
  processes?: ProcessesConfig | null;
  hooks?: Record<string, Record<string, unknown>> | null;
  triggers?: Record<string, unknown> | null;
  execute?: ExecuteConfig | null;
  agent?: AgentConfig | null;
  commit?: CommitConfig | null;
}

export interface GitConfig {
  auto_create_on_workspace: boolean;
  main_workspace: string;
  workspace_filter_regex?: string | null;
  exclude_workspaces: string[];
}

export interface BehaviorConfig {
  max_workspaces?: number | null;
}

export interface WorktreeConfig {
  path_template: string;
  copy_files: string[];
  copy_ignored: boolean;
  respect_gitignore: boolean;
  copy_ai_configs: boolean;
  extra_ai_dirs: string[];
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

export type ProcessProvider = "native" | "pitchfork";
export type PitchforkConfigPolicy = "devflow-owned" | "import" | "mirror" | "merge";
export type PitchforkExternalDaemons = "hide" | "show" | "importable";
export type PitchforkWebEditMode = "readonly" | "warn" | "merge";
export type ProcessPortBump = boolean | number;
export type ProcessPortConfig =
  | number
  | number[]
  | {
      expect: number[];
      bump?: ProcessPortBump;
    };

export interface PitchforkWebUiConfig {
  enabled: boolean;
  bind_port?: number | null;
  bind_address?: string | null;
  edit_mode: PitchforkWebEditMode;
}

export interface PitchforkProcessConfig {
  config_policy: PitchforkConfigPolicy;
  external_daemons: PitchforkExternalDaemons;
  web_ui?: PitchforkWebUiConfig | null;
}

export interface ProcessesConfig {
  provider: ProcessProvider;
  auto_start: boolean;
  auto_stop: boolean;
  pitchfork?: PitchforkProcessConfig | null;
  daemons: Record<string, ProcessDaemonConfig>;
}

export interface ProcessDaemonConfig {
  run: string;
  dir?: string | null;
  env?: Record<string, string>;
  required?: boolean;
  depends?: string[];
  port?: ProcessPortConfig | null;
  ready_delay?: number | null;
  ready_port?: number | null;
  ready_http?: string | null;
  ready_cmd?: string | null;
  ready_output?: string | null;
  ready_timeout?: number | null;
  stop_timeout?: number | null;
  shutdown_signal?: string | null;
  watch?: string[];
  retry?: number | null;
}

export interface ExecuteConfig {
  detach_command?: string | null;
  multiplexer?: string | null;
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
  main_workspace: "main",
  workspace_filter_regex: null,
  exclude_workspaces: ["main", "master"],
};

const DEFAULT_BEHAVIOR: BehaviorConfig = {
  max_workspaces: 10,
};

const DEFAULT_WORKTREE: WorktreeConfig = {
  path_template: "../{repo}.{workspace}",
  copy_files: [".env", ".env.local"],
  copy_ignored: false,
  respect_gitignore: true,
  copy_ai_configs: true,
  extra_ai_dirs: [],
};

/** DevflowConfig with all required fields filled in */
export type FilledConfig = DevflowConfig & {
  git: GitConfig;
  behavior: BehaviorConfig;
  worktree: WorktreeConfig;
};

/** Fill in defaults for fields that serde may omit via skip_serializing_if */
export function withDefaults(cfg: DevflowConfig): FilledConfig {
  return {
    ...cfg,
    git: cfg.git ? { ...DEFAULT_GIT, ...cfg.git } : { ...DEFAULT_GIT },
    behavior: cfg.behavior
      ? { ...DEFAULT_BEHAVIOR, ...cfg.behavior }
      : { ...DEFAULT_BEHAVIOR },
    worktree: cfg.worktree
      ? { ...DEFAULT_WORKTREE, ...cfg.worktree }
      : { ...DEFAULT_WORKTREE },
  };
}
