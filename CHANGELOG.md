# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-03-06

### Added

- **CLI** — Full command-line interface: `init`, `switch`, `list`, `graph`, `link`, `remove`, `merge`, `cleanup`, `status`, `doctor`, `capabilities`, `gc`.
- **Multi-service support** — PostgreSQL, ClickHouse, MySQL, generic Docker containers, and plugin backends from a single config.
- **Local Docker backend** — Docker containers with CoW storage (APFS clones, ZFS snapshots, Btrfs/XFS reflinks).
- **Template backend** — PostgreSQL `CREATE DATABASE ... WITH TEMPLATE` for server-side branching.
- **Cloud backends** — Neon, DBLab, and Xata API integration.
- **Plugin backend** — Custom backends via JSON-over-stdio protocol.
- **Git worktree management** — Creates worktree directories with configurable path templates and file copying.
- **Jujutsu (jj) VCS support** — Auto-detects and supports Jujutsu alongside Git.
- **Git hook integration** — Auto-creates/switches service workspaces on `git checkout` via installed hooks.
- **Hook engine** — MiniJinja-templated lifecycle hooks (15 phases) with approval system and built-in recipes.
- **AI tool config sync** — Auto-copies `.claude/`, `.cursor/`, `.opencode/`, `.agents/` into worktrees; `sync-ai-configs` merges back.
- **AI commit messages** — `devflow commit --ai` generates commit messages via LLM (CLI-first, API fallback).
- **AI agent integration** — `devflow agent start/status/context/skill/docs` for managing AI coding agents in isolated workspaces.
- **Native reverse proxy** — Auto-discovers Docker containers and serves them via HTTPS `*.localhost` domains with auto-generated certificates.
- **Desktop GUI** — Tauri 2 desktop app with React frontend for managing projects, workspaces, services, hooks, proxy, and configuration.
- **TUI** — Ratatui-based terminal dashboard.
- **Seed support** — Seed databases from PostgreSQL URLs, local dump files, or S3.
- **Shell integration** — `eval "$(devflow shell-init)"` for automatic `cd` into worktrees.
- **Smart merge system** — Per-project merge configuration with workspace cleanup.
- **Workspace sandbox** — OS-level isolation for workspace processes.
- **Multiplexer support** — Terminal multiplexer integration (tmux, zellij) with `--open` flag.
- **Three-tier configuration** — `.devflow.yml` (team) -> `.devflow.local.yml` (local) -> environment variables.
- **JSON output + non-interactive mode** — For CI/CD and AI agent workflows.

## v0.2.0 (2026-03-06)

### Feat

- devflow — universal development environment tool
