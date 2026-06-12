---
title: Merging & merge trains
description: Merge and rebase workspaces with readiness checks, cascade reports, and a queue-based merge train.
sidebar:
  order: 8
---

devflow wraps merge/rebase with workspace awareness: merges run in the target's worktree when it has one, cleanup removes the whole workspace (branch + worktree + services), and the **Smart Merge** feature set adds readiness checks, cascade analysis, and merge trains.

## Basic merge

```bash
devflow merge                  # current workspace → main
devflow merge develop          # explicit target
devflow merge --dry-run        # print the plan
devflow merge --cleanup        # delete source branch + worktree + services after
```

When the target workspace has a dedicated worktree, the merge executes **there** — your current checkout isn't disturbed. `pre-merge` hooks (tests, lint) run before, `post-merge` hooks after; a failing blocking hook aborts the merge.

Cleanup is safe by default: a source worktree with uncommitted changes is never deleted as a side effect — commit/stash, or `devflow remove <ws> --force` explicitly. Run `merge --cleanup` from outside the worktree being deleted.

## Rebase

```bash
devflow rebase                 # current workspace onto main
devflow rebase develop
devflow rebase --dry-run
```

Conflicts abort with the conflicting files listed; resolve and re-run. `pre-rebase` (blocking) and `post-rebase` hooks fire around it.

## Smart Merge

Enabled via the desktop app's settings or global config (`~/.config/devflow/config.yml`). It adds:

**Readiness checks** — configured under `merge:` in `.devflow.yml` and evaluated before merging:

```bash
devflow merge --check-only     # report READY / NOT READY without merging
devflow merge --force          # skip checks
```

**Cascade reports** — after merging, devflow analyzes child workspaces (parent relationships from the registry) and reports which ones now need a rebase:

```bash
devflow merge --cascade-rebase   # auto-rebase affected children onto the target
```

`post-merge-cascade` hooks fire for cascade processing.

## Merge train

A queue that merges workspaces into a target sequentially, re-checking readiness between entries — the "several features land on main today" workflow:

```bash
devflow train add                      # queue current workspace (target: main)
devflow train add feature/auth --target develop
devflow train status [--target develop]
devflow train run                      # process the queue in order
devflow train run --stop-on-failure
devflow train run --cleanup            # remove each workspace after its merge
devflow train pause / resume
devflow train remove feature/auth
```

All train commands support `--json`. If Smart Merge is disabled, `devflow train` exits with instructions to enable it.
