# Architecture

devflow 1.0 creates Git worktrees, clones their payloads copy-on-write, branches PostgreSQL databases, and runs trusted setup hooks. Everything else (processes, the HTTPS proxy, toolchains, dependency installation) belongs to pitchfork and mise.

This page is the map. The design, schema and rationale are in [docs/plans/2026-10-10-devflow-1.0.md](plans/2026-10-10-devflow-1.0.md), the authoritative spec. Coming from 0.x? See [MIGRATING.md](MIGRATING.md).

## Crates

```
Cargo.toml              workspace + `devflow` CLI package (bin `devflow`)
src/                    CLI: clap parsing, JSON output, shell integration, prompts
crates/devflow-core/    all behavior; blocking API, nothing async public, no GPUI types
crates/devflow-gui/     GPUI desktop app (bin `devflow-gui`), gpui-kit pinned to =0.7.1
```

Dependencies point one way: `devflow` and `devflow-gui` depend on `devflow-core`; core depends on neither. Frontends build typed requests, call core, and render the result. The CLI calls core directly. The GUI calls it from `cx.background_spawn` and applies a result only if its request key (project and generation) still matches the selected project.

The GUI is split into `state` (selected project, request keys), `jobs` (background calls, typed failure kinds, masking), `ops` (calls into core), `model` (view rows), `activity` (progress panel), `settings` (editor, terminal, theme in `gui.toml`) and `views/` (pages and dialogs).

## Core modules (`crates/devflow-core/src`)

| Module | Responsibility |
| --- | --- |
| `config` | `devflow.toml` schema, merge with `devflow.local.toml`, raw-text editing with concurrent-edit detection, detection of 0.x files |
| `conn` | libpq-compatible parsing of connection URLs and conninfo strings; the `PG*` variables for a connection and the ones to clear |
| `error` | Typed errors frontends act on: `NotInitialized`, `Busy`, `Protected`, `ConfigChanged` |
| `project` | Project discovery: the main checkout, its configuration and the Git common directory |
| `registry` | Machine-local list of known projects for the GUI and `devflow list --all` (`projects.toml`); removing an entry never touches the project or its data |
| `worktree` | Create, list, protect and remove Git worktrees (`git2`) |
| `cow` | Clone files and directories: APFS `clonefile`, Linux `FICLONE`, copy fallback; payload staging and path validation |
| `receipt` | Per-worktree receipt: instance id, data binding, phase states, receipt lock |
| `setup` | The idempotent pipeline shared by `switch` and `setup`; status computation |
| `hooks` | Run `setup`/`switch`/`teardown` commands with `sh -c`, env, timeout, process groups and masked logs |
| `redact` | Credential masking for output and logs: exact match for known secrets, patterns for the rest |
| `trust` | SHA-256 trust of the canonicalized `[hooks]` section (`trust.toml`) |
| `githook` | Install and uninstall the `post-checkout` hook; `--if-new` detection |
| `env_file` | Write and read `.devflow/env`; keeps `.devflow/` self-ignored |
| `db::admin` | Admin connection, version check (>= 18), generated names, SQL quoting, session advisory lock |
| `db::meta` | `devflow` metadata database: bootstrap, schema migration, projects, branches, snapshots, retired roles |
| `db::steps` | Single DDL steps on the lock session: owner and login roles, closed creation and cloning, ACL normalization, grants, opening, busy-source sessions, drop and the reset swap |
| `db::managed` | Managed server: `compose.yaml`, `docker compose` lifecycle, CoW probe, `admin.json` |
| `db::ops` | Branch operations: create, clone, snapshot, reset, drop, credential rotation |
| `db::reconcile` | Crash recovery: completes or rolls back interrupted creates, deletes and resets |
| `doctor` | Checks: project config, Git, worktree-directory CoW, Docker/compose, server reachability and version, `file_copy_method`, CoW capability (measured or declared), hook trust, Git hook state, pitchfork and mise, workspaces needing attention |
| `pitchfork` | Optional adapter over the `pitchfork` CLI (`list --json`, start, stop) |
| `paths` | Config and data directories (`DEVFLOW_CONFIG_DIR`, `DEVFLOW_DATA_DIR` override) |

## Principles

- **Data lifetime is independent of code lifetime.** A database branch has its own identity; a worktree or VM attaches to it. Code origin (`--from`) and data origin (`--data-from`) are separate and immutable once chosen.
- **Never infer ownership from names.** Branches, snapshots and roles are recorded in a metadata database on the server. Worktrees carry a receipt with their instance id. Generated database and role names are never derived from user input.
- **Resumable, not transactional.** Every multi-step operation records intent before its external effect and can be re-run to completion.
- **Report what actually happened.** CoW is `verified`, `copy` or `unknown`; it is never assumed.
- **Synchronous core.** Blocking API, no async runtime, no UI types.
- **No connection data on a command line.** `devflow db psql` parses the workspace URL like libpq does (`conn`), drops every connection variable inherited from the caller, and sets the workspace's `PG*` variables, password included, in psql's own environment (or the docker client's, forwarded into the container by name). argv is readable by every local user; a process's environment only by the same user and root, the audience of a 0600 file, and no temporary file has to be cleaned up. devflow then replaces itself with psql, so psql handles Ctrl-C and signals itself.

## Where state lives

| What | Where |
| --- | --- |
| Project config | `devflow.toml` (or `.devflow.toml`) in the repository, committed |
| Machine-local config | `devflow.local.toml` beside it, git-ignored, merged on top |
| Project registry (GUI, `list --all`) | `<config dir>/projects.toml` and `projects.lock`. Config dir: `$XDG_CONFIG_HOME/devflow` (macOS: `~/Library/Application Support/devflow`), or `DEVFLOW_CONFIG_DIR` |
| GUI settings | `<config dir>/gui.toml` |
| Hook trust | `<config dir>/trust.toml`: `{project_id, main checkout}` to the SHA-256 of the merged `[hooks]` section |
| Workspace receipt | `<per-worktree git dir>/devflow/receipt.json` (0600) and its lock `setup.lock`; removed with the worktree |
| Worktree creation lock | `<common git dir>/devflow/create.lock` |
| Setup logs | `<per-worktree git dir>/devflow/logs/<timestamp>-<setup\|switch\|teardown>.log` (0600, masked) |
| Generated env | `<worktree>/.devflow/env` (0600; the directory ignores itself) |
| Managed server | `<data dir>/postgres/<project id>/`: `compose.yaml` and `admin.json` (both 0600; they hold the superuser password; `admin.json` also caches the CoW probe). Data dir: `$XDG_DATA_HOME/devflow` (macOS: `~/Library/Application Support/devflow`), or `DEVFLOW_DATA_DIR` |
| Branch metadata | `devflow` database on the PostgreSQL server |

## The pipeline

`switch -c`, `setup` and `switch` to an existing workspace run the same five phases. The receipt is written as soon as the worktree exists; every later phase is recorded before and after its effect, and a re-run resumes at the first incomplete phase.

1. **materialize**: create the Git branch (or reuse an existing local branch of that name) and the worktree; write the receipt with a fresh instance id.
2. **payload**: clone each `worktree.copy` path from the source worktree. Paths stay inside both worktrees; entries are staged under a temporary name and renamed; existing destinations are left alone.
3. **database**: create or reconcile the data branch from `--data-from` and record the binding.
4. **env**: write `.devflow/env`.
5. **hooks**: if trusted, run `setup` commands; otherwise prompt (interactive) or skip and mark `needs_attention`.

Status is derived from the phases: `ready` when every phase is done or skipped on request (`--no-db`, `--no-hooks`), `needs_attention` (a required phase was skipped or failed) or `in_progress` (another process holds the receipt lock, or a crash left a phase started). `list`, `switch` to a ready workspace and `remove --keep-data` do not contact the PostgreSQL server, so they work while it is unreachable; `db status` reports it unreachable. `remove` checks all protections before any effect, runs `teardown`, drops the data branch if this instance created it, then removes the worktree.

Worktrees created by other tools are adopted through a `post-checkout` hook that runs `devflow setup --if-new --from-git-hook`: it skips a worktree whose receipt is `ready`, resumes any other, never prompts, and always exits 0.

## Role model

One PostgreSQL server per project. Verified on PostgreSQL 18.6 with a non-superuser provisioner.

| Role | Attributes | Holds |
| --- | --- | --- |
| Admin (provisioner) | Superuser (managed) or `CREATEDB CREATEROLE` login (external) | Owns every devflow database and the metadata database. Never given to sandboxes. |
| Project owner `dfo_<project8>` | `NOLOGIN` | Owns every object in every branch and snapshot database of the project; `CREATE, TEMPORARY` on each; no `CONNECT` |
| Branch login `dfl_<id12>` | `LOGIN`, random password, member of the owner, default `role` set to the owner | `CONNECT` on its own database only |

Because the objects of all branches share one owner, clones need no ownership translation, and the owner role is the supported identity for anything that must survive cloning (default privileges, RLS policies, grants). Objects a session creates after `SET ROLE NONE` belong to its login; cloning or snapshotting reassigns them to the owner, and privileges or policies that name the login are rejected rather than translated.

Sessions run as the owner, which deliberately holds no `CONNECT`. Functions that check `CONNECT` on the current database, such as `pg_database_size(current_database())`, are therefore denied inside a branch; run them after `SET ROLE NONE`, or read sizes with `devflow db branch list`.

Every new database is created closed (`ALLOW_CONNECTIONS false`) and opened only after its ACLs are normalized. The `devflow` metadata database is closed the same way, so branch logins cannot reach it; managed mode also revokes `PUBLIC` `CONNECT` on `postgres` and `template1`. On an external server devflow changes only databases it created. Snapshots are not templates; only the admin can clone them.

## Recovery model

- Every branch operation holds a session-level advisory lock on the project id and records each step on the branch row before performing it. If the lock connection dies, the operation stops and leaves the row.
- The next mutation of the project, or `devflow db reconcile`, completes or rolls back rows in `creating` or `deleting` and resets with an `op`, by inspecting `pg_database` and `pg_roles`. A reset in `staging` drops its replacement; one in `cutover` whose rename did not commit reopens the original; one in `cleanup` drops the backup. Any other combination is left alone and reported.
- A crash between creating a branch and saving the receipt is repaired by instance id: the retry finds the branch and sets a new password, since the old one was never delivered. Roles that cannot be dropped are made `NOLOGIN` and recorded in `retired_roles`; `reconcile` retries them. Prepared transactions and replication slots that block `DROP DATABASE … WITH (FORCE)` are reported, not forced.
- Worktree setup is resumable from the receipt. Hooks run at least once: a crash after a command succeeds but before the receipt update reruns it, so hook commands must be idempotent.
- A VM fork copies the receipt. `devflow setup --new-instance` gives it a fresh identity and a new branch. Only the instance that created a branch may reset or drop it; `--database-url` attachments are detach-only.

## Release gates

CoW verification on OrbStack and Linux btrfs/XFS, branch isolation, a privilege matrix with a non-superuser provisioner, crash recovery at every phase, fork and attach, remove protections, and GUI flows on macOS. The list is in the plan. Docker-backed tests are ignored by default; see [CONTRIBUTING.md](../CONTRIBUTING.md).
