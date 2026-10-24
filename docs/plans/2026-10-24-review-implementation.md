# Review implementation (2026-10-24)

Status: done (2026-10-24). Implements the review of main `c3841964` (two GPT-6-Astra rounds). Integration branch: `t3/review-worktree-design`. Each workstream ran in its own T3 thread and worktree on a branch `review/<workstream>` and was merged by rebase or cherry-pick after review.

## Outcome

Every workstream below is implemented and merged: W1-A to W1-E, W2-A to W2-C, W3-A to W3-C and W4-A, the last split into its code half (schema, version pins, init trim) and its documentation half.

- **Reviews.**
  - GPT-6-Astra reviewed wave 1 and the first rounds of wave 2. GPT-6.1 Sol took over from there.
  - Each workstream had at most three review rounds, then a verification pass of its last fixes.
  - No high finding is open.
- **Checks on the final tree.**
  - fmt, plus clippy for core, CLI and GUI.
  - Core and CLI tests, GUI tests, the link test and the schema drift test.
  - All four Docker suites (PostgreSQL, workspace, Valkey/Redis, MySQL) and the pitchfork suite.
- **Known limits, documented where they apply.**
  - **SQLite:** a process that holds the database file open between transactions holds no lock devflow can see. SQLite does not say which inode its connection opened, so a double rename within microseconds of the open can defeat the check. A live-file copy can exist, under a private name, outside the worktree for the length of one copy before it is removed. All three are written up in ARCHITECTURE.md.
  - **Host files without an atomic exchange:** where the platform has no exchange, the hard-link fallback keeps the check-then-rename window.
  - **`claude-create`:** a creation that crashed before its receipt was written is refused, not resumed.
  - **Records without an identity:** records written by pre-release builds have no directory identity and stay `unknown` until `gc --rebind`.
  - **Focus:** a forwarder that dies abnormally leaves no `ended.json`.
  - **mise:** hook gating needs mise 2026.4.18 or later.

## Why

devflow's data layer (PostgreSQL 18 `FILE_COPY` clones, copy-on-write Valkey/Redis and MySQL, journals, ownership by identity) is ahead of every other worktree tool. The loop around it is thin: worktrees made or deleted by agent hosts, finding and leaving workspaces, cleanup and resource control, and several correctness gaps reproduced with the release binary:

- `switch -c X` with only `origin/X` creates `X` from main's HEAD, untracked (`worktree.rs:228`).
- A `--no-db` workspace's hooks and `devflow exec` inherit the caller's `DATABASE_URL` (`exec.rs:44`, `hooks.rs:287`).
- After `git clean -fdx`, `setup` reports ready and never rebuilds `.devflow/env` (`setup.rs:2884` ignores the file).
- One approval per checkout (`trust.rs:284`): worktrees whose `pitchfork.toml` differ revoke each other's approval.
- `remove` of a workspace whose directory is gone leaves its daemon running and deletes the receipt (`processes.rs:2890`).
- The post-checkout hook blocks `git worktree add` for the whole setup; `git worktree remove` deletes the receipt and leaves data, containers and daemons.
- CI on main is red (`tests/cli.rs:1174` needs Docker, shellcheck SC2317 in `install.sh`); `install.sh` installs v0.8.0 while the README documents 1.0.

## Ground rules (every workstream)

- **Scope.** Do what your section says; where it leaves a choice, choose, and record the choice in your report. Do not restructure documentation (W4 does that); update the sections your change affects.
- **Architecture.** Core owns behavior; the CLI and the GUI build requests and render results. Every new problem state carries typed `Remedy` values (buttons in the GUI, `hint:` lines in the CLI); core messages never contain command lines; the GUI never shows `devflow …` text. Messages name files, keys and variables, never values; credentials stay masked.
- **Compatibility.** Receipts stay version 2: new fields are optional (`#[serde(default)]`). New configuration keys are optional and validated; unknown keys stay errors.
- **Safety.** Record intent before every external effect; never delete what is not positively owned; a state devflow cannot read is "unknown", never "nothing".
- **Tests.** Every behavior change gets tests (core unit or integration, CLI). Docker-backed code: run the matching Docker suite once before reporting.
- **Git.** Conventional commits as in `git log` (`fix(core): …`, `feat(cli): …`, `test(…)`, `docs: …`). No `Co-Authored-By` or other AI attribution lines. Never `--no-verify` or `HK=0`. Do not push, tag, open pull requests, or touch `wip/0.8`.
- **Checks before reporting** (from your worktree):
  - `cargo fmt --check`
  - `cargo clippy --locked -p devflow -p devflow-core --all-targets -- -D warnings`
  - `cargo test --locked -p devflow -p devflow-core`
  - Only if you change `crates/devflow-gui` or a core API it uses: `CARGO_TARGET_DIR=/tmp/devflow-gui-target-<workstream> CARGO_PROFILE_DEV_DEBUG=line-tables-only cargo clippy --locked -p devflow-gui --all-targets -- -D warnings`, then `cargo test --locked -p devflow-gui` with the same variables, then delete that directory.
  - If mise refuses the worktree's `mise.toml` as untrusted, run `mise trust` there (it is this repository).
- **Machine.** Check `df -h /System/Volumes/Data` before large builds and stop if under 25 GiB free. OrbStack is running; the images the suites use are present (`postgres:18`, `valkey/valkey:8-alpine`, `redis:8-alpine`, `mysql:8.4`): pull nothing else, no Docker-based Linux builds, never prune or remove resources the suites did not create. Do not start or stop OrbStack.
- **Report** (final message): what changed and why, choices made, files touched, checks run with results (counts), commits, open questions.

## Wave 1 (parallel)

### W1-A `review/ci-release`: CI, install truth, test hygiene

1. `tests/cli.rs` `id_arguments_take_the_short_ids_listings_show` fails without Docker because `service NAME drop ID` asks the Docker daemon before looking the id up in the catalog. Resolve the id first (an id that matches no recorded branch is reported without Docker), keep the test unignored. If the order cannot change safely, mark it `#[ignore = "needs Docker"]` and say why.
2. `scripts/install.sh`: fix the shellcheck findings (SC2317) that fail the CI step; keep POSIX `sh`.
3. Truthful installation until 1.0 is released: the README's Install section says the latest published release is 0.x (another configuration format, see `docs/MIGRATING.md`) and how to build 1.0 from source; `install.sh` warns after installing a 0.x release. No version bump, no tag.
4. Installer smoke test: a CI job on release tags (and `workflow_dispatch`) installs the published artifact with `install.sh` into a temporary directory and runs a quick start against a scratch repository (`--version` equals the tag, `init --yes --postgres none`, `switch -c smoke`, `list`, `remove smoke`).
5. CI runs the Valkey/Redis and MySQL Docker suites (`cargo test -p devflow --test services_docker -- --ignored --test-threads=1`, same for `services_mysql_docker`) on Linux with timeouts, next to the PostgreSQL job.
6. `scripts/bench-workspaces.sh`: five workspaces end to end on a project with PostgreSQL and Valkey (cold and warm `switch -c` times, per-container memory from `docker stats`, Docker VM disk growth, MySQL source downtime when configured), printed as a table and JSON. Manual; documented in CONTRIBUTING.
7. `doctor`'s docker check also asks the daemon (bounded, through `subprocess::run`): "client found, daemon not reachable" is a failure when the project needs Docker, a warning otherwise.
8. When devflow creates `devflow.local.toml` (`--local` edits), it lists it in `<git common dir>/info/exclude` unless Git already ignores it, and says so.

Files: `.github/workflows/*`, `scripts/install.sh`, `scripts/bench-workspaces.sh`, `tests/cli.rs`, the service id lookup (`src/cli/service.rs`, `services/*`), `doctor.rs` (the docker function only), `config_edit`, README Install section, CONTRIBUTING, CHANGELOG.

### W1-B `review/env-ownership`: the workspace's variables are its own

Hooks and `devflow exec` set the workspace's variables over the inherited environment and clear the libpq selectors only when the target has a `DATABASE_URL`. A name the target does not set (a `--no-db` workspace, a removed recipe, another project) keeps the caller's value.

1. One implementation, used by hooks (every phase), `devflow exec`, the mise checks (`env_file::check_command_env`) and anything else that builds a child environment from `.devflow/env`:
   - **Categories.** Identity (`DEVFLOW_PROJECT`, `DEVFLOW_PROJECT_ROOT`, `DEVFLOW_WORKSPACE`, `DEVFLOW_WORKSPACE_PATH`, `DEVFLOW_SOURCE_PATH`, `DEVFLOW_WORKSPACE_ID`, `COMPOSE_PROJECT_NAME`); connection outputs per configured service (postgres: `DATABASE_URL`, the libpq selector family, each `<NAME>_DATABASE_URL`; valkey/redis: its `env` name; mysql: its URL variable and `MYSQL_*`); `[env]` recipe and preset outputs. Execution controls are never owned: `DEVFLOW_SETUP_ACTIVE`, `DEVFLOW_PHASE`, `DEVFLOW_TRUST_HOOKS`, `DEVFLOW_NON_INTERACTIVE`, `DEVFLOW_CD_FILE`, `DEVFLOW_BIN`, `DEVFLOW_CONFIG_DIR`, `DEVFLOW_DATA_DIR`, `DEVFLOW_INSTANCE_ID`, `DEVFLOW_INHERIT` and every other configuration `DEVFLOW_*`.
   - **Target set.** Every name the target's configuration declares, bound or not: a `--no-db` workspace of a project with PostgreSQL still owns `DATABASE_URL` and the selector family.
   - **Source set.** When the inherited environment has `DEVFLOW_WORKSPACE_PATH` and `DEVFLOW_WORKSPACE_ID`, and that path's `.devflow/env` exists, is not a symlink (nor under one) and records the same `DEVFLOW_WORKSPACE_ID`, every name in that file is owned too (another project's included). Otherwise the source set is empty.
   - **Rule.** Remove target ∪ source names, then apply the target's values.
2. Explicit inheritance: `devflow exec --inherit NAME` (repeatable) and `DEVFLOW_INHERIT=NAME,NAME` keep the caller's value of those names when the target sets no value of its own (CI with its own `DATABASE_URL` and `--no-db`). A `setup --database-url` attachment is a target value.
3. Diagnostics, names and paths only: setup and `doctor` warn when a copied dotenv file (`[worktree] copy` entries named `.env` or `.env.*`) defines an owned name with a value other than `.devflow/env`'s; `doctor` gains an `isolation` check for the current workspace (the process environment's owned values equal `.devflow/env`'s, naming the source workspace when known; with a reachable server, `SELECT current_database()` through the workspace URL equals the binding's database).
4. Tests: the reproduced `--no-db` leak, a removed recipe cleared through the source set, another project's names, `DEVFLOW_SETUP_ACTIVE` kept, `--inherit` and `DEVFLOW_INHERIT`, a forged or symlinked source file ignored, the mise check getting the same environment.

Files: `exec.rs`, `hooks.rs`, `env_file.rs` (or a new `env_owner.rs`), `env_file/mise.rs`, the place in `setup.rs` that assembles hook environments, `doctor.rs` (new checks only), `src/cli/exec.rs`, docs.

### W1-C `review/worktree-create`: create the right checkout, fast

1. **Remote branches.** No local branch `NAME`: look for `refs/remotes/*/NAME`. Exactly one: create the local branch at that commit with its upstream set (`branch.NAME.remote`, `branch.NAME.merge`), as `git worktree add` does. Several: use `checkout.defaultRemote` when it names one of them, otherwise a typed ambiguity error listing them (remedy: name one with `--base`). None: as today. Never fetch. Output and JSON say what it tracks.
2. **`switch -c NAME --base REF`** (`CreateRequest.base`): any commit-ish, resolved once and reported; only for a new branch (an existing local branch with a different commit is an error). `--from WS` keeps choosing the workspace payloads and data come from; without `--base` the base is `--from`'s HEAD, as today. `--base origin/x` for `x` tracks it. The GUI's new-workspace dialog gets an optional "Start from (branch, tag or commit)" field.
3. **Refuse what libgit2 cannot check out faithfully**, before creating anything, as a typed problem with remedies: Git LFS (`filter=lfs` in the base commit's `.gitattributes`, or `filter.lfs.*` configured) and partial clones (`extensions.partialClone`, any `remote.*.promisor = true`). A configured sparse checkout is a warning (the worktree is a full checkout). W3-B adds an opt-in native backend.
4. **Plain errors.** No raw libgit2 text (`class=…; code=…`) for common failures: a ref namespace conflict (`feature` beside `feature/x`), invalid names, a sanitized directory collision (`feature-x` and `feature/x` both become `feature-x`: name the other workspace), names over 100 characters.
5. **Payloads** (`cow.rs`): install with an atomic no-replace rename (`renamex_np` with `RENAME_EXCL` on macOS, `renameat2` with `RENAME_NOREPLACE` on Linux, the current check-then-rename elsewhere); an existing destination is `AlreadyPresent`. The serial timestamp walk after a whole-tree `clonefile` (`cow.rs:221`) costs 1.5 s for 20,000 files: test what APFS `clonefile` preserves (file and directory times, modes), then drop or parallelize the walk so timestamps still equal the source's; target under 0.5 s for that tree. The human output says "cloned" for reflinked entries and "copied" only for copies.

Files: `worktree.rs`, `cow.rs`, `setup.rs` (`CreateRequest.base` only), `src/cli/workspace.rs` (`SwitchArgs`), `remedy.rs`, the GUI new-workspace dialog, tests. W1-D1 changes `worktree::find`; do not touch it.

### W1-D1 `review/activate`: one core activation

1. `setup::activate(project, workspace, options, progress)` for `devflow switch NAME` and the GUI's actions on an existing workspace, and `setup::create_and_activate` for `switch -c` and the GUI's New workspace: both frontends run the same steps in the same order (setup or resume, repair, endpoint refresh, container services, switch hooks, autostart).
2. **Locks.** Process lock, then receipt lock (the documented order). Setup, repair, endpoint refresh and switch hooks run under the receipt lock; autostart as today. A devflow started by a hook (`DEVFLOW_SETUP_ACTIVE`) gets `Busy` instead of waiting for its parent.
3. **Repair derived files** (in activation and in `devflow setup`): a missing or unreadable `.devflow/env` is rebuilt from the receipt's bindings, identity and the approved recipes, with no new data and no hook run again; devflow's own `pitchfork.local.toml` is rewritten when missing. The outcome says what was repaired.
4. **Outcome.** Setup status is separate from activation: each service `running`, `started` or `failed` (with the reason), the switch hook report, the autostart report. A ready workspace whose service failed to start says so (`services_state` in JSON); exit codes keep following the status.
5. **Warnings reach the user.** `BranchCredentials.warnings` (sessions ended on the source, recovered operations) and container clone reports (MySQL source downtime) go into the outcome's warnings, in the CLI and the GUI.
6. **Branch drift.** A workspace is identified by its receipt (`workspace` is the name at creation). When the checkout holds another branch, activation, `list` and the GUI say so ("now on branch X; its data still belongs to workspace Y"). `worktree::find` also matches the receipt's workspace name, so `devflow switch feature/a` still works after `git switch other` there.
7. `last_active_at` in the receipt, set by activation.
8. The first activation of the default workspace (no receipt yet) says, before running, that setup (hooks included) runs in the main checkout; JSON `first_setup: true`.

Files: `setup.rs` (create, switch, setup paths and the env phase), `receipt.rs`, `worktree.rs` (`find` only), `src/cli/workspace.rs`, `crates/devflow-gui` (`ops.rs` create, switch and rerun use the new functions; the workspace detail shows drift and service state), tests.

### W1-D2 `review/removal-processes`: removal accounts for everything

1. `remove` plans drops from `receipt.creating` too: under the backend's lock it verifies that this instance owns the pending branch and drops it. The worktree and receipt are deleted only once every owned resource is accounted for; otherwise the removal stops before any effect (forceable, said so).
2. A missing checkout with recorded daemons: the stop plan (`processes.rs:2890`) uses the receipt's `processes` ids and the namespace `df-<project8>-<instance8>` to list and stop by full id; when pitchfork cannot confirm, `state_unknown` (forceable), never "nothing runs". `prune` stays consistent with it.
3. `ps` and `up` agree: `ps --json` `can_start` reflects every check `up` makes, the mise delivery check included. `init` proposes `run = "devflow exec -- CMD"` for every daemon it generates with `mise = true`, so its own proposal passes.
4. Switch and teardown hooks get the mise gate setup hooks have (`check_command_env`): failing switch hooks are skipped with a warning and remedies; failing teardown hooks refuse the removal before any effect (forceable).
5. The `--delete-branch` refusal's remedy includes the flag, and a forced removal never silently keeps the branch.

Files: `setup.rs` (`remove`, teardown), `processes.rs`, `pitchfork.rs`, `propose` (daemon commands), `env_file/mise.rs`, `remedy.rs`, tests.

### W1-E `review/data-sources`: cloning without surprises

1. **Busy sources.** `on_busy_source` keeps its defaults (`disconnect` for a managed loopback server, `fail` otherwise) and gains `"wait"`: wait up to `busy_wait` (default `"30s"`) for the source's sessions to end, then fail naming them (application name, client address, state; never queries). Under `disconnect`, the warning names the sessions that were ended.
2. **Seed data for automation.** Optional `[data] noninteractive_from = "snapshot:NAME"`: a non-interactive creation or adoption (agents, the Git hook, integrations) without `--data-from` takes each service's data from that snapshot when it exists, else the service's default with a warning. `noninteractive_max_age` (optional) warns when the snapshot is older.
3. **Strategy from measured CoW.** `STRATEGY FILE_COPY` when the capability is `verified` (measured or declared); when it is `copy` or `unknown` and the source is smaller than `wal_log_below` (default 256 MiB), `STRATEGY WAL_LOG` (no forced checkpoints; on ext4, 0.47 s against 6.7 s). The branch records the strategy used.
4. **MySQL downtime** of a clone is a warning in the setup outcome ("the source sql was stopped for 3.0 s"), not only in `service status`.

Files: `config.rs`, `db/*`, `services/*` (downtime, seed source), the source resolution in `setup.rs` (minimal), tests (`db_postgres.rs` and the services suites).

## Amendments after design review round 3

These replace the matching parts of the wave 1 sections.

- **W1-B.** The owned-names manifest in `.devflow/env` is versioned. A target file without one (an older devflow) needs repair: `exec` refuses with a typed problem whose remedy is setup, which rebuilds it. Each instance keeps every name it has ever exported (receipt and manifest), so a shell holding a removed recipe's variable is still cleared. An inherited workspace that cannot be resolved (path gone, no file, id mismatch, symlink) is refused with a typed problem and an explicit override that clears the target set plus every name devflow can generate; path and id matching only decide what to clear, never ownership or approval. The mise checks verify absence as well as values for every owned name, through one shared gate that W1-D2 uses for switch and teardown hooks and the daemon start gate.
- **W1-C.** Precedence is `--base`, then an explicit `--from`, then remote DWIM, then the default workspace's HEAD, resolved once. The LFS and partial-clone check inspects the commit actually checked out (an existing local branch included), scans completely or refuses when a bound is reached, and takes `.git/info/attributes`, `core.attributesFile` and the effective `filter.lfs` configuration into account.
- **W1-D1.** Process lock, then receipt lock; setup, repair, endpoint refresh and switch hooks run under both, through explicitly locked helpers (no re-acquiring, no try-lock that silently skips); the receipt lock is released before autostart, which receives the held process lock; a nested devflow tries both locks without waiting. Repair runs migration and invalidation first, honours a recorded `--no-db`, checks the current approval, refreshes endpoints before writing, runs before any `--if-new` shortcut, never claims or rotates credentials, and treats a manifest-less `.devflow/env` as stale. `worktree::find` collects every matching identity and refuses an ambiguous name, listing the paths.
- **W1-D2.** Planning drops nothing; execution uses the backends' journal-aware operations with ownership rechecked under the deletion's own lock. Pending records stay recoverable when their service leaves the configuration. A pending data branch that cannot be resolved refuses the removal (not forceable) until W2-A gives it a durable record; an unknown daemon state stays forceable, and the forced result lists what may still run. A failure after teardown or a drop started is a partial removal saying what remains. A missing checkout's daemons are stopped by full id through the supervisor and verified, or reported unknown.
- **W1-E.** A missing seed snapshot fails that service with a typed problem unless an explicit fallback (`noninteractive_fallback = "empty"`) is configured; a live source is never the fallback. The default workspace's root branch is unaffected. `[data]` and clone-tuning keys stay out of configuration generations. WAL_LOG never satisfies `require_cow`; the strategy is journaled before the creation; busy retries share one deadline on the lock-owning session.
- **W1-A.** A manual installer smoke run needs an explicit release tag, waits for every artifact, and release publication depends on the Docker suites.
- **Merge order.** A, C, B, E, D2, D1.

## Wave 2 (after wave 1 is merged)

### W2-A `review/ledger-gc`: workspaces stay accounted for after Git forgets them

Receipts live in `.git/worktrees/<id>/devflow/`, which `git worktree remove` deletes, as do the agent hosts that delete worktrees themselves (T3 Code, Cursor, Codex, Claude Code). Their data branches, containers and daemons then stay with no record, and W1-D2 must refuse removals it cannot account for.

1. **Workspace record.** `<git common dir>/devflow/workspaces/<instance-id>.json` (0600, versioned, written atomically, one lock per record) holds: instance id, project id, the local checkout identity (canonical main checkout path and common dir), the workspace name at creation, the worktree path, the Git admin id, `created_at`, `last_active_at`, a state (`active`, `removing`), the owned resources by identity (PostgreSQL branch ids, pending ones included; container branch ids per service key; the pitchfork namespace and recorded daemon ids) and a removal journal. No credentials.
   - Written before provisioning, updated whenever the receipt records a binding, a pending branch or a daemon start, and by activation. One writer updates both under the receipt lock, record first. The receipt stays the phase record and the holder of credentials; the record is authoritative for ownership once the receipt is gone.
   - Workspaces set up before this get a record from their receipt on the next setup, activation or `gc` (no provisioning).
   - `remove` journals into the record before each effect and deletes the record only when everything is accounted for; a partial removal keeps it (`removing`) so `gc` or a later `remove` can finish. W1-D2's unforceable "pending branch that cannot be resolved" becomes forceable, since the record survives.
2. **`devflow gc`** (core `gc::inventory`, `gc::remove`) classifies every record and every registered worktree:
   - `active`: registered, directory present, receipt matching the record;
   - `moved`: the recorded path is gone but a registered worktree with the same Git admin id and instance exists elsewhere (refresh the path, keep everything);
   - `missing`: registered, directory gone (prune's case);
   - `removed elsewhere`: no registration and no directory (`git worktree remove` or an agent host); the record still lists the resources;
   - `unknown`: an unreadable record or receipt, an unreachable server or Docker daemon, or a missing parent directory (a disconnected drive). Never removable.
   Each entry shows its resources with what is measurable (database size, container state and memory), last activity, branch, and advisory merge evidence: ancestry against the default branch's upstream (the local default without one), and "looks squash-merged" from patch ids or an identical tree, a hint only.
   - `devflow gc` alone prints the inventory and changes nothing. `devflow gc --remove ID…` removes the named instances, revalidating ownership and expected generations under the backends' locks; `--yes` skips the prompt and never widens the selection; `--snapshot` snapshots each branch first (`gc-<workspace>-<date>`).
   - Never removed: attachments (`setup --database-url`), shared servers, snapshots, branches another instance owns, anything `unknown`, an `active` workspace.
   - JSON output and remedies; a GUI Clean up page with the same inventory, selection and confirmations.
3. **Visibility.** `devflow list` adds one line when removed-elsewhere or missing workspaces still hold resources, with the `gc` remedy; `doctor` reports them. Records are local files: `list`, `exec`, `path` and completion never query a server or Docker.

Files: new `ledger.rs` and `gc.rs` in core; record writes in `setup.rs`, `setup/removal.rs`, `setup/activate.rs`, `receipt.rs`; `worktree/prune.rs` kept consistent; the CLI `gc` command; the GUI Clean up page; tests; docs.

### W2-B `review/integrations`: agent hosts set workspaces up and down

1. **Adoption contract**, proven end to end per host before the next: create (adopt what the host made, or create for hosts that delegate creation), wait until ready, run through `devflow exec`, remove or archive, recover.
   - `devflow wait [PATH] [--timeout D]` and `devflow setup --wait`: wait for a setup running elsewhere; exit codes follow the status (0 ready, 2 needs attention or still running at the timeout, 1 error). `devflow exec --wait` waits the same way before starting the command.
   - Optional background Git hook: `[integrations] git_hook = "background"` starts `devflow setup --if-new --from-git-hook` detached (own process group, log in the worktree's Git directory) and returns at once. The default stays synchronous.
2. **`devflow integrate HOST [--remove] [--print]`** for `t3`, `claude`, `conductor`, `cursor` (and `codex` once its file is verified): writes the host's configuration additively, keeping existing content; a conflicting existing script is reported, never overwritten.
   - **T3 Code:** `t3.json` script with `runOnWorktreeCreate` and `async: false` running `devflow setup --wait`. Verify the schema in pingdotgg/t3code (`packages/contracts/src/t3ProjectFile.ts`, `docs/user/project-settings.md`). There is no teardown hook, so removal relies on `devflow gc`, and the integration says so.
   - **Claude Code:** `.claude/settings.json` hooks. `WorktreeCreate` runs `devflow integrate claude-create`, which reads `{"name": …}` on stdin, creates the workspace non-interactively, and prints only the worktree path as the last line on stdout (non-zero exit fails Claude's creation). `WorktreeRemove` runs `devflow integrate claude-remove` with `worktree_path` on stdin. Verify the contract in Claude Code's hooks reference.
   - **Conductor:** `.conductor/settings.toml` `[scripts] setup = "devflow setup --wait"`, and an `archive` script that removes the workspace and keeps the branch. A `CONDUCTOR_PORT` at creation becomes the app port (W2-C).
   - **Cursor:** `.cursor/worktrees.json` `setup-worktree` with `devflow setup --wait`. There is no teardown, and Cursor deletes worktrees past 25 or 50 GB, even ones it did not create: gc covers them.
3. **`.worktreeinclude`** (read by Claude Code, Codex, Conductor, worktrunk): gitignore-syntax patterns; files that match and that Git ignores join the payload set, merged with `[worktree] copy`, reported like copy entries.
4. **Onboarding.** `init` and the GUI wizard detect host files (`t3.json`, `.conductor/`, `conductor.json`, `.cursor/`, `.claude/`) and propose the integration. The GUI wizard also offers the post-checkout hook, with its limits stated. The wizard never offered it, which is why tt2's T3 worktrees were never set up.
5. **Diagnostics.** `doctor` explains a relative `core.hooksPath` (husky's `.husky/_` is generated and ignored, so the hook never fires in a new worktree) and points to the host integration. Detached-HEAD worktrees (Codex) are adopted under their directory name.

Files: new `integrate.rs` and the CLI `integrate`, `wait`; `githook.rs`; `.worktreeinclude` in `cow.rs`/`config.rs`; `propose` (host detection); the GUI wizard; tests, including a T3-style flow (`git worktree add`, `devflow setup --wait`, `exec`, `gc`); README "Agents and automation", AGENTS.md.

### W2-C `review/navigation`: find, enter and leave workspaces

1. `devflow switch` with no name opens a picker on a terminal (most recently active first, with branch, status and drift); without a terminal it is a usage error with a remedy.
2. `devflow switch -` enters the previous workspace of this shell: the `shell-init` wrapper exports `DEVFLOW_PREVIOUS` (the directory it left) after each change of directory. Without the wrapper, a clear error.
3. Dynamic completion of workspace names for `switch`, `path`, `remove`, `up`, `down`, `ps` and `open`, from local Git and receipts only.
4. Switching keeps the relative subdirectory when it exists in the target (`apps/web`), else the root.
5. `devflow open [NAME] [--editor|--terminal]`: the editor and terminal settings move from the GUI's `gui.toml` to a core settings file (`<config dir>/settings.toml`; the GUI migrates its values), so the CLI and the app open the same tools.
6. `devflow list --wide` (and JSON fields) adds dirty, ahead/behind the default branch's upstream, advisory merge evidence (as W2-A), last activity, container services state, daemons with ports and URLs. The default stays fast.
7. **App port.** One persisted port per workspace, exported as `DEVFLOW_PORT` (an owned name, usable in `[env]` recipes and compose files), allocated once under a machine-wide lock from `[worktree] port_range` (default 40000–49999), bind-checked, journaled in the receipt and W2-A's record, and kept. A `CONDUCTOR_PORT` present at creation is used instead. Shown by `list --wide` and `ps`.
8. `devflow down [NAME] --services` and `up --services` stop and start the workspace's own containers (never the shared PostgreSQL server; volumes kept); `down --all --services` covers every workspace of the project. The GUI offers the same.
9. **Shell integration diagnostics.** The wrapper marks its calls (`DEVFLOW_WRAPPED=1` in the command's environment); `doctor` and `switch` say when it is not active and how to enable it.

Files: `src/cli/workspace.rs`, `src/cli/shell.rs`, `src/cli/system.rs`, `setup::list`, `env_file.rs` (`DEVFLOW_PORT`), a new `ports.rs`, `processes.rs` and services (up/down), GUI settings and list; tests; docs.

## Wave 3 (after wave 2 is merged)

### W3-A `review/config-trust`: each workspace runs its own branch's recipes

Today `devflow.toml` is read from the main checkout's working copy (`project.rs`) while pitchfork files come from each worktree, and `trust.toml` keeps one approval per checkout, so worktrees whose files differ revoke each other's approval (reproduced) and a branch cannot try its own hooks.

1. **Effective configuration.** Each operation resolves one immutable effective configuration and keeps it for its whole run, with its sources and digest:
   - from the main checkout (stable, shared): `[project]`, the `[services.*]` sections (engines, images, modes, ports, admin settings). A branch never reconfigures a shared server or acquires a resource by renaming a service;
   - from the target worktree (versioned with the code): `[hooks]`, `[env]` recipes and presets, `[worktree] copy`, and the pitchfork files (as today);
   - `devflow.local.toml` of the main checkout overlays both, as today (checkout-wide).
2. **A branch without the recipes.** A worktree whose `devflow.toml` is missing or invalid is an explicit condition: `needs_attention` with a typed problem and the remedy "use the default workspace's recipes for this workspace", an explicit choice recorded in the receipt. devflow never silently borrows the main checkout's hooks.
3. **Approvals per digest.** `trust.toml` keeps several approved digests per checkout, each with its scope version and date; an approval adds one, `trust --revoke` removes the current scope's, `--revoke-all` every one of the project; identical content is approved once. Old entries are pruned beyond a bound (oldest first, never the current scope).
4. **Generations** follow the effective configuration: a change to a branch's own hooks reopens that workspace only.
5. **Frontends.** `trust`, `doctor`, `env list`, `hooks list` and the GUI show which file each part comes from. The GUI Setup tab edits the main checkout's files and says that branches carry their own copy; a workspace's detail shows its effective recipes.

Files: `project.rs`, `config.rs`, `scope.rs`, `trust.rs`, `setup*`, `env_file*`, `hooks.rs`, CLI `trust`/`hooks`/`env`, GUI Setup tab and detail; tests (two worktrees with different hooks and pitchfork files both approved; a branch without `devflow.toml`); docs.

### W3-B `review/native-checkout-sqlite`: the checkouts libgit2 cannot make, and SQLite branches

1. **Native checkout, opt-in.** `[worktree] checkout = "git"` is accepted only in the machine-local `devflow.local.toml` (a repository cannot opt anyone in). Choosing it authorizes Git's own execution (filters such as LFS, hooks, helpers), which `devflow trust` does not cover; the choice is reported in plans and results.
   - devflow resolves the start point itself (the W1-C rules) and runs `git worktree add` with structured arguments, a scrubbed environment (no `GIT_*` from the caller, no workspace variables, `GIT_TERMINAL_PROMPT=0`) and `DEVFLOW_SETUP_ACTIVE=1` so devflow's own post-checkout hook does not re-enter; devflow's setup runs once afterwards.
   - Git missing is an actionable error. A failure never falls back to libgit2: devflow reports the branch and worktree that survive and resumes by identity.
   - With the native backend, LFS and partial-clone repositories are allowed (W1-C refuses them otherwise).
2. **SQLite engine.** `[services.<key>] engine = "sqlite"` with `path` (relative to the worktree) and `env` (default `DATABASE_URL`, or `SQLITE_URL` beside another engine claiming it); exported as an absolute path (and `sqlite:` URL form where frameworks expect it). A workspace's database is a consistent snapshot of the source workspace's file: checkpoint or backup through SQLite's own API (choose the dependency deliberately and state its cost: `rusqlite` with the bundled library versus a `sqlite3` binary), then a reflink of the result; `copy` when the filesystem cannot share blocks, reported like every engine; `--data-from` and snapshots as for the others where feasible, stated otherwise. A file the source holds locked for writing is waited for within a bound, never copied torn.

Files: `worktree.rs` (backend selection), a new `worktree/native.rs`, `config.rs`, a new `services/sqlite.rs` (or `db_sqlite.rs`), `setup.rs` bindings, env names, docs, tests (native checkout of an LFS-marked repo when `git` is present; SQLite clone consistency under a concurrent writer).

### W3-C `review/suspend-focus`: rest idle workspaces, and one stable address

1. **Suspend, opt-in.** `devflow suspend [NAME | --idle DURATION] [--yes]` stops a workspace's own containers and daemons (never the shared PostgreSQL server; volumes kept) and records it; `devflow switch NAME` (activation) resumes it. `devflow pin NAME` / `unpin` protects a workspace from `--idle`. Last activity comes from activation plus a cheap per-workspace touch by `devflow exec`. Nothing is ever suspended automatically; `list --wide` and the GUI show suspended and pinned workspaces, and the GUI offers Suspend/Resume/Pin.
2. **Focus, opt-in.** `devflow focus NAME [--port PORT]…` forwards a canonical loopback port (default the project's `[worktree] focus_ports`, e.g. 3000) to the focused workspace's app port (`DEVFLOW_PORT`) or a named daemon's port, through a small devflow forwarder process (pid and state in the data dir, one per project, bounded and logged); `devflow focus` shows it, `devflow focus --stop` ends it, switching focus re-points it. It is per user, never implied by `switch`, and parallel agents keep their own ports. Documented for OAuth callbacks and cookies; TCP only (a PostgreSQL database name cannot be re-pointed by a forward).

Files: CLI `suspend`, `resume`, `pin`, `focus`; core `services/stop.rs`/`processes.rs` reuse; a new `focus.rs` (forwarder); receipt fields (`suspended_at`, `pinned`); GUI; tests; docs.

## Wave 4

### W4-A `review/docs-schema`: smaller documents, a schema, a leaner init

1. **README** around 12 KB: what it does and why, install, quick start, the daily commands (`switch`, `list`, `exec`, `remove`, `gc`, `integrate`), a minimal configuration, and links. The detail moves to `docs/guide/` (configuration, data services per engine, hooks and environment, processes, agents and hosts, remote data hosts, troubleshooting).
2. **AGENTS.md** becomes a short contract (JSON envelope, exit codes, remedies, the agent flow, the host integrations), around 5 KB, with `docs/agents-reference.md` holding the rest.
3. **JSON Schema** for `devflow.toml`: generated from the configuration types (or maintained beside them with a test that keeps them equal), printed by `devflow config schema`, committed as `docs/devflow.schema.json`, with the editor hint documented.
4. **Supported versions.** pitchfork and mise minimum versions pinned in one place, checked by `doctor`; compatibility branches for older versions removed.
5. **Init engine.** Remove detection that feeds no proposal; keep plan/apply safety, conflicts and reviewed content. Report the lines before and after.
6. ARCHITECTURE.md, CHANGELOG (curated `## Unreleased`), CONTRIBUTING and `docs/MIGRATING.md` describe the final state.

## Integration

The lead rebases each branch onto the integration branch, merges with `--ff-only`, runs every check (GUI and Docker suites included) and runs GPT-6-Astra review rounds (per workstream for W1-B, W1-D1, W1-D2, W2-A and W3-A; per wave otherwise) until no high or medium finding remains or three rounds have passed.
