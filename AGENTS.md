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

Check the per-phase `hook_results` summary in the JSON output: `skipped > 0` usually means an approval is missing.

## Bootstrap a Repository

```bash
./examples/agent-bootstrap.sh
```

Equivalent manual flow:

```bash
INIT=$(devflow --json --non-interactive init --name "$(basename "$PWD")")
echo "$INIT" | jq '.next_steps'
devflow --json --non-interactive install-hooks
devflow --json --non-interactive agent skill --target all
devflow --json capabilities
```

## Start Work on a New Task

```bash
TASK_ID="issue-123"
./examples/agent-task.sh "$TASK_ID"  # emits {workspace, agent_workdir, switch, connection}
```

Equivalent manual flow:

```bash
WORKSPACE="agent/$TASK_ID"
OUTPUT=$(devflow --json --non-interactive switch -c "$WORKSPACE")

# Use the materialized workspace path for subsequent agent tool calls
AGENT_WORKDIR=$(echo "$OUTPUT" | jq -r '.agent_workdir // .worktree_path // empty')
test -d "$AGENT_WORKDIR"
CONNECTION=$(devflow --json service connection "$WORKSPACE")

jq -n --arg workspace "$WORKSPACE" --arg agent_workdir "$AGENT_WORKDIR" \
  --argjson switch "$OUTPUT" --argjson connection "$CONNECTION" \
  '{workspace: $workspace, agent_workdir: $agent_workdir, switch: $switch, connection: $connection}'
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
devflow --json agent status        # summary counts + per-workspace agent_workdir

# Get project context (workspace info, services, connections)
devflow agent context
devflow agent context --format json
devflow agent context --workspace feature/auth

# Generate project-specific guide files for coding agents
devflow agent skill --target all
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

# Project health for automation (general checks include hooks, agent guides, worktrees)
devflow --json doctor
# Strict mode exits non-zero if any check fails; inspect summary + next_steps for fixes
devflow --json doctor --strict
# Service-only diagnostics expose the same summary + next_steps shape
devflow --json service doctor --strict
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

## Commits

Use Git and your preferred editor or agent directly. The devflow commit wrapper and commit-generation settings have been retired; Git continues to own commit hooks and signing.

## Automation Contract

- Multi-provider `service create`, `service delete`, and `switch` return non-zero exit code when any provider fails.
- JSON mode emits one document on stdout for supported machine-readable commands; `switch -x`/`--detach`/`--open` nests command or session details under `execution`. Output interfaces (`shell-init`, `completions`, `tui`) reject `--json`.
- `destroy` and `remove` require `--force` in `--non-interactive` or `--json` mode.
- Unapproved hooks are skipped with a warning in non-interactive mode (the command completes; the JSON `hook_results` summary reports them as `skipped`). Set `DEVFLOW_APPROVE_HOOKS=1` to auto-approve.
- Git workspaces are always materialized as linked worktrees; the primary checkout is the default workspace. jj uses native workspaces.
- `devflow --json list` always returns one versioned tree document, including `context_workspace`, `default_workspace`, `roots`, `workspaces`, `flat_order` (canonical depth-first display order), and `warnings`.
- Treat `name` as the raw VCS identity. Use `service_key` (also exposed to hooks as `workspace_key` and the `workspace_sanitized` compatibility alias) for database, container, and path identifiers.
- Read `service_key` from command/inventory output instead of reconstructing it; an unambiguously migrated workspace may retain a legacy key, while ambiguous legacy ownership is blocked.
- A workspace's `parent` is immutable creation provenance. A missing/deleted parent remains visible in inventory rather than silently changing the child into a root.
- Removal checks dirty/default/current workspaces before changing anything. `--force` explicitly accepts dirty-worktree or partial-cleanup risk.
- Use `devflow --json capabilities` for a machine-readable summary of guarantees.

## Readiness and recovery

- Use `switch -c task --from code-parent --data-from data-parent` for separate code and data origins.
- Inspect `operation.status`; `ready` means required setup completed, while `needs_attention` includes skipped hooks or failed prerequisites. Retry the same command to resume pending phases.
- `post-create` stays pending after failure. `doctor` reports unfinished operations. JSON errors include operation details when available.
- Read provider `data_strategy` capabilities; shared Redis, ClickHouse and RustFS children start empty.
- Existing unowned resources require explicit adoption. See `docs-site/src/content/docs/guides/ownership-and-recovery.md` before migrating data.
- Broad AI settings copying and implicit `mise trust` are disabled by default. Configure an approved hook when trust is required.


## Reusable data and operation recovery

```bash
devflow baseline prepare clean --service db --from ./fixtures/base.sql
devflow --json --non-interactive switch -c agent/task --from main --data-from baseline:clean@v1
devflow exec --workspace agent/task -- pytest -k 'login or signup'
devflow --json operation list
devflow --json operation inspect OPERATION_ID
devflow --json operation resume OPERATION_ID
devflow --json retention plan --service db --max-count 10
```

Use exact baseline versions. Inventory exposes `workspaces[].control.data`, stable identities, pins, and desired state. Reset verifies the recorded baseline before restoring it. Baseline preparation/imports can have partial SQL effects; inspect failures and explicitly remove/reprepare unused failed versions.

Required hooks finish before readiness, even with `background: true`. Set `required: false` for optional hooks; their errors and background completion receipts remain visible. Arbitrary shell hooks must tolerate retries.

`exec -- PROGRAM ARGS` preserves literal arguments; use `exec --shell SCRIPT` explicitly for shell syntax. Cleanup protects configured defaults, materialized/locked worktrees, pins, active operations, and data dependencies. Preview with `retention plan`; `retention apply` returns individual failures.

The native process runtime is the supported default. Proxy networking is an optional companion. AI permission merging and the built-in LLM API backend are retired; keep explicit guide generation, file-copy allowlists, and direct VCS commands.
