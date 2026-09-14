---
title: Cloud providers
description: Experimental managed-Postgres branching with Neon, DBLab, and Xata.
sidebar:
  order: 10
  badge:
    text: Experimental
    variant: caution
---

Cloud providers map devflow workspaces onto managed database branching — no local containers at all. They implement the same provider interface, so `switch`, `connection`, hooks, and cleanup behave identically.

:::caution
Neon, DBLab and Xata are **experimental** and **untested against live accounts for v0.8.0**. Compilation and contract tests do not establish live API compatibility. Enable their optional build features explicitly; they are excluded from the standard release binary.
:::

## Neon

Workspace = [Neon branch](https://neon.tech). Instant CoW branching on Neon's storage.

```yaml
services:
  - name: cloud-db
    type: neon
    service_type: postgres
    auto_workspace: true
    neon:
      api_key: ${NEON_API_KEY}          # ${ENV_VAR} interpolation supported
      project_id: ${NEON_PROJECT_ID}
      base_url: https://console.neon.tech/api/v2   # default
```

## DBLab (Database Lab Engine)

Workspace = DBLab clone — thin clones of a full-size PostgreSQL instance, great for production-sized data.

```yaml
services:
  - name: staging-db
    type: dblab
    service_type: postgres
    auto_workspace: true
    dblab:
      api_url: https://dblab.example.com
      auth_token: ${DBLAB_TOKEN}
```

## Xata

Workspace = Xata branch (PostgreSQL-compatible platform).

```yaml
services:
  - name: xata-db
    type: xata
    service_type: postgres
    auto_workspace: true
    xata:
      api_key: ${XATA_API_KEY}
      organization_id: my-org
      project_id: my-project
      base_url: https://api.xata.tech    # default
```

## Notes

- Secrets stay out of the committed file via `${ENV_VAR}` interpolation; put real values in your environment or `.devflow.local.yml`.
- Cloud services are skipped by `devflow service up` / the daemon (nothing local to keep alive).
- Mixing is fine: a Neon database alongside local Redis and RustFS in one project.
