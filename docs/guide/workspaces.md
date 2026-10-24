# Workspaces

A workspace is a Git worktree plus everything devflow prepared for it: a copy-on-write payload, a data branch per service, a generated `.devflow/env`, a port of its own, and a receipt that records how far its setup got. The main checkout is the default workspace. Every workspace is identified by the name it was created under, not by the branch its checkout holds now.

- [Creating a workspace](#creating-a-workspace)
- [Finding and entering workspaces](#finding-and-entering-workspaces)
- [Removing a workspace](#removing-a-workspace)
- [Each workspace has its own port](#each-workspace-has-its-own-port)
- [Shell integration](#shell-integration)

## Creating a workspace

`devflow switch -c NAME` makes the worktree, clones the payload, branches each service's data, writes `.devflow/env` and runs the setup hooks; `devflow switch NAME` enters an existing workspace and finishes any phase it had not reached, so running either again is safe. `--from WS` chooses the workspace the code and the payload come from (default: the default workspace), `--data-from WS|snapshot:NAME[@vN]|branch:ID|empty` where the data comes from (see [data services](data-services.md)), and `--base REF` where a new branch starts: a branch, tag, commit or remote branch such as `origin/NAME`, which `NAME` then tracks. Without `--base`, a branch that exists only as a remote branch of that name is created there and tracks it. `--no-db` and `--no-hooks` skip those phases, and the skip is recorded. `devflow setup` runs the same pipeline in an existing worktree (adopt, resume, fork).

`devflow switch -c` prints the new worktree path (`devflow path feature/x` prints it again). The directory is the sanitized workspace name (`feature/x` becomes `feature-x`), so use the printed path rather than building it. The generated `.devflow/env` holds `DATABASE_URL`, the `PG*` variables and `DEVFLOW_*` identifiers. The directory ignores itself, so it never shows up in `git status`.

## Finding and entering workspaces

- `devflow switch` with no name opens a picker at a terminal (most recently entered first, with branch, status and where a checkout drifted); without a terminal it is a usage error that points at `devflow list`. `devflow switch -` enters the workspace this shell was in before its last `devflow switch` and lands in the directory it left (it needs the shell integration and says so when that is missing).
- A switch keeps the subdirectory you are in when the target has it: from `apps/web` you land in the target's `apps/web`, else at its root.
- `devflow open [NAME] [--editor|--terminal]` starts the editor (the default) or the terminal in a workspace. The commands are `editor_command` and `terminal_command` in `<config dir>/settings.toml` (defaults: `code`; `open -a Terminal` on macOS); the workspace's path is appended unless the command contains `{path}`. The desktop app reads the same file, and moves the values an older `gui.toml` held into it the first time it starts.
- `devflow list --wide` adds what costs more to find than reading Git and receipts: uncommitted changes, ahead/behind the default branch's upstream, advisory evidence the branch was merged (in the default branch's history, the same content, or "looks squash-merged" from patch ids: a hint, never acted on), when the workspace was last entered, its port, its containers (Docker is asked, once per container, under a short bound) and its daemons with ports and URLs. The default `list` stays offline and fast; `--json` gains a `wide` object per workspace.
- `devflow down [NAME] --services` also stops the workspace's own containers (Valkey, Redis, MySQL: their data stays in the volumes, and the project's shared PostgreSQL server is never stopped); `devflow up --services` starts them first. `devflow down --all` covers every workspace of the project, so `down --all --services` winds the whole project down. The desktop app has the same buttons in a workspace's header.

## Removing a workspace

`devflow remove NAME` checks every protection before it changes anything, then stops the workspace's daemons, runs the `teardown` hooks, drops the data branches this workspace created, and removes the worktree. It always refuses the default workspace and the workspace you are standing in, and `--force` does not override either. Dirty or locked worktrees, untrusted teardown hooks, and a failing teardown hook need `--force`; `--keep-data` keeps the workspace's database branch (and its other services' branches), `--delete-branch` also deletes the Git branch (without `--force` it requires the branch to be merged into the default workspace's HEAD). For a workspace whose directory is already gone, `devflow prune` forgets the Git registration, and `devflow gc` finds what a worktree removed outside devflow still owns ([agents and hosts](agents-and-hosts.md)). The daemons are pitchfork's; see [Processes](processes.md) for how removal treats them.

## Each workspace has its own port

Two dev servers started from two workspaces both want port 3000. Every workspace is given one port of its own, exported as `DEVFLOW_PORT` in `.devflow/env`, in hooks and through `devflow exec`, usable in `[env]` recipes and compose files as `${DEVFLOW_PORT}` (a recipe cannot set it):

```toml
[env]
PORT = "${DEVFLOW_PORT}"
```

The port is chosen once, from `[worktree] port_range = [40000, 49999]` (that is the default), under a machine-wide lock in the data directory: a candidate is derived from the project and the workspace's identity, skipped when another workspace on this machine already holds it (a small ledger in `<data dir>/ports/` records them by workspace identity, not by directory, so two workspaces whose servers are not running do not share one and `git worktree move` frees nothing) or when something listens on it, written to the receipt before it is reserved, and then kept. devflow never moves a port a workspace has, even if something listens on it later: it stays what the variable, your compose files and your browser tabs say. If Conductor made the workspace (`CONDUCTOR_PORT` is set and `CONDUCTOR_WORKSPACE_PATH` names this workspace; an inherited `CONDUCTOR_PORT` alone is no evidence and is ignored), that port is used instead. A new identity for a checkout (`devflow setup --new-instance`, `DEVFLOW_INSTANCE_ID`) gets a port of its own and gives up the old one. The ledger is checked against the checkouts that carry each identity (project and instance), so a removed workspace's port is freed even if its directory is reused; a ledger that cannot be read is never replaced by an empty one: choosing a port is refused with the file named (the port a workspace already has is kept). `devflow ps` and `devflow list --wide` show it, and removing the workspace gives it back.

A workspace set up before ports existed gets its port the next time it is entered or set up (`devflow switch`, `devflow setup`, the Git hook's `setup --if-new`): `.devflow/env` is written again with `DEVFLOW_PORT` in it. Nothing else is reopened: no hook runs again, the configuration generation is unchanged, and where the `[env]` recipes are not approved the file is left alone (a port never turns a ready workspace into one that needs attention).

## Shell integration

Optional shell integration, so `devflow switch` changes your directory (and `devflow switch -` goes back), and workspace names complete when you press Tab:

```sh
eval "$(devflow shell-init zsh)"     # or bash, fish
eval "$(devflow completions zsh)"    # Tab completes workspace names for switch, path, remove, up, down, ps and open
```

The wrapper marks its calls (`DEVFLOW_WRAPPED=1`), which is how `devflow switch` and `devflow doctor` tell you when it is not active, and remembers the directory it left in `DEVFLOW_PREVIOUS`. The completion script only hooks the shell to devflow: the names come from local Git and the workspaces' receipts when you press Tab (no server, no Docker), so they are always current.
