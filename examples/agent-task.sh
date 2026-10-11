#!/usr/bin/env bash
# Run a command for an agent task in its own worktree, database branch and env.
#
# Usage (from the main checkout):
#   ./examples/agent-task.sh TASK_ID [COMMAND [ARGS...]]
#
# 1. devflow --json --non-interactive switch -c agent/TASK_ID
# 2. run COMMAND in the returned worktree with .devflow/env loaded
# 3. remove the workspace from the main checkout (set KEEP=1 to keep it)
#
# Exit codes: the command's, or 2 if the workspace is not ready (for example the
# project's hooks are not trusted), or 1 on errors. Requires devflow and jq.
set -euo pipefail

task_id="${1:-}"
if [ -z "$task_id" ]; then
  echo "usage: $0 TASK_ID [COMMAND [ARGS...]]" >&2
  exit 1
fi
shift

workspace="agent/${task_id}"
main_checkout="$(pwd)"

# Create the worktree. Exit status 0 = ready, 2 = needs_attention / in_progress,
# anything else = error. The document is on stdout; progress is on stderr.
# If a phase fails after the worktree was created (for example the database server
# is unreachable), this exits 1 before the cleanup trap below exists: the worktree
# is kept and the error names it. Re-run `devflow setup` there, or remove it with
# `devflow remove` from the main checkout.
status=0
out="$(devflow --json --non-interactive switch -c "$workspace")" || status=$?
if [ "$status" -ne 0 ] && [ "$status" -ne 2 ]; then
  jq -r '.error // "devflow switch failed"' <<<"$out" >&2 || printf '%s\n' "$out" >&2
  exit 1
fi

path="$(jq -r '.workspace.path' <<<"$out")"
state="$(jq -r '.status' <<<"$out")"
if [ ! -d "$path" ]; then
  echo "devflow did not return a worktree path" >&2
  exit 1
fi

cleanup() {
  if [ "${KEEP:-0}" != "1" ]; then
    # From the main checkout: devflow refuses to remove the workspace you are in.
    # --force also skips teardown hooks that are not trusted (with a warning).
    (cd "$main_checkout" && devflow --json --non-interactive remove "$workspace" --force >/dev/null) \
      || echo "warning: could not remove $workspace; run: devflow remove $workspace --force" >&2
  fi
}
trap cleanup EXIT

if [ "$status" -eq 2 ]; then
  # Most often the [hooks] are not trusted: run `devflow trust` once, or set
  # DEVFLOW_TRUST_HOOKS=1 for this run if you have read them.
  echo "workspace $workspace is $state; not running the command" >&2
  jq '{status, path, receipt, warnings}' <<<"$out" >&2
  exit 2
fi

# Emit one machine-readable line about the environment, then run the command.
jq -cn --arg workspace "$workspace" --arg path "$path" --arg status "$state" \
  '{workspace: $workspace, path: $path, status: $status}'

if [ "$#" -eq 0 ]; then
  exit 0
fi

# The generated env uses single-quoted values, so it can be sourced by a shell.
(
  cd "$path"
  set -a
  # shellcheck disable=SC1091
  . ./.devflow/env
  set +a
  exec "$@"
)
