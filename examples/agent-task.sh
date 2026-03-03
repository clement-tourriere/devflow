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

# --no-verify avoids interactive hook approvals in headless runs.
devflow --json --non-interactive switch "$BRANCH" --no-verify >/dev/null
devflow --json service connection "$BRANCH"
