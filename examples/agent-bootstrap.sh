#!/usr/bin/env bash
set -euo pipefail

# Idempotent bootstrap for AI agents and CI runners.
#
# Usage:
#   ./examples/agent-bootstrap.sh [project-name]
#
# Optional env:
#   DEVFLOW_BOOTSTRAP_BACKEND=local|postgres_template|neon|dblab|xata

PROJECT_NAME="${1:-$(basename "$PWD")}" 
BACKEND="${DEVFLOW_BOOTSTRAP_BACKEND:-local}"

if [ ! -f ".devflow.yml" ]; then
  devflow --json --non-interactive init "$PROJECT_NAME" --backend "$BACKEND"
fi

if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  devflow --json --non-interactive install-hooks >/dev/null
fi

devflow --json capabilities
