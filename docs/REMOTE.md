# Remote data hosts

Run PostgreSQL once, on a machine that stays up, and let code run elsewhere: laptops, local VM forks (for example Cove), or cloud sandboxes and microVMs. Each workspace still gets its own database branch, cloned in seconds on the data host. This page covers the setup. The design is in the [plan](plans/2026-10-10-devflow-1.0.md#remote-data-hosts).

```
              trusted                                       untrusted
   laptop / launcher (admin URL)                    VM fork / cloud sandbox
   devflow db branch create ... --json   ---->      devflow setup --database-url URL
              |                                                |
              | admin connection (provision, snapshot, drop)   | branch URL (one database)
              v                                                v
        +-------------------------------------------------------------+
        |  data host: PostgreSQL 18, file_copy_method = clone,        |
        |  data directory on btrfs / XFS (reflink) / ZFS 2.2+         |
        +-------------------------------------------------------------+
```

| Party | Holds | Can |
| --- | --- | --- |
| Trusted machine (laptop, launcher) | Admin URL, or Docker access to the data host | Create, snapshot, reset and drop any branch of the project |
| Sandbox | One branch URL | Connect to that one database |

## 1. Prepare the data host

The server must be PostgreSQL 18 or newer, and its data directory must be on a filesystem that supports reflinks: btrfs, XFS created with `reflink=1`, ZFS 2.2 or newer, or an OrbStack volume. On ext4 and Docker Desktop, branches still work but each one is a full copy, and for a managed server devflow reports `copy`. For an external server devflow cannot measure the filesystem: it reports `unknown` unless you declare `cow` (below), and `copy` only when `file_copy_method` is not `clone`. Set `require_cow = true` under `[postgres]` to refuse branches unless copy-on-write is verified.

Check the filesystem before you declare anything. From a directory on the same filesystem as `$PGDATA`:

```sh
dd if=/dev/zero of=probe bs=1M count=64 && cp --reflink=always probe probe.clone
rm -f probe probe.clone
```

`cp --reflink=always` fails if the filesystem cannot share blocks.

## 2. Choose a mode

### External server

Use an existing PostgreSQL 18 server. Enable cloning and create a provisioner role:

```sql
ALTER SYSTEM SET file_copy_method = 'clone';
SELECT pg_reload_conf();
CREATE ROLE prov LOGIN PASSWORD '…' CREATEDB CREATEROLE;
```

Allow the private network in `pg_hba.conf` (for example `host all all 100.64.0.0/10 scram-sha-256` for Tailscale) and make the server listen on that interface.

On an external server devflow changes only the databases it creates. Branch logins are closed out of devflow's own databases, but access to every other database on the server (including `postgres` and `template1`) is your responsibility: revoke `CONNECT` from `PUBLIC` on them or restrict `pg_hba.conf`. A dedicated server per project or team is the simplest arrangement. devflow creates a metadata database named `devflow` on the server, plus `df_*` databases for branches and snapshots and `dfo_*` and `dfl_*` roles; an existing database named `devflow` that devflow did not create is refused, never adopted. A managed server revokes `PUBLIC` `CONNECT` on `postgres` and `template1` itself.

Commit only what is not secret:

```toml
# devflow.toml
[postgres]
mode = "external"
app_host = "db.tailnet.example"      # host written into workspace URLs, if different from the admin endpoint
app_port = 5432                      # port written into workspace URLs, if different from the admin endpoint
```

Keep the admin URL on the trusted machine, in the git-ignored `devflow.local.toml` (or in `DEVFLOW_PG_ADMIN_URL`):

```toml
# devflow.local.toml
[postgres]
admin_url = "postgres://prov:…@db.internal:5432/postgres"
cow = "verified"                     # your declaration, after the check above; devflow cannot measure an external server
```

devflow shows whether a capability was measured or declared.

### Managed server on a remote Docker engine

devflow runs the container for you through `docker compose`, which honours `DOCKER_HOST` and `DOCKER_CONTEXT`:

```sh
docker context create datahost --docker "host=ssh://ops@datahost.example"
export DOCKER_CONTEXT=datahost
devflow db up
```

```toml
# devflow.toml
[postgres]
mode = "managed"
image = "postgres:18"
bind = "100.101.102.103"             # IP address (not a hostname) on the data host the container port is published on
port = 5432                          # recommended: fixed, so URLs survive restarts
app_host = "datahost.tailnet.example"
```

Docker's volume storage on the engine host must be on a reflink filesystem. devflow measures the real clone path once per server start and records `verified`, `copy` or `unknown`; `devflow db status` and `devflow doctor` show it. `compose.yaml` and the generated superuser password (`admin.json`, mode 0600) are written on the machine where you run `devflow db up`, not on the engine host, under `postgres/<project id>/` in devflow's data directory: `~/Library/Application Support/devflow` on macOS, `$XDG_DATA_HOME/devflow` (default `~/.local/share/devflow`) on Linux, or `DEVFLOW_DATA_DIR`. `compose.yaml` there also contains the superuser password (mode 0600).

Nothing enforces a fixed `port`. With `port = 0` Docker picks one and it changes when the container restarts: devflow rewrites the URLs of its own workspaces on the next `devflow switch`, but URL-only sandboxes are never refreshed.

With a bind address other than loopback, `on_busy_source` defaults to `fail`: cloning a database that has open connections is refused. On loopback a managed server defaults to `disconnect`, which terminates the open sessions of the source database. Set `"disconnect"` here only if aborting those sessions is acceptable.

## 3. Networking

Docker over SSH reaches only the Docker API. The PostgreSQL port must also be reachable from every machine that runs code, so put them on a private network such as Tailscale or WireGuard.

**devflow 1.0 has no TLS support.** It does not set up server certificates, and its own connections to the server are unencrypted. Do not expose the port to the internet. Bind to the private interface, and restrict `pg_hba.conf` and the firewall to the private network. If a link crosses an untrusted network, tunnel it.

## 4. Provision a sandbox

On the trusted machine, create a branch from a snapshot and hand over only its URL:

```sh
devflow db snapshot create seed                              # once, from a prepared workspace
creds=$(devflow db branch create --data-from snapshot:seed --json)
BRANCH_ID=$(jq -r '.branch.id' <<<"$creds")                  # keep this for cleanup
BRANCH_URL=$(jq -r '.url' <<<"$creds")                       # hand only this to the sandbox
```

The document also has `host`, `port`, `user`, `password` and `database`, and a `warnings` array when there is something to report. `db branch create` takes `--data-from` (default: `empty`) and `--name` (default: generated), and starts a managed server that is not running.

To make launcher retries safe, pass a stable id: `devflow db branch create --instance-id UUID …` reuses the branch created for that id and sets a new password instead of creating a second branch, which invalidates any URL handed out earlier.

`db branch create` and `db url` are the only commands that print the password (it is also in the generated `.devflow/env`, mode 0600). Inject the URL into the sandbox through your launcher's secret mechanism. Inside the sandbox, with the repository checked out:

```sh
devflow setup --database-url "$BRANCH_URL"
```

This records the URL without provisioning, writes `.devflow/env`, and runs the setup hooks. Hooks from the repository need trust in the sandbox: run `devflow trust` once, or set `DEVFLOW_TRUST_HOOKS=1` when the launcher controls the repository content. A URL-only attachment is detach-only: `devflow remove` in the sandbox never drops the branch.

## 5. Forks

A VM fork copies the worktree, including its receipt and database binding. Give the fork its own identity:

```sh
devflow setup --new-instance            # or run `devflow setup` with DEVFLOW_INSTANCE_ID=UUID set
```

This marks the database, env and hooks phases pending and provisions a new branch, from `--data-from` if given, otherwise as a clone of the previous binding's branch, otherwise from the recorded data source. It needs the admin URL, so use it only in a trusted fork. The copied binding has no authority over the original branch: only the instance that created a branch may reset or drop it.

For an untrusted fork, provision from the trusted side and attach the result:

```sh
devflow setup --new-instance --database-url "$BRANCH_URL"
```

## 6. What a sandbox must never get

| Secret | Why |
| --- | --- |
| The admin URL, `DEVFLOW_PG_ADMIN_URL`, or a `devflow.local.toml` containing it | Full control of every branch, snapshot and role in the project |
| `admin.json` and the managed server's `compose.yaml` | The superuser password of a managed server |
| Docker access to the data host (socket, context, SSH key) | A shell in the database container is superuser access |
| Another branch's URL | Direct access to that branch's data |

A branch URL is a login for one database. A branch role cannot connect to sibling branches or snapshots, but the server's own limits (connections, disk) are the only limits on what that database can use; devflow adds none in 1.0. A provisioning API for untrusted sandboxes (`devflow serve`) is deferred.

## 7. Clean up

Branches outlive the sandboxes that use them until the trusted side drops them:

```sh
devflow db branch list                     # includes branches no local worktree is bound to
devflow db branch delete BRANCH_ID
devflow db snapshot list
devflow db snapshot delete seed@v1
devflow db reconcile                       # finish or roll back interrupted operations
```

Run the delete from your launcher when it destroys a sandbox, and list periodically for strays. `devflow db down` stops the managed server and every branch of the project.
