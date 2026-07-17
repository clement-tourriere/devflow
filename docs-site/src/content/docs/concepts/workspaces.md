---
title: Workspaces & isolation
description: What a devflow workspace is, how identity and lineage work, and how the two service-isolation models fit into it.
sidebar:
  order: 1
---

A **workspace** is devflow's unit of isolation: one materialized VCS directory plus its service instances, processes, hook-generated files, and registry state. Git uses the primary checkout for the default workspace and a linked worktree for every additional workspace. Jujutsu uses native workspaces.

For jj, the raw devflow workspace name is also a bookmark. `devflow commit` advances that bookmark to the commit it just finalized, and workspace removal refreshes it before forgetting the native workspace. A direct `jj commit` does not advance bookmarks automatically; if you commit outside devflow and need the bookmark immediately, run `jj bookmark set <workspace> --revision @-`.

devflow treats jj's native primary workspace (internal name `default`) as the stable project root. Keep that internal workspace registered under `default`; if it is renamed or forgotten, devflow fails closed instead of guessing that another workspace is the primary. Raw user-facing workspace identities remain bookmarks and are unaffected by this internal-name requirement.

```
Git worktree  feature/auth
 ├─ directory         ../myapp.feature_auth_fc659bd73585
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

## Workspace identity

devflow keeps two names with different jobs:

- **`name`** is the raw VCS identity, such as `feature/Auth-System`. It is shown in every frontend, passed to VCS operations, and exposed to hooks as `{{ workspace }}`.
- **`service_key`** is a deterministic, database/file-safe identity for services and generated paths. Already-safe names are preserved; names that need normalization receive a stable short hash, so `feature/auth`, `feature-auth`, and case variants never collide. Hooks expose it as `{{ workspace_key }}` and as the `{{ workspace_sanitized }}` compatibility alias.

Never reconstruct a workspace name from its `service_key`; use the raw `name` field for `switch`, `remove`, and other VCS operations.

## State & identity

Workspace metadata (creation parents, paths, service keys, executed commands) lives in `~/.config/devflow/local_state.yml` — machine-local, never committed. Project identity is the **canonical main-repo root**: commands run from inside any worktree resolve to the same project as the primary checkout, so registries, hook approvals, and lookups agree.

`devflow list` reconciles this state with live Git worktrees or jj workspaces, services, and processes. Its JSON output is one versioned tree document for zero, one, or many services: `schema_version`, project/VCS metadata, `context_workspace`, `default_workspace`, `roots`, workspace nodes, and `warnings`. `context_workspace` is derived from the VCS workspace containing the project path used for the request; it does not imply other worktrees are inactive.

When upgrading from the older lossy naming scheme, devflow recovers raw names only where a live worktree gives an unambiguous match. That workspace keeps its persisted legacy `service_key`, so existing services and process state remain visible without risky renames. Ambiguous ownership is shown in inventory warnings and service/process operations fail before creating a parallel namespace or attaching another workspace's data. Inventory nodes expose `identity_status` (`canonical`, `legacy_adopted`, or `legacy_unresolved`) plus `canonical_service_key`, so automation can handle recovery without parsing warning text.

## Parent relationships

Every created workspace records its parent (`--from <ws>`, or the current context workspace). This is immutable creation/clone provenance, not inferred commit ancestry. Parents drive:

- **service branching** — the new database is cloned from the parent's,
- **`devflow list`** — the rendered parent tree in CLI, TUI, GUI, and JSON.

Deleting a parent does not rewrite its children. Inventory keeps the recorded relationship and marks that parent as missing/deleted, which preserves how service data was originally cloned.

Override the inferred context with `DEVFLOW_CONTEXT_BRANCH=<ws>` (useful in CI).
