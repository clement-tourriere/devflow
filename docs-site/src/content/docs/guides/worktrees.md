---
title: Worktree workflows
description: Daily worktree flow — parallel features, PR reviews, multiplexer sessions, adopting manual worktrees, pruning, and cleanup.
sidebar:
  order: 1
---

This guide assumes `worktree.enabled: true` and [shell integration](/devflow/getting-started/shell-integration/) installed. Concepts and configuration are covered in [Worktrees](/devflow/concepts/worktrees/).

## The daily flow

```bash
# Start a new feature — worktree + isolated services + hooks, then auto-cd
devflow switch -c feature/auth
# → Created worktree for 'feature/auth' at ../my-project.feature_auth
# → service workspace cloned from main
# → Changed directory to: ../my-project.feature_auth

npm run migrate && npm test          # work normally

# A PR review comes in — keep everything running, open a second worktree
devflow switch -c review/pr-417 --from main

# Jump back — instant, nothing was stopped or rebuilt
devflow switch feature/auth

# Forgot a name? Fuzzy picker:
devflow switch
```

Each worktree keeps its own build artifacts, env files, and database. Nothing is stashed, paused, or reset when you move between them.

## Useful switch flags

```bash
devflow switch -c feature/x --from develop      # explicit parent (branch + database)
devflow switch feature/x -x "npm run dev"       # run a command in the worktree after switching
devflow switch feature/x -x "npm run dev" -d    # …in a detached tmux/zellij session
devflow switch feature/x -o                     # open an interactive multiplexer session
devflow switch -c tmp/spike --no-services       # VCS only, skip service branching
devflow switch feature/x --no-processes         # skip process auto-start
devflow switch -c agent/t42 --sandboxed         # restricted filesystem/commands
devflow switch -c big --no-respect-gitignore    # also copy gitignored entries this time
devflow switch feature/x --dry-run              # print the plan (worktree path, services, processes, hooks)
```

Multiplexer sessions auto-detect tmux, then zellij; configure a preference or a fully custom launcher:

```yaml
execute:
  multiplexer: zellij                 # or "tmux"
  # detach_command: "screen -dmS {session} bash -c {cmd}"   # {session} {dir} {cmd}
```

To open a session automatically on every new workspace: `devflow hook install multiplexer-session`.

## Seeing your worktrees

```bash
devflow list      # workspaces with services + worktree paths
devflow graph     # parent/child tree with worktree paths and service states
devflow status    # current workspace details
```

`devflow --json list` includes a `worktree_path` per entry — that's what agents use to find their workdir.

## Adopting worktrees you made by hand

`git worktree add ../myapp.hotfix hotfix` is fully supported: the post-checkout hook detects the new worktree and sets it up (copies files, creates service workspaces, runs hooks). Manually:

```bash
cd ../myapp.hotfix
devflow worktree-setup
```

For branches created outside devflow (no worktree yet), `devflow link <branch>` registers them and can materialize services.

:::note
Hook-driven setup currently skips AI config dirs (`.claude/` etc.) — see the [worktrees concept page](/devflow/concepts/worktrees/#manually-created-worktrees).
:::

## Merging back and cleaning up

```bash
devflow merge                 # merge current workspace into main
devflow merge --cleanup       # …and delete the source branch, worktree, and services
```

When the *target* has its own worktree, the merge runs there — your current directory isn't disturbed. Cleanup refuses to delete a dirty worktree; commit/stash first, or remove explicitly:

```bash
devflow remove feature/auth            # confirm, then delete worktree + branch + services
devflow remove feature/auth --force    # skip confirmation AND dirty-worktree protection
devflow remove feature/auth --keep-services
```

:::tip
Run `merge --cleanup` from the **main** worktree (or any directory other than the worktree being deleted). Deleting the directory you're standing in confuses both git and your shell.
:::

## Pruning stale worktrees

If a worktree directory was deleted by hand, Git metadata lingers. devflow auto-prunes stale entries when recreating a workspace of the same name; the desktop GUI has a *Prune worktrees* button for bulk cleanup, and `git worktree prune` always works.

## Troubleshooting

- **“Failed to create worktree”** — usually a name collision: another branch normalizing to the same directory (see [name sanitization](/devflow/concepts/workspaces/#workspace-names)), or a leftover directory at the target path. Remove/rename and retry.
- **Switch didn't `cd`** — shell integration not installed in this shell; see [Shell integration](/devflow/getting-started/shell-integration/). devflow prints the path either way.
- **Copy flags seemed ignored** — overrides like `--no-respect-gitignore` only apply when the worktree is *created*; switching to an existing worktree reuses it as-is.
- **`.env.local` missing in a new worktree** — add it to `worktree.copy_files`, or generate it with a `post-create`/`post-switch` [write-env hook](/devflow/concepts/hooks/) (preferred: values stay correct per workspace).
