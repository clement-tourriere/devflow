<p align="center">
  <img src="banner.jpg" alt="devflow" width="600" />
</p>

<h1 align="center">devflow</h1>

<p align="center">Create, switch, and clean up isolated development workspaces with matching local services.</p>

<p align="center">
  <a href="https://clement-tourriere.github.io/devflow/">Documentation</a> ·
  <a href="docs/CLI.md">CLI reference</a> ·
  <a href="AGENTS.md">Agent guide</a> ·
  <a href="CHANGELOG.md">Changelog</a>
</p>

## What is devflow?

devflow is a workspace orchestrator for local development. It connects your Git branch or worktree workflow with per-workspace state: service containers, connection strings, lifecycle hooks, generated env files, optional worktree directories, and stable HTTPS URLs.

The goal is simple: work on multiple features, reviews, migrations, or agent tasks without sharing the same local database or constantly stashing and resetting state.

Services can be isolated two ways, chosen per service: **physically** (a Copy-on-Write Docker container per workspace) or **logically** (one shared global engine — Postgres, Redis, RustFS object storage, ClickHouse — with a database, bucket, or DB index provisioned per workspace on the fly).

## Current focus

- Branch/workspace switching with `devflow switch`
- Optional Git worktree management
- Per-workspace local services: Copy-on-Write Docker containers, or shared global engines with logical isolation (`type: shared`)
- An HTTPS reverse proxy: every container gets a trusted `https://name.local` URL that resolves the same from the host and from inside containers
- A controller daemon (`devflow daemon`) that keeps shared engines running
- Lifecycle hooks for setup, migrations, env files, and cleanup
- JSON and non-interactive modes for scripts and coding agents
- A terminal dashboard via `devflow tui`

Some advanced areas, such as merge train, sandboxing, plugin providers, the cloud branching providers (Neon/DBLab/Xata — currently experimental), and the desktop GUI, are still evolving. Check `devflow --help-all` and the changelog for what is available in your build.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/clement-tourriere/devflow/main/scripts/install.sh | sh
```

Installs the latest release binary to `~/.local/bin` (override with `DEVFLOW_INSTALL_DIR`; pin a release with `DEVFLOW_VERSION=v0.5.0`). Supported platforms: Linux (x86_64, arm64) and macOS (Apple Silicon) — see [Install from source](#install-from-source) for everything else.

Update at any time with:

```bash
devflow update          # or: devflow update --check
```

## Quick start

```bash
# 1. Initialize a project
cd ~/my-project
devflow init

# 2. Create and switch to an isolated workspace
devflow switch -c feature/auth

# 3. Inspect the current environment
devflow status

# 4. Print connection details for scripts or .env files
devflow connection feature/auth --format env
```

If worktrees are enabled, install shell integration so `devflow switch` can move your shell into the selected worktree:

```bash
eval "$(devflow shell-init)"
```

## Everyday commands

```bash
devflow switch                  # Pick a workspace interactively
devflow switch -c feature/api    # Create and switch to a workspace
devflow list                    # List workspaces and service status
devflow status                  # Show current workspace information
devflow connection feature/api   # Show service connection info
devflow remove feature/api       # Remove workspace resources
devflow tui                     # Open the terminal dashboard
```

Run `devflow --help-all` to see advanced service, hook, proxy, agent, and config commands.

## Services

devflow can manage named services per workspace. Two models are available, per service:

- `type: local` — one Copy-on-Write Docker container per workspace (Postgres, ClickHouse, MySQL, or any image). Strong isolation, instant clones.
- `type: shared` — one global container per engine; each workspace gets a logical boundary created on the fly: a Postgres database (`CREATE DATABASE … TEMPLATE parent` keeps branch-from-parent semantics), a Redis DB index, a RustFS (S3-compatible) bucket, or a ClickHouse database.

```bash
devflow service add app-db --provider local --service-type postgres
devflow service create feature/auth
devflow service connection feature/auth
devflow service logs feature/auth
devflow service reset feature/auth

devflow service up                 # ensure all shared global engines are running
devflow daemon start               # keep them running in the background (restarts dead engines)
```

Connection information can be emitted as URI, env, or JSON output for scripts and tooling.

## Hooks and automation

Hooks run during workspace lifecycle phases such as creation and switching. They can write env files, run migrations, or execute project-specific commands.

```bash
devflow hook show
devflow hook explain post-switch
devflow hook vars
```

For automation, use:

```bash
devflow --json --non-interactive switch -c agent/task-42
devflow agent context --format json
```

See `AGENTS.md` for the recommended coding-agent workflow. In `--non-interactive` mode, unapproved hooks are skipped with a warning (set `DEVFLOW_APPROVE_HOOKS=1` to auto-approve in CI/agent runs).

## Configuration

`devflow init` creates a `.devflow.yml` (a lightweight `devflow.toml` is also read). A minimal example:

```yaml
services:
  - name: app-db
    type: local
    service_type: postgres
    default: true
    local:
      image: postgres:17

worktree:
  enabled: true
  path_template: "../{repo}.{workspace}"

hooks:
  post-switch:
    env:
      action:
        type: write-env
        path: .env.local
        vars:
          DATABASE_URL: "{{ service['app-db'].url }}"
```

A shared-engine service is one stanza — no per-workspace containers, ports, or volumes:

```yaml
services:
  - name: cache
    service_type: redis        # one global redis; a DB index per workspace
  - name: storage
    service_type: rustfs       # one global RustFS; a bucket per workspace
```

Config precedence:

1. Environment variables
2. `.devflow.local.yml`
3. `.devflow.yml` (or `devflow.toml`)

## Install from source

```bash
git clone https://github.com/clement-tourriere/devflow.git
cd devflow
cargo install --path .
```

Requirements:

- Rust toolchain
- Docker or a compatible container runtime for local services
- Optional: `bun` and Tauri prerequisites for desktop GUI development

## More examples

- `examples/simple.devflow.yml`
- `examples/multi-service.devflow.yml`
- `examples/django.devflow.yml`
- `docs/CLI.md`
- `AGENTS.md`

## License

MIT
