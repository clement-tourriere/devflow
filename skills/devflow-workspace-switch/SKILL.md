---
name: devflow-workspace-switch
description: Switch to an existing devflow workspace and its isolated services.
---

## When to use

- You need to change the active workspace to work on a different task
- You want to switch services (databases, caches) to match a specific workspace
- After listing workspaces, you want to activate one of them

## Instructions

1. The workspace name is provided in `$ARGUMENTS`
2. Run `devflow --json --non-interactive switch $ARGUMENTS` to switch
3. Parse the JSON output and check for `worktree_path` — if present, change your working directory to it
4. Verify the switch succeeded with `devflow status`
5. If the workspace has services, retrieve connection info with `devflow --json connection $ARGUMENTS`
   - If this returns `"services": "none_configured"`, the project uses workspaces without database services — skip this step
6. Report the new workspace state and any connection strings to the user

Always use `--json --non-interactive` when running as an agent. Do NOT use `--no-verify` — it skips all lifecycle hooks (e.g. migrations, env setup) which are usually needed.

## Examples

Switch to an existing workspace:

```bash
OUTPUT=$(devflow --json --non-interactive switch my-feature)
WORKTREE=$(echo "$OUTPUT" | jq -r '.worktree_path // empty')
[ -n "$WORKTREE" ] && cd "$WORKTREE"
```

Verify the switch and get connection info:

```bash
devflow status
devflow --json connection my-feature
```
