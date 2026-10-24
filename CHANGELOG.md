# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

devflow 1.0 is a breaking simplification. It keeps Git worktrees, copy-on-write payloads, PostgreSQL branches and trusted setup hooks, adds Valkey, Redis, MySQL and SQLite branches, the loop around agent hosts and a GPUI desktop app, and delegates processes and the proxy to pitchfork and toolchains and dependencies to mise. There is no in-place upgrade from 0.x: read [docs/MIGRATING.md](docs/MIGRATING.md). The documentation is the [guide](docs/guide/README.md), [AGENTS.md](AGENTS.md) with its [reference](docs/agents-reference.md) and [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md); the design is in [docs/plans/2026-10-10-devflow-1.0.md](docs/plans/2026-10-10-devflow-1.0.md).

### Workspaces

- **Worktrees, receipts and resumable setup** — `switch [-c]`, `setup`, `list`, `path`, `remove` and `prune` create, enter, inspect and remove Git worktrees. Each worktree records its phases (materialize, payload, services, env, hooks), when each attempt began and one binding per service in a receipt, so `switch` and `setup` resume at the first incomplete phase. Status is `ready`, `needs_attention` or `in_progress`, and exit code 2 means the workspace is not `ready`. `--from` (code) and `--data-from` (data) are independent and immutable once chosen; `--no-db` and `--no-hooks` skips are recorded and kept. Entering a workspace (`switch`, `switch -c`, the app's New workspace and Set up) is one activation in core: setup or resume, repair, refresh of a moved database endpoint, container services, the `switch` hooks once the workspace is `ready`, then autostart; a ready workspace whose service failed to start is still `ready`. A workspace is its receipt, not the branch its checkout holds now: after `git switch other` it keeps its name and data, `list` shows the drift, and a name that means several checkouts is refused with their paths (`use_path`), never resolved by precedence.
- **Remote branches and `--base`** — `switch -c NAME` with only `origin/NAME` creates `NAME` at that commit with its upstream set, as `git worktree add` does (several remotes: `checkout.defaultRemote`, else a typed ambiguity error; nothing is fetched). `--base REF` chooses where a new branch starts (a branch, tag, commit or remote branch; `origin/x` for `x` tracks it) and the outcome reports `branch_start`. Ref conflicts (`feature` beside `feature/x`), invalid names, names over 100 characters and directory collisions are plain errors that name the other branch or workspace. A repository that uses Git LFS or is a partial clone is refused before anything is created (libgit2 cannot check it out faithfully); a sparse checkout is a warning.
- **Native checkout** — `[worktree] checkout = "git"` in `devflow.local.toml` (a committed `devflow.toml` that sets it, a tracked `devflow.local.toml` and a symbolic link are configuration errors) has the `git` on `PATH` check worktrees out, so LFS and partial-clone repositories work. devflow still resolves the starting point and creates the branch and its upstream; Git runs `worktree add` with structured arguments, no `GIT_*` from the caller, `GIT_TERMINAL_PROMPT=0` and `DEVFLOW_SETUP_ACTIVE=1`, under a deadline. Choosing it authorizes Git's own filters, hooks and helpers, which `devflow trust` does not cover; the result's `checkout`, the phase detail and `doctor` say which backend ran. A missing Git is a typed problem before anything is created, and a failure never falls back to libgit2: it names what Git printed and what survives, and the creation resumes by identity.
- **Faster, honest payloads** — A cloned directory tree gets only its directories' times back (APFS `clonefile` keeps those of files and links), in a parallel walk: a 20,000-file `node_modules` takes about as long as the clone itself instead of eight times that. Entries are installed with an atomic no-replace rename, and the payload phase says `cloned` for entries that share blocks and `copied` only for copies. `.worktreeinclude` (gitignore syntax, also read by Claude Code, Codex, Conductor and worktrunk) adds the ignored files it lists to `[worktree] copy`.
- **A port per workspace** — Every workspace gets one port of its own, `DEVFLOW_PORT`, chosen once from `[worktree] port_range` (default `[40000, 49999]`) under a machine-wide lock, checked against a ledger of the ports other workspaces hold and against what listens, journaled in the receipt and kept for the workspace's life (never moved, even when something listens on it later). It is a generated, owned name: `[env]` recipes may use `${DEVFLOW_PORT}` and cannot set it. `CONDUCTOR_PORT` is used where Conductor made the workspace and `CONDUCTOR_WORKSPACE_PATH` names it. Reservations belong to the workspace identity, not its directory (a moved checkout keeps its port; a new identity gets a new one). `ps` and `list --wide` show it; a workspace set up before gets its port on its next entry as a repair of the env phase only, with no hook run again.
- **Finding, entering and leaving workspaces** — `devflow switch` with no name opens a picker on a terminal (most recently entered first, with branch, status and drift) and is a usage error with a `choose_workspace` remedy elsewhere. `switch -` enters the previous workspace of the shell (the `shell-init` wrapper marks its calls with `DEVFLOW_WRAPPED=1` and exports `DEVFLOW_PREVIOUS`; without it the error carries `shell_integration`, and `doctor` warns), and a switch keeps the relative subdirectory when the target has it. `devflow completions SHELL` registers dynamic completion of workspace names, read from local Git and receipts. `devflow open [NAME] [--editor|--terminal]` starts the program set in `<config dir>/settings.toml`, which the app reads and writes too. `list --wide` (and a `wide` object in `--json`) adds uncommitted changes, ahead/behind, advisory merge evidence, last activity, port, containers and daemons while plain `list` stays offline and fast; `up --services` and `down --services` start and stop a workspace's own containers (volumes kept, the shared PostgreSQL server never touched) and `down --all` covers every workspace.
- **Resting workspaces** — `devflow suspend [NAME | --idle DURATION] [--yes]` stops a workspace's own containers (volumes kept) and pitchfork daemons (the gated stop `down` uses, never forced; an unreadable state is unknown, not stopped) and records `suspended_at`; `switch` or `resume` enters it again and starts what was stopped. `pin`/`unpin` keep a workspace out of `--idle`, which also skips the main checkout, the workspace you are in and anything whose receipt cannot be read. Last use is the later of the last activation and the last `devflow exec` (one `stat` and at most one empty write every 30 s of `.devflow/active`). Nothing is ever suspended automatically; `list --wide` and the app show suspended and pinned workspaces.
- **Focus** — `devflow focus NAME [--port PORT[=DAEMON]]…` runs one small forwarder per project that listens on canonical loopback ports (default `[worktree] focus_ports`, say 3000) and forwards TCP to the workspace's `DEVFLOW_PORT` or a daemon's port, for OAuth callbacks and cookies bound to `localhost:3000`. It binds `127.0.0.1` only, forwards only to the ports it was told, keeps its sockets when re-pointed and closes the connections to the previous workspace; `switch` never moves it. It ends itself when its target is no longer the one the ports were taken from, and removal, `gc --remove` and `setup --new-instance` end it first (`released_focus`) and refuse to go on when it cannot be seen to stop. Publishing waits behind the workspace's process lock, so it cannot interleave with a removal or a new identity; a forward to a daemon's port is bound to the process that listened and ends when pitchfork says it changed, and a state devflow cannot read or recognise is unknown, never gone.
- **Removal accounts for everything** — `devflow remove` checks every protection (dirty or locked worktree, untrusted or mise-refused teardown hooks, an unmerged branch with `--delete-branch`, data it cannot account for) before any effect, runs `teardown`, drops only the branches this instance created (`--keep-data` keeps them, recorded before the first effect so a run again does not drop them), and deletes the Git branch only with `--delete-branch`. It reads the branch recorded before its creation finished and a database an earlier build bound too, and a branch it cannot inspect stops it (forceably for a bound branch, for good for a pending one, with remedies). A failure after the first effect is a partial removal that says what was done and what remains, with the same `remove` run converging. The `--force` hint repeats `--delete-branch` and `--keep-data`. A workspace whose checkout is gone is planned from its receipt: pitchfork is asked what runs under its namespace and the daemons are never stopped (a stop would run their lifecycle hooks from a configuration nobody approved), so running or unreadable daemons refuse the removal (forceable, listed in `may_still_run`). `devflow prune` forgets Git registrations whose directory is gone, reviewed and confirmed, keeping any that still own data.

### Configuration, recipes and trust

- **Configuration is TOML only** — `devflow.toml` (or `.devflow.toml`) plus a git-ignored `devflow.local.toml`; 0.x files stop with an error that links the migration guide. Services are `[services.<key>]` tables selected by `engine` (`postgres`, `valkey`, `redis`, `mysql`, `sqlite`; at most one `postgres` and one `mysql`); keys match `^[a-z][a-z0-9_-]{0,30}$`, unknown engines and fields are errors, and files mixing 1.0 and 0.x keys are refused with the conflicting keys named. An earlier build's top-level `[postgres]` table is `[services.db]` now: the old spelling fails with the new one, and `devflow config migrate` (or the app's button) rewrites it in place, keeping comments. Receipts are version 2 (`services.<key>` replaces `data`); a version 1 receipt is migrated in place with no provisioning effect.
- **JSON Schema for `devflow.toml`** — `devflow config schema` prints the schema of the project file (draft-07, for taplo / Even Better TOML and other TOML-aware editors) and `--local` the one of `devflow.local.toml`; both are committed under `docs/`, generated from the configuration types, and a test fails when a committed file is out of date and says how to regenerate it. The schema marks unknown keys, the engine-tagged service tables, bad hook entries, service keys, variable and database names and ports; the project file forbids the machine-local `[worktree] checkout`. Examples, fixtures and templates are validated against it.
- **Each workspace runs its own branch's recipes** — `[hooks]`, `[env]` and `[worktree] copy` are read from the worktree's own `devflow.toml` (with the main checkout's `devflow.local.toml` over it); the project's services, data, integrations, `[pitchfork] autostart` and the other `[worktree]` keys stay the main checkout's, and a branch file never overrides them (differences are warnings naming file and section in setup, switch and `doctor`). An operation resolves one configuration and keeps it for its whole run. A worktree whose file is missing, invalid, 0.x or another project's is `needs_attention` with the `use_default_recipes` remedy: nothing of the main checkout's recipes runs unless a person chooses it, and the choice is recorded in the receipt and cleared when the file is valid again. `devflow hooks` and `env` take `--workspace`; `trust`, `doctor`, `hooks list`, `env list`, setup and `list` show which file each part comes from. A workspace set up before keeps the default workspace's recipes (recorded as the choice and reported once), and a worktree made before the main checkout's `devflow.toml` is committed says exactly that.
- **Configuration generations** — Each service section and the environment have a stable SHA-256 generation (passwords in `admin_url` are masked). Receipts record the generation each binding and `.devflow/env` were applied under: adding or changing a service reopens `services`, `env` and `hooks` (`needs_attention` until `devflow setup` or the next `switch`), keeping `--no-db` and `--no-hooks` skips and never deleting a removed service's data; a change to a branch's own setup hooks reopens that workspace's `hooks` phase alone. A ready workspace whose bound container branch the catalog no longer has is reopened too, so setup provisions a new branch.
- **Trusted hooks and trust scope v2** — `[hooks]` `setup`, `switch` and `teardown` are shell commands, each a string or a `{ name, run, timeout }` table, gated by `devflow trust` or `DEVFLOW_TRUST_HOOKS=1`. One approval covers a versioned scope: the hooks (names and timeouts), the `[env]` recipes and presets as templates (never generated values or passwords) and the project's pitchfork configuration (`pitchfork.toml`, `.config/pitchfork.toml` and their `.local.toml` overlays, every daemon field, hashed whole up to 256 MiB, every path component checked for links). `devflow trust` and the app's dialog show all of it, flag the daemons pitchfork starts without devflow (`cron`, `watch`, `boot_start`, `auto`) and mask credential-named variables; an approval made when only hooks were covered authorizes nothing (`format_changed`). `trust.toml` keeps up to 20 approved digests per project, each with its scope version and date, so worktrees whose hooks or pitchfork files differ no longer revoke each other's approval; `trust --revoke` withdraws the current scope's digest and `--revoke-all` every one. A one-time approval (the terminal's "Run once", the app's "Trust and run") is bound to the digest that was shown and compared with the scope of the workspace the operation acts on, so `switch -c` onto an existing branch that holds other hooks runs none of them on the review of another workspace.
- **`[env]` recipes and presets** — Extra variables for `.devflow/env` and hooks: recipes with `${NAME}` references to devflow-generated variables (one pass, no defaults, no nesting) and `presets = ["prisma", "laravel", "spring", "sqlalchemy", "db_parts"]` rendering typed fields from the database's connection parameters. Invalid, reserved or generated names, bad references and conflicts are configuration errors naming the key. Values built from the password are secret (0600 file, never shown, masked whole in hook output and logs) and applied only for an approved scope. `[env] preset = "prisma"` (a key one edit away from `presets`) is valid but warned about, in `env list` and `doctor`. Every workspace exports `COMPOSE_PROJECT_NAME=df-<project8>-<instance8>`.
- **Editing hooks and `[env]` from the command line** — `devflow hooks list|add|update|remove|move` and `devflow env list|set|unset|preset add|remove`, each with `--json`, edit `devflow.toml` (or `devflow.local.toml` with `--local`) through typed, format-preserving edits: comments and order survive, the merged result is validated before the write, and a file that changed since it was read is never overwritten (`--if-unchanged SHA256` for scripts and agents, hashes in `list --json`; a save moved aside is kept as `NAME.devflow-conflict-<timestamp>`). A list in the local file replaces the committed one, so the first local edit copies it and says so; a credential is refused in the committed file and accepted with `--local`; errors name the key path and never the value. A change that alters what an approval covers prints that the configuration needs review and points to `devflow trust`; it never approves. `env list` previews the recipes with credentials masked by name and by pattern, `--show-values` prints them on request, and an edit's response lists recipe keys only. `devflow.local.toml` is listed in `info/exclude` when an edit creates it.
- **The workspace's variables are its own** — `.devflow/env` holds the generated `DATABASE_URL`, `PG*`, `DEVFLOW_*` and `COMPOSE_PROJECT_NAME` variables plus the project's `[env]`, and mise can load it with `[env] _.file`. Hooks (every phase), `devflow exec` and the mise checks build a child environment with one function (`env_owner`): every name the workspace's configuration declares, bound or not, every name the instance ever exported, and every name in the `.devflow/env` of the workspace the caller came from are taken out of the inherited environment before the workspace's values are set. A `--no-db` workspace no longer gets the caller's `DATABASE_URL` or `REDIS_URL`; a removed recipe or another project's variables do not leak. Execution controls are never touched; `--inherit NAME` and `DEVFLOW_INHERIT` keep a caller's value of a name the workspace sets none for. `.devflow/env` records the names in versioned comments (`# devflow-owns v1:`, `# devflow-formerly v1:`), so `exec` still reads only that file; a file without them is refused with `setup` as the remedy. An environment that names a workspace that cannot be resolved is refused (`clear_inherited`; `DEVFLOW_CLEAR_INHERITED=1` goes on). `setup` and `doctor` name copied dotenv files that set an owned name to another value, and `doctor` has an `isolation` check (the process environment against `.devflow/env`, and `SELECT current_database()` against the receipt's database). A lost `.devflow/env` (`git clean -fdx`) is rebuilt from the receipt by `setup`, activation and `setup --if-new`, with no hook run again.
- **`devflow exec`** — `devflow exec [--workspace NAME] -- CMD [ARGS…]` runs a command with the workspace's `.devflow/env` set on top of the inherited environment, so what mise, a profile or the shell set cannot win, and replaces itself with the command (signals, standard streams and the exit status pass through; 127 or 126 like `env(1)`). It finds the workspace from any subdirectory or by name, never starts the command without the file (outside a workspace, without `.devflow/env`, or with a symlinked `.devflow` it exits 1) and prints no value; measured on macOS with a release build it costs about 5 ms over starting the program directly. Hooks written as `mise exec -- devflow exec -- CMD` and daemons as `run = "devflow exec -- CMD"` get the workspace's database whatever mise's configuration says. This is a new command, not the 0.x `devflow exec`.
- **mise checks for hooks** — mise applies its `[env]` over the variables devflow gives a command that goes through it. Before the first setup hook, `switch` hook and `teardown` hook, devflow reads the hooks' commands: a hook that runs `devflow exec` inside mise needs no check; for any other hook that runs mise it runs `mise exec -- sh -c …` in the worktree with exactly the environment the hook gets (bounded; no tool installed) and compares each variable with what the process sees (overridden, removed, or an owned name with no value reintroduced). Either, a failed check, an untrusted mise configuration (asked with `mise env`, which trusts nothing) or mise missing leaves the setup phase a required skip (`needs_attention`) with no hook run, names the hook by name or index and never its command or values. A `switch` hook that fails the check is not run (a warning and an `open_file` remedy; the switch succeeds) and a `teardown` hook that does refuses the removal (forceable). A hook the check cannot stand for (a `cd` ahead of mise, `mise run`, `mise -C`/`-E`, a `MISE_*` in the command) gets a warning recommending `devflow exec`. `--skip-mise-check` on `setup` and `switch` runs the hooks anyway, recorded in the receipt (`mise_check_skipped_at`) and shown by `devflow list`. `doctor` runs the check for the current workspace (`mise-env`), reports what shells get (`mise-wiring`) and prints the choices for an untrusted configuration as text.
- **Errors never quote values** — Errors about `devflow.toml`, `devflow.local.toml`, receipts, `admin.json`, init plans and connection strings name the file, the key path (`hooks.setup[2].timeout`) or the line and column, and the rule broken; they never repeat a recipe, a hook command or name, a timeout, a database name or any piece of one. JSON masking keeps the structure of user-chosen keys (a service called `tokens` stays an object), and judges variable-shaped keys (`DB_PASS`) with the variable classifier.

### Data services

- **PostgreSQL branches** — One PostgreSQL 18 server per project (a managed container or an external server). Each workspace database is cloned with `CREATE DATABASE … TEMPLATE … STRATEGY FILE_COPY` and `file_copy_method = clone`, so on btrfs, XFS (reflink), ZFS 2.2+ and OrbStack volumes it shares blocks with its source. Copy-on-write is measured and reported as `verified`, `copy` or `unknown`; `require_cow` refuses branches otherwise. When it is `copy` or `unknown` and the source is smaller than `wal_log_below` (default `"256MiB"`), the clone uses `STRATEGY WAL_LOG`, which forces no checkpoints (0.47 s against 6.7 s for a small database on ext4); each branch records the strategy it was cloned with (`clone_strategy`). Branch logins connect to their own database only, behind a per-project owner role and a metadata database (schema version 4; an older devflow refuses it, and a non-superuser `CREATEDB CREATEROLE` provisioner is enough); the owner holds `CONNECT` on each branch's databases so `pg_database_size(current_database())` works in GUI tools. A managed server's host port is allocated once (20000–39999), recorded in `admin.json` and published fixed; a recorded port that becomes unavailable is an error naming the port and the cause, never moved. On OrbStack the server and each container service also get a `….devflow.orb.local` name, reported as `container_endpoint`.
- **Snapshots, branches and short ids** — Versioned, immutable `db snapshot` seeds selectable with `--data-from snapshot:NAME[@vN]`; `db reset`, `db reconcile` and explicit `db branch` management. Every branch id argument (`service NAME drop ID`, `db branch delete ID`, `--data-from branch:ID`, `--to branch:ID`) accepts what the listings show: a unique prefix of at least 8 hex characters, with or without dashes; an unknown or ambiguous prefix is an error naming the candidates.
- **Busy sources you can wait for** — `on_busy_source` (`disconnect`, `fail` or `wait` with `busy_wait`, default `"30s"`) chooses what a clone does when the source database has sessions: end them, refuse, or wait for them to end and then fail naming them. Sessions are named by application, role, process id, client address and state, never by query. A managed server bound to loopback defaults to `disconnect`; external and non-loopback servers to `fail`.
- **Test and shadow databases** — `extra_databases = ["test", "shadow"]` (names `^[a-z][a-z0-9_]{0,20}$`, unique, not `main`, `postgres`, `devflow` or `template*`) gives every branch recorded, empty **scratch databases**, exported as `<NAME>_DATABASE_URL` with the branch's own login and password. Branch logins keep `NOCREATEDB`; the databases are created by the same closed-creation protocol as a branch's own, with generated names (`df_<project8>_<branch12>_<n>`). They are empty in every clone, absent from snapshots, emptied again by `db reset` (finished by `db reconcile` after an interruption), covered by credential rotation and dropped with their branch. A name added to the configuration is provisioned for existing workspaces by the next `setup` or `switch`; a name removed leaves a recorded orphan that `db reconcile --drop-orphans` drops.
- **Seed data for automation** — `[data] noninteractive_from = "snapshot:NAME"` (optionally `@vN`), `noninteractive_max_age` and `noninteractive_fallback = "empty"`: a creation that cannot prompt and has no `--data-from` takes each service's data from its snapshot of that name. A service without it fails with a typed problem (remedies: create the snapshot, or pass `--data-from`), because its default source is a live workspace, which would end PostgreSQL sessions or stop MySQL; only the fallback starts it empty, with a warning. The default workspace is never seeded and the section is part of no generation.
- **Valkey and Redis branches** — `[services.<key>] engine = "valkey" | "redis"` (`image`; `env`, default `REDIS_URL`, and two services exporting one variable are a configuration error naming both; `require_cow`) gives every workspace a managed container of its own on a fixed `127.0.0.1` port allocated once from `hash(project, service, branch)` (journaled before Docker publishes it, kept), with its own `--requirepass`, exported as `redis://:<password>@127.0.0.1:<port>/0` (a secret, masked). A clone has the source write a complete `dump.rdb` (`BGSAVE`, completion proven by `INFO persistence`; the source is never paused) and reflinks it into a new volume inside the Docker VM with GNU `cp -a --reflink=always`; a source with nothing new since its last save is cloned from that save, so repeated clones no longer cost a dataset of new blocks each. Without reflink, `require_cow` aborts and otherwise a full copy is made and reported as `copy` (journaled before the source is touched). The clone's own new password is the whole identity fix-up, checked with `AUTH` and `PING` before it counts as ready; the image digest is pinned per branch and snapshot.
- **MySQL branches (preview)** — `[services.<key>] engine = "mysql"` (`image`, default `mysql:8.4`, the only profile; `database`; `env`; `require_cow`) gives every workspace a MySQL 8.4 server of its own, cloned copy-on-write. The URL is `DATABASE_URL` (`MYSQL_URL` beside a postgres service) and `MYSQL_HOST`, `MYSQL_PORT`, `MYSQL_USER`, `MYSQL_PASSWORD` and `MYSQL_DATABASE` are exported next to it; one mysql service per project. devflow generates a `my.cnf` (standalone InnoDB, no binary log, `performance_schema` off, a 64 MiB buffer pool, `innodb_fast_shutdown=0`). A clone refuses a running source with tables outside InnoDB, shuts the source down cleanly (verified by exit code 0 and InnoDB's shutdown report; a SIGKILL aborts the clone and the source starts again), reflinks the whole data directory, starts the source again and reports its downtime as a setup warning, then gives the clone its own identity before it listens anywhere (new root and application passwords through a no-network bootstrap, accounts audited and normalized, a source with accounts devflow did not create refused by name). A start that logs InnoDB crash recovery abandons the clone. Passwords reach MySQL on the standard input of `docker exec`, never a command line or a container environment.
- **SQLite databases** — `[services.KEY] engine = "sqlite"` (`path` relative to the worktree; `env`, default `DATABASE_URL`, or `SQLITE_URL` when another service claims it; `path_env`, default `SQLITE_PATH`; `busy_wait`; `require_cow`) gives every workspace its own copy of a database **file**. Nothing runs and Docker is not needed. The default workspace uses the file in the main checkout and never replaces it; another workspace starts as a copy of its source's file (the code source's, `--data-from WS`, `snapshot:NAME[@vN]` or `empty`), exported as the absolute path and as a `sqlite://` URL. The copy is **never torn**: it is made under SQLite's own locks, by cloning the quiesced file with a reflink (the write lock is taken, in WAL mode after folding the log in) or, when that is not provably safe, through SQLite's backup API (a new file, reported `copy`); a writer is waited for up to `busy_wait` and the source refused after that. The creation is journaled in the receipt before the first byte and installed without replacing anything. Every effect on a worktree's file goes through a directory descriptor with no link followed (a branch that ships `db -> ../shop/db` cannot reach the main checkout's database), and reset and drop hold SQLite's exclusive lock until the file is gone. Names are judged as a case-insensitive disk reads them (`app.db-WAL` is `app.db`'s log, `.GIT` is `.git`), a file is removed only after it is moved aside through the directory descriptor and found to be the inode that was locked, snapshots are cloned into the worktree through that descriptor, and a platform without inodes refuses these operations instead of treating all files as one; the narrow windows that remain are written up in ARCHITECTURE.md. The library is `rusqlite` with the bundled SQLite: no `sqlite3` program is needed.
- **`devflow service` and the container catalog** — `devflow service [NAME] status|up|down|url|list|snapshot create|list|delete|reset|reconcile|drop|forget` manages Valkey, Redis and MySQL (and, for a `sqlite` service, the files), each with a JSON document (`url` prints the password, the rest redact it). Snapshots are immutable volumes; `reset` stages a replacement volume from the recorded source, starts and checks it, then swaps it in under the journal, keeping the URL; `list --orphans` and `reconcile` report services removed from the configuration and branches whose workspace is gone, and `forget` removes a service's data explicitly. `<data dir>/services/<project id>.json` (0600, versioned, locked, written atomically, bound to the Docker daemon id) is the authority for branches, snapshots and the operation journal; Docker labels only corroborate, and all of them must agree before anything is removed, written into or started. A clone, snapshot or reset is journaled first and run by an operation-scoped coordinator process (`devflow __clone`) that holds the source's lock; every later operation first reconciles unfinished entries: it kills the recorded helpers, confirms they are gone, aborts the destination (never promoted) and finishes what was asked for. A `docker stop` whose return was never seen is fenced by identity (stopped, removed by its recorded id, recreated on the same volume, name and port), helper creations of unknown outcome are retired by tombstones, `service down` goes through the same journal, and `forget` is exclusive per service.
- **Per-daemon copy-on-write probe** — `devflow doctor` and `devflow service status` report `verified`, `copy` or `unknown` for the Docker engine's volumes, measured by reflinking a 64 MiB file in a scratch volume and comparing the guest's free space, cached per daemon id.
- **Remote data hosts** — External mode, `app_host`, `DOCKER_CONTEXT` for a managed server on a remote engine, `setup --database-url` and `setup --new-instance` for VM forks. See [the guide](docs/guide/remote-data-hosts.md).

### Processes with pitchfork and mise

- **`up`, `down`, `ps` and `pitchfork`** — `devflow up`, `down` and `ps` start, stop and list a workspace's pitchfork daemons (`--json` for all); `devflow pitchfork status|install|proxy` reports and offers installation (`mise use -g pitchfork@latest` after asking; the proxy commands are only printed, and devflow never runs `proxy setup`). Each worktree gets a machine-local `pitchfork.local.toml` holding only `namespace = "df-<project8>-<instance8>"` and `worktree_label`, listed once in `info/exclude`. A start requires a `ready` workspace whose own scope matches the approval and no daemon that starts on its own; it runs `pitchfork start -q NAMESPACE/NAME…` for the workspace's own daemons with a timeout (default 60 s) and reads the real state back. A stop runs only while the scope still matches the approval. `[pitchfork] autostart = true` (proposed by `init`, off by default) starts the daemons after `setup` and `switch`; the outcome is reported apart from readiness. Daemons receive `.devflow/env` through `run = "devflow exec -- CMD"`, never through pitchfork files.
- **Process safety** — devflow starts and stops only the full `namespace/name` ids of the workspace's own daemons and never uses pitchfork's `--local`; a `depends` that points outside the workspace's own files is refused (`external_dependency`). A workspace acts only through the namespace its receipt assigned: a namespace in its files that is not that one refuses `up`, `down` and `remove` (`foreign_namespace`), and only the generated `namespace` and `worktree_label` are left out of the trust digest. Readiness, namespace and approval are read again immediately before every start, stop and removal's stop. A pitchfork state that cannot be read, a daemon in a state devflow does not know, a malformed entry and a supervisor that is down while devflow started daemons here are never taken for "nothing runs" (`state_unknown`; `--force` goes on without knowing). One classifier reads pitchfork's status strings everywhere (`running (ready)` is running, `failed: exit 3` failed), and a process observation says so when it could not attribute daemons. A daemon counts as wrapped by `devflow exec` only when its whole `run` is one simple `devflow exec -- CMD` that starts no mise (any occurrence of `mise` as a word after `devflow exec --` counts, for daemons and the hook gate alike). Start, stop, restart, removal and the replacement of an instance (`setup --new-instance`) hold a per-workspace process lock (`devflow/process.lock`, taken before the receipt lock) from the record of intent to the read-back, so a stop cannot erase the record of a start still being made; `--new-instance` is refused while this machine's pitchfork lists a daemon of the old namespace as active (`daemons_running`, `state_unknown`), and `DEVFLOW_INSTANCE_ID` declares a copied receipt so its record of started daemons does not block. The remaining instant between the last look and pitchfork reading its own files is documented in SECURITY.md.
- **Supported tool versions, pinned in one place** — `crates/devflow-core/src/tools.rs` pins the oldest supported release of pitchfork (2.20.0, the release of `supervisor status --json`), mise (2026.4.18, the release of `mise exec --no-deps`) and, for the native checkout only, Git (2.19.0), each derived from the features the code calls. `devflow doctor` checks them and an older release, or one whose version cannot be read, is a typed problem with the page to upgrade it; the pitchfork gates, the hook gate of mise and the native checkout refuse it the same way (`devflow pitchfork status --json` has `unsupported`), and branches for older releases are not kept.

### Agents and hosts

- **Agent hosts** — `devflow integrate t3|claude|conductor|cursor [--remove] [--print]` writes the host's project file additively (`t3.json` script with `runOnWorktreeCreate` and `async: false`; Claude Code `WorktreeCreate`/`WorktreeRemove` hooks through `integrate claude-create`/`claude-remove`, which keep standard output for the one path line; Conductor `setup` and `archive`; Cursor `setup-worktree`), shows the change first, never overwrites a script that is not devflow's (reported with the line to add by hand, exit 1) and writes through directory descriptors with no link followed. `init`, `doctor` and the setup wizard offer the hosts a repository shows signs of. Codex has no documented file format: its detached-HEAD worktrees are adopted under their directory name.
- **Waiting for a setup** — `devflow wait [PATH] [--timeout D]`, `setup --wait` and `exec --wait` wait for a setup that runs elsewhere without holding its lock and end at a timeout (exit 0 ready, 2 not ready or still running, 1 error).
- **Worktrees from other tools** — `devflow hook install` adds a `post-checkout` hook that runs `devflow setup --if-new` for `git worktree add`, T3 Code, Claude Code and fresh clones; a hook another tool wrote is never edited, moved or wrapped (a typed refusal with the `copy_hook_line` remedy). `[integrations] git_hook = "background"` starts the setup detached so `git worktree add` returns at once, and `doctor` explains a relative `core.hooksPath` (husky's `.husky/_`) that never fires in a new worktree.
- **The ledger and `devflow gc`** — A worktree's receipt lives in its Git directory and goes with `git worktree remove`, as with the agent hosts that delete worktrees themselves, which have no teardown hook. Every receipt write now writes a record per workspace instance in the repository's common Git directory first (`devflow/workspaces/<instance id>.json`, 0600, versioned, atomic, no credentials): the instance, the checkout it was written for, the owned resources by identity (a pending branch included, the pitchfork namespace and daemon ids) and a removal journal. `devflow gc` classifies every record and registered worktree (`active`, `moved`, `missing`, `removed elsewhere`, `unknown`; anything not positively identified is unknown and never removable), shows what each owns with sizes and advisory merge evidence, and `gc --remove ID… [--yes] [--snapshot] [--force]` removes the instances named after reading each again (generation, ownership under the backends' locks), asking pitchfork about the recorded daemons and never stopping them. A record is acted on only by the repository directory it was written for (a copy or a restored backup is `unknown` until a person binds it with `gc --rebind`), and a setup holds a per-instance lock so a clean-up cannot settle a branch it is still creating. `list` adds a line when such workspaces still own something, `doctor` reports them (files only: no server, no Docker), and the app has a Clean up page.
- **Setting up a project: `devflow init`** — `init` plans from a read-only scan (manifests, lockfiles, compose, `.env*` variable names, Procfiles, mise and pitchfork files, ignored directories; no project code runs and no `.env` value is read) and proposes items with stable ids, a confidence, evidence, risks, dependencies, conflicts and a structured patch: the PostgreSQL service, `worktree.copy` paths, `mise install` (never `mise trust`), idempotent dependency installs, code generation, migrations, `[env]` presets, pitchfork daemons with their ports and the `.devflow/env` wiring in `mise.toml`. A compose service with a Valkey, Redis or `mysql:8.4*` image becomes `service:<key>` (MySQL, a preview, is offered unticked); the compose hooks start only what devflow does not branch, and branching or compose is decided at selection. `--plan [--json]` writes nothing; `--apply PLAN.json [--accept ID] [--reject ID] [--no-defaults]` applies a saved plan; `--yes` plans and applies; `--name` and `--postgres managed|external|none` override the proposal. Existing files are edited additively with their formatting and comments, a key that exists with another value is a reported conflict, and `--apply` checks the inputs' hashes, journals its intent (`.git/devflow/init-apply.json`), replaces files by atomic exchange where the filesystem has one (a save in the last instant is put back or kept as `NAME.devflow-conflict-<timestamp>`) and resumes an interrupted run. Hooks that run repository commands through mise are built as `mise exec -- devflow exec -- CMD` and daemons as `run = "devflow exec -- CMD"`, so none depends on how mise is configured. Writing configuration never installs, trusts or runs anything. The scan collects only what a proposal reads or the wizard shows.
- **The agent runner** — `examples/agent-task.sh` creates a workspace, runs a command and removes the workspace after a success and after a failure (`KEEP=1` keeps it): it stays the parent, exits with the command's status, forwards INT and TERM to the command's whole process group, and removes the workspace only once that group has exited (TERM, then KILL after `AGENT_TASK_GRACE` seconds, default 10, validated as 0 to 3600; exit 70 and the workspace kept when processes survive KILL).

### Remedies and output

- **Messages and fixes are separate** — Errors, `doctor` checks, setup outcomes and the reasons a workspace needs attention carry no command line. The fix travels as data: a `remedies` array (kind, target, label, CLI `command`) on JSON errors, `doctor` checks and setup outcomes, printed by the CLI as `hint:` lines; the desktop app draws the same remedies as buttons and runs them through the same core functions, so nothing in it shows or runs a CLI command. A remedy always names its target, and `devflow --json list --all` gives each registered project that cannot be opened a `problem` with remedies.
- **Remedies act on what they name** — A refused `up` or `down` names the workspace it resolved. `start_service` names the checkout that owns the branch; a workspace-owned branch whose volume is gone has `drop_lost_branch` (guarded: the record is read again under the branch lock and dropped only if it still names that volume); dropping unused scratch databases reads the configuration again under the database lock. A failed setup phase keeps its cause's remedies before the retry.
- **Output** — One versioned JSON document per command; passwords are redacted except where handing out the credential is the command's purpose (`db url`, `db branch create`, `service [NAME] url`, `env list --show-values`). `devflow --json trust` reports `scope` as `{version, hooks, env, processes, digest}` and the state as `nothing_to_approve`, `trusted` or `untrusted`. Counts are in the singular where they are one (`1 command`).

### Desktop app

- **The app** — `devflow-gui`, built on GPUI and `gpui-kit`: projects, workspaces and their detail (phase timeline with durations, errors and Retry setup, connections and scratch databases, time and disk reported honestly), data services with branches, snapshots and orphans, pitchfork daemons, the Clean up page, a config editor and setup logs. Its actions run against the project, workspace instance and source they were opened for (core refuses with "the workspace changed since you reviewed it" otherwise); names that look like credentials are masked on screen but still act on the right object; what could not be read (Docker, the catalog, pitchfork, a receipt) is shown as unavailable or unknown, never as zero or none, and a process state nobody observed offers a confirmed "Stop anyway".
- **Setting up from the app** — A project with no `devflow.toml` offers **Set up devflow…**, a six-step wizard (Detected, Proposal, Review, Apply, Approve, Done) over core's `detect`, `propose` and `apply`; the new **Setup** tab edits the hooks and `[env]` recipes and presets through core's `config_edit`, keeps the form's file hash so a changed file is a conflict, and follows edits made outside the app.

### Shell, install and tooling

- **Shell integration** — `devflow shell-init` (a `cd`-ing `switch`), `devflow completions SHELL` and `devflow gui`.
- **Installation** — Binaries ship `devflow` only; the desktop app is built from source. The README says the latest release is 0.x with another configuration format and how to build 1.0 from source, and `scripts/install.sh` warns after installing a 0.x release.
- **CI and scripts** — On a release tag (and by hand) a job installs the published artifact with `scripts/install.sh` on Linux amd64, Linux arm64 and macOS and runs `scripts/smoke-test.sh`; the Valkey/Redis and MySQL Docker suites run on Linux next to the PostgreSQL job; `scripts/bench-workspaces.sh` is a manual benchmark of five workspaces end to end; `scripts/kingfisher.sh` and `examples/agent-task.sh` pass shellcheck. `tests/docs.rs` fails when a documentation link, or a link into the repository that the code carries, does not resolve.
- **`devflow doctor`** reports the copy-on-write capability (measured or declared), asks the Docker daemon (`docker info`, bounded to 10 seconds: unreachable is a failure when the project needs Docker and a warning otherwise), and checks pitchfork, mise, the shell wrapper and the agent hosts in use.
- **Native, synchronous core** — `devflow-core` has a blocking API (nothing async is public) and no GPUI types. Examples for `devflow.toml`, external mode, mise, pitchfork and an agent task script are under `examples/`.

### Removed

- **Detection nothing read** — `devflow init`'s repository scan no longer collects what no proposal reads and the wizard does not show: language version pins, monorepo tools, workspace globs, a package's `name`, the version of a pinned package manager, compose project names and file lists, port protocols, the `.env` names of services no proposal acts on, a driver's dependency name, sqlx's offline data, the rendered command of a process. Proposals, their evidence and their conflicts are unchanged; the plan's detection digest is computed over fewer facts, so a plan written by an earlier build is stale.
- Process supervisor, controller daemon, execution records and the operation journal: use pitchfork and `devflow setup` / `db reconcile`.
- The HTTPS proxy crate, local CA and `devflow-proxy` binary: use pitchfork's proxy.
- The terminal UI and the terminal crate; the Tauri + React desktop app.
- Jujutsu support.
- ClickHouse, RustFS and generic Docker services, the shared PostgreSQL engine and ZFS driver, Neon/DBLab/Xata and plugin providers (0.x's Redis and MySQL services are replaced by `valkey`, `redis` and `mysql` services with another model).
- Baselines (replaced by snapshots), seeding from URLs, dumps and S3, retention, pins and expiry, artifact caches.
- Hook phases, actions, recipes and MiniJinja templating, hook approvals, 0.x's `devflow exec`, `switch -x`, the agent guide generator, 0.x's `gc`, `capabilities` and `plugin`. Their 1.0 counterparts (`devflow exec`, `devflow gc`, `devflow config`) are different commands.
- Broad ignored-file and AI-configuration copying; list explicit paths in `worktree.copy`.
- The documentation site, generated `llms.txt` files, integration shell scripts and the `DEVFLOW_INSTALL_PROXY` installer option.

### Known limitations

- **Platforms** — The desktop app is covered by headless tests on macOS 15+; native visual checks are manual. The Linux GUI is untested and unsupported. PostgreSQL 18 or later is required.
- **No TLS** — devflow's own database connections do not use TLS. Remote data hosts need a private network or a tunnel.
- **Copy-on-write depends on the filesystem** — Clones share blocks only on reflink-capable storage. External servers report `unknown` unless the operator declares the capability; sources in other tablespaces report `unknown`.
- **Container services** — Valkey, Redis and MySQL 8.4 (a preview; others are later), managed only; MySQL is InnoDB only (MEMORY tables are not supported and a source that has tables outside InnoDB is refused) and its source is unavailable while it is cloned; one machine per Docker daemon; servers with AOF on or ACL files are refused; containers start on `setup`, `switch` and `devflow service up` and do not restart by themselves; the password is on the container's command line for Valkey and Redis (see SECURITY.md); reflink needs Docker volumes on btrfs or XFS with reflink (OrbStack), otherwise clones are full copies and reported as such.
- **Busy sources** — On a local managed server, cloning from a live workspace terminates the source database's sessions (open transactions are aborted). External and remote servers default to refusing instead.
- **Cloning contract** — Privileges or RLS policies that name a branch login are rejected; use the project owner role. Replication machinery is not cloned. Some cleanup blockers (prepared transactions, replication slots) need operator action.
- **Connections** — Multi-host URLs, `service=`, `sslpassword`, `ssl=`/`requiressl=` aliases and replication connections are refused.
- **Hooks** — Hooks run at least once and must tolerate retries. Trust covers the configured commands, not the scripts they invoke.
- **mise checks** — The check of what a hook receives reads the hook's command: a command hidden behind `xargs` or a variable, or one that reaches mise through a shim, is not seen, and a hook the check cannot stand for is only warned about; `devflow exec` is the way to be sure. The probe is a `mise exec`, so it needs mise 2026.4.18 or later.
- **Replacing files without an atomic exchange** — `init --apply` is airtight against a concurrent save only where the filesystem has an atomic exchange (APFS, Linux filesystems with `RENAME_EXCHANGE`); elsewhere a narrow window between the last check and the rename remains.
- **pitchfork scope** — The trust scope reads the project directory's pitchfork files only. pitchfork's user and system configuration and the files of parent directories are the user's, and running pitchfork directly acts on whatever a worktree's configuration says, outside devflow's approval.
- **Redaction** — Credentials devflow knows are masked exactly in hook output and logs; other credentials are masked by pattern on a best-effort basis. Unencoded spaces in URL passwords, `.pgpass` lines, multi-line values that are not known credentials and non-PostgreSQL credential formats are not covered (see SECURITY.md).
- **Manual cleanup** — A failed worktree creation leaves its paths for you to inspect; an interrupted metadata bootstrap can leave an unmarked `devflow_bootstrap_*` database that devflow reports but never drops. Copied workspaces (VM forks) need `devflow setup --new-instance` or `DEVFLOW_INSTANCE_ID`.
- **Scope** — Git only; processes and the proxy come from pitchfork; no embedded terminal or tray; migration from 0.x is manual.

## v0.8.0 (2026-09-14)

### Added

- **Workspace plans and desktop recovery** — Inspect code/data selections before effects, retain exact selections on retry, and inspect/resume/cancel eligible operations from shared CLI and desktop APIs.
- **Reusable data** — Versioned, sealed PostgreSQL baselines with recorded data origins, exact engine identities, and dependency-aware removal.
- **Recovery and cleanup** — Stable resource identities, durable operation inspection/resume, project relocation, previewable retention, workspace pins/expiry, and bounded fingerprinted caches.
- **Provider contracts** — Versioned capability descriptors and plugin negotiation, with generated desktop choices and provider documentation.

### Changed

- **Native operations** — Git, jj, Docker, filesystem and ZFS operations use direct libraries. Explicit execution, hooks, terminals and executable providers remain supported.
- **Proxy distribution** — Package the optional standalone companion and checksums; `DEVFLOW_INSTALL_PROXY=1` installs it alongside the standard CLI.
- **Validation labels** — Linux real-project environments (including ZFS/XFS/Btrfs), live Neon/DBLab/Xata accounts and interactive desktop sessions are explicitly **untested**. Automated Linux checks remain release gates.
- **Workspace lifecycle** — CLI, TUI, and desktop share readiness and service operation policy; required background hooks finish before dependent commands, and optional hooks expose completion receipts.
- **Local data safety** — Recoverable ZFS replacement, cancellation-safe database quiescence, and cross-process port reservations protect concurrent workspace creation and reset.
- **Desktop and proxy** — Configuration uses a format-preserving raw editor; the optional proxy runs as a companion with one runtime owner.
- **Automation** — Workspace execution preserves literal arguments, partial provider failures produce non-zero exits, and cleanup retains ownership state when retry is needed.
- **Agent workflow** — `devflow agent skill` now generates harness-specific guide/rule files instead of installing embedded bundled skills; switch/context JSON expose `agent_workdir` for agent tool workdirs.
- **Hooks** — Lifecycle hooks run synchronously by default unless `background: true`; non-blocking phases now describe failure policy rather than background execution.
- **Local PostgreSQL storage** — `local.storage` / `--storage` support `auto`, `zfs`, `apfs_clone`, `reflink`, and `copy`, with clearer doctor diagnostics for configured preferences and a matching GUI Add Service selector.
- **Doctor UX** — Project-level doctor checks are shared across CLI, TUI, and GUI; `devflow --json doctor` now includes general checks and a summary for automation.
- **Agent automation** — `devflow --json agent status` now returns workspace summary counts and per-workspace `agent_workdir` values.
- **GUI setup** — Doctor/Setup shows a health summary, and local PostgreSQL doctor checks include data-root and port-range diagnostics.
- **Release readiness** — Added `devflow doctor --strict`, `devflow service doctor --strict`, and `scripts/release-check.sh` for CI/agent health gates and local release-candidate verification.
- **Actionable diagnostics** — Doctor JSON, GUI, and TUI surfaces now include aggregate summaries and suggested next steps for failed checks.
- **First-run onboarding** — `devflow init` now reports structured onboarding next steps for services, hooks, agent guide generation, strict doctor validation, and first workspace creation.

### Fixed

- Upgrade `h2` to 0.4.16 and `rustls` to 0.23.45 to address RUSTSEC-2026-0258 and RUSTSEC-2026-0285 found by release CI.
- Preserve exact baseline generations and engine images through recreation, nested descendants and reset; reject incompatible existing containers.
- Preserve Git staging/merge state during jj inspection and recover stale jj workspaces without losing local edits.
- Keep failed removal and project destruction recoverable, confine payload writes, and validate destination cache inputs before reuse.
- Record failed/concurrent executions truthfully, enforce controller ownership, and preserve existing project hooks during installation.

### Removed

- **Redundant integrations** — Removed embedded Pitchfork supervision, built-in LLM API commit generation, AI policy merging, inert configuration options, and duplicate desktop configuration forms. Migration warnings retain supported legacy state and resource ownership.
- **Workspace sandbox** — Removed devflow's partial workspace sandbox implementation; use an agent harness, container, VM, or dedicated sandbox platform for hard isolation.

## v0.7.1 (2026-08-09)

### Fix

- address confirmed code-review findings on the consolidation range
- apply round-2 review findings across the new consolidation code
- **test**: serialize env-mutating switch tests on the shared lock

### Refactor

- sweep branch→workspace naming debt
- **services**: extract shared LocalEngineBackend for clickhouse/mysql
- **workspace**: single canonical tree flattening in core
- **cli**: dedup the detach/pidfile pattern into core
- **hooks**: move sync-ai-configs into core, add built-in action
- **state**: finish the _by_dir local-state migration
- **hooks**: drop the decoy configurable triggers section
- sweep dead code, unify duplicated logic

## v0.7.0 (2026-07-18)

### BREAKING CHANGE

- the docker-compose recipe is removed (devflow init
imports compose app services as processes.daemons; data services are
devflow services) and local-dev-setup is renamed to workspace-setup —
both names now return migration messages. --json hook recipes emits
RecipeDetectionInfo[] inside a project, and the install_recipe(s) GUI
IPC now takes params/selections.
- `devflow destroy --json` now emits a uniform
`{name, success, workspaces_destroyed, error}` shape for every entry
in `services` (previously `branches_deleted` for some providers) and
includes `processes_stopped`.

### Feat

- **hooks**: rework recipes into detection-driven generators
- **cli**: add config-validate command
- **sandbox**: enable landlock in default Linux features
- **scripts**: add safety-gated PR auto-merge loop

### Fix

- **workspace**: clear fail-closed dead ends left by the identity refactor
- **processes**: probe wildcard addrs for port availability
- **services**: resolve service workspaces by normalized name
- **project**: stop processes and purge state on destroy
- resolve workspace-creation store deadlock
- bound pitchfork readiness in workspace lifecycle
- **gui**: align Tauri package versions
- **cli**: doctor exits non-zero and replace unreachable! catch-alls
- **proxy**: drop blanket CORS and add configurable bind address
- **services**: bind docker ports to loopback and harden secrets
- **workspace**: validate names and roll back branch on worktree failure
- **hooks**: gate http action and confine file actions to workspace
- **deps**: adapt code for chore(deps): bump git2 from 0.20.2 to 0.21.0 (automated)
- **deps**: adapt code for chore(deps): bump git2 from 0.20.2 to 0.21.0 (automated)
- **mise**: drop redundant cargo tool entry

### Refactor

- **workspace**: split raw VCS names from service keys behind a shared inventory
- focus devflow on workspaces
- **config**: split config/mod.rs into loading and tests submodules
- **cli**: finish splitting workspace.rs into 7 submodules
- **cli**: split workspace.rs into context and exec submodules

### Perf

- **workspace**: speed up CoW workspace creation

## v0.6.0 (2026-06-12)

### Feat

- **cli**: add self-update command and curl installer

### Fix

- **scripts**: drop removed docs/index.html from version sync

## v0.5.0 (2026-06-11)

### Feat

- **proxy**: unify domain suffix to .local on all platforms
- **daemon**: controller daemon keeps shared engines running (G5)
- **services**: reconcile primitive + `devflow service up` (G5 partial)
- **services**: shared Redis logical-isolation provider (G3)
- **services**: shared ClickHouse logical-isolation provider (G6)
- **config**: read lightweight devflow.toml (G4)
- **services**: RustFS shared object-storage provider (G2)
- post-review remediation (A–F) + shared-engine provider (G1)

## v0.4.4 (2026-06-02)

### Fix

- include service storage in top-level status

## v0.4.3 (2026-06-02)

## v0.4.2 (2026-06-02)

## v0.4.1 (2026-05-31)

### Fix

- update vulnerable dependencies

## v0.4.0 (2026-05-29)

### Feat

- **proxy**: advertise .local names via mDNS for host access
- **mise**: add install task for building and installing the CLI

## v0.3.1 (2026-04-08)

### Fix

- **workspace**: surface post-create hook failures

## v0.3.0 (2026-04-03)

### Feat

- **proxy**: add Firefox trust via enterprise policies.json on Linux

### Fix

- handle missing configs
- **proxy**: preserve original Host header and use origin-form URI
- **gui**: pin bun and tauri-cli in mise, fix hanging bun install
- gate HashSet import behind cfg(target_os = "macos") for Linux CI
- resolve remaining clippy errors on Linux CI

## v0.2.0 (2026-03-06)

### Feat

- Initial public release of devflow as a universal development environment tool
- All features listed in 0.1.0 shipped as the first tagged release
