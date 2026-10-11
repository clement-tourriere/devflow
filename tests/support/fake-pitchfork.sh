#!/bin/sh
# A fake `pitchfork` for tests. It never touches a real supervisor: what it
# prints comes from files beside the script, and every call is appended to
# calls.log as `cwd|arguments|PITCHFORK_SUPERVISOR_AUTO_START`.
#
#   version_exit       exists: --version fails like a mise shim without a version
#   version            printed after "pitchfork " by --version (default: 2.29.0)
#   missing            lines naming features whose --help does not mention the flag
#   supervisor_down    exists: `supervisor status --json` says down
#   list.json          printed by `list --json`
#   on_list            a shell script `list` runs first (a change landing while
#                      devflow asks); list_fail makes `list` fail, and
#                      list_fails_after_stop makes it fail once `stop` has run
#   proxy.json         printed by `proxy status --json` (default: proxy off)
#   after_start.json   copied over list.json by `start`; start_sleep seconds to
#                      sleep afterwards; start_fail is printed to stderr and fails it
#   after_stop.json    the same for `stop` (stop_sleep, stop_fail); on_stop is a shell
#                      script `stop` runs first (a change landing while the stop runs)
#   stray              exists: `list` and `start` leave a `sleep 30` behind, in their
#                      process group and holding their pipes (pids in stray.pids)
d=$(dirname "$0")
printf '%s|%s|%s\n' "$PWD" "$*" "${PITCHFORK_SUPERVISOR_AUTO_START-unset}" >> "$d/calls.log"

help=0
for a in "$@"; do
  [ "$a" = "--help" ] && help=1
done
if [ "$help" = 1 ]; then
  case "$1 $2" in
    "list --help") feature="list --json"; flag="--json" ;;
    "proxy status") feature="proxy status --json"; flag="--json" ;;
    *) feature=""; flag="" ;;
  esac
  echo "Usage: pitchfork $1 [FLAGS]"
  if [ -n "$flag" ] && ! { [ -f "$d/missing" ] && grep -qxF "$feature" "$d/missing"; }; then
    echo "      $flag  a flag"
  fi
  exit 0
fi

# A process the call leaves behind in its group, holding the pipes (a supervisor
# started by `start`, or a stray of a query); its pid is appended to stray.pids.
if [ -f "$d/stray" ] && { [ "$1" = list ] || [ "$1" = start ]; }; then
  sleep 30 &
  echo $! >> "$d/stray.pids"
fi

case "$1" in
  --version)
    if [ -f "$d/version_exit" ]; then
      echo "mise ERROR No version is set for shim: pitchfork" >&2
      exit 1
    fi
    if [ -f "$d/version" ]; then echo "pitchfork $(cat "$d/version")"; else echo "pitchfork 2.29.0"; fi
    ;;
  supervisor)
    if [ -f "$d/supervisor_down" ]; then echo '{"status": "down"}'; else echo '{"status": "up"}'; fi
    ;;
  list)
    # A change that lands while devflow is asking (a script, run first).
    [ -f "$d/on_list" ] && sh "$d/on_list"
    if [ -f "$d/list_fail" ]; then echo "pitchfork list failed" >&2; exit 1; fi
    if [ -f "$d/list.json" ]; then cat "$d/list.json"; else echo '[]'; fi
    ;;
  proxy)
    if [ -f "$d/proxy.json" ]; then cat "$d/proxy.json"; else echo '{"enabled": false, "slugs": []}'; fi
    ;;
  start)
    # The daemon appears first; a pitchfork that waits for readiness then sleeps.
    [ -f "$d/after_start.json" ] && cp "$d/after_start.json" "$d/list.json"
    [ -f "$d/start_sleep" ] && sleep "$(cat "$d/start_sleep")"
    if [ -f "$d/start_fail" ]; then cat "$d/start_fail" >&2; exit 1; fi
    ;;
  stop)
    [ -f "$d/on_stop" ] && sh "$d/on_stop"
    [ -f "$d/after_stop.json" ] && cp "$d/after_stop.json" "$d/list.json"
    [ -f "$d/list_fails_after_stop" ] && : > "$d/list_fail"
    [ -f "$d/stop_sleep" ] && sleep "$(cat "$d/stop_sleep")"
    if [ -f "$d/stop_fail" ]; then cat "$d/stop_fail" >&2; exit 1; fi
    ;;
  *) echo "unexpected: $*" >&2; exit 64 ;;
esac
