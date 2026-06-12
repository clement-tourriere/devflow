---
title: Services & providers
description: The service provider model — engines, lifecycle states, Copy-on-Write cloning, and how branching propagates to your data.
sidebar:
  order: 3
---

A **service** is a named, stateful dependency of your project — a database, cache, or object store — that devflow branches alongside your code. Each service is backed by a **provider** that knows how to create, clone, switch, and destroy per-workspace instances.

## Providers at a glance

| Provider | Type | Isolation | Engines |
| --- | --- | --- | --- |
| Local Docker | `type: local` | physical — one CoW container per workspace | PostgreSQL, ClickHouse, MySQL, generic (any image) |
| Shared engine | `type: shared` (or implied) | logical — one global container, one db/bucket/index per workspace | PostgreSQL, ClickHouse, Redis, RustFS (S3-compatible) |
| Cloud (experimental) | `type: neon` / `dblab` / `xata` | provider-managed branching | PostgreSQL |
| Plugin | `service_type: plugin` | up to the plugin | anything — JSON-over-stdio protocol |

Multiple services coexist in one project (e.g. CoW Postgres + shared Redis + RustFS), and each declares `auto_workspace` — whether it follows Git branching automatically (default `true`) or stays global.

```yaml
services:
  - name: app-db
    type: local
    service_type: postgres
    default: true            # target of `-s`-less commands
    local:
      image: postgres:17
  - name: cache
    service_type: redis       # shared engine, DB index per workspace
  - name: storage
    service_type: rustfs      # shared engine, bucket per workspace
```

## How branching propagates

```
git checkout -b feature/x        (or: devflow switch -c feature/x)
        │
        ▼
post-checkout hook → devflow git-hook
        │
        ▼
for each service with auto_workspace:
  local:   clone parent's data dir (CoW) → start container on its own port
  shared:  CREATE DATABASE feature_x TEMPLATE main   (or bucket / DB index)
        │
        ▼
lifecycle hooks fire → .env.local updated, migrations run
```

Branch filtering (`git.workspace_filter_regex`, `exclude_workspaces`) and env toggles (`DEVFLOW_AUTO_CREATE=false`, …) control when this fires — see [configuration](/devflow/reference/configuration/#git).

## Copy-on-Write cloning

For `type: local`, creating a workspace clones the parent's **entire data directory**. On a CoW filesystem the clone is near-instant and uses almost no extra disk — only blocks that change afterwards are duplicated.

| Filesystem | Method |
| --- | --- |
| APFS (macOS) | `cp -c` clonefile |
| ZFS (Linux) | dataset snapshot + clone (`devflow setup-zfs` creates a file-backed pool) |
| Btrfs / XFS (Linux) | reflink copy |
| Anything else | full copy fallback |

`devflow doctor` and `devflow capabilities` report which method is active. With ZFS, each project gets a dataset and each workspace a zero-copy clone of the parent snapshot:

```
devflow/myapp            # project dataset
devflow/myapp@main       # snapshot of main
devflow/myapp/feature    # instant clone
```

## Service lifecycle

Each local service workspace moves through these states:

```
Provisioning ──▶ Running ──▶ Stopped ──▶ (deleted)
     │              │            │
     └──────────▶ Failed ◀──────┘
```

| State | Meaning | Commands |
| --- | --- | --- |
| Provisioning | container being created, data cloning | `service create` |
| Running | accepting connections | `service start`, `switch` |
| Stopped | container stopped, data preserved | `service stop` |
| Failed | crashed or failed to start | `service logs`, `service reset` |

`devflow service reset <ws>` re-clones from the parent — the "give me a clean database" button, ideal for agent retries.

## Connection info

Every provider exposes uniform connection info per workspace — host, port, database, user, password, URL — surfaced by:

- `devflow connection <ws> [--format uri|env|json]`
- hook templates: `{{ service['app-db'].url }}`, `{{ service.cache.port }}`, …
- `devflow agent context --format json`

## Going further

- [Local containers](/devflow/guides/local-containers/) — engine-specific options, ports, data roots
- [Shared engines](/devflow/guides/shared-engines/) — logical isolation + the controller daemon
- [Seeding](/devflow/guides/seeding/) — load dumps, live databases, or S3 backups
- [Cloud providers](/devflow/guides/cloud-providers/) and [plugins](/devflow/guides/plugins/)
