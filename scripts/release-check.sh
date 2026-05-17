#!/usr/bin/env bash
# Run the local release-candidate verification suite.
#
# This intentionally mirrors the checks expected before handing a devflow
# release candidate to humans or agents. It avoids destructive integration
# tests; run scripts/integration-test.sh separately when Docker/storage backends
# need end-to-end coverage.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

run() {
  printf '\n==> %s\n' "$*"
  "$@"
}

run cargo fmt --all -- --check
run git diff --check
run ./scripts/generate-llms-txt.sh --check
run cargo check --workspace --all-features
run cargo test --workspace
run cargo build --workspace
run npm --prefix ui run build

printf '\nRelease-candidate checks passed.\n'
