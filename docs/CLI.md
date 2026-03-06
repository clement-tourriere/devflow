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
devflow switch feature/auth --no-services
devflow switch feature/auth --dry-run
devflow switch -c agent/task-42 --sandboxed
```

Important flags:

- `-c, --create` create before switching
- `--from <workspace>` choose the parent workspace
- `-x, --execute <command>` run a command after switching
- `-d, --detach` run the post-switch command in a detached multiplexer session
- `--no-services` skip service branching/switching
- `--no-verify` skip hooks
- `--template` switch to the main/template workspace
- `--no-respect-gitignore` include gitignored files in worktree copy
- `--sandboxed` create the workspace with sandbox restrictions
- `--no-sandbox` disable sandboxing even if enabled by default

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

## Merge and Rebase

These commands are available in the CLI and are especially useful when Smart Merge is enabled.

### `devflow merge [target]`

Merge the current workspace into the target workspace, which defaults to the configured main workspace.

```bash
devflow merge
devflow merge develop
devflow merge --cleanup
devflow merge --dry-run
devflow merge --check-only
devflow merge --cascade-rebase
```

Important flags:

- `--cleanup` remove the source workspace after a successful merge
- `--dry-run` preview without mutating state
- `--force` skip readiness checks
- `--check-only` run checks without merging
- `--cascade-rebase` rebase affected child workspaces after merge

### `devflow rebase [target]`

Rebase the current workspace onto the target workspace.

```bash
devflow rebase
devflow rebase develop
devflow rebase --dry-run
```

## Merge Train

Merge train is part of the Smart Merge feature set. If disabled, `devflow train` exits with guidance on enabling it.

### `devflow train add [workspace]`

Add a workspace to the merge train for a target.

```bash
devflow train add
devflow train add feature/auth
devflow train add --target develop feature/auth
```

### `devflow train remove <workspace>`

Remove a workspace from the merge train.

```bash
devflow train remove feature/auth
```

### `devflow train status`

Show the current merge train queue and entry states.

```bash
devflow train status
devflow train status --target develop
devflow --json train status
```

### `devflow train run`

Run the merge train queue.

```bash
devflow train run
devflow train run --stop-on-failure
devflow train run --cleanup
```

### `devflow train pause`

Pause a merge train.

```bash
devflow train pause
devflow train pause --target develop
```

### `devflow train resume`

Resume a paused merge train.

```bash
devflow train resume
devflow train resume --target develop
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

## Hooks

Hooks are MiniJinja-templated lifecycle entries defined in `.devflow.yml`. They can be shell commands or built-in actions.

### Built-in hook phases

Current built-in phases include:

- `pre-switch`, `post-create`, `post-start`, `post-switch`
- `pre-remove`, `post-remove`
- `pre-commit`, `pre-merge`, `post-merge`, `post-rewrite`
- `pre-rebase`, `post-rebase`, `post-merge-cascade`
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

List available pre-built hook recipes.

```bash
devflow hook recipes
devflow --json hook recipes
```

### `devflow hook install <recipe>`

Install a built-in hook recipe into `.devflow.yml` without overwriting existing entries.

```bash
devflow hook install sync-ai-configs
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

Install project-specific workspace skills into `.agents/skills/` and compatible tool locations.

```bash
devflow agent skill
devflow --json agent skill
```

### `devflow sync-ai-configs`

Merge AI tool configuration directories from the current worktree back to the main worktree.

```bash
devflow sync-ai-configs
```

For `.claude/settings.local.json`, permission arrays are union-merged. For other AI config directories, copying is additive only.

## Reverse Proxy

### `devflow proxy start`

Start the native HTTPS reverse proxy.

```bash
devflow proxy start
devflow proxy start --daemon
devflow proxy start --https-port 8443
devflow proxy start --http-port 8080
devflow proxy start --api-port 2020
```

### `devflow proxy stop`

Stop the proxy daemon.

### `devflow proxy status`

Show proxy status, ports, and CA info.

### `devflow proxy list`

List proxied containers and their URLs.

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

Install devflow-managed VCS hooks. Current Git integration uses `post-checkout`, `post-merge`, `pre-commit`, and `post-rewrite`.

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
| `DEVFLOW_AGENT_COMMAND=...` | Default agent command configured for this project |

## Shell Integration Notes

With shell integration installed, commands like `devflow switch`, `devflow init <dir>`, and opening a workspace from the TUI can emit `DEVFLOW_CD=<path>` so your shell moves into the correct worktree automatically.

## Context Override

Override the context workspace used as the default parent for workspace creation:

```bash
DEVFLOW_CONTEXT_BRANCH=release_1_0 devflow switch -c hotfix/patch
```
