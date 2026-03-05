# CLI Reference

Complete reference for all devflow commands. All core commands support the `--json` and `--non-interactive` global flags for automation.

Tip: run `devflow --help-all` to print the full command surface directly in the CLI.

## Global Flags

```bash
devflow [--json] [--non-interactive] [-s <service-name>] <command>
```

| Flag | Description |
|---|---|
| `--json` | Output structured JSON on stdout (diagnostics on stderr) |
| `--non-interactive` | Skip all prompts, use defaults |
| `-s <name>` | Target a specific named service |

---

## Workspace Management

### `devflow switch [workspace]`

Switch to a workspace and its associated services/worktree. The most-used command — supports creating, switching, and executing post-switch commands.

```bash
devflow switch                           # Interactive fuzzy picker
devflow switch feature/auth              # Switch to existing workspace
devflow switch -c feature/new            # Create and switch
devflow switch -c feature/new --from develop  # Create from specific parent
devflow switch feature/auth -x "npm ci"  # Run command after switch
devflow switch feature/auth --no-services # Skip service operations
devflow switch feature/auth --no-verify  # Skip hooks
devflow switch --template                # Switch to main/template
devflow switch feature/auth --dry-run    # Preview what would happen
```

### `devflow list`

List all workspaces with service and worktree status.

```bash
devflow list
devflow --json list
```

### `devflow graph`

Render the full environment graph — workspace tree with service status, worktree paths, and provider info.

```bash
devflow graph
devflow --json graph
```

### `devflow link <workspace>`

Link an existing VCS workspace into devflow and optionally materialize service instances.

```bash
devflow link feature/auth
devflow link feature/auth --from main    # Specify parent workspace
```

### `devflow remove <workspace>`

Full cleanup: deletes the Git workspace, worktree, and all service instances.

```bash
devflow remove feature/auth
devflow remove feature/auth --force          # Skip confirmation
devflow remove feature/auth --keep-services  # Keep service instances
```

### `devflow merge [target]`

Merge the current workspace into the target (defaults to main).

```bash
devflow merge                            # Merge into main
devflow merge develop                    # Merge into develop
devflow merge --cleanup                  # Delete source workspace after merge
devflow merge --dry-run                  # Preview the merge
```

### `devflow cleanup`

Remove old service instances, keeping the most recent N. Alias for `devflow service cleanup`.

```bash
devflow cleanup                          # Use max_workspaces from config
devflow cleanup --max-count 5            # Keep only 5 most recent
```

---

## Services

### `devflow service add [name]`

Add and configure a service provider. Interactive wizard when flags are omitted. Use `--from` to seed the main workspace on creation.

```bash
devflow service add app-db --provider local --service-type postgres
devflow service add app-db --provider local --service-type postgres --from ./backup.sql
devflow service add app-db --provider local --service-type postgres --from postgresql://user:pass@host:5432/db
devflow service add app-db --provider local --service-type postgres --from s3://bucket/path/dump.sql
```

### `devflow service remove <name>`

Remove a service from the project configuration.

### `devflow service list`

List all configured services.

### `devflow service status`

Show service status across all providers.

### `devflow service capabilities`

Show the service provider capability matrix — which operations each configured provider supports.

```bash
devflow service capabilities
devflow --json service capabilities
```

### `devflow service create <workspace>`

Create service instance(s) for a workspace without switching VCS or worktree.

```bash
devflow service create feature/auth
devflow service create feature/auth --from develop
```

### `devflow service delete <workspace>`

Delete service instance(s) for a workspace. Keeps the Git workspace and worktree.

```bash
devflow service delete feature/auth
```

### `devflow service cleanup`

Clean up old service instances.

```bash
devflow service cleanup
devflow service cleanup --max-count 5
```

### `devflow service start <workspace>`

Start a stopped container (local provider).

### `devflow service stop <workspace>`

Stop a running container. Preserves data.

### `devflow service reset <workspace>`

Reset workspace data to the state of the parent workspace (local provider).

### `devflow service destroy`

Destroy all instances and data for a service. Requires `--force` in `--json` or `--non-interactive` mode.

```bash
devflow service destroy
devflow service destroy --force
```

### `devflow service connection <workspace>`

Show connection info for a workspace's services.

```bash
devflow service connection feature/auth              # URI (default)
devflow service connection feature/auth --format env  # DATABASE_URL=...
devflow service connection feature/auth --format json # JSON object
```

Also available as a top-level alias: `devflow connection <workspace>`.

### `devflow service logs <workspace>`

Show Docker container logs.

```bash
devflow service logs feature/auth              # Last 100 lines
devflow service logs feature/auth --tail 50    # Last 50 lines
```

### `devflow service seed <workspace> --from <source>`

Seed a workspace from an external source.

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
```

---

## VCS

### `devflow commit`

Commit staged changes with optional AI-generated messages.

```bash
devflow commit                           # Open editor
devflow commit -m "feat: add auth"       # Inline message
devflow commit --ai                      # AI-generated message
devflow commit --ai --edit               # AI message + editor review
devflow commit --ai --dry-run            # Preview AI message only
```

---

## Hooks

### `devflow hook show [phase]`

Show all configured hooks, or hooks for a specific phase.

```bash
devflow hook show
devflow hook show post-create
```

### `devflow hook run <phase> [name]`

Manually run hooks for a phase.

```bash
devflow hook run post-create                     # All post-create hooks
devflow hook run post-create migrate             # Just the "migrate" hook
devflow hook run post-create --workspace feat/x  # For a specific workspace
```

### `devflow hook explain <phase>`

Explain what a hook phase does and when it fires.

### `devflow hook vars`

Show all template variables available for the current workspace.

```bash
devflow hook vars
devflow hook vars --workspace feat/x
devflow --json hook vars
```

### `devflow hook render <template>`

Render a MiniJinja template string against the current workspace context.

```bash
devflow hook render "DATABASE_URL={{ service['app-db'].url }}"
```

### `devflow hook approvals`

Manage the hook approval store.

```bash
devflow hook approvals list              # List approved hooks
devflow hook approvals add "npm test"    # Approve a specific command
devflow hook approvals clear             # Clear all approvals
```

### `devflow hook triggers`

Show the VCS event to hook phase mapping.

### `devflow hook actions`

List built-in action types (shell, replace, write-file, write-env, copy, docker-exec, http).

---

## AI Agents

### `devflow agent start <task>`

Start an AI agent in a new isolated workspace. Creates a workspace with the configured prefix (default: `agent/`), provisions services, and launches the agent command.

```bash
devflow agent start fix-login -- 'Fix the login timeout bug'
devflow agent start fix-login --command codex
devflow agent start fix-login --dry-run
```

### `devflow agent status`

Show agent status across all workspaces.

```bash
devflow agent status
devflow --json agent status
```

### `devflow agent context`

Output project context (workspace info, services, connections) for AI agents.

```bash
devflow agent context
devflow agent context --format json
devflow agent context --workspace feature/x
```

### `devflow agent skill`

Generate project-specific skills and rules for AI coding tools.

```bash
devflow agent skill                      # All tools (Claude, Cursor, OpenCode)
devflow agent skill --target claude      # .claude/skills/devflow/SKILL.md
devflow agent skill --target cursor      # .cursor/rules/devflow.md
devflow agent skill --target opencode    # AGENTS.md
```

### `devflow agent docs`

Generate an `AGENTS.md` tailored to this project.

---

## Reverse Proxy

### `devflow proxy start`

Start the native HTTPS reverse proxy. Auto-discovers Docker containers and serves them via `*.localhost` domains.

```bash
devflow proxy start                      # Start in foreground
devflow proxy start --daemon             # Start in background
devflow proxy start --https-port 8443    # Custom HTTPS port
devflow proxy start --http-port 8080     # Custom HTTP port
devflow proxy start --api-port 2020      # Custom API port
```

### `devflow proxy stop`

Stop the proxy daemon.

### `devflow proxy status`

Show proxy status (running/stopped, ports, CA info).

### `devflow proxy list`

List all proxied containers with their HTTPS URLs.

### `devflow proxy trust`

Manage the Certificate Authority for HTTPS.

```bash
devflow proxy trust install              # Install CA to system trust
devflow proxy trust verify               # Check if CA is trusted
devflow proxy trust remove               # Remove CA from system trust
devflow proxy trust info                 # Show platform-specific instructions
```

---

## Setup & Configuration

### `devflow init [path]`

Initialize devflow in the current directory or create and initialize a new path. Creates `.devflow.yml`.

```bash
devflow init                             # Initialize current directory
devflow init myapp                       # Create ./myapp and initialize
devflow init myapp --name app            # Explicit project name
devflow init myapp --force               # Overwrite existing config
```

### `devflow destroy`

Tear down the entire devflow project (inverse of init). Requires `--force` in non-interactive mode.

```bash
devflow destroy
devflow destroy --force
```

### `devflow config`

Show the current merged configuration.

```bash
devflow config                           # Show config
devflow config -v                        # Show with precedence details
```

### `devflow doctor`

Run system diagnostics — checks config, Docker, VCS, hooks, storage, and connectivity.

```bash
devflow doctor
devflow --json doctor
```

### `devflow install-hooks`

Install devflow Git hooks (post-checkout, post-merge, pre-commit, post-rewrite).

### `devflow uninstall-hooks`

Remove devflow Git hooks. Only removes hooks with the devflow marker.

### `devflow shell-init [shell]`

Print the shell wrapper function for automatic worktree `cd`.

```bash
eval "$(devflow shell-init)"             # Auto-detect shell
eval "$(devflow shell-init bash)"        # Bash
eval "$(devflow shell-init zsh)"         # Zsh
devflow shell-init fish | source         # Fish
```

### `devflow worktree-setup`

Set up devflow in an existing Git worktree (copy files, create service instances). Normally called automatically by hooks.

### `devflow setup-zfs`

Create a file-backed ZFS pool for Copy-on-Write storage (Linux only).

```bash
devflow setup-zfs                        # 10G pool named "devflow"
devflow setup-zfs --size 20G             # Custom size
devflow setup-zfs --pool-name mypool     # Custom pool name
```

### `devflow capabilities`

Show the machine-readable automation contract summary.

```bash
devflow capabilities
devflow --json capabilities
```

### `devflow gc`

Garbage collection — detect and clean up orphaned projects.

```bash
devflow gc                               # Interactive cleanup
devflow gc --list                        # List orphans only
devflow gc --all                         # Clean up all orphans
devflow gc --force                       # Skip confirmation
```

### `devflow tui`

Launch the interactive terminal UI dashboard.

---

## Plugins

### `devflow plugin list`

List all configured plugin providers.

### `devflow plugin check <name>`

Verify a plugin provider is reachable and responds correctly.

### `devflow plugin init <name>`

Generate a plugin scaffold script.

```bash
devflow plugin init my-plugin --lang bash      # Bash scaffold
devflow plugin init my-plugin --lang python    # Python scaffold
```

---

## Shell Integration

Add to your shell profile for automatic `cd` into worktrees when devflow emits `DEVFLOW_CD`:

```bash
# Bash (~/.bashrc) or Zsh (~/.zshrc)
eval "$(devflow shell-init)"

# Fish (~/.config/fish/config.fish)
devflow shell-init fish | source
```

This creates a `devflow` shell wrapper that automatically changes directory after commands like `devflow switch`, `devflow init <dir>`, or opening a workspace from the TUI.

---

## Context Override

Override the devflow context workspace used as the default parent for workspace creation:

```bash
DEVFLOW_CONTEXT_BRANCH=release_1_0 devflow switch -c hotfix_patch
```

---

## Environment Variables

| Variable | Description |
|---|---|
| `DEVFLOW_DISABLED=true` | Completely disable devflow |
| `DEVFLOW_SKIP_HOOKS=true` | Skip Git hook execution |
| `DEVFLOW_AUTO_CREATE=false` | Override auto_create_on_workspace |
| `DEVFLOW_AUTO_SWITCH=false` | Override auto_switch_on_workspace |
| `DEVFLOW_BRANCH_FILTER_REGEX=...` | Override workspace filtering |
| `DEVFLOW_DISABLED_BRANCHES=main,release/*` | Disable for specific workspaces |
| `DEVFLOW_CURRENT_BRANCH_DISABLED=true` | Disable for current workspace only |
| `DEVFLOW_CONTEXT_BRANCH=...` | Override context workspace for parent resolution |
| `DEVFLOW_ZFS_DATASET=...` | Force a specific ZFS dataset |
| `DEVFLOW_LLM_API_KEY=...` | API key for AI commit messages |
| `DEVFLOW_LLM_API_URL=...` | LLM endpoint URL (OpenAI-compatible) |
| `DEVFLOW_LLM_MODEL=...` | LLM model name |
| `DEVFLOW_COMMIT_COMMAND=...` | External CLI for commit messages (e.g., `claude -p`) |
| `DEVFLOW_AGENT_COMMAND=...` | Default agent command (e.g., `claude`, `codex`) |
