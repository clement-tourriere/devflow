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
OUTPUT=$(devflow --json --non-interactive switch -c "$WORKSPACE")

# Capture the materialized path and use it as the workdir for subsequent agent tool calls.
WORKTREE=$(echo "$OUTPUT" | jq -r '.worktree_path // empty')
if [ -z "$WORKTREE" ] || [ ! -d "$WORKTREE" ]; then
  echo "devflow switch did not return a materialized worktree_path" >&2
  exit 1
fi

CONNECTION=$(devflow --json service connection "$WORKSPACE")

# Emit one machine-readable document containing everything the caller needs.
jq -n \
  --arg worktree_path "$WORKTREE" \
  --argjson switch "$OUTPUT" \
  --argjson connection "$CONNECTION" \
  '{worktree_path: $worktree_path, switch: $switch, connection: $connection}'
