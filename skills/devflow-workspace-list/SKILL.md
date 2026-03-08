---
name: devflow-workspace-list
description: List all devflow workspaces with their status, services, and worktree paths.
---

## When to use

- You need to see which workspaces exist in the project
- You want to check service statuses across workspaces
- You need to find a workspace's worktree path before navigating to it
- You want to verify workspace state after creating or switching

## Instructions

1. Run `devflow --json list` to get structured workspace data
2. Parse the JSON array — each object contains:
   - `name` — workspace identifier
   - `is_current` — boolean, whether this is the active workspace
   - `is_default` — boolean, whether this is the default (main) workspace
   - `worktree_path` — filesystem path to the worktree directory (if any)
   - `parent` — parent workspace name
   - `services` — array of service objects with `name`, `status`, `service_type`
3. Present the results clearly, highlighting the current workspace

Use `devflow list` (without `--json`) for human-readable output when not parsing programmatically.

## Examples

List all workspaces as JSON:

```bash
devflow --json list
```

List workspaces in human-readable format:

```bash
devflow list
```

Check which workspace is currently active:

```bash
devflow --json list | jq '.[] | select(.is_current) | .name'
```
