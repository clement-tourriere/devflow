#!/usr/bin/env bash
#
# generate-llms-txt.sh — Generate llms.txt and llms-full.txt from the devflow source.
#
# Usage:
#   ./scripts/generate-llms-txt.sh           # Generate both files
#   ./scripts/generate-llms-txt.sh --check   # Verify files are up to date (for CI)
#
# Requires: a built devflow binary (cargo build) or the ability to run cargo.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
LLMS_TXT="$ROOT_DIR/llms.txt"
LLMS_FULL_TXT="$ROOT_DIR/llms-full.txt"

CHECK_MODE=false
if [[ "${1:-}" == "--check" ]]; then
  CHECK_MODE=true
fi

# ---------------------------------------------------------------------------
# Find the devflow binary — prefer release, then debug, then build it.
# ---------------------------------------------------------------------------
find_devflow_bin() {
  if [[ -x "$ROOT_DIR/target/release/devflow" ]]; then
    echo "$ROOT_DIR/target/release/devflow"
  elif [[ -x "$ROOT_DIR/target/debug/devflow" ]]; then
    echo "$ROOT_DIR/target/debug/devflow"
  else
    echo "::info:: No devflow binary found, building..." >&2
    cargo build --manifest-path "$ROOT_DIR/Cargo.toml" --quiet 2>/dev/null
    echo "$ROOT_DIR/target/debug/devflow"
  fi
}

DEVFLOW_BIN="$(find_devflow_bin)"

# ---------------------------------------------------------------------------
# Extract CLI help for all commands
# ---------------------------------------------------------------------------
get_help() {
  # Run in a temp dir so devflow doesn't try to load .devflow.yml
  local tmpdir
  tmpdir="$(mktemp -d)"
  (cd "$tmpdir" && "$DEVFLOW_BIN" "$@" 2>/dev/null) || true
  rm -rf "$tmpdir"
}

get_main_help() {
  get_help --help
}

# Extract version from Cargo.toml
VERSION=$(grep '^version' "$ROOT_DIR/Cargo.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/')

# Extract hook phases from source
extract_hook_phases() {
  grep -oP '(?<=HookPhase::)\w+' "$ROOT_DIR/src/hooks/mod.rs" \
    | grep -v Custom \
    | sort -u \
    | while read -r variant; do
        # Convert PascalCase to kebab-case
        echo "$variant" | sed 's/\([a-z]\)\([A-Z]\)/\1-\2/g' | tr '[:upper:]' '[:lower:]'
      done
}

# Extract environment variables from CLAUDE.md / README.md
extract_env_vars() {
  grep -oP 'DEVFLOW_\w+' "$ROOT_DIR/README.md" | sort -u
}

# ---------------------------------------------------------------------------
# Generate llms.txt (curated index, follows llms.txt convention)
# ---------------------------------------------------------------------------
generate_llms_txt() {
  cat <<'HEADER'
# devflow

> Isolated development branch environments for every Git branch — databases, caches, and stateful services that sync with your Git workflow.

devflow maps each Git branch to an isolated set of services. When you
`git checkout feature-auth`, devflow automatically spins up (or switches to)
service instances that belong to that branch. Data is cloned using
Copy-on-Write for near-instant, space-efficient branching.

## Automation defaults

- Always pass `--json --non-interactive` for machine-safe execution.
- Use `--no-verify` on `switch` in headless environments to skip hook approval prompts.
- Use `devflow --json capabilities` to detect automation guarantees at runtime.
- Non-zero exit code means failure; partial multi-backend failures also return non-zero.

## Docs

HEADER

  # Links section — relative paths for local use, descriptions for context
  cat <<'LINKS'
- [README.md](README.md): Product overview, quickstart, configuration reference, CLI reference, and install guide.
- [AGENTS.md](AGENTS.md): Agent-first onboarding, bootstrap flow, suggested agent loop, and automation contract.
- [CLAUDE.md](CLAUDE.md): Project structure, config schema, development commands, and AI agent context.
- [CHANGELOG.md](CHANGELOG.md): Version history and release notes.
- [docs/index.html](docs/index.html): Full single-page documentation site with search, dark/light theme.

## Agent workflows

- [examples/agent-bootstrap.sh](examples/agent-bootstrap.sh): Idempotent repository setup for agents and CI.
- [examples/agent-task.sh](examples/agent-task.sh): Create/switch to a task-scoped branch environment, fetch connection info.
- [examples/simple.devflow.yml](examples/simple.devflow.yml): Minimal single-service configuration.
- [examples/multi-service.devflow.yml](examples/multi-service.devflow.yml): Multi-service setup with hooks and worktrees.
- [examples/django.devflow.yml](examples/django.devflow.yml): Django project config with migrations and Docker Compose.

## Source (for code agents)

- [src/cli.rs](src/cli.rs): Command routing, JSON/non-interactive behavior, multi-backend orchestration.
- [src/config/mod.rs](src/config/mod.rs): Config loading, 3-tier merging, env var overrides, validation.
- [src/services/mod.rs](src/services/mod.rs): `ServiceBackend` trait — the interface all backends implement.
- [src/services/factory.rs](src/services/factory.rs): Backend creation, dispatch, multi-backend orchestration.
- [src/hooks/executor.rs](src/hooks/executor.rs): Hook execution engine, approval checks, conditions.
- [src/hooks/template.rs](src/hooks/template.rs): MiniJinja template engine with custom filters.
- [src/vcs/mod.rs](src/vcs/mod.rs): VCS abstraction (Git + Jujutsu), auto-detection.
- [src/services/plugin.rs](src/services/plugin.rs): Plugin backend protocol (JSON-over-stdio).

## Optional

- [DEVFLOW_PLAN.md](DEVFLOW_PLAN.md): Historical design notes and architecture decisions.
- [llms-full.txt](llms-full.txt): Comprehensive agent context (all commands, config schema, hook phases).
LINKS
}

# ---------------------------------------------------------------------------
# Generate llms-full.txt (comprehensive context dump for LLM ingestion)
# ---------------------------------------------------------------------------
generate_llms_full_txt() {
  cat <<INTRO
# devflow — Full Agent Context

> Version: $VERSION
> Repository: https://github.com/clement-tourriere/devflow
> License: MIT

This file provides comprehensive context for AI agents and LLMs working with
the devflow codebase. It is auto-generated by \`scripts/generate-llms-txt.sh\`.

## What devflow does

devflow creates isolated development branch environments for every Git branch.
An environment can include one or more stateful services (PostgreSQL,
ClickHouse, MySQL, generic Docker containers, cloud backends, plugins),
optional Git worktree management, and lifecycle hooks.

Four backend modes:
- **Local** — Docker containers with Copy-on-Write storage (APFS, ZFS, Btrfs, XFS)
- **Template** — PostgreSQL \`CREATE DATABASE ... WITH TEMPLATE\` on existing server
- **Cloud** — Neon, DBLab, or Xata APIs
- **Plugin** — Custom backends via JSON-over-stdio protocol

Five service types: postgres, clickhouse, mysql, generic (any Docker image), plugin.

## Automation contract

- Pass \`--json --non-interactive\` for all machine executions.
- Use \`--no-verify\` with \`switch\` in headless runs unless hooks are pre-approved.
- Non-zero exit code on any failure; partial multi-backend failures also return non-zero.
- \`destroy\` and \`remove\` require \`--force\` in \`--json\` or \`--non-interactive\` mode.
- Hook approvals are required; unapproved hooks fail in non-interactive mode.
- \`devflow --json capabilities\` returns a machine-readable contract summary.

## Minimal agent loop

\`\`\`bash
TASK_ID="issue-123"
BRANCH="agent/\$TASK_ID"

devflow --json --non-interactive switch "\$BRANCH" --no-verify
CONN=\$(devflow --json connection "\$BRANCH" | jq -r '.connection_string')

# run task against \$CONN ...

devflow --json --non-interactive reset "\$BRANCH"    # optional retry
devflow --json --non-interactive delete "\$BRANCH"   # cleanup
\`\`\`

INTRO

  # ---- CLI REFERENCE ----
  cat <<'SECTION'
## CLI commands

### Global flags

| Flag | Description |
|---|---|
| `--json` | JSON output for automation commands |
| `--non-interactive` | Skip prompts, use defaults |
| `-d <name>` | Target a specific named backend |

### Branch management

| Command | Description |
|---|---|
| `devflow create <branch> [--from <parent>]` | Create a new service branch |
| `devflow delete <branch>` | Delete service branch (keeps Git branch + worktree) |
| `devflow list` | List all branches with service + worktree status |
| `devflow switch [<branch>] [-c] [--base <b>] [-x <cmd>] [--no-services] [--no-verify] [--template] [--dry-run]` | Switch to a branch (interactive picker if no arg) |
| `devflow remove <branch> [--force] [--keep-services]` | Remove branch + worktree + all service branches |
| `devflow cleanup [--max-count N]` | Remove old branches, keep most recent N |

### Lifecycle (local backend)

| Command | Description |
|---|---|
| `devflow start <branch>` | Start a stopped container |
| `devflow stop <branch>` | Stop a running container |
| `devflow reset <branch>` | Reset branch data to parent state |
| `devflow destroy [--force]` | Remove all containers and data |
| `devflow seed <branch> --from <source>` | Seed from PostgreSQL URL, file, or s3:// |
| `devflow logs <branch> [--tail N]` | Show container logs |

### VCS

| Command | Description |
|---|---|
| `devflow merge [<target>] [--cleanup] [--dry-run]` | Merge current branch into target |
| `devflow commit [-m <msg>] [--ai] [--edit] [--dry-run]` | Commit staged changes |

### Info & diagnostics

| Command | Description |
|---|---|
| `devflow connection <branch> [--format uri\|env\|json]` | Connection info |
| `devflow status` | Project and backend status |
| `devflow capabilities` | Machine-readable automation contract |
| `devflow config [-v]` | Current configuration |
| `devflow doctor` | System health check |
| `devflow logs <branch> [--tail N]` | Container logs |

### Setup

| Command | Description |
|---|---|
| `devflow init [name] [--backend <type>] [--from <source>]` | Initialize configuration |
| `devflow install-hooks` | Install Git hooks |
| `devflow uninstall-hooks` | Remove Git hooks |
| `devflow setup-zfs [--pool-name <n>] [--size <s>]` | Create file-backed ZFS pool |
| `devflow shell-init [bash\|zsh\|fish]` | Print shell integration script |
| `devflow worktree-setup` | Set up devflow in a Git worktree |

### Hooks

| Command | Description |
|---|---|
| `devflow hook show [<phase>]` | Show configured hooks |
| `devflow hook run <phase> [<name>] [--branch <b>]` | Run hooks manually |
| `devflow hook approvals list` | List approved hooks |
| `devflow hook approvals add <cmd>` | Approve a hook command |
| `devflow hook approvals clear` | Clear all approvals |

### Plugins

| Command | Description |
|---|---|
| `devflow plugin list` | List configured plugin backends |
| `devflow plugin check <name>` | Verify a plugin backend |
| `devflow plugin init <name> [--lang bash\|python]` | Print plugin scaffold |

SECTION

  # ---- CONFIGURATION SCHEMA ----
  cat <<'SECTION'
## Configuration schema (.devflow.yml)

All sections are optional. An empty file is valid.

```yaml
git:
  auto_create_on_branch: true         # Auto-create service branch on git checkout
  auto_switch_on_branch: true         # Auto-switch services on git checkout
  main_branch: main                   # Main git branch (auto-detected on init)
  branch_filter_regex: "^feature/.*"  # Only branch for matching patterns
  exclude_branches: [main, master]    # Never create branches for these

behavior:
  auto_cleanup: false                 # Auto-cleanup old branches
  max_branches: 10                    # Max branches before cleanup
  naming_strategy: prefix             # prefix, suffix, or replace

backends:
  - name: app-db                      # Backend identifier
    type: local                       # local, postgres_template, neon, dblab, xata
    service_type: postgres            # postgres, clickhouse, mysql, generic, plugin
    auto_branch: true                 # Branch this service with git (default: true)
    default: true                     # Default target for -d flag
    local:                            # Local Docker backend config
      image: postgres:17
      data_root: null                 # Custom data directory (default: ~/.local/share/devflow/)
      storage: null                   # Force storage: zfs, apfs_clone, reflink, copy
      port_range_start: null          # Port allocation start
      postgres_user: null             # PG superuser (default: postgres)
      postgres_password: null         # PG password
      postgres_db: null               # Default database name

  - name: analytics
    type: local
    service_type: clickhouse
    clickhouse:
      image: clickhouse/clickhouse-server:latest
      port_range_start: null
      data_root: null
      user: default
      password: null

  - name: app-mysql
    type: local
    service_type: mysql
    mysql:
      image: mysql:8
      root_password: dev
      database: null
      user: null
      password: null

  - name: cache
    type: local
    service_type: generic
    auto_branch: false                # Shared across branches
    generic:
      image: redis:7-alpine
      port_mapping: "6379:6379"
      port_range_start: null
      environment: {}
      volumes: []
      command: null
      healthcheck: null

  - name: my-plugin
    type: local
    service_type: plugin
    plugin:
      path: ./plugins/my-plugin.sh   # Plugin executable path
      name: my-plugin                 # Or resolve as devflow-plugin-{name} on PATH
      timeout: 30                     # Seconds per invocation
      config: {}                      # Opaque JSON passed to plugin

  - name: cloud-db
    type: neon
    neon:
      api_key: "..."
      project_id: "..."
      base_url: "https://console.neon.tech/api/v2"

  - name: dblab-db
    type: dblab
    dblab:
      api_url: "https://..."
      auth_token: "..."

  - name: xata-db
    type: xata
    xata:
      api_key: "..."
      organization_id: "..."
      project_id: "..."
      base_url: "https://api.xata.tech"

worktree:
  enabled: true
  path_template: "../{repo}.{branch}" # Supports {repo}, {branch} placeholders
  copy_files: [".env.local", ".env"]
  copy_ignored: true                  # Copy gitignored files too

hooks:
  post-create:
    migrate: "npm run migrate"        # Simple form
    env-setup:                        # Extended form
      command: "echo DATABASE_URL={{ service['app-db'].url }} > .env.local"
      working_dir: "."
      condition: "always"             # always, never, or MiniJinja expression
      continue_on_error: false
      background: false
  post-switch:
    update-env:
      command: "echo DATABASE_URL={{ service['app-db'].url }} > .env.local"
  pre-merge:
    test: "npm test"
```

### Configuration hierarchy (highest to lowest precedence)

1. **Environment variables** — Quick toggles and overrides
2. **`.devflow.local.yml`** — Project-specific local overrides (gitignored)
3. **`.devflow.yml`** — Team shared configuration

### Environment variables

| Variable | Description |
|---|---|
| `DEVFLOW_DISABLED=true` | Completely disable devflow |
| `DEVFLOW_SKIP_HOOKS=true` | Skip Git hook execution |
| `DEVFLOW_AUTO_CREATE=false` | Override auto_create_on_branch |
| `DEVFLOW_AUTO_SWITCH=false` | Override auto_switch_on_branch |
| `DEVFLOW_BRANCH_FILTER_REGEX=...` | Override branch filtering |
| `DEVFLOW_DISABLED_BRANCHES=main,release/*` | Disable for specific branches |
| `DEVFLOW_CURRENT_BRANCH_DISABLED=true` | Disable for current branch only |
| `DEVFLOW_DATABASE_HOST=...` | Override database host |
| `DEVFLOW_DATABASE_PORT=...` | Override database port |
| `DEVFLOW_DATABASE_USER=...` | Override database user |
| `DEVFLOW_DATABASE_PASSWORD=...` | Override database password |
| `DEVFLOW_DATABASE_PREFIX=...` | Override database prefix |
| `DEVFLOW_ZFS_DATASET=...` | Force a specific ZFS dataset |
| `DEVFLOW_LLM_API_KEY=...` | API key for AI commit messages |
| `DEVFLOW_LLM_API_URL=...` | LLM endpoint URL (OpenAI-compatible) |
| `DEVFLOW_LLM_MODEL=...` | LLM model name |

SECTION

  # ---- HOOK PHASES ----
  cat <<'SECTION'
## Hook lifecycle phases

| Phase | When it fires | Blocking? |
|---|---|---|
| `pre-switch` | Before switching to a branch | Yes |
| `post-create` | After creating a new branch | Yes |
| `post-start` | After starting a stopped branch | No |
| `post-switch` | After switching to a branch | No |
| `pre-remove` | Before removing a branch | Yes |
| `post-remove` | After removing a branch | No |
| `pre-commit` | Before committing (Git pre-commit) | Yes |
| `pre-merge` | Before merging branches | Yes |
| `post-merge` | After merging (Git post-merge) | No |
| `post-rewrite` | After rebase/amend (Git post-rewrite) | No |
| `pre-service-create` | Before creating a service branch | Yes |
| `post-service-create` | After creating a service branch | No |
| `pre-service-delete` | Before deleting a service branch | Yes |
| `post-service-delete` | After deleting a service branch | No |
| `post-service-switch` | After switching a service branch | No |

### Hook template variables

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

### Custom template filters

| Filter | Description | Example |
|---|---|---|
| `sanitize` | Replace `/` with `-` | `{{ branch \| sanitize }}` → `feature-auth` |
| `sanitize_db` | DB-safe: replace non-alphanumeric with `_` | `{{ branch \| sanitize_db }}` → `feature_auth` |
| `hash_port` | Deterministic port in 10000-19999 | `{{ branch \| hash_port }}` → `14523` |

SECTION

  # ---- COPY-ON-WRITE STORAGE ----
  cat <<'SECTION'
## Copy-on-Write storage backends

| Filesystem | Platform | Method | Auto-detected? |
|---|---|---|---|
| APFS | macOS | `cp -c` clone | Yes |
| ZFS | Linux | Snapshots + clones | Yes (checks `zfs list` mountpoints) |
| Btrfs | Linux | Reflink copy | Yes |
| XFS | Linux | Reflink copy | Yes (if created with `reflink=1`) |
| ext4 / other | Any | Full copy (fallback) | Yes |

## Project structure

```
src/
  main.rs               CLI entry point (clap derive)
  cli.rs                All command implementations (~4500 lines)
  config/mod.rs         Config types, 3-tier merging, validation
  services/
    mod.rs              ServiceBackend trait
    factory.rs          Backend creation + multi-backend orchestration
    postgres/
      local/            Docker backend (mod.rs, docker.rs, state.rs, storage/, seed.rs)
      template.rs       PostgreSQL TEMPLATE backend
      neon.rs           Neon cloud backend
      dblab.rs          DBLab cloud backend
      xata.rs           Xata cloud backend
    clickhouse/local.rs ClickHouse Docker backend
    mysql/local.rs      MySQL Docker backend
    generic/mod.rs      Generic Docker backend
    plugin.rs           Plugin backend (JSON-over-stdio)
  hooks/
    mod.rs              HookPhase, HookEntry, HooksConfig types
    executor.rs         HookEngine execution with approval/conditions
    template.rs         MiniJinja TemplateEngine with custom filters
    approval.rs         ApprovalStore (YAML persistence with file locking)
  vcs/
    mod.rs              VcsProvider trait (Git + Jujutsu auto-detection)
    git.rs              Git implementation (git2 crate)
    jj.rs               Jujutsu implementation (jj CLI)
    cow_worktree.rs     CoW worktree creation
  state/local_state.rs  User-level state (SQLite)
  docker/compose.rs     Docker Compose file parsing
  database.rs           PostgreSQL template backend DB operations
  llm.rs                LLM integration for AI commit messages
```

## Primary references

- `AGENTS.md` — Agent onboarding and automation contract
- `README.md` — Product overview, quickstart, full reference
- `CLAUDE.md` — Developer context and project structure
- `CHANGELOG.md` — Version history
- `docs/index.html` — Full documentation site
SECTION
}

# ---------------------------------------------------------------------------
# Main logic
# ---------------------------------------------------------------------------
if $CHECK_MODE; then
  # Generate to temp files and diff
  TMPDIR="$(mktemp -d)"
  trap 'rm -rf "$TMPDIR"' EXIT

  generate_llms_txt > "$TMPDIR/llms.txt"
  generate_llms_full_txt > "$TMPDIR/llms-full.txt"

  EXIT_CODE=0
  if ! diff -q "$LLMS_TXT" "$TMPDIR/llms.txt" >/dev/null 2>&1; then
    echo "ERROR: llms.txt is out of date. Run: mise run generate-llms" >&2
    diff -u "$LLMS_TXT" "$TMPDIR/llms.txt" >&2 || true
    EXIT_CODE=1
  fi
  if ! diff -q "$LLMS_FULL_TXT" "$TMPDIR/llms-full.txt" >/dev/null 2>&1; then
    echo "ERROR: llms-full.txt is out of date. Run: mise run generate-llms" >&2
    diff -u "$LLMS_FULL_TXT" "$TMPDIR/llms-full.txt" >&2 || true
    EXIT_CODE=1
  fi
  if [ $EXIT_CODE -eq 0 ]; then
    echo "llms.txt and llms-full.txt are up to date."
  fi
  exit $EXIT_CODE
else
  generate_llms_txt > "$LLMS_TXT"
  generate_llms_full_txt > "$LLMS_FULL_TXT"
  echo "Generated: llms.txt ($(wc -l < "$LLMS_TXT") lines)"
  echo "Generated: llms-full.txt ($(wc -l < "$LLMS_FULL_TXT") lines)"
fi
