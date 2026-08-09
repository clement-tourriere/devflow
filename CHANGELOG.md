# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

- devflow — universal development environment tool
