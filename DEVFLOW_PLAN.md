# devflow — Universal Development Environment Branching Tool

## Vision

**devflow** is a universal branching orchestrator for development environments — where "branching" applies to git worktrees, databases, caches, and any stateful service. It combines worktrunk-style worktree management with first-class stateful service branching (CoW cloning, cloud backends, lifecycle management).

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        devflow CLI                              │
│  (commands: switch, list, create, remove, merge, status, ...)   │
├─────────────────────────────────────────────────────────────────┤
│                     Hook Engine                                 │
│  (lifecycle hooks with MiniJinja templates, conditions,         │
│   approval system, blocking/background execution)               │
├──────────────┬──────────────────────────────────────────────────┤
│  VCS Layer   │            Service Layer                         │
│              │                                                  │
│  ┌────────┐  │  ┌─────────────────────────────────────────┐     │
│  │  Git   │  │  │           ServiceBackend                │     │
│  │ branch │  │  │  (trait: create/delete/switch/connect)  │     │
│  │  +     │  │  ├─────────┬──────────┬──────────┬────────┤     │
│  │worktree│  │  │Postgres │ClickHouse│  MySQL   │Generic │     │
│  ├────────┤  │  │(local,  │(local,   │(local,   │Docker  │     │
│  │  jj    │  │  │ neon,   │ cloud?)  │ planet-  │Service │     │
│  │(future)│  │  │ dblab,  │          │ scale?)  │        │     │
│  └────────┘  │  │ xata,   │          │          │        │     │
│              │  │ tpl)    │          │          │        │     │
│              │  └─────────┴──────────┴──────────┴────────┘     │
├──────────────┴──────────────────────────────────────────────────┤
│                    Configuration Layer                           │
│  (.devflow.yml / .devflow.toml / .devflow.local.yml / env vars) │
├─────────────────────────────────────────────────────────────────┤
│                    State Management                              │
│  (SQLite: projects, branches, services, hook approvals)         │
└─────────────────────────────────────────────────────────────────┘
```

## Key Design Decisions

- **Name**: `devflow` (CLI: `devflow` or `df` alias)
- **Scope**: All-in-one tool (worktree management + service branching + hooks)
- **Template engine**: MiniJinja (Jinja2-compatible)
- **Config format**: Both YAML and TOML (auto-detect)
- **Backward compat**: Clean break from pgbranch — no migration path
- **VCS support**: Git + worktrees now, jj (Jujutsu) architecture-ready
- **Service providers**: All existing PG backends + ClickHouse + MySQL + Generic Docker

## Differentiators vs. Worktrunk

| Aspect | Worktrunk | devflow |
|--------|-----------|---------|
| Services | Hooks only (dumb Docker) | First-class service branching with CoW, cloud backends |
| Database cloning | None (user manages) | ZFS/APFS/reflink CoW cloning, Neon/DBLab/Xata cloud |
| Service types | N/A | Postgres, ClickHouse, MySQL, generic Docker |
| Connection info | Manual .env generation | Auto-generated, template variables in hooks |
| Service lifecycle | Start/stop via hooks | Native start/stop/reset/seed commands |
| VCS support | Git only | Git + jj (future) |
| Config format | TOML only | YAML + TOML |
| Worktree mgmt | Core focus | Equal focus with services |
| AI integration | `-x claude` | `-x` + structured JSON output + AI-friendly |

## Module Design

### VCS Layer

```rust
// src/vcs/mod.rs
#[async_trait]
pub trait VcsProvider: Send + Sync {
    fn current_branch(&self) -> Result<String>;
    fn default_branch(&self) -> Result<String>;
    fn list_branches(&self) -> Result<Vec<BranchRef>>;
    fn create_branch(&self, name: &str, base: Option<&str>) -> Result<()>;
    fn delete_branch(&self, name: &str) -> Result<()>;

    // Worktree support
    fn supports_worktrees(&self) -> bool;
    fn list_worktrees(&self) -> Result<Vec<WorktreeInfo>>;
    fn create_worktree(&self, branch: &str, path: &Path) -> Result<()>;
    fn remove_worktree(&self, path: &Path) -> Result<()>;
    fn worktree_path(&self, branch: &str) -> Result<Option<PathBuf>>;

    // Hooks
    fn install_hooks(&self, hooks_dir: &Path) -> Result<()>;
    fn uninstall_hooks(&self, hooks_dir: &Path) -> Result<()>;

    fn provider_name(&self) -> &'static str;
}
```

### Service Layer

```rust
// src/services/mod.rs
#[async_trait]
pub trait ServiceBackend: Send + Sync {
    fn service_type(&self) -> &'static str;
    fn backend_name(&self) -> &'static str;
    fn display_name(&self) -> String;

    async fn create_branch(&self, branch_name: &str, from_branch: Option<&str>) -> Result<ServiceBranchInfo>;
    async fn delete_branch(&self, branch_name: &str) -> Result<()>;
    async fn list_branches(&self) -> Result<Vec<ServiceBranchInfo>>;
    async fn branch_exists(&self, branch_name: &str) -> Result<bool>;
    async fn switch_to_branch(&self, branch_name: &str) -> Result<ServiceBranchInfo>;
    async fn get_connection_info(&self, branch_name: &str) -> Result<ConnectionInfo>;

    async fn start_branch(&self, branch_name: &str) -> Result<()> { Ok(()) }
    async fn stop_branch(&self, branch_name: &str) -> Result<()> { Ok(()) }
    async fn reset_branch(&self, branch_name: &str) -> Result<()> { Ok(()) }
    fn supports_lifecycle(&self) -> bool { false }

    async fn doctor(&self) -> Result<DoctorReport>;
    async fn test_connection(&self) -> Result<()>;
    async fn cleanup_old_branches(&self, max_count: usize) -> Result<Vec<String>>;

    async fn seed_from_source(&self, branch_name: &str, source: &str) -> Result<()> {
        Err(anyhow!("Seeding not supported for this service"))
    }
}
```

### Hook Engine

```rust
// src/hooks/mod.rs
pub enum HookPhase {
    // VCS/worktree lifecycle
    PreSwitch,
    PostCreate,       // blocking
    PostStart,        // background
    PostSwitch,       // background
    PreRemove,
    PostRemove,       // background

    // Merge lifecycle
    PreCommit,
    PreMerge,
    PostMerge,

    // Service lifecycle
    PreServiceCreate,
    PostServiceCreate,
    PreServiceDelete,
    PostServiceDelete,
    PostServiceSwitch,

    // Custom
    Custom(String),
}
```

#### Template Variables

| Variable | Description |
|----------|-------------|
| `{{ branch }}` | Current branch name |
| `{{ repo }}` | Repository directory name |
| `{{ worktree_path }}` | Worktree path |
| `{{ default_branch }}` | Default branch (main/master) |
| `{{ service.<name>.host }}` | Service connection host |
| `{{ service.<name>.port }}` | Service connection port |
| `{{ service.<name>.database }}` | Database name |
| `{{ service.<name>.user }}` | Service user |
| `{{ service.<name>.password }}` | Service password |
| `{{ service.<name>.url }}` | Full connection URL |
| `{{ commit }}` | HEAD commit SHA |
| `{{ target }}` | Target branch (merge hooks) |
| `{{ base }}` | Base branch (creation hooks) |

#### Filters

- `sanitize` — Replace `/` and `\` with `-`
- `sanitize_db` — Database-safe identifier with hash suffix
- `hash_port` — Hash to port 10000-19999

## Configuration

### Example .devflow.yml

```yaml
# All sections are optional — an empty file is valid.

git:
  auto_create_on_branch: true
  auto_switch_on_branch: true
  main_branch: main
  branch_filter_regex: "^feature/.*"
  exclude_branches: [main, master]

behavior:
  auto_cleanup: true
  max_branches: 10
  naming_strategy: prefix

# Multi-provider setup
services:
  - name: app-db
    type: local
    service_type: postgres
    auto_branch: true
    default: true
    local:
      image: postgres:17
      port_range_start: 55432
      postgres_user: dev
      postgres_password: dev

  - name: analytics-db
    type: local
    service_type: clickhouse
    auto_branch: true
    clickhouse:
      image: clickhouse/clickhouse-server:latest

  - name: legacy-db
    type: local
    service_type: mysql
    auto_branch: true
    mysql:
      image: mysql:8

  - name: cache
    type: local
    service_type: generic
    auto_branch: false
    generic:
      image: redis:7-alpine
      port_mapping: "6379:6379"
      environment:
        REDIS_MAXMEMORY: "100mb"

  - name: cloud-db
    type: neon
    service_type: postgres
    auto_branch: true
    neon:
      api_key: ${NEON_API_KEY}
      project_id: ${NEON_PROJECT_ID}

worktree:
  enabled: true
  path_template: "../{repo}.{branch}"
  copy_files: [".env.local"]
  copy_ignored: true

hooks:
  post-create:
    install: "npm ci"
    env: |
      cat > .env.local << EOF
      DATABASE_URL={{ service['app-db'].url }}
      CLICKHOUSE_URL={{ service.analytics-db.url }}
      REDIS_URL={{ service.cache.url }}
      EOF

  post-start:
    dev-server: "npm run dev -- --port {{ branch | hash_port }}"

  pre-merge:
    test: "npm test"
    lint: "npm run lint"

  post-remove:
    cleanup: "docker stop {{ repo }}-{{ branch | sanitize }}-* 2>/dev/null || true"
```

## CLI Commands

```
devflow (df)
├── init                     # Initialize project (.devflow.yml)
├── switch [branch]          # Switch branch/worktree (create if needed)
│   ├── --create (-c)        # Create new branch
│   ├── --base (-b)          # Base branch
│   ├── --execute (-x)       # Run command after switch
│   ├── --no-services        # Skip service branching
│   └── --no-verify          # Skip hooks
├── list                     # List branches with service status
├── remove [branch]          # Remove branch/worktree + service branches
├── merge [target]           # Merge workflow
├── status                   # Detailed status of current branch
│
├── service                  # Service management
│   ├── list                 # List all configured services
│   ├── create [branch]      # Create service branch(es)
│   ├── delete [branch]      # Delete service branch(es)
│   ├── start [branch]       # Start service (local providers)
│   ├── stop [branch]        # Stop service
│   ├── reset [branch]       # Reset to parent state
│   ├── connection [service] # Show connection info
│   ├── seed [service]       # Seed from source
│   ├── destroy [--force]    # Remove all containers and data
│   └── doctor               # Health check
│
├── hook                     # Hook management
│   ├── show                 # Show configured hooks
│   ├── run <phase> [name]   # Run hooks manually
│   └── approvals            # Manage approvals
│       ├── add
│       └── clear
│
├── config                   # Configuration management
│   ├── show                 # Show effective config
│   ├── shell install        # Install shell integration
│   └── state                # State management
│
├── install-hooks            # Install VCS hooks
├── uninstall-hooks          # Remove VCS hooks
└── doctor                   # Full system health check
```

## Target Project Structure

```
src/
├── main.rs
├── cli/
│   ├── mod.rs              # Command routing
│   ├── switch.rs           # devflow switch
│   ├── list.rs             # devflow list
│   ├── remove.rs           # devflow remove
│   ├── merge.rs            # devflow merge
│   ├── service.rs          # devflow service *
│   ├── hook.rs             # devflow hook
│   ├── config_cmd.rs       # devflow config / init
│   └── doctor.rs           # devflow doctor
├── vcs/
│   ├── mod.rs              # VcsProvider trait
│   ├── git.rs              # Git + worktree impl
│   └── jj.rs               # Jujutsu (future stub)
├── services/
│   ├── mod.rs              # ServiceBackend trait + shared structs
│   ├── factory.rs          # Service creation/resolution
│   ├── postgres/
│   │   ├── mod.rs
│   │   ├── local/
│   │   │   ├── mod.rs      # LocalBackend (Docker + CoW)
│   │   │   ├── docker.rs   # DockerRuntime
│   │   │   ├── state.rs    # SQLite Store
│   │   │   ├── model.rs    # Project, Branch, StorageBackend, BranchState
│   │   │   ├── seed.rs     # Seeding
│   │   │   ├── reconcile.rs
│   │   │   └── storage/
│   │   │       ├── mod.rs
│   │   │       ├── local_driver.rs
│   │   │       ├── zfs_driver.rs
│   │   │       └── zfs_setup.rs
│   │   ├── template.rs     # CREATE DATABASE WITH TEMPLATE
│   │   ├── neon.rs
│   │   ├── dblab.rs
│   │   └── xata.rs
│   ├── clickhouse/
│   │   ├── mod.rs
│   │   └── local.rs
│   ├── mysql/
│   │   ├── mod.rs
│   │   └── local.rs
│   └── generic/
│       └── mod.rs          # Generic Docker service
├── hooks/
│   ├── mod.rs              # HookEngine, HookPhase
│   ├── template.rs         # MiniJinja template engine
│   ├── approval.rs         # Security/approval system
│   └── executor.rs         # Hook execution (blocking/background)
├── config/
│   ├── mod.rs              # Config structs + merging
│   ├── yaml.rs             # YAML parser
│   ├── toml.rs             # TOML parser
│   └── env.rs              # Environment variable overrides
├── state/
│   ├── mod.rs              # SQLite state store
│   └── local_state.rs      # Per-project state
└── docker/
    ├── mod.rs              # Docker runtime (bollard)
    └── compose.rs          # Docker compose detection
```

## Implementation Phases

### Phase 1 — Foundation & Rename (weeks 1-2)
- [x] Write plan
- [x] Rename project to devflow (Cargo.toml, binary name)
- [x] Restructure: create src/vcs/, src/services/, src/hooks/, src/config/
- [x] Introduce VcsProvider trait, implement GitProvider (wrap existing git.rs)
- [x] Rename DatabaseBranchingBackend -> ServiceBackend
- [x] Move PG backends into src/services/postgres/
- [x] Update config file names: .devflow.yml, .devflow.local.yml
- [x] Update CLI help text, error messages
- [x] Ensure cargo build + cargo test pass

### Phase 2 — Hook Engine (weeks 2-3)
- [x] Add minijinja dependency
- [x] Implement HookPhase enum and HookEngine
- [x] Template engine with service variable support
- [x] Add worktrunk-style filters: sanitize, sanitize_db, hash_port
- [x] Migrate PostCommandExecutor to new hook engine (keep backward compat)
- [x] Service template variables: {{ service.<name>.host }}, etc.
- [x] Hook approval system for project hooks
- [x] Background hook support (non-blocking via tokio::spawn)

### Phase 3 — Worktree Management (weeks 3-4)
- [x] Full worktree management in GitProvider
- [x] `devflow switch` command (worktrunk-style)
- [x] Path template configuration
- [x] `devflow list` with rich status (git status + service status)
- [x] `devflow remove` with cleanup
- [x] `devflow merge` workflow
- [x] Interactive picker (inquire crate)
- [x] Shell integration for directory changes

### Phase 4 — Service Expansion (weeks 4-6)
- [x] GenericDockerService backend
- [x] ClickHouse local backend
- [x] MySQL/MariaDB local backend
- [ ] Cloud service provider abstraction
- [x] Multi-service orchestration (best-effort, sequential with partial failure tolerance)

### Phase 5 — jj + Polish (future)
- [x] JjProvider implementing VcsProvider
- [x] LLM commit messages
- [x] devflow step copy-ignored equivalent
- [x] Plugin system (executable plugins with JSON-over-stdio protocol)
- [x] AI-friendly output modes
