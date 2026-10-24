#!/bin/sh
# Quick start against a scratch repository, with a devflow binary that was just
# installed: the version, init, switch -c, list and remove. Nothing outside a
# temporary directory is touched (the registry and data directories are moved
# there too).
#
# Usage: smoke-test.sh DEVFLOW_BINARY [EXPECTED_VERSION]
#   EXPECTED_VERSION  e.g. 1.0.0 or v1.0.0; --version must print exactly that.

set -eu

devflow="${1:?usage: smoke-test.sh DEVFLOW_BINARY [EXPECTED_VERSION]}"
expected="${2:-}"
case "$devflow" in
    /*) ;;
    *) devflow="$PWD/$devflow" ;;
esac

fail() {
    printf 'smoke test: %s\n' "$1" >&2
    exit 1
}

scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT INT TERM

DEVFLOW_CONFIG_DIR="$scratch/config"
DEVFLOW_DATA_DIR="$scratch/data"
export DEVFLOW_CONFIG_DIR DEVFLOW_DATA_DIR
unset DEVFLOW_TRUST_HOOKS DEVFLOW_INHERIT DEVFLOW_SETUP_ACTIVE

installed="$("$devflow" --version | awk '{print $2}')"
if [ -n "$expected" ] && [ "$installed" != "${expected#v}" ]; then
    fail "installed version '$installed' does not match '${expected#v}'"
fi
printf 'devflow %s\n' "$installed"

repo="$scratch/repo"
mkdir "$repo"
cd "$repo"
git init -q -b main
git -c user.name=smoke -c user.email=smoke@example.invalid commit -q --allow-empty -m init

"$devflow" init --yes --postgres none
"$devflow" switch -c smoke
"$devflow" list | grep -F smoke >/dev/null || fail "'devflow list' does not show the workspace"
"$devflow" remove smoke
if "$devflow" list | grep -F smoke >/dev/null; then
    fail "'devflow list' still shows the removed workspace"
fi
printf 'smoke test passed\n'
