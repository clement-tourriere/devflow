---
title: Hooks reference
description: Every hook phase, template variable, filter, condition, action type, recipe, and trigger mapping.
sidebar:
  order: 3
---

Concepts and patterns: [Hooks concept](/devflow/concepts/hooks/) · [hooks guide](/devflow/guides/hooks/).

## Phases

| Phase | Fires | Blocking |
| --- | --- | --- |
| `pre-switch` | before switching to a workspace | **yes** |
| `post-create` | after a new branch **or worktree** was created | **yes** |
| `post-start` | after starting a stopped service workspace | no |
| `post-switch` | after every switch (including the one implied by creation) | no |
| `pre-remove` | before removing a workspace | **yes** |
| `post-remove` | after removal | no |
| `pre-commit` | before a commit (installed Git pre-commit hook) | **yes** |
| `pre-service-create` | before creating service workspaces | **yes** |
| `post-service-create` | after creating service workspaces | no |
| `pre-service-delete` | before deleting service workspaces | **yes** |
| `post-service-delete` | after deleting service workspaces | no |
| `post-service-switch` | after switching service workspaces | no |
| *anything else* | custom phase — run with `devflow hook run <phase>` | no |

A failing hook in a **blocking** phase aborts the operation (unless `continue_on_error: true`). Non-blocking phases are best-effort: failures are reported in the result summary but don't abort. `background: true` hooks are spawned concurrently and awaited at process exit up to `DEVFLOW_BACKGROUND_HOOK_TIMEOUT` seconds (default 30).

:::note
Which phases fire depends on the operation: a CLI `switch -c` runs `pre-switch → (services) → post-service-switch → post-create → post-switch`; a GUI *create* runs `pre-service-create → (services) → post-service-create → post-create → post-switch`. Put critical setup in `post-create`/`post-switch`, which fire on both paths.
:::

## Entry schema

```yaml
hooks:
  <phase>:
    <name>: "<command>"          # Simple
    <name>:                      # Extended
      command: "<template>"
      working_dir: "<rel-path>"  # relative to the workspace's worktree (or project root)
      condition: "<condition>"
      continue_on_error: false
      background: false
      environment: { KEY: "<template>" }
    <name>:                      # Action (shell-free, no approval needed)
      action:
        type: <action-type>
        …action fields…
      condition: "<condition>"
```

Hooks run with their working directory set to the **target workspace's worktree** when one exists, the project root otherwise.

## Template variables

Rendered with MiniJinja (Jinja2-compatible) in commands, environment values, action fields, and conditions.

| Variable | Description | Example |
| --- | --- | --- |
| `{{ workspace }}` | raw VCS branch name | `feature/auth` |
| `{{ workspace_sanitized }}` | normalized name (db/file-safe) | `feature_auth` |
| `{{ name }}` | project name (config `name:` or directory) | `my-project` |
| `{{ repo }}` | repository directory name | `my-project` |
| `{{ worktree_path }}` | absolute worktree path, when in worktree context | `/…/my-project.feature_auth` |
| `{{ default_workspace }}` | main workspace | `main` |
| `{{ commit }}` / `{{ short_commit }}` | HEAD SHA / abbreviated | `a1b2c3d…` / `a1b2c3d` |
| `{{ base }}` | base/parent workspace (creation hooks) | `main` |
| `{{ trigger_source }}` | what invoked the hook: `cli`, `vcs`, `gui` | `vcs` |
| `{{ vcs_event }}` | originating VCS event, when any | `post-checkout` |
| `{{ service.<name>.host }}` | service host | `localhost` |
| `{{ service.<name>.port }}` | service port | `55433` |
| `{{ service.<name>.database }}` | database/bucket/index | `feature_auth` |
| `{{ service.<name>.user }}` / `.password` | credentials | `postgres` |
| `{{ service.<name>.url }}` | full connection URL | `postgresql://…` |

Use bracket access for hyphenated service names: `{{ service['app-db'].url }}`. Inspect live values with `devflow hook vars`.

## Filters

| Filter | Description | Example |
| --- | --- | --- |
| `sanitize` | replace `/` and `\` with `-` | `{{ workspace \| sanitize }}` → `feature-auth` |
| `sanitize_db` | database-safe identifier (≤63 chars, hash suffix) | `{{ workspace \| sanitize_db }}` → `feature_auth` |
| `hash_port` | deterministic port in 10000–19999 | `{{ workspace \| hash_port }}` → `14523` |
| `lower` / `upper` | case mapping | `{{ workspace \| upper }}` |
| `replace` | string replacement | `{{ workspace \| replace("/", "-") }}` |
| `truncate` | first N characters | `{{ workspace \| truncate(20) }}` |

## Conditions

Conditions are template-rendered first, then evaluated. Built-ins:

| Condition | True when |
| --- | --- |
| `file_exists:<path>` / `dir_exists:<path>` | path exists (relative to the hook working dir) |
| `workspace_is:<name>` / `workspace_not:<name>` | workspace equals / differs |
| `workspace_matches:<regex>` | workspace matches the regex |
| `is_default_workspace` / `not_default_workspace` | workspace is / isn't the main workspace |
| `is_worktree` / `not_worktree` | a worktree exists for this context |
| `trigger_is:<src>` / `trigger_not:<src>` | trigger source is / isn't `cli`·`vcs`·`gui` |
| `env_set:<VAR>` / `env_is:<VAR>=<value>` | environment variable set / equals |
| `always` · `true` / `never` · `false` | constant |
| *anything else* | executed as a shell command; exit 0 = true |

## Action types

| Type | Purpose | Key fields |
| --- | --- | --- |
| `write-env` | create/merge an env file | `path`, `vars: {K: V}` |
| `write-file` | write a file from a template | `path`, `content` |
| `copy` | copy a file/dir | `from`, `to`, `overwrite` |
| `replace` | in-file string/regex replacement | `path`, `find`, `replace` |
| `shell` | run a command (same as `command:`) | `command` |
| `docker-exec` | exec inside a service container | `service`, `command` |
| `http` | HTTP request (webhooks, health checks) | `url`, `method`, `body` |
| `notify` | desktop notification | `message`, `title` |

`devflow hook actions` lists the authoritative set with all fields. Actions are shell-free and skip the approval system.

## Recipes

```bash
devflow hook recipes              # list
devflow hook install <name>      # add to .devflow.yml (never overwrites your entries)
```

`sync-ai-configs` · `install-deps` · `docker-compose` · `local-dev-setup` · `db-migrate` · `multiplexer-session` — descriptions in the [hooks guide](/devflow/guides/hooks/#recipes).

## VCS trigger mapping

Installed Git hooks dispatch to devflow phases:

| Git hook | devflow phases |
| --- | --- |
| `post-checkout` | `post-switch` (plus `post-create` when the workspace is new) |
| `pre-commit` | `pre-commit` |

`devflow hook triggers` displays the active mapping.

## Approvals

Shell hooks from the committed config require one-time approval per user; built-in actions don't. Approvals are keyed by **canonical project root + command template** in `~/.config/devflow/hook_approvals.yml` — one approval covers all workspaces and worktrees. Non-interactive behavior and pre-approval: [hooks concept → approvals](/devflow/concepts/hooks/#approvals).
