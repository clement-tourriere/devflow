---
title: Desktop GUI
description: The Tauri-based desktop app — projects, workspaces, services, hooks, proxy, and configuration with a system tray.
sidebar:
  order: 12
---

A native desktop application (Tauri 2 + React) for workspaces, services, processes and configuration. Lifecycle requests use the same core application operations as the CLI.

## Run it

```bash
mise run gui           # development mode with hot-reload
mise run gui:build     # production bundle
```

Requires `bun` and the Tauri prerequisites.

## What's inside

- **Dashboard** — projects at a glance: configuration status, workspace/service counts, proxy state.
- **Workspaces** — one collapsible parent tree shared with CLI/TUI inventory, including raw names, paths, missing-parent state, service/process health, and connection info. Git creation always materializes a linked worktree (jj uses a native workspace); copied files and `copy_ignored` can still be overridden. Delete runs a safety preflight, and force deletion requires a separate second confirmation.
- **Services** — inspect recorded data origins and supported operations, start/stop/reset instances, view logs and health diagnostics. Provider choices come from the core capability catalog; unavailable builds and providers needing credentials direct you to configuration setup.
- **Processes** — select a workspace, inspect readiness and runtime state, start/stop/restart processes, view logs and follow proxy URLs.
- **Hooks** — inspect configured phases and actions, browse template variables and preview rendered templates. Edit hooks in the configuration document.
- **Proxy** — status, discovered containers with HTTPS links and native database endpoints, one-click CA trust management, filtering by domain/name/project.
- **Config editor** — edit the project YAML or TOML document with validation before saving. The original format, comments and key ordering are retained.
- **Doctor** — diagnostics page mirroring `devflow doctor`.
- **Prune worktrees** — one click to clear stale worktree metadata.

GUI-initiated actions skip hook approval prompts (clicking the button *is* the approval) and tag hook context with `trigger_source: "gui"` so hooks can react conditionally (`condition: "trigger_is:gui"`).

The proxy controls use the optional `devflow-proxy` companion. Install it on `PATH`; the desktop does not embed another proxy runtime. Proxy settings live in the shared global devflow configuration.

## System tray

The app lives in the system tray with quick access to: dashboard toggle, proxy start/stop, the project list with per-workspace shortcuts, and workspace activation (worktree indicator included).
