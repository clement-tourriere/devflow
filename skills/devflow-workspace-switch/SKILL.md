---
name: devflow-workspace-switch
description: Switch to an existing devflow workspace.
---

## When to use

- You need to change the active workspace to work on a different task

## Instructions

1. Run `devflow --json --non-interactive switch $ARGUMENTS`
2. If the JSON output has `worktree_path`, use it as the working directory for subsequent tool calls
3. Run `devflow --json connection $ARGUMENTS` to get service connection strings
