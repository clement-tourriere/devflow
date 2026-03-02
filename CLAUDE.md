# devflow — Universal Development Environment Branching Tool

## Overview
devflow is a Rust-based tool that provides branching support for development services (PostgreSQL, ClickHouse, MySQL, Redis, and more) that automatically synchronize with Git branches. It manages Git worktrees, Docker containers with Copy-on-Write storage, cloud database branches, and lifecycle hooks — all from a single CLI.

## Core Concepts
- **Service branching**: Each Git branch gets its own isolated set of services (databases, caches, etc.)
- **Git worktree integration**: Optionally creates worktree directories per branch for true parallel development
- **Multi-provider**: Local Docker containers, PostgreSQL TEMPLATE, Neon, DBLab, Xata, or custom plugins
- **Multi-service**: A single project can manage multiple services (e.g., PostgreSQL + ClickHouse + Redis)
- **Lifecycle hooks**: MiniJinja-templated commands that run at specific phases (post-create, pre-merge, etc.)
- **Copy-on-Write storage**: Uses APFS clones (macOS), ZFS snapshots, Btrfs/XFS reflinks for near-instant branching

## Key Features
- **Automatic Git integration**: Creates/switches service branches on `git checkout` via Git hooks
- **Git worktree management**: Creates worktree directories with configurable path templates and file copying
- **Multi-service support**: PostgreSQL, ClickHouse, MySQL, generic Docker, and plugin providers
- **Hook engine**: MiniJinja templates with custom filters (`sanitize`, `sanitize_db`, `hash_port`)
- **Seed support**: Seed databases from PostgreSQL URLs, local dump files, or S3
- **Shell integration**: `eval "$(devflow shell-init)"` for automatic `cd` into worktrees
- **JSON output + non-interactive mode**: For CI/CD and AI agent workflows
- **AI commit messages**: `devflow commit --ai` generates commit messages via LLM (CLI-first, API fallback)
- **AI agent integration**: `devflow agent start/status/context/skill/docs` for managing AI coding agents in isolated branches

## Configuration

The tool is configured via `.devflow.yml` in your Git repository root (created by `devflow init`).

### Configuration Hierarchy (highest to lowest):
1. **Environment Variables** — Quick toggles and overrides
2. **Local Config File** (`.devflow.local.yml`) — Project-specific local overrides (gitignored)
3. **Committed Config** (`.devflow.yml`) — Team shared configuration

### Environment Variables:
- `DEVFLOW_DISABLED=true` — Completely disable devflow
- `DEVFLOW_SKIP_HOOKS=true` — Skip Git hook execution
- `DEVFLOW_AUTO_CREATE=false` — Override auto_create_on_branch
- `DEVFLOW_AUTO_SWITCH=false` — Override auto_switch_on_branch
- `DEVFLOW_BRANCH_FILTER_REGEX=...` — Override branch filtering
- `DEVFLOW_DISABLED_BRANCHES=main,release/*` — Disable for specific branches
- `DEVFLOW_CURRENT_BRANCH_DISABLED=true` — Disable for current branch only
- `DEVFLOW_DATABASE_HOST=...` — Override database host
- `DEVFLOW_DATABASE_PORT=...` — Override database port
- `DEVFLOW_DATABASE_USER=...` — Override database user
- `DEVFLOW_DATABASE_PASSWORD=...` — Override database password
- `DEVFLOW_DATABASE_PREFIX=...` — Override database prefix
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
  auto_create_on_branch: true       # Auto-create service branch on git checkout
  auto_switch_on_branch: true       # Auto-switch services on git checkout
  main_branch: main                 # Main git branch
  branch_filter_regex: "^feature/.*"  # Only branch for matching patterns
  exclude_branches: [main, master]  # Never create branches for these

behavior:
  auto_cleanup: false
  max_branches: 10
  naming_strategy: prefix           # prefix, suffix, or replace

# Multi-provider setup
services:
  - name: app-db
    type: local
    service_type: postgres
    auto_branch: true
    default: true
    local:
      image: postgres:17
  - name: analytics
    type: local
    service_type: clickhouse
    auto_branch: true
    clickhouse:
      image: clickhouse/clickhouse-server:latest

# Worktree configuration
worktree:
  enabled: true
  path_template: "../{repo}.{branch}"
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
  branch_prefix: "agent/"           # Prefix for agent branches
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
# Build the project
cargo build

# Run tests
cargo test

# Run with development profile
cargo run

# Build release version
cargo build --release

# Run linting
cargo clippy

# Format code
cargo fmt

# Check for issues
cargo check
```

## Project Structure
- `src/main.rs` — CLI entry point with custom help template
- `src/cli.rs` — All command implementations (~7800 lines)
- `src/agent.rs` — AI agent integration (skill generation, context, rules)
- `src/config/mod.rs` — Config parsing, validation, env var overrides, local config merging
- `src/services/mod.rs` — `ServiceBackend` trait definition
- `src/services/factory.rs` — Backend creation, dispatch, orchestration
- `src/services/plugin.rs` — Plugin backend (JSON-over-stdio protocol)
- `src/services/postgres/local/` — Local Docker PostgreSQL backend with CoW storage
- `src/services/clickhouse/` — ClickHouse backend
- `src/services/mysql/` — MySQL backend
- `src/services/generic/` — Generic Docker backend (Redis, etc.)
- `src/vcs/mod.rs` — `VcsProvider` trait
- `src/vcs/git.rs` — Git implementation (branches, worktrees, hooks)
- `src/vcs/cow_worktree.rs` — Copy-on-Write worktree support (APFS, ZFS, Btrfs, XFS)
- `src/vcs/jj.rs` — Jujutsu VCS implementation
- `src/hooks/` — Hook engine (executor, approval, templates)
- `src/state/` — Local state persistence (`~/.local/share/devflow/`)
- `src/docker.rs` — Docker helper utilities
- `src/llm.rs` — LLM integration for AI commit messages (CLI-first + API fallback)

## References
- PostgreSQL TEMPLATE documentation for template backend
- Git worktree documentation for worktree management
- MiniJinja documentation for hook template syntax
