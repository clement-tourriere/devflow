---
title: Project processes & Pitchfork
description: Run app servers, workers, and schedulers per workspace with service URLs injected, readiness checks, logs, port bumping, and optional Pitchfork supervision.
sidebar:
  order: 6
---

Devflow separates **services** from **processes**:

- **Services** are stateful dependencies that devflow creates, branches, resets, and deletes: databases, caches, object storage, or generic Docker containers.
- **Processes** are project commands that run in the selected workspace/worktree: web servers, frontend dev servers, workers, schedulers, bots, or one-off long-running integrations.

That split is the usual migration path away from a large Docker Compose file: move data containers to devflow services, then run the app commands as devflow processes on the host.

## Basic configuration

```yaml
services:
  - name: app-db
    type: shared
    service_type: postgres
    default: true
  - name: cache
    type: shared
    service_type: redis

hooks:
  post-switch:
    env:
      action:
        type: write-env
        path: .env.local
        vars:
          DATABASE_URL: "{{ service['app-db'].url }}"
          REDIS_URL: "{{ service['cache'].url }}"

processes:
  provider: native       # native (default) or pitchfork
  auto_start: true       # start after devflow switch aligns services + hooks
  auto_stop: true        # stop before devflow remove cleans up the workspace
  daemons:
    web:
      run: "npm run dev -- --host 127.0.0.1 --port $PORT"
      port: { expect: [3000], bump: 50 }
      ready_http: "http://127.0.0.1:3000/health"
      env:
        DATABASE_URL: "{{ service['app-db'].url }}"
        REDIS_URL: "{{ service['cache'].url }}"
    worker:
      run: "npm run worker"
      required: false
      env:
        DATABASE_URL: "{{ service['app-db'].url }}"
        REDIS_URL: "{{ service['cache'].url }}"
```

When a `port` is configured, devflow resolves a free port and injects the first resolved value as `$PORT`. Readiness URLs and `ready_port` values that reference an expected port are remapped to the bumped port automatically.

## Native vs Pitchfork

| Provider | Use when | Notes |
| --- | --- | --- |
| `native` | You want the built-in devflow process runner. | No extra CLI required; logs are stored by devflow. |
| `pitchfork` | You want Pitchfork's supervisor and log store embedded directly in devflow. | Set `processes.provider: pitchfork`; devflow calls Pitchfork's Rust APIs, not the `pitchfork` binary. |

Both providers use the same `.devflow.yml` schema and the same CLI:

```bash
devflow process start --all
devflow process status
devflow process logs web --tail 100 --follow
devflow process restart worker
devflow process stop --all
```

`processes.provider: pitchfork` is a runtime choice, not a separate Pitchfork config file. Devflow still owns workspace identity, service interpolation, desired state, proxy records, GUI/TUI status, and cleanup. In the desktop GUI, Pitchfork-backed rows show a `pitchfork` badge plus the generated Pitchfork daemon ID.

## Readiness, dependencies, and optional processes

```yaml
processes:
  daemons:
    web:
      run: "mise x -- python manage.py runserver 127.0.0.1:$PORT"
      port: { expect: [8000], bump: 100 }
      ready_port: 8000
      ready_timeout: 60
    worker:
      run: "mise x -- celery -A app worker -l INFO"
      required: false
      retry: 3
      env:
        DATABASE_URL: "{{ service['app-db'].url }}"
    scheduler:
      run: "mise x -- celery -A app beat -l INFO"
      required: false
      depends: [worker]
```

- `depends` orders process startup; it does not replace service readiness. Services are aligned before processes start.
- `required: false` is useful for optional integrations: failures are reported but do not fail `devflow switch`.
- `retry` and `watch` are reconciled by the controller daemon.

## Watch/retry reconciliation

`devflow switch` starts processes once. To keep desired state, restart crashed processes, and apply `watch` restarts, run the daemon:

```bash
devflow daemon start            # background controller
devflow daemon status
devflow daemon stop
```

Example:

```yaml
processes:
  provider: pitchfork
  auto_start: true
  daemons:
    web:
      run: "npm run dev -- --port $PORT"
      port: { expect: [3000], bump: 50 }
      watch: ["src/**/*.ts", "package.json"]
      retry: 3
```

## Compose-to-process mapping

| Docker Compose concept | Devflow equivalent |
| --- | --- |
| `postgres`, `redis`, `minio` data services | `services:` entries (`shared`, `local`, RustFS, Redis, etc.) |
| `web.command`, `worker.command`, `scheduler.command` | `processes.daemons.<name>.run` |
| `depends_on` between app containers | `processes.daemons.<name>.depends` |
| `env_file` / generated app env | `hooks.post-switch` `write-env` plus `processes.daemons.<name>.env` |
| Port mappings for app containers | `port: { expect: [...], bump: N }` and devflow proxy URLs |
| Compose `develop.watch` | `watch` patterns plus `devflow daemon start` |
| Fixed container hostnames like `postgres` or `redis` | `{{ service['name'].url }}` rendered to workspace-specific URLs |

Keep Compose for pieces you are not ready to migrate yet. Just avoid running the same app command twice on the same fixed port, or enable devflow port bumping.

## Pitchfork config reconciliation

Devflow treats `.devflow.yml` as the source of truth by default, even when using Pitchfork as the runtime. That avoids losing workspace-aware templates such as `{{ service['app-db'].url }}` to rendered Pitchfork config values.

```yaml
processes:
  provider: pitchfork
  pitchfork:
    config_policy: devflow-owned   # devflow-owned | import | mirror | merge
    external_daemons: show         # hide | show | importable
    web_ui:
      enabled: true
      bind_address: 127.0.0.1
      bind_port: 3120
      edit_mode: warn              # readonly | warn | merge
```

Policy modes:

- `devflow-owned` — default and active behavior. Ignore Pitchfork config for devflow-managed namespaces; devflow owns `.devflow.yml` and process cleanup.
- `import` — intent for one-way adoption from Pitchfork config into `.devflow.yml`.
- `mirror` — intent for generated Pitchfork config from `.devflow.yml` so Pitchfork-native tools can discover devflow processes.
- `merge` — intent for advanced explicit diff/merge mode; never silently overwrite devflow templates.

The GUI can open Pitchfork's own Web UI when you enable the bridge and the Web UI is already running on loopback. It can also launch `pitchfork tui` in devflow's integrated terminal when the external `pitchfork` CLI is installed.

## Automation approvals

When `auto_start: true`, process commands are treated like lifecycle hook commands. In interactive mode devflow can ask once. In `--non-interactive`/`--json` mode, unapproved commands are skipped unless you pre-approve them:

```bash
devflow hook approvals add "npm run dev -- --host 127.0.0.1 --port $PORT"
devflow hook approvals add "mise x -- python manage.py runserver 127.0.0.1:$PORT"
# or for trusted CI/agent runs:
DEVFLOW_APPROVE_HOOKS=1 devflow --json --non-interactive switch -c agent/task-42
```

Manual `devflow process start` commands do not prompt for hook approval.

## Proxy URLs

Processes with resolved ports are exposed through the devflow proxy as:

```text
https://<process>.<workspace>.<project>.<suffix>
```

For example, `web` in workspace `feature/auth` of project `myapp` becomes `https://web.feature-auth.myapp.local` with the default suffix. Start the proxy with:

```bash
devflow proxy start --daemon
```

See the [proxy guide](/devflow/guides/proxy/) for trust setup and name resolution details.

## Troubleshooting checklist

```bash
devflow config-validate
devflow hook vars --workspace feature/auth
devflow connection feature/auth --format env
devflow process status --workspace feature/auth
devflow process logs web --workspace feature/auth --tail 200
```

If a process starts on the wrong database, check the generated `.env.local` and the process `env:` templates. If it never becomes ready, start with `ready_port` before adding stricter HTTP or output readiness checks.
