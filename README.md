<p align="center">
  <img src="banner.jpg" alt="devflow" width="600" />
</p>

<h1 align="center">devflow</h1>

<p align="center">Create, switch, inspect, and clean up isolated development workspaces with matching local services.</p>

<p align="center">
  <a href="https://clement-tourriere.github.io/devflow/">Documentation</a> ·
  <a href="docs/CLI.md">CLI reference</a> ·
  <a href="AGENTS.md">Agent guide</a> ·
  <a href="CHANGELOG.md">Changelog</a>
</p>

## What is devflow?

devflow is a workspace orchestrator for local development. Every workspace is a materialized directory — a Git worktree or Jujutsu workspace — with its own service containers, connection strings, generated env files, processes, and stable HTTPS URLs. The primary checkout is the default workspace; every additional Git workspace is a linked worktree.

```bash
devflow switch -c feature/auth
# → VCS ref created, worktree at ../myapp.feature_auth_fc659bd73585
# → postgres workspace cloned from main (Copy-on-Write, near-instant)
# → hooks wrote .env.local with the new DATABASE_URL
# → your shell is now inside the worktree
```

Work on multiple features, reviews, migrations, or AI-agent tasks in parallel — without sharing a database or stashing state. Drive it from the CLI, a terminal dashboard (`devflow tui`), or a desktop GUI.

Default CLI builds include local services, the native process supervisor, and the TUI. Optional build features are `proxy`, `service-neon`, `service-xata`, and `service-dblab`; use `cargo build --all-features` for all integrations. Use Git and your preferred editor or agent for commits.

For separate code/data origins, explicit adoption of existing resources, and interrupted-operation recovery, see [Data ownership and recovery](docs-site/src/content/docs/guides/ownership-and-recovery.md).

## How it works

1. `devflow switch -c feature-x` creates a Git worktree or jj workspace from the selected parent. `devflow switch feature-x` reuses one that already exists.
2. devflow creates or switches the matching service workspaces for every configured service.
3. Lifecycle hooks fire inside the target directory — write `.env.local`, run migrations, open a tmux window.
4. The shell wrapper `cd`s you into that directory.

Every workspace's app is reachable at a predictable HTTPS URL, every workspace's database at its own connection string — and `devflow remove` cleans all of it up.

## Highlights

- **Git and jj workspaces** — embedded Git and Jujutsu libraries manage linked worktrees and native workspaces without CLI helpers, and manually created worktrees can be adopted without an in-place checkout mode
- **Two isolation models, chosen per service** — physical (a Copy-on-Write Docker container per workspace) or logical (one shared global engine with a database, bucket, or DB index per workspace)
- **Multi-service** — PostgreSQL, ClickHouse, MySQL, Redis, RustFS object storage, any Docker image, or custom plugin providers; cloud branching via Neon/DBLab/Xata (experimental)
- **Project processes** — workspace-scoped web servers, workers, and schedulers with native start/stop/status/logs, ready checks, dependency ordering, port bumping, and service env interpolation
- **Managed worktrees** — per-workspace directories from a path template, with configured payloads and caches whose inputs are checked before reuse; broad ignored-file and AI-directory copying is disabled by default
- **Lifecycle hooks** — MiniJinja-templated commands and built-in actions at every phase, with conditions, an approval system, and recipes that detect your project's stack
- **Optional HTTPS proxy** — discover Docker containers and host processes through `https://name.localhost` URLs; inspect CA setup instructions and configure trust with your system tools
- **Controller daemon** — `devflow daemon start` keeps every registered project's shared engines and process desired state reconciled
- **Seeding** — initialize databases from a PostgreSQL URL, a local dump file, or S3
- **Built for AI agents** — `--json` and `--non-interactive` everywhere, literal-argument execution, saved operation recovery, and isolated workspace commands

Cloud branching providers and plugin providers are newer and still maturing — check `devflow --help-all` and the changelog for what is available in your build.

### v0.8.0 validation status

| Area | Status |
| --- | --- |
| macOS core workflows and local Docker fixtures | Tested: Rust, Docker, CLI, UI build and documentation checks passed |
| Linux real-project use, including ZFS, XFS and Btrfs | **Untested**; automated Linux CI checks run separately |
| Neon, DBLab and Xata live accounts | **Untested**; providers remain experimental and require optional build features |
| Interactive desktop session | **Untested**; compilation, frontend build and result-handling tests passed |

See the [v0.8.0 release notes](docs/releases/v0.8.0.md) for changes, migration notes and validation details.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/clement-tourriere/devflow/main/scripts/install.sh | sh
```

Installs the latest release binary to `~/.local/bin` (override with `DEVFLOW_INSTALL_DIR`; pin a release with `DEVFLOW_VERSION=v0.5.0`). Supported platforms: Linux (x86_64, arm64) and macOS (Apple Silicon) — see [Install from source](#install-from-source) for everything else.

Update at any time with:

```bash
curl -fsSL https://raw.githubusercontent.com/clement-tourriere/devflow/main/scripts/install.sh | sh
```

## Quick start

```bash
# 1. Initialize a repository (prints guided next steps)
cd ~/my-project
devflow init

# 2. Validate setup and generate agent guidance
devflow doctor
devflow agent skill --target all

# 3. Create a workspace with isolated services
devflow switch -c feature/auth

# 4. Inspect the environment
devflow status

# 4. Print connection details for scripts or .env files
devflow connection feature/auth --format env

# 5. When the work is done, clean everything up in one step
devflow remove feature/auth
```

Install shell integration so `devflow switch` can move your shell into the selected workspace directory:

```bash
eval "$(devflow shell-init)"
```

## Everyday commands

```bash
devflow switch                  # Pick a workspace interactively
devflow switch -c feature/api    # Create and switch to a workspace
devflow --json plan feature/api  # Preview code, data, paths, and setup requirements
devflow list                    # Parent tree with paths, services, and process health
devflow status                  # Show current workspace information
devflow connection feature/api   # Show service connection info
devflow remove feature/api       # Remove workspace resources
devflow doctor                  # Diagnose Docker, VCS, and config issues
devflow tui                     # Open the terminal dashboard
```

Run `devflow --help-all` to see advanced service, hook, proxy, agent, and config commands.

`devflow --json list` has one stable document shape regardless of service count. It includes `schema_version`, project/VCS context, `context_workspace`, `default_workspace`, `roots`, workspace nodes, and `warnings`. Workspace names remain the raw VCS names users typed; a separate backend `service_key` is used for databases, containers, and generated paths. `identity_status` and `canonical_service_key` distinguish new canonical keys from safely retained or unresolved legacy identities. Parent relationships record immutable creation provenance, so children stay attached to their recorded parent even if that parent is later missing.

`devflow --json plan agent/task --from main --data-from baseline:clean@2` previews an exact baseline selection before creating resources. Incomplete operations can be inspected and resumed with `devflow operation`; cancellation is limited to failures proven to have made no resource changes. The desktop exposes baseline selection and workspace recovery through the same core API. See [ownership and recovery](docs-site/src/content/docs/guides/ownership-and-recovery.md).

## Services

devflow can manage named services per workspace. Two models are available, per service:

- `type: local` — one Copy-on-Write Docker container per workspace (Postgres, ClickHouse, MySQL, or any image). Strong isolation, instant clones via APFS, ZFS, Btrfs, or XFS.
- `type: shared` — one global container per engine; each workspace gets a logical boundary created on the fly: a Postgres database (`CREATE DATABASE … TEMPLATE parent` keeps branch-from-parent semantics), a Redis DB index, a RustFS (S3-compatible) bucket, or a ClickHouse database.

```bash
devflow service add app-db --provider local --service-type postgres
devflow service create feature/auth
devflow service connection feature/auth
devflow service seed main --from dump.sql   # or a postgres:// URL, or s3://
devflow service logs feature/auth
devflow service reset feature/auth

devflow service up                 # ensure all shared global engines are running
devflow daemon start               # keep engines/process watch+retry running in the background
```

Connection information can be emitted as URI, env, or JSON output for scripts and tooling.

## Processes

For complex projects, devflow can also manage workspace-scoped project processes — app servers, frontend dev servers, background workers, and schedulers — without Docker:

```yaml
processes:
  auto_start: true
  daemons:
    api:
      run: "npm run dev"
      port: { expect: [3000], bump: 50 }
      ready_http: "http://127.0.0.1:3000/health"
      env:
        DATABASE_URL: "{{ service['app-db'].url }}"
    worker:
      run: "npm run worker"
      required: false
      depends: [api]
```

```bash
devflow process start --all
devflow process status
devflow process logs api --tail 100 --follow
devflow process stop --all
```

When `processes.auto_start: true`, `devflow switch` starts configured processes after services and hooks are aligned. Auto-started commands use the same approval store as hooks, so agents can pre-approve with `devflow hook approvals add "npm run dev"` or set `DEVFLOW_APPROVE_HOOKS=1`. The desktop and CLI share native runtime records. Running processes with ports are also exposed through the devflow proxy as `https://<process>.<workspace>.<project>.<suffix>` (default suffix: `.localhost`). `devflow remove` preflights the worktree, runs removal hooks while the directory still exists, stops processes, deletes services, and only then removes the worktree and VCS ref. Run `devflow daemon start` to keep desired-state, `watch` restart-on-change, and `retry` reconciliation active in the background. See [Project processes](https://clement-tourriere.github.io/devflow/guides/processes/) and [Adding devflow to an existing project](https://clement-tourriere.github.io/devflow/getting-started/existing-project/) for Compose-to-devflow migration examples.

## Hooks and automation

Hooks run during workspace lifecycle phases such as creation and switching. They can write env files, run migrations, or execute project-specific commands — templated with MiniJinja, gated by an approval system, and installable via recipes that detect your project's stack (`devflow hook setup`).

```bash
devflow hook show
devflow hook explain post-switch
devflow hook vars
devflow hook run post-create       # run a phase manually
```

## Built for AI agents

Every command supports `--json` and `--non-interactive`, and the worktree-per-task pattern gives each agent an isolated directory, database, and env file:

```bash
devflow --json --non-interactive switch -c agent/task-42   # isolated env for the task
devflow agent context --format json                        # project + connection context for the agent
devflow switch -c agent/fix-login -x claude -- 'Fix the login timeout bug'
```

AI tool configs (`.claude/`, `.cursor/`, `.opencode/`, `.agents/`) are copied into new worktrees automatically. In `--non-interactive` mode, unapproved hooks are skipped with a warning (set `DEVFLOW_APPROVE_HOOKS=1` to auto-approve in CI/agent runs). See `AGENTS.md` for the recommended coding-agent workflow.

`devflow --json agent status` reports workspace counts and `agent_workdir` values. Generate project guides with `devflow agent skill --target all`, or a root guide with `devflow agent docs`.

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
  path_template: "../{repo}.{workspace}"

hooks:
  post-switch:
    env:
      action:
        type: write-env
        path: .env.local
        vars:
          DATABASE_URL: "{{ service['app-db'].url }}"

processes:
  auto_start: true
  daemons:
    api:
      run: "npm run dev"
      port: { expect: [3000], bump: 50 }
      ready_http: "http://127.0.0.1:3000/health"
      env:
        DATABASE_URL: "{{ service['app-db'].url }}"
```

The `worktree` block controls paths and copy behavior; there is no enable/disable mode. Older `worktree.enabled` keys are ignored with a deprecation warning; they can no longer switch devflow back to an in-place checkout.

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

Run `devflow --json doctor --strict` for a health check that exits non-zero when checks fail.

## More examples

- `examples/simple.devflow.yml`
- `examples/multi-service.devflow.yml`
- `examples/django.devflow.yml`
- `examples/processes.devflow.yml`
- `examples/migrate-existing-app.devflow.yml`
- `docs/CLI.md`
- `AGENTS.md`
- `docs/ARCHITECTURE.md`
- `CONTRIBUTING.md`
- `SECURITY.md`

## License

MIT

## Repeatable data and recovery

```sh
devflow baseline prepare clean --service db --from ./fixtures/base.sql
devflow --json --non-interactive switch -c agent/task --from main --data-from baseline:clean@v1
devflow exec --workspace agent/task -- pytest -k 'login or signup'
devflow service reset agent/task --service db
devflow operation inspect OPERATION_ID
devflow operation resume OPERATION_ID
devflow --json retention plan --service db --max-count 10
```

Baselines are immutable local PostgreSQL generations. Workspace inventory exposes exact per-service origins, readiness, identities, and retention state. See [architecture and migrations](docs/architecture.md), [baselines](docs-site/src/content/docs/guides/baselines.md), and [retention and caches](docs-site/src/content/docs/guides/retention-and-caches.md).

The desktop uses the optional proxy companion. Install it independently with `cargo install --path . --no-default-features --features proxy --bin devflow-proxy`. Workspace and database commands work without it.
