#!/usr/bin/env bash
set -euo pipefail

# Idempotent bootstrap for AI agents and CI runners.
#
# Usage:
#   ./examples/agent-bootstrap.sh [project-name]
#
# Optional env:
#   DEVFLOW_BOOTSTRAP_PROVIDER=local|shared
#
# Cloud providers require credentials and provider-specific fields in project
# configuration; `service add --provider` intentionally cannot invent them.

PROJECT_NAME="${1:-$(basename "$PWD")}"
PROVIDER="${DEVFLOW_BOOTSTRAP_PROVIDER:-local}"
INIT_RESULT=null
SERVICE_RESULT=null

case "$PROVIDER" in
  local|shared) ;;
  *)
    echo "DEVFLOW_BOOTSTRAP_PROVIDER must be 'local' or 'shared'; configure credentialed cloud providers in .devflow.yml" >&2
    exit 2
    ;;
esac

if [ ! -f ".devflow.yml" ]; then
  INIT_RESULT=$(devflow --json --non-interactive init --name "$PROJECT_NAME")
  SERVICE_RESULT=$(devflow --json --non-interactive service add db --provider "$PROVIDER")
fi

if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  devflow --json --non-interactive install-hooks >/dev/null
fi

CAPABILITIES=$(devflow --json capabilities)

jq -n \
  --argjson init "$INIT_RESULT" \
  --argjson service "$SERVICE_RESULT" \
  --argjson capabilities "$CAPABILITIES" \
  '{init: $init, service: $service, capabilities: $capabilities}'
