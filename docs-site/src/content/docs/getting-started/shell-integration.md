---
title: Shell integration
description: Enable automatic cd into worktrees whenever devflow switches workspaces.
sidebar:
  order: 3
---

A child process can't change its parent shell's directory. The shell wrapper fixes that: whenever devflow emits a `DEVFLOW_CD=<path>` line (switching to a worktree, `devflow init <dir>`, …), the wrapper `cd`s your shell there automatically.

## Install

Add to your shell profile:

```bash
eval "$(devflow shell-init)"          # auto-detects your shell
eval "$(devflow shell-init bash)"     # ~/.bashrc
eval "$(devflow shell-init zsh)"      # ~/.zshrc
devflow shell-init fish | source      # ~/.config/fish/config.fish
```

This defines a `devflow` shell function that wraps the real binary, watches its output for `DEVFLOW_CD=`, and changes directory when it sees one.

## What it enables

```bash
devflow switch -c feature/auth
# Created worktree for 'feature/auth' at ../myapp.feature_auth_fc659bd73585
# Changed directory to: ../myapp.feature_auth_fc659bd73585  ← the wrapper did this
```

Without the wrapper, devflow prints the worktree path and a manual `cd` hint instead.

## Caveats

:::caution
The current wrapper captures command output while devflow runs, which interferes with fully interactive commands:

- `devflow tui` should be run **without** the wrapper (call `command devflow tui` or run it from a shell profile that doesn't define the wrapper).
- Prompts that read from stdin (e.g. the `devflow remove` confirmation) may not display through the wrapper — first switch away from the target workspace, then prefer `devflow remove <ws> --force` in wrapped shells, or run `command devflow remove <ws>`.

`command devflow …` always bypasses the wrapper.
:::

## Opening worktrees from the TUI

Pressing `o` in the [TUI](/devflow/guides/tui/) exits and prints the selected workspace's worktree path. `cd` there manually (or use `devflow switch <name>` from your wrapped shell, which auto-cds).
