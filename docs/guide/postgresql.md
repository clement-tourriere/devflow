# PostgreSQL

A project has at most one PostgreSQL 18 server, and every workspace gets its own database on it, cloned from its source's with `CREATE DATABASE … TEMPLATE … STRATEGY FILE_COPY` and `file_copy_method = clone`, so on a reflink filesystem the clone shares blocks ([measured](copy-on-write.md)). A branch login connects to its own databases only, behind a per-project owner role and a metadata database named `devflow`. `devflow db …` addresses the PostgreSQL service whatever its key.

- [The server](#the-server)
- [Database branches](#database-branches)
- [Test and shadow databases](#test-and-shadow-databases-extra_databases)

## The server

```toml
[services.db]                      # the key is yours
engine = "postgres"
mode = "managed"                   # devflow runs the container through docker compose; or "external"
image = "postgres:18"              # managed: must be PostgreSQL 18 or newer
# port = 0                         # managed: fixed host port; 0 lets devflow allocate one and keep it
# bind = "127.0.0.1"               # managed: the address the port is published on
# admin_url = "postgres://prov:…@db.internal:5432/postgres"   # external: put it in devflow.local.toml, or DEVFLOW_PG_ADMIN_URL
# app_host = "db.example"          # host and port written into workspace URLs, when they differ from the admin endpoint
# app_port = 5432
# cow = "verified"                 # external: your declaration, after checking the server's filesystem
# require_cow = false              # true: refuse a branch unless copy-on-write is verified
# on_busy_source = "disconnect"    # disconnect | fail | wait (below)
# busy_wait = "30s"                # with "wait"
# wal_log_below = "256MiB"         # smaller sources use STRATEGY WAL_LOG unless copy-on-write is verified
# extra_databases = ["test", "shadow"]
```

A managed server is one container per project (`devflow db up|down|status`), with its `compose.yaml` and generated superuser password (`admin.json`) written 0600 under `postgres/<project id>/` in devflow's data directory. An external server needs PostgreSQL 18 or newer and a provisioner role with `CREATEDB CREATEROLE`, and should have `file_copy_method = clone` (without it branches are full copies, which devflow reports as `copy` and warns about); devflow changes only the databases it creates. [Remote data hosts](remote-data-hosts.md) covers the external server, a managed server on a remote Docker engine, and sandboxes. `devflow db psql` opens `psql` against the workspace's database without putting the password on a command line.

## Database branches

A new workspace's database is a clone of the database of the workspace its code comes from (`--from`, else the default workspace), unless you pass `--data-from WS|snapshot:NAME[@vN]|branch:ID|empty`. A branch `ID` is a UUID or any unique prefix of at least 8 hex characters, with or without dashes, so the short ids `db branch list` and `service list` show work in `--data-from branch:ID`, `--to branch:ID`, `db branch delete ID` and `service NAME drop ID`; an unknown or ambiguous prefix is an error that names the candidates. The default workspace's database is created empty the first time something needs it: `switch -c` (which warns about it) or `devflow setup` in the main checkout.

Cloning needs the source database to have no open sessions. `on_busy_source` under `[services.db]` chooses what happens when it has some:

| `on_busy_source` | Behavior | Default for |
| --- | --- | --- |
| `disconnect` | Terminate the source's sessions (their open transactions are aborted), then clone. The warning names what was ended: `terminated 2 session(s) on the source database (psql (app, pid 812, 172.17.0.1, idle in transaction), …)`. | a managed server bound to loopback |
| `fail` | Refuse, naming the sessions. | external and non-loopback servers |
| `wait` | Wait up to `busy_wait` (default `"30s"`) for the sessions to end on their own, then fail naming the ones that are left. Nothing is terminated. | (opt in) |

Sessions are named by application name, role, process id, client address and state (`idle in transaction`, `active`), never by query. Under `wait`, `busy_wait = "2m"` sets the time; it is an error without `on_busy_source = "wait"`.

How the database is copied follows the measured copy-on-write capability (see [Copy-on-write, measured](copy-on-write.md)): `STRATEGY FILE_COPY` when it is `verified` (measured on a managed server, declared with `cow = "verified"` on an external one), and, when it is `copy` or `unknown`, `STRATEGY WAL_LOG` for a source smaller than `wal_log_below` (default `"256MiB"`; `"0"` always uses `FILE_COPY`), because `FILE_COPY` forces checkpoints that cost more than the copy of a small database where blocks cannot be shared. The branch records the strategy it was cloned with (`clone_strategy` in `db branch list --json`, and in the setup detail).

### Test and shadow databases (`extra_databases`)

Branch logins cannot create databases (they keep `NOCREATEDB`), but Django's test runner, Prisma Migrate and Rails want one. List the names you need and every branch gets its own **scratch databases**:

```toml
[services.db]
engine = "postgres"
extra_databases = ["test", "shadow"]   # ^[a-z][a-z0-9_]{0,20}$, unique; not main, postgres, devflow or template*
```

- Each is exported as `<NAME>_DATABASE_URL` (`TEST_DATABASE_URL`, `SHADOW_DATABASE_URL`) in `.devflow/env` and to hooks, with the branch's own login and password, and masked like `DATABASE_URL`. `[env]` recipes can reference them (`${TEST_DATABASE_URL}`) but not set them; the `prisma` preset needs nothing more, because `SHADOW_DATABASE_URL` is already there for `shadowDatabaseUrl = env("SHADOW_DATABASE_URL")`.
- They are created empty, with the same closed-creation protocol as the branch's database, and named by devflow (`df_<project8>_<branch12>_<n>`), never from the label. A workspace cloned from another (or from a snapshot) gets its own empty ones; they are never copied and never part of a snapshot.
- `devflow db reset` empties them again: it drops and recreates each one, recorded step by step, so an interrupted reset is finished by `devflow db reconcile`. Credential rotation covers them (one login), and removing the workspace drops them with its branch (`--keep-data` keeps them).
- A name added to `devflow.toml` is provisioned for existing workspaces by the next `devflow setup` or `switch` (they read as `needs_attention` until then). A name removed stays on the server as a recorded **orphan**: its data is kept, it is not exported, `devflow db reconcile` and `devflow doctor` report it, and `devflow db reconcile --drop-orphans` drops it.
- A workspace attached to a database URL (`setup --database-url`) has no branch to add to: it gets no scratch databases, and a warning says so.

`devflow db branch list` shows them (`EXTRAS` column; `--json` has `extras` with names and databases, never passwords) and `devflow --json list` shows them in each receipt binding.

A project has at most one `postgres` service; `devflow db …` addresses it whatever its key. A workspace's receipt records each service's binding under `receipt.services.<key>` together with the configuration it was applied under: add a service or change its section in `devflow.toml` and every existing workspace becomes `needs_attention` (services, env and hooks are reopened; `--no-db` and `--no-hooks` skips are kept) until `devflow setup` or the next `switch` completes them. Removing a service never deletes its data.

A managed server's host port is fixed. With the default `port = 0`, devflow picks one from the project and service identity in 20000–39999 the first time the server starts, records it in the server's `admin.json` and publishes it on `127.0.0.1` from then on, so URLs survive restarts. If something else holds the recorded port later, `devflow db up` fails and names the port and the cause; it never moves it (set `port` yourself to choose another). On OrbStack the server is also reachable from other containers at `db.<project>-<id8>.devflow.orb.local:5432` (shown by `devflow db status` as `container_endpoint`); `DATABASE_URL` keeps using `127.0.0.1`.
