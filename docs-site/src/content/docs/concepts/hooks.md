---
title: Hooks
description: The lifecycle hook engine — phases, templating, conditions, approvals, and how hooks interact with worktrees.
sidebar:
  order: 4
---

Hooks are commands (or built-in actions) that run at specific points of the workspace lifecycle: write env files after a switch, run migrations after creation, test before a commit, clean up before removal. They are defined in `.devflow.yml` and rendered with [MiniJinja](https://docs.rs/minijinja/) (Jinja2-compatible) templates.

```yaml
hooks:
  post-create:
    install: "npm ci"                                  # simple form
    env:                                               # action form
      action:
        type: write-env
        path: .env.local
        vars:
          DATABASE_URL: "{{ service['app-db'].url }}"
  pre-commit:
    test:                                              # extended form
      command: "npm test"
      condition: "file_exists:package.json"
      continue_on_error: false
```

## Three entry forms

| Form | When to use |
| --- | --- |
| **Simple** — `name: "command"` | one-line shell commands |
| **Extended** — `command:` + options (`working_dir`, `condition`, `environment`, `background`, `continue_on_error`) | anything needing context or control |
| **Action** — `action: {type: …}` | built-in, shell-free operations: `write-env`, `write-file`, `copy`, `replace`, `docker-exec`, `http`, `notify`, `shell` |

The full schema, every template variable, filter, condition, and action is in the [hooks reference](/devflow/reference/hooks/).

## Phases

Phases group into workspace lifecycle (`pre-switch`, `post-create`, `post-switch`, `pre-remove`, `post-remove`, …), commit lifecycle (`pre-commit`), and service lifecycle (`pre/post-service-create`, `pre/post-service-delete`, `post-service-switch`). Unknown phase names are **custom phases** you can run manually with `devflow hook run <phase>`.

**Blocking** phases (`pre-switch`, `post-create`, `pre-remove`, `pre-commit`, `pre-service-create`, `pre-service-delete`) run synchronously and a failure aborts the operation (unless `continue_on_error: true`). All other phases are best-effort: failures are reported but don't abort. Hooks with `background: true` are spawned concurrently and awaited at process exit, up to `DEVFLOW_BACKGROUND_HOOK_TIMEOUT` seconds (default 30).

## Worktree awareness

Hooks run **inside the target workspace's worktree** when one exists (project root otherwise). `{{ worktree_path }}`, `is_worktree`/`not_worktree` conditions, and relative paths in actions (`write-env path: .env.local`) all resolve against that working directory. This is what makes "write `.env.local` on every switch" land in the right directory per workspace.

## Templating in 30 seconds

```yaml
hooks:
  post-switch:
    banner: "echo Switched to {{ workspace }} ({{ workspace_sanitized }})"
    env: "echo DATABASE_URL={{ service['app-db'].url }} > .env.local"
  post-start:
    dev: 
      command: "npm run dev -- --port {{ workspace | hash_port }}"
      background: true
```

Key variables: `workspace` (raw branch name), `workspace_sanitized`, `worktree_path`, `default_workspace`, `repo`, `name`, `commit`/`short_commit`, `trigger_source` (`cli`/`vcs`/`gui`), and `service.<name>.{host,port,database,user,password,url}`. Filters include `sanitize`, `sanitize_db`, `hash_port` (deterministic port from the workspace name), `lower`, `upper`, `replace`, `truncate`. [Full tables →](/devflow/reference/hooks/)

Inspect the live context anytime:

```bash
devflow hook vars                  # all variables for the current workspace
devflow hook render "{{ service['app-db'].url }}"
devflow hook explain post-create
```

## Approvals

Shell hooks from the (committed, hence attacker-writable) config require a one-time approval per user before they execute — protection against a malicious `.devflow.yml` running code via Git hooks. On first encounter devflow prompts: approve always / approve once / deny.

- Approvals are stored in `~/.config/devflow/hook_approvals.yml`, keyed by the **canonical project root** and the hook's **command template** (not the rendered output) — one approval covers every workspace, including agent-created worktrees.
- In `--non-interactive` / `--json` mode, an unapproved hook is **skipped with a visible warning** (it never blocks the command; the JSON `hooks` summary counts it as `skipped`).
- Pre-approve for automation: `devflow hook approvals add "npm run migrate"`, or set `DEVFLOW_APPROVE_HOOKS=1` for CI/agent runs.
- Built-in actions (`write-env`, `copy`, …) don't require approval — they're shell-free.

## Recipes

Pre-built hook bundles installable with one command (`devflow hook install <name>`): `sync-ai-configs`, `install-deps`, `docker-compose`, `local-dev-setup`, `db-migrate`, `multiplexer-session`. List them with `devflow hook recipes`. Details in the [hooks guide](/devflow/guides/hooks/#recipes).
