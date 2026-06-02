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

# Set up a temporary git repo for testing
TEST_DIR="/tmp/devflow-test-$$"
mkdir -p "$TEST_DIR" && cd "$TEST_DIR"
git init
git config user.email "ci@test.com"
git config user.name "CI"
git commit --allow-empty -m "init"

cleanup() {
  cd /
  rm -rf "$TEST_DIR"
}
trap cleanup EXIT

# 1. Doctor + Init
echo "--- doctor ---"
$DEVFLOW_BIN doctor

echo "--- init ---"
$DEVFLOW_BIN --non-interactive init ci-test
cd ci-test

# Configure a local Postgres service. `devflow init` no longer creates a
# service in non-interactive mode; services are stored in local state via
# `devflow service add`.
echo "--- service add app-db ---"
$DEVFLOW_BIN --non-interactive service add app-db --provider local --service-type postgres

# 2. Verify storage backend
# `devflow status --json` returns a map keyed by service name for configured
# services. Older builds exposed a top-level `.storage`; keep the fallback so
# this script remains useful against both shapes.
echo "--- verify storage ---"
STATUS_JSON=$($DEVFLOW_BIN --json status)
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

# 3. Workspace lifecycle: create -> list -> connection -> stop -> start -> delete
echo "--- service create test-feature ---"
$DEVFLOW_BIN --non-interactive service create test-feature

echo "--- list ---"
$DEVFLOW_BIN --json list

echo "--- service connection ---"
$DEVFLOW_BIN --json service connection test-feature

echo "--- service stop ---"
$DEVFLOW_BIN service stop test-feature

echo "--- service start ---"
$DEVFLOW_BIN service start test-feature

echo "--- service delete ---"
$DEVFLOW_BIN service delete test-feature

echo "--- list (post-delete) ---"
$DEVFLOW_BIN --json list

# 4. Cleanup
echo "--- service destroy ---"
$DEVFLOW_BIN --non-interactive service destroy --force

echo ""
echo "=== All integration tests passed ==="
