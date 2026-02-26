# devflow

Isolated services for every Git branch — automatically.

## What It Does

devflow gives each Git branch its own set of services: databases, caches, or anything that runs in Docker. When you `git checkout feature-auth`, devflow automatically spins up (or switches to) a PostgreSQL, ClickHouse, MySQL, or Redis instance that belongs to that branch. Data is cloned from the parent branch using Copy-on-Write, so branching is near-instant and uses almost no extra disk space.

It works in four modes:
- **Local** — Docker containers with CoW storage (APFS, ZFS, Btrfs, XFS)
- **Template** — PostgreSQL's `CREATE DATABASE ... WITH TEMPLATE` on an existing server
- **Cloud** — Neon, DBLab, or Xata APIs
- **Plugin** — custom backends via JSON-over-stdio protocol

## Install

```bash
git clone https://github.com/clement-tourriere/devflow.git
cd devflow
cargo install --path .
```

Requires Rust 1.70+ and Docker (for local mode). See [Full Install](#full-install) for platform-specific instructions.

## Quick Start

```bash
# 1. Initialize (creates .devflow.yml and a "main" service branch)
devflow init myapp

# 2. Install Git hooks (auto-create/switch on checkout)
devflow install-hooks

# 3. Create a feature branch — devflow branches services automatically
git checkout -b feature/auth

# 4. Check what's running
devflow status

# 5. Get connection info
devflow connection feature/auth
devflow connection feature/auth --format env    # DATABASE_URL=...
```

That's it. Your feature branch now has its own database. Schema changes, test data, and migrations are completely isolated from main.

## How It Works

### Local Backend

1. `devflow init` pulls a Docker image and starts a container for the "main" branch, with data bind-mounted to the host filesystem
2. `devflow create feature-auth` pauses the parent container, clones the data directory using Copy-on-Write, then starts a new container pointing at the clone
3. Each branch container gets a unique port, so multiple branches can run simultaneously
4. `devflow delete feature-auth` stops the container and removes its data directory

**Copy-on-Write storage** makes step 2 near-instant regardless of database size. Only changed blocks are duplicated:

| Filesystem | Platform | CoW Method | Setup Required |
|---|---|---|---|
| APFS | macOS | `cp -c` clone | None (automatic) |
| ZFS | Linux | Snapshots + clones | `apt install zfsutils-linux` then `devflow setup-zfs` |
| Btrfs | Linux | Reflink copy | None (if filesystem is Btrfs) |
| XFS | Linux | Reflink copy | None (if created with reflink support) |
| ext4 / other | Any | Full copy (fallback) | None (works, just slower) |

### Template Backend

Uses PostgreSQL's built-in `CREATE DATABASE ... WITH TEMPLATE` for server-side copies. No Docker required, but branches share the same PostgreSQL instance and the template database must have no active connections during branching.

### Cloud Backends

Neon, DBLab, and Xata backends use their respective APIs to manage branches remotely. Configure with API keys in `.devflow.yml`.

### Plugin Backend

Custom backends can be built as standalone executables that communicate via JSON-over-stdio. Run `devflow plugin init <path>` for a scaffold.

## Configuration

### `.devflow.yml`

Created by `devflow init`. All sections are optional.

#### Single backend

```yaml
backend:
  type: local                       # local, postgres_template, neon, dblab, xata
  service_type: postgres            # postgres, clickhouse, mysql, generic
  local:
    image: postgres:17
    port_range_start: 55432
    postgres_user: postgres
    postgres_password: postgres
    postgres_db: myapp
```

#### Multiple backends

```yaml
backends:
  - name: app-db
    type: local
    service_type: postgres
    auto_branch: true               # Branch this service with git
    default: true                   # Default target for -d flag
    local:
      image: postgres:17

  - name: analytics
    type: local
    service_type: clickhouse
    auto_branch: true
    clickhouse:
      image: clickhouse/clickhouse-server:latest

  - name: cache
    type: local
    service_type: generic
    auto_branch: false              # Shared across branches
    generic:
      image: redis:7-alpine
      port_mapping: "6379:6379"
```

#### Git integration

```yaml
git:
  auto_create_on_branch: true       # Create service branches on git checkout
  auto_switch_on_branch: true       # Switch services on git checkout
  main_branch: main                 # Main git branch (auto-detected on init)
  auto_create_branch_filter: "^feature/.*"  # Only branch for matching patterns
  exclude_branches:                 # Never create branches for these
    - main
    - master
    - develop
```

#### Behavior

```yaml
behavior:
  auto_cleanup: false               # Auto-cleanup old branches
  max_branches: 10                  # Max branches before cleanup
  naming_strategy: prefix           # prefix, suffix, or replace
```

#### Worktrees

```yaml
worktree:
  enabled: true
  path_template: "../{repo}.{branch}"
  copy_files: [".env.local", ".env"]
  copy_ignored: true                # Copy files even if gitignored
```

### Config Hierarchy

Highest to lowest precedence:

1. **Environment variables** — quick toggles and overrides
2. **`.devflow.local.yml`** — project-specific local overrides (add to `.gitignore`)
3. **`.devflow.yml`** — team shared configuration

### Environment Variables

```bash
DEVFLOW_DISABLED=true                # Completely disable devflow
DEVFLOW_SKIP_HOOKS=true              # Skip Git hook execution
DEVFLOW_AUTO_CREATE=false            # Override auto_create_on_branch
DEVFLOW_AUTO_SWITCH=false            # Override auto_switch_on_branch
DEVFLOW_BRANCH_FILTER_REGEX=...      # Override branch filtering
DEVFLOW_DISABLED_BRANCHES=main,release/*  # Disable for specific branches
DEVFLOW_CURRENT_BRANCH_DISABLED=true # Disable for current branch only
DEVFLOW_DATABASE_HOST=...            # Override database host
DEVFLOW_DATABASE_PORT=...            # Override database port
DEVFLOW_DATABASE_USER=...            # Override database user
DEVFLOW_DATABASE_PASSWORD=...        # Override database password
DEVFLOW_DATABASE_PREFIX=...          # Override database prefix
DEVFLOW_ZFS_DATASET=...              # Force a specific ZFS dataset
DEVFLOW_LLM_API_KEY=...              # API key for AI commit messages
DEVFLOW_LLM_API_URL=...              # LLM endpoint URL
DEVFLOW_LLM_MODEL=...               # LLM model name
```

## CLI Reference

### Branch Management

```bash
devflow create <branch>                  # Create a service branch
devflow create <branch> --from <parent>  # Create from a specific parent
devflow delete <branch>                  # Delete a service branch
devflow remove <branch>                  # Remove branch + worktree + all services
devflow list                             # List all branches (tree view)
devflow switch                           # Interactive switch with fuzzy search
devflow switch <branch>                  # Switch to a branch (creates if needed)
devflow switch --template                # Switch to main/template
devflow cleanup --max-count 5            # Remove old branches, keep most recent N
```

### Lifecycle (Local Backend)

```bash
devflow start <branch>                   # Start a stopped container
devflow stop <branch>                    # Stop a running container
devflow reset <branch>                   # Reset branch data to parent state
devflow destroy                          # Remove all containers and data
devflow destroy --force                  # Skip confirmation
devflow seed <branch> --from <source>    # Seed from PostgreSQL URL, file, or s3://
devflow logs <branch>                    # Show container logs (last 100 lines)
devflow logs <branch> --tail 50          # Show last 50 lines
```

### VCS

```bash
devflow merge <target>                   # Merge current branch into target
devflow commit                           # Commit staged changes
devflow commit --ai                      # AI-generated commit message
```

### Info & Diagnostics

```bash
devflow status                           # Project and backend status
devflow config                           # Current configuration
devflow config -v                        # Config with precedence details
devflow doctor                           # System health check
devflow connection <branch>              # Connection URI (default)
devflow connection <branch> --format env # Environment variables
devflow connection <branch> --format json # JSON object
```

### Setup

```bash
devflow init [name]                      # Initialize configuration
devflow init [name] --backend <type>     # Specify backend type
devflow init [name] --from <source>      # Seed main branch from source
devflow install-hooks                    # Install Git hooks
devflow uninstall-hooks                  # Remove Git hooks
devflow setup-zfs                        # Create file-backed ZFS pool (Linux)
devflow setup-zfs --size 20G             # Custom pool size
devflow worktree-setup                   # Set up devflow in a Git worktree
```

### Hooks

```bash
devflow hook show                        # Show all configured hooks
devflow hook show <phase>                # Show hooks for a phase
devflow hook run <phase>                 # Run hooks manually
devflow hook approvals                   # List approved hooks
devflow hook approvals --clear           # Clear all approvals
```

### Plugins

```bash
devflow plugin list                      # List configured plugin backends
devflow plugin check <name>              # Verify a plugin backend
devflow plugin init <path>               # Generate a plugin scaffold
```

### Shell Integration

```bash
# Add to your shell profile for automatic worktree cd:
eval "$(devflow shell-init)"             # Auto-detects shell
eval "$(devflow shell-init bash)"        # Bash (~/.bashrc)
eval "$(devflow shell-init zsh)"         # Zsh (~/.zshrc)
devflow shell-init fish | source         # Fish (~/.config/fish/config.fish)
```

This creates a `devflow` shell wrapper that automatically `cd`s into worktree directories after `devflow switch`.

### Global Flags

```bash
--json                                   # JSON output for all commands
--non-interactive                        # Skip prompts, use defaults
-d <name>                                # Target a specific named backend
```

## Hooks

### Lifecycle Hooks

Hooks are MiniJinja-templated commands that run at specific lifecycle phases. Configure them in `.devflow.yml`:

```yaml
hooks:
  post-create:
    migrate: "npm run migrate"
    env-setup:
      command: "echo DATABASE_URL={{ service.app-db.url }} > .env.local"
      working_dir: "."

  post-switch:
    update-env:
      command: "echo DATABASE_URL={{ service.app-db.url }} > .env.local"

  pre-merge:
    test: "npm test"
```

Hooks are executed in definition order (deterministic, using ordered maps).

### Hook Phases

| Phase | Fires when... |
|---|---|
| `pre-switch` | Before switching to a branch |
| `post-create` | After creating a new branch |
| `post-start` | After starting a stopped branch |
| `post-switch` | After switching to a branch |
| `pre-remove` | Before removing a branch |
| `post-remove` | After removing a branch |
| `pre-commit` | Before committing (Git pre-commit hook) |
| `pre-merge` | Before merging branches |
| `post-merge` | After merging (Git post-merge hook) |
| `post-rewrite` | After rebase/amend (Git post-rewrite hook) |
| `pre-service-create` | Before creating a service branch |
| `post-service-create` | After creating a service branch |
| `pre-service-delete` | Before deleting a service branch |
| `post-service-delete` | After deleting a service branch |
| `post-service-switch` | After switching a service branch |

### Git Hooks Installed

`devflow install-hooks` installs four Git hooks:

- **post-checkout** — auto-create/switch service branches on `git checkout`
- **post-merge** — run post-merge hooks after `git merge`
- **pre-commit** — run pre-commit hooks before `git commit`
- **post-rewrite** — run post-rewrite hooks after `git rebase` or `git commit --amend`

### Template Variables

| Variable | Description |
|---|---|
| `{{ branch }}` | Current Git branch name |
| `{{ repo }}` | Repository directory name |
| `{{ worktree_path }}` | Worktree path (if enabled) |
| `{{ default_branch }}` | Default branch (main/master) |
| `{{ service.<name>.host }}` | Service host |
| `{{ service.<name>.port }}` | Service port |
| `{{ service.<name>.database }}` | Database name |
| `{{ service.<name>.user }}` | Service user |
| `{{ service.<name>.password }}` | Service password |
| `{{ service.<name>.url }}` | Full connection URL |

**Filters:** `sanitize` (replace `/` with `-`), `sanitize_db` (DB-safe identifier), `hash_port` (deterministic port in 10000-19999).

### Hook Approval

Hooks that change between runs require approval before execution. This prevents unexpected commands from running automatically via Git hooks. Manage approvals with `devflow hook approvals`.

### Legacy Post-Commands

Post-commands use a simpler `{variable}` syntax and are still supported:

```yaml
post_commands:
  # Simple command
  - "echo 'Database {db_name} ready!'"

  # Command with options
  - name: "Run migrations"
    command: "npm run migrate"
    working_dir: "./backend"
    condition: "file_exists:package.json"
    continue_on_error: false
    environment:
      DATABASE_URL: "postgresql://{db_user}@{db_host}:{db_port}/{db_name}"

  # File replacement
  - action: replace
    file: .env.local
    pattern: "DATABASE_URL=.*"
    replacement: "DATABASE_URL=postgresql://{db_user}@{db_host}:{db_port}/{db_name}"
    create_if_missing: true
```

**Post-command variables:** `{branch_name}`, `{db_name}`, `{db_host}`, `{db_port}`, `{db_user}`, `{db_password}`, `{template_db}`, `{prefix}`.

## Examples

Example configuration files are in the [`examples/`](examples/) directory:

- [`simple.devflow.yml`](examples/simple.devflow.yml) — Single PostgreSQL backend with post-commands
- [`multi-service.devflow.yml`](examples/multi-service.devflow.yml) — PostgreSQL + ClickHouse + Redis with lifecycle hooks and worktrees
- [`django.devflow.yml`](examples/django.devflow.yml) — Django project with migrations and Docker Compose restart

### Node.js / Express

```yaml
backend:
  type: local
  service_type: postgres
  local:
    image: postgres:17

hooks:
  post-create:
    env:
      command: "echo DATABASE_URL={{ service.url }} > .env.local"
    migrate: "npx prisma migrate deploy"

  post-switch:
    env:
      command: "echo DATABASE_URL={{ service.url }} > .env.local"
```

### Seeding

```bash
# Seed main from a production dump
devflow init myapp --from /path/to/dump.sql

# Seed from a live database
devflow init myapp --from postgresql://readonly:pass@replica:5432/mydb

# Seed from S3
devflow init myapp --from s3://my-bucket/backups/latest.dump

# Re-seed an existing branch
devflow seed main --from dump.sql
devflow seed feature/auth --from postgresql://...
```

### AI Agent / CI Automation

```bash
# Create an isolated branch for the agent
devflow --json --non-interactive create agent-task-42

# Get connection info
CONN=$(devflow --json connection agent-task-42 | jq -r '.connection_string')

# Agent works against isolated database...

# Reset to clean state if needed
devflow reset agent-task-42

# Check container logs on failure
devflow logs agent-task-42

# Clean up
devflow delete agent-task-42
```

## Workflows

### Typical Development Flow

1. **Start a feature:** `git checkout -b feature/auth`
2. **Automatic branching:** Git hooks create isolated services, run post-create hooks, set up worktree (if enabled)
3. **Develop:** make schema changes, test migrations — everything is isolated
4. **Switch context:** `git checkout main` — automatically switches back to main services
5. **Review a PR:** `git checkout feature/other` — services are created/switched automatically
6. **Interactive switch:** `devflow switch` — fuzzy search across all branches

### PR Review Workflow

```bash
git fetch origin
git checkout feature/payment-refactor    # Services created automatically
# Review, test, check logs if needed
devflow logs feature/payment-refactor
git checkout main                        # Switch back, services switch too
devflow delete feature/payment-refactor  # Clean up after merge
```

### AI Agent Workflow

```bash
# 1. Create isolated environment
BRANCH=$(devflow --json create task-123 | jq -r '.name')
CONN=$(devflow --json connection "$BRANCH" | jq -r '.connection_string')

# 2. Agent works against $CONN
# 3. Reset and retry if needed
devflow reset "$BRANCH"

# 4. Clean up
devflow delete "$BRANCH"
```

## Use Cases

- **Migration testing** — test schema migrations in isolation before merging
- **Feature development** — each feature branch gets its own database state
- **PR review** — switch to any branch and have the correct service state
- **AI agent sandboxing** — give each agent task isolated services with programmatic access
- **CI/CD preview environments** — spin up per-PR services, destroy on merge
- **Data migration testing** — seed from production, test migrations, reset, iterate
- **Parallel development** — multiple developers work without service conflicts

## Full Install

### Requirements

- **Local mode:** Docker
- **Template mode:** PostgreSQL server with template database access
- **Cloud modes:** API keys for Neon, DBLab, or Xata
- **Building from source:** Rust 1.70+, Git

### Ubuntu

```bash
# Install Docker
sudo apt-get update
sudo apt-get install -y docker.io
sudo usermod -aG docker $USER
newgrp docker

# Install Rust (if not already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# Build and install devflow
git clone https://github.com/clement-tourriere/devflow.git
cd devflow
cargo install --path .
```

### Optional: ZFS for Copy-on-Write (Linux)

If you're on ext4 (Ubuntu default) and want near-instant branching:

```bash
# Install ZFS
sudo apt-get install -y zfsutils-linux

# Option 1: Let devflow handle it (recommended)
# During init, devflow detects ZFS tools and offers to create a file-backed pool:
devflow init myapp
# → "ZFS tools detected but no ZFS pool found."
# → "Create a file-backed ZFS pool? (Y/n)"

# Option 2: Standalone setup
devflow setup-zfs                        # 10G pool named "devflow"
devflow setup-zfs --size 20G             # Custom size
devflow setup-zfs --pool-name mypool     # Custom name

# Option 3: Manual with a spare disk
sudo zpool create pgdata /dev/sdX
sudo zfs set mountpoint=/pgdata pgdata
sudo chown $USER:$USER /pgdata
```

devflow auto-detects ZFS by matching the data directory against `zfs list` mountpoints. Verify with `devflow doctor`.

### macOS

No special setup needed — APFS cloning is used automatically. Just install Docker Desktop and Rust, then `cargo install --path .`.

## License

MIT
