---
title: Workspaces & isolation
description: What a devflow workspace is, how names map to branches, databases, and directories, and how the two isolation models work.
sidebar:
  order: 1
---

A **workspace** is devflow's unit of isolation: one VCS branch plus everything that belongs to it — optionally a worktree directory, one service workspace per configured service, hook-generated files, and registry state.

```
Git branch  feature/auth
 ├─ worktree          ../myapp.feature_auth          (optional)
 ├─ service app-db    postgres container or database  (isolated)
 ├─ service cache     redis DB index                  (isolated)
 └─ .env.local        written by hooks                (per-workspace values)
```

## Architecture

Three frontends (CLI, TUI, desktop GUI) drive one shared core:

```
 ┌────────────┐ ┌────────────┐ ┌────────────┐
 │ Desktop GUI│ │    TUI     │ │    CLI     │
 └─────┬──────┘ └─────┬──────┘ └─────┬──────┘
       └──────────────┼──────────────┘
                      ▼
 ┌──────────────────────────────────────────┐
 │               devflow-core               │
 │  workspace lifecycle · hook engine ·     │
 │  VCS layer (git/jj) · service providers ·│
 │  config · state · reverse proxy          │
 └──────────────────────────────────────────┘
```

The lifecycle (create → switch → remove) lives in core, so every frontend gets the same behavior: hooks fire, services follow, state stays consistent.

## Two isolation models

Chosen **per service** in `.devflow.yml`; a project can mix both.

### Physical isolation — `type: local`

One Copy-on-Write Docker container per workspace. Creating a workspace clones the parent's entire data directory (APFS clone / ZFS snapshot / reflink — near-instant, near-zero extra disk). Strongest isolation: separate process, port, and data.

```yaml
services:
  - name: app-db
    type: local
    service_type: postgres   # postgres | clickhouse | mysql | generic | plugin
    local:
      image: postgres:17
```

### Logical isolation — `type: shared`

One global container per engine; each workspace gets a logical boundary provisioned on the fly:

| Engine | Per-workspace unit | Branching semantics |
| --- | --- | --- |
| PostgreSQL | database | `CREATE DATABASE … TEMPLATE parent` (branch-from-parent) |
| Redis | numbered DB index (0–15) | none — empty DB per workspace |
| RustFS (S3) | bucket (`{project}-{workspace}`) | none — empty bucket |
| ClickHouse | database | none — empty database |

```yaml
services:
  - name: cache
    service_type: redis      # always shared/global
  - name: app-db
    type: shared
    service_type: postgres
    shared:
      port: 5432             # fixed well-known port
```

Shared engines use one fixed port, start faster, and consume less memory — at the cost of weaker isolation (shared process). The [controller daemon](/devflow/guides/shared-engines/#keeping-engines-alive) keeps them running.

See [Local containers](/devflow/guides/local-containers/) and [Shared engines](/devflow/guides/shared-engines/) for full configuration.

## Workspace names

Git branch names are sanitized wherever they become identifiers (database names, container names, worktree directories, registry keys):

- lowercased,
- every character outside `[a-z0-9_$]` becomes `_` (so `/`, `-`, `.` all map to `_`),
- leading digits stripped, doubled `__` collapsed,
- truncated to 63 characters with a hash suffix if longer.

`feature/Auth-System` → `feature_auth_system`. The raw branch name is preserved in Git and in hook templates as `{{ workspace }}`; the sanitized form is `{{ workspace_sanitized }}`.

:::caution
Distinct branches can normalize to the same name (`feature/auth` and `feature-auth` both become `feature_auth`) — they would share a service workspace and worktree path. Avoid branch names that differ only in separators or case.
:::

## State & identity

Workspace metadata (parent relationships, worktree paths, executed commands) lives in `~/.config/devflow/local_state.yml` — machine-local, never committed. Project identity is the **canonical main-repo root**: commands run from inside a worktree resolve to the same project as the main checkout, so registries, hook approvals, and lookups agree no matter where you invoke devflow.

## Parent relationships

Every created workspace records a parent (`--from <ws>`, or your current context branch). Parents drive:

- **service branching** — the new database is cloned from the parent's,
- **`devflow graph`** — the rendered tree.

Override the inferred context with `DEVFLOW_CONTEXT_BRANCH=<ws>` (useful in CI).
