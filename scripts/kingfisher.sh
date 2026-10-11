#!/bin/sh
# Secret scanning with kingfisher, for the hk hooks in hk.pkl and for CI. It
# never contacts a provider and lifts kingfisher's size, archive and time
# limits (a watchdog fails the scan after KINGFISHER_TIMEOUT seconds, default
# 600). It prints only the rule, location and commit of each finding, with
# every detected secret masked out of the locations.
#
#   kingfisher.sh tree         the working tree as `git add -A` would stage it
#   kingfisher.sh staged       the changed files in the index, as Git stores
#                              them, and their names: what the next commit records
#   kingfisher.sh message FILE a commit message, without the diff that
#                              `git commit -v` appends below the scissors line
#   kingfisher.sh pushed       everything a push publishes: the contents, names,
#                              messages and headers of every commit, and every
#                              annotated tag. Reads Git's pre-push lines on stdin:
#                              <local ref> <local sha> <remote ref> <remote sha>
#
# Kingfisher's own errors can quote a path verbatim, so they are shown only
# with KINGFISHER_DEBUG=1. Mark an intentional fake credential with a
# `kingfisher:ignore` comment on its line or the line above.
set -eu
umask 077
# Replacement refs would show Git commands other objects than the ones pushed.
GIT_NO_REPLACE_OBJECTS=1
export GIT_NO_REPLACE_OBJECTS

# The whole history up to this commit (main on 2026-10-10) has been scanned;
# its only findings are fake credentials in 0.x unit tests. Every later commit
# is scanned, whichever remote already has it.
audited=706ef67a4e2f8965963f44edd178b92d0762aa45

for tool in kingfisher jq; do
	command -v "$tool" >/dev/null || {
		echo "kingfisher.sh: $tool is not installed (mise install)" >&2
		exit 1
	}
done

tmp=$(mktemp -d)
mkdir "$tmp/text"
scanner=
watchdog=
# shellcheck disable=SC2329 # run by the EXIT trap
cleanup() {
	for pid in $scanner $watchdog; do
		kill "$pid" 2>/dev/null || true
		wait "$pid" 2>/dev/null || true
	done
	rm -rf "$tmp"
}
trap 'cleanup' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
status=0
scans=0
show_commits=true

# scan ARGS...: run `kingfisher scan ARGS...` under the watchdog. Its report is
# kept for `report`; fail if it found anything or could not finish.
scan() {
	scans=$((scans + 1))
	out=$tmp/scan-$scans
	kingfisher scan "$@" --no-validate --no-update-check --quiet --no-limits \
		--format json --output "$out.json" >/dev/null 2>"$out.stderr" &
	scanner=$!
	(
		trap 'kill "$sleeper" 2>/dev/null; exit 0' TERM
		sleep "${KINGFISHER_TIMEOUT:-600}" &
		sleeper=$!
		wait "$sleeper" && : >"$out.timeout" && kill "$scanner" 2>/dev/null
	) &
	watchdog=$!
	rc=0
	wait "$scanner" || rc=$?
	scanner=
	kill "$watchdog" 2>/dev/null || true
	wait "$watchdog" 2>/dev/null || true
	watchdog=
	case $rc in
	0) return 0 ;;
	200 | 205) mv "$out.json" "$tmp/findings-$scans.json" ;;
	*)
		if [ "${KINGFISHER_DEBUG-}" = 1 ]; then cat "$out.stderr" >&2; fi
		if [ -e "$out.timeout" ]; then
			echo "kingfisher.sh: the scan did not finish in ${KINGFISHER_TIMEOUT:-600}s (KINGFISHER_TIMEOUT)" >&2
		else
			echo "kingfisher.sh: kingfisher failed (exit $rc); KINGFISHER_DEBUG=1 shows its errors" >&2
		fi
		;;
	esac
	return 1
}

# Scan the text files gathered in $tmp/text (names, messages, headers). No
# subshell: it would reset the traps that stop the scanner.
scan_text() {
	here=$(pwd)
	cd "$tmp/text"
	scan . || status=1
	cd "$here"
}

# Print every finding of every scan. The reports hold the secrets, in this
# private directory; a location can contain one too (a file named after a
# token), so every detected secret is masked out of every location.
report() {
	set -- "$tmp"/findings-*.json
	[ -e "$1" ] || return 0
	jq -rs --argjson commits "$show_commits" '
		[.[].findings[]] as $all
		| [$all[].finding.snippet | select(length > 0)] as $secrets
		| $all[]
		| (reduce $secrets[] as $s (.finding.path | ltrimstr("./"); split($s) | join("***"))) as $path
		| "secret: \(.rule.id) in \($path):\(.finding.line)"
			+ (.finding.git_metadata.commit.id // "" | if $commits and . != "" then " (commit \(.[0:12]))" else "" end)' \
		"$@" >&2
}

# commit_tree [PARENT]: a commit of the tree in $tmp/index, for kingfisher to
# read as Git stores it (no checkout filters, any file name, binaries).
commit_tree() {
	tree=$(GIT_INDEX_FILE=$tmp/index git write-tree 2>/dev/null) || {
		echo "kingfisher.sh: the index has unmerged entries" >&2
		exit 1
	}
	set -- -m "kingfisher.sh" ${1:+-p "$1"} "$tree"
	GIT_AUTHOR_NAME=kingfisher.sh GIT_AUTHOR_EMAIL=kingfisher.sh@localhost \
		GIT_COMMITTER_NAME=kingfisher.sh GIT_COMMITTER_EMAIL=kingfisher.sh@localhost \
		git commit-tree --no-gpg-sign "$@"
}

copy_index() {
	index=$(git rev-parse --git-path index)
	if [ -f "$index" ]; then cp "$index" "$tmp/index"; fi
}

working_tree() {
	copy_index
	GIT_INDEX_FILE=$tmp/index git add -A
	commit=$(commit_tree)
	show_commits=false
	git -c core.quotePath=false ls-tree -r --name-only "$commit" >"$tmp/text/file-names"
	scan "$(git rev-parse --git-common-dir)" --branch "$commit" || status=1
	scan_text
}

# A copy of the index becomes a commit on top of HEAD, so kingfisher scans only
# the blobs that change, and the real index stays free for hk.
staged() {
	copy_index
	head=$(git rev-parse -q --verify 'HEAD^{commit}') || head=
	commit=$(commit_tree "$head")
	show_commits=false
	if [ -n "$head" ]; then
		git -c core.quotePath=false diff-tree -r --name-only --no-renames "$head" "$commit" >"$tmp/text/file-names"
		scan "$(git rev-parse --git-common-dir)" --branch "$commit" --since-commit "$head" || status=1
	else
		git -c core.quotePath=false ls-tree -r --name-only "$commit" >"$tmp/text/file-names"
		scan "$(git rev-parse --git-common-dir)" --branch "$commit" || status=1
	fi
	scan_text
}

message() {
	sed '/^. -\{24\} >8 -\{24\}$/,$d' "$1" >"$tmp/text/commit-message"
	scan_text
}

pushed() {
	if [ "$(git rev-parse --is-shallow-repository)" != false ]; then
		echo "kingfisher.sh: a shallow clone hides history it would publish; run git fetch --unshallow" >&2
		exit 1
	fi
	git_dir=$(git rev-parse --git-common-dir)
	since=
	if git cat-file -e "$audited^{commit}" 2>/dev/null; then since=$audited; fi
	while read -r _ object _ _; do
		case $object in *[!0]*) ;; *) continue ;; esac # a deletion publishes nothing
		while [ "$(git cat-file -t "$object")" = tag ]; do
			git cat-file tag "$object" >"$tmp/text/tag-$object"
			object=$(git cat-file tag "$object" | sed -n '1s/^object //p')
		done
		case $(git cat-file -t "$object") in
		commit)
			set -- "$object"
			if [ -n "$since" ]; then set -- "$@" --not "$since"; fi
			git rev-list "$@" | git cat-file --batch >"$tmp/text/commits-$object"
			git -c core.quotePath=false log --format= --name-only --no-renames \
				--diff-merges=first-parent "$@" >"$tmp/text/file-names-$object"
			set -- "$git_dir" --branch "$object"
			if [ -n "$since" ]; then set -- "$@" --since-commit "$since"; fi
			scan "$@" || status=1
			;;
		blob) git cat-file blob "$object" >"$tmp/text/tag-target-$object" ;;
		*)
			echo "kingfisher.sh: cannot scan a pushed $(git cat-file -t "$object") ($object)" >&2
			status=1
			;;
		esac
	done
	scan_text
}

case ${1-} in
tree) working_tree ;;
staged) staged ;;
message) message "$2" ;;
pushed) pushed ;;
*)
	echo "usage: kingfisher.sh tree | staged | message FILE | pushed" >&2
	exit 2
	;;
esac

report
exit "$status"
