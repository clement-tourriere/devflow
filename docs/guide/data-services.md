# Data services

Every workspace gets data of its own. A project lists its data services as `[services.<key>]` tables (the key is yours: lowercase letters, digits, `_` and `-`, starting with a letter), each selected by its `engine`:

| Engine | What a workspace gets | Needs | Page |
| --- | --- | --- | --- |
| `postgres` | A database on the project's one PostgreSQL 18 server, cloned with `CREATE DATABASE … TEMPLATE …`; its own login, and optional scratch databases | Docker for a managed server, or an external PostgreSQL 18 server | [PostgreSQL](postgresql.md) |
| `valkey`, `redis` | A container and volume of its own, cloned from the source's dump with a reflink | Docker | [Valkey and Redis](valkey-redis.md) |
| `mysql` (preview) | A MySQL 8.4 container; the data directory is reflinked while the source is stopped for a few seconds | Docker | [MySQL](mysql.md) |
| `sqlite` | A copy of the database file in the worktree, made under SQLite's own locks | Nothing | [SQLite](sqlite.md) |

A project has at most one `postgres` and one `mysql` service, and two services cannot export the same variable (a configuration error names both). Each service's connection variable is in `.devflow/env` and in the hooks' environment, masked in every output: `DATABASE_URL` and the `PG*` variables for PostgreSQL, `REDIS_URL` for Valkey and Redis, `DATABASE_URL` (or `MYSQL_URL`) and `MYSQL_*` for MySQL, `SQLITE_PATH` and a `sqlite:` URL for SQLite.

## Where a workspace's data comes from

Code and data origins are chosen separately and are immutable once chosen: `--from WS` is the code (and payload) source, `--data-from SRC` the data source, where `SRC` is a workspace, `snapshot:NAME[@vN]`, `branch:ID` (not for SQLite) or `empty`. Without `--data-from` a workspace takes the data of its code source, and the default workspace starts empty. A source that a service does not have fails that service and is never replaced by an empty one. A branch `ID` is a UUID or any unique prefix of at least 8 hex characters, with or without dashes, so the short ids `db branch list` and `service list` show work in `--data-from branch:ID`, `--to branch:ID`, `db branch delete ID` and `service NAME drop ID`; an unknown or ambiguous prefix is an error that names the candidates. `--no-db` skips the services phase for a workspace, and the skip is kept.

A snapshot is an immutable, versioned copy of one service's data under a name (`devflow db snapshot create NAME`, `devflow service NAME snapshot create NAME`; `NAME@vN` picks a version). Cloning from a snapshot stops nothing and has no live source to disturb. `reset` (`devflow db reset`, `devflow service NAME reset`) restores a workspace's data to its recorded source, or to `--to SRC`.

## Configuration changes

A workspace's receipt records, per service, its binding and the configuration generation it was applied under. Adding a service or changing a service's section in `devflow.toml` makes every existing workspace `needs_attention` (the `services`, `env` and `hooks` phases are reopened; `--no-db` and `--no-hooks` skips are kept) until `devflow setup` or the next `switch` completes them. Settings that only shape how a later clone runs (`busy_wait` and `wal_log_below` of a PostgreSQL service, `busy_wait` of a SQLite one) are not part of that. Removing a service never deletes its data: for a container or SQLite service `devflow service list --orphans` and `reconcile` report what is left and `devflow service NAME forget` removes it; a PostgreSQL service's server and branches stay where they are. Removing a workspace drops the branches that workspace created (`--keep-data` keeps them).

## Cost and speed

Cloning is as cheap as the storage under the data: [copy-on-write, measured](copy-on-write.md) reports `verified`, `copy` or `unknown` for every service and what it costs. `require_cow = true` on a service refuses a clone that would be a full copy.

## Seed data for automation (`[data]`)

A run that cannot prompt (`--non-interactive`, `--json`, no terminal, the Git hook) and was not told where the data comes from (`--data-from`) can start from a snapshot you keep for the purpose:

```toml
[data]
noninteractive_from = "snapshot:seed"   # "snapshot:NAME" or "snapshot:NAME@vN"
noninteractive_max_age = "7d"           # optional: warn when the snapshot is older
# noninteractive_fallback = "empty"     # optional: a service without the snapshot starts empty
```

Snapshots are per service and named (`devflow db snapshot create`, `devflow service NAME snapshot create`): each service takes its data from its own snapshot of that name. A service **without** it fails, with remedies (create the snapshot, or pass `--data-from`): its usual data source is a live workspace, and cloning one ends PostgreSQL sessions or stops MySQL, which a run nobody watches must not do on its own. `noninteractive_fallback = "empty"` is the one opt-out: such a service starts empty, with a warning naming it. A snapshot older than `noninteractive_max_age` (counted from when its data was taken) is used with a warning. The seed applies to the creation or adoption of a workspace; `--data-from` and an interactive run never use it, and neither does the default workspace, whose root branch is shared and always created empty. A data source stays immutable once chosen (a retry keeps the source it pinned), so changing `[data]` does not touch existing workspaces. Unknown keys under `[data]` are errors like everywhere else.
