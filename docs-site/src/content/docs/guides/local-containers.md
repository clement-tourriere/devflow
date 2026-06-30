---
title: Local containers (CoW)
description: Physical isolation with one Copy-on-Write Docker container per workspace — PostgreSQL, ClickHouse, MySQL, and generic images.
sidebar:
  order: 2
---

`type: local` gives every workspace its own container with a Copy-on-Write clone of the parent's data. Strongest isolation, instant creation on APFS/ZFS/Btrfs/XFS.

## PostgreSQL

```yaml
services:
  - name: app-db
    type: local
    service_type: postgres
    auto_workspace: true        # follow git branching (default true)
    default: true               # default target for -s-less commands
    local:
      image: postgres:17
```

## ClickHouse

```yaml
services:
  - name: analytics
    type: local
    service_type: clickhouse
    clickhouse:
      image: clickhouse/clickhouse-server:latest
      port_range_start: 59000        # HTTP port (native protocol = HTTP + 877)
      data_root: ~/.local/share/devflow
      user: default
      password: ""
```

## MySQL

```yaml
services:
  - name: app-mysql
    type: local
    service_type: mysql
    mysql:
      image: mysql:8
      port_range_start: 53306
      data_root: ~/.local/share/devflow
      root_password: dev
      database: myapp
      user: dev
      password: dev
```

## Generic (any Docker image)

For services without a dedicated backend — search indexes, queues, anything:

```yaml
services:
  - name: search
    type: local
    service_type: generic
    auto_workspace: false              # one shared instance for all workspaces
    generic:
      image: opensearchproject/opensearch:2
      port_mapping: "9200:9200"        # fixed host:container mapping…
      # port_range_start: 56000        # …or dynamic allocation
      environment:
        discovery.type: single-node
      volumes:
        - "/data/search:/usr/share/opensearch/data"
      command: ""                      # override container command
      healthcheck: "curl -fs localhost:9200"
```

:::tip
For Redis, prefer `service_type: redis` (a [shared engine](/devflow/guides/shared-engines/#redis) with a DB index per workspace) over a generic container — unless you need full physical isolation.
:::

## Day-to-day commands

```bash
devflow service create feature/x          # create instances without switching VCS
devflow service start feature/x           # start a stopped container
devflow service stop feature/x            # stop (data preserved)
devflow service reset feature/x           # re-clone from parent — clean slate
devflow service logs feature/x --tail 50
devflow service connection feature/x --format env
devflow service delete feature/x          # delete instances, keep branch + worktree
devflow service cleanup --max-count 10    # drop oldest service workspaces
```

Target a specific service in multi-service projects with `-s <name>`; the `default: true` service is used otherwise.

## Ports and discovery

Each workspace container gets its own host port, allocated from `port_range_start` upward. `devflow connection` (and hook templates) always reflect the live assignment — never hardcode ports; render them:

```yaml
hooks:
  post-switch:
    env: "echo DATABASE_URL={{ service['app-db'].url }} > .env.local"
```

Already running containers (started outside devflow) can be adopted: `devflow service discover` lists candidates and generates config. For incremental Compose migrations and hybrid setups, see [Adding devflow to an existing project](/devflow/getting-started/existing-project/#hybrid-rollout-patterns).

## Storage layout

Data directories live under `data_root` (default `~/.local/share/devflow`), one per service per workspace, cloned from the parent on creation. `devflow doctor` shows the active CoW method; `devflow setup-zfs` provisions a file-backed ZFS pool on Linux when no CoW filesystem is available (see [Installation](/devflow/getting-started/installation/#copy-on-write-storage)).
