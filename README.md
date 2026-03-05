<p align="center">
  <img src="docs/icon.png" alt="devflow" width="128" />
</p>

<h1 align="center">devflow</h1>

<p align="center">Isolated dev environments for every workspace — automatically.</p>

> [Full Documentation](docs/index.html) | [CLI Reference](docs/CLI.md) | [AI Agent Guide](AGENTS.md) | [Changelog](CHANGELOG.md)

## What is devflow?

devflow gives each workspace its own isolated development environment: databases, caches, worktrees, and any stateful service. When you `git checkout feature-auth`, devflow automatically creates or switches to dedicated service instances that belong to that workspace. Data is cloned from the parent using Copy-on-Write, so creating a workspace is near-instant and costs almost no extra disk space. It works via CLI, interactive TUI, or desktop GUI.

## Features

- Per-workspace isolated services (PostgreSQL, ClickHouse, MySQL, Redis, any Docker image)
- Automatic Git sync — services create and switch with your checkout
- Copy-on-Write cloning (APFS, ZFS, Btrfs, XFS) — instant, space-efficient
- Git worktree management with CoW directory cloning
- Native HTTPS reverse proxy with auto-discovered `*.localhost` domains
- Desktop GUI (Tauri 2 + React) for graphical management
- Interactive TUI dashboard with workspace and service control
- Lifecycle hooks with MiniJinja templates
- AI agent integration and AI-powered commit messages
- Cloud providers (Neon, DBLab, Xata) and custom plugin system
- JSON output and non-interactive mode for CI/CD and agent automation

## Install

```bash
git clone https://github.com/clement-tourriere/devflow.git
cd devflow
cargo install --path .
```

Requires Rust 1.70+ and Docker (for local mode). See [Full Install](#full-install) for platform-specific instructions.

## Quick Start

```bash
# 1. Initialize (guided wizard handles services, hooks, and shell integration)
cd ~/my-project
devflow init

# 2. Create your first workspace
devflow switch -c feature/auth

# 3. Get connection info
devflow status
devflow connection feature/auth --format env
```

Your feature workspace now has its own database. Schema changes, test data, and migrations are completely isolated from main.

### Adding to an existing project

```bash
cd ~/my-existing-project
devflow init    # Guided wizard offers service setup, hooks, and shell integration
```

### Seeding with data

```bash
# From a dump file
devflow service add app-db --provider local --service-type postgres --from ./backup.sql

# From a running database
devflow service add app-db --provider local --service-type postgres --from postgresql://user:pass@host/db

# From S3
devflow service add app-db --provider local --service-type postgres --from s3://bucket/backups/latest.dump
```

Every workspace created from main inherits seeded data via Copy-on-Write.

## Concepts

| Concept | Description |
|---------|-------------|
| **Workspace** | An isolated development environment corresponding to a Git branch. Each workspace gets its own service instances and optionally its own worktree directory. (This is a devflow concept, not a built-in Git feature.) |
| **Service** | A stateful backend (database, cache, queue) managed per workspace. Services are configured in `.devflow.yml`. |
| **Worktree** | An optional Git worktree directory for a workspace, enabling true parallel development without stashing. |
| **Provider** | The backend that manages service instances: Local (Docker), Neon, DBLab, Xata, or Plugin. |
| **Hook** | A MiniJinja-templated command that runs at lifecycle events (post-create, post-switch, pre-merge, etc.). |

## Desktop GUI

The desktop GUI (Tauri 2 + React) provides graphical management of projects, workspaces, services, hooks, proxy, and configuration.

```bash
mise run gui          # Development mode with hot-reload
mise run gui:build    # Production bundle
```

Key features:

- Dashboard with project overview and proxy status
- Workspace management with create, switch, delete, and connection info
- Service lifecycle control (start, stop, reset, logs)
- Hook editor with MiniJinja template preview and variable browser
- Proxy dashboard with container discovery and one-click CA trust
- Section-based configuration editor (no raw YAML needed)
- System tray with quick access to projects and workspaces

Requires [bun](https://bun.sh) and the [Tauri CLI](https://v2.tauri.app/start/prerequisites/).

## TUI Dashboard

```bash
devflow tui
```

Three-tab interactive terminal dashboard:

- **Environments** — workspace tree with service states, start/stop controls, press `o` to open a workspace
- **System** — configuration, hooks (with template reference and scaffold snippets), and diagnostics
- **Logs** — service log viewer with workspace/service picker and keyboard navigation

## Reverse Proxy

devflow includes a native HTTPS reverse proxy that auto-discovers Docker containers and serves them via `*.localhost` domains.

```bash
devflow proxy start                      # Start the proxy
devflow proxy trust install              # Trust the CA (one-time)
devflow proxy list                       # See proxied containers
```

| Container Type | Domain Pattern |
|---|---|
| Standalone | `container_name.localhost` |
| Compose service | `service.project.localhost` |
| devflow service | `service.workspace.project.localhost` |
| Custom label | value of `devproxy.domain` label |

Certificates are auto-generated using a local CA. After `devflow proxy trust install`, all `*.localhost` domains work with HTTPS — no browser warnings or `-k` flags needed.

## Configuration

Created by `devflow init`. All sections are optional — an empty file is valid.

```yaml
services:
  - name: app-db
    type: local
    service_type: postgres
    default: true
    local:
      image: postgres:17

git:
  auto_create_on_workspace: true
  auto_switch_on_workspace: true
  main_workspace: main

worktree:
  enabled: true
  path_template: "../{repo}.{workspace}"
  copy_files: [".env.local", ".env"]

hooks:
  post-create:
    env:
      command: "echo DATABASE_URL={{ service['app-db'].url }} > .env.local"
    migrate: "npm run migrate"
  post-switch:
    env:
      command: "echo DATABASE_URL={{ service['app-db'].url }} > .env.local"

agent:
  command: claude
  workspace_prefix: "agent/"

commit:
  generation:
    command: "claude -p --model haiku"
```

### Config hierarchy (highest to lowest)

1. **Environment variables** — quick toggles and overrides
2. **`.devflow.local.yml`** — project-specific local overrides (gitignored)
3. **`.devflow.yml`** — team shared configuration

See the [full documentation](docs/index.html#configuration) for all options, environment variables, and provider-specific settings.

## Hooks

Lifecycle hooks are MiniJinja-templated commands that run at specific phases:

| Phase | When it fires |
|---|---|
| `post-create` | After creating a new workspace |
| `post-switch` | After switching to a workspace |
| `pre-commit` | Before committing |
| `pre-merge` | Before merging workspaces |
| `post-merge` | After merging |
| `pre-remove` | Before removing a workspace |

There are 15 phases in total — see [all hook phases](docs/CLI.md#hooks) for the complete list.

Template variables: `{{ workspace }}`, `{{ service['name'].url }}`, `{{ service['name'].host }}`, `{{ service['name'].port }}`, `{{ repo }}`, `{{ worktree_path }}`.

Filters: `sanitize`, `sanitize_db`, `hash_port`, `lower`, `upper`, `replace`, `truncate`.

## AI Agents

devflow provides first-class support for AI coding agents:

```bash
# Start an agent in an isolated workspace
devflow agent start fix-login -- 'Fix the login timeout bug'

# Check agent status
devflow agent status

# Generate skills/rules for AI tools
devflow agent skill                      # All tools (Claude, Cursor, OpenCode)
devflow agent skill --target claude      # Claude Code only
```

For CI/CD and automation, use `--json --non-interactive` for structured output:

```bash
devflow --json --non-interactive switch -c agent/task-42 --no-verify
CONN=$(devflow --json service connection agent/task-42 | jq -r '.connection_string')
```

See [AGENTS.md](AGENTS.md) for the full agent guide and automation contract.

## Workflows

### Feature development

```bash
git checkout -b feature/auth             # Git hooks auto-create services
# ... develop with isolated database ...
git checkout main                        # Services switch back automatically
devflow remove feature/auth              # Clean up when done
```

### PR review

```bash
git checkout feature/payment-refactor    # Services created automatically
devflow service logs feature/payment-refactor  # Check logs if needed
git checkout main                        # Switch back
devflow remove feature/payment-refactor --force
```

### AI agent

```bash
devflow agent start task-42 -- 'Fix the checkout flow'
devflow agent status                     # Monitor progress
```

## Releases (Commitizen)

`cz` is the primary release workflow for version bumps, changelog updates, and tags.

```bash
# create Conventional Commit messages
cz commit

# dry-run next release bump
cz bump --dry-run --yes --allow-no-commit --increment PATCH

# create release commit + changelog + tag
cz bump --yes
git push origin main --follow-tags
```

Equivalent `mise` tasks are available:

```bash
mise run release:dry-run
mise run release
```

`cz bump` updates all release version targets from `.cz.toml`, including Rust crates, Tauri config, UI package version, lockfile entries for `devflow*` packages, docs banner version, and `llms-full.txt`.

## Documentation deployment

Docs are deployed from `docs/` to GitHub Pages by `.github/workflows/docs-pages.yml` on every push to `main`.

For private repositories, enable Pages in repository settings and choose **GitHub Actions** as the source.

## Copy-on-Write Storage

| Filesystem | Platform | Method | Setup |
|---|---|---|---|
| APFS | macOS | `cp -c` clone | Automatic |
| ZFS | Linux | Snapshots + clones | `devflow setup-zfs` |
| Btrfs | Linux | Reflink copy | Automatic |
| XFS | Linux | Reflink copy | Automatic |
| ext4 / other | Any | Full copy (fallback) | None |

## Full Install

### macOS

No special setup — APFS cloning is automatic. Install [Docker Desktop](https://www.docker.com/products/docker-desktop/) (or [OrbStack](https://orbstack.dev)) and Rust, then `cargo install --path .`.

### Ubuntu

```bash
# Docker
sudo apt-get update && sudo apt-get install -y docker.io
sudo usermod -aG docker $USER && newgrp docker

# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# devflow
git clone https://github.com/clement-tourriere/devflow.git
cd devflow && cargo install --path .
```

### Optional: ZFS (Linux)

For near-instant cloning on ext4 (Ubuntu default):

```bash
sudo apt-get install -y zfsutils-linux
devflow setup-zfs                        # 10G pool named "devflow"
```

## Further Reading

- [Full Documentation](docs/index.html) — complete reference with search and dark mode
- [CLI Reference](docs/CLI.md) — all commands and flags
- [AI Agent Guide](AGENTS.md) — automation contract and agent workflows
- [Changelog](CHANGELOG.md) — version history
- [Examples](examples/) — ready-to-use configuration files

## License

MIT
