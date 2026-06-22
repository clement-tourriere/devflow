#!/usr/bin/env bash
#
# auto-merge-zellij.sh — run the PR auto-merge loop inside a detached zellij
# session so it keeps waking up every 10 minutes independently of any terminal.
#
# The loop itself lives in scripts/auto-merge-prs.sh; this is just a supervisor
# that parks it in a background zellij session you can attach to at any time.
#
# Usage:
#   scripts/auto-merge-zellij.sh start  [loop args...]   # start (default)
#   scripts/auto-merge-zellij.sh stop                    # stop loop + kill session
#   scripts/auto-merge-zellij.sh restart [loop args...]  # stop then start
#   scripts/auto-merge-zellij.sh status                  # is it running?
#   scripts/auto-merge-zellij.sh attach                  # open the live pane
#   scripts/auto-merge-zellij.sh logs                    # tail the log file
#
# Examples:
#   scripts/auto-merge-zellij.sh start                       # 10-min loop, pi reviewer
#   scripts/auto-merge-zellij.sh start --interval 300 --strict
#   scripts/auto-merge-zellij.sh restart --method squash
#
# Env:
#   AUTOMERGE_SESSION   zellij session name (default: devflow-automerge)
#
set -uo pipefail

# Resolve repo root so the loop and logs are always relative to it.
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

SESSION="${AUTOMERGE_SESSION:-devflow-automerge}"
LOOP="$ROOT/scripts/auto-merge-prs.sh"
LOG="$ROOT/auto-merge.out.log"

command -v zellij >/dev/null || { echo "zellij not found" >&2; exit 1; }
[[ -x "$LOOP" ]] || { echo "loop script not found/executable: $LOOP" >&2; exit 1; }

session_exists() { zellij list-sessions -s 2>/dev/null | grep -qx "$SESSION"; }
loop_running()   { pgrep -f "auto-merge-prs\.sh" >/dev/null 2>&1; }

cmd="${1:-start}"; [[ $# -gt 0 ]] && shift || true

case "$cmd" in
  start)
    if loop_running; then
      echo "auto-merge loop already running (pid: $(pgrep -f 'auto-merge-prs\.sh' | tr '\n' ' '))."
      echo "Use 'restart' to apply new args, or 'attach' to view it."
      exit 0
    fi

    session_exists || zellij attach --create-background "$SESSION" >/dev/null 2>&1
    # Give the background daemon a moment to come up.
    for _ in 1 2 3 4 5; do session_exists && break; sleep 0.5; done

    # Run the loop in a named pane; tee to a logfile so output survives detaches.
    # shellcheck disable=SC2068
    zellij --session "$SESSION" run --name "auto-merge" --cwd "$ROOT" -- \
      bash -lc "exec scripts/auto-merge-prs.sh ${*:-} 2>&1 | tee -a '$LOG'"

    sleep 1
    if loop_running; then
      echo "✅ auto-merge loop started in zellij session '$SESSION'."
      echo "   attach:  $0 attach   (detach with Ctrl-o then d)"
      echo "   logs:    $0 logs"
      echo "   stop:    $0 stop"
    else
      echo "⚠️  started the pane but the loop process isn't visible yet — check '$0 logs'." >&2
    fi
    ;;

  stop)
    # The loop spends most of its time in `sleep`, so a SIGTERM to the bash
    # process alone is queued behind the sleep. Kill children first, then the
    # loop itself, escalating to SIGKILL so it stops immediately.
    pids=$(pgrep -f "auto-merge-prs\.sh" || true)
    if [[ -n "$pids" ]]; then
      for p in $pids; do pkill -TERM -P "$p" 2>/dev/null; done
      kill -TERM $pids 2>/dev/null
      sleep 1
      for p in $pids; do pkill -KILL -P "$p" 2>/dev/null; done
      kill -KILL $pids 2>/dev/null || true
      echo "stopped loop process(es): $pids"
    else
      echo "no loop process running."
    fi
    if session_exists; then
      zellij kill-session "$SESSION"  >/dev/null 2>&1 || true
      zellij delete-session "$SESSION" --force >/dev/null 2>&1 || true
      echo "killed zellij session '$SESSION'."
    fi
    ;;

  restart)
    "$0" stop
    sleep 1
    "$0" start "$@"
    ;;

  status)
    echo "session '$SESSION': $(session_exists && echo present || echo absent)"
    if loop_running; then
      echo "loop process: running"
      ps -o pid,etime,args -C bash 2>/dev/null | grep auto-merge-prs || pgrep -af auto-merge-prs.sh
    else
      echo "loop process: not running"
    fi
    if [[ -f "$ROOT/.devflow-automerge.jsonl" ]]; then
      echo "--- last 5 audit records ---"
      tail -n 5 "$ROOT/.devflow-automerge.jsonl"
    fi
    ;;

  attach)
    session_exists || { echo "session '$SESSION' not running. Start it with: $0 start" >&2; exit 1; }
    exec zellij attach "$SESSION"
    ;;

  logs)
    [[ -f "$LOG" ]] || { echo "no log file yet at $LOG" >&2; exit 1; }
    exec tail -n 100 -f "$LOG"
    ;;

  -h|--help|help)
    sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
    ;;

  *)
    echo "unknown command: $cmd (try: start|stop|restart|status|attach|logs)" >&2
    exit 2
    ;;
esac
