# Worktrees, Workspaces & Hooks — Integration Review

**Date**: 2026-06-12
**Scope**: How Git worktrees integrate across the whole product — core lifecycle (`crates/devflow-core/src/workspace/`), VCS layer, hook engine, CLI, TUI, desktop GUI, git-hook path, agent integration, and documentation.
**Method**: Static review of the current `main` (0.5.0, post-remediation), building on `docs/full-review-2026-06-11.md`. Every finding below was verified against the code unless marked otherwise. Yesterday's findings are referenced as “FR §…”.

---

## 1. TL;DR

Worktrees are devflow's strongest differentiator and the core is genuinely good: a shared `workspace/` lifecycle module used by CLI, TUI and GUI, CoW-accelerated file copying, dirty-state protection on removal, and an agent story that is worktree-first. The remediation that landed yesterday fixed the worst behavioral bugs (identity unification, safe deletion, resilient switches, approval keying).

What remains is **integration unevenness**, in three flavors:

1. **Config-surface confusion** — the worktree config has a *dead field* (`respect_gitignore`) with a doc comment promising behavior that doesn't exist, a CLI flag named after that dead field that actually toggles a *different* field (`copy_ignored`), and a GUI editor that exposes the dead field while hiding two real ones (`copy_ai_configs`, `extra_ai_dirs`).
2. **Surface asymmetry** — the same logical operation behaves differently per entry point: CLI vs GUI creation fires different hook phases; the GUI has per-workspace creation mode (branch/worktree), copy-file overrides, and worktree pruning that the CLI lacks; the TUI promises auto-cd but never emits `DEVFLOW_CD`; manually-added worktrees (git hook path) get a *third*, partial copy implementation with no AI-config dirs.
3. **Identity matching drift** — three different name mappings (worktree *name* `/`→`-`, worktree *path* normalized `_`, registry normalized) plus HEAD-shorthand-only lookup still cause orphaned worktrees on `merge --cleanup` and missing worktree paths in GUI listings for `feature/...` branches.

Documentation is the weakest surface: the single-page site has a real worktree section but documents only 4 of 7 config fields and makes a factually wrong claim about worktree paths; the README mentions worktrees four times in passing; `worktree-setup`, the auto-setup of manually created worktrees, `--no-respect-gitignore`, creation modes, and the agent worktree flow are essentially undocumented. A ground-up docs rewrite is in progress alongside this review (Astro Starlight, replacing `docs/index.html`).

---

## 2. Fixed since yesterday — verified

Credit where due; these FR findings are resolved on current `main`:

| FR finding | Status today |
|---|---|
| FR: `remove_worktree` force-prunes without dirty check (CRITICAL) | **Fixed.** `remove_worktree(path, force)` calls `ensure_worktree_clean()` unless forced (`vcs/git.rs:368-383`, `638-666`); `delete.rs` aborts *before* services/branch are touched and only falls back to `remove_dir_all` when `force` (`workspace/delete.rs:61-92`). |
| FR: approvals keyed on rendered command + per-worktree project key (HIGH) | **Fixed.** Approvals key on the canonical main-repo root via `resolve_project_root()` (`workspace/hooks.rs:69-76`); non-interactive unapproved hooks now *skip with a warning* instead of failing the switch (test `switch_workspace_skips_unapproved_hooks_non_interactive`). |
| FR: service orchestration error aborts switch/create mid-flight (HIGH) | **Fixed.** Both `switch.rs:159-177` and `create.rs:119-141` record the failure as a `ServiceResult` and finish the lifecycle (hooks still run, `DEVFLOW_CD` still emitted). |
| FR: state registry / project identity fragments across worktrees (MEDIUM) | **Fixed.** `vcs::resolve_project_root()` (`vcs/mod.rs:252-289`) unifies identity; consumed by config discovery (`config/mod.rs:744`), state keys (`state/local_state.rs:142,598`), approval keys, and the postgres provider. |
| FR: background hooks fire-and-forget, race process exit (HIGH) | **Fixed.** Background hooks are tracked in a `JoinHandle` registry and awaited at exit with `DEVFLOW_BACKGROUND_HOOK_TIMEOUT` (`hooks/executor.rs:19-40`). |
| FR: merge checks out target in your working tree | **Improved.** `merge` now executes in the target's dedicated worktree when one exists (`src/cli/workspace.rs:2575-2579`, `2739-2762`). |

---

## 3. Worktree feature inventory (what exists today)

- **Creation**: `worktree.enabled: true` makes `devflow switch` create worktrees instead of checkouts (`workspace/switch.rs:72-109`). Path from `path_template` (`{repo}`, `{workspace}`, legacy `{branch}`) resolved against the *normalized* workspace name (`workspace/worktree.rs:46-56`). Stale metadata is auto-pruned on name collision (`vcs/git.rs:583-604`).
- **File seeding**: `copy_files` (reflink/CoW per file), gitignored entries when `copy_ignored` (collapsed directory-level entries, rayon-parallel), AI tool dirs (`.claude`, `.cursor`, …) when `copy_ai_configs` + `extra_ai_dirs` (`workspace/worktree.rs:94-168`).
- **CoW**: `cow_worktree.rs` is *capability detection only* (APFS clonefile / Linux reflink probes). The actual acceleration is `reflink_or_copy` on the copied files — the worktree itself is a normal `git worktree` checkout. (The init-time message “worktrees will use fast copy-on-write cloning” refers to the copies, not the checkout.)
- **Auto-cd**: `DEVFLOW_CD=<path>` emitted by `switch`, consumed by the `shell-init` wrapper.
- **Manual worktrees**: the installed post-checkout hook detects worktree context and calls `devflow git-hook --worktree --main-worktree-dir …` (`vcs/git.rs:280-288`), so `git worktree add` gets devflow setup automatically; `devflow worktree-setup` does it manually.
- **Lifecycle integration**: hooks run with `working_dir` = the target's worktree (`workspace/hooks.rs:62-67`); `is_worktree`/`not_worktree` built-in conditions; `{{ worktree_path }}` template var; `multiplexer-session` recipe opens tmux/zellij in the worktree.
- **Removal**: `devflow remove` / `merge --cleanup` / GUI delete go through `delete_workspace` with dirty-check + `--force` escalation.
- **Listing**: `list` (tree + JSON), `graph`, `status`, TUI workspaces pane, GUI ProjectDetail all show worktree paths; `link` adopts pre-existing branches/worktrees into the registry.
- **Agents**: skills and `agent context` are worktree-first — they instruct agents to parse `worktree_path` from `--json switch -c` and use it as workdir (`agent.rs:232-333`).
- **jj parity**: jj workspaces map to the worktree API (`vcs/jj.rs:213-360`); `create_worktree_with_files` is provider-agnostic, so file copying works for jj too.

## 4. Cross-surface capability matrix

| Capability | core | CLI | TUI | GUI | git-hook path | Documented |
|---|---|---|---|---|---|---|
| Create worktree on switch | ✅ | ✅ | ✅ (exit-switch) | ✅ | ✅ | ✅ site §06 |
| Force mode (branch vs worktree) per creation | ✅ `WorkspaceCreationMode` | ❌ no flag | ❌ | ✅ selector | n/a | ❌ |
| `copy_files` override per creation | ✅ | ❌ | ❌ | ✅ | ❌ | ❌ |
| `copy_ignored` override per creation | ✅ | ⚠️ misnamed `--no-respect-gitignore` | ❌ | ✅ | ❌ | ❌ |
| AI config dirs copied | ✅ | ✅ via switch | ✅ | ✅ | ❌ **missing** | ❌ |
| Auto-cd into worktree | ✅ emits path | ✅ `DEVFLOW_CD` | ❌ **prints `Worktree:` only** | n/a (GUI) | ✅ | ⚠️ site claims TUI works |
| Worktree path in listings | ✅ | ✅ list/graph/status | ✅ | ⚠️ raw-vs-normalized miss | n/a | ⚠️ |
| Prune stale worktrees | ✅ trait + auto on create | ❌ no command | ❌ | ⚠️ shells out to `git` | n/a | ❌ |
| Dirty-state-protected removal | ✅ | ✅ + `--force` | ✅ | ⚠️ no force path in UI | n/a | ❌ |
| Adopt manually created worktree | ✅ | ✅ `worktree-setup`, `link` | ❌ | ❌ | ✅ auto | ⚠️ site only |

---

## 5. Findings

### W1 · HIGH — `worktree.respect_gitignore` is dead config, and three names fight over one behavior

**Where**: `config/mod.rs:455-461`, `src/cli/mod.rs:117-118`, `src/cli/workspace.rs:1191-1195`, `ui/src/pages/config/sections/WorktreeSection.tsx:109-119`

The field's doc comment promises: *“Exclude gitignored files from worktrees (both CoW and non-CoW paths). Default: true.”* Nothing reads it at runtime — its only consumers are its own default fn, tests, and a literal in a test fixture. There is no code path that excludes gitignored files (a `git worktree` checkout contains only tracked files; nothing to exclude), so the promised behavior cannot exist.

Meanwhile the CLI flag named after it, `--no-respect-gitignore` (“Include gitignored files in worktree”), actually maps to `SwitchOptions.copy_ignored = Some(true)` — i.e. it's a per-invocation alias for the *other* config field, `copy_ignored`. So:

- `worktree.copy_ignored` — real behavior (copy gitignored entries from main).
- `worktree.respect_gitignore` — dead, but rendered as a working toggle in the GUI config editor (“Respect .gitignore”) and present in the TS type.
- `--no-respect-gitignore` — real, but named after the dead field.

A user toggling “Respect .gitignore” off in the GUI and expecting `node_modules` to appear in new worktrees gets nothing; the knob that does that is the one labeled “Copy ignored files”.

**Fix**: Delete `respect_gitignore` from `WorktreeConfig`, the TS type, and the GUI section (serde will ignore it in existing YAML; optionally warn on load). Rename the CLI flag to `--copy-ignored` (keep the old name as a hidden alias). If exclusion semantics are ever wanted (e.g. for a future full-clone mode), reintroduce deliberately.

### W2 · HIGH — CLI and GUI fire different hook phases for the same “create workspace” operation

**Where**: `workspace/switch.rs:38-240` vs `workspace/create.rs:53-203`

Two parallel creation orchestrators exist:

- CLI `devflow switch -c X` → `switch_workspace()`: **PreSwitch** → VCS/worktree → services → **PostServiceSwitch** → PostCreate → PostSwitch.
- GUI “create workspace” → `create_workspace()`: **PreServiceCreate** → services → **PostServiceCreate** → PostCreate → PostSwitch (no PreSwitch, no PostServiceSwitch).

Consequences: a team's `pre-service-create` / `post-service-create` hooks *never run* for CLI-created workspaces; `pre-switch` / `post-service-switch` hooks *never run* for GUI-created ones. Same `.devflow.yml`, different lifecycle depending on which surface created the workspace — exactly the class of “hooks don't always work” confusion FR documented, surviving in a new form. (TUI exit-switch uses `switch_workspace`, so it follows the CLI shape, but with `NoApproval` and no `trigger_source` set — a third variant.)

**Fix**: Make `create_workspace()` delegate to `switch_workspace()` (it already supports `create_if_missing` + overrides) or extract one shared phase plan both call. If `WorkspaceCreationMode` is the reason `create` exists, move `creation_mode` into `SwitchOptions` and collapse. Document the canonical phase order in one place. Set `trigger_source: Some("tui")` in the TUI path.

### W3 · HIGH — `merge --cleanup` from inside the source worktree still orphans it (FR §“merge --cleanup” partially fixed)

**Where**: `src/cli/workspace.rs:2868-2904`, `workspace/delete.rs:61-92`, `vcs/git.rs:668-700`

Cleanup detaches HEAD first (“so the branch becomes deletable”), *then* calls `delete_workspace`. `worktree_path()` matches a worktree only by `head.shorthand() == workspace`; after `detach_head()` the source worktree's HEAD is detached, so the lookup returns `None`, the worktree directory and its git metadata survive, and the branch is deleted out from under it. Net result when you run `devflow merge --cleanup` from inside the source worktree: branch gone, orphaned worktree dir, and a later `switch -c <same-name>` hits the stale-metadata path with a directory that *does* exist (`git.rs:585-604` only auto-prunes when the path is missing), so re-creation fails.

The state-registry half of FR's finding is fixed (keys go through `resolve_project_root`), but the detach-ordering half is alive.

**Fix**: Resolve (and remove) the worktree **before** detaching HEAD — or better, make `worktree_path()` also match by worktree name (`worktree_name_for_branch`) / gitdir, not just checked-out HEAD (see W7). `delete_workspace` could also consult the registry's stored `worktree_path` as a fallback; it's already persisted at creation.

### W4 · HIGH — Shell wrapper still swallows interactive stdout; TUI never emits `DEVFLOW_CD` despite shell-init promising it (FR §shell-init, unfixed)

**Where**: `src/cli/config.rs:55-135`, `src/cli/workspace.rs:2413-2417`, `src/tui/mod.rs:82-86`, `src/cli/mod.rs:296-303`

The recommended wrapper does `output="$(command devflow "$@")"` — all stdout is captured until exit. Through it:

- `devflow remove x` prints `Continue? [y/N]` to captured stdout and blocks on stdin → appears to hang.
- `devflow tui` renders ratatui to captured stdout → blank screen, ANSI garbage on exit.
- TUI's “press o to open workspace/worktree” exits and prints `Worktree: <path>` (`tui/mod.rs:84-85`) — **not** `DEVFLOW_CD=<path>` — while `shell-init`'s own help text promises auto-cd for “open from TUI”. So the flagship worktree UX (jump into the worktree) silently doesn't work from the TUI, wrapper or not.

**Fix**: (a) Emit `DEVFLOW_CD=` from the TUI exit-switch path. (b) Rework the wrapper to stream: run devflow with stdout passed through and capture only a side-channel (e.g. `DEVFLOW_CD_FILE=$(mktemp)` env var the binary writes to, which the wrapper reads after exit — this also fixes TUI/prompt pass-through wholesale and is simpler than line-filtering). (c) Until then, route interactive prompts to stderr (inquire already does; the hand-rolled `print!` confirm in remove does not).

### W5 · MEDIUM — Branch-mode creation runs post-create/post-switch hooks in the main checkout

**Where**: `workspace/create.rs:90-101` + `workspace/hooks.rs:62-67`

With `creation_mode: branch` (GUI selector; also config default when worktrees are disabled), no worktree exists and nothing is checked out — `create_workspace` only creates the branch. Hook context then resolves `worktree_path = None`, so `working_dir` falls back to `project_dir` = the main checkout, *which is still on the previous branch*. A standard `post-create` hook like write-env (`DATABASE_URL={{ service[...].url }}` → `.env.local`) overwrites the **main checkout's** `.env.local` with the *new* workspace's connection string. Same for `post-switch` hooks, which `create_workspace` fires even though no switch happened.

**Fix**: In branch mode, either check out the new branch before hooks (making “create” = “create + switch”, matching the GUI's mental model), or skip working-dir-sensitive phases (PostSwitch at minimum) and document that `post-create` in branch mode runs in the project root on the *old* checkout. The first option is what users expect.

### W6 · MEDIUM — GUI workspace list matches worktrees by raw branch name against normalized registry names

**Where**: `src-tauri/src/commands/workspaces.rs:93-98`

`worktrees.iter().find(|w| w.workspace.as_deref() == Some(&b.name))` compares the worktree's raw HEAD shorthand (`feature/auth`) to the registry's normalized name (`feature_auth`). For any branch containing `/`, `.`, or uppercase, the VCS-fresh path never matches and the code silently falls back to the (possibly stale) registry value; if the worktree was created outside devflow, no path shows at all and the “git worktree” badge disappears. The same function normalizes `current_vcs_branch` for the `is_current` comparison two lines up — the worktree comparison just missed the same treatment.

**Fix**: Normalize `w.workspace` with the same `get_normalized_workspace_name` before comparing (config is already loaded in this function).

### W7 · MEDIUM — Three name mappings + HEAD-only matching is the root cause behind W3/W6 (FR §sanitization-collisions, still open)

**Where**: `vcs/git.rs:361-366` (worktree *name*: `/`→`-`), `workspace/worktree.rs:46-56` (worktree *path*: full normalization, `/`→`_`, lowercase), `config/mod.rs` sanitize (registry/services), `vcs/git.rs:668-700` (lookup: raw HEAD shorthand only)

One workspace `Feature/Auth` yields: git branch `Feature/Auth`, libgit2 worktree name `Feature-Auth`, worktree directory `../repo.feature_auth`, registry/service name `feature_auth`. Every consumer picks a different representation to match on, which is why lookups keep missing (W3, W6) and why FR's collision finding (`feature/auth` vs `feature-auth` vs `feature_auth` all mapping to one DB/path) still stands.

**Fix**: Add a single `WorkspaceIdentity { raw, normalized, worktree_name }` resolved in one place in core; make `worktree_path()` match by worktree name *and* HEAD shorthand *and* normalized form; emit a hard warning (or refuse) in `create_workspace`/`create_worktree` when a new raw name normalizes onto an existing different raw name.

### W8 · MEDIUM — The git-hook path is a second, divergent copy implementation — and it never copies AI config dirs

**Where**: `src/cli/git_hook.rs:8-93` vs `workspace/worktree.rs:65-176`

`copy_worktree_files` (used by the installed post-checkout hook inside worktrees and by `devflow worktree-setup`) reimplements the copy logic with different behavior: no `copy_ai_configs`/`extra_ai_dirs` handling at all, no override support, sequential `copy_files` (vs parallel), different logging. So the two ways a worktree comes into existence diverge: `devflow switch -c` worktrees get `.claude/`/`.cursor/` dirs; `git worktree add` + hook (or `worktree-setup`) worktrees don't — for an AI-focused tool, that's the worst file class to drop. It also means future fixes (e.g. W1 cleanup) must land twice.

**Fix**: Extract the copy phase of `create_worktree_with_files` into `copy_worktree_payload(vcs, config, main_dir, target_dir, overrides)` and call it from both paths. `worktree-setup` then inherits AI-dir copying for free.

### W9 · MEDIUM — Worktree pruning: GUI bypasses the VcsProvider (breaks jj), CLI has nothing

**Where**: `src-tauri/src/commands/workspaces.rs:361-405`, `vcs/mod.rs:192-199`

The trait has `prune_worktrees()`, yet the GUI command checks `supports_worktrees()` and then shells out to `git worktree prune` — wrong VCS for jj projects, ignores the abstraction, and depends on a `git` binary the rest of the GUI doesn't need. The CLI exposes no prune at all (`gc` handles orphaned *projects*; stale worktree metadata is only pruned opportunistically during creation collisions).

**Fix**: GUI: call `vcs_provider.prune_worktrees()`. CLI: add `devflow gc --worktrees` (or fold into `devflow cleanup`) using the same trait method, reporting what was pruned like the GUI does.

### W10 · LOW (downgraded from FR HIGH) — Hook-context worktree short-circuit is now a latent trap rather than an active bug

**Where**: `crates/devflow-core/src/hooks/mod.rs:436-466`

`build_hook_context` still returns the *current* directory as `worktree_path` whenever `project_dir` is itself a worktree, ignoring the target workspace. Since config discovery and state now resolve through `resolve_project_root`, normal CLI/GUI flows pass the main root and never hit the branch. It still misfires if a caller passes a worktree path directly (e.g. a GUI project registered at a worktree directory, or future API users) — hooks for workspace B would then run inside worktree A (FR's original scenario).

**Fix**: Guard the short-circuit: only return the current dir when `current_workspace()` matches the requested workspace (raw or normalized); otherwise fall through to the existing lookup. Three lines, removes the trap.

### W11 · MEDIUM — CLI lacks creation-mode and copy-file parity with the GUI

**Where**: `workspace/mod.rs:12-40`, `src/cli/mod.rs:79-126`, `ui/src/pages/projects/ProjectDetail.tsx:1578-1592`

`WorkspaceCreationMode { Default, Worktree, Branch }` exists in core and the GUI exposes it per creation (“Git worktree / Git branch” selector defaulting from config). The CLI cannot express it: with `worktree.enabled: true` there is no way to create a plain branch workspace (`switch -c --branch`), and with worktrees disabled no way to opt one workspace in (`--worktree`). Likewise `CreateOptions.copy_files` override is GUI-only; the CLI always passes `None`. Agents (CLI-driven) are the main loser — an agent that wants a lightweight branch-only workspace can't ask for it.

**Fix**: Add `--worktree` / `--branch` (mutually exclusive) and `--copy-file <path>...` to `devflow switch`, mapping to the existing core options. Mention them in `agent skill` output where relevant.

### W12 · LOW — Dead or half-wired hook surface: `triggers:` config parses but is never consulted; `previous_workspace` is advertised but never populated

**Where**: `config/mod.rs:34` + `hooks/triggers.rs:52-64` (no callers of `resolve_git_event`); `hooks/mod.rs:378-380` + `src/cli/hook.rs:478-479`

The `triggers:` section (FR flagged it; still true) deserializes into config but the git-hook dispatch hard-codes its routing in `git_hook.rs`, so user remapping does nothing. `previous_workspace` appears in `HookContext`, is printed by `devflow hook vars` when set, but no call site ever sets it — templates using it always render empty. Both are silent-promise bugs of the same kind as W1.

**Fix**: Either wire `TriggersConfig::resolve_git_event` into `handle_git_hook` (it's ~20 lines and gives users post-checkout→post-create remapping for free) or delete the config section. Populate `previous_workspace` in `switch_workspace` (the value is available as `current_workspace()` before checkout) — it's genuinely useful for cleanup hooks.

### W13 · LOW — GUI config editor: missing worktree fields and comment-destroying saves

**Where**: `ui/src/types/config.ts:33-39`, `WorktreeSection.tsx:8-16`, `src-tauri/src/commands/config.rs:13-24`

The TS `WorktreeConfig` carries 5 of 7 fields — `copy_ai_configs` and `extra_ai_dirs` can't be seen or edited in the form (they survive round-trips only because the section spreads the loaded object; the *fresh-enable* path builds the object from `DEFAULT_WT`, which omits them — serde defaults happen to match, so no data corruption today, but the invariant is accidental). Saving goes JSON→`Config`→YAML re-serialization, which drops every comment and reorders keys in a hand-maintained `.devflow.yml`. Combined with W1 (the one extra field it *does* show is the dead one), the worktree form is the weakest section of the editor.

**Fix**: Add the two missing fields to the type + form (chips input for `extra_ai_dirs`), remove the dead toggle, and consider a warning banner when the YAML on disk contains comments (“GUI save will reformat the file”).

### W14 · LOW — Switching to an existing worktree silently ignores copy overrides

**Where**: `workspace/switch.rs:74-83`, `workspace/worktree.rs:73-80`

`devflow switch feature-x --no-respect-gitignore` on a workspace whose worktree already exists short-circuits to the existing path; the override (and `copy_files` override in the GUI) is dropped without a message. Reasonable behavior, but worth a one-line notice (“worktree exists; copy options ignored”) — today it looks like the flag is broken.

---

## 6. Documentation findings (feeds the rewrite)

Quantified worktree coverage today: `README.md` 4 mentions (all passing), `docs/CLI.md` 13, `docs/index.html` 65 (a real §06 “Worktrees”), `llms.txt` 3, `AGENTS.md` 7, `CLAUDE.md` 11.

**Factual errors found** (now corrected in the new docs):

1. **Site §06 path-template table**: claims `{workspace}` renders “slashes become dashes” with example `../my-project.feature-auth`. Wrong — `resolve_worktree_path` uses full normalization (`workspace/worktree.rs:48`): `feature/auth` → `../my-project.feature_auth` (underscores, lowercased). The dash mapping applies only to the internal libgit2 worktree *name*.
2. **Site §06 config block**: documents 4 of 7 fields — `copy_ai_configs` (default **true**! it's why `.claude/` shows up in worktrees), `extra_ai_dirs`, `respect_gitignore` missing. (The last one should stay out — see W1 — but the first two are flagship.)
3. **shell-init help + site**: promise auto-cd on “open from TUI” — false today (W4).
4. **Hook docs staleness**: the site's conditions table predates `trigger_is:`/`trigger_not:`, `env_set:`/`env_is:`, `workspace_is:`/`workspace_not:`, `is_default_workspace`/`not_default_workspace` (0 mentions of each); template-variable docs omit `trigger_source`, `vcs_event`, `name`; `previous_workspace` is documented but never populated (W12).
5. **CoW phrasing**: init output and parts of the site imply the *worktree* is CoW-cloned; only the copied files (`copy_files` / `copy_ignored` / AI dirs) are reflinked. The checkout itself is a standard `git worktree`.

**Coverage gaps** (worktree-relevant things documented nowhere user-facing):

- `devflow init` enables worktrees **by default** in non-interactive/JSON mode (`src/cli/mod.rs:1123-1126`) and recommends them interactively — a significant default, unstated.
- Manually created worktrees are auto-configured by the installed post-checkout hook (`git worktree add` “just works”) — arguably the best worktree feature, only inferable from `worktree-setup`'s help.
- `devflow worktree-setup`, `devflow link`, `--no-respect-gitignore`, `merge` executing in the target worktree, dirty-worktree protection + `--force`, GUI creation-mode selector and prune button, `capabilities`' `worktree_cow` probe, sandboxed workspaces (`--sandboxed`), the multiplexer flags (`-o`/`-d`), and the sync-ai-configs round-trip are all undocumented or one-liner-only.
- `CLAUDE.md` project map predates the `workspace/` module, `merge/`, `sandbox/`, `skills/`, `docker/` core modules and the `daemon`/`train`/`gc` CLI files; its config schema omits `respect_gitignore` (rightly, but by accident), `name:`, `sandbox:`, `merge:`, `commit:` subtleties.

**Structural**: a single 3,919-line `index.html` can't host reference + guides + concepts at this point (proxy, daemon, shared engines, sandbox, trains, agents…). Replaced by the Starlight site (see §8).

---

## 7. What's good — keep and build on

- **One core lifecycle, three frontends.** `workspace/{create,switch,delete,hooks,worktree}` shared by CLI/TUI/GUI is the right architecture; the remaining work is making the frontends expose it *uniformly* (W2, W11), not re-architecting.
- **`resolve_project_root` identity unification** is exactly right and already consumed at the four load-bearing spots.
- **Agent ergonomics**: skills emitting `--json switch -c` + `worktree_path` parsing, non-interactive hooks that skip instead of abort, and `DEVFLOW_APPROVE_HOOKS` form a coherent contract.
- **Safety posture on deletion** (clean-check, abort-before-destroy, force escalation with fs fallback) is now better than raw `git worktree remove`.
- **`is_worktree`/`not_worktree` conditions + worktree-aware `working_dir`** make hooks genuinely worktree-native — they just need to be documented.

## 8. Prioritized recommendations

1. **Unify the creation paths** (W2, W5, W11): one orchestrator, one phase order, `creation_mode` in `SwitchOptions`, CLI `--worktree/--branch/--copy-file` flags, TUI trigger_source. *Removes a whole class of “works in GUI, not in CLI” reports.*
2. **Kill the dead config** (W1, W12): drop `respect_gitignore` everywhere, rename the CLI flag, wire or drop `triggers:`, populate `previous_workspace`.
3. **Fix identity matching once** (W7 → fixes W3, W6): `WorkspaceIdentity` in core; `worktree_path()` matching by name+shorthand+normalized; normalize in the GUI list; resolve-before-detach in merge cleanup.
4. **Make auto-cd honest** (W4): TUI emits `DEVFLOW_CD`; wrapper switches to a side-channel file and stops capturing stdout.
5. **One copy engine** (W8): shared `copy_worktree_payload`, giving hook-created worktrees AI-config parity.
6. **Prune parity** (W9): trait-based prune in GUI, `gc --worktrees` in CLI.
7. **GUI worktree form** (W13): add the two real fields, drop the fake one.
8. **Docs**: ground-up rewrite — executed alongside this review as an Astro Starlight site replacing `docs/index.html`, with a dedicated Worktrees concept page + workflow guide, a complete config/hook reference generated from the structs above, and the corrections from §6.
