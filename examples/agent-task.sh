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

BRANCH="agent/${TASK_ID}"

# --non-interactive runs hooks but skips interactive prompts.
# Hooks requiring approval must be pre-approved: devflow hook approvals add "<cmd>"
OUTPUT=$(devflow --json --non-interactive switch -c "$BRANCH")

# If worktrees are enabled, switch to the worktree directory
WORKTREE=$(echo "$OUTPUT" | jq -r '.worktree_path // empty')
[ -n "$WORKTREE" ] && cd "$WORKTREE"

devflow --json service connection "$BRANCH"
