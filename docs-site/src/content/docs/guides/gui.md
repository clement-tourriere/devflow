---
title: Desktop GUI
description: The Tauri-based desktop app — projects, workspaces, services, hooks, proxy, and configuration with a system tray.
sidebar:
  order: 11
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
- **Workspaces** — create, switch, and delete workspaces; see worktree paths (with a *git worktree* badge), parent relationships, sandbox flags, and connection info. Creation lets you pick **branch or worktree mode** per workspace (defaulting from the project's config) and override copied files / `copy_ignored` for worktree creations.
- **Services** — start/stop/reset instances, view logs, health diagnostics.
- **Hooks** — three-panel editor: phase list, hook entries with run/edit/delete, and a live MiniJinja preview with a variable browser.
- **Proxy** — status, discovered containers with HTTPS links and native database endpoints, one-click CA trust management, filtering by domain/name/project.
- **Config editor** — section-based forms (General, Git, Worktree, Services, Hooks, Agent, Commit) with validation before save, plus a raw-YAML mode.
- **Doctor** — diagnostics page mirroring `devflow doctor`.
- **Prune worktrees** — one click to clear stale worktree metadata.

GUI-initiated actions skip hook approval prompts (clicking the button *is* the approval) and tag hook context with `trigger_source: "gui"` so hooks can react conditionally (`condition: "trigger_is:gui"`).

:::note
Saving from the form editor re-serializes `.devflow.yml` — hand-written comments and key ordering are not preserved. Use the raw-YAML mode when you care about file layout.
:::

## System tray

The app lives in the system tray with quick access to: dashboard toggle, proxy start/stop, the project list with per-workspace shortcuts, and workspace activation (worktree indicator included).
