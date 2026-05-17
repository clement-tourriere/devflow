# Architecture

This document describes the high-level architecture of devflow. It is intended for contributors and anyone who wants to understand how the codebase is organized.

## Crate Structure

```
devflow (workspace)
├── devflow           CLI binary + TUI (src/)
├── devflow-core      Shared library (crates/devflow-core/)
├── devflow-proxy     Reverse proxy (crates/devflow-proxy/)
├── devflow-terminal  PTY manager (crates/devflow-terminal/)
└── devflow-app       Tauri desktop backend (src-tauri/)
```

### Dependency graph

```
devflow (CLI)  ──→  devflow-core
    │               devflow-proxy
    │
devflow-app  ──→  devflow-core
    │             devflow-proxy
    │             devflow-terminal
```

`devflow-core` is the shared library — it knows nothing about the CLI, TUI, or GUI. `devflow-proxy` is also standalone and reusable.

## Core Architecture (`devflow-core`)

### Layered Design

```
┌──────────────────────────────────────────────────────┐
│  Workspace Lifecycle (workspace/)                    │
│  create, switch, delete — orchestrates everything    │
├──────────┬──────────────┬──────────────┬─────────────┤
│  VCS     │  Services    │  Hooks       │  State      │
│  (vcs/)  │  (services/) │  (hooks/)    │  (state/)   │
├──────────┴──────────────┴──────────────┴─────────────┤
│  Config (config/)                                    │
│  .devflow.yml + .devflow.local.yml + env vars        │
└──────────────────────────────────────────────────────┘
```

### Key Traits

**`ServiceProvider`** (`services/mod.rs`) — The core abstraction for all service backends. Every provider (PostgreSQL, ClickHouse, MySQL, generic Docker, Neon, DBLab, Xata, plugin) implements this trait. Methods are capability-driven: optional methods have default no-op implementations, and `capabilities()` advertises what the provider supports.

**`VcsProvider`** (`vcs/mod.rs`) — Abstraction over version control systems. Git is the primary implementation (via `git2` library). Jujutsu is also supported. Detection is automatic via `detect_vcs_provider()`.

### Service Provider Dispatch

```
NamedServiceConfig
    │
    ▼
factory::create_provider_from_named_config()
    │
    ├─ service_type == "postgres"
    │   ├─ provider_type == "local"  → LocalProvider (Docker + CoW)
    │   ├─ provider_type == "neon"   → NeonProvider (API)
    │   ├─ provider_type == "dblab"  → DBLabProvider (API)
    │   └─ provider_type == "xata"   → XataProvider (API)
    │
    ├─ service_type == "clickhouse" → ClickHouseLocalProvider
    ├─ service_type == "mysql"      → MySQLLocalProvider
    ├─ service_type == "generic"    → GenericDockerProvider
    └─ plugin config present        → PluginProvider (JSON-over-stdio)
```

### Workspace Lifecycle

A `switch -c` (create + switch) executes this sequence:

```
1. Pre-switch hooks
2. VCS branch creation
3. Linked worktree setup + file copying
4. Pre-service-create hooks
5. Service orchestration (create instances for each configured service)
6. Post-service-create hooks
7. State registration (SQLite)
8. Post-create hooks
9. Post-switch hooks
```

`workspace/create.rs` delegates to the shared lifecycle in `workspace/switch.rs`, which also registers workspace state.

### Hook Engine

The hook engine (`hooks/`) is MiniJinja-based:

- **Phases**: 18 built-in lifecycle phases + custom phases
- **Entry types**: Simple (shell command string) or Action (structured: `write-env`, `copy`, `docker-exec`, `http`, `notify`, etc.)
- **Approval**: Shell commands from project config require approval in `--non-interactive` mode
- **Execution**: Blocking phases wait for completion; non-blocking phases run in background
- **Templates**: Full MiniJinja with custom filters (`sanitize`, `sanitize_db`, `hash_port`)

### Configuration

Three-tier precedence (highest to lowest):
1. Environment variables (`DEVFLOW_*`)
2. `.devflow.local.yml` (gitignored, per-developer)
3. `.devflow.yml` (committed, team-shared)

Configuration types and merging live in `config/mod.rs`; loading and validation live in `config/loading.rs`.

### State Management

Local state is stored in SQLite at `~/.local/share/devflow/state.db`:
- Project registry (path → project metadata)
- Workspace registry (project + workspace → parent, worktree path, timestamps)
- Hook approval store

## Proxy Architecture (`devflow-proxy`)

```
Docker Events (bollard)
    │
    ▼
Monitor → Discovery → Router → Server
                         │        │
                         │        ├─ TLS (rustls + rcgen CA)
                         │        └─ HTTP reverse proxy (hyper)
                         │
                         └─ JSON API (/api/status, /api/targets)
```

The proxy watches Docker events, discovers containers, and dynamically routes `*.localhost` HTTPS requests to the correct container IP:port.

## GUI Architecture (`src-tauri/` + `ui/`)

```
React Frontend (ui/src/)
    │
    │  Tauri IPC (invoke)
    ▼
Rust Backend (src-tauri/src/)
    │
    ├─ devflow-core (workspace/service/hook operations)
    ├─ devflow-proxy (proxy management)
    └─ devflow-terminal (PTY sessions)
```

The desktop app uses Tauri 2 with a React frontend. IPC commands in `src-tauri/src/commands/` wrap `devflow-core` calls. Project-level doctor diagnostics live in `crates/devflow-core/src/doctor.rs` so the CLI, TUI, and GUI report the same setup checks.

## Feature Flags

All optional backends and UI components are feature-gated:

| Flag | What it enables | Key dependency |
|---|---|---|
| `service-local` | Docker-based local providers | `bollard`, `rusqlite` |
| `service-neon` | Neon cloud provider | `reqwest` |
| `service-dblab` | DBLab provider | `reqwest` |
| `service-xata` | Xata provider | `reqwest` |
| `llm` | AI commit messages | `reqwest` |
| `skills` | External skill management and generated agent guides (no bundled skills) | `reqwest`, `sha2` |
| `tui` | Terminal UI | `ratatui`, `crossterm` |
