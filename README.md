# devflow

Isolated dev environments for every Git branch — automatically.

> **[Full Documentation](docs/index.html)** | **[AI Agent Guide](AGENTS.md)** | **[Changelog](CHANGELOG.md)**

## What It Does

devflow gives each Git branch its own isolated development environment: worktrees, databases, caches, and any stateful service. When you `git checkout feature-auth`, devflow automatically creates (or switches to) a dedicated worktree and spins up PostgreSQL, ClickHouse, MySQL, or Redis instances that belong to that branch. Data is cloned from the parent branch using Copy-on-Write, so branching is near-instant and uses almost no extra disk space.

It works in five modes:
- **Local** — Docker containers with CoW storage (APFS, ZFS, Btrfs, XFS)
- **Template** — PostgreSQL's `CREATE DATABASE ... WITH TEMPLATE` on an existing server
- **Cloud** — Neon, DBLab, or Xata APIs
- **Plugin** — custom backends via JSON-over-stdio protocol
- **AI-Ready** — `--json` output, `llms.txt`, `AGENTS.md`, and agent scripts for autonomous coding agents

Copy-on-Write storage (APFS, ZFS, Btrfs, XFS) is also applied to worktree directories, making branch switching fast and space-efficient.

## Install

```bash
git clone https://github.com/clement-tourriere/devflow.git
cd devflow
mise trust && mise install   # installs the Rust toolchain
cargo install --path .
```

Requires [mise](https://mise.jdx.dev) (or Rust 1.70+ installed manually) and Docker or [OrbStack](https://orbstack.dev) (for local mode). See [Full Install](#full-install) for platform-specific instructions.

## Quick Start

```bash
# 1. Initialize project config
devflow init myapp

# 2. Add at least one service
devflow service add app-db --provider local --service-type postgres

# 3. Install Git hooks (auto-create/switch on checkout)
devflow install-hooks

# 4. Create and switch to a feature environment
devflow switch -c feature/auth

# 5. Check what's running
devflow status

# 6. Get connection info
devflow connection feature/auth
devflow connection feature/auth --format env    # DATABASE_URL=...
```

That's it. Your feature branch now has its own database. Schema changes, test data, and migrations are completely isolated from main.

### Common Getting Started Scenarios

#### 1) New project (default)

```bash
cd ~/workspace
devflow init myapp
cd myapp

devflow service add app-db --provider local --service-type postgres
devflow install-hooks
```

#### 2) Existing project

```bash
cd ~/workspace/my-existing-project
devflow init

devflow service add app-db --provider local --service-type postgres
devflow install-hooks
```

#### 3) Add a service with or without seed data

```bash
# No seed (fresh main data)
devflow service add app-db --provider local --service-type postgres

# Seed main branch from a dump
devflow service add app-db --provider local --service-type postgres --from ./backup.sql

# Seed main branch from a running PostgreSQL instance
devflow service add app-db --provider local --service-type postgres --from postgresql://user:pass@localhost:5432/myapp

# Seed main branch from S3
devflow service add app-db --provider local --service-type postgres --from s3://my-bucket/backups/latest.dump

# Get connection info for your app
devflow connection main --format env
```

With shell integration enabled (`eval "$(devflow shell-init)"`), commands that
emit `DEVFLOW_CD=...` auto-`cd` your shell (for example `init`, `switch`, and
TUI open with `o`).

Every branch created from main inherits seeded data via Copy-on-Write. See the [full documentation](docs/index.html#existing-project) for a detailed walkthrough including local overrides with `.devflow.local.yml`.

### Using mise

devflow ships with a [`mise.toml`](mise.toml) for [mise](https://mise.jdx.dev) users. mise manages the Rust toolchain and provides task shortcuts:

```bash
mise install          # Install Rust toolchain
mise run build        # Build devflow
mise run test         # Run all tests
mise run docs         # Serve documentation locally at localhost:8787
```

## How It Works

### Local Backend

1. `devflow init` creates `.devflow.yml` and configures branch/worktree behavior
2. `devflow service add app-db ...` registers a service and provisions main-branch service state
3. `devflow switch -c feature-auth` creates the branch environment (and worktree when enabled)
4. `devflow service delete feature-auth` removes service data for that branch (`devflow remove` removes git branch + worktree + services)

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

Custom backends can be built as standalone executables that communicate via JSON-over-stdio. Run `devflow plugin init <name>` to print a scaffold script.

## Configuration

### `.devflow.yml`

Created by `devflow init`. All sections are optional.

#### Services

```yaml
services:
  - name: app-db
    type: local
    service_type: postgres
    auto_branch: true               # Branch this service with git
    default: true                   # Default target for -s flag
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
  branch_filter_regex: "^feature/.*"  # Only branch for matching patterns
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
devflow switch -c <branch>               # Create + switch (parent = context branch)
devflow switch -c <branch> --from <p>    # Create from explicit parent
devflow link <branch>                    # Link an existing VCS branch into devflow
devflow service create <branch>          # Create service branch only
devflow service delete <branch>          # Delete service branch only
devflow service cleanup --max-count 5    # Cleanup old branches for a service
devflow remove <branch>                  # Remove branch + worktree + all services
devflow list                             # List all branches (tree view)
devflow graph                            # Full environment graph (human view)
devflow --json graph                     # Full environment graph (machine view)
devflow switch                           # Interactive switch with fuzzy search
devflow switch <branch>                  # Switch to an existing branch/worktree
devflow switch --template                # Switch to main/template
devflow cleanup --max-count 5            # Alias for `devflow service cleanup`
```

### Lifecycle (Local Backend)

```bash
devflow service start <branch>           # Start a stopped container
devflow service stop <branch>            # Stop a running container
devflow service reset <branch>           # Reset branch data to parent state
devflow service destroy                  # Remove all data for a service
devflow service destroy --force          # Skip confirmation
devflow service seed <branch> --from <source>  # Seed from PostgreSQL URL, file, or s3://
devflow service logs <branch>            # Show container logs (last 100 lines)
devflow service logs <branch> --tail 50  # Show last 50 lines
```

### VCS

```bash
devflow merge <target>                   # Merge current branch into target
devflow commit                           # Commit staged changes
devflow commit --ai                      # AI-generated commit message
```

### Info & Diagnostics

```bash
devflow status                           # Project and service status
devflow config                           # Current configuration
devflow config -v                        # Config with precedence details
devflow doctor                           # System health check
devflow capabilities                     # Automation contract summary
devflow service capabilities             # Service provider capability matrix
devflow connection <branch>              # Connection URI (default)
devflow connection <branch> --format env # Environment variables
devflow connection <branch> --format json # JSON object
```

### Context Override

```bash
DEVFLOW_CONTEXT_BRANCH=release_1_0 devflow switch -c hotfix_patch
```

When set, `DEVFLOW_CONTEXT_BRANCH` defines the devflow context branch used as
the default parent for branch creation.

### TUI Dashboard

```bash
devflow tui
```

The TUI now includes:

- **Environments**: tree view with parent/child branches, service states, focused-service actions, start/stop-all shortcuts, and `o` to open a branch/worktree and exit.
- **System**: consolidated config, hooks (with template variable/filter reference + scaffold snippets), and doctor panels.
- **Logs**: service/branch picker with filter support and keyboard-driven navigation.

### Setup

```bash
devflow init [path]                      # Initialize current dir or create/init path
devflow init [path] --name <project>     # Explicit project name
devflow init [path] --force              # Overwrite existing config
devflow service add <name> --provider <type> --service-type <kind>
devflow service add <name> --provider local --service-type postgres --from <source>
devflow install-hooks                    # Install Git hooks
devflow uninstall-hooks                  # Remove Git hooks
devflow setup-zfs                        # Create file-backed ZFS pool (Linux)
devflow setup-zfs --size 20G             # Custom pool size
devflow worktree-setup                   # Set up devflow in a Git worktree
```

With shell integration enabled, `devflow init <directory>` also emits
`DEVFLOW_CD=...`, so your shell wrapper can automatically `cd` into the
newly initialized directory.

### Hooks

```bash
devflow hook show                        # Show all configured hooks
devflow hook show <phase>                # Show hooks for a phase
devflow hook run <phase>                 # Run hooks manually
devflow hook approvals                   # List approved hooks
devflow hook approvals clear             # Clear all approvals
```

### Plugins

```bash
devflow plugin list                      # List configured plugin backends
devflow plugin check <name>              # Verify a plugin backend
devflow plugin init <name>               # Print a plugin scaffold script
```

### Shell Integration

```bash
# Add to your shell profile for automatic worktree cd:
eval "$(devflow shell-init)"             # Auto-detects shell
eval "$(devflow shell-init bash)"        # Bash (~/.bashrc)
eval "$(devflow shell-init zsh)"         # Zsh (~/.zshrc)
devflow shell-init fish | source         # Fish (~/.config/fish/config.fish)
```

This creates a `devflow` shell wrapper that automatically `cd`s when devflow
emits `DEVFLOW_CD=...` (for example after `devflow switch`, `devflow init <dir>`,
or opening a branch/worktree from the TUI with `o`).

### Global Flags

```bash
--json                                   # JSON output for core automation commands
--non-interactive                        # Skip prompts, use defaults
-s <name>                                # Target a specific named service
```

### Agent Automation Contract

- Multi-provider `service create`, `service delete`, and `switch` return non-zero when any provider fails.
- `destroy` and `remove` require `--force` when using `--json` or `--non-interactive`.
- Unapproved hooks fail in non-interactive mode (no prompts).
- Use `devflow --json capabilities` to detect current automation guarantees.

## Hooks

### Lifecycle Hooks

Hooks are MiniJinja-templated commands that run at specific lifecycle phases. Configure them in `.devflow.yml`:

```yaml
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
| `{{ commit }}` | HEAD commit SHA (when available) |
| `{{ target }}` | Merge target branch (merge hooks) |
| `{{ base }}` | Parent/base branch (create hooks) |
| `{{ service['<name>'].host }}` | Service host |
| `{{ service['<name>'].port }}` | Service port |
| `{{ service['<name>'].database }}` | Database name |
| `{{ service['<name>'].user }}` | Service user |
| `{{ service['<name>'].password }}` | Service password |
| `{{ service['<name>'].url }}` | Full connection URL |

**Filters:** `sanitize`, `sanitize_db`, `hash_port`, `lower`, `upper`, `replace`, `truncate`.

### Hook Approval

Hooks that change between runs require approval before execution. This prevents unexpected commands from running automatically via Git hooks. In `--non-interactive` mode, unapproved hooks fail instead of prompting. Manage approvals with `devflow hook approvals`.

## Examples

Example configuration files are in the [`examples/`](examples/) directory:

- [`simple.devflow.yml`](examples/simple.devflow.yml) — Single PostgreSQL service
- [`multi-service.devflow.yml`](examples/multi-service.devflow.yml) — PostgreSQL + ClickHouse + Redis services with lifecycle hooks and worktrees
- [`django.devflow.yml`](examples/django.devflow.yml) — Django project with migrations and Docker Compose restart
- [`agent-bootstrap.sh`](examples/agent-bootstrap.sh) — Idempotent repository bootstrap for agents/CI
- [`agent-task.sh`](examples/agent-task.sh) — Task-scoped branch environment setup for agents

### Node.js / Express

```yaml
services:
  - name: app-db
    type: local
    service_type: postgres
    default: true
    local:
      image: postgres:17

hooks:
  post-create:
    env:
      command: "echo DATABASE_URL={{ service['app-db'].url }} > .env.local"
    migrate: "npx prisma migrate deploy"

  post-switch:
    env:
      command: "echo DATABASE_URL={{ service['app-db'].url }} > .env.local"
```

### Seeding

```bash
# Seed while adding a service
devflow service add app-db --provider local --service-type postgres --from /path/to/dump.sql
devflow service add app-db --provider local --service-type postgres --from postgresql://readonly:pass@replica:5432/mydb
devflow service add app-db --provider local --service-type postgres --from s3://my-bucket/backups/latest.dump

# Re-seed an existing branch
devflow service seed main --from dump.sql
devflow service seed feature/auth --from postgresql://...
```

### AI Agent / CI Automation

```bash
# One-time bootstrap (idempotent)
./examples/agent-bootstrap.sh

# Create an isolated branch for the agent
devflow --json --non-interactive switch -c agent-task-42 --no-verify

# Get connection info
CONN=$(devflow --json connection agent-task-42 | jq -r '.connection_string')

# Agent works against an isolated development branch environment...

# Reset to clean state if needed
devflow --json --non-interactive service reset agent-task-42

# Check container logs on failure
devflow service logs agent-task-42

# Clean up
devflow --json --non-interactive remove agent-task-42 --force
```

For a full agent-oriented workflow, see `AGENTS.md`.

```bash
# Quick setup for a new subject
./examples/agent-bootstrap.sh
./examples/agent-task.sh issue-123
```

### LLM-Friendly Docs

- `llms.txt` — curated index of agent-relevant project resources
- `llms-full.txt` — compact context summary for local agent ingestion

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
devflow service logs feature/payment-refactor
git checkout main                        # Switch back, services switch too
devflow remove feature/payment-refactor --force  # Clean up after merge
```

### AI Agent Workflow

```bash
# 1. Create isolated environment
BRANCH="task-123"
devflow --json --non-interactive switch -c "$BRANCH" --no-verify >/dev/null
CONN=$(devflow --json connection "$BRANCH" | jq -r '.connection_string')

# 2. Agent works against $CONN
# 3. Reset and retry if needed
devflow --json --non-interactive service reset "$BRANCH"

# 4. Clean up
devflow --json --non-interactive remove "$BRANCH" --force
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
