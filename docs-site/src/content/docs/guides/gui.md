---
title: Desktop GUI
description: The Tauri-based desktop app — projects, workspaces, services, hooks, proxy, and configuration with a system tray.
sidebar:
  order: 12
---

A native desktop application (Tauri 2 + React) for managing everything devflow does — sharing the same core as the CLI, so hooks, services, and state behave identically.

## Run it

```bash
mise run gui           # development mode with hot-reload
mise run gui:build     # production bundle
```

Requires `bun` and the Tauri prerequisites.

## What's inside

- **Dashboard** — projects at a glance: configuration status, workspace/service counts, proxy state.
- **Workspaces** — one collapsible parent tree shared with CLI/TUI inventory, including raw names, paths, missing-parent state, service/process health, and connection info. Git creation always materializes a linked worktree (jj uses a native workspace); copied files and `copy_ignored` can still be overridden. Delete runs a safety preflight, and force deletion requires a separate second confirmation.
- **Services** — start/stop/reset instances, view logs, health diagnostics.
- **Processes** — select a workspace, see configured/recorded app processes (including `pitchfork` runtime badges and daemon IDs), start/stop/restart/force-restart processes, batch-select rows, open logs, follow proxy URLs, and open Pitchfork Web UI/TUI bridge actions when configured.
- **Hooks** — three-panel editor: phase list, hook entries with run/edit/delete, and a live MiniJinja preview with a variable browser.
- **Proxy** — status, discovered containers with HTTPS links and native database endpoints, one-click CA trust management, filtering by domain/name/project.
- **Config editor** — section-based forms (General, Git, Worktree, Services, Processes, Hooks, Agent, Commit) with validation before save, plus a raw-YAML mode. Worktree settings cover paths/copying rather than an enable/disable mode. The Processes section edits provider, auto-start/stop, Pitchfork reconciliation policy, daemon commands, env templates, readiness, watch, and retry settings.
- **Doctor** — diagnostics page mirroring `devflow doctor`.
- **Prune worktrees** — one click to clear stale worktree metadata.

GUI-initiated actions skip hook approval prompts (clicking the button *is* the approval) and tag hook context with `trigger_source: "gui"` so hooks can react conditionally (`condition: "trigger_is:gui"`).

:::note
Saving from the form editor re-serializes `.devflow.yml` — hand-written comments and key ordering are not preserved. Use the raw-YAML mode when you care about file layout.
:::

## System tray

The app lives in the system tray with quick access to: dashboard toggle, proxy start/stop, the project list with per-workspace shortcuts, and workspace activation (worktree indicator included).
