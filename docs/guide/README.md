# devflow guide

One page per topic. The [README](../../README.md) is the short tour; these pages hold the detail, and every page links to the next one you are likely to need. For agents and scripts there is the [automation contract](../../AGENTS.md) and its [reference](../agents-reference.md).

| Page | What it covers |
| --- | --- |
| [Configuration](configuration.md) | `devflow.toml` and `devflow.local.toml`: the sections, which keys come from the main checkout and which from each worktree, the editor schema, `devflow init`, the native checkout |
| [Workspaces](workspaces.md) | Creating, finding, entering and removing workspaces; each workspace's own port; shell integration and completion |
| [Data services](data-services.md) | What a workspace's data is, where it comes from (`--from`, `--data-from`, snapshots, seed data for automation), and what a configuration change reopens |
| [PostgreSQL](postgresql.md) | The per-project server, database branches, busy sources, scratch databases for tests and shadows |
| [Valkey and Redis](valkey-redis.md) | A container and volume per workspace, how a clone is made, snapshots and reset, crashes |
| [MySQL (preview)](mysql.md) | A MySQL 8.4 container per workspace, the clean-stop clone, accounts, limits |
| [SQLite](sqlite.md) | A database file per workspace, copied under SQLite's own locks |
| [Hooks, environment and trust](hooks-env-trust.md) | `[hooks]`, `[env]` and `.devflow/env`, what `devflow trust` approves, `devflow exec`, the mise check |
| [Processes](processes.md) | pitchfork daemons, autostart, supported pitchfork and mise releases, resting workspaces (`suspend`, `pin`) and one stable port (`focus`) |
| [Agents and hosts](agents-and-hosts.md) | An agent's flow, `devflow wait`, `devflow integrate` for T3 Code, Claude Code, Conductor and Cursor, the Git hook, `.worktreeinclude`, cleaning up after a host |
| [Remote data hosts](remote-data-hosts.md) | PostgreSQL on an always-on machine, sandboxes that receive only a branch URL, forks |
| [Copy-on-write, measured](copy-on-write.md) | Which storage shares blocks, and what a clone costs in time and disk |
| [Desktop app](desktop-app.md) | The window over the same core: workspaces, data, setup, processes |
| [Command overview](commands.md) | Every command by purpose |
| [Troubleshooting](troubleshooting.md) | Symptoms and remedies, and what each `devflow doctor` check looks at |

Elsewhere: [MIGRATING.md](../MIGRATING.md) (from 0.x or an earlier 1.0 build), [ARCHITECTURE.md](../ARCHITECTURE.md) (the module map and how the pieces fit), [CONTRIBUTING.md](../../CONTRIBUTING.md), [SECURITY.md](../../SECURITY.md) and the [changelog](../../CHANGELOG.md).
