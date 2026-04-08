---
name: devflow-workspace-create
description: Create a new devflow workspace with isolated services for a task or feature.
---

## When to use

- You are starting work on a new task or feature that needs isolated services
- You need a fresh database or cache instance that won't affect other workspaces
- You want to set up a parallel development environment with its own worktree

## Instructions

1. The workspace name is provided in `$ARGUMENTS`
2. Run `devflow --json --non-interactive switch -c --sandboxed $ARGUMENTS` to create and switch
   - The `-c` flag creates the workspace if it doesn't exist
   - The `--sandboxed` flag enables sandbox mode — restricting filesystem access and blocking dangerous commands (git push, npm publish, sudo, ssh, etc.)
   - This provisions isolated service instances (databases, caches) automatically
   - If worktrees are enabled, a new Git worktree directory is created
   - Lifecycle hooks (e.g. `post-create`, `post-switch`) run automatically
3. **Parse the JSON output** to check for `worktree_path`:
   - If `worktree_path` is present, use it as the working directory/workdir for subsequent tool calls
   - Do not rely on shell `cd` to retarget an already running agent session
   - If `worktree_created` is `true`, a new worktree was just created for this workspace
4. If the project has database services, retrieve connection info with `devflow --json connection $ARGUMENTS`
   - If this returns `"services": "none_configured"`, the project uses workspaces without database services — skip this step
5. Report the new workspace details including service connection strings to the user

Use a descriptive name like `feature/auth-refactor` or `agent/task-123` for the workspace.

**Important**: Do NOT use `--no-verify` — it skips all lifecycle hooks (migrations, env setup, etc.) which are usually needed for a working environment.

### Sandbox mode

By default, always use `--sandboxed` when creating workspaces. This provides OS-level filesystem isolation and blocks dangerous commands.

If the user explicitly asks you to create a workspace **without** sandbox restrictions (e.g. they need `git push` access or SSH), use `--no-sandbox` instead:

```bash
devflow --json --non-interactive switch -c --no-sandbox $ARGUMENTS
```

## Examples

Create a new sandboxed workspace for a feature:

```bash
OUTPUT=$(devflow --json --non-interactive switch -c --sandboxed feature/my-task)

# For agents, use WORKTREE as the workdir for later tool calls
WORKTREE=$(echo "$OUTPUT" | jq -r '.worktree_path // empty')
```

Get connection strings for the new workspace:

```bash
devflow --json connection feature/my-task
```

Create a workspace and run a command in it:

```bash
devflow switch -c --sandboxed agent/task-42 -x claude -- 'Implement the auth feature'
```

Create a workspace and immediately get full context:

```bash
OUTPUT=$(devflow --json --non-interactive switch -c --sandboxed agent/task-42)
WORKTREE=$(echo "$OUTPUT" | jq -r '.worktree_path // empty')
# For agents, use WORKTREE as the workdir for later tool calls
devflow --json connection agent/task-42
devflow agent context
```
