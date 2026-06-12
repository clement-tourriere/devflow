---
title: Adding devflow to an existing project
description: Adopt devflow in an established repository — config, hooks, existing branches, and mise task-runner integration.
sidebar:
  order: 4
---

## Initialize in place

```bash
cd ~/existing-project
devflow init
```

`init` never touches your code — it writes `.devflow.yml`, installs VCS hooks (`post-checkout`, `post-merge`, `pre-commit`, `post-rewrite`; all marked so `devflow uninstall-hooks` removes only devflow's), and registers the project in local state.

## Adopt existing branches and worktrees

Branches that existed before devflow can be linked into the registry:

```bash
devflow link feature/auth                # register + create matching services
devflow link feature/auth --from main    # set the parent explicitly
```

Worktrees you created manually with `git worktree add` are picked up automatically: the installed post-checkout hook detects worktree context and runs the setup (file copying + service workspace creation). To do it explicitly from inside a worktree:

```bash
devflow worktree-setup
```

## Control which branches get environments

```yaml
git:
  auto_create_on_workspace: true        # create service workspaces on git checkout
  auto_switch_on_workspace: true        # switch services on git checkout
  main_workspace: main
  workspace_filter_regex: "^(feature|fix|agent)/.*"   # only these patterns
  exclude_workspaces: [main, master, develop]          # never these
```

Per-machine overrides go in `.devflow.local.yml` (gitignored), quick toggles in [environment variables](/devflow/reference/environment/) — e.g. `DEVFLOW_DISABLED=true` to turn devflow off entirely, or `DEVFLOW_CURRENT_BRANCH_DISABLED=true` for just the branch you're on.

## Using mise as a task runner

If your project uses [mise](https://mise.jdx.dev/), pair it with devflow hooks so new worktrees are immediately trusted and tooled:

```yaml
hooks:
  post-create:
    mise-trust:
      command: "mise trust --quiet || true"
      condition: "file_exists:mise.toml"
    mise-install:
      command: "mise install"
      condition: "file_exists:mise.toml"
      continue_on_error: true
```

Or install the pre-built recipe, which covers mise, direnv, and common setups:

```bash
devflow hook install local-dev-setup
```

## Team rollout

`.devflow.yml` is committed — teammates get the same services, hooks, and worktree layout by running `devflow init` (idempotent; it detects the existing config) or just `devflow install-hooks` + `devflow switch`. Hook commands from the config require a one-time [approval](/devflow/concepts/hooks/#approvals) per user before they execute.
