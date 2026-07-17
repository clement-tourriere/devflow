#!/usr/bin/env bash
#
# integration-test.sh — Shared integration test for devflow CI.
#
# Usage:
#   DEVFLOW_BIN=/path/to/devflow EXPECTED_STORAGE=zfs ./scripts/integration-test.sh
#
# Required env vars:
#   DEVFLOW_BIN         — Path to the devflow binary
#   EXPECTED_STORAGE    — Expected storage backend (zfs, reflink, apfs_clone, copy)
#
# Optional env vars:
#   DEVFLOW_ZFS_DATASET — ZFS dataset name (required for ZFS tests)
#
set -euo pipefail

: "${DEVFLOW_BIN:?DEVFLOW_BIN must be set}"
: "${EXPECTED_STORAGE:?EXPECTED_STORAGE must be set}"

echo "=== devflow integration test ==="
echo "Binary:           $DEVFLOW_BIN"
echo "Expected storage: $EXPECTED_STORAGE"
echo ""

# Set up a temporary project directory for testing.
TEST_DIR=$(mktemp -d "${TMPDIR:-/tmp}/devflow-test.XXXXXX")
cd "$TEST_DIR"

cleanup() {
  cd /
  rm -rf "$TEST_DIR"
}
trap cleanup EXIT

# 1. Init + Doctor
echo "--- init ---"
mkdir ci-test
cd ci-test
git init
git config user.email "ci@test.com"
git config user.name "CI"
git commit --allow-empty -m "init"
"$DEVFLOW_BIN" --non-interactive init --name ci-test
PROJECT_ROOT=$(pwd -P)

# Configure a local Postgres service. `devflow init` no longer creates a
# service in non-interactive mode; services are stored in local state via
# `devflow service add`.
echo "--- service add app-db ---"
"$DEVFLOW_BIN" --non-interactive service add app-db --provider local --service-type postgres

# Doctor is a health gate and intentionally returns non-zero when no VCS is
# present, so run it only after entering the initialized project.
echo "--- doctor ---"
"$DEVFLOW_BIN" doctor

# 2. Verify storage backend
# `devflow status --json` returns a map keyed by service name for configured
# services. Older builds exposed a top-level `.storage`; keep the fallback so
# this script remains useful against both shapes.
echo "--- verify storage ---"
STATUS_JSON=$("$DEVFLOW_BIN" --json status)
STORAGE=$(jq -r '
  if type == "object" and (.storage? != null) then
    .storage
  elif type == "object" then
    ([to_entries[] | select(.value | type == "object") | .value.storage? | select(. != null)] | first) // "null"
  else
    "null"
  end
' <<<"$STATUS_JSON")
echo "Detected storage: $STORAGE"
if [ "$STORAGE" != "$EXPECTED_STORAGE" ]; then
  echo "ERROR: Expected storage '$EXPECTED_STORAGE' but got '$STORAGE'"
  echo "$STATUS_JSON" | jq .
  exit 1
fi

# 3. Workspace lifecycle: switch/create -> inventory -> child lineage -> remove
WORKSPACE="integration/parent"
CHILD_WORKSPACE="integration/child"

echo "--- switch -c $WORKSPACE ---"
SWITCH_JSON=$("$DEVFLOW_BIN" --json --non-interactive switch -c "$WORKSPACE")
WORKTREE_PATH=$(jq -r '.worktree_path // empty' <<<"$SWITCH_JSON")
if [ -z "$WORKTREE_PATH" ] || [ ! -d "$WORKTREE_PATH" ]; then
  echo "ERROR: switch did not return a materialized worktree_path"
  echo "$SWITCH_JSON" | jq .
  exit 1
fi
if [ "$WORKTREE_PATH" = "$PWD" ]; then
  echo "ERROR: additional Git workspace reused the primary checkout"
  exit 1
fi
git -C "$WORKTREE_PATH" rev-parse --is-inside-work-tree >/dev/null

echo "--- list (from linked worktree) ---"
cd "$WORKTREE_PATH"
LIST_JSON=$("$DEVFLOW_BIN" --json list)
jq -e --arg workspace "$WORKSPACE" --arg project_root "$PROJECT_ROOT" '
  (.schema_version | type == "number") and
  (.project.name == "ci-test") and
  (.project.root == $project_root) and
  (.default_workspace | type == "string") and
  (.context_workspace == $workspace) and
  (.roots | type == "array") and
  (.workspaces | type == "array") and
  (.warnings | type == "array") and
  any(.workspaces[];
    .name == $workspace and
    (.service_key | type == "string") and
    .service_key != .name and
    (.worktree_path | type == "string")
  )
' <<<"$LIST_JSON" >/dev/null
DEFAULT_WORKSPACE=$(jq -r '.default_workspace // empty' <<<"$LIST_JSON")
if [ -z "$DEFAULT_WORKSPACE" ]; then
  echo "ERROR: inventory did not report default_workspace"
  exit 1
fi

echo "--- resolve primary workspace (from linked worktree) ---"
MAIN_JSON=$("$DEVFLOW_BIN" --json --non-interactive switch "$DEFAULT_WORKSPACE" --no-services)
MAIN_PATH=$(jq -r '.worktree_path // empty' <<<"$MAIN_JSON")
if [ "$MAIN_PATH" != "$PROJECT_ROOT" ]; then
  echo "ERROR: primary workspace resolved to '$MAIN_PATH', expected '$PROJECT_ROOT'"
  echo "$MAIN_JSON" | jq .
  exit 1
fi

echo "--- service connection ---"
"$DEVFLOW_BIN" --json service connection "$WORKSPACE"

echo "--- service stop ---"
"$DEVFLOW_BIN" service stop "$WORKSPACE"

echo "--- service start ---"
"$DEVFLOW_BIN" service start "$WORKSPACE"

echo "--- switch -c $CHILD_WORKSPACE --from $WORKSPACE ---"
CHILD_JSON=$("$DEVFLOW_BIN" --json --non-interactive switch -c "$CHILD_WORKSPACE" --from "$WORKSPACE" --no-services)
CHILD_PATH=$(jq -r '.worktree_path // empty' <<<"$CHILD_JSON")
if [ -z "$CHILD_PATH" ] || [ ! -d "$CHILD_PATH" ]; then
  echo "ERROR: child switch did not materialize a worktree"
  echo "$CHILD_JSON" | jq .
  exit 1
fi

cd "$PROJECT_ROOT"

echo "--- verify parent lineage ---"
LIST_JSON=$("$DEVFLOW_BIN" --json list)
jq -e --arg child "$CHILD_WORKSPACE" --arg parent "$WORKSPACE" '
  any(.workspaces[]; .name == $child and .parent == $parent)
' <<<"$LIST_JSON" >/dev/null

# Non-interactive removal without explicit force must fail before changing the
# worktree. This also guards the automation contract used by agents and CI.
echo "--- verify removal preflight ---"
touch "$CHILD_PATH/integration-untracked"
if "$DEVFLOW_BIN" --json --non-interactive remove "$CHILD_WORKSPACE" >/dev/null 2>&1; then
  echo "ERROR: non-interactive removal succeeded without --force"
  exit 1
fi
if [ ! -d "$CHILD_PATH" ]; then
  echo "ERROR: failed removal mutated the child worktree"
  exit 1
fi

echo "--- remove child workspace ---"
"$DEVFLOW_BIN" --json --non-interactive remove "$CHILD_WORKSPACE" --force

echo "--- remove parent workspace ---"
"$DEVFLOW_BIN" --json --non-interactive remove "$WORKSPACE" --force

echo "--- list (post-delete) ---"
LIST_JSON=$("$DEVFLOW_BIN" --json list)
jq -e --arg parent "$WORKSPACE" --arg child "$CHILD_WORKSPACE" '
  all(.workspaces[]; .name != $parent and .name != $child)
' <<<"$LIST_JSON" >/dev/null

# 4. Cleanup
echo "--- service destroy ---"
"$DEVFLOW_BIN" --non-interactive service destroy --force

echo ""
echo "=== All integration tests passed ==="
