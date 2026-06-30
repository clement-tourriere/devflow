---
title: TUI dashboard
description: The interactive terminal dashboard — workspaces, services, proxy, system info, and logs without leaving the terminal.
sidebar:
  order: 13
---

```bash
devflow tui
```

:::caution
Run the TUI **without** the shell-integration wrapper (`command devflow tui`) — the wrapper captures output and breaks full-screen rendering. See [shell integration caveats](/devflow/getting-started/shell-integration/#caveats).
:::

## Tabs

| Tab | Shows | Key actions |
| --- | --- | --- |
| **Workspaces** | workspace tree with parent/child relationships, service states, worktree paths | switch workspace, start/stop services, `o` to open a workspace |
| **Services** | configured services, provider state, capability matrix | inspect inventory and provider support |
| **Proxy** | proxy status, CA trust state, discovered endpoints | check proxy health and routes |
| **System** | config overview, hook list with template-variable reference and scaffold snippets, doctor diagnostics | browse hooks, view template context, health check |
| **Logs** | service container logs | pick workspace/service, filter, scroll |

## Navigation

- `Tab` / `Shift+Tab` — switch tabs
- Arrow keys — move within a tab
- `Enter` — select / confirm
- `o` — open the selected workspace: exits the TUI, switches to it (creating it if needed), and prints the worktree path — `cd` there to continue
- `q` / `Esc` — quit

Switching workspaces from the TUI runs the same core lifecycle as `devflow switch` — services follow and hooks fire.
