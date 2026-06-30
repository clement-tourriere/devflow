---
title: Shared engines
description: Logical isolation — one global container per engine with a database, bucket, or DB index per workspace, kept alive by the controller daemon.
sidebar:
  order: 3
---

Shared engines run **one** global container per engine and give each workspace a logical boundary inside it, created on the fly during `switch`. One fixed, well-known port; no per-workspace containers, volumes, or port juggling.

## PostgreSQL — `type: shared`

A database per workspace, with real branch-from-parent semantics via PostgreSQL templates:

```yaml
services:
  - name: app-db
    type: shared
    service_type: postgres
    auto_workspace: true
    shared:
      image: postgres:17          # default
      port: 5432                  # fixed well-known port
      template_branching: true    # CREATE DATABASE … TEMPLATE parent (default true)
```

Creating workspace `feature_x` from `main` runs `CREATE DATABASE feature_x TEMPLATE main` — schema *and data* are copied engine-side. With `template_branching: false`, new workspaces get empty databases.

## ClickHouse — `type: shared`

A database per workspace via `CREATE DATABASE` (no template branching — databases start empty):

```yaml
services:
  - name: analytics
    type: shared
    service_type: clickhouse
    shared:
      image: clickhouse/clickhouse-server:latest   # HTTP on :8123
```

## Redis

Always shared/global. Each workspace gets a numbered DB index (0–15), allocated atomically and tracked inside Redis itself:

```yaml
services:
  - name: cache
    service_type: redis
    shared:
      image: redis:7        # default; port 6379
```

:::caution
Redis has only 16 databases **globally** — at most 15 workspace allocations across *all* projects sharing this Redis. Fine for a developer machine; not for big fleets.
:::

## RustFS object storage (S3-compatible)

One global RustFS container, one bucket per workspace named `{project}-{workspace}`:

```yaml
services:
  - name: storage
    service_type: rustfs        # aliases: s3, objectstorage
    shared:
      image: rustfs/rustfs:latest   # S3 API on :9000, console on :9001
      port: 9000
      user: rustfsadmin             # access key (default)
      password: rustfsadmin         # secret key (default)
```

## Keeping engines alive

Provisioning happens during `switch`; these commands keep the global containers themselves running:

```bash
devflow service up              # one-shot reconcile: start every shared engine that's down
devflow daemon start            # background controller, reconciles every 30s
devflow daemon start --interval 10
devflow daemon start --once     # same as service up
devflow daemon start --foreground
devflow daemon status           # last reconcile + per-engine health
devflow daemon stop
```

The daemon covers **every registered project's** shared engines (`type: shared`, plus `service_type: rustfs`/`redis`), restarting any that go down.

## Choosing shared vs local

| | `type: local` (CoW) | `type: shared` |
| --- | --- | --- |
| Isolation | process + data directory | logical (db/bucket/index) in one process |
| Startup | per-workspace container start | instant (engine already running) |
| Memory | one engine per workspace | one engine total |
| Ports | dynamic per workspace | fixed well-known port |
| Data branching | full CoW clone of everything | Postgres: TEMPLATE copy · others: empty |
| Reset/seed granularity | per container | per database/bucket |

Mixing is normal: CoW Postgres for the data you branch, shared Redis/RustFS for caches and blobs. For a step-by-step Docker Compose migration, see [Adding devflow to an existing project](/devflow/getting-started/existing-project/#migration-map-from-docker-compose).
