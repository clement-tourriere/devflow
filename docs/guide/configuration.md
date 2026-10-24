# Configuration

devflow reads `devflow.toml` (or `.devflow.toml`) in the repository, merged with the git-ignored `devflow.local.toml` beside it. Configuration is TOML only: a 0.x file stops with an error that links [MIGRATING.md](../MIGRATING.md). `devflow init` proposes the file for you, and the JSON Schema checks it in an editor.

- [A minimal `devflow.toml`](#a-minimal-devflowtoml)
- [The sections, and where each is read from](#the-sections-and-where-each-is-read-from)
- [Which configuration a workspace runs](#which-configuration-a-workspace-runs)
- [Machine-local overrides: `devflow.local.toml`](#machine-local-overrides-devflowlocaltoml)
- [Editor support: the JSON Schema](#editor-support-the-json-schema)
- [`devflow init`: a proposed setup](#devflow-init-a-proposed-setup)
- [The native checkout](#the-native-checkout)

## A minimal `devflow.toml`

A minimal `devflow.toml` (see [`examples/`](../../examples) for complete ones):

```toml
[project]
name = "shop"
id = "0b3c9d2e-…"                # written by `devflow init`

[worktree]
dir = "../{project}.worktrees"   # relative to the main checkout; absolute and ~/ paths work too
copy = [".env", "node_modules"]  # cloned from the source worktree; omit to copy nothing (init proposes the ignored paths it finds)
                                 # a .worktreeinclude file at the root adds the ignored files it lists (see Agents and hosts)
# port_range = [40000, 49999]    # where each workspace's DEVFLOW_PORT is chosen from (this is the default)
# focus_ports = [3000]           # the loopback ports `devflow focus` forwards to the workspace in use

[services.db]                    # a service is a table named by its key, selected by its engine
engine = "postgres"              # postgres, valkey, redis, mysql (preview) or sqlite
mode = "managed"                 # or "external"
image = "postgres:18"

[hooks]
setup = [
  "mise install",
  "mise exec -- devflow exec -- bun install",
  { name = "migrate", run = "mise exec -- devflow exec -- bun run db:migrate", timeout = "5m" },
]
teardown = []

[env]
DIRECT_URL = "${DATABASE_URL}"   # a recipe: devflow-generated variables, referenced once
presets = ["spring"]             # or typed fields: prisma, laravel, spring, sqlalchemy, db_parts
                                 # (a preset owns its variables: prisma sets DIRECT_URL, so use the recipe above or that preset, not both)
```

Optional `[integrations] git_hook = "background"` makes the `post-checkout` hook return at once instead of waiting for the setup (see [Agents and hosts](agents-and-hosts.md)); it is part of no approval.

## The sections, and where each is read from

| Section | What it holds | Read from |
| --- | --- | --- |
| `[project]` | `name` (`{project}` in `[worktree] dir`) and the `id` `devflow init` writes | the main checkout |
| `[worktree]` | `dir` (parent of the worktrees; default `../{repo}.worktrees`), `port_range`, `focus_ports`, `checkout` (only in `devflow.local.toml`, see [The native checkout](#the-native-checkout)) | the main checkout |
| `[worktree] copy` | the paths cloned from the source worktree | the workspace's own `devflow.toml` |
| `[services.<key>]` | one table per data service, selected by `engine` (`postgres`, `valkey`, `redis`, `mysql` (preview), `sqlite`; at most one `postgres` and one `mysql`): see [data services](data-services.md) | the main checkout |
| `[data]` | the seed snapshot for runs that cannot prompt ([seed data](data-services.md#seed-data-for-automation-data)) | the main checkout |
| `[hooks]` | `setup`, `switch` and `teardown` commands, and their default `timeout` ([hooks](hooks-env-trust.md#hooks)) | the workspace's own `devflow.toml` |
| `[env]` | recipes and `presets` for `.devflow/env` ([environment](hooks-env-trust.md#environment-env)) | the workspace's own `devflow.toml` |
| `[integrations]` | `git_hook`: `"synchronous"` (the default) or `"background"` | the main checkout |
| `[pitchfork]` | `autostart` (off by default): start a workspace's daemons once it is ready ([processes](processes.md)) | the main checkout |

Unknown keys are errors (in `[env]` every name is a recipe, and the names devflow generates are refused). Adding a service or changing a service's section reopens the `services`, `env` and `hooks` phases of every workspace until `devflow setup` or the next `switch` there completes them, except for settings that only shape how a later clone runs (`busy_wait`, `wal_log_below`); deliberate `--no-db` and `--no-hooks` skips are kept.

## Which configuration a workspace runs

`[hooks]`, `[env]` and `[worktree] copy` are read from the `devflow.toml` of the workspace's own worktree, so a branch can try its own hooks and the worktrees of one project need not agree. The project itself (`[project]`, `[services.*]`, `[data]`, `[integrations]`, `[pitchfork] autostart` and `[worktree]` apart from `copy`: `dir`, `port_range`, `focus_ports`) is read from the main checkout: a branch never reconfigures a shared server. When a branch's file has shared sections that differ, `switch`, `setup` and `devflow doctor` say which (file and section, never values) and apply none of them. `devflow.local.toml` is the main checkout's and overlays every workspace; one inside a worktree is ignored. `devflow trust`, `doctor`, `hooks list` and `env list` name the file each part comes from.

A worktree whose `devflow.toml` is missing, invalid, a 0.x file or another project's has no recipes, and devflow does not borrow the main checkout's: the workspace is `needs_attention`, services are bound and `.devflow/env` holds what devflow generates, and nothing else runs. Commit a valid file there, or choose the default workspace's recipes for it (the app's button, or `devflow setup --use-default-recipes`; `switch` takes the flag too). The choice is recorded in its receipt and lasts only until the worktree has a valid file of its own. Commit `devflow.toml` before creating the first workspace: a worktree is made from a commit, and one made before the file is committed says so (the start point has none and the main checkout's is not committed) and offers the choice, then the file. A workspace set up by an earlier devflow, when every workspace ran the default workspace's recipes, keeps them if its branch has none of its own; devflow records that as the choice and says so once.

## Machine-local overrides: `devflow.local.toml`

Machine-local overrides (an admin URL, a declared `cow`) go in `devflow.local.toml` beside `devflow.toml`. It overlays every section, `[hooks]` and `[env]` included (tables merge, lists are replaced), and approval covers the merged result. It can hold an admin password, so keep it out of Git: `devflow init` does not edit `.gitignore` (this repository's `.gitignore` lists it; add it to yours), and an edit with `--local` (or the app) that creates the file lists it in `<git common dir>/info/exclude` unless Git already ignores it. It is the main checkout's and overlays every workspace, each part with its own fields: its shared fields go to the main checkout's shared sections, its recipe fields to the workspace's recipes; one inside a linked worktree is ignored (`devflow doctor` says so). It is the one file that may set `[worktree] checkout`.

## Editor support: the JSON Schema

`devflow.toml` has a JSON Schema, [`docs/devflow.schema.json`](../devflow.schema.json), generated from the same types the parser reads (`devflow config schema` prints it). An editor that understands TOML schemas completes the keys, shows what each one means and marks an unknown key, a wrong type, a service key or a hook entry the parser would refuse. The machine-local overlay has its own, [`docs/devflow.local.schema.json`](../devflow.local.schema.json) (`devflow config schema --local`): nothing in it is required, and it is the one file that may set `[worktree] checkout`. A test fails when a committed schema is not the generated one, and says how to regenerate it. What a schema cannot state stays with the parser: durations, at most one `postgres` and one `mysql` service, two services exporting one variable, `${NAME}` references in recipes.

With [taplo / Even Better TOML](https://taplo.tamasfe.dev/configuration/directives.html), put a directive on the first line of the file:

```toml
#:schema https://raw.githubusercontent.com/clement-tourriere/devflow/main/docs/devflow.schema.json
[project]
```

or associate the files once for the whole repository, in `.taplo.toml` (the extension reads it) or in its `evenBetterToml.schema.associations` setting:

```toml
[[rule]]
include = ["**/devflow.toml", "**/.devflow.toml"]
schema.path = "https://raw.githubusercontent.com/clement-tourriere/devflow/main/docs/devflow.schema.json"

[[rule]]
include = ["**/devflow.local.toml"]
schema.path = "https://raw.githubusercontent.com/clement-tourriere/devflow/main/docs/devflow.local.schema.json"
```

A checkout of devflow can point `schema.path` at its own `docs/devflow.schema.json` instead. The files in [`examples/`](../../examples) start with the directive.

## `devflow init`: a proposed setup

`devflow init` looks at the repository and proposes the configuration instead of writing a fixed template. The scan is read-only: it parses manifests, lockfiles, the compose file, `.env*` variable *names* (never their values), `Procfile`s, mise and pitchfork files, and which expensive directories Git ignores. It never runs project code.

The proposal is a list of items, each with a stable id, a confidence (`high`, `medium`, `low`), the evidence behind it, the risks, and what it depends on:

| Items | Examples |
| --- | --- |
| `[worktree] copy` | `copy:node_modules`, `copy:.env`, `copy:config/master.key` (never `.venv`, `.next`, `target`, `tmp`, `log`) |
| `[services.db]` | `service:db`: PostgreSQL 18 managed by devflow, from a compose `postgres`, `postgis`, `pgvector` or `timescaledb` image, or from a driver or ORM plus `DATABASE_URL`-style variables; risks when compose runs an older major version or mounts init scripts |
| `[services.<key>]` (Valkey, Redis, MySQL) | `service:<key>` for a compose service whose image is `valkey/valkey`, `redis` (or `docker.io/library/…`, Bitnami's, a tag that says `valkey` or `redis`) or `mysql:8.4*`: `engine = "valkey" \| "redis" \| "mysql"` with the compose image when devflow's profile can run it (Redis 7+, Valkey 7.2+, the official MySQL 8.4 image) and its own default otherwise, with the reason in `risks`. The URL variable is the one your `.env*` files or compose services use (`REDIS_URL`, `VALKEY_URL`, `CACHE_URL`; MySQL: `DATABASE_URL`, or `MYSQL_URL` beside PostgreSQL); a second cache gets its own (`QUEUE_URL`). Valkey and Redis are recommended and applied by default; **MySQL is a preview and is offered unticked** (`--accept service:mysql`). Not proposed: a project that already configures that service (no duplicate), more than one MySQL service, MySQL other than 8.4 (unsupported), images with modules (`redis-stack`, `valkey-bundle`: *not proposed automatically*) and Redis cluster or sentinel (unsupported). Each is said in the plan's `warnings` with the reason and a pointer to the manual configuration ([Valkey and Redis branches](valkey-redis.md), [MySQL branches](mysql.md)) |
| `[hooks]` | `mise install` first (when mise pins tools; never `mise trust`: see [Checking what mise hands your hooks](hooks-env-trust.md#checking-what-mise-hands-your-hooks)), then `hook:setup:install` (the idempotent install for the detected package manager, never `npm ci`), `hook:setup:codegen` (`prisma generate`), `hook:setup:migrate` (Prisma, Django, Rails, Alembic, Laravel, Ecto, sqlx, diesel, goose, dbmate, Atlas; an existing `db:migrate` script or mise task is preferred), and `docker compose up -d --wait …` / `down -v` hooks for the compose services devflow does not branch (MongoDB, MariaDB, MySQL other than 8.4, an image with Redis modules, …; PostgreSQL, Valkey, Redis and MySQL 8.4 are services instead). A compose container devflow branches can stay in the compose file for other uses, but the application must use the URL devflow exports. Branching or compose is decided when you select: a service whose `service:<key>` you leave unticked (MySQL always starts that way) or `--reject` is started by the compose hooks you accept, and a hook with nothing left to start is not written (a project whose only datastore is a cache lists the compose hooks as the alternative to `service:cache`) |
| `[env]` | `env:preset:prisma`, `laravel`, `spring`, `sqlalchemy`, `db_parts` |
| `pitchfork.toml` | `daemon:web`, `daemon:worker`: dev servers from `package.json` scripts, `bin/dev`, `runserver`, as `run = "devflow exec -- CMD"` (with `mise = true` when the repository uses mise), with `port = { expect = [N], bump = true }` and `$PORT` wired where the framework allows |
| `[pitchfork]` | `pitchfork:autostart` (medium confidence, so off unless accepted): start the daemons when a workspace is ready; offered when `pitchfork.toml` exists or daemons are proposed |
| `mise.toml` | `mise:env-file`: `[env] _.file = ".devflow/env"`, so shells and editors in the worktree see the workspace's own database (commands devflow runs do not need it: they go through `devflow exec`) |

```sh
devflow init                                  # show the plan, ask (all high-confidence items at once, then medium and low), write
devflow init --plan                           # show the plan and write nothing
devflow --json init --plan > plan.json        # the proposal as one JSON document: review it, keep it
devflow init --apply plan.json                # apply its high-confidence items
devflow init --apply plan.json --accept hook:setup:compose-up --reject daemon:web
devflow init --yes                            # plan + high-confidence items + write, without asking
devflow init --name shop --postgres external  # --postgres managed|external|none overrides the proposal
```

Existing files are preserved: patches only add, keep the formatting and comments of what is there, and recognize what is already configured. A key that exists with a different value (a `mise.toml` with another `_.file`, an `[env]` recipe that sets `DIRECT_URL` by hand, a hook named `migrate` that runs something else) is reported as a conflict with the reason and never overwritten; an item with a conflict cannot be applied. `--apply` first checks that the files the plan read have not changed since (it names the ones that did), records the hashes it intends to write in an apply journal under the repository's `.git/devflow/`, and writes atomically: a file saved by someone else while `--apply` replaces it is not overwritten (the replacement is exchanged with the file and what was displaced is compared; a save that cannot be put back is kept as `NAME.devflow-conflict-<timestamp>` and named in the error). That guarantee needs an atomic exchange, which APFS and Linux filesystems with `RENAME_EXCHANGE` have; elsewhere a narrow window between the last check and the rename remains and only a hard-link backup of the displaced file is kept. If it is interrupted, running it again with the same plan and the same `--accept`/`--reject` continues; running it after it finished does nothing. **Writing configuration never installs tools, trusts hooks or runs a command**: review the files, commit them, then `devflow trust` shows everything one approval covers.

## The native checkout

devflow embeds libgit2, so the `git` binary is not required. With `[worktree] checkout = "git"` in `devflow.local.toml`, Git itself checks new worktrees out instead (Git 2.19.0 or later): that is how repositories that use Git LFS or are partial clones are supported, since the libgit2 checkout refuses them before creating anything. Choosing it authorizes Git's own filters, hooks and helpers, which `devflow trust` does not cover, so the file must be yours: a `devflow.local.toml` that the repository tracks or that is a symbolic link cannot set it, and a committed `devflow.toml` that sets the key is a configuration error naming it. The result's `checkout`, the phase detail and `devflow doctor` say which backend ran; a failed Git checkout is never retried with libgit2. [Checkouts devflow does not make](../ARCHITECTURE.md#checkouts-devflow-does-not-make) has the details.
