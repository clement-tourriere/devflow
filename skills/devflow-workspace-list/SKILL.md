---
name: devflow-workspace-list
description: List devflow workspaces with status and services.
---

## When to use

- You need to see existing workspaces or check service status

## Instructions

1. Run `devflow --json list` to get workspace data
2. Parse the JSON array of `{ name, is_current, is_default, worktree_path, services }`
