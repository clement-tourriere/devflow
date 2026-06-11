# devflow — Remediation & Vision Plan (2026-06-11)

Source: `docs/full-review-2026-06-11.md` (176 findings, 56 confirmed). This plan turns the review into an ordered, implementable program. Each item cites the finding location; UNVERIFIED findings are re-verified against the code before fixing.

**Principles**
- Small, safe diffs; one phase = one reviewable unit; `cargo test --workspace` green after each phase.
- Never make a destructive operation *more* destructive while fixing it; thread `--force` flags instead of removing safety.
- New behavior gets a unit test alongside the existing test style (devflow-core has 168 tests to pattern-match).
- Object storage in the vision phase uses **RustFS** (Rust-native, S3-compatible, Apache-2.0) instead of MinIO.

---

## Phase A — Data-loss criticals (do first)

| # | Fix | Where |
|---|-----|-------|
| A1 | `remove_worktree`: check worktree `statuses()` for uncommitted/untracked changes before pruning; refuse unless `force`. Thread `force: bool` through `VcsProvider::remove_worktree` (git + jj impls), `workspace/delete.rs`, CLI `remove --force`, GUI/TUI callers. | `crates/devflow-core/src/vcs/git.rs:608` |
| A2 | Remove (or gate behind `force`) the `std::fs::remove_dir_all` fallback when VCS worktree removal fails. | `crates/devflow-core/src/workspace/delete.rs:65` |
| A3 | Fast-forward merge: replace `CheckoutBuilder::new().force()` with `.safe()`; pre-check `repo.statuses()` and bail with "commit/stash first" message. | `crates/devflow-core/src/vcs/git.rs:949` |
| A4 | Orphan GC: (a) `ensure_project` updates `project_path` whenever it differs from stored (not only when `None`), so moved projects aren't declared orphans; (b) `cleanup_orphan` only removes containers detection attributed to the orphan — drop the name-pattern re-discovery loop (or re-apply the live-project exclusion). | `crates/devflow-core/src/services/postgres/local/mod.rs:139`, `crates/devflow-core/src/services/orphan.rs:144,304` |

## Phase B — Hook chain (the agent/mise bug)

| # | Fix | Where |
|---|-----|-------|
| B1 | Track background-phase hook tasks (JoinSet in `HookEngine`); CLI paths await them (bounded timeout, default 60s, configurable) before exit. GUI unaffected. | `hooks/executor.rs:257,285` |
| B2 | Background hooks honor `working_dir` (currently ignored). | `hooks/executor.rs:278` |
| B3 | Approvals: key on (project, phase, hook name, **template** hash) instead of the rendered command, so once-approved hooks stay approved across workspaces. Keep rendered-command display in the prompt. | `hooks/executor.rs:247,495` |
| B4 | Non-interactive policy: `DEVFLOW_APPROVE_HOOKS=true` env + `--approve-hooks` flag auto-approve config-file hooks; without it, unapproved hooks are **skipped with a visible warning** in non-interactive mode instead of aborting the whole command. | `hooks/executor.rs:661` |
| B5 | `switch -c` / create: hook or service failures no longer suppress the JSON result — always emit `worktree_path` + per-hook/per-service failure summary; exit code signals partial failure. | `workspace/switch.rs:164,199` |
| B6 | mise-trust recipe: condition matches `mise.toml` OR `.mise.toml` (extend `file_exists:` to accept comma-separated alternatives, condition true if any exists). | `hooks/recipes.rs:284`, condition eval in `hooks/mod.rs` |
| B7 | Tool activation: when spawning shell hooks, prepend `~/.local/share/mise/shims` to PATH if it exists (cheap, no login shell needed); document `mise x --` wrapping for full parity. | `hooks/actions/shell.rs:79` |
| B8 | Hook context: pass the same (raw) workspace name on create and switch paths; normalized name available as `{{ workspace_sanitized }}`. | `workspace/switch.rs:65,199` |

## Phase C — Proxy resilience

| # | Fix | Where |
|---|-----|-------|
| C1 | Docker event monitor: outer retry loop with backoff; on reconnect, re-enumerate running containers and reconcile router + mDNS (diff add/remove). | `proxy/src/monitor.rs:84` |
| C2 | Deterministic port selection: sort exposed ports, prefer HTTP-ish ports (80, 8080, 3000, 8000, 5173, 8123, …), else lowest; warn when ambiguous. | `proxy/src/discovery.rs:289` |
| C3 | `run_proxy()` binds listeners eagerly and returns Err on bind failure; CLI/GUI report real status. | `proxy/src/lib.rs:247` |
| C4 | `die` event: remove route by container id directly from cached state — don't depend on post-die inspect. | `proxy/src/monitor.rs:67` |
| C5 | Leaf certs: add ExtendedKeyUsage serverAuth (Apple requirement). | `proxy/src/ca.rs:155` |
| C6 | Accept-loop resilience: log-and-continue on transient `accept()` errors instead of breaking. | `proxy/src/server.rs:50`, `api.rs:49` |

## Phase D — CoW correctness (Postgres first)

| # | Fix | Where |
|---|-----|-------|
| D1 | Clone source quiescing: **stop** (graceful, generous timeout) the parent container before cloning PGDATA; restart it afterward if it was running. Replaces pause-based cloning on the APFS/copy path (ZFS keeps snapshot path, gains stop too for belt-and-braces). | `postgres/local/mod.rs:256-281` |
| D2 | Reconciler: never unpause/start a parent while a clone is in flight (in-flight marker in SQLite or file lock). | `postgres/local/reconcile.rs:34` |
| D3 | ZFS `reset_workspace`: unique snapshot names (monotonic suffix) and never destroy the child dataset before the replacement clone succeeds. | `storage/zfs_driver.rs:183,235` |
| D4 | postgres:18+/latest: pass explicit `PGDATA=/var/lib/postgresql/data` env so the bind mount and the server agree across image versions. | `postgres/local/docker.rs:19` |
| D5 | Readiness: `pg_isready` over TCP (`-h 127.0.0.1`) inside the container so initdb's unix-socket temp server can't false-positive. | `postgres/local/docker.rs:390` |

## Phase E — GUI visibility (before beautification)

| # | Fix | Where |
|---|-----|-------|
| E1 | Replace all `window.alert()`/`confirm()` (27 sites) with a toast component + the existing ConfirmDialog (WKWebView no-ops today). | `ui/src/**` |
| E2 | Render `OrchestrationResult`/hook summaries from create/switch/delete instead of discarding them. | `ui/src/pages/projects/ProjectDetail.tsx:235` |
| E3 | GUI PATH bootstrap: capture login-shell PATH at startup (`$SHELL -lc 'echo $PATH'`) so Dock-launched hooks find mise/npm/docker. | `src-tauri/src/main.rs:59` |
| E4 | Fix unreachable onboarding route (`projects/*/setup` → proper v6 nesting). | `ui/src/App.tsx:100` |

Full UX redesign (Tailwind + shadcn/ui, workspace-first table, hooks log drawer) is a separate effort — see review report §GUI UX; not part of this remediation pass.

## Phase F — Identity unification (enabler, cross-cutting)

Canonical project identity = canonicalized **main repo root** (worktrees resolve to their parent repo), with `name:` override. One derivation function used by: local state keys, SQLite project rows, container labels (add `devflow.project_path`), approval keys, hook context. Migration: on load, merge per-worktree state silos into the canonical key. This collapses ~15 findings (state fragmentation, approval misses inside worktrees, orphan false-positives, container collisions).

## Phase G — Vision: global engines + logical isolation (with RustFS)

Target: one global, optimized container per engine (Postgres, Redis, **RustFS**, ClickHouse); per-project/workspace *logical* boundaries provisioned on the fly; lightweight `devflow.toml`; coexists with CoW `local` providers per service entry.

- **G0 (validate, zero core changes):** recipe `shared-db`: docker-exec hook running `CREATE DATABASE "{{ project|sanitize_db }}_{{ workspace|sanitize_db }}"` against a hand-started global postgres; write-env hook emits the URL.
- **G1 `SharedPostgresProvider`** (`type: shared`): `ensure_global_container()` (extracted from `generic/mod.rs` ensure_image/create_and_start/wait_healthy/exec_check, 409-tolerant), `create_workspace` → `CREATE DATABASE x [TEMPLATE parent]` (keeps branch-from-parent semantics in-engine), `delete_workspace` → terminate connections + `DROP DATABASE`, fixed well-known port, `global_engines` + `logical_resources` tables (BEGIN IMMEDIATE for cross-process safety), refcounted engine GC.
- **G2 `RustFSProvider`** (object storage): global `rustfs/rustfs` container (S3 API on 9000, console 9001, data under devflow data root; image/ports/credentials configurable — verify current image docs at implementation time), bucket-per-workspace `{project}-{workspace}` via the **rust-s3 crate already in-tree** (`postgres/local/seed.rs` shows client setup); connection info exposes endpoint/bucket/access keys to hooks (`{{ service['storage'].url }}`, `.bucket`); delete = remove bucket (objects too only with force).
- **G3 `SharedRedisProvider`**: logical DB index allocation recorded in `logical_resources` (fallback key-prefix mode for >16 DBs).
- **G4 `devflow.toml`**: extend `Config::find_config_file`/`from_file` to parse TOML by extension (`toml` crate); minimal schema (project name, engines list, hooks); `devflow init --toml`.
- **G5 Controller daemon**: fold reconciliation into the proxy process (it already has Docker events, JSON API, pidfile lifecycle) or `devflow daemon` sharing the same modules; git-hook paths call the daemon API when alive, else in-process ensure (same code path).
- **G6 ClickHouse logical isolation** (`CREATE DATABASE` via existing exec plumbing) — sidesteps the untested ClickHouse CoW path.

## Sequencing & status

A → B → C → D are independent of F/G and land first (A+B are this session's target).
E can proceed in parallel (frontend-only).
F before G1 ships (G's resource ownership needs stable identity).
Cloud providers (Neon/Xata/DBLab): **out of scope** here — per review, either delete or rebuild against verified APIs with contract tests; decision pending.

| Phase | Status |
|-------|--------|
| A | ✅ done (2026-06-11) — all 4 items, +3 regression tests in `vcs/git.rs` |
| B | ✅ done (2026-06-11) — B1–B8, +3 regression tests in `hooks/executor.rs`, switch test updated to new semantics |
| C | ✅ done (2026-06-11) — C1–C6, +1 regression test in `proxy/discovery.rs` |
| D (CoW correctness) | ✅ done (2026-06-11) — D1 (stop-don't-pause, both clone paths), D2 (moot: stopped parents are never auto-started by the reconciler), D3 (unique ZFS snapshot names + safe ordering + no rm -rf on mounted datasets), D4 (explicit PGDATA), D5 (TCP readiness probe). Bonus: fixed `regex` built without the unicode feature silently dropping ALL CommandGuard security patterns in standalone builds, and the `rm -fr`/`-Rf` flag-order bypass. |
| E (GUI visibility + uplift) | ✅ done (2026-06-11) — E1 (toast + promise-based confirm providers replace all 27 `alert`/`confirm` no-ops), E2 (`reportWorkspaceResult` surfaces per-service + hook failures from create/switch/delete), E3 (login-shell PATH bootstrap in `main.rs`), E4 (onboarding route fixed `onboard/*`, made reachable, doctor link relabeled). **Bonus uplift:** dark overlay title bar; design-token + component CSS layer (buttons/cards/badges/tables/toasts/tabs/status-dots/service-chips/skeletons/empty-states); icon set; sidebar with icons; **workspace-first service-status column** (the review's #1 leverage change); live ProxyDashboard (poll + event + copy/open actions); Home stats + skeletons; ProjectList empty/loading states. UI typechecks + builds clean. |
| F (identity unification) | ✅ done (2026-06-11) — single `vcs::resolve_project_root()` helper (worktree → main repo root) now feeds all four identity chokepoints: local-state project key (`get_project_key`), hook approval key (`workspace/hooks.rs`), container/project name (`config::project_name`), and SQLite `project_path` (`LocalProvider::new`). Backends inherit via factory's single `project_name`. One-time state migration merges per-worktree silos into the canonical key (`migrate_worktree_project_keys` + `merge_project_state`). +3 regression tests. Collapses the per-worktree fragmentation behind ~15 findings. |
| G (vision: shared engines + RustFS + devflow.toml + daemon) | planned |
