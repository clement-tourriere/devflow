#!/usr/bin/env bash
# Run a command for an agent task in its own worktree, database branch and env.
#
# Usage (from the main checkout):
#   ./examples/agent-task.sh TASK_ID [COMMAND [ARGS...]]
#
# 1. devflow --json --non-interactive switch -c agent/TASK_ID
# 2. run COMMAND in the returned worktree with `devflow exec` (.devflow/env applied)
# 3. remove the workspace from the main checkout, whatever the command's outcome
#    (set KEEP=1 to keep it)
#
# INT and TERM are forwarded to the command's whole process group, and the group has exited before the
# workspace is removed. AGENT_TASK_GRACE (whole seconds, 0 to 3600, default 10) is how long a command may
# ignore the signal, and how long leftovers get after TERM, before the group is killed (KILL); a value
# that is not a base-10 number in range is refused before anything is created. If the group is still
# there after KILL the workspace is kept (never removed over a live process).
#
# Exit codes: the command's own status; 2 if the workspace is not ready (for example the project's hooks
# are not trusted); 70 if the command's processes could not be stopped, so the workspace was kept (the
# message on stderr names the workspace and the remaining PIDs; the command's status is in it); 1 on
# other errors. Requires devflow and jq.
set -euo pipefail

task_id="${1:-}"
if [ -z "$task_id" ]; then
  echo "usage: $0 TASK_ID [COMMAND [ARGS...]]" >&2
  exit 1
fi
shift

# AGENT_TASK_GRACE, validated and normalised to a plain base-10 number before
# anything is created ("08" is 8, not an invalid octal number).
grace="${AGENT_TASK_GRACE:-10}"
case "$grace" in
  '' | *[!0-9]*)
    echo "AGENT_TASK_GRACE must be a whole number of seconds (0 to 3600), not '$grace'" >&2
    exit 1
    ;;
esac
grace="${grace#"${grace%%[!0]*}"}" # without leading zeros: no octal reading, no overflow
grace="${grace:-0}"
if [ "${#grace}" -gt 4 ] || [ "$((10#$grace))" -gt 3600 ]; then
  echo "AGENT_TASK_GRACE must be a whole number of seconds (0 to 3600), not '${AGENT_TASK_GRACE}'" >&2
  exit 1
fi
grace=$((10#$grace))

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

# Set when the command's processes could not be stopped: the workspace stays.
unsafe=0
cleanup() {
  if [ "${KEEP:-0}" != "1" ] && [ "$unsafe" != "1" ]; then
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
# `receipt.services.<key>` holds the binding of each configured service; here the
# first postgres one (the password in its URL is masked in this document).
database="$(jq -r '[.receipt.services // {} | to_entries[] | select(.value.engine == "postgres")][0].value.database // empty' <<<"$out")"
jq -cn --arg workspace "$workspace" --arg path "$path" --arg status "$state" --arg database "$database" \
  '{workspace: $workspace, path: $path, status: $status} + (if $database == "" then {} else {database: $database} end)'

if [ "$#" -eq 0 ]; then
  exit 0
fi

# `devflow exec` sets the workspace's variables (.devflow/env) on top of the
# environment and replaces itself with the command; it refuses to start the
# command without them. This shell stays the parent (so the EXIT trap above
# runs whatever happens), waits for the command and exits with its status.
#
# `set -m` gives the command its own process group (its pid is the group id,
# $pgid below) and the default signal dispositions: without it a non-interactive
# shell starts background commands with SIGINT ignored. The command is then not
# in the terminal's foreground group, so an interactive program that reads the
# terminal is not supported here (run `devflow exec -- CMD` yourself for that).
#
# The group is what this script answers for, not just its leader:
#  - INT and TERM go to the whole group (`kill -s SIG -- -$pgid`), so a dev
#    server or test runner the command started hears them too. The first one
#    starts the grace (AGENT_TASK_GRACE seconds): a command that is still
#    running when it is over, because it ignores the signal, is killed (KILL to
#    the group), and its real exit status is collected before anything else.
#    The script polls for the command instead of blocking in `wait`, because a
#    blocked `wait` would never get to enforce the deadline.
#  - Before the cleanup runs, the group has exited. The cleanup deletes the
#    worktree and drops the workspace's database branches and containers, which
#    a process the command left behind would still be using. When the command
#    itself exits (normally, or after the signal) and members of the group are
#    still alive, they are stopped: TERM at once on a normal exit (the task is
#    over, nothing may outlive it), then KILL if the group is still there after
#    the grace, with a message on stderr. A process that started its own
#    session (setsid, a daemonizing server) has left the group and is not this
#    script's to stop.
#  - After KILL the script confirms the group is gone (it checks for up to
#    five seconds). If it is not (a process in uninterruptible sleep, say), the
#    workspace is NOT removed: the message names it and the remaining PIDs, and
#    the exit status is 70. Removing a worktree and its database under a live
#    process is the harm this script exists to prevent.
cd "$path"
set -m
pgid=""
ticks=0      # tenths of a second spent waiting below: never less than the real time
cancelled="" # the tick of the first INT or TERM
killed=0     # KILL has been sent to the group
confirm=50   # ticks to wait for the group to be gone after KILL
limit=$((grace * 10))
group_alive() {
  [ -n "$pgid" ] && kill -0 -- "-$pgid" 2>/dev/null
}
forward() {
  if [ -n "$pgid" ]; then
    kill -s "$1" -- "-$pgid" 2>/dev/null || kill -s "$1" "$pgid" 2>/dev/null || true
  fi
}
# A signal the person (or the CI runner) sent: forwarded, and the grace starts.
cancel() {
  if [ -z "$cancelled" ]; then
    cancelled=$ticks
  fi
  forward "$1"
}
kill_group() {
  killed=1
  forward KILL
}
# The PIDs of the group's members that are not zombies. Fails when the
# processes cannot be listed.
leftover_pids() {
  local listing
  listing="$(ps -A -o pid= -o pgid= -o stat= 2>/dev/null)" || return 1
  awk -v group="$pgid" '$2 == group && $3 !~ /^Z/ { printf "%s%s", sep, $1; sep = " " }' <<<"$listing"
}
# Wait (at most the grace) for the group to exit, KILL what is left, and
# confirm it is gone. Sets `remaining` when it is not.
remaining=""
settle() {
  local waited=0
  if [ "$killed" -eq 0 ]; then
    while group_alive && [ "$waited" -lt "$limit" ]; do
      sleep 0.1
      waited=$((waited + 1))
    done
    if group_alive; then
      echo "the command's process group $pgid is still running after ${grace}s; killing it" >&2
      kill_group
    fi
  fi
  waited=0
  while group_alive && [ "$waited" -lt "$confirm" ]; do
    sleep 0.1
    waited=$((waited + 1))
  done
  if group_alive; then
    # Members that are only zombies are gone for our purposes (their parent
    # has yet to collect them); anything else, or a list we cannot get, is not.
    if remaining="$(leftover_pids)" && [ -z "$remaining" ]; then
      return 0
    fi
    remaining="${remaining:-unknown (group $pgid)}"
  fi
}
trap 'cancel INT' INT
trap 'cancel TERM' TERM
devflow exec -- "$@" &
pgid=$!
# Wait for the command, which is the group's leader. Once a signal has been
# forwarded and the grace is over, the group is killed; the leader's status is
# collected below, when it is gone.
while kill -0 "$pgid" 2>/dev/null; do
  if [ -n "$cancelled" ] && [ "$killed" -eq 0 ] && [ $((ticks - cancelled)) -ge "$limit" ]; then
    echo "the command is still running ${grace}s after the signal; killing its process group $pgid" >&2
    kill_group
  fi
  sleep 0.1
  ticks=$((ticks + 1))
done
code=0
wait "$pgid" || code=$?
# The command has exited. Anything it left in its group is stopped before the
# cleanup: asked politely first (a signal that was forwarded already did that).
if group_alive; then
  if [ "$killed" -eq 0 ]; then
    echo "the command exited but left processes in its group; stopping them (TERM, KILL after ${grace}s)" >&2
    forward TERM
  fi
  settle
fi
# A signal during the cleanup below must not be forwarded to a finished group.
trap - INT TERM
if [ -n "$remaining" ]; then
  unsafe=1
  echo "error: the command's processes could not be stopped (PID $remaining, group $pgid); \
$workspace was NOT removed (the command's status was $code). Stop them, then run: devflow remove $workspace --force" >&2
  exit 70
fi
exit "$code"
