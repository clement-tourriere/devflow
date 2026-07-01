---
title: Troubleshooting & FAQ
description: Common issues — doctor, hooks that don't run, worktree problems, proxy resolution, and recovery commands.
sidebar:
  order: 99
---

Start with the built-in diagnostics — most issues surface there:

```bash
devflow doctor          # docker, VCS, config, storage, hook health (+ --json)
devflow config -v       # effective config with per-value provenance
devflow capabilities    # CoW method, automation guarantees
```

## Hooks didn't run

1. **Approval missing** — in `--non-interactive`/`--json` mode unapproved hooks are *skipped with a warning* (check `hooks[].skipped` in JSON). Fix: `devflow hook approvals add "<template>"` or `DEVFLOW_APPROVE_HOOKS=1`. Interactive runs prompt instead.
2. **`--no-verify`** skips all hooks entirely — including in Git-hook-triggered switches it wraps.
3. **Wrong phase** — `post-create` fires only when a branch/worktree was actually created; recurring setup belongs in `post-switch`. `devflow hook explain <phase>` documents each one.
4. **Condition false** — conditions resolve against the hook's working dir (the worktree). `devflow hook vars` + `devflow hook render "<condition>"` to debug.
5. **Background hook cut off** — raise `DEVFLOW_BACKGROUND_HOOK_TIMEOUT` (default 30s).
6. **devflow disabled** — `DEVFLOW_DISABLED`, `DEVFLOW_SKIP_HOOKS`, branch filters (`workspace_filter_regex`, `exclude_workspaces`, `DEVFLOW_DISABLED_BRANCHES`).

## Worktrees

- **“Failed to create worktree”** — name collision (two branches normalizing to the same `feature_x` directory — see [sanitization](/devflow/concepts/workspaces/#workspace-names)) or a leftover directory at the target path.
- **Stale metadata after deleting a directory by hand** — devflow auto-prunes when recreating the same name; otherwise `git worktree prune` (or the GUI's *Prune worktrees*).
- **Switch didn't `cd`** — [shell integration](/devflow/getting-started/shell-integration/) isn't installed in this shell. The path is printed either way.
- **Removal refused** — the worktree has uncommitted/untracked changes. Commit, stash, or `--force`.
- **`.env.local` missing in a new worktree** — list it in `worktree.copy_files`, or better, generate it with a `post-switch` `write-env` hook so values stay per-workspace.
- **Worktree exists but no `.claude/` dir** — it was created via plain `git worktree add` (the hook path doesn't copy AI dirs yet); run `devflow switch <branch>` once or copy manually.

## Services

- **Docker not running** — `switch` still completes (branch + worktree created; the service failure is reported per-service). Start Docker, then `devflow service create <ws>` or re-`switch`.
- **Container failed** — `devflow service logs <ws>`, then `devflow service reset <ws>` to re-clone from the parent.
- **Shared engine down** — `devflow service up` (one-shot) or `devflow daemon start` (keep-alive).
- **Port conflicts** — local providers allocate from `port_range_start`; adjust it per service. Never hardcode ports — template them (`{{ service['app-db'].port }}`).
- **Redis: “no free database”** — Redis has 16 DBs globally; remove stale workspaces (`devflow remove`) or use a dedicated `type: local` generic Redis.

## Proxy

- **Name doesn't resolve on the host** — is the proxy running (`devflow proxy status`)? On Linux, mDNS needs `avahi-daemon`. Fall back to the UPSTREAM IP from `devflow proxy list`.
- **Database name doesn't resolve / connects nowhere** — direct TCP endpoints need host-routable container IPs (Linux native, OrbStack on macOS — not Docker Desktop).
- **Browser certificate warning** — `devflow proxy trust install`, then restart the browser. `devflow proxy trust verify` to confirm.
- **Container not proxied** — check `devproxy.enabled` label, that it's running, and `devflow proxy list`. Explicit `devproxy.domains` always wins.

## Shell & TUI

- **A command appears to hang in a wrapped shell** — the wrapper captures stdout, hiding interactive prompts (e.g. `devflow remove` confirmation). Use `command devflow …` to bypass, or `--force`/`--non-interactive` flags.
- **`devflow tui` shows a blank screen** — same cause; run `command devflow tui`.

## Recovery & cleanup

```bash
devflow gc --list            # orphaned projects / leftover state
devflow gc --all --force
devflow cleanup --max-count 10   # prune old service workspaces
devflow uninstall-hooks          # remove devflow's git hooks (services/worktrees untouched)
devflow destroy                  # nuke the whole project's devflow footprint (irreversible)
```

## Where things live

| Path | Contents |
| --- | --- |
| `.devflow.yml` / `.devflow.local.yml` | committed config / local overrides |
| `~/.config/devflow/local_state.yml` | workspace registry (parents, worktree paths, flags) |
| `~/.config/devflow/hook_approvals.yml` | hook approvals (project root + template keyed) |
| `~/.config/devflow/config.yml` | global config (proxy ports, …) |
| `~/.local/share/devflow/` | service data directories (CoW clones) |
| `~/.devflow/proxy/` | proxy CA cert + key |

Still stuck? [Open an issue](https://github.com/clement-tourriere/devflow/issues) with `devflow --json doctor` output.
