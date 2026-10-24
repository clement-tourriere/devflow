# Command overview

Every command, by purpose. `devflow --help` lists them and `devflow COMMAND --help` has the flags; the guide page of each topic is linked from the [index](README.md).

| Command | Purpose |
| --- | --- |
| `hooks list\|add\|update\|remove\|move`, `env list\|set\|unset\|preset add\|remove` | List and edit the `[hooks]` commands and the `[env]` recipes and presets in `devflow.toml` (or `devflow.local.toml` with `--local`), keeping comments and refusing stale writes (`--if-unchanged SHA256`) |
| `init`, `trust`, `hook install\|uninstall` | Propose and write the config (`--plan`, `--apply PLAN.json`, `--yes`, `--accept`/`--reject ID`, `--name`, `--postgres managed\|external\|none`; `--hook` also installs the Git hook), review and approve what the project runs (`-y` skips the question), install the post-checkout hook |
| `switch [-c] [NAME]`, `switch -`, `list [--all\|--wide]`, `path NAME`, `open [NAME]` | Create, enter and inspect workspaces (a picker without a name, the previous workspace with `-`); `--all` lists every registered project, `--wide` adds changes, port, merge evidence, containers and daemons; `open` starts the configured editor or terminal |
| `gc [--remove ID…]` | Find workspaces whose worktree Git or an agent host removed (T3 Code, Cursor, Codex and `git worktree remove` do not tell devflow) and see what they still own: database branches, containers, daemons, with sizes and advisory merge evidence. Alone it changes nothing; `--remove ID…` removes the ones you name (`--snapshot` keeps a snapshot of each data branch first) |
| `remove NAME` | Delete a workspace; `--keep-data` keeps its database branch, `--delete-branch` also deletes the Git branch, `--force` overrides dirty, locked and failing-teardown protections |
| `exec [--workspace NAME] -- CMD [ARGS…]` | Run a command with the workspace's `.devflow/env` applied last (over whatever mise set); see [Commands that need the workspace's database go through `devflow exec`](hooks-env-trust.md#commands-that-need-the-workspaces-database-go-through-devflow-exec) |
| `setup` | Run the same pipeline in an existing worktree (adopt, resume, fork); `--skip-mise-check` (also on `switch`) runs the setup hooks without the mise checks below |
| `db status\|up\|down\|url\|psql\|reset\|reconcile` | Manage the server and a workspace's database; `db reconcile --drop-orphans` also drops scratch databases the configuration no longer lists |
| `db snapshot create\|list\|delete`, `db branch list\|create\|delete` | Reusable seed data and explicit branch control |
| `service [NAME] status\|up\|down\|url\|list\|snapshot create\|list\|delete\|reset\|reconcile\|drop\|forget` | Valkey, Redis and MySQL (preview) branches: containers, snapshots, staged reset, crash recovery (`list --orphans` finds data nothing uses); for a `sqlite` service the same words act on the files |
| `wait [PATH] [--timeout D]`, `setup --wait`, `exec --wait` | Wait for a setup that runs elsewhere (a Git hook, a host's script) and end at a timeout: exit 0 ready, 2 not ready or still running, 1 an error |
| `integrate [t3\|claude\|conductor\|cursor] [--remove] [--print]` | Make an agent host set new worktrees up with devflow: writes the host's project file additively (alone, lists the hosts the repository shows signs of) |
| `prune [NAME…] [--yes]` | Forget the Git registrations of worktrees whose directory is gone: it lists the candidates and, at a terminal, asks before forgetting (`--yes` skips the question; under `--json` it only lists); registrations that still own data are kept |
| `project forget\|relocate` | Mend the list of known projects: forget one, or point one at its new folder (nothing on disk changes) |
| `pitchfork status\|install\|proxy` | Is pitchfork installed and its proxy on; install it (asks first); print the proxy commands (devflow never runs them) |
| `up`, `down`, `ps` | A workspace's pitchfork daemons; `up --services` and `down --services` also start and stop its own containers, `down --all` covers every workspace |
| `suspend [NAME \| --idle D]`, `resume`, `pin`, `unpin`, `focus [NAME]` | Rest a workspace's own containers and daemons (or the idle ones), enter it again, keep it out of `--idle`; forward canonical loopback ports to the workspace in use (`focus --stop` ends it) |
| `config schema [--local]`, `config migrate` | Print the JSON Schema of `devflow.toml` (`--local`: of `devflow.local.toml`) for editors; rewrite an earlier build's `[postgres]` table as `[services.db]` |
| `doctor`, `shell-init`, `completions SHELL`, `gui` | Diagnostics (including whether the shell integration is active), shell integration, the shell completion script, desktop app |

All inspection and mutation commands accept `--json`; `db psql`, `shell-init`, `completions` and `gui` do not (`config schema` prints the schema, which is JSON already, with or without it). Run `devflow --help` for flags.
