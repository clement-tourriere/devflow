---
name: devflow-brainstorming
description: "Use before any creative work — creating features, building components, adding functionality, or modifying behavior. Explores user intent, requirements and design through collaborative dialogue, then transitions to implementation in an isolated devflow workspace."
---

# Brainstorming Ideas Into Designs

## Overview

Help turn ideas into fully formed designs and specs through natural collaborative dialogue.

Start by understanding the current project context, then ask questions one at a time to refine the idea. Once you understand what you're building, present the design, get user approval, then create an isolated devflow workspace for implementation.

**HARD GATE**: Do NOT write any code, scaffold any project, or take any implementation action until you have presented a design and the user has approved it. This applies to EVERY task regardless of perceived simplicity.

## Anti-Pattern: "This Is Too Simple To Need A Design"

Every project goes through this process. A todo list, a single-function utility, a config change — all of them. "Simple" projects are where unexamined assumptions cause the most wasted work. The design can be short (a few sentences for truly simple tasks), but you MUST present it and get approval.

## Checklist

Complete these steps in order:

1. **Explore project context** — check files, docs, recent commits, `devflow status`
2. **Ask clarifying questions** — one at a time, understand purpose/constraints/success criteria
3. **Propose 2-3 approaches** — with trade-offs and your recommendation
4. **Present design** — in sections scaled to complexity, get user approval after each section
5. **Write design doc** — save to `docs/plans/YYYY-MM-DD-<topic>-design.md`
6. **Create devflow workspace** — create an isolated workspace for implementation (see below)
7. **Write implementation plan** — break the approved design into concrete tasks

## The Process

### Understanding the idea

- Check out the current project state first (files, docs, recent commits)
- Run `devflow status` and `devflow list` to understand the workspace context
- Ask questions one at a time to refine the idea
- Prefer multiple choice questions when possible, but open-ended is fine too
- Only one question per message — if a topic needs more exploration, break it into multiple questions
- Focus on understanding: purpose, constraints, success criteria

### Exploring approaches

- Propose 2-3 different approaches with trade-offs
- Present options conversationally with your recommendation and reasoning
- Lead with your recommended option and explain why

### Presenting the design

- Once you believe you understand what you're building, present the design
- Scale each section to its complexity: a few sentences if straightforward, up to 200-300 words if nuanced
- Ask after each section whether it looks right so far
- Cover: architecture, components, data flow, error handling, testing
- Be ready to go back and clarify if something doesn't make sense

### Transition to implementation

After the user approves the design:

1. Save the design to `docs/plans/YYYY-MM-DD-<topic>-design.md`
2. Create an isolated devflow workspace for the implementation:

```bash
OUTPUT=$(devflow --json --non-interactive switch -c --sandboxed feature/<topic>)
WORKTREE=$(echo "$OUTPUT" | jq -r '.worktree_path // empty')
[ -n "$WORKTREE" ] && cd "$WORKTREE"
```

3. Write the implementation plan as `docs/plans/YYYY-MM-DD-<topic>-plan.md` in the new workspace
4. Begin implementation in the isolated workspace — changes are contained and won't affect other work

## Key Principles

- **One question at a time** — Don't overwhelm with multiple questions
- **Multiple choice preferred** — Easier to answer than open-ended when possible
- **YAGNI ruthlessly** — Remove unnecessary features from all designs
- **Explore alternatives** — Always propose 2-3 approaches before settling
- **Incremental validation** — Present design, get approval before moving on
- **Isolate work** — Use devflow workspaces so implementation doesn't interfere with ongoing work
