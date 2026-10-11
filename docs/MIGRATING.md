# Migrating from devflow 0.x

devflow 1.0 is a clean break. It reads TOML only, stops with an error that links to this page when it finds 0.x configuration (a `.devflow.yml` or `.devflow.yaml` and no `devflow.toml`, or a TOML file with 0.x keys), and **never adopts or deletes 0.x containers, volumes or data directories**. Its own resources use new names (`devflow-<name>-<id8>` compose projects, label `dev.devflow.project=<id>`, `df_` databases and `dfl_`/`dfo_` roles), so the two generations cannot collide.

The migration has three parts: translate the configuration, move the data you want to keep, and remove the 0.x resources. Do all three with devflow 0.8 still installed for the steps that say so. Keep a copy of the 0.8 binary under another name (the examples use `devflow-0.8`) until you are done.

## What was removed

| 0.x | 1.0 |
| --- | --- |
| Native process supervisor (`devflow process`, `processes:`), controller daemon (`devflow daemon`) | [pitchfork](https://pitchfork.jdx.dev) daemons in `pitchfork.toml` |
| HTTPS proxy (`devflow proxy`, `devflow-proxy`, local CA) | pitchfork's proxy (`<daemon>.<worktree>.<project>.localhost`) |
| Terminal dashboard (`devflow tui`) | `devflow-gui` (`devflow gui`) and the CLI |
| Tauri + React desktop app | `devflow-gui`, a new GPUI app. Its state is not migrated; add your projects again. |
| Jujutsu workspaces | Not supported. Git only. |
| MySQL, ClickHouse, Redis, RustFS and generic Docker services | Run them with `compose.yaml`, pitchfork daemons or mise. devflow no longer branches them. |
| Local per-workspace and shared PostgreSQL engines, ZFS/APFS/reflink data-directory drivers | One PostgreSQL 18 server per project; each workspace is a database cloned with `file_copy_method = clone` |
| Cloud providers (Neon, DBLab, Xata) and plugin providers | An external PostgreSQL 18 server (`mode = "external"`) |
| Baselines (`baseline prepare`, `--data-from baseline:NAME@vN`) | Snapshots: `devflow db snapshot create NAME`, `--data-from snapshot:NAME[@vN]` |
| Seeding (`service seed` from a URL, dump or S3) | `pg_restore` or `psql` into the workspace database, then a snapshot |
| Hook phases, actions, recipes and MiniJinja templates | `setup`, `switch` and `teardown` shell commands that read environment variables |
| Hook approvals (`devflow hook approvals`, `DEVFLOW_APPROVE_HOOKS`) | `devflow trust`, `DEVFLOW_TRUST_HOOKS=1` |
| `devflow exec`, `switch -x`, operation journal (`devflow operation`), retention, pins and expiry, artifact caches | Run commands in the worktree directly. `devflow setup` and `devflow db reconcile` resume interrupted work. `worktree.copy` clones explicit paths; mbx shares Rust build caches. |
| `devflow connection`, `status`, `link`, `worktree-setup` | `devflow db url`, `devflow list`, `devflow setup` |
| `agent skill`, `agent context`, `agent docs`, `capabilities`, `plugin`, `gc`, `config` | `--json` on every command; see [AGENTS.md](../AGENTS.md) |
| YAML configuration, `.env.local` written by hook actions | `devflow.toml`; generated `.devflow/env` |

Commands and flags that changed:

| 0.x | 1.0 |
| --- | --- |
| `switch` with no argument (picker), `--template`, `--dry-run` | Removed. Name the workspace: `devflow switch NAME`; `devflow list` shows them. |
| `switch --from`/`-b`/`--base` | `switch -c NAME --from WS`. The default source is the default workspace, not the current one. |
| `switch --no-services`, `--no-processes`, `--no-verify` | `--no-db`, `--no-hooks`. A skip is recorded: a later `devflow setup` does not run the skipped phase. |
| `remove --keep-services` | `remove --keep-data` |
| `remove --force` | Different meaning. 0.x skipped the confirmation; 1.0 overrides protections (dirty or locked worktree, failing or untrusted teardown hooks, an unmerged branch with `--delete-branch`). A person at a terminal gets a prompt instead of needing the flag. |
| `init [PATH]`, `init --force` | `init [--name NAME] [--postgres managed\|external] [--hook]` in an existing repository. It never overwrites a configuration. |
| `doctor --strict` | `devflow doctor` always exits 1 when a check fails (warnings do not). |
| `commit`, `update` | Removed. Commit with Git directly; upgrade by rerunning the installer. |
| `install-hooks`, `uninstall-hooks`, `git-hook` | `devflow hook install`, `devflow hook uninstall`. The 1.0 hook runs `devflow setup --if-new`; see part 3 for removing the 0.x hooks. |
| `sync-ai-configs` | Removed, with no replacement. |
| `cleanup` | `devflow db branch list` and `db branch delete` for stray database branches; `devflow db reconcile` for interrupted operations. |
| `project` | `devflow list --all` lists the registered projects. |
| `plan` | Removed. There is no dry run; `devflow trust` shows the hook commands without running them. |
| `config-validate` | `devflow doctor`; every command reports configuration errors. |

## 1. Translate the configuration

| 0.x (`.devflow.yml`) | 1.0 (`devflow.toml`) |
| --- | --- |
| `services:` entry with `service_type: postgres` | `[postgres]` with `mode = "managed"` and `image = "postgres:18"`. The image must be PostgreSQL 18 or newer. |
| other `services:` entries | Removed. Declare them in `compose.yaml` or `pitchfork.toml`. |
| `worktree.path_template: "../{repo}.{workspace}"` | `[worktree] dir = "../{repo}.worktrees"`. This is the parent directory; each workspace gets a subdirectory. |
| `worktree.copy_files` | `[worktree] copy` |
| `worktree.copy_ignored`, AI-config copying | Removed. List the paths you want in `copy`. |
| `hooks.post-create` | `[hooks] setup` |
| `hooks.post-switch` | `[hooks] switch` |
| `hooks.pre-remove` | `[hooks] teardown` |
| other phases (`pre-switch`, `post-start`, `post-remove`, `pre-commit`, `*-service-*`) | Removed. Fold the work into the three lists, or use Git hooks (for example with hk). |
| `{{ service['app-db'].url }}` | `$DATABASE_URL` and the `PG*` variables, from `.devflow/env` |
| `{{ workspace }}` | `$DEVFLOW_WORKSPACE` (also `DEVFLOW_WORKSPACE_PATH`, `DEVFLOW_PROJECT_ROOT`, `DEVFLOW_SOURCE_PATH`, `DEVFLOW_WORKSPACE_ID`, `DEVFLOW_PHASE`) |
| `write-env`, `write-file`, `replace`, `copy`, `docker-exec`, `http` actions | Shell commands. `.devflow/env` is generated for you. |
| `processes:` | `pitchfork.toml` |
| `git.*`, `behavior.*` (`main_workspace`, `exclude_workspaces`, `max_workspaces`, `auto_create_on_workspace`) | Removed. The main checkout is the default workspace. `devflow hook install` prepares worktrees created by other tools. |
| `agent:`, `commit:` | Removed |
| `.devflow.local.yml` | `devflow.local.toml` (git-ignored) |
| `DEVFLOW_APPROVE_HOOKS`, `DEVFLOW_SKIP_HOOKS`, `DEVFLOW_DISABLED`, `DEVFLOW_AUTO_CREATE` | `DEVFLOW_TRUST_HOOKS`, `switch --no-hooks`; the others are removed |

Before:

```yaml
# .devflow.yml
services:
  - name: app-db
    type: local
    service_type: postgres
    default: true
    local:
      image: postgres:17

worktree:
  path_template: "../{repo}.{workspace}"
  copy_files: [.env.local]

hooks:
  post-create:
    install: "npm ci"
    env:
      action:
        type: write-env
        path: .env.local
        vars:
          DATABASE_URL: "{{ service['app-db'].url }}"
    migrate: "npm run migrate"

processes:
  daemons:
    api:
      run: "npm run dev"
```

After:

```toml
# devflow.toml
[project]
name = "shop"
id = "…"                          # written by `devflow init`

[worktree]
dir = "../{repo}.worktrees"
copy = [".env.local"]

[postgres]
mode = "managed"
image = "postgres:18"

[hooks]
setup = ["mise trust", "npm ci", "npm run migrate"]   # DATABASE_URL is already set; a new worktree is not trusted by mise yet
```

```toml
# mise.toml: load the generated variables in every command in the worktree
[env]
_.file = ".devflow/env"
```

```toml
# pitchfork.toml
[daemons.api]
run = "mise exec -- npm run dev"
```

Move or delete `.devflow.yml`, `.devflow.local.yml` and any 0.x `.devflow.toml`. `devflow init` refuses while a `.devflow.toml` exists, and until a `devflow.toml` exists the other commands stop with the migration error. Then run `devflow init --postgres managed`. It writes `devflow.toml` with a fresh project id; edit it as above. Do not run `devflow trust` yet: part 2 restores your data before the setup hooks, which usually migrate the database, run.

## 2. Move your data

1.0 requires PostgreSQL 18, so 0.x data directories cannot be reused. Dump and restore.

**With 0.8** (services still running), for each workspace whose data you want:

```sh
devflow-0.8 connection main                       # prints the connection string
pg_dump --format=custom --no-owner --no-acl \
  --file shop.dump "postgres://…old connection string…"
```

Use the PostgreSQL 18 client tools if you have them; they can dump older servers.

**With 1.0**, in the main checkout:

```sh
devflow db up                                     # start the managed server (or point external mode at yours)
devflow --non-interactive setup                   # create the default workspace's empty database and .devflow/env
pg_restore --no-owner --no-acl --dbname "$(devflow db url)" shop.dump
#   plain SQL dumps: psql "$(devflow db url)" -f shop.sql
devflow db snapshot create seed                   # reusable starting point for new workspaces
devflow trust                                     # review and approve the [hooks] commands
devflow setup                                     # run the setup hooks against the restored data
```

`devflow db up` only starts the server; it creates no database. The default workspace gets its database, and a receipt that `devflow db url` reads, when `devflow setup` runs in the main checkout, so run it before the restore. `devflow db url` then prints the URL of that database.

`setup` also runs the `[hooks] setup` commands once the hooks are trusted, and you do not want a migration to run before the restore. That is why `trust` comes last. Until then, `--non-interactive` skips the untrusted hooks: the first `setup` exits with status 2 and reports `needs_attention` (the hooks phase was skipped), which is expected here because the database already exists. The second `setup` resumes at the hooks phase. If `[hooks] setup` is empty, the first `setup` finishes `ready` and the last two commands are not needed.

Restore with `--no-owner --no-acl`: the old roles do not exist, and the workspace login already acts as the project owner role, so restored objects belong to it. Trusted extensions install from the dump. Untrusted extensions need the admin role; create them in the database with the admin connection before restoring.

Then create workspaces from the seed:

```sh
devflow switch -c feature/x --data-from snapshot:seed
```

Existing 0.x worktrees are ordinary Git worktrees. Run `devflow setup` inside one to adopt it; it uses the default workspace as its code and data source. `devflow setup --database-url URL` attaches an existing database without provisioning.

## 3. Remove the 0.x resources

devflow 1.0 will not do this for you. Use 0.8 (written as `devflow` below; run your `devflow-0.8` copy) from each project, while its state files still exist:

| Goal | 0.8 command |
| --- | --- |
| Stop the background pieces | `devflow daemon stop`, `devflow proxy stop`, `devflow process stop --all` |
| Delete one workspace's services but keep its worktree | `devflow service delete WORKSPACE` |
| Delete one workspace completely (worktree, branch, services) | `devflow remove WORKSPACE` |
| Delete the service data of the project | `devflow service destroy --force` |
| Tear down the whole project | `devflow destroy --force` (read its `--help` first) |
| Find leftovers from deleted or moved projects | `devflow gc --list`, then `devflow gc --all` |
| Remove the proxy CA from your trust store | `devflow proxy trust remove` |
| Remove the Git hooks 0.x installed | `devflow uninstall-hooks` |

Remove the 0.x Git hooks before you commit again. The `pre-commit` hook runs `devflow hook run pre-commit`, which 1.0 does not have, so it fails and blocks every commit. The `post-checkout` hook calls `devflow git-hook`, which is gone too, and `devflow hook install` in 1.0 treats it as a hook it did not write and refuses to overwrite it. If 0.8 is not available, delete `.git/hooks/post-checkout` and `.git/hooks/pre-commit` (or the same files in your `core.hooksPath` directory) when they contain `# devflow auto-generated hook`. Then run `devflow hook install` in 1.0 if you want worktrees made by other tools set up.

If 0.8 is gone, find containers by the labels it set and remove them one by one after reading the list:

```sh
docker ps -a --filter label=devflow.managed=true \
  --format 'table {{.ID}}\t{{.Names}}\t{{.Label "devflow.project"}}\t{{.Label "devflow.workspace"}}'
docker rm -f CONTAINER_ID...
```

Check the listing before removing anything, and do not remove by name prefix. Then delete the leftover volumes and data directories (the data root you configured, or under `devflow/` in your platform's data directory) and the 0.x state files in devflow's config directory: `config.yml`, `local_state.yml`, `hook_approvals.yml`, `daemon-status.json`, `daemon-owner.json`, `daemon.lock` and the directories `executions/`, `operations/`, `storage-operations/`, `port-reservations/` and `resources/`. 1.0 uses `projects.toml` and `trust.toml` in the same directory and ignores the rest.
