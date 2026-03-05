# devflow — Universal Development Environment Tool

## Overview
devflow is a Rust-based tool that provides per-workspace isolation for development services (PostgreSQL, ClickHouse, MySQL, Redis, and more) that automatically synchronize with workspaces. It manages Git worktrees, Docker containers with Copy-on-Write storage, cloud database workspaces, and lifecycle hooks — from a CLI, TUI, or desktop GUI. It also includes a native reverse proxy that auto-discovers Docker containers and provides HTTPS access via `*.localhost` domains.

## Core Concepts
- **Workspace isolation**: Each workspace gets its own isolated set of services (databases, caches, etc.)
- **Git worktree integration**: Optionally creates worktree directories per workspace for true parallel development
- **Multi-provider**: Local Docker containers, PostgreSQL TEMPLATE, Neon, DBLab, Xata, or custom plugins
- **Multi-service**: A single project can manage multiple services (e.g., PostgreSQL + ClickHouse + Redis)
- **Lifecycle hooks**: MiniJinja-templated commands that run at specific phases (post-create, pre-merge, etc.)
- **Copy-on-Write storage**: Uses APFS clones (macOS), ZFS snapshots, Btrfs/XFS reflinks for near-instant workspace creation

## Key Features
- **Automatic Git integration**: Creates/switches service workspaces on `git checkout` via Git hooks
- **Git worktree management**: Creates worktree directories with configurable path templates and file copying
- **Multi-service support**: PostgreSQL, ClickHouse, MySQL, generic Docker, and plugin providers
- **Hook engine**: MiniJinja templates with custom filters (`sanitize`, `sanitize_db`, `hash_port`)
- **Seed support**: Seed databases from PostgreSQL URLs, local dump files, or S3
- **Shell integration**: `eval "$(devflow shell-init)"` for automatic `cd` into worktrees
- **JSON output + non-interactive mode**: For CI/CD and AI agent workflows
- **AI commit messages**: `devflow commit --ai` generates commit messages via LLM (CLI-first, API fallback)
- **AI agent integration**: `devflow agent start/status/context/skill/docs` for managing AI coding agents in isolated workspaces
- **Native reverse proxy**: Auto-discovers Docker containers and serves them via HTTPS `*.localhost` domains with auto-generated certificates
- **Desktop GUI**: Tauri 2 desktop app with React frontend for managing projects, workspaces, services, hooks, proxy, and configuration

## Configuration

The tool is configured via `.devflow.yml` in your Git repository root (created by `devflow init`).

### Configuration Hierarchy (highest to lowest):
1. **Environment Variables** — Quick toggles and overrides
2. **Local Config File** (`.devflow.local.yml`) — Project-specific local overrides (gitignored)
3. **Committed Config** (`.devflow.yml`) — Team shared configuration

### Environment Variables:
- `DEVFLOW_DISABLED=true` — Completely disable devflow
- `DEVFLOW_SKIP_HOOKS=true` — Skip Git hook execution
- `DEVFLOW_AUTO_CREATE=false` — Override auto_create_on_workspace
- `DEVFLOW_AUTO_SWITCH=false` — Override auto_switch_on_workspace
- `DEVFLOW_BRANCH_FILTER_REGEX=...` — Override workspace filtering
- `DEVFLOW_DISABLED_BRANCHES=main,release/*` — Disable for specific workspaces
- `DEVFLOW_CURRENT_BRANCH_DISABLED=true` — Disable for current workspace only
- `DEVFLOW_ZFS_DATASET=...` — Force a specific ZFS dataset
- `DEVFLOW_LLM_API_KEY=...` — API key for AI commit messages
- `DEVFLOW_LLM_API_URL=...` — LLM endpoint URL
- `DEVFLOW_LLM_MODEL=...` — LLM model name
- `DEVFLOW_COMMIT_COMMAND=...` — External CLI for commit messages (e.g., "claude -p")
- `DEVFLOW_AGENT_COMMAND=...` — Default agent command (e.g., "claude", "codex")

### Config File Schema (`.devflow.yml`):
```yaml
# All sections are optional — an empty file is valid
git:
  auto_create_on_workspace: true       # Auto-create service workspace on git checkout
  auto_switch_on_workspace: true       # Auto-switch services on git checkout
  main_workspace: main                 # Main git workspace
  workspace_filter_regex: "^feature/.*"  # Only workspace for matching patterns
  exclude_workspaces: [main, master]  # Never create workspaces for these

behavior:
  max_workspaces: 10

# Multi-provider setup
services:
  - name: app-db
    type: local
    service_type: postgres
    auto_workspace: true
    default: true
    local:
      image: postgres:17
  - name: analytics
    type: local
    service_type: clickhouse
    auto_workspace: true
    clickhouse:
      image: clickhouse/clickhouse-server:latest

# Worktree configuration
worktree:
  enabled: true
  path_template: "../{repo}.{workspace}"
  copy_files: [".env.local", ".env"]
  copy_ignored: true

# Lifecycle hooks (MiniJinja templates)
hooks:
  post-create:
    migrate: "npm run migrate"
    env-setup:
      command: "echo DATABASE_URL={{ service['app-db'].url }} > .env.local"
      working_dir: "."
  post-switch:
    update-env:
      command: "echo DATABASE_URL={{ service['app-db'].url }} > .env.local"
  pre-merge:
    test: "npm test"

# AI agent configuration
agent:
  command: claude                    # Default agent command
  workspace_prefix: "agent/"           # Prefix for agent workspaces
  auto_context: true                # Provide context on launch

# AI commit message generation
commit:
  generation:
    command: "claude -p --model haiku"  # External CLI (preferred)
    # api_url: "http://localhost:11434/v1"   # OpenAI-compatible API (fallback)
    # model: "llama3"
    # api_key: "..."
```

## Development Commands
When working on this project, use these commands:

```bash
# Build / check / test (entire workspace)
cargo build                       # or: mise run build
cargo check --workspace           # or: mise run check
cargo test --workspace            # or: mise run test
cargo clippy --workspace          # or: mise run lint
cargo fmt                         # or: mise run fmt

# Build only a specific crate
cargo build -p devflow            # CLI only
cargo build -p devflow-proxy      # Proxy only
cargo build -p devflow-app        # GUI backend only

# Desktop GUI (requires bun + Tauri CLI)
mise run gui                      # Dev mode with hot-reload
mise run gui:build                # Production bundle
mise run gui:install              # Install frontend deps only
```

## Project Structure

The project is organized as a Cargo workspace with four crates:

### Root crate (`devflow`) — CLI binary
- `src/main.rs` — CLI entry point with custom help template
- `src/cli/mod.rs` — Command enum and dispatch
- `src/cli/workspace.rs` — Workspace operations (switch, list, graph, link, remove, merge, cleanup)
- `src/cli/service.rs` — Service operations (add, create, delete, start, stop, reset, seed, logs, discover)
- `src/cli/agent.rs` — AI agent commands (start, status, context, skill, docs)
- `src/cli/proxy.rs` — Proxy commands (start, stop, status, list, trust)
- `src/cli/hook.rs` — Hook commands (show, run, explain, vars, render, approvals, triggers, actions)
- `src/cli/commit.rs` — VCS commit with AI message generation
- `src/tui/` — Terminal UI (ratatui-based dashboard)

### `crates/devflow-core/` — Shared library
- `src/config/mod.rs` — Config parsing, validation, env var overrides, local config merging
- `src/services/mod.rs` — `ServiceProvider` trait definition
- `src/services/factory.rs` — Backend creation, dispatch, orchestration
- `src/services/plugin.rs` — Plugin backend (JSON-over-stdio protocol)
- `src/services/postgres/local/` — Local Docker PostgreSQL backend with CoW storage
- `src/services/clickhouse/` — ClickHouse backend
- `src/services/mysql/` — MySQL backend
- `src/services/generic/` — Generic Docker backend (Redis, etc.)
- `src/vcs/mod.rs` — `VcsProvider` trait
- `src/vcs/git.rs` — Git implementation (workspaces, worktrees, hooks)
- `src/vcs/cow_worktree.rs` — Copy-on-Write worktree support (APFS, ZFS, Btrfs, XFS)
- `src/vcs/jj.rs` — Jujutsu VCS implementation
- `src/hooks/` — Hook engine (executor, approval, templates)
- `src/state/` — Local state persistence (`~/.local/share/devflow/`)
- `src/docker.rs` — Docker helper utilities
- `src/agent.rs` — AI agent integration (skill generation, context, rules)
- `src/llm.rs` — LLM integration for AI commit messages (CLI-first + API fallback)

### `crates/devflow-proxy/` — Native reverse proxy
- `src/lib.rs` — `ProxyConfig`, `ProxyHandle`, `run_proxy()` entry point
- `src/ca.rs` — Certificate Authority generation, cert signing, cache (rcgen)
- `src/platform.rs` — System trust installation (macOS keychain, Linux cert stores)
- `src/monitor.rs` — Docker event streaming via bollard
- `src/discovery.rs` — Container-to-domain/IP/port extraction
- `src/router.rs` — Dynamic routing table (Host header → upstream)
- `src/tls.rs` — rustls ServerConfig with SNI-based cert resolution
- `src/server.rs` — hyper reverse proxy (TLS termination + HTTP forwarding)
- `src/api.rs` — JSON API endpoints (/api/status, /api/targets, /api/ca)

### `src-tauri/` — Tauri 2 desktop GUI (Rust backend)
- `src/main.rs` — Tauri app setup, system tray, window management
- `src/state.rs` — `AppState`, `AppSettings`, project registry
- `src/commands/` — Tauri IPC commands (projects, workspaces, services, hooks, proxy, config, settings)

### `ui/` — React frontend (for the desktop GUI)
- `src/App.tsx` — Routes and global layout
- `src/components/Layout.tsx` — Sidebar navigation + content area
- `src/pages/` — Home, ProjectList, ProjectDetail, ProxyDashboard, HookManager, ConfigEditor, Settings
- `src/utils/invoke.ts` — Typed wrappers around Tauri IPC
- `src/types/index.ts` — TypeScript interfaces matching Rust DTOs

## References
- PostgreSQL TEMPLATE documentation for template backend
- Git worktree documentation for worktree management
- MiniJinja documentation for hook template syntax
