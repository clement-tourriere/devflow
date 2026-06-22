#!/usr/bin/env bash
#
# auto-merge-prs.sh — periodic, safety-gated PR auto-merger.
#
# Wakes up every INTERVAL_SECONDS (default 600 = 10 min), lists open PRs via the
# GitHub CLI, and for each one runs the pipeline:
#
#       rebase  ->  full review  ->  supply-chain safety check  ->  merge
#
# A PR is ONLY merged when ALL of the mandatory gates pass:
#
#   1. Author is trusted          (TRUSTED_AUTHORS, default: dependabot)
#   2. Branch is mergeable & up to date with the base branch
#                                 (stale/conflicting PRs get a rebase request
#                                  and are retried on the next cycle)
#   3. CI is fully green          (every required check SUCCESS/SKIPPED,
#                                  zero failures, zero still-pending)
#   4. Supply-chain scan is clean (only dependency-manifest files touched for
#                                  bot PRs; no git/path/insecure/registry-
#                                  override sources injected into the lockfile)
#   5. Code review approves       (optional external/AI reviewer hook;
#                                  see REVIEWER_CMD / STRICT_REVIEW)
#
# Anything that fails a gate is skipped (never merged) and reported, then the
# loop continues to the next PR — and to the next cycle.
#
# Usage:
#   scripts/auto-merge-prs.sh [--once] [--dry-run] [--json]
#                             [--repo OWNER/NAME] [--interval SECONDS]
#                             [--method rebase|squash|merge]
#
# Common env knobs (all have flag equivalents where it matters):
#   REPO                 owner/name              (default: auto-detected)
#   INTERVAL_SECONDS     seconds between cycles  (default: 600)
#   BASE_BRANCH          base branch to target   (default: repo default branch)
#   MERGE_METHOD         rebase|squash|merge     (default: rebase)
#   TRUSTED_AUTHORS      comma/space list        (default: dependabot)
#   DEP_PATHS_REGEX      "dependency-only" paths (default: Cargo manifests)
#   REQUIRE_CI           true|false              (default: true)
#   STRICT_REVIEW        require an AI/ext review (default: false)
#   REVIEWER             auto|pi|claude|custom|none  (default: auto -> pi)
#   REVIEW_MODEL         model for the pi reviewer    (optional, e.g. anthropic/*sonnet*)
#   REVIEWER_CMD         external review command (reads context on stdin)
#   DRY_RUN              true|false              (default: false)
#   RUN_ONCE             true|false              (default: false)
#   AUDIT_LOG            path to JSONL audit log (default: .devflow-automerge.jsonl)
#
set -uo pipefail

# ----------------------------------------------------------------------------
# Configuration / defaults
# ----------------------------------------------------------------------------
REPO="${REPO:-}"
INTERVAL_SECONDS="${INTERVAL_SECONDS:-600}"
BASE_BRANCH="${BASE_BRANCH:-}"
MERGE_METHOD="${MERGE_METHOD:-rebase}"
TRUSTED_AUTHORS="${TRUSTED_AUTHORS:-dependabot}"
DEP_PATHS_REGEX="${DEP_PATHS_REGEX:-^([^/]*/)*Cargo\.(toml|lock)$}"
REQUIRE_CI="${REQUIRE_CI:-true}"
STRICT_REVIEW="${STRICT_REVIEW:-false}"
REVIEWER="${REVIEWER:-auto}"
REVIEW_MODEL="${REVIEW_MODEL:-}"
REVIEWER_CMD="${REVIEWER_CMD:-}"
DRY_RUN="${DRY_RUN:-false}"
RUN_ONCE="${RUN_ONCE:-false}"
JSON_OUT="${JSON_OUT:-false}"
AUDIT_LOG="${AUDIT_LOG:-.devflow-automerge.jsonl}"

# ----------------------------------------------------------------------------
# Arg parsing
# ----------------------------------------------------------------------------
while [[ $# -gt 0 ]]; do
  case "$1" in
    --once)      RUN_ONCE=true ;;
    --dry-run)   DRY_RUN=true ;;
    --json)      JSON_OUT=true ;;
    --strict)    STRICT_REVIEW=true ;;
    --repo)      REPO="$2"; shift ;;
    --interval)  INTERVAL_SECONDS="$2"; shift ;;
    --method)    MERGE_METHOD="$2"; shift ;;
    --base)      BASE_BRANCH="$2"; shift ;;
    -h|--help)
      sed -n '2,60p' "$0" | sed 's/^# \{0,1\}//'
      exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

# ----------------------------------------------------------------------------
# Logging helpers
# ----------------------------------------------------------------------------
c_reset=$'\033[0m'; c_dim=$'\033[2m'; c_red=$'\033[31m'
c_grn=$'\033[32m'; c_ylw=$'\033[33m'; c_blu=$'\033[34m'; c_bold=$'\033[1m'
if [[ ! -t 1 || "$JSON_OUT" == true ]]; then
  c_reset=; c_dim=; c_red=; c_grn=; c_ylw=; c_blu=; c_bold=
fi

ts() { date -u +%Y-%m-%dT%H:%M:%SZ; }

log()  { [[ "$JSON_OUT" == true ]] && return 0; printf '%s %s\n' "${c_dim}[$(ts)]${c_reset}" "$*"; }
ok()   { [[ "$JSON_OUT" == true ]] && return 0; printf '%s %s%s%s\n' "${c_dim}[$(ts)]${c_reset}" "$c_grn" "$*" "$c_reset"; }
warn() { [[ "$JSON_OUT" == true ]] && return 0; printf '%s %s%s%s\n' "${c_dim}[$(ts)]${c_reset}" "$c_ylw" "$*" "$c_reset"; }
err()  { printf '%s %s%s%s\n' "${c_dim}[$(ts)]${c_reset}" "$c_red" "$*" "$c_reset" >&2; }
hdr()  { [[ "$JSON_OUT" == true ]] && return 0; printf '\n%s%s%s\n' "$c_bold$c_blu" "$*" "$c_reset"; }

# Append a structured record to the JSONL audit log and, in --json mode, stdout.
audit() {
  # audit <pr> <decision> <reason>
  local rec
  rec=$(jq -nc \
        --arg ts "$(ts)" --arg repo "$REPO" \
        --argjson pr "${1:-null}" --arg decision "$2" --arg reason "$3" \
        '{ts:$ts, repo:$repo, pr:$pr, decision:$decision, reason:$reason}')
  printf '%s\n' "$rec" >>"$AUDIT_LOG"
  [[ "$JSON_OUT" == true ]] && printf '%s\n' "$rec"
  return 0
}

die() { err "$*"; exit 1; }

# ----------------------------------------------------------------------------
# Pre-flight
# ----------------------------------------------------------------------------
command -v gh  >/dev/null || die "gh CLI not found"
command -v jq  >/dev/null || die "jq not found"
gh auth status >/dev/null 2>&1 || die "gh not authenticated (run: gh auth login)"

if [[ -z "$REPO" ]]; then
  REPO=$(gh repo view --json nameWithOwner --jq .nameWithOwner 2>/dev/null) \
    || die "could not auto-detect repo; pass --repo OWNER/NAME"
fi
if [[ -z "$BASE_BRANCH" ]]; then
  BASE_BRANCH=$(gh repo view "$REPO" --json defaultBranchRef --jq .defaultBranchRef.name 2>/dev/null || echo main)
fi

case "$MERGE_METHOD" in rebase|squash|merge) ;; *) die "invalid --method: $MERGE_METHOD" ;; esac

# ----------------------------------------------------------------------------
# Gate: trusted author
# ----------------------------------------------------------------------------
# True if any AI/external reviewer is usable (respects REVIEWER selection).
reviewer_available() {
  case "$REVIEWER" in
    none)   return 1 ;;
    pi)     command -v pi >/dev/null 2>&1 ;;
    claude) command -v claude >/dev/null 2>&1 ;;
    custom) [[ -n "$REVIEWER_CMD" ]] ;;
    auto|*) [[ -n "$REVIEWER_CMD" ]] || command -v pi >/dev/null 2>&1 || command -v claude >/dev/null 2>&1 ;;
  esac
}

is_trusted_author() {
  # $1 = author login (e.g. dependabot[bot], app/dependabot)
  local login="${1,,}" a
  IFS=', ' read -r -a _trusted <<<"${TRUSTED_AUTHORS,,}"
  for a in "${_trusted[@]}"; do
    [[ -z "$a" ]] && continue
    [[ "$login" == *"$a"* ]] && return 0
  done
  return 1
}

# ----------------------------------------------------------------------------
# Gate: CI status -> echoes PASS | PENDING | FAIL | NONE
# ----------------------------------------------------------------------------
ci_status() {
  local n="$1" rollup
  rollup=$(gh pr view "$n" --repo "$REPO" --json statusCheckRollup --jq '.statusCheckRollup' 2>/dev/null)
  [[ -z "$rollup" || "$rollup" == "null" || "$rollup" == "[]" ]] && { echo NONE; return; }
  printf '%s' "$rollup" | jq -r '
    def fail: (
      (.conclusion // "" | ascii_upcase) as $c |
      (.state // "" | ascii_upcase)      as $s |
      ($c | IN("FAILURE","CANCELLED","TIMED_OUT","ACTION_REQUIRED","STARTUP_FAILURE","STALE")) or
      ($s | IN("FAILURE","ERROR"))
    );
    def pending: (
      ((.status // "" | ascii_upcase) | IN("QUEUED","IN_PROGRESS","PENDING","WAITING","REQUESTED")) or
      ((.state  // "" | ascii_upcase) | IN("PENDING","EXPECTED"))
    );
    reduce .[] as $c ({fail:0,pending:0};
      if ($c|fail) then .fail+=1
      elif ($c|pending) then .pending+=1
      else . end)
    | if .fail>0 then "FAIL" elif .pending>0 then "PENDING" else "PASS" end'
}

# ----------------------------------------------------------------------------
# Gate: supply-chain safety scan.
# Returns 0 (clean) or 1 (blocked); prints reasons to stderr-of-caller via echo.
# ----------------------------------------------------------------------------
supply_chain_scan() {
  local n="$1" author="$2" files_json="$3"
  local reasons=() diff added

  # (a) For trusted bot dependency PRs, every touched file must be a manifest.
  if is_trusted_author "$author"; then
    local bad
    bad=$(printf '%s' "$files_json" | jq -r --arg re "$DEP_PATHS_REGEX" \
            '[.[] | select((test($re)) | not)] | join(", ")')
    [[ -n "$bad" ]] && reasons+=("bot PR touches non-manifest files: $bad")
  fi

  # (b) Inspect added lines of the diff for dependency-source tampering.
  diff=$(gh pr diff "$n" --repo "$REPO" 2>/dev/null)
  added=$(printf '%s\n' "$diff" | grep -E '^\+' | grep -v '^\+\+\+')

  # Any *added* lockfile source that is not the canonical crates.io registry.
  local bad_src
  bad_src=$(printf '%s\n' "$added" \
            | grep -E 'source[[:space:]]*=' \
            | grep -v 'registry\+https://github.com/rust-lang/crates.io-index' || true)
  [[ -n "$bad_src" ]] && reasons+=("non-crates.io source introduced: $(printf '%s' "$bad_src" | tr '\n' ';')")

  # git+ / local path / insecure http / registry override / patch tables.
  printf '%s\n' "$added" | grep -Eq '"git\+'              && reasons+=("git+ dependency source introduced")
  printf '%s\n' "$added" | grep -Eq '^\+[[:space:]]*path[[:space:]]*=' && reasons+=("local path dependency introduced")
  printf '%s\n' "$added" | grep -Eq 'http://'             && reasons+=("insecure http:// reference introduced")
  printf '%s\n' "$added" | grep -Eq 'replace-with|^\+[[:space:]]*\[source\.' && reasons+=("cargo source replacement introduced")
  printf '%s\n' "$added" | grep -Eq '^\+[[:space:]]*\[\[?patch' && reasons+=("[patch]/[[patch]] table introduced")

  # Touching cargo registry config is a strong red flag.
  printf '%s' "$files_json" | jq -e 'any(.[]; test("(^|/)\\.cargo/config(\\.toml)?$"))' >/dev/null \
    && reasons+=(".cargo/config(.toml) modified (registry/source config)")

  if ((${#reasons[@]})); then
    printf '%s\n' "${reasons[@]}"
    return 1
  fi
  return 0
}

# ----------------------------------------------------------------------------
# Full code review by an AI/external reviewer.
# Preference order (REVIEWER=auto): pi -> claude -> custom. `pi` is the agent
# this script ships with and is used headless & read-only (--no-tools).
# Returns 0 = approve, 1 = block, 2 = unavailable.
# ----------------------------------------------------------------------------
review_pr() {
  local n="$1" title="$2" author="$3" files_json="$4"
  local context verdict used=""

  context=$(
    printf 'Repository: %s\nPR #%s: %s\nAuthor: %s\nChanged files: %s\n\nUnified diff:\n' \
      "$REPO" "$n" "$title" "$author" "$(printf '%s' "$files_json" | jq -r 'join(", ")')"
    gh pr diff "$n" --repo "$REPO" 2>/dev/null | head -c 60000
  )

  local prompt='You are a careful security reviewer for a pull request. Decide if it is SAFE to merge. Hunt for supply-chain attacks: malicious/typosquatted dependencies, unexpected source/registry/git/path changes, obfuscated or exfiltrating code, build-script (build.rs) or CI/workflow tampering, postinstall hooks, secret/network access. If anything is suspicious, BLOCK. Respond with EXACTLY one line and nothing else: "VERDICT: APPROVE" or "VERDICT: BLOCK <short reason>".'
  local payload; payload=$(printf '%s\n\n%s' "$prompt" "$context")

  _try_pi() {
    command -v pi >/dev/null 2>&1 || return 2
    local args=(-p --no-tools --no-session --no-context-files)
    [[ -n "$REVIEW_MODEL" ]] && args+=(--model "$REVIEW_MODEL")
    local out; out=$(printf '%s' "$payload" | pi "${args[@]}" 2>/dev/null)
    [[ -z "$out" ]] && return 2
    used="pi"; verdict="$out"; return 0
  }
  _try_claude() {
    command -v claude >/dev/null 2>&1 || return 2
    local out; out=$(printf '%s' "$payload" | claude -p 2>&1)
    printf '%s' "$out" | grep -qi 'not logged in\|/login\|invalid api key' && return 2
    used="claude"; verdict="$out"; return 0
  }
  _try_custom() {
    [[ -n "$REVIEWER_CMD" ]] || return 2
    local out; out=$(printf '%s' "$payload" | eval "$REVIEWER_CMD" 2>/dev/null)
    [[ -z "$out" ]] && return 2
    used="custom"; verdict="$out"; return 0
  }

  case "$REVIEWER" in
    none)   return 2 ;;
    pi)     _try_pi     || return 2 ;;
    claude) _try_claude || return 2 ;;
    custom) _try_custom || return 2 ;;
    auto|*) _try_custom || _try_pi || _try_claude || return 2 ;;
  esac

  log "  reviewer ($used): $(printf '%s' "$verdict" | grep -i 'VERDICT' | head -1)"
  printf '%s' "$verdict" | grep -qi 'VERDICT:[[:space:]]*APPROVE' && return 0
  return 1
}

# ----------------------------------------------------------------------------
# Rebase / bring branch up to date with base.
# ----------------------------------------------------------------------------
request_rebase() {
  local n="$1" author="$2"
  if is_trusted_author "$author" && [[ "${author,,}" == *dependabot* ]]; then
    if [[ "$DRY_RUN" == true ]]; then
      log "  would comment '@dependabot rebase' on #$n"
    else
      gh pr comment "$n" --repo "$REPO" --body "@dependabot rebase" >/dev/null 2>&1 \
        && log "  requested rebase via '@dependabot rebase'"
    fi
  else
    if [[ "$DRY_RUN" == true ]]; then
      log "  would call update-branch API on #$n"
    else
      gh api -X PUT "repos/$REPO/pulls/$n/update-branch" >/dev/null 2>&1 \
        && log "  updated branch with base via API" \
        || warn "  could not update branch (may need a manual rebase)"
    fi
  fi
}

# ----------------------------------------------------------------------------
# Process a single PR through every gate, merging only if all pass.
# ----------------------------------------------------------------------------
process_pr() {
  local n="$1"
  local meta title author is_bot mergeable mstate files
  meta=$(gh pr view "$n" --repo "$REPO" \
          --json number,title,author,mergeable,mergeStateStatus,isDraft,baseRefName,files 2>/dev/null) \
    || { warn "#$n: could not load metadata"; return; }

  title=$(jq -r '.title'              <<<"$meta")
  author=$(jq -r '.author.login'      <<<"$meta")
  is_bot=$(jq -r '.author.is_bot'     <<<"$meta")
  mergeable=$(jq -r '.mergeable'      <<<"$meta")
  mstate=$(jq -r '.mergeStateStatus'  <<<"$meta")
  local draft base
  draft=$(jq -r '.isDraft'            <<<"$meta")
  base=$(jq -r '.baseRefName'         <<<"$meta")
  files=$(jq -c '[.files[].path]'     <<<"$meta")

  hdr "PR #$n — $title"
  log "  author=$author (bot=$is_bot) base=$base mergeable=$mergeable state=$mstate"

  # Gate 0: draft / wrong base
  if [[ "$draft" == true ]]; then warn "  SKIP: draft"; audit "$n" skip "draft"; return; fi
  if [[ "$base" != "$BASE_BRANCH" ]]; then warn "  SKIP: base is $base (want $BASE_BRANCH)"; audit "$n" skip "wrong-base"; return; fi

  # Gate 1: trusted author
  if ! is_trusted_author "$author"; then
    if ! reviewer_available; then
      warn "  SKIP: untrusted author '$author' and no reviewer available"
      audit "$n" skip "untrusted-author-no-reviewer"; return
    fi
    warn "  author '$author' not in trusted set — requiring code review to approve"
  fi

  # Gate 4 (cheap, do early): supply-chain safety scan
  local sc_reasons
  if ! sc_reasons=$(supply_chain_scan "$n" "$author" "$files"); then
    err "  BLOCK: supply-chain scan flagged this PR:"
    printf '         - %s\n' $(printf '%s\n' "$sc_reasons") >&2
    audit "$n" block "supply-chain: $(printf '%s; ' $sc_reasons)"
    return
  fi
  ok  "  supply-chain scan: clean (sources on crates.io, no source/patch tampering)"

  # Gate 2: mergeable & up to date
  case "$mstate" in
    CLEAN|HAS_HOOKS|UNSTABLE) : ;;                  # CI handled by gate 3
    BEHIND)   warn "  branch behind $BASE_BRANCH — requesting rebase, will retry next cycle"
              request_rebase "$n" "$author"; audit "$n" defer "behind-rebasing"; return ;;
    DIRTY)    warn "  merge conflict — requesting rebase, will retry next cycle"
              request_rebase "$n" "$author"; audit "$n" defer "conflict-rebasing"; return ;;
    BLOCKED)  warn "  SKIP: blocked by branch protection / required review"
              audit "$n" skip "blocked"; return ;;
    UNKNOWN)  log "  mergeability unknown — waiting briefly"; sleep 5
              mstate=$(gh pr view "$n" --repo "$REPO" --json mergeStateStatus --jq .mergeStateStatus)
              [[ "$mstate" == BEHIND || "$mstate" == DIRTY ]] && { request_rebase "$n" "$author"; audit "$n" defer "rebasing"; return; } ;;
    *)        warn "  unexpected merge state '$mstate' — skipping"; audit "$n" skip "state-$mstate"; return ;;
  esac
  if [[ "$mergeable" == CONFLICTING ]]; then
    warn "  conflicting — requesting rebase, will retry next cycle"
    request_rebase "$n" "$author"; audit "$n" defer "conflict-rebasing"; return
  fi

  # Gate 3: CI fully green
  if [[ "$REQUIRE_CI" == true ]]; then
    local ci; ci=$(ci_status "$n")
    case "$ci" in
      PASS)    ok  "  CI: all checks green" ;;
      PENDING) warn "  CI still running — will retry next cycle"; audit "$n" defer "ci-pending"; return ;;
      FAIL)    err "  SKIP: CI is failing — refusing to merge"; audit "$n" skip "ci-failing"; return ;;
      NONE)    warn "  no CI checks found"; audit "$n" skip "no-ci"; return ;;
    esac
  fi

  # Gate 5: full code review (optional / graceful)
  review_pr "$n" "$title" "$author" "$files"; local rv=$?
  case "$rv" in
    0) ok  "  review: APPROVED" ;;
    1) err "  BLOCK: reviewer rejected this PR"; audit "$n" block "review-rejected"; return ;;
    2) warn "  review: no AI/external reviewer produced a verdict"
       if [[ "$STRICT_REVIEW" == true ]] || ! is_trusted_author "$author"; then
         err "  SKIP: no reviewer available and review is required"; audit "$n" skip "review-unavailable"; return
       fi
       warn "  review: no AI/external reviewer available — relying on deterministic gates" ;;
  esac

  # All gates passed -> merge
  if [[ "$DRY_RUN" == true ]]; then
    ok  "  DRY-RUN: would merge #$n via $MERGE_METHOD"
    audit "$n" dry-run "would-merge-$MERGE_METHOD"; return
  fi

  log "  merging via $MERGE_METHOD ..."
  if gh pr merge "$n" --repo "$REPO" "--$MERGE_METHOD" --delete-branch >/dev/null 2>&1; then
    ok  "  MERGED #$n ✅"
    audit "$n" merged "$MERGE_METHOD"
    MERGED_THIS_CYCLE=$((MERGED_THIS_CYCLE + 1))
    sleep 3   # let GitHub recompute mergeability of the remaining PRs
  else
    local mout; mout=$(gh pr merge "$n" --repo "$REPO" "--$MERGE_METHOD" --delete-branch 2>&1 || true)
    err "  merge failed: $(printf '%s' "$mout" | head -1)"
    audit "$n" error "merge-failed: $(printf '%s' "$mout" | head -1)"
  fi
}

# ----------------------------------------------------------------------------
# One full cycle over all open PRs.
# ----------------------------------------------------------------------------
run_cycle() {
  MERGED_THIS_CYCLE=0
  local prs
  prs=$(gh pr list --repo "$REPO" --state open --base "$BASE_BRANCH" \
         --json number --jq 'sort_by(.number) | .[].number' 2>/dev/null)

  if [[ -z "$prs" ]]; then
    log "no open PRs targeting $BASE_BRANCH"
    audit null cycle "no-open-prs"
    return
  fi

  local count; count=$(wc -w <<<"$prs")
  hdr "Cycle: $count open PR(s) on $REPO -> $BASE_BRANCH"
  local n
  for n in $prs; do
    process_pr "$n" || warn "#$n: processing error (continuing)"
  done
  ok "Cycle complete — merged $MERGED_THIS_CYCLE PR(s) this pass"
  audit null cycle "merged=$MERGED_THIS_CYCLE"
}

# ----------------------------------------------------------------------------
# Main loop
# ----------------------------------------------------------------------------
log "${c_bold}devflow auto-merge${c_reset} repo=$REPO base=$BASE_BRANCH method=$MERGE_METHOD"
log "interval=${INTERVAL_SECONDS}s dry_run=$DRY_RUN strict_review=$STRICT_REVIEW trusted=[$TRUSTED_AUTHORS]"
log "reviewer=$REVIEWER available=$(reviewer_available && echo yes || echo no)${REVIEW_MODEL:+ model=$REVIEW_MODEL}"
trap 'log "received signal — exiting"; exit 0' INT TERM

while :; do
  run_cycle
  if [[ "$RUN_ONCE" == true ]]; then
    log "RUN_ONCE set — exiting after one cycle"
    break
  fi
  log "sleeping ${INTERVAL_SECONDS}s until next cycle ..."
  sleep "$INTERVAL_SECONDS"
done
