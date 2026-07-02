# CLI Reference

Complete reference for the current `devflow` CLI surface. Core automation flows should prefer `--json` and `--non-interactive` where possible.

Tip: run `devflow --help-all` to print the full command surface directly from the binary.

## Global Flags

```bash
devflow [--json] [--non-interactive] [-s <service-name>] <command>
```

| Flag | Description |
|---|---|
| `--json` | Print structured JSON to stdout when supported |
| `--non-interactive` | Skip prompts and use defaults or fail when approval is required |
| `-s <name>` | Target a specific configured service |

## Daily Workspace Flow

### `devflow switch [workspace]`

Create or switch a workspace, align services, optionally move into a worktree, and run lifecycle hooks.

```bash
devflow switch
devflow switch feature/auth
devflow switch -c feature/new
devflow switch -c feature/new --from develop
devflow switch feature/auth -x "npm run dev"
devflow switch feature/auth --open
devflow switch feature/auth --no-services
devflow switch feature/auth --no-processes
devflow switch feature/auth --dry-run
```

Important flags:

- `-c, --create` create before switching
- `--from <workspace>` choose the parent workspace
- `-x, --execute <command>` run a command after switching
- `-d, --detach` run the post-switch command in a detached multiplexer session
- `-o, --open` open an interactive multiplexer session in the workspace worktree
- `--no-services` skip service branching/switching
- `--no-processes` skip process auto-start during switch
- `--no-verify` skip hooks
- `--template` switch to the main/template workspace
- `--no-respect-gitignore` include gitignored files in worktree copy

### Multiplexer Integration

`--open` and `--detach` use tmux or zellij to launch sessions in the workspace worktree. The multiplexer is auto-detected (tmux first, then zellij), or you can set a preference in `.devflow.yml`:

```yaml
execute:
  multiplexer: zellij  # or "tmux"
```

For a fully custom template, use `detach_command` with `{session}`, `{dir}`, and `{cmd}` placeholders:

```yaml
execute:
  detach_command: "screen -dmS {session} bash -c {cmd}"
```

Examples:

```bash
# Open interactive session in workspace worktree
devflow switch feature-auth --open

# Run a command in a detached session
devflow switch feature-auth -x "npm run dev" --detach

# Auto-open sessions on new workspace creation (install hook recipe)
devflow hook install multiplexer-session
```

### `devflow status`

Show current project, workspace, and service state.

```bash
devflow status
devflow --json status
```

### `devflow connection <workspace>`

Alias for `devflow service connection <workspace>`.

```bash
devflow connection feature/auth
devflow connection feature/auth --format env
devflow connection feature/auth --format json
```

### `devflow list`

List known workspaces and their service/worktree state.

```bash
devflow list
devflow --json list
```

### `devflow graph`

Render the full environment graph: workspace tree, services, worktree paths, and provider info.

```bash
devflow graph
devflow --json graph
```

### `devflow link <workspace>`

Register an existing VCS workspace with devflow and optionally materialize matching service instances.

```bash
devflow link feature/auth
devflow link feature/auth --from main
```

### `devflow remove <workspace>`

Delete the workspace, worktree, and associated service instances.

```bash
devflow remove feature/auth
devflow remove feature/auth --force
devflow remove feature/auth --keep-services
```

### `devflow cleanup`

Alias for `devflow service cleanup`.

```bash
devflow cleanup
devflow cleanup --max-count 5
```

## Services

### `devflow service add [name]`

Add and configure a service provider. With no flags, opens an interactive wizard.

```bash
devflow service add
devflow service add app-db --provider local --service-type postgres
devflow service add analytics --provider local --service-type clickhouse
devflow service add app-db --provider local --service-type postgres --from ./backup.sql
devflow service add app-db --provider local --service-type postgres --from postgresql://user:pass@host/db
devflow service add app-db --provider local --service-type postgres --from s3://bucket/path/dump.sql
```

### `devflow service remove <name>`

Remove a service configuration from the project.

### `devflow service list`

List configured services.

### `devflow service status`

Show service status across providers.

### `devflow service up`

Ensure every configured **shared global engine** (`type: shared`, or `service_type: rustfs`/`redis`) is running — a one-shot reconcile / pre-warm. Per-workspace (CoW) and cloud services are skipped.

```bash
devflow service up
devflow --json service up
```

### `devflow service capabilities`

Show the capability matrix for configured services.

```bash
devflow service capabilities
devflow --json service capabilities
```

### `devflow service create <workspace>`

Create service instance(s) for a workspace without switching your VCS context.

```bash
devflow service create feature/auth
devflow service create feature/auth --from develop
```

### `devflow service delete <workspace>`

Delete service instances for a workspace while keeping the workspace and worktree.

```bash
devflow service delete feature/auth
```

### `devflow service cleanup`

Clean up old service workspaces.

```bash
devflow service cleanup
devflow service cleanup --max-count 5
```

### `devflow service start <workspace>`

Start a stopped local-provider workspace container.

### `devflow service stop <workspace>`

Stop a running local-provider workspace container.

### `devflow service reset <workspace>`

Reset a local-provider workspace to its parent state.

### `devflow service destroy`

Destroy all data for a service. Requires `--force` in `--json` or `--non-interactive` mode.

```bash
devflow service destroy
devflow service destroy --force
```

### `devflow service connection <workspace>`

Show connection information for workspace services.

```bash
devflow service connection feature/auth
devflow service connection feature/auth --format env
devflow service connection feature/auth --format json
```

### `devflow service logs <workspace>`

Show logs for a local workspace container.

```bash
devflow service logs feature/auth
devflow service logs feature/auth --tail 50
```

### `devflow service seed <workspace> --from <source>`

Seed a workspace from a file, database URL, or S3 object.

```bash
devflow service seed main --from dump.sql
devflow service seed main --from postgresql://user:pass@host/db
devflow service seed main --from s3://bucket/path/dump.sql
```

### `devflow service discover`

Auto-discover running Docker containers and suggest adding them as services.

```bash
devflow service discover
devflow service discover --service-type postgres
devflow service discover --global
```

## Processes

Workspace processes are project commands such as app servers, frontend dev servers, background workers, and schedulers. They are configured under `processes.daemons` and run in the selected workspace/worktree with service connection variables rendered from the same MiniJinja context as hooks.

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

### `devflow process start [names...] [--all] [--workspace <ws>] [--force]`

Start selected processes, or all configured processes when no names are given (or `--all` is used). Dependencies start first. `--force` restarts an already-running process.

```bash
devflow process start --all
devflow process start api --force
```

### `devflow process stop [names...] [--all] [--workspace <ws>]`

Stop selected processes, or all configured/running workspace processes.

### `devflow process restart [names...] [--all] [--workspace <ws>]`

Stop then start selected processes.

### `devflow process list|status [--workspace <ws>]`

Show recorded process state, PID, resolved ports, proxy URLs, retry count, and log paths.

### `devflow process logs <name> [--workspace <ws>] [--tail N] [--follow]`

Print logs captured from stdout/stderr. Use `--follow`/`-f` to stream appended output until interrupted.

Set `required: false` on optional processes so readiness failures are reported but do not fail `switch`/`process start`.

When `processes.auto_start: true`, `devflow switch` starts configured processes after services and hooks are aligned. Auto-started shell commands use the same approval store as hooks: unapproved commands are skipped in `--json`/`--non-interactive` mode unless pre-approved with `devflow hook approvals add "npm run dev"` or `DEVFLOW_APPROVE_HOOKS=1`. Use `processes.provider: pitchfork` to embed Pitchfork's Rust supervisor directly without shelling out to the `pitchfork` CLI. Running processes with resolved ports are exposed by `devflow proxy` as `https://<process>.<workspace>.<project>.<suffix>` (default suffix: `.local`). `devflow remove` stops processes before deleting the worktree and service workspaces. Run `devflow daemon start` to keep desired-state, `watch` restart-on-change, and `retry` reconciliation active in the background. See `docs-site/src/content/docs/guides/processes.md` and `examples/migrate-existing-app.devflow.yml` for migration examples.

## Controller Daemon

A background controller that keeps every registered project's **shared global engines** running and reconciles process desired state plus `watch`/`retry` behavior. Service/process provisioning happens on `switch`; the daemon keeps engines alive, starts/stops processes whose recorded desired state drifts from reality, and restarts watched/crashed processes that devflow manages.

### `devflow daemon start`

```bash
devflow daemon start                 # background, reconcile every 30s
devflow daemon start --interval 10   # custom reconcile interval (seconds)
devflow daemon start --once          # reconcile once and exit (no background process)
devflow daemon start --foreground    # run attached (Ctrl+C to stop)
```

### `devflow daemon status`

Show whether the daemon is running, plus the last reconcile and per-engine health.

### `devflow daemon stop`

Stop the running daemon.

## Hooks

Hooks are MiniJinja-templated lifecycle entries defined in `.devflow.yml`. They can be shell commands or built-in actions.

### Built-in hook phases

Current built-in phases include:

- `pre-switch`, `post-create`, `post-start`, `post-switch`
- `pre-remove`, `post-remove`
- `pre-commit`
- `pre-service-create`, `post-service-create`
- `pre-service-delete`, `post-service-delete`, `post-service-switch`

Custom phases are also supported.

### `devflow hook show [phase]`

Show configured hooks, optionally filtered by phase.

```bash
devflow hook show
devflow hook show post-create
```

### `devflow hook run <phase> [name]`

Run hooks manually.

```bash
devflow hook run post-create
devflow hook run post-create migrate
devflow hook run post-create --workspace feature/auth
```

### `devflow hook explain [phase]`

Explain hook phases and template variables.

```bash
devflow hook explain
devflow hook explain post-switch
```

### `devflow hook vars`

Show the current hook template context.

```bash
devflow hook vars
devflow hook vars --workspace feature/auth
devflow --json hook vars
```

### `devflow hook render <template>`

Render a MiniJinja template against the current context.

```bash
devflow hook render "DATABASE_URL={{ service['app-db'].url }}"
```

### `devflow hook approvals`

Manage the approval store for hook commands.

```bash
devflow hook approvals list
devflow hook approvals add "npm run migrate"
devflow hook approvals clear
```

### `devflow hook triggers`

Show the VCS event to hook phase mapping.

### `devflow hook actions`

List built-in action types.

Current built-in action types include:

- `shell`
- `replace`
- `write-file`
- `write-env`
- `copy`
- `docker-exec`
- `http`
- `notify`

### `devflow hook recipes`

List built-in hook recipes. Inside a project, each recipe is probed against
the codebase (files present, tools actually installed) and shown with its
evidence, suggested parameter values, and install state.

```bash
devflow hook recipes
devflow --json hook recipes
```

Available: `env-file`, `patch-config`, `db-migrate`, `install-deps`,
`workspace-setup`, `sync-ai-configs`, `multiplexer-session`.

### `devflow hook install <recipe>`

Install a recipe into `.devflow.yml` without overwriting existing entries.
Detection proposes parameter values matching this project; adjust them
interactively or via `--param`. The generated hooks are plain entries —
edit them like any other hook.

```bash
devflow hook install env-file                                    # interactive params
devflow hook install db-migrate --param command="sqlx migrate run" --yes
devflow --json --non-interactive hook install install-deps --yes # CI/agents
```

### `devflow hook setup`

Interactive wizard: probes the project for applicable recipes (services,
lockfiles, migration tools, mise/direnv, ...), lets you multi-select and
confirm parameters, then writes all generated hooks in one go.

```bash
devflow hook setup
```

## AI and Automation

### `devflow commit`

Commit staged changes with a manual or AI-generated message.

```bash
devflow commit
devflow commit -m "fix: typo"
devflow commit --ai
devflow commit --ai --edit
devflow commit --ai --dry-run
```

### `devflow agent status`

Show workspaces that have executed commands tracked by devflow.

```bash
devflow agent status
devflow --json agent status
```

### `devflow agent context`

Output project context for AI tools, including workspace, config, and service connection details.

```bash
devflow agent context
devflow agent context --format json
devflow agent context --workspace feature/auth
```

### `devflow agent skill`

Install devflow workspace helper skills into `.claude/skills/`.

```bash
devflow agent skill
devflow --json agent skill
```

### `devflow sync-ai-configs`

Sync AI tool configuration directories from the current worktree back to the main worktree.

```bash
devflow sync-ai-configs
```

For `.claude/settings.local.json`, permission arrays are unioned and deduplicated. For other AI config directories, copying is additive only.

## Reverse Proxy

### `devflow proxy start`

Start the native HTTP(S) reverse proxy and friendly-name discovery. Discovered containers get `*.local` names (the default suffix on all platforms) advertised over mDNS so the **same name resolves from the host and from inside containers**: on the host via Bonjour (macOS) or Avahi (Linux — needs avahi-daemon + avahi-utils), and inside containers via Docker DNS aliases. Web names resolve to the proxy (HTTPS with the trusted CA); database names resolve directly to the container IP for native access at the standard port (needs routable container IPs: OrbStack/Colima/Linux). Devflow-managed host processes with resolved ports are also routed to `127.0.0.1` as `https://<process>.<workspace>.<project>.<suffix>`. `--domain-suffix localhost` opts into loopback-only names, but beware: many runtimes hard-resolve `*.localhost` to loopback inside containers (RFC 6761), so those names don't work container-to-container.

```bash
devflow proxy start
devflow proxy start --daemon
devflow proxy start --https-port 8443
devflow proxy start --http-port 8080
devflow proxy start --api-port 2020
devflow proxy start --no-mdns          # disable mDNS advertising
```

### `devflow proxy stop`

Stop the proxy daemon.

### `devflow proxy status`

Show proxy status, ports, and CA info.

### `devflow proxy list`

List discovered endpoints. Web services and devflow-managed host processes are shown as HTTPS URLs (`https://name.local`); well-known database ports are shown as native direct endpoints such as `postgresql://name.local:5432`. Database names resolve to the container IP via the proxy's mDNS advertising. Host process names resolve to the proxy and forward to `127.0.0.1:<port>`. If a database name does not resolve, check the proxy is running with mDNS enabled and that your platform routes container IPs, or use the UPSTREAM IP shown by this command.

### `devflow proxy trust`

Manage the local CA trust.

```bash
devflow proxy trust install
devflow proxy trust verify
devflow proxy trust remove
devflow proxy trust info
```

## Setup and Configuration

### `devflow init [path]`

Initialize devflow in the current directory or create and initialize a new project directory.

```bash
devflow init
devflow init myapp
devflow init myapp --name app
devflow init myapp --force
```

### `devflow destroy`

Tear down the entire devflow project. Requires `--force` in non-interactive mode.

```bash
devflow destroy
devflow destroy --force
```

### `devflow config`

Show merged configuration.

```bash
devflow config
devflow config -v
```

### `devflow doctor`

Run diagnostics for config, Docker, VCS, hooks, storage, and connectivity.

```bash
devflow doctor
devflow --json doctor
```

### `devflow install-hooks`

Install devflow-managed VCS hooks. Current Git integration uses `post-checkout` and `pre-commit`.

### `devflow uninstall-hooks`

Remove devflow-managed hooks.

### `devflow shell-init [shell]`

Print shell integration for automatic `cd` when devflow emits `DEVFLOW_CD`.

```bash
eval "$(devflow shell-init)"
eval "$(devflow shell-init bash)"
eval "$(devflow shell-init zsh)"
devflow shell-init fish | source
```

### `devflow worktree-setup`

Set up devflow in an existing Git worktree by copying files and creating service instances. Usually called automatically by hooks.

### `devflow setup-zfs`

Create a file-backed ZFS pool for Copy-on-Write storage on Linux.

```bash
devflow setup-zfs
devflow setup-zfs --size 20G
devflow setup-zfs --pool-name mypool
```

### `devflow capabilities`

Show the machine-readable automation contract summary.

```bash
devflow capabilities
devflow --json capabilities
```

### `devflow gc`

Detect and clean up orphaned projects and leftover state.

```bash
devflow gc
devflow gc --list
devflow gc --all
devflow gc --all --force
devflow --json gc
```

## Plugins

### `devflow plugin list`

List registered plugin services and status.

### `devflow plugin check <name>`

Check whether a plugin service is reachable and responding correctly.

### `devflow plugin init <name>`

Print a skeleton plugin script.

```bash
devflow plugin init my-plugin --lang bash
devflow plugin init my-plugin --lang python
```

## Interactive Tools

### `devflow tui`

Launch the interactive terminal dashboard.

The current tabs are:

- `Workspaces`
- `Services`
- `Proxy`
- `System`
- `Logs`

## Environment Variables

| Variable | Description |
|---|---|
| `DEVFLOW_DISABLED=true` | Completely disable devflow |
| `DEVFLOW_SKIP_HOOKS=true` | Skip hook execution |
| `DEVFLOW_AUTO_CREATE=false` | Override `auto_create_on_workspace` |
| `DEVFLOW_AUTO_SWITCH=false` | Override `auto_switch_on_workspace` |
| `DEVFLOW_BRANCH_FILTER_REGEX=...` | Override workspace filtering |
| `DEVFLOW_DISABLED_BRANCHES=main,release/*` | Disable devflow for specific workspaces |
| `DEVFLOW_CURRENT_BRANCH_DISABLED=true` | Disable devflow for the current workspace only |
| `DEVFLOW_CONTEXT_BRANCH=...` | Override context workspace for parent resolution |
| `DEVFLOW_ZFS_DATASET=...` | Force a specific ZFS dataset |
| `DEVFLOW_LLM_API_KEY=...` | API key for AI commit messages |
| `DEVFLOW_LLM_API_URL=...` | OpenAI-compatible LLM endpoint |
| `DEVFLOW_LLM_MODEL=...` | LLM model name |
| `DEVFLOW_COMMIT_COMMAND=...` | External CLI used for commit generation |
| `DEVFLOW_APPROVE_HOOKS=1` | Auto-approve config-file hooks (CI/agent runs) |
| `DEVFLOW_BACKGROUND_HOOK_TIMEOUT=30` | Seconds to await background hooks before CLI exit |

## Shell Integration Notes

With shell integration installed, commands like `devflow switch`, `devflow init <dir>`, and opening a workspace from the TUI can emit `DEVFLOW_CD=<path>` so your shell moves into the correct worktree automatically.

## Context Override

Override the context workspace used as the default parent for workspace creation:

```bash
DEVFLOW_CONTEXT_BRANCH=release_1_0 devflow switch -c hotfix/patch
```
