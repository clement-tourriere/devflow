#!/usr/bin/env bash
set -euo pipefail

# Create/switch an isolated workspace environment for an agent task.
#
# Usage:
#   ./examples/agent-task.sh <task-id>

TASK_ID="${1:-}"
if [ -z "$TASK_ID" ]; then
  echo "usage: $0 <task-id>" >&2
  exit 2
fi

WORKSPACE="agent/${TASK_ID}"

# --non-interactive runs hooks but skips interactive prompts.
# Hooks requiring approval must be pre-approved: devflow hook approvals add "<cmd>"
if ! OUTPUT=$(devflow --json --non-interactive switch -c "$WORKSPACE"); then
  printf '%s\n' "$OUTPUT"
  exit 1
fi

# Capture the materialized path and use it as the workdir for subsequent agent tool calls.
WORKTREE=$(echo "$OUTPUT" | jq -r '.agent_workdir // .worktree_path // empty')
if [ -z "$WORKTREE" ] || [ ! -d "$WORKTREE" ]; then
  echo "devflow switch did not return a materialized worktree_path" >&2
  exit 1
fi

if ! echo "$OUTPUT" | jq -e '.operation.status == "ready"' >/dev/null; then
  jq -n --arg workspace "$WORKSPACE" --arg agent_workdir "$WORKTREE" \
    --argjson switch "$OUTPUT" \
    '{workspace: $workspace, agent_workdir: $agent_workdir, worktree_path: $agent_workdir, ready: false, switch: $switch, connection: null}'
  exit 1
fi

CONNECTION=$(devflow --json service connection "$WORKSPACE")

# Emit one machine-readable document containing everything the caller needs.
jq -n \
  --arg workspace "$WORKSPACE" \
  --arg agent_workdir "$WORKTREE" \
  --arg worktree_path "$WORKTREE" \
  --argjson switch "$OUTPUT" \
  --argjson connection "$CONNECTION" \
  '{workspace: $workspace, agent_workdir: $agent_workdir, worktree_path: $worktree_path, switch: $switch, connection: $connection}'
