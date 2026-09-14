---
title: Reusable data baselines
description: Prepare immutable PostgreSQL datasets and reset workspaces to an exact version.
sidebar:
  order: 5
---

A baseline is a dedicated PostgreSQL dataset, independent of a Git branch. Preparing a baseline imports a local SQL/custom dump, optionally applies migration and validation SQL files, verifies connectivity, stops the container, and seals the generation. Baselines currently require `service_type: postgres` with `type: local`.

```sh
devflow baseline prepare clean --service db --from ./fixtures/base.sql \
  --migrate ./fixtures/migrate.sql --validate ./fixtures/validate.sql
devflow --json baseline list
devflow --json baseline inspect baseline:clean@v1

devflow --json --non-interactive switch -c agent/login \
  --from main --data-from baseline:clean@v1
devflow exec --workspace agent/login -- pytest -k 'login or signup'
devflow service reset agent/login --service db
```

Use the exact reference returned by `prepare`. Each preparation allocates a new version. A child records its baseline reference, resource generation, and engine image identity; resetting verifies that binding and restores its original version. A sealed baseline rejects ordinary start, switch, seed, reset, and deletion operations. Physical children use the sealed source's image digest.

The selected baseline supplies its configured service. Other automatically provisioned services start empty. Without a baseline, clone-capable services inherit the data workspace; Redis, generic containers, object storage, and shared ClickHouse start with an empty namespace. Inspect `workspaces[].control.data` in `devflow --json list` for the per-service bindings.

Keep a dedicated baseline for repeatable tasks instead of cloning an actively edited development database. Physical PostgreSQL copies require a quiescent source. A stopped baseline avoids interrupting a developer's running source container.

An import or validation failure leaves a visible failed preparation and its operation record. SQL imports and migrations can have partial effects: inspect the failure, remove that unused version, and prepare a new version. Devflow does not promise exactly-once execution of arbitrary SQL or shell commands.

```sh
devflow operation inspect OPERATION_ID
devflow baseline remove baseline:clean@v1
```

Baseline removal refuses live dependent workspaces. Retention cleanup always preserves recorded baselines. Remove children first using the ordinary workspace removal workflow; deleting a code parent does not rewrite its descendants' provenance.
