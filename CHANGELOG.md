# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

devflow 1.0 is a breaking simplification. It keeps Git worktrees, copy-on-write payloads, PostgreSQL branches and trusted setup hooks, adds a GPUI desktop app, and delegates processes and the proxy to pitchfork and toolchains and dependencies to mise. There is no in-place upgrade: read [docs/MIGRATING.md](docs/MIGRATING.md). The design is in [docs/plans/2026-10-10-devflow-1.0.md](docs/plans/2026-10-10-devflow-1.0.md).

### Added

- **Postgres branches** — One PostgreSQL 18 server per project (managed container or external). Each workspace database is cloned with `CREATE DATABASE … TEMPLATE … STRATEGY FILE_COPY` and `file_copy_method = clone`, so on btrfs, XFS (reflink), ZFS 2.2+ and OrbStack volumes it shares blocks with its source. CoW is measured and reported as `verified`, `copy` or `unknown`; `require_cow` refuses branches otherwise.
- **Snapshots** — Versioned, immutable `db snapshot` seeds selectable with `--data-from snapshot:NAME[@vN]`; `db reset`, `db reconcile` and explicit `db branch` management.
- **Role isolation** — Branch logins connect to their own database only, behind a per-project owner role and a metadata database; a non-superuser `CREATEDB CREATEROLE` provisioner is enough.
- **Receipts and resumable setup** — Each worktree records its phases (materialize, payload, database, env, hooks). `devflow switch` and `devflow setup` resume at the first incomplete phase; status is `ready`, `needs_attention` or `in_progress`, and exit code 2 means the workspace is not `ready`.
- **Trusted hooks** — `[hooks]` `setup`, `switch` and `teardown` shell commands gated by `devflow trust` (SHA-256 of the section) or `DEVFLOW_TRUST_HOOKS=1`.
- **Worktrees from other tools** — `devflow hook install` adds a `post-checkout` hook that runs `devflow setup --if-new` for `git worktree add`, T3 Code, Claude Code and fresh clones.
- **Remote data hosts** — External mode, `app_host`, `DOCKER_CONTEXT` for a managed server on a remote engine, `setup --database-url`, and `setup --new-instance` for VM forks. See [docs/REMOTE.md](docs/REMOTE.md).
- **`.devflow/env`** — Generated `DATABASE_URL`, `PG*` and `DEVFLOW_*` variables, loadable with mise `[env] _.file`.
- **Desktop app** — `devflow-gui`, built on GPUI and `gpui-kit`, with projects, workspaces, database branches and snapshots, pitchfork daemons, a config editor and setup logs.
- **`devflow doctor`** reports the CoW capability and whether it was measured or declared.
- **Shell integration** — `devflow shell-init` (a `cd`-ing `switch`), `devflow completions SHELL`, and `devflow gui` to open the desktop app.
- Examples for `devflow.toml`, external mode, mise, pitchfork and an agent task script.

### Changed

- **Configuration is TOML only** — `devflow.toml` (or `.devflow.toml`) plus a git-ignored `devflow.local.toml`. 0.x files stop with an error that links the migration guide.
- **Data and code origins** — `--from` (code) and `--data-from` (workspace, `snapshot:NAME[@vN]`, `branch:ID` or `empty`) are independent and immutable once chosen.
- **Removal** — `devflow remove` checks every protection before any effect, runs `teardown`, drops only branches this instance created, and deletes the Git branch only with `--delete-branch`.
- **Output** — One versioned JSON document per command; passwords are redacted except in `db url` and `db branch create`.
- **Native, synchronous core** — `devflow-core` has a blocking API (nothing async is public) and no GPUI types.
- **Releases** — Binaries ship `devflow` only; the desktop app is built from source. CI builds the GUI on macOS and runs the Docker database tests on Linux.

### Removed

- Process supervisor, controller daemon, execution records and the operation journal: use pitchfork and `devflow setup` / `db reconcile`.
- The HTTPS proxy crate, local CA and `devflow-proxy` binary: use pitchfork's proxy.
- The terminal UI and the terminal crate; the Tauri + React desktop app.
- Jujutsu support.
- MySQL, ClickHouse, Redis, RustFS and generic Docker services, the shared PostgreSQL engine and ZFS driver, Neon/DBLab/Xata and plugin providers.
- Baselines (replaced by snapshots), seeding from URLs, dumps and S3, retention, pins and expiry, artifact caches.
- Hook phases, actions, recipes and MiniJinja templating, hook approvals, `devflow exec`, `switch -x`, the agent guide generator, `devflow gc`, `capabilities` and `plugin`.
- Broad ignored-file and AI-configuration copying; list explicit paths in `worktree.copy`.
- The documentation site, generated `llms.txt` files, integration shell scripts and the `DEVFLOW_INSTALL_PROXY` installer option.

### Known limitations

- **Platforms** — The desktop app is covered by headless tests on macOS 15+; native visual checks are manual. The Linux GUI is untested and unsupported. PostgreSQL 18 or later is required.
- **No TLS** — devflow's own database connections do not use TLS. Remote data hosts need a private network or a tunnel.
- **Copy-on-write depends on the filesystem** — Clones share blocks only on reflink-capable storage. External servers report `unknown` unless the operator declares the capability; sources in other tablespaces report `unknown`.
- **Busy sources** — On a local managed server, cloning from a live workspace terminates the source database's sessions (open transactions are aborted). External and remote servers default to refusing instead.
- **Cloning contract** — Privileges or RLS policies that name a branch login are rejected; use the project owner role. Replication machinery is not cloned. Some cleanup blockers (prepared transactions, replication slots) need operator action.
- **Connections** — Multi-host URLs, `service=`, `sslpassword`, `ssl=`/`requiressl=` aliases and replication connections are refused.
- **Hooks** — Hooks run at least once and must tolerate retries. Trust covers the configured commands, not the scripts they invoke.
- **Redaction** — Credentials devflow knows are masked exactly in hook output and logs; other credentials are masked by pattern on a best-effort basis. Unencoded spaces in URL passwords, `.pgpass` lines, multi-line values and non-PostgreSQL credential formats are not covered (see SECURITY.md).
- **Manual cleanup** — A failed worktree creation leaves its paths for you to inspect; an interrupted metadata bootstrap can leave an unmarked `devflow_bootstrap_*` database that devflow reports but never drops. Copied workspaces (VM forks) need `devflow setup --new-instance` or `DEVFLOW_INSTANCE_ID`.
- **Scope** — Git only; processes and the proxy come from pitchfork; no embedded terminal or tray; migration from 0.x is manual.

## v0.8.0 (2026-09-14)

### Added

- **Workspace plans and desktop recovery** — Inspect code/data selections before effects, retain exact selections on retry, and inspect/resume/cancel eligible operations from shared CLI and desktop APIs.
- **Reusable data** — Versioned, sealed PostgreSQL baselines with recorded data origins, exact engine identities, and dependency-aware removal.
- **Recovery and cleanup** — Stable resource identities, durable operation inspection/resume, project relocation, previewable retention, workspace pins/expiry, and bounded fingerprinted caches.
- **Provider contracts** — Versioned capability descriptors and plugin negotiation, with generated desktop choices and provider documentation.

### Changed

- **Native operations** — Git, jj, Docker, filesystem and ZFS operations use direct libraries. Explicit execution, hooks, terminals and executable providers remain supported.
- **Proxy distribution** — Package the optional standalone companion and checksums; `DEVFLOW_INSTALL_PROXY=1` installs it alongside the standard CLI.
- **Validation labels** — Linux real-project environments (including ZFS/XFS/Btrfs), live Neon/DBLab/Xata accounts and interactive desktop sessions are explicitly **untested**. Automated Linux checks remain release gates.
- **Workspace lifecycle** — CLI, TUI, and desktop share readiness and service operation policy; required background hooks finish before dependent commands, and optional hooks expose completion receipts.
- **Local data safety** — Recoverable ZFS replacement, cancellation-safe database quiescence, and cross-process port reservations protect concurrent workspace creation and reset.
- **Desktop and proxy** — Configuration uses a format-preserving raw editor; the optional proxy runs as a companion with one runtime owner.
- **Automation** — Workspace execution preserves literal arguments, partial provider failures produce non-zero exits, and cleanup retains ownership state when retry is needed.
- **Agent workflow** — `devflow agent skill` now generates harness-specific guide/rule files instead of installing embedded bundled skills; switch/context JSON expose `agent_workdir` for agent tool workdirs.
- **Hooks** — Lifecycle hooks run synchronously by default unless `background: true`; non-blocking phases now describe failure policy rather than background execution.
- **Local PostgreSQL storage** — `local.storage` / `--storage` support `auto`, `zfs`, `apfs_clone`, `reflink`, and `copy`, with clearer doctor diagnostics for configured preferences and a matching GUI Add Service selector.
- **Doctor UX** — Project-level doctor checks are shared across CLI, TUI, and GUI; `devflow --json doctor` now includes general checks and a summary for automation.
- **Agent automation** — `devflow --json agent status` now returns workspace summary counts and per-workspace `agent_workdir` values.
- **GUI setup** — Doctor/Setup shows a health summary, and local PostgreSQL doctor checks include data-root and port-range diagnostics.
- **Release readiness** — Added `devflow doctor --strict`, `devflow service doctor --strict`, and `scripts/release-check.sh` for CI/agent health gates and local release-candidate verification.
- **Actionable diagnostics** — Doctor JSON, GUI, and TUI surfaces now include aggregate summaries and suggested next steps for failed checks.
- **First-run onboarding** — `devflow init` now reports structured onboarding next steps for services, hooks, agent guide generation, strict doctor validation, and first workspace creation.

### Fixed

- Upgrade `h2` to 0.4.16 and `rustls` to 0.23.45 to address RUSTSEC-2026-0258 and RUSTSEC-2026-0285 found by release CI.
- Preserve exact baseline generations and engine images through recreation, nested descendants and reset; reject incompatible existing containers.
- Preserve Git staging/merge state during jj inspection and recover stale jj workspaces without losing local edits.
- Keep failed removal and project destruction recoverable, confine payload writes, and validate destination cache inputs before reuse.
- Record failed/concurrent executions truthfully, enforce controller ownership, and preserve existing project hooks during installation.

### Removed

- **Redundant integrations** — Removed embedded Pitchfork supervision, built-in LLM API commit generation, AI policy merging, inert configuration options, and duplicate desktop configuration forms. Migration warnings retain supported legacy state and resource ownership.
- **Workspace sandbox** — Removed devflow's partial workspace sandbox implementation; use an agent harness, container, VM, or dedicated sandbox platform for hard isolation.

## v0.7.1 (2026-08-09)

### Fix

- address confirmed code-review findings on the consolidation range
- apply round-2 review findings across the new consolidation code
- **test**: serialize env-mutating switch tests on the shared lock

### Refactor

- sweep branch→workspace naming debt
- **services**: extract shared LocalEngineBackend for clickhouse/mysql
- **workspace**: single canonical tree flattening in core
- **cli**: dedup the detach/pidfile pattern into core
- **hooks**: move sync-ai-configs into core, add built-in action
- **state**: finish the _by_dir local-state migration
- **hooks**: drop the decoy configurable triggers section
- sweep dead code, unify duplicated logic

## v0.7.0 (2026-07-18)

### BREAKING CHANGE

- the docker-compose recipe is removed (devflow init
imports compose app services as processes.daemons; data services are
devflow services) and local-dev-setup is renamed to workspace-setup —
both names now return migration messages. --json hook recipes emits
RecipeDetectionInfo[] inside a project, and the install_recipe(s) GUI
IPC now takes params/selections.
- `devflow destroy --json` now emits a uniform
`{name, success, workspaces_destroyed, error}` shape for every entry
in `services` (previously `branches_deleted` for some providers) and
includes `processes_stopped`.

### Feat

- **hooks**: rework recipes into detection-driven generators
- **cli**: add config-validate command
- **sandbox**: enable landlock in default Linux features
- **scripts**: add safety-gated PR auto-merge loop

### Fix

- **workspace**: clear fail-closed dead ends left by the identity refactor
- **processes**: probe wildcard addrs for port availability
- **services**: resolve service workspaces by normalized name
- **project**: stop processes and purge state on destroy
- resolve workspace-creation store deadlock
- bound pitchfork readiness in workspace lifecycle
- **gui**: align Tauri package versions
- **cli**: doctor exits non-zero and replace unreachable! catch-alls
- **proxy**: drop blanket CORS and add configurable bind address
- **services**: bind docker ports to loopback and harden secrets
- **workspace**: validate names and roll back branch on worktree failure
- **hooks**: gate http action and confine file actions to workspace
- **deps**: adapt code for chore(deps): bump git2 from 0.20.2 to 0.21.0 (automated)
- **deps**: adapt code for chore(deps): bump git2 from 0.20.2 to 0.21.0 (automated)
- **mise**: drop redundant cargo tool entry

### Refactor

- **workspace**: split raw VCS names from service keys behind a shared inventory
- focus devflow on workspaces
- **config**: split config/mod.rs into loading and tests submodules
- **cli**: finish splitting workspace.rs into 7 submodules
- **cli**: split workspace.rs into context and exec submodules

### Perf

- **workspace**: speed up CoW workspace creation

## v0.6.0 (2026-06-12)

### Feat

- **cli**: add self-update command and curl installer

### Fix

- **scripts**: drop removed docs/index.html from version sync

## v0.5.0 (2026-06-11)

### Feat

- **proxy**: unify domain suffix to .local on all platforms
- **daemon**: controller daemon keeps shared engines running (G5)
- **services**: reconcile primitive + `devflow service up` (G5 partial)
- **services**: shared Redis logical-isolation provider (G3)
- **services**: shared ClickHouse logical-isolation provider (G6)
- **config**: read lightweight devflow.toml (G4)
- **services**: RustFS shared object-storage provider (G2)
- post-review remediation (A–F) + shared-engine provider (G1)

## v0.4.4 (2026-06-02)

### Fix

- include service storage in top-level status

## v0.4.3 (2026-06-02)

## v0.4.2 (2026-06-02)

## v0.4.1 (2026-05-31)

### Fix

- update vulnerable dependencies

## v0.4.0 (2026-05-29)

### Feat

- **proxy**: advertise .local names via mDNS for host access
- **mise**: add install task for building and installing the CLI

## v0.3.1 (2026-04-08)

### Fix

- **workspace**: surface post-create hook failures

## v0.3.0 (2026-04-03)

### Feat

- **proxy**: add Firefox trust via enterprise policies.json on Linux

### Fix

- handle missing configs
- **proxy**: preserve original Host header and use origin-form URI
- **gui**: pin bun and tauri-cli in mise, fix hanging bun install
- gate HashSet import behind cfg(target_os = "macos") for Linux CI
- resolve remaining clippy errors on Linux CI

## v0.2.0 (2026-03-06)

### Feat

- Initial public release of devflow as a universal development environment tool
- All features listed in 0.1.0 shipped as the first tagged release
