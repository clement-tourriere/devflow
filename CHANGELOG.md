# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- **Workspace creation freeze (GUI, and CLI after a repo move)** — `LocalProvider::ensure_project` self-deadlocked on its store mutex whenever the stored `project_path` mismatched the computed one (the `if let` scrutinee held the `MutexGuard` across the second `store()` lock). The GUI always mismatched because Finder launches apps with cwd `/`, freezing every db-service operation and leaving "Creating..." stuck forever. Regression-tested.
- **Project identity no longer derives from process cwd** — `Config` now carries `project_root` (set at load time); project naming and orphan-detection paths use it instead of `std::env::current_dir()`, which was wrong in the GUI/daemon.
- **GUI lifecycle commands are bounded** — create/switch time out after 5 minutes (delete already had a bound), and the frontend mirrors the timeouts, so a wedged backend surfaces as an error instead of a stuck dialog.
- **Tray updates no longer hold a lock across main-thread dispatch** — the tray handle is cloned out of its mutex before `set_menu`/`set_tooltip`.

### Changed

- **Single source of truth across CLI/TUI/GUI** — new core `Config::load_effective_for_dir` (full committed+global+local+env merge for a project dir) and `Config::overlay_local_state_services` (merge-by-name; fixes CLI/TUI wholesale-replace of committed services); GUI/daemon/factory/processes all use them. Workspace-list enrichment unified in `workspace::list::enriched_workspaces` (GUI uses it; a byte-identical CLI copy in `service.rs` was deleted). Delete safety checks (refuse main / currently-checked-out workspace) moved into core `delete_workspace`. GUI/TUI creates route through core `create_workspace`.
- **Fewer subprocesses** — `git worktree prune`, staged diff/summary now use git2 (also from the GUI); `id -u/-g` and `ps -o pgid=` replaced with `nix` calls; the `http` hook action uses reqwest instead of `curl`; the `docker-exec` hook action uses the Docker Engine API (bollard) with a CLI fallback.

### Removed

- Unused dependencies: `async-trait`, `rpassword` (root), `rpassword` (core), `log` (devflow-terminal), `chrono` (devflow-proxy, devflow-app).
- Dead code: 14 unused functions across config/state/vcs/hooks/services, stale `#[allow(dead_code)]` attributes, and 5 unused GUI IPC commands (`add_project`, `init_project`, `get_service_status`, `validate_hook`, `preview_hook`) with their frontend wrappers.

## [0.1.0] - 2026-03-06

### Added

- **CLI** — Full command-line interface: `init`, `switch`, `list`, `graph`, `link`, `remove`, `cleanup`, `status`, `doctor`, `capabilities`, `gc`.
- **Multi-service support** — PostgreSQL, ClickHouse, MySQL, generic Docker containers, and plugin backends from a single config.
- **Local Docker backend** — Docker containers with CoW storage (APFS clones, ZFS snapshots, Btrfs/XFS reflinks).
- **Template backend** — PostgreSQL `CREATE DATABASE ... WITH TEMPLATE` for server-side branching.
- **Cloud backends** — Neon, DBLab, and Xata API integration.
- **Plugin backend** — Custom backends via JSON-over-stdio protocol.
- **Git worktree management** — Creates worktree directories with configurable path templates and file copying.
- **Jujutsu (jj) VCS support** — Auto-detects and supports Jujutsu alongside Git.
- **Git hook integration** — Auto-creates/switches service workspaces on `git checkout` via installed hooks.
- **Hook engine** — MiniJinja-templated lifecycle hooks with approval system and built-in recipes.
- **AI tool config sync** — Auto-copies `.claude/`, `.cursor/`, `.opencode/`, `.agents/` into worktrees; `sync-ai-configs` merges back.
- **AI commit messages** — `devflow commit --ai` generates commit messages via LLM (CLI-first, API fallback).
- **AI agent integration** — `devflow agent status/context/skill` for managing AI coding agents in isolated workspaces; launch agents with `devflow switch -c <workspace> -x <command>`.
- **Native reverse proxy** — Auto-discovers Docker containers and serves them via HTTPS `*.local` domains with auto-generated certificates.
- **Desktop GUI** — Tauri 2 desktop app with React frontend for managing projects, workspaces, services, hooks, proxy, and configuration.
- **TUI** — Ratatui-based terminal dashboard.
- **Seed support** — Seed databases from PostgreSQL URLs, local dump files, or S3.
- **Shell integration** — `eval "$(devflow shell-init)"` for automatic `cd` into worktrees.
- **Multiplexer support** — Terminal multiplexer integration (tmux, zellij) with `--open` flag.
- **Three-tier configuration** — `.devflow.yml` (team) -> `.devflow.local.yml` (local) -> environment variables.
- **JSON output + non-interactive mode** — For CI/CD and AI agent workflows.

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
