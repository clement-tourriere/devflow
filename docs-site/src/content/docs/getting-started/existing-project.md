---
title: Adding devflow to an existing project
description: Adopt devflow in an established repository — config, hooks, existing branches, Docker Compose migration, Pitchfork processes, and hybrid rollout patterns.
sidebar:
  order: 4
---

## Initialize in place

```bash
cd ~/existing-project
devflow init
```

`init` never touches your application code. It writes `.devflow.yml`, installs VCS hooks (`post-checkout` and `pre-commit`; both marked so `devflow uninstall-hooks` removes only devflow's), and registers the project in local state.

For non-interactive adoption, start from a small committed config and validate it:

```bash
devflow config-validate
devflow install-hooks
devflow capabilities
```

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

## Migration map from Docker Compose

Most existing applications have three kinds of Compose services. Move them at different speeds:

| Existing Compose item | Good devflow target | Why |
| --- | --- | --- |
| PostgreSQL/MySQL/ClickHouse | `services:` with `type: shared` or `type: local` | Workspace-specific data and connection URLs. |
| Redis/cache/object storage | `service_type: redis` or `service_type: rustfs` | One shared engine, per-workspace DB index/bucket. |
| App server, frontend dev server, workers, schedulers | `processes.daemons` with `provider: native` or `pitchfork` | Runs in each worktree with service env injected. |
| Search/mail/queue containers you still want as containers | `service_type: generic`, or keep them in Compose temporarily | Hybrid migration without a big-bang rewrite. |
| `env_file` values that depend on ports/branches | `hooks.post-switch` `write-env` | Regenerated on every switch with the right workspace URLs. |

A practical migration is: move the database first, generate `.env.local`, then move app processes once the app can run on the host.

## Recipe: shared Postgres + Redis

This replaces Compose-managed Postgres/Redis containers with devflow-managed shared engines. Each workspace gets its own Postgres database and Redis DB index, while the engine containers stay global and cheap.

```yaml
services:
  - name: app-db
    type: shared
    service_type: postgres
    default: true
    shared:
      image: postgres:17
      port: 5432
      template_branching: true
  - name: cache
    type: shared
    service_type: redis
    shared:
      image: redis:7

worktree:
  enabled: true
  path_template: "../{repo}.{workspace}"
  copy_files: [.env]

hooks:
  post-switch:
    env:
      action:
        type: write-env
        path: .env.local
        vars:
          DATABASE_URL: "{{ service['app-db'].url }}"
          REDIS_URL: "{{ service['cache'].url }}"
          # Framework-specific aliases are fine too:
          CACHE_URL: "{{ service['cache'].url }}"
          CELERY_BROKER_URL: "{{ service['cache'].url }}"
```

Bootstrap and seed the main database:

```bash
devflow service up
devflow service create main
MAIN_DATABASE_URL="$(devflow connection main --format uri)"

# Example: import from an existing Compose Postgres container.
docker compose exec -T postgres pg_dump -U postgres postgres | psql "$MAIN_DATABASE_URL"
```

Now create a workspace. With `template_branching: true`, workspace databases are copied from the parent database:

```bash
devflow switch -c feature/auth
devflow connection feature/auth --format env
```

If you prefer one full Docker container per workspace instead, use [local containers](/devflow/guides/local-containers/) for Postgres:

```yaml
services:
  - name: app-db
    type: local
    service_type: postgres
    default: true
    local:
      image: postgres:17
```

## Add app processes with Pitchfork

Once the app can run from the host (for example through `mise`, `uv`, `npm`, `bun`, `pdm`, or your language's task runner), move Compose app containers to devflow processes:

```yaml
processes:
  provider: pitchfork     # or omit for native
  auto_start: true
  auto_stop: true
  daemons:
    web:
      run: "mise x -- python manage.py runserver 127.0.0.1:$PORT"
      port: { expect: [8000], bump: 100 }
      ready_port: 8000
      env:
        DATABASE_URL: "{{ service['app-db'].url }}"
        REDIS_URL: "{{ service['cache'].url }}"
    worker:
      run: "mise x -- celery -A app worker -l INFO"
      required: false
      env:
        DATABASE_URL: "{{ service['app-db'].url }}"
        REDIS_URL: "{{ service['cache'].url }}"
    scheduler:
      run: "mise x -- celery -A app beat -l INFO"
      required: false
      depends: [worker]
      env:
        DATABASE_URL: "{{ service['app-db'].url }}"
        REDIS_URL: "{{ service['cache'].url }}"
```

`$PORT` is set by devflow after port bumping, and `ready_port: 8000` follows the bumped port. Pitchfork is embedded through devflow (`processes.provider: pitchfork`); you do not need a separate `pitchfork` CLI process.

Useful commands:

```bash
devflow process start --all
devflow process status
devflow process logs web --tail 100 --follow
devflow process stop --all
devflow daemon start        # keep shared engines alive; reconcile watch/retry
```

See [Project processes & Pitchfork](/devflow/guides/processes/) for readiness checks, watch/retry, logs, proxy URLs, and approval behavior.

## Hybrid rollout patterns

You do not need to migrate everything at once.

### 1. Data in devflow, app still in Compose

Generate `.env.local` from devflow, but keep running your app container manually. Change the app container env to point at the devflow URLs instead of Compose hostnames. This is useful when the app still depends on container-only tooling.

### 2. App processes in devflow, a few containers left in Compose

Move the web/worker/scheduler commands to `processes.daemons`, but keep specialized containers (mailcatcher, search, queue emulator, browser test grid) in Compose until you have a reason to move them.

### 3. Devflow generic containers for non-branching dependencies

For a dependency that should remain one shared local container for all workspaces:

```yaml
services:
  - name: search
    type: local
    service_type: generic
    auto_workspace: false
    generic:
      image: opensearchproject/opensearch:2
      port_mapping: "9200:9200"
      environment:
        discovery.type: single-node
```

For a dependency that needs one container per workspace, keep `auto_workspace: true` and use `port_range_start` rather than a fixed port mapping.

### 4. External or cloud-managed data

If a team already uses a hosted branching database, use a cloud provider (`neon`, `dblab`, `xata`) or keep the external URL in `.devflow.local.yml`. Avoid committing personal credentials; commit only the shared shape and keep secrets machine-local.

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

Or install the recipe, which detects which of mise / direnv / `.env.example` actually apply:

```bash
devflow hook install workspace-setup
```

In non-interactive automation, approve trusted hook/process command templates once:

```bash
devflow hook approvals add "mise trust --quiet || true"
devflow hook approvals add "mise install"
devflow hook approvals add "mise x -- python manage.py runserver 127.0.0.1:$PORT"
```

## Validation checklist

```bash
devflow config-validate
devflow hook vars --workspace main
devflow service status
devflow connection main --format env
devflow switch -c feature/devflow-smoke
devflow process start --all
devflow process status
devflow process logs web --tail 100
```

If a hook renders an empty service URL, create the workspace services first (`devflow service create <workspace>` or `devflow switch -c <workspace>`) and check the service name used in the template.

## Team rollout

`.devflow.yml` is committed — teammates get the same services, hooks, process definitions, and worktree layout by running `devflow init` (idempotent; it detects the existing config) or just `devflow install-hooks` + `devflow switch`. Hook commands from the config require a one-time [approval](/devflow/concepts/hooks/#approvals) per user before they execute.
