---
title: Configuration
description: The complete .devflow.yml schema — VCS, behavior, services, processes, worktrees, hooks, execute, commit, and agent settings.
sidebar:
  order: 2
---

devflow is configured by a committed file plus environment variables. All sections are optional — an empty `.devflow.yml` is valid.

**File formats**: `.devflow.yml` / `.devflow.yaml` (full-featured YAML) or a lightweight `devflow.toml` / `.devflow.toml` — both parse into the same schema. `devflow init` and the GUI currently write YAML.

## Hierarchy

Merged from three sources, highest precedence first:

| Priority | Source | Purpose |
| --- | --- | --- |
| 1 | [Environment variables](/devflow/reference/environment/) | quick toggles, CI overrides, secrets |
| 2 | `.devflow.local.yml` | machine-specific overrides (gitignore it) |
| 3 | `.devflow.yml` / `devflow.toml` | team-shared config (committed) |

`devflow config -v` shows the effective config with per-value provenance. Machine-local *state* (registered workspaces, parents, worktree paths) lives separately in `~/.config/devflow/local_state.yml`.

## `name`

```yaml
name: my-project     # optional; defaults to the main repo root's directory name
```

Used as `{repo}` in worktree templates, in container/database naming, and as the proxy project name.

## `git`

```yaml
git:
  auto_create_on_workspace: true        # provision services for manually added worktrees
  main_workspace: main                  # auto-detected on init
  workspace_filter_regex: "^feature/.*" # only branches matching this pattern
  exclude_workspaces: [main, master]    # never provision these (supports * globs)
```

For Git, the branch checked out in the physical primary checkout is the default workspace; initialization refuses a detached primary checkout. This keeps the configured root aligned with Git's actual worktree graph instead of guessing from `origin/HEAD` or branch-name conventions.

These toggles apply to installed VCS-hook handling for materialized worktrees. Ordinary in-place `git checkout` no longer provisions devflow environments; use `devflow switch`, or create a linked worktree manually and let the hook adopt it.

### Opt-in provisioning with a branch marker

By default every adopted worktree gets services. To make hook-driven provisioning opt-in instead, set a marker pattern:

```yaml
git:
  workspace_filter_regex: "df_"   # unanchored regex — matches anywhere in the branch name
```

Now `git worktree add ../repo.df_login df_login` provisions databases, while a scratch worktree like `quickfix` is adopted without services (worktree files are still copied). The filter only gates the *automatic* path: explicit commands — `devflow switch -c <branch>`, `devflow service create` — always provision, whatever the branch is called.

The pattern is a search regex matched against the raw branch name, so anchor it (`^df/`) if the marker must be a prefix. An invalid regex fails closed — nothing auto-provisions — and `devflow doctor` reports it. `workspace_filter_regex` also accepts the legacy spellings `branch_filter_regex`, `auto_create_workspace_filter`, and `auto_create_branch_filter`.

## `behavior`

```yaml
behavior:
  max_workspaces: 10      # default retention for `devflow service cleanup`
```

## `services`

An array of named services; each picks a provider (`type`) and an engine (`service_type`). Common fields:

```yaml
services:
  - name: app-db          # unique name (used by -s and {{ service['app-db'] }})
    type: local           # local | shared | neon | dblab | xata
    service_type: postgres # postgres | clickhouse | mysql | redis | rustfs | generic | plugin
    auto_workspace: true  # follow git branching (default true)
    default: true         # default target when -s is omitted
```

### `local:` (PostgreSQL via CoW containers)

```yaml
    local:
      image: postgres:17
```

### `clickhouse:` / `mysql:` (CoW containers)

```yaml
    clickhouse:
      image: clickhouse/clickhouse-server:latest
      port_range_start: 59000          # HTTP port (native = HTTP + 877)
      data_root: ~/.local/share/devflow
      user: default
      password: ""
    mysql:
      image: mysql:8
      port_range_start: 53306
      data_root: ~/.local/share/devflow
      root_password: dev
      database: myapp
      user: dev
      password: dev
```

### `generic:` (any Docker image)

```yaml
    generic:
      image: redis:7-alpine
      port_mapping: "6379:6379"        # fixed mapping, or:
      port_range_start: 56000          # dynamic allocation
      environment: { KEY: value }
      volumes: ["/data/redis:/data"]
      command: "redis-server --save 60 1"
      healthcheck: "redis-cli ping"
```

### `shared:` (one global engine, logical isolation)

```yaml
  - name: app-db
    type: shared
    service_type: postgres             # postgres | clickhouse (redis/rustfs imply shared)
    shared:
      image: postgres:17               # engine image
      port: 5432                       # fixed well-known port
      template_branching: true         # postgres only: CREATE DATABASE … TEMPLATE parent
      user: rustfsadmin                # rustfs: access key
      password: rustfsadmin            # rustfs: secret key
```

Redis is always shared (`service_type: redis`, default `redis:7`, port 6379, a DB index 0–15 per workspace). RustFS (`service_type: rustfs`, aliases `s3`/`objectstorage`) serves S3 on 9000 with a bucket per workspace.

## `processes`

Workspace-scoped project processes — app servers, frontend dev servers, background workers, schedulers — run directly on the machine without Docker. Process env values use the same MiniJinja context as hooks, so service connection URLs are available as `{{ service['name'].url }}`.

```yaml
processes:
  provider: native       # native (default) or pitchfork (direct Rust supervisor embedding)
  auto_start: true       # start after devflow switch aligns services and hooks
  auto_stop: true        # stop before devflow remove deletes the workspace
  # Optional when provider: pitchfork
  pitchfork:
    config_policy: devflow-owned   # devflow-owned | import | mirror | merge
    external_daemons: show         # hide | show | importable
    web_ui:
      enabled: false               # show/open loopback Pitchfork Web UI bridge actions
      bind_address: 127.0.0.1
      bind_port: 3120
      edit_mode: warn              # readonly | warn | merge
  daemons:
    api:
      run: "npm run dev"
      dir: "."          # relative to workspace/worktree root
      depends: []
      port: { expect: [3000], bump: 50 }
      ready_http: "http://127.0.0.1:3000/health"
      watch: ["src/**/*.ts", "package.json"]   # devflow daemon restarts on changes
      env:
        DATABASE_URL: "{{ service['app-db'].url }}"
    worker:
      run: "npm run worker"
      required: false  # optional; failures do not fail switch/process start
      depends: [api]
      ready_delay: 2
      stop_timeout: 10
```

Daemon fields:

| Field | Purpose |
| --- | --- |
| `run` | shell command to execute |
| `dir` | working directory relative to the workspace root, or absolute |
| `env` | environment variables; values are templates |
| `required` | defaults to `true`; set `false` for optional processes whose failures should not fail lifecycle commands |
| `depends` | process names that start first |
| `port` | a port number, port array, or `{ expect: [...], bump: true|N }`; first resolved port is exposed as `$PORT` |
| `ready_delay` | seconds to wait before readiness checks begin, or before considering ready when no other check is configured |
| `ready_port` | TCP readiness check |
| `ready_http` | HTTP 2xx readiness check (ports are remapped when `port.bump` changes them) |
| `ready_cmd` | shell command readiness check |
| `ready_output` | regex matched against captured stdout/stderr logs |
| `ready_timeout` | readiness timeout in seconds (default 60) |
| `stop_timeout` | graceful shutdown timeout before SIGKILL (default 3, Unix) |
| `shutdown_signal` | graceful Unix signal: `TERM`, `INT`, `HUP`, `QUIT`, or `KILL` |
| `watch` | glob patterns, relative to `dir`, that the controller daemon polls for restart-on-change |
| `retry` | number of controller-daemon restart attempts after a crash |

Manage them with `devflow process start|stop|restart|status|logs`. Auto-started process commands reuse hook approvals; pre-approve with `devflow hook approvals add "npm run dev"` for non-interactive automation, or set `DEVFLOW_APPROVE_HOOKS=1`. `provider: pitchfork` embeds Pitchfork's Rust supervisor/log APIs directly; devflow still owns desired state and proxy/GUI records. `processes.pitchfork.config_policy` records how devflow should reconcile `.devflow.yml` with Pitchfork-native config files: `devflow-owned` is the safe default and the active behavior for devflow-managed processes; `import`, `mirror`, and `merge` are explicit opt-in reconciliation modes for GUI workflows and never silently overwrite devflow templates. `devflow proxy` reads process state and exposes port-backed processes as `https://<process>.<workspace>.<project>.<suffix>` (default suffix `.local`). Run `devflow daemon start` to keep desired-state/watch/retry reconciliation active in the background. See [Project processes & Pitchfork](/devflow/guides/processes/) for migration examples, readiness checks, and operational commands.

### Cloud providers (experimental)

```yaml
    neon:  { api_key: ${NEON_API_KEY}, project_id: ${NEON_PROJECT_ID}, base_url: https://console.neon.tech/api/v2 }
    dblab: { api_url: https://dblab.example.com, auth_token: ${DBLAB_TOKEN} }
    xata:  { api_key: ${XATA_API_KEY}, organization_id: my-org, project_id: my-project, base_url: https://api.xata.tech }
```

### `plugin:`

```yaml
    plugin:
      name: my-plugin        # resolved as devflow-plugin-my-plugin on PATH
      # path: /usr/local/bin/my-plugin
      timeout: 30
      config: { region: us-east-1 }    # opaque JSON forwarded to the plugin
```

## `worktree`

```yaml
worktree:
  path_template: "../{repo}.{workspace}"  # {repo}, {workspace} (collision-safe key), {branch} legacy
  copy_files: [.env.local, .env]          # files/dirs reflink-copied from main
  copy_ignored: false                     # also copy gitignored entries (collapsed, parallel)
  copy_ai_configs: true                   # copy .claude/, .cursor/, .opencode/, .agents/
  extra_ai_dirs: []                       # additional AI tool dirs
```

Full semantics in [Worktrees](/devflow/concepts/worktrees/#configuration). Worktrees are the only Git workspace model; an old `worktree.enabled` key is accepted for compatibility but ignored with a deprecation warning — it can no longer restore in-place checkout behavior. `{workspace}` is the collision-safe `service_key`, while user-facing and VCS operations keep the raw workspace name.

## `hooks`

```yaml
hooks:
  <phase>:                  # post-create, post-switch, pre-commit, … or any custom name
    <hook-name>: "command"  # simple form
    <hook-name>:            # extended form
      command: "npm run migrate"
      working_dir: "./backend"
      condition: "file_exists:package.json"
      continue_on_error: false
      background: false
      environment: { NODE_ENV: development }
    <hook-name>:            # action form
      action:
        type: write-env     # write-env | write-file | copy | replace | docker-exec | http | notify | sync-ai-configs | shell
        path: .env.local
        vars: { DATABASE_URL: "{{ service['app-db'].url }}" }
```

Phases, variables, filters, conditions, actions, and recipes: [hooks reference](/devflow/reference/hooks/).

## `execute`

```yaml
execute:
  multiplexer: tmux                      # or zellij (auto-detected when unset)
  detach_command: "screen -dmS {session} bash -c {cmd}"   # custom launcher: {session} {dir} {cmd}
```

Used by `devflow switch -o/--open` and `-d/--detach`.

## `commit`

```yaml
commit:
  generation:
    command: "claude -p --model haiku"   # external CLI (preferred)
    # api_url: "http://localhost:11434/v1"  # OpenAI-compatible fallback
    # model: "llama3"
    # api_key: ${DEVFLOW_LLM_API_KEY}
```

## `agent`

```yaml
agent:
  auto_context: true       # provide project context to agents on launch
```

## Value interpolation

Secrets in service configs support `${ENV_VAR}` interpolation, resolved at runtime:

```yaml
neon:
  api_key: ${NEON_API_KEY}
```

## Workspace identity

The raw VCS name is canonical for display, registry lookup, and VCS operations. Services and generated paths use a separate deterministic `service_key`: already-safe names are preserved, while names requiring normalization include a stable short hash. Consequently names such as `feature/auth`, `feature-auth`, and case variants remain distinct. Hook templates expose the key as `workspace_key`; `workspace_sanitized` is an alias.
