#!/bin/sh
# Manual benchmark: N workspaces end to end on a scratch project with a managed
# PostgreSQL and a Valkey service (and MySQL with --mysql).
#
# It measures, then prints a table and a JSON document:
#   - the cold `switch -c` (starts the project's server and the root branches)
#     and each warm one after it, in milliseconds;
#   - the memory of every container the project owns (`docker stats`);
#   - the growth of the Docker VM's disk (a helper container's `df /`, which
#     counts blocks, so shared blocks are not counted twice) and of the logical
#     volume size (`docker system df`);
#   - with --mysql, the downtime a MySQL clone causes its source.
#
# Needs docker, git, jq and perl. It uses only images the suites already use
# (postgres:18, valkey/valkey:8-alpine, mysql:8.4, debian:stable-slim) and pulls
# nothing: a missing image is an error. Everything it creates lives in a
# temporary directory (the registry and data directories are moved there) and
# is removed on exit, containers and volumes included, found by the project id
# it generated. Other containers running on the Docker engine add noise to the
# disk and memory figures; run it on an idle engine.
#
# Usage: bench-workspaces.sh [--workspaces N] [--mysql] [--json FILE] [--keep]
#                            [--devflow PATH]
#   --workspaces N  workspaces to create after the project's first (default 5)
#   --mysql         add a mysql service (MySQL 8.4, preview)
#   --json FILE     also write the JSON document to FILE
#   --keep          keep the scratch project and its containers
#   --devflow PATH  the binary to measure (default: devflow on PATH)

set -eu

count=5
with_mysql=0
json_file=
keep=0
devflow=devflow

while [ $# -gt 0 ]; do
    case "$1" in
        --workspaces) count="${2:?--workspaces needs a number}"; shift 2 ;;
        --mysql) with_mysql=1; shift ;;
        --json) json_file="${2:?--json needs a file}"; shift 2 ;;
        --keep) keep=1; shift ;;
        --devflow) devflow="${2:?--devflow needs a path}"; shift 2 ;;
        -h | --help) sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) printf 'bench: unknown argument %s\n' "$1" >&2; exit 2 ;;
    esac
done

fail() {
    printf 'bench: %s\n' "$1" >&2
    exit 1
}

case "$count" in
    '' | *[!0-9]*) fail "--workspaces takes a positive number" ;;
esac
[ "$count" -ge 1 ] || fail "--workspaces takes a positive number"
for tool in docker git jq perl; do
    command -v "$tool" >/dev/null 2>&1 || fail "$tool is required"
done
command -v "$devflow" >/dev/null 2>&1 || [ -x "$devflow" ] || fail "devflow not found: $devflow"
# The script works in a temporary directory: keep these valid there.
case "$devflow" in
    */*) devflow="$(cd "$(dirname "$devflow")" && pwd)/$(basename "$devflow")" ;;
esac
case "$json_file" in
    '' | /*) ;;
    *) json_file="$PWD/$json_file" ;;
esac
docker info >/dev/null 2>&1 || fail "the Docker daemon is not reachable"
helper=debian:stable-slim
images="postgres:18 valkey/valkey:8-alpine $helper"
[ "$with_mysql" = 1 ] && images="$images mysql:8.4"
for image in $images; do
    docker image inspect "$image" >/dev/null 2>&1 || fail "image $image is not present (this script pulls nothing)"
done

scratch="$(mktemp -d)"
DEVFLOW_CONFIG_DIR="$scratch/config"
DEVFLOW_DATA_DIR="$scratch/data"
DEVFLOW_NON_INTERACTIVE=1
export DEVFLOW_CONFIG_DIR DEVFLOW_DATA_DIR DEVFLOW_NON_INTERACTIVE
unset DEVFLOW_TRUST_HOOKS DEVFLOW_INHERIT DEVFLOW_SETUP_ACTIVE
repo="$scratch/repo"
project_id=
compose_project=

# Removes what this run created: the workspaces, then the managed server, then
# any container or volume that carries the generated project id.
cleanup() {
    status=$?
    trap - EXIT INT TERM
    if [ "$keep" = 1 ]; then
        printf 'bench: kept %s (project %s)\n' "$scratch" "${project_id:-none}" >&2
        exit "$status"
    fi
    if [ -d "$repo" ]; then
        names="$("$devflow" -C "$repo" --json list 2>/dev/null | jq -r '.workspaces[]?.workspace | select(.is_default | not) | .name' 2>/dev/null || true)"
        for name in $names; do
            "$devflow" -C "$repo" remove "$name" --force >/dev/null 2>&1 || true
        done
    fi
    if [ -n "$project_id" ]; then
        compose="$DEVFLOW_DATA_DIR/postgres/$project_id/compose.yaml"
        if [ -f "$compose" ] && [ -n "$compose_project" ]; then
            docker compose -f "$compose" -p "$compose_project" down -v >/dev/null 2>&1 || true
        fi
        containers="$(docker ps -aq --filter "label=dev.devflow.project=$project_id")"
        # shellcheck disable=SC2086 # a list of ids
        [ -z "$containers" ] || docker rm -f $containers >/dev/null 2>&1 || true
        volumes="$(docker volume ls -q --filter "label=dev.devflow.project=$project_id")"
        # shellcheck disable=SC2086 # a list of names
        [ -z "$volumes" ] || docker volume rm $volumes >/dev/null 2>&1 || true
    fi
    rm -rf "$scratch"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

now_ms() {
    perl -MTime::HiRes=time -e 'printf "%d\n", time * 1000'
}

# Bytes of a size as Docker prints it: 512kB, 1.5GB, 12.3MiB, 0B.
to_bytes() {
    awk -v s="$1" 'BEGIN {
        n = s + 0
        u = s; sub(/^[0-9.]+/, "", u)
        m = 1
        if (u == "kB" || u == "KB") m = 1000
        else if (u == "MB") m = 1000 ^ 2
        else if (u == "GB") m = 1000 ^ 3
        else if (u == "TB") m = 1000 ^ 4
        else if (u == "KiB") m = 1024
        else if (u == "MiB") m = 1024 ^ 2
        else if (u == "GiB") m = 1024 ^ 3
        printf "%.0f\n", n * m
    }'
}

# Used bytes of the filesystem the Docker engine keeps its data on, as a helper
# container sees it (blocks, so reflinked data counts once).
vm_used_bytes() {
    kib="$(docker run --rm --pull=never "$helper" df -kP / | awk 'NR == 2 { print $3 }')"
    printf '%s\n' "$((kib * 1024))"
}

volumes_bytes() {
    size="$(docker system df --format '{{.Type}}|{{.Size}}' | awk -F'|' '$1 == "Local Volumes" { print $2 }')"
    to_bytes "${size:-0B}"
}

mkdir "$repo"
cd "$repo"
git init -q -b main
printf 'bench\n' > README
git add README
git -c user.name=bench -c user.email=bench@example.invalid commit -q -m init
"$devflow" init --yes --postgres managed >/dev/null 2>&1 || fail "devflow init failed"
{
    printf '\n[services.cache]\nengine = "valkey"\n'
    if [ "$with_mysql" = 1 ]; then
        printf '\n[services.sql]\nengine = "mysql"\n'
    fi
} >> devflow.toml
project_id="$(awk -F'"' '/^id = / { print $2; exit }' devflow.toml)"
project_name="$(awk -F'"' '/^name = / { print $2; exit }' devflow.toml)"
[ -n "$project_id" ] || fail "devflow.toml has no project id"
compose_project="devflow-${project_name}-$(printf '%s' "$project_id" | cut -c1-8)"

printf 'bench: project %s, %s workspaces after the first%s\n' "$project_id" "$count" \
    "$([ "$with_mysql" = 1 ] && printf ', with MySQL')" >&2
vm_before="$(vm_used_bytes)"
vol_before="$(volumes_bytes)"

rows="$scratch/rows.tsv"
: > "$rows"
i=1
while [ "$i" -le "$count" ]; do
    name="b$i"
    kind=warm
    [ "$i" = 1 ] && kind=cold
    out="$scratch/switch-$name.json"
    start="$(now_ms)"
    "$devflow" --json switch -c "$name" --non-interactive > "$out" 2> "$scratch/switch-$name.err" \
        || fail "switch -c $name failed: $(cat "$scratch/switch-$name.err")"
    end="$(now_ms)"
    methods="$(jq -r '[.receipt.services // {} | to_entries[] | select(.value.cloned) | "\(.key)=\(.value.cloned.method)"] | join(",")' "$out")"
    status="$(jq -r '.status' "$out")"
    printf '%s\t%s\t%s\t%s\t%s\n' "$name" "$kind" "$((end - start))" "$status" "${methods:--}" >> "$rows"
    printf 'bench: %s (%s) %s ms\n' "$name" "$kind" "$((end - start))" >&2
    i=$((i + 1))
done

vm_after="$(vm_used_bytes)"
vol_after="$(volumes_bytes)"

# Memory of every container of the project.
mem="$scratch/mem.tsv"
: > "$mem"
ids="$(docker ps -q --filter "label=dev.devflow.project=$project_id")"
if [ -n "$ids" ]; then
    # shellcheck disable=SC2086 # a list of ids
    docker stats --no-stream --format '{{.Name}}|{{.MemUsage}}' $ids | while IFS='|' read -r cname usage; do
        used="${usage%% /*}"
        printf '%s\t%s\n' "$cname" "$(to_bytes "$used")" >> "$mem"
    done
fi

# MySQL: the downtime each clone caused its source.
downtime="$scratch/downtime.tsv"
: > "$downtime"
if [ "$with_mysql" = 1 ]; then
    "$devflow" --json service sql status 2>/dev/null \
        | jq -r '.services[].branches[] | select(.source_downtime_ms != null) | "\(.name)\t\(.source_downtime_ms)"' \
        >> "$downtime" || true
fi

# The table.
printf '\n%-6s %-5s %10s  %-18s %s\n' WORKSPACE KIND 'TIME (ms)' STATUS CLONES
while IFS="$(printf '\t')" read -r name kind ms status methods; do
    printf '%-6s %-5s %10s  %-18s %s\n' "$name" "$kind" "$ms" "$status" "$methods"
done < "$rows"
printf '\n%-52s %12s\n' CONTAINER 'MEMORY (MiB)'
while IFS="$(printf '\t')" read -r cname bytes; do
    printf '%-52s %12s\n' "$cname" "$(awk -v b="$bytes" 'BEGIN { printf "%.1f", b / 1048576 }')"
done < "$mem"
printf '\n%-52s %12s\n' 'Docker VM disk growth (blocks, MiB)' "$(awk -v a="$vm_before" -v b="$vm_after" 'BEGIN { printf "%.1f", (b - a) / 1048576 }')"
printf '%-52s %12s\n' 'Volumes, logical size growth (MiB)' "$(awk -v a="$vol_before" -v b="$vol_after" 'BEGIN { printf "%.1f", (b - a) / 1048576 }')"
if [ "$with_mysql" = 1 ]; then
    printf '\n%-52s %12s\n' 'MYSQL SOURCE DOWNTIME, per clone' 'ms'
    while IFS="$(printf '\t')" read -r name ms; do
        printf '%-52s %12s\n' "$name" "$ms"
    done < "$downtime"
fi

# The JSON document.
document="$(jq -n \
    --arg project "$project_id" \
    --argjson workspaces "$count" \
    --argjson vm_before "$vm_before" --argjson vm_after "$vm_after" \
    --argjson vol_before "$vol_before" --argjson vol_after "$vol_after" \
    --rawfile rows "$rows" --rawfile mem "$mem" --rawfile downtime "$downtime" '
    def tsv: split("\n") | map(select(length > 0) | split("\t"));
    {
        project: $project,
        workspaces: $workspaces,
        switches: ($rows | tsv | map({
            workspace: .[0], kind: .[1], time_ms: (.[2] | tonumber), status: .[3], clones: .[4]
        })),
        containers: ($mem | tsv | map({ name: .[0], memory_bytes: (.[1] | tonumber) })),
        docker_vm_disk: { before_bytes: $vm_before, after_bytes: $vm_after, growth_bytes: ($vm_after - $vm_before) },
        volumes_logical: { before_bytes: $vol_before, after_bytes: $vol_after, growth_bytes: ($vol_after - $vol_before) },
        mysql_source_downtime_ms: ($downtime | tsv | map({ branch: .[0], downtime_ms: (.[1] | tonumber) }))
    }')"
printf '\n%s\n' "$document"
if [ -n "$json_file" ]; then
    printf '%s\n' "$document" > "$json_file"
fi
