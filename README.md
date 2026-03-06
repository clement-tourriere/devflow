<p align="center">
  <img src="docs/icon.png" alt="devflow" width="128" />
</p>

<h1 align="center">devflow</h1>

<p align="center">Isolated dev environments for every workspace — automatically.</p>

> [Full Documentation](docs/index.html) | [CLI Reference](docs/CLI.md) | [AI Agent Guide](AGENTS.md) | [Changelog](CHANGELOG.md)

## What is devflow?

devflow gives each Git or Jujutsu workspace its own development environment: databases, caches, worktrees, hooks, and other stateful services. When you switch workspaces, devflow can automatically create or switch the matching service instances, keep worktrees aligned, and surface the right connection info for your app and tools.

The result: no more shared local databases between branches, less stashing, faster reviews, safer migrations, and cleaner parallel work.

## Why teams use it

- Isolated per-workspace services for PostgreSQL, MySQL, ClickHouse, Redis, and custom Docker images
- Fast Copy-on-Write cloning for databases and worktrees on APFS, ZFS, Btrfs, and XFS
- Automatic workspace sync through CLI commands or installed VCS hooks
- Multiple interfaces: CLI, TUI dashboard, and desktop GUI
- Hook system with MiniJinja templates, approvals, built-in actions, and reusable recipes
- Native HTTPS reverse proxy for `*.localhost` Docker services
- AI-friendly automation with `--json`, `--non-interactive`, `AGENTS.md`, `llms.txt`, and agent skills
- Advanced workflow support including merge checks, rebase flows, merge train, and sandboxed workspaces

## Choose your interface

- `CLI` for scripts, daily workspace switching, CI, and power users
- `devflow tui` for keyboard-driven workspace, service, proxy, and log management in the terminal
- Desktop app for project setup, services, hooks, config editing, embedded terminal, proxy controls, settings, and merge train

## Quick start

```bash
# 1. Initialize a repository
cd ~/my-project
devflow init

# 2. Create a workspace with isolated services
devflow switch -c feature/auth

# 3. Inspect the environment
devflow status
devflow connection feature/auth --format env
```

If worktrees are enabled, devflow can also move you into the matching worktree directory with shell integration:

```bash
eval "$(devflow shell-init)"
```

## Core concepts

| Concept | Description |
|---|---|
| **Workspace** | A devflow-managed isolated environment associated with a Git branch or JJ bookmark/change. It is the unit that gets services, hooks, and optional worktree directories. |
| **Service** | A stateful backend managed per workspace: database, cache, queue, or generic Docker container. |
| **Worktree** | An optional per-workspace checkout directory so you can work on multiple tasks without stashing. |
| **Provider** | The system that creates service instances: local Docker, Neon, DBLab, Xata, or a plugin provider. |
| **Hook** | A command or built-in action that runs during lifecycle events like create, switch, merge, rebase, or cleanup. |

## Main capabilities

### Workspace isolation

- `devflow switch -c feature/x` creates a new workspace and its services
- `devflow switch feature/x` returns to an existing workspace and updates the environment
- `devflow switch feature/x --open` opens a tmux/zellij session in the workspace worktree
- `devflow remove feature/x` cleans up the workspace, worktree, and service instances together

### Services

Supported service types include PostgreSQL, MySQL, ClickHouse, generic Docker containers, and plugin-backed services. Local mode supports fast cloning and common lifecycle operations such as start, stop, reset, logs, seed, and cleanup.

```bash
devflow service add app-db --provider local --service-type postgres
devflow service add analytics --provider local --service-type clickhouse
devflow service discover
```

### Hooks

Hooks are MiniJinja-templated lifecycle actions that can update `.env` files, run migrations, call APIs, copy files, notify the desktop, or execute shell commands.

```bash
devflow hook show
devflow hook explain post-switch
devflow hook actions
devflow hook recipes
```

Built-in phases cover switching, create/remove, commit, merge, rebase, merge cascade, and service lifecycle events. Custom phases are also supported.

### Smart merge

devflow includes advanced merge tooling for teams that want stronger branch hygiene:

- merge readiness checks before landing work
- `devflow merge` and `devflow rebase` helpers
- merge train queue with pause, resume, and run support
- optional cleanup after successful merge

Smart merge is feature-gated in settings/config before `devflow train` commands are available.

### Sandboxed workspaces

For risky automation or agent tasks, you can create a restricted workspace:

```bash
devflow switch -c agent/fix-login --sandboxed
```

This uses the sandbox support in `devflow-core` to reduce filesystem and command access where supported by the platform.

### Reverse proxy

devflow ships with a native reverse proxy that auto-discovers Docker containers and maps them to HTTPS `*.localhost` domains.

```bash
devflow proxy start
devflow proxy trust install
devflow proxy list
```

| Container Type | Domain Pattern |
|---|---|
| Standalone | `container_name.localhost` |
| Compose service | `service.project.localhost` |
| devflow service | `service.workspace.project.localhost` |
| Custom label | value of `devproxy.domain` |

### AI agents and automation

devflow is designed to work well with coding agents and CI:

- `--json` for structured output
- `--non-interactive` for automation-safe execution
- `devflow agent context` for machine-readable project and service context
- `devflow agent skill` to install Agent Skills-compatible workspace skills
- `AGENTS.md`, `llms.txt`, and example agent bootstrap scripts in `examples/`

```bash
devflow --json --non-interactive switch -c agent/task-42
devflow agent context --format json
devflow agent skill
```

See `AGENTS.md` for the recommended agent workflow.

## Desktop app

The desktop GUI is a substantial part of the product, not just a wrapper around the CLI. It includes:

- Project list and setup flow
- Workspace and service management
- Hook manager and config editor
- Proxy dashboard
- Embedded terminal panel
- Settings page with feature toggles such as Smart Merge
- Merge Train page when Smart Merge is enabled

```bash
mise run gui
mise run gui:build
```

Requires [bun](https://bun.sh) and the [Tauri CLI](https://v2.tauri.app/start/prerequisites/).

## TUI dashboard

```bash
devflow tui
```

The TUI has five tabs:

- `Workspaces` — workspace tree, status, and open/create/switch actions
- `Services` — service inventory and capabilities
- `Proxy` — proxy status and proxied containers
- `System` — config, hooks, and diagnostics
- `Logs` — service log viewer

## Configuration

`devflow init` creates `.devflow.yml`. All sections are optional.

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
      action:
        type: write-env
        path: .env.local
        vars:
          DATABASE_URL: "{{ service['app-db'].url }}"
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

Config precedence:

1. Environment variables
2. `.devflow.local.yml`
3. `.devflow.yml`

## Example workflows

### Feature development

```bash
devflow switch -c feature/auth
# make schema changes, seed data, run app
devflow status
```

### Review an existing branch with isolated state

```bash
devflow switch feature/payment-refactor
devflow service logs feature/payment-refactor
```

### Merge train

```bash
devflow train add
devflow train status
devflow train run --cleanup
```

### Agent or automation task

```bash
devflow --json --non-interactive switch -c agent/task-42
devflow agent context --format json
```

## Install

```bash
git clone https://github.com/clement-tourriere/devflow.git
cd devflow
cargo install --path .
```

Requirements:

- Rust 1.70+
- Docker for local services
- `bun` + Tauri prerequisites if you want the desktop GUI

### macOS

APFS cloning works automatically. Install Docker Desktop or OrbStack, install Rust, then run `cargo install --path .`.

### Ubuntu

```bash
sudo apt-get update && sudo apt-get install -y docker.io
sudo usermod -aG docker $USER && newgrp docker

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

git clone https://github.com/clement-tourriere/devflow.git
cd devflow
cargo install --path .
```

### Optional: ZFS on Linux

```bash
sudo apt-get install -y zfsutils-linux
devflow setup-zfs
```

## Storage backends for fast cloning

| Filesystem | Platform | Method | Setup |
|---|---|---|---|
| APFS | macOS | `cp -c` clone | Automatic |
| ZFS | Linux | Snapshots + clones | `devflow setup-zfs` |
| Btrfs | Linux | Reflink copy | Automatic |
| XFS | Linux | Reflink copy | Automatic |
| ext4 / other | Any | Full copy fallback | None |

## Examples and further reading

- `examples/simple.devflow.yml` — minimal single-service setup
- `examples/multi-service.devflow.yml` — multi-service project with hooks and worktrees
- `examples/django.devflow.yml` — framework-oriented example
- `docs/CLI.md` — command reference
- `docs/index.html` — full documentation site
- `AGENTS.md` — agent workflow guide

## Documentation deployment

Docs are deployed from `docs/` to GitHub Pages via `.github/workflows/docs-pages.yml` on pushes to `main`.

## License

MIT
