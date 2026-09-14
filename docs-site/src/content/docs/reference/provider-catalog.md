---
title: Provider catalog
description: Generated provider configuration and operation contracts.
---

Generated from `devflow --json capabilities`. Build availability is reported at runtime in `available`; this table lists the catalog across builds. Plugins negotiate their own capabilities. Local Redis without a `generic` block retains legacy shared allocation.

| Provider | Services | Data strategy | Operations | Required setup | Allocation limit |
| --- | --- | --- | --- | --- | --- |
| local | postgres | physical_copy | logs, reset, seed, snapshot, start, stop | none | — |
| local | mysql, clickhouse, redis, generic | per_workspace_container | logs, start, stop | none | — |
| shared | postgres, clickhouse, rustfs | logical_namespace | none / negotiated | none | — |
| shared | redis | empty_database | none / negotiated | none | 15 |
| neon (experimental) | postgres | remote_branch | none / negotiated | neon.api_key, neon.project_id | — |
| dblab (experimental) | postgres | remote_clone | none / negotiated | dblab.api_url, dblab.auth_token | — |
| xata (experimental) | postgres | remote_branch | none / negotiated | xata.api_key, xata.organization_id, xata.project_id | — |
| plugin (experimental) | plugin | negotiated | none / negotiated | plugin.path or plugin.name | — |
