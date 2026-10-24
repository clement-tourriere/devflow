# Troubleshooting

Start with `devflow doctor`. It runs a list of checks, each `ok`, `warn` or `fail` with a plain-words detail, and for a problem it can fix, the remedy: the command that does it (`devflow doctor` prints it after the hint; `--json` has it in `remedies[].command`) and, in the desktop app, a button. `doctor` exits 1 when a check fails (warnings do not) and prints its normal document with `"ok": false`. It judges the workspace you run it in, and says which directory. Every failing command prints the same remedies as `hint:` lines, and as data under `--json` ([the contract](../../AGENTS.md#remedies)).

- [A workspace is not ready](#a-workspace-is-not-ready)
- [Data services](#data-services)
- [Processes](#processes)
- [Worktrees made or removed by other tools](#worktrees-made-or-removed-by-other-tools)
- [Projects and configuration](#projects-and-configuration)
- [The doctor checks](#the-doctor-checks)

## A workspace is not ready

`switch` and `setup` report `needs_attention` (exit 2) when a required phase was skipped or failed, and `in_progress` when another process holds the workspace or a crash left a phase started; `wait` and `exec --wait` exit 2 for the same states, and `list` shows the status (it always exits 0). The reason is in the output and in the receipt; `devflow setup` in the worktree resumes at the first unfinished phase.

| Reason | What to do |
| --- | --- |
| Hooks or `[env]` recipes are not approved (`trust` remedy) | Read what an approval covers and approve it: `devflow trust`, then `devflow setup`. Approvals are per scope: a branch with other hooks needs its own, see [what `devflow trust` approves](hooks-env-trust.md#what-devflow-trust-approves) |
| A setup hook would not get the workspace's variables from mise ("setup hook #2 would not get the workspace's variables as devflow sets them …") | Route the command through `devflow exec` (`mise exec -- devflow exec -- CMD`), or fix mise's configuration; if mise does not trust the worktree's configuration, `mise trust PATH` (devflow never trusts for you). `--skip-mise-check` runs the hooks anyway and says so. [Checking what mise hands your hooks](hooks-env-trust.md#checking-what-mise-hands-your-hooks) |
| The workspace's own `devflow.toml` is missing, invalid, a 0.x file or another project's (`use_default_recipes` remedy) | Commit a valid file there, or choose the default workspace's recipes for it (`devflow setup --use-default-recipes`). A worktree made before the main checkout's `devflow.toml` was committed says so. [Which configuration a workspace runs](configuration.md#which-configuration-a-workspace-runs) |
| The configuration changed since the workspace was set up (a service added or changed, its own hooks or `[env]`) | `devflow setup` or the next `devflow switch` completes the reopened phases |
| `in_progress` | Wait (`devflow wait` waits without holding the lock), then run the command again; after a crash `devflow setup` resumes |
| A phase failed after the worktree was created (an unreachable server, say) | The worktree is kept and named in the message: fix the cause (`start_database`, `start_service`) and run `devflow setup` there |

A workspace whose `.devflow/env` was deleted (`git clean -fdx`) is not "not ready": `list` says the file is missing, and `devflow setup` or `devflow switch` writes it again from the receipt, with no hook run again.
## Data services

| Symptom | What to do |
| --- | --- |
| Docker is not reachable (`doctor`: `docker`, `services-docker`) | Start Docker or OrbStack. Managed PostgreSQL and the container services need it; SQLite and an external PostgreSQL server do not |
| `devflow db up` or `service up` names a port that is taken | A managed port is chosen once and kept, and devflow never moves it: free the port, or set `port` under `[services.<key>]` yourself (PostgreSQL) |
| Branches are full copies (`doctor`: `postgres-cow`, `services-cow`, `worktree-cow`) | The storage under the data cannot share blocks. Move the data to btrfs, XFS with reflink, ZFS 2.2+ or an OrbStack volume ([copy-on-write, measured](copy-on-write.md)); `require_cow = true` refuses a copy instead. An external server reports `unknown` until you declare `cow = "verified"` in `devflow.local.toml` |
| A clone is refused because the source has open sessions | `on_busy_source` under `[services.<key>]`: `disconnect`, `fail` or `wait` ([PostgreSQL](postgresql.md#database-branches)) |
| A clone, snapshot or reset was interrupted | `devflow db reconcile` or `devflow service reconcile` completes or rolls it back (the `reconcile_databases` and `reconcile_service` remedies); the next operation on the same data does it as well |
| A MySQL clone is refused | A running source that has tables outside InnoDB, or accounts devflow did not create, is refused with the tables or accounts named ([MySQL](mysql.md)) |
| A SQLite clone is refused or waits | A writer holds the source; the copy waits up to `busy_wait` and then refuses rather than copy a changing file (`retry`) |
| A workspace's branch lost its volume | `devflow service KEY drop ID --lost` (the `drop_lost_branch` remedy), then `devflow setup` provisions a new branch |
| Data nothing uses is left over (`doctor`: `services-orphans`, `postgres-scratch`) | It is kept on purpose. `devflow service list --orphans` and `devflow db reconcile` report it; `service NAME forget`, `drop ID` and `db reconcile --drop-orphans` remove it |

## Processes

`devflow up` and `down` refuse, with a typed reason, rather than guess. `devflow ps` shows each workspace's `can_start` and `can_stop` (with the refusal) and each daemon's `startable` before you try ([Processes](processes.md)).

| Refusal | What to do |
| --- | --- |
| pitchfork is missing, older than 2.20.0 or does not run (`doctor`: `pitchfork`) | `devflow pitchfork status` says which; `devflow pitchfork install` (asks first) or install it yourself |
| The workspace is not `ready`, or its scope is not approved | Make it ready (above), then `devflow trust` |
| A daemon starts on its own (`cron`, `watch`, `boot_start`, `auto`) | Start plain daemons by name (`devflow up --daemon NAME`); `devflow trust` flags the autonomous ones |
| The pitchfork namespace does not belong to this workspace | A pitchfork file names another workspace's namespace: remove the `namespace` from the committed file; `devflow up` restores devflow's own `pitchfork.local.toml` when it is merely stale |
| pitchfork's state cannot be read, or its supervisor is down while devflow started daemons here | `pitchfork supervisor start`, then `devflow down`; `--force` goes on without knowing and says so |
| A daemon does not see `.devflow/env` (`doctor`: `pitchfork-env`; `mise-wiring` is about shells and editors) | `run = "devflow exec -- CMD"` in `pitchfork.toml`, with `mise = true` for the toolchain |

## Worktrees made or removed by other tools

| Symptom | What to do |
| --- | --- |
| A worktree made by `git worktree add` or an agent host is not set up | `devflow hook install` once per repository, or the host's own integration (`devflow integrate HOST`; `doctor`: `git-hook`, `integration-<host>`). A relative `core.hooksPath` that a new worktree lacks (husky's `.husky/_`) never fires there (`git-hook-path`) |
| An agent's script needs the workspace before it is ready | `devflow wait`, `devflow setup --wait`, `devflow exec --wait -- CMD` |
| A host deleted a worktree and its data, containers or daemons are still around (`doctor`: `workspaces-gone`) | `devflow gc` lists them, `devflow gc --remove ID… --yes` removes the ones you name (`--snapshot` keeps a snapshot of each data branch first) |
| `gc` lists a record as `unknown` with `cause: other_checkout` | The record was written for another copy of the repository (a copy, a restored backup): `devflow gc --rebind ID… --yes` binds it to this one, and removes nothing |
| `devflow list` shows a workspace whose directory is gone | `devflow prune` forgets the registration (those that still own data are kept; `devflow remove` or `gc` removes them) |

## Projects and configuration

| Symptom | What to do |
| --- | --- |
| "no devflow.toml in …" | `devflow init` (`--plan` shows the proposal first) |
| An error that links the migration guide | The file is a 0.x one: [MIGRATING.md](../MIGRATING.md). A top-level `[postgres]` table of an earlier 1.0 build is rewritten by `devflow config migrate` |
| A project in `devflow list --all` has a `problem` (`folder_missing`, `not_git_checkout`, `not_initialized`, `pre_release_config`, `legacy_config`, `invalid_config`, `wrong_project`, `unreadable`) | The remedy: `devflow project forget PATH_OR_ID` for an entry that is gone, `devflow project relocate PATH_OR_ID NEW_PATH` for a moved folder, `init` or `config migrate` for the rest |
| An unknown key, a wrong type | The error names the file and the key, never the value; an editor with the [JSON Schema](configuration.md#editor-support-the-json-schema) marks it as you type |
| "'NAME' names more than one workspace: …" | A checkout moved to another branch and another took its old one: name the one meant by its path (`use_path`) |
| `devflow switch` without a name in a script | It needs a terminal to pick; name the workspace (`choose_workspace`) |
| `devflow switch -` or the directory change does nothing | The shell wrapper is not active (`doctor`: `shell`): `eval "$(devflow shell-init zsh)"` |
| "the environment carries variables of workspace …, which cannot be resolved" | A shell started inside a workspace that is gone still carries its variables: open a new shell, or `DEVFLOW_CLEAR_INHERITED=1` (`devflow exec --clear-inherited`) |
| A hook or `devflow exec` has the wrong `DATABASE_URL` | Another database came from mise or the caller's environment: go through `devflow exec`; `doctor`'s `isolation` check names the variables that carry another value |

## The doctor checks

| Check | Looks at |
| --- | --- |
| `git`, `checkout` | libgit2 (built in), and the `git` that will run when `[worktree] checkout = "git"` chose the native checkout |
| `docker`, `services-docker` | The `docker` client, `docker compose` and the daemon (needed for managed PostgreSQL and the container services) |
| `project`, `recipes`, `shared-sections`, `local-config`, `env-presets` | The configuration: which file the workspace takes its hooks, `[env]` and copy list from, shared sections a branch file has that are not applied, a `devflow.local.toml` inside a worktree that is ignored, an `[env] preset = …` that probably means `presets` |
| `worktree-dir`, `worktree-cow` | Where worktrees are created (writable, same filesystem) and whether payloads are cloned copy-on-write |
| `postgres`, `postgres-file-copy`, `postgres-cow`, `postgres-scratch` | The server's reachability and version (18 or newer), `file_copy_method = clone`, the copy-on-write capability (measured or declared) and scratch databases |
| `services`, `services-cow`, `service-<key>`, `services-orphans`, `services-receipts` | The container services: the catalog and the Docker daemon it is bound to, copy-on-write per Docker daemon, each service's branches and unfinished operations, data nothing uses, and workspace receipts that cannot be read (so ownership is unknown and nothing is offered as an orphan) |
| `sqlite-<key>` | A `sqlite` service's file in the main checkout and its snapshots |
| `hook-trust` | Whether the hooks, `[env]` and pitchfork processes are approved |
| `git-hook`, `git-hook-path`, `integration-<host>` | The post-checkout hook, a relative `core.hooksPath` a new worktree lacks, and agent hosts the repository uses that devflow does not set up |
| `pitchfork`, `pitchfork-namespace`, `pitchfork-env`, `pitchfork-proxy` | pitchfork's install state and release, the workspace's namespace, daemons without the mise wiring, and (for daemons with a port) whether the proxy is on and its certificate trusted |
| `mise`, `mise-env`, `mise-wiring` | mise's release, what mise does to the current workspace's variables (and the setup hooks not routed through `devflow exec`), and what shells and editors get through mise |
| `isolation`, `dotenv` | The process environment against `.devflow/env`, `SELECT current_database()` through the workspace URL against the receipt (when the server is reachable), and copied dotenv files that set an owned name to another value |
| `workspaces`, `workspaces-gone` | Workspaces that need attention, are interrupted or missing, and workspaces removed outside devflow that still own something (files only: no server, no Docker) |
| `shell` | Whether the shell wrapper is active |
