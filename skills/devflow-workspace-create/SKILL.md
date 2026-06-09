---
name: devflow-workspace-create
description: Create a new devflow workspace with isolated services.
---

## When to use

- Starting work on a new task that needs isolated services

## Instructions

1. Run `devflow --json --non-interactive switch -c --sandboxed $ARGUMENTS` to create and switch
2. If the JSON output has `worktree_path`, use it as the working directory for subsequent tool calls
3. Run `devflow --json connection $ARGUMENTS` to get service connection strings
