# devflow for AI Agents

This guide is for autonomous coding agents and CI runners.

## Goal

Use devflow to create an isolated development workspace environment per task, with machine-readable output and deterministic behavior.

## Recommended Flags

- `--json`: structured output on stdout
- `--non-interactive`: disable prompts in automation (unapproved shell hooks are **skipped with a warning**, never block the command)

> **Note**: `--no-verify` on `switch` skips **all** lifecycle hooks entirely. This is usually not what agents want — hooks run migrations, set up `.env` files, and configure tools. Use `--non-interactive` instead, which runs hooks but skips interactive prompts.

## Hook Approval

Shell hooks from `.devflow.yml` require approval before they run. In `--non-interactive` mode an unapproved hook is **skipped** (visibly, and counted in the JSON result) rather than aborting — so `switch` always completes and reports `worktree_path`. To make hooks actually run in automation, either:

```bash
# Option A — approve once per project (keyed on the command TEMPLATE, so one
# approval covers every workspace, including agent-created worktrees):
devflow hook approvals add "mise trust"
devflow hook approvals add "npm run migrate"
devflow hook approvals list

# Option B — auto-approve all config-file hooks for this run (CI/agents):
DEVFLOW_APPROVE_HOOKS=1 devflow --json --non-interactive switch -c agent/task-42
```

Check the per-phase `hooks` summary in the JSON output: `skipped > 0` usually means an approval is missing.

## Bootstrap a Repository

```bash
./examples/agent-bootstrap.sh
```

Equivalent manual flow:

```bash
devflow --json --non-interactive init --name "$(basename "$PWD")"
devflow --json install-hooks
devflow --json capabilities
```

## Start Work on a New Task

```bash
TASK_ID="issue-123"
./examples/agent-task.sh "$TASK_ID"
```

Equivalent manual flow:

```bash
WORKSPACE="agent/$TASK_ID"
OUTPUT=$(devflow --json --non-interactive switch -c "$WORKSPACE")

# Keep the materialized workspace path and use it for subsequent agent tool calls
WORKTREE=$(echo "$OUTPUT" | jq -r '.worktree_path // empty')
test -d "$WORKTREE"

devflow --json service connection "$WORKSPACE"
```

## Agent Commands

devflow includes built-in agent management commands:

```bash
# Launch an agent (or any command) inside an isolated workspace:
# -x runs the command after switching, in the workspace's worktree.
devflow switch -c agent/fix-login -x claude -- -p 'Fix the login timeout bug'
devflow switch -c agent/fix-login -x codex
devflow switch -c agent/fix-login -x claude --detach   # in a tmux/zellij session

# Check agent workspaces
devflow agent status
devflow --json agent status

# Get project context (workspace info, services, connections)
devflow agent context
devflow agent context --format json
devflow agent context --workspace feature/auth

# Install the bundled workspace helper skills (.claude/skills/)
devflow agent skill
```

## Hook Inspection

Agents can inspect hooks and template variables without running them:

```bash
# Show all template variables for the current workspace
devflow hook vars
devflow --json hook vars

# Render a template string
devflow hook render "DATABASE_URL={{ service['app-db'].url }}"

# Explain what a hook phase does
devflow hook explain post-create
```

## Suggested Agent Loop

```bash
WORKSPACE="agent/$TASK_ID"

# 1) Create/switch isolated environment for this task
OUTPUT=$(devflow --json --non-interactive switch -c "$WORKSPACE")

# 2) Use the materialized workspace as the workdir for subsequent agent tool calls
WORKTREE=$(echo "$OUTPUT" | jq -r '.worktree_path // empty')
test -d "$WORKTREE"

# 3) Read connection info and run the task
CONN=$(devflow --json service connection "$WORKSPACE" | jq -r '.connection_string')

# 4) Optional reset for retries
devflow --json --non-interactive service reset "$WORKSPACE"

# 5) Cleanup from the primary checkout; a workspace cannot remove itself
PROJECT_ROOT=$(devflow --json list | jq -r '.project.root')
(cd "$PROJECT_ROOT" && devflow --json --non-interactive remove "$WORKSPACE" --force)
```

## AI Commit Messages

```bash
# Generate commit message via external CLI tool (preferred)
devflow commit --ai

# Configure in .devflow.yml:
# commit:
#   generation:
#     command: "claude -p --model haiku"
#
# Or via environment:
# DEVFLOW_COMMIT_COMMAND="claude -p --model haiku"
# DEVFLOW_LLM_API_KEY=sk-...  (OpenAI-compatible API fallback)
```

## Automation Contract

- Multi-provider `service create`, `service delete`, and `switch` return non-zero exit code when any provider fails.
- JSON mode emits one document on stdout for supported machine-readable commands; `switch -x`/`--detach`/`--open` nests command or session details under `execution`. Output interfaces (`shell-init`, `completions`, `tui`) reject `--json`.
- `destroy` and `remove` require `--force` in `--non-interactive` or `--json` mode.
- Unapproved hooks are skipped with a warning in non-interactive mode (the command completes; the JSON `hooks` summary reports them as `skipped`). Set `DEVFLOW_APPROVE_HOOKS=1` to auto-approve.
- Git workspaces are always materialized as linked worktrees; the primary checkout is the default workspace. jj uses native workspaces.
- `devflow --json list` always returns one versioned tree document, including `context_workspace`, `default_workspace`, `roots`, `workspaces`, `flat_order` (canonical depth-first display order), and `warnings`.
- Treat `name` as the raw VCS identity. Use `service_key` (also exposed to hooks as `workspace_key` and the `workspace_sanitized` compatibility alias) for database, container, and path identifiers.
- Read `service_key` from command/inventory output instead of reconstructing it; an unambiguously migrated workspace may retain a legacy key, while ambiguous legacy ownership is blocked.
- A workspace's `parent` is immutable creation provenance. A missing/deleted parent remains visible in inventory rather than silently changing the child into a root.
- Removal checks dirty/default/current workspaces before changing anything. `--force` explicitly accepts dirty-worktree or partial-cleanup risk.
- Use `devflow --json capabilities` for a machine-readable summary of guarantees.
