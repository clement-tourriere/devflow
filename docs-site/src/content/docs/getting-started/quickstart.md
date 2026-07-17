---
title: Quickstart
description: Initialize a project, create an isolated workspace, and connect to its services in five minutes.
sidebar:
  order: 2
---

## 1. Initialize a project

```bash
cd ~/my-project
devflow init
```

The interactive wizard:

- detects your VCS (Git or Jujutsu) and primary/default workspace,
- configures the worktree path and copy policy (every workspace gets its own directory),
- detects Copy-on-Write support on your filesystem,
- optionally adds a first service (PostgreSQL, ClickHouse, MySQL, Redis, …),
- installs the VCS hooks and offers shell integration.

Pass a path (`devflow init myapp`) to create and initialize a new directory. Git worktrees (or jj workspaces) are always used for additional workspaces in interactive and non-interactive modes.

This writes a `.devflow.yml` you commit with the repo:

```yaml
services:
  - name: app-db
    type: local
    service_type: postgres
    default: true
    local:
      image: postgres:17

worktree:
  path_template: "../{repo}.{workspace}"
```

## 2. Create an isolated workspace

```bash
devflow switch -c feature/auth
```

One command does all of it:

- creates the Git ref and linked worktree (or jj workspace) from the current context or `--from <parent>`,
- creates a worktree at `../my-project.feature_auth_fc659bd73585` and copies configured files into it,
- clones the parent's database into a new isolated service workspace (CoW — near-instant),
- runs your `post-create` / `post-switch` hooks (write `.env.local`, run migrations, …),
- `cd`s your shell into the worktree (with [shell integration](/devflow/getting-started/shell-integration/) installed).

## 3. Inspect the environment

```bash
devflow status          # current workspace, services, connection info
devflow list            # parent tree with paths, services, processes, and health
devflow --json list     # stable versioned tree document for automation
```

## 4. Use the connection info

```bash
devflow connection feature/auth                # URI
devflow connection feature/auth --format env   # KEY=value lines
devflow connection feature/auth --format json  # machine-readable
```

Most projects don't call this manually — a [hook](/devflow/concepts/hooks/) writes `.env.local` on every switch:

```yaml
hooks:
  post-switch:
    env:
      action:
        type: write-env
        path: .env.local
        vars:
          DATABASE_URL: "{{ service['app-db'].url }}"
```

## 5. Clean up

```bash
devflow switch --template       # move back to the configured default workspace
devflow remove feature/auth
```

Preflight always protects the default/current workspace, so move to another workspace before removing the one you finished; `--force` does not override that protection. A dirty worktree is also refused unless explicitly forced. Removal hooks run while the directory exists; processes and services are cleaned up before the worktree and VCS ref are deleted.

## Where to go next

- [Workspaces & isolation models](/devflow/concepts/workspaces/) — how local CoW containers and shared engines differ
- [Worktrees](/devflow/concepts/worktrees/) — what gets copied into a worktree and why
- [Hooks](/devflow/concepts/hooks/) — automate env files, migrations, and setup
- [Project processes & Pitchfork](/devflow/guides/processes/) — run app servers and workers per workspace
- [Adding devflow to an existing project](/devflow/getting-started/existing-project/) — migrate from Docker Compose incrementally
- [AI agents](/devflow/guides/ai-agents/) — give every agent task its own environment
