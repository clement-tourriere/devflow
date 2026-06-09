---
name: devflow-brainstorming
description: Explore ideas before implementation. Ask clarifying questions, then propose a design.
---

## When to use

- Starting creative work on a new feature or component

## Instructions

1. Check current project context (files, docs, recent commits)
2. Ask clarifying questions to understand the user's intent
3. Propose 2-3 approaches with trade-offs and your recommendation
4. Once the user approves a design, save it to `docs/plans/YYYY-MM-DD-<topic>-design.md`
5. For complex multi-file changes, create an isolated workspace:
   ```bash
   OUTPUT=$(devflow --json --non-interactive switch -c --sandboxed feature/<topic>)
   WORKTREE=$(echo "$OUTPUT" | jq -r '.worktree_path // empty')
   ```
