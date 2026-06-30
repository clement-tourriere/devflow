---
title: CLI reference
description: Every devflow command, grouped by workflow, with flags and examples.
sidebar:
  order: 1
---

Run `devflow --help-all` for the authoritative surface straight from your binary. Automation should prefer `--json --non-interactive` ([contract](/devflow/guides/ai-agents/#the-contract)).

## Global flags

```bash
devflow [--json] [--non-interactive] [-s <service-name>] <command>
```

| Flag | Description |
| --- | --- |
| `--json` | structured JSON on stdout where supported |
| `--non-interactive` | no prompts; unapproved hooks are skipped with a warning; destructive ops require `--force` |
| `-s <name>` | target a specific configured service (defaults to the `default: true` service) |

## Daily workspace flow

### `devflow switch [workspace]`

Create or switch a workspace, align services, move into the worktree, run hooks. No argument = interactive fuzzy picker.

```bash
devflow switch
devflow switch feature/auth
devflow switch -c feature/new --from develop
devflow switch feature/auth -x "npm run dev" --detach
devflow switch -c agent/task-42 --sandboxed
devflow switch feature/auth --dry-run
```

| Flag | Effect |
| --- | --- |
| `-c, --create` | create the workspace first |
| `-b, --from <ws>` (alias `--base`) | parent workspace for creation (default: current context) |
| `-x, --execute <cmd>` | run a command after switching (in the worktree); trailing args after `--` |
| `-d, --detach` | run the `-x` command in a detached tmux/zellij session |
| `-o, --open` | open an interactive multiplexer session in the workspace |
| `--no-services` | VCS only — skip service branching |
| `--no-processes` | skip process auto-start during switch |
| `--no-verify` | skip **all** hooks |
| `--template` | switch to the main/template workspace |
| `--dry-run` | print the plan (worktree path, services, hooks) without acting |
| `--no-respect-gitignore` | also copy gitignored entries into a newly created worktree (one-shot `copy_ignored: true`) |
| `--sandboxed` / `--no-sandbox` | force sandbox on/off for this workspace |

### `devflow list` · `devflow graph` · `devflow status`

```bash
devflow list            # workspaces with service + worktree state (--json includes worktree_path)
devflow graph           # full tree: workspaces → services → worktree paths
devflow status          # current workspace, services, connections
```

### `devflow connection <workspace>`

Alias for `service connection`. `--format uri|env|json`.

### `devflow link <workspace>`

Adopt an existing VCS branch into devflow (registry entry + optional service materialization). `--from <ws>` sets the parent.

### `devflow remove <workspace>`

Delete workspace + worktree + services. Refuses on a dirty worktree without `--force`; requires `--force` in `--json`/`--non-interactive`. `--keep-services` removes only branch + worktree.

### `devflow cleanup`

Alias for `service cleanup` (`--max-count <n>`).

## Merge & rebase

```bash
devflow merge [target] [--cleanup] [--dry-run] [--check-only] [--force] [--cascade-rebase]
devflow rebase [target] [--dry-run]
devflow train add|remove|status|run|pause|resume [...]
```

See [Merging & merge trains](/devflow/guides/merging/). Train flags: `--target <ws>`, `run --stop-on-failure --cleanup`.

## Services

```bash
devflow service add [name] [--provider local] [--service-type postgres] [--from <seed>]
devflow service remove <name>              # remove the service config
devflow service list | status | capabilities
devflow service up                         # start all shared global engines (one-shot reconcile)
devflow service create <ws> [--from <parent>]
devflow service delete <ws>                # delete instances; keep branch + worktree
devflow service start|stop|reset <ws>
devflow service connection <ws> [--format uri|env|json]
devflow service logs <ws> [--tail N]
devflow service seed <ws> --from <file|postgres-url|s3-url>
devflow service discover [--service-type t] [--global]
devflow service cleanup [--max-count N]
devflow service destroy [--force]          # destroy ALL data for a service
```

## Processes

```bash
devflow process start [names...] [--all] [--workspace <ws>] [--force]
devflow process stop [names...] [--all] [--workspace <ws>]
devflow process restart [names...] [--all] [--workspace <ws>]
devflow process list|status [--workspace <ws>]
devflow process logs <name> [--workspace <ws>] [--tail N] [--follow]
```

Processes are workspace-scoped app commands configured under `processes.daemons` (web servers, workers, schedulers). They run in the worktree, capture stdout/stderr to devflow logs, support dependency ordering, port bumping, and readiness checks, and can interpolate service URLs via MiniJinja (`{{ service['app-db'].url }}`). `processes.auto_start: true` makes `devflow switch` start them after services and hooks; auto-started shell commands use the same approval store as hooks (`devflow hook approvals add "npm run dev"` or `DEVFLOW_APPROVE_HOOKS=1` for automation). `processes.provider: pitchfork` embeds Pitchfork's Rust supervisor/log APIs directly. Running processes with ports are exposed by `devflow proxy` as `https://<process>.<workspace>.<project>.<suffix>` (default `.local`). `devflow remove` stops them before cleanup. Run `devflow daemon start` to keep desired-state, `watch` restart-on-change, and `retry` reconciliation active in the background. See [Project processes & Pitchfork](/devflow/guides/processes/) for Compose migration patterns and provider details.

## Controller daemon

```bash
devflow daemon start [--interval 30] [--once] [--foreground]
devflow daemon status
devflow daemon stop
```

Keeps every registered project's shared engines running ([details](/devflow/guides/shared-engines/#keeping-engines-alive)) and reconciles managed process desired-state plus `watch`/`retry` behavior.

## Hooks

```bash
devflow hook show [phase]
devflow hook run <phase> [name] [--workspace <ws>]
devflow hook explain [phase]
devflow hook vars [--workspace <ws>]
devflow hook render "<template>"
devflow hook approvals [list|add <template>|clear]
devflow hook triggers                  # VCS event → phase mapping
devflow hook actions                   # built-in action types
devflow hook recipes
devflow hook install <recipe>
```

## AI & automation

```bash
devflow commit [--ai] [--edit] [--dry-run] [-m <msg>]
devflow agent status | context [--format json] [--workspace <ws>]
devflow agent skill                    # install bundled workspace skills
devflow skill list | search <q> | ...  # full skills management (skills.sh marketplace)
devflow sync-ai-configs                # merge AI tool configs from worktree back to main
devflow capabilities                   # machine-readable automation contract
```

## Reverse proxy

```bash
devflow proxy start [--daemon] [--https-port 443] [--http-port 80] [--api-port 2019]
                    [--domain-suffix local] [--no-mdns] [--no-auto-network]
devflow proxy stop | status | list
devflow proxy trust install | verify | remove | info
```

See the [proxy guide](/devflow/guides/proxy/).

## Setup & maintenance

```bash
devflow init [path] [--name <n>] [--force]    # initialize (worktrees default ON when non-interactive)
devflow destroy [--force]                     # tear down the whole project (irreversible)
devflow config [-v]                           # merged config (+ precedence details)
devflow doctor                                # diagnostics: docker, vcs, config, storage, hooks
devflow install-hooks | uninstall-hooks       # git hooks: post-checkout, post-merge, pre-commit, post-rewrite
devflow shell-init [bash|zsh|fish]            # print the auto-cd wrapper
devflow worktree-setup                        # set up devflow inside a manually created worktree
devflow setup-zfs [--size 20G] [--pool-name p]  # file-backed ZFS pool (Linux)
devflow gc [--list] [--all] [--force]         # detect/clean orphaned projects and leftover state
devflow tui                                   # terminal dashboard
devflow plugin list | check <name> | init <name> --lang bash|python
```

## Environment overrides

`DEVFLOW_CONTEXT_BRANCH=<ws>` overrides the context workspace used as default parent — useful in CI. The full table is in [Environment variables](/devflow/reference/environment/).
