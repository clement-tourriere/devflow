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

devflow is a workspace orchestrator for local development. It connects your Git branch or worktree workflow with per-workspace state: service containers, connection strings, lifecycle hooks, generated env files, and optional worktree directories.

The goal is simple: work on multiple features, reviews, migrations, or agent tasks without sharing the same local database or constantly stashing and resetting state.

## Current focus

- Branch/workspace switching with `devflow switch`
- Optional Git worktree management
- Per-workspace local services, especially Docker-backed databases and caches
- Lifecycle hooks for setup, migrations, env files, and cleanup
- JSON and non-interactive modes for scripts and coding agents
- A terminal dashboard via `devflow tui`

Some advanced areas, such as merge train, sandboxing, proxy, plugin providers, and the desktop GUI, are still evolving. Check `devflow --help-all` and the changelog for what is available in your build.

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

devflow can manage named services per workspace. Local Docker services are the main path today, with support for common database/cache use cases and generic containers.

```bash
devflow service add app-db --provider local --service-type postgres
devflow service create feature/auth
devflow service connection feature/auth
devflow service logs feature/auth
devflow service reset feature/auth
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

See `AGENTS.md` for the recommended coding-agent workflow, including hook pre-approval in non-interactive mode.

## Configuration

`devflow init` creates a `.devflow.yml`. A minimal example:

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

Config precedence:

1. Environment variables
2. `.devflow.local.yml`
3. `.devflow.yml`

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
