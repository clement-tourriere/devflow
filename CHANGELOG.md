# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0] - 2025-02-27

### Added

- **Multi-service support** — Manage PostgreSQL, ClickHouse, MySQL, generic Docker containers, and plugin backends from a single config.
- **Jujutsu (jj) VCS support** — Auto-detects and supports Jujutsu alongside Git, with colocated repo support.
- **Plugin backend** — Custom backends via JSON-over-stdio protocol (`devflow plugin init <name>` to scaffold).
- **AI commit messages** — `devflow commit --ai` generates commit messages via OpenAI-compatible LLM APIs.
- **Copy-on-Write worktrees** — Worktree directories are cloned using APFS/ZFS/Btrfs/XFS reflinks for near-instant creation.
- **Hook approval system** — Hooks require user approval before first execution; approvals persist across sessions.
- **`devflow capabilities`** — Machine-readable automation contract for AI agents and CI pipelines.
- **`devflow cleanup`** — Remove old service workspaces, keeping the most recent N.
- **`devflow remove`** — Comprehensive cleanup: deletes Git workspace, worktree, and all associated service workspaces.
- **`devflow merge`** — Merge current workspace into target with optional cleanup of source workspace.
- **`devflow seed`** — Seed workspaces from PostgreSQL URLs, local dump files, or S3 objects.
- **`devflow logs`** — Show container logs for local backend workspaces.
- **`devflow config -v`** — Show effective configuration with precedence details.
- **Shell integration** — `eval "$(devflow shell-init)"` for automatic `cd` into worktrees after switch.
- **Docker Compose auto-detection** — `devflow init` reads `docker-compose.yml` to pre-fill PostgreSQL config.
- **Workspace filter regex** — `git.workspace_filter_regex` and `DEVFLOW_BRANCH_FILTER_REGEX` to limit which workspaces get service environments.
- **`devflow switch --execute`** — Run a command after switching workspaces.
- **`devflow switch --dry-run`** — Simulate switching without performing operations.
- **15 hook lifecycle phases** — Including `pre-service-create`, `post-service-create`, `pre-service-delete`, `post-service-delete`, `post-service-switch`.
- **MiniJinja template engine** for hooks with custom filters: `sanitize`, `sanitize_db`, `hash_port`.
- **Three-tier configuration** — `.devflow.yml` (team) -> `.devflow.local.yml` (local) -> environment variables.
- **`llms.txt` and `llms-full.txt`** — LLM-friendly documentation for AI agent ingestion.

### Changed

- Renamed from `pgbranch` to `devflow` to reflect multi-service scope.
- Backend configuration moved from flat fields to named `backends` array for multi-service support.
- State storage moved from JSON to SQLite for local backend.
- User-level state moved to `~/.local/share/devflow/` (XDG-compliant).

## [0.2.0] - 2025-01-15

### Added

- **Local Docker backend** — Docker containers with CoW storage (APFS clones, ZFS snapshots, reflinks).
- **Template backend** — PostgreSQL `CREATE DATABASE ... WITH TEMPLATE` for server-side branching.
- **Cloud backends** — Neon, DBLab, and Xata API integration.
- **Git hook integration** — `post-checkout`, `post-merge`, `pre-commit`, `post-rewrite` hooks.
- **ZFS setup** — `devflow setup-zfs` for file-backed ZFS pool creation on Linux.
- **Basic lifecycle hooks** — `post-create`, `post-switch`, `pre-merge` command execution.
- **`devflow doctor`** — System health diagnostics.
- **`devflow status`** — Project and backend status display.
- **`devflow connection`** — Output formats: URI, env, JSON.

## [0.1.0] - 2024-12-01

### Added

- Initial release as `pgbranch`.
- Single PostgreSQL backend with Docker container management.
- Basic workspace create/delete/switch lifecycle.
- Git hook installation for automatic branching on checkout.
