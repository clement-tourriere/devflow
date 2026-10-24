# Hooks, environment and trust

Three things decide what runs in a workspace and with which variables: the **hooks** (shell commands at setup, switch and teardown), the **environment** (`.devflow/env`, with your `[env]` recipes) and the **approval** (`devflow trust`) that gates both. Hooks and `[env]` come from the workspace's own `devflow.toml` ([which configuration a workspace runs](configuration.md#which-configuration-a-workspace-runs)). Commands that need the workspace's database go through `devflow exec`, and a check keeps mise from handing a hook another database.

- [Hooks](#hooks)
- [Environment (`[env]`)](#environment-env)
- [What `devflow trust` approves](#what-devflow-trust-approves)
- [Commands that need the workspace's database go through `devflow exec`](#commands-that-need-the-workspaces-database-go-through-devflow-exec)
- [Checking what mise hands your hooks](#checking-what-mise-hands-your-hooks)

## Hooks

`setup`, `switch` and `teardown` are lists of shell commands, run with `sh -c` in the worktree with `.devflow/env` applied. `setup` runs once in a new worktree, `switch` on every `devflow switch` into an existing ready workspace (and once after `switch -c` reaches ready), `teardown` before removal. Each entry is a string, or a table when it needs a name or its own timeout:

```toml
[hooks]
timeout = "15m"                                   # the default, per command
setup = [
  "bun install",                                  # a plain command
  { name = "migrate", run = "bun run db:migrate", timeout = "5m" },
]
```

A name (1 to 64 characters, unique within its phase) is shown in the output, the setup log and failures: `[migrate] $ bun run db:migrate`. A `timeout` overrides `[hooks] timeout` for that command. Hooks run in order, stop at the first failure, run at least once (write them to be idempotent), and are never templated.

Edit them without opening the file. The commands keep your comments and order, validate the result before writing it, and refuse to overwrite a file that changed meanwhile:

```sh
devflow hooks list                                        # each phase, with positions
devflow hooks add setup 'bunx prisma migrate deploy' --name migrate --timeout 5m --after 1
devflow hooks update setup migrate --timeout 10m
devflow hooks move setup migrate first                    # a position from 1, first or last
devflow hooks remove setup 2                              # a name or a position
devflow hooks add setup 'make seed' --local               # this machine only: devflow.local.toml
```

`hooks` and `env` act on the workspace you are in (`--workspace NAME` names another) and edit the file that workspace takes the section from: its own `devflow.toml` for a worktree, the main checkout's for the main checkout; the output names the file. A list in `devflow.local.toml` (always the main checkout's) replaces the committed one whole, so the first `--local` edit of a phase starts from a copy of the committed commands and says so. Changing a command changes what an approval covers: the command prints that the new configuration needs review and points to `devflow trust`; it never approves anything itself. The desktop app's Setup tab does the same with forms.

## Environment (`[env]`)

`.devflow/env` holds what devflow generates (`DATABASE_URL`, the `PG*` variables, `DEVFLOW_*`, `COMPOSE_PROJECT_NAME=df-<project8>-<instance8>`, a Compose project name unique to the workspace, and `<NAME>_DATABASE_URL` for each of the service's `extra_databases`). `[env]` adds your own variables, written to the same file and passed to hooks:

```toml
[env]
presets = ["prisma", "spring"]
DIRECT_URL_FOR_TOOLS = "${DATABASE_URL}"
DB_HOST_ALIAS = "${PGHOST}"
API_ORIGIN = "http://localhost:3000"
```

- A value may reference devflow-generated variables as `${NAME}`: `DATABASE_URL`, the `PG*` variables, `DEVFLOW_*`, `COMPOSE_PROJECT_NAME` and the `<NAME>_DATABASE_URL` of your `extra_databases`. One pass only: no defaults (`${A:-b}`), no nesting, and never another recipe. Write `$${` for a literal `${`.
- Configuration is refused, naming the key, for an invalid name (`^[A-Za-z_][A-Za-z0-9_]*$`), a variable devflow generates or reserves (`DEVFLOW_*`, `PG*`, `DATABASE_URL`, `COMPOSE_PROJECT_NAME`, `TEST_DATABASE_URL`, `SHADOW_DATABASE_URL` and the `<NAME>_DATABASE_URL` of each of your `extra_databases`), a reference to anything else, and a conflict between recipes and presets.
- Presets render typed fields from the database's connection parameters, percent-encoded where they are URLs: `prisma` sets `DIRECT_URL`; `laravel` sets `DB_CONNECTION`, `DB_HOST`, `DB_PORT`, `DB_DATABASE`, `DB_USERNAME`, `DB_PASSWORD`; `spring` sets `SPRING_DATASOURCE_URL` (JDBC), `_USERNAME`, `_PASSWORD`; `sqlalchemy` sets `SQLALCHEMY_DATABASE_URI` (`postgresql+psycopg://`); `db_parts` sets `DB_HOST`, `DB_PORT`, `DB_NAME`, `DB_USER`, `DB_PASSWORD`. A workspace without a database (`--no-db`) skips the presets and any recipe that needs a database variable, with a warning.
- `preset = "prisma"` (or `presets_`, `Presets`) is not the `presets` list: it sets a variable named `preset`. devflow accepts it, since it is a valid recipe, and warns in `devflow env list` and `devflow doctor`: `did you mean presets = ["prisma"]?`.
- A value built from the database password is secret: it is written only to `.devflow/env` (mode 0600), never printed, and masked whole in hook output and logs.
- Recipes and presets are applied only for an approved scope (next section); until then `.devflow/env` holds what devflow generates and the workspace is `needs_attention`.
- A workspace owns every name its configuration declares, whether it has a value for it or not: its identity (`DEVFLOW_PROJECT`, `DEVFLOW_PROJECT_ROOT`, `DEVFLOW_WORKSPACE`, `DEVFLOW_WORKSPACE_PATH`, `DEVFLOW_SOURCE_PATH`, `DEVFLOW_WORKSPACE_ID`, `COMPOSE_PROJECT_NAME`) and its port (`DEVFLOW_PORT`), the connection variables of each configured service (PostgreSQL: `DATABASE_URL`, the `PG*` variables, every `<NAME>_DATABASE_URL`; Valkey and Redis: their `env` name; MySQL: its URL variable and `MYSQL_*`) and your recipes and presets. It also keeps every name it ever exported: the receipt remembers them (and setup takes them from the file it is about to replace, so a recipe removed before the first setup of this devflow is remembered too, saved before the new file is written), so a recipe you remove stays the workspace's. A `--no-db` workspace that has no service still reopens its environment when a service comes, or exports another variable, since the names the file lists change; its services stay skipped. Hooks and `devflow exec` take each of those out of the inherited environment and then set the workspace's own values, so a `--no-db` workspace of a project with PostgreSQL has no `DATABASE_URL` rather than the one of the workspace you ran `devflow switch -c` from. `.devflow/env` lists the names in two versioned comments (`# devflow-owns v1:` for what the configuration declares, `# devflow-formerly v1:` for the names it exported once), which is how `devflow exec` knows them without reading the project. A `.devflow/env` without them, written by an earlier devflow, is not guessed at: `devflow exec` refuses it and setup writes it again, and hooks run only from a file that has them. When the inherited environment names the workspace it came from (`DEVFLOW_WORKSPACE_PATH` and `DEVFLOW_WORKSPACE_ID`) and that workspace's `.devflow/env` is a plain file (no component of its path, from `/` down, is a symlink) recording the same id, every name in that file, and every former name it lists, is taken out too: a recipe you removed, another project's variables. Matching the path and the id only decides what to clear; it gives the workspace no ownership of anything and approves nothing.
- **A source that cannot be resolved is not "no source".** If the inherited environment carries `DEVFLOW_WORKSPACE_PATH` or `DEVFLOW_WORKSPACE_ID` and the workspace they name cannot be resolved (its directory is gone, it has no `.devflow/env`, the file is a symlink or records another id, or one of the two variables is missing), hooks and `devflow exec` refuse, naming the stale workspace and its directory: its variables may be in the environment, and nothing says which. Open a new shell, or unset them. To go on anyway, set `DEVFLOW_CLEAR_INHERITED=1` (`devflow exec` also takes `--clear-inherited`): every variable the workspace owns and every name devflow can generate (`DATABASE_URL`, the `PG*` family, `REDIS_URL`, `MYSQL_*`, the preset variables, the identity names) is taken out first.
- Execution controls (`DEVFLOW_SETUP_ACTIVE`, `DEVFLOW_PHASE`, `DEVFLOW_TRUST_HOOKS`, `DEVFLOW_NON_INTERACTIVE`, `DEVFLOW_CD_FILE`, `DEVFLOW_BIN`, `DEVFLOW_CONFIG_DIR`, `DEVFLOW_DATA_DIR`, `DEVFLOW_INSTANCE_ID`, `DEVFLOW_INHERIT`, `DEVFLOW_CLEAR_INHERITED` and the other `DEVFLOW_*`) and the variables a process needs to run (`PATH`, `HOME`, …) are never taken out. To keep the caller's value of a name the workspace sets no value for (CI that brings its own `DATABASE_URL` to a `--no-db` workspace), name it: `devflow exec --inherit DATABASE_URL -- CMD` (repeatable), or `DEVFLOW_INHERIT=DATABASE_URL,REDIS_URL` for hooks and `exec` alike. A value the workspace sets always wins, and `setup --database-url` is a value it sets.
- A copied dotenv file (`[worktree] copy` entries named `.env` or `.env.*`) that sets one of these names to another value than `.devflow/env` is named by `devflow setup` and `devflow doctor` (file and variables, never a value): tools that read the file themselves, and not through `devflow exec`, use its value.

`devflow env list` shows the recipes, the presets and what they resolve to in a workspace (credentials masked, including a literal under a name like `APP_PASSWORD`; `--show-values` prints them); `devflow env set NAME '${DATABASE_URL}'`, `devflow env unset NAME` and `devflow env preset add|remove NAME` edit them like `hooks` does, and `--local` writes `devflow.local.toml`, the only place a literal credential is accepted.

## What `devflow trust` approves

One approval covers a versioned scope: every hook command (with its name and timeout), the `[env]` recipes and presets as written (the templates, never generated values), and the pitchfork configuration of the project directory (`pitchfork.toml`, `.config/pitchfork.toml` and their `.local.toml` overlays: every daemon's command, `dir`, `env`, hooks, `cron`, `watch`, `boot_start`, `auto`, `depends` and `ready_*`). `devflow trust` prints all of it and flags the daemons pitchfork can start without devflow (`cron`, `watch`, `boot_start`, `auto`). Any change to any part needs a new approval, and an approval made by an earlier devflow (hooks only) is reported as an older format and does not apply. Approvals are kept per digest, up to 20 per project: each workspace has the scope of its own recipes and pitchfork files, any workspace whose digest is on record is trusted, and approving one never revokes another's (`devflow trust --revoke` withdraws the scope you are in, `--revoke-all` every one of the project). A project with none of these has nothing to approve. `devflow trust` asks at a terminal (`-y`/`--yes` approves exactly what it printed without asking); `devflow --json trust` prints the scope and the state and approves nothing; setting `DEVFLOW_TRUST_HOOKS=1` trusts the whole scope for one run, for CI or for repositories whose configuration you have read. Without an approval, setup skips the hooks and the `[env]` recipes and the workspace is `needs_attention`; a `switch` hook that is not approved is not run (a warning, and the switch still succeeds); a `teardown` hook that is not approved refuses the removal, which `--force` goes on past with a warning. The review masks a credential-named `[env]` variable like `env list` does (`DB_PASS = ***`), in the terminal, in `--json` and in the desktop app; the approval is of the real content.

## Commands that need the workspace's database go through `devflow exec`

mise applies its `[env]` over the environment of every command that goes through it (`mise exec`, `mise run`, a shim). A `_.file = ".env"` listed after `.devflow/env`, a `mise.local.toml`, a plain `DATABASE_URL = …` in `[env]`, a `mise.toml` in a subdirectory, an environment-specific file selected by `MISE_ENV`, a task's own `env`, `DATABASE_URL = false` or the global configuration then hands the command another database than the workspace's: a `postinstall` script or a migration would run against the main checkout's. So the workspace's variables are applied last:

```sh
devflow exec -- bun run db:migrate        # .devflow/env on top of the environment, then the command
mise exec -- devflow exec -- bun install  # mise first, devflow after it: devflow's values win
cd apps/web && mise exec -- devflow exec -- npm test   # also from a subdirectory
```

`devflow exec [--workspace NAME] -- CMD [ARGS…]` finds the workspace containing the current directory (any subdirectory; the nearest directory with a `.git` is the worktree root), reads its `.devflow/env`, sets each variable over the inherited environment and replaces itself with `CMD` (Unix `exec`: signals, standard streams and the exit status pass through; where the program cannot start, the codes are `env(1)`'s, 127 and 126). Like for hooks, every name the workspace owns (see Environment above) is first taken out of the inherited environment, set or not: a `--no-db` workspace gets no `DATABASE_URL`, and an inherited `PGSERVICE` or `PGSSLROOTCERT` cannot redirect a `psql`. `--inherit NAME` (repeatable) and `DEVFLOW_INHERIT` keep the caller's value of a name the workspace sets none for. It never starts `CMD` against an unknown database: outside a workspace, in a checkout without `.devflow/env` (setup has not reached it), or with a symlinked `.devflow` or `.devflow/env`, it exits 1 with a message that holds no value. It prints nothing of its own and never a value. The fast path reads the workspace's `.devflow/env` (and, when the environment names the workspace it came from, that one's; no project scan, no registry, no server), and measured on macOS with a release build it costs about 5 ms over starting the program directly, nearly all of it starting devflow itself.

What `devflow init` proposes is built this way: setup hooks that run repository commands through mise become `mise exec -- devflow exec -- CMD` (the `cd DIR &&` ahead of it is safe, because devflow applies last), and pitchfork daemons get `run = "devflow exec -- CMD"` with `mise = true` for the toolchain. That holds for every project that has a managed service (PostgreSQL, Valkey, Redis or MySQL, configured or proposed) or `[env]` content, a Redis-only project included; a project with none of them keeps its commands as they are. None of that depends on how mise is configured. The `mise:env-file` item (`[env] _.file = ".devflow/env"`) stays a proposal for the shells and editors you use in the worktree; when it is blocked it only warns that "shells in the worktree won't see the branch's variables through mise; commands devflow runs are unaffected". A mise *task* cannot be wrapped from outside (its own `env` comes after everything), so `mise run migrate` keeps depending on that wiring; call `devflow exec -- COMMAND` inside the task instead.

## Checking what mise hands your hooks

For hooks you wrote by hand, devflow checks at run time, before the first `setup` hook, and reads the commands, not the files that happen to exist:

- A hook that runs `devflow exec` inside mise (`mise exec -- devflow exec -- …`) needs no check of the variables.
- For any other hook that runs mise (`mise` is a command word of its string, also inside `sh -c '…'`; a command hidden behind `xargs` or a variable is not seen, and neither is a shim), devflow runs `mise exec -- sh -c …` in the worktree with exactly the environment the hook will get (the inherited one without Git's context and without the names the workspace owns, the workspace's variables injected, `MISE_ENV` among them if you set it in `[env]`), bounded to 20 seconds, and compares what the process mise starts sees with the workspace's environment, for every name the workspace owns: a variable it sets with a different value is *overridden*, a missing one *removed*, and a name it owns and sets no value for (a `--no-db` workspace's `DATABASE_URL`) that mise brings back is *reintroduced*. Names you keep with `--inherit` or `DEVFLOW_INHERIT` are not expected either way. Only variable names are shown, never values. Tools are not installed and project dependencies are not prepared by the check.
- Whether mise trusts the worktree's configuration is asked for every hook that runs mise, with `mise env`, which refuses and trusts nothing. (`mise exec` and `mise run` trust a configuration they have not been told to trust when they run without a terminal, as of mise 2026.10; devflow does not let them do that for a worktree it set up.) devflow never trusts a configuration for you.
- A hook the check cannot stand for is named in a warning, not blocked: a `cd` ahead of mise (mise reads that directory's configuration), a mise task (`mise run`, `mise r`, `mise tasks run`, `mise watch`: the task's own `env` comes after), `mise -C`/`-E`, or a `MISE_*` assigned in the command. The warning names the hook (its name, or `setup hook #N`), never its command, and recommends `devflow exec`.

| The check finds | What devflow does |
| --- | --- |
| Same values | The hooks run. |
| A variable overridden or removed | No hook runs; the workspace is `needs_attention` (exit 2): "setup hook #2 would not get the workspace's variables as devflow sets them: mise resolves DATABASE_URL from another source than .devflow/env; …", suggesting `devflow exec`. |
| The configuration is not trusted | The same, telling you to run `mise trust PATH`. `devflow doctor` lists the options (below). |
| mise is not installed | The hooks run, unless one of them runs `mise`. |
| The check fails or times out | No hook runs; fix the cause (`mise exec -- true` in the worktree shows it). |

`devflow doctor` also has an `isolation` check for the workspace it runs in: it compares the process environment with `.devflow/env` for every name the workspace owns and names the variables that carry another value (and the workspace they came from, when the environment says so), and, with a reachable server, asks the workspace's URL `SELECT current_database()` and compares the answer with the database its receipt binds. A `dotenv` check names the copied dotenv files that disagree.

Only the `setup` hooks are checked, once, when the `hooks` phase runs; `switch` and `teardown` hooks are not. `devflow setup --skip-mise-check` (or `switch --skip-mise-check`) runs the hooks anyway. The receipt records when (`mise_check_skipped_at`), `devflow list` shows `ready (mise check skipped)` and the setup summary says so.

`devflow doctor` runs the same check for the workspace you are in (`mise-env`) and lists the setup hooks that run mise without `devflow exec`; it finds the workspace's mise configuration the way `init` does (every location mise reads, `conf.d` and environment-specific files included). `mise-wiring` says what shells and editors get through mise. For a configuration mise does not trust, doctor prints your choices and runs neither: `mise trust PATH` for that one file, or `mise settings add trusted_config_paths DIR` for the worktree directory, which trusts every mise configuration file that will ever appear under it, so choose it only when you trust everything that will be checked out there. `devflow init` does not propose `mise trust`: a hook cannot trust the worktree it is stopped in front of.
