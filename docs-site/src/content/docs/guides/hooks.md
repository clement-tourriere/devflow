---
title: Working with hooks
description: Practical hook patterns — env files, migrations, recipes, approvals for automation, and debugging hooks.
sidebar:
  order: 5
---

Concepts live in [Hooks](/devflow/concepts/hooks/); the full schema in the [reference](/devflow/reference/hooks/). This page is patterns and operations.

## The essential pattern: env files per workspace

```yaml
hooks:
  post-create:
    install:
      command: "npm ci"
      condition: "file_exists:package.json"
    migrate:
      command: "npm run migrate"
      environment:
        DATABASE_URL: "{{ service['app-db'].url }}"
  post-switch:
    env:
      action:
        type: write-env
        path: .env.local
        vars:
          DATABASE_URL: "{{ service['app-db'].url }}"
          REDIS_URL: "redis://{{ service.cache.host }}:{{ service.cache.port }}/{{ service.cache.database }}"
```

`post-switch` fires on every switch (including the implicit one after creation), so `.env.local` always matches the active workspace — in the right worktree.

The `write-env` **action** is preferred over `echo … > .env.local`: no shell, no quoting bugs, no approval prompt, and it merges instead of clobbering.

## More patterns

```yaml
hooks:
  post-start:
    dev-server:
      command: "npm run dev -- --port {{ workspace | hash_port }}"
      background: true                       # deterministic port per workspace
  pre-commit:
    test: { command: "npm test", continue_on_error: false }
    lint: { command: "npm run lint", continue_on_error: false }
  post-remove:
    cleanup:
      command: "docker stop {{ repo }}-{{ workspace | sanitize }}-app 2>/dev/null || true"
      continue_on_error: true
  post-create:
    notify:
      action:
        type: notify
        message: "Workspace {{ workspace }} ready"
```

Conditions keep hooks polyglot-safe (`condition: "file_exists:requirements.txt"` for the Python path, `package.json` for Node) and context-aware (`is_worktree`, `workspace_matches:^agent/.*`, `trigger_is:vcs`). [All conditions →](/devflow/reference/hooks/#conditions)

## Recipes

Installable bundles of proven hooks (added to `.devflow.yml` without overwriting your entries):

```bash
devflow hook recipes                      # list with descriptions
devflow hook install local-dev-setup
```

| Recipe | What it does |
| --- | --- |
| `local-dev-setup` | mise trust/install, direnv allow, and friends on workspace creation |
| `install-deps` | dependency install for detected package managers |
| `db-migrate` | run migrations after create/switch |
| `docker-compose` | bring compose services up/down with the workspace |
| `sync-ai-configs` | sync AI tool configs back to main before workspace removal |
| `multiplexer-session` | auto-open a tmux/zellij session in the worktree after creation |

## Approvals in automation

Interactive runs prompt once per hook template. For CI and agents (`--non-interactive`), unapproved hooks are **skipped with a warning** — the command still succeeds and reports per-phase counts. Make hooks actually run by either:

```bash
# approve specific templates once per project (covers all workspaces/worktrees)
devflow hook approvals add "npm run migrate"
devflow hook approvals list

# or blanket-approve config hooks for this run
DEVFLOW_APPROVE_HOOKS=1 devflow --json --non-interactive switch -c agent/t42
```

In JSON output, `hooks[].skipped > 0` is your signal that an approval is missing. `--no-verify` is different — it skips *all* hooks entirely.

## Running and debugging hooks

```bash
devflow hook show                      # everything configured
devflow hook show post-create
devflow hook run post-create           # run a phase manually
devflow hook run post-create migrate   # one named hook
devflow hook run post-create --workspace feature/auth
devflow hook explain post-switch       # phase docs + when it fires
devflow hook vars                      # live template context
devflow hook render "{{ service['app-db'].url }}"
devflow hook triggers                  # VCS event → phase mapping
devflow hook actions                   # built-in action list
```

:::note
`devflow hook run` currently resolves relative paths and `file_exists:` conditions against your **current directory**, not the target workspace's worktree — results can differ from real lifecycle runs if you're not standing in the worktree.
:::

## Custom phases

Any unknown phase name is a custom phase — define it and run it on demand:

```yaml
hooks:
  load-fixtures:
    seed: "psql {{ service['app-db'].url }} -f fixtures.sql"
```

```bash
devflow hook run load-fixtures
```
