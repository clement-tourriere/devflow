# Agents and hosts

devflow is built to be driven by agents: every command that inspects or changes something prints one versioned JSON document with `--json`, a failure carries remedies as data, and the exit code says whether a workspace is ready. This page is the human-sized guide: how an agent uses a workspace, how agent hosts set workspaces up and tear them down, and what to do about worktrees a host removes. [AGENTS.md](../../AGENTS.md) is the contract and [the reference](../agents-reference.md) has every document, field and refusal.

- [An agent's flow](#an-agents-flow)
- [Waiting for a setup](#waiting-for-a-setup)
- [Agent hosts: `devflow integrate`](#agent-hosts-devflow-integrate)
- [The Git hook](#the-git-hook)
- [`.worktreeinclude`](#worktreeinclude)
- [When a host removes a worktree](#when-a-host-removes-a-worktree)

## An agent's flow

```sh
devflow --json --non-interactive switch -c agent/fix-login   # read workspace.path from the output
```

- `--json` prints one versioned document; `--non-interactive` disables prompts. Exit code 2 means the workspace is not `ready` (`needs_attention` or `in_progress`).
- Commands that need the workspace's database go through `devflow exec`: `cd "$path" && devflow exec -- npm test`. It applies `.devflow/env` last, from any subdirectory, and fails (exit 1) rather than start the command without it.
- A non-interactive creation can start from a seed snapshot instead of the code source's data: set `[data] noninteractive_from = "snapshot:NAME"` (see [Seed data for automation](data-services.md#seed-data-for-automation-data)).
- Untrusted hooks and `[env]` recipes are skipped in non-interactive mode and the workspace is marked `needs_attention`. Run `devflow trust` once (`-y`/`--yes` approves without asking), or set `DEVFLOW_TRUST_HOOKS=1` for a single run in CI.

[`examples/agent-task.sh`](../../examples/agent-task.sh) implements this flow: create, run a command, remove, with the cleanup after a failure. [AGENTS.md](../../AGENTS.md) is the full contract.

## Waiting for a setup

`devflow wait [PATH] [--timeout 10m]` waits for a setup that runs elsewhere (a Git hook, a host's script, another terminal) and reports how it ended: exit 0 `ready`, 2 `needs_attention`, still running at the timeout, or never set up, 1 an error. `devflow setup --wait` sets the worktree up, or, when another run holds it, waits for that run. `devflow exec --wait -- CMD` starts the command only once the workspace is ready. Waiting never holds the lock it waits for, and always ends at the timeout (default 15 minutes).

## Agent hosts: `devflow integrate`

`devflow integrate HOST` writes the host's project file, so the host sets every new worktree up with devflow before its agent starts. It keeps what the file holds, never replaces a script that is not devflow's (it reports it, with the line to add by hand, and exits 1), shows the change before writing it and writes nothing with `--print`; `--remove` takes devflow's entries out again. `devflow integrate` alone lists the hosts the repository shows signs of, and `devflow init` offers them.

| Host | File | What runs |
| --- | --- | --- |
| `t3` ([T3 Code](https://t3.codes)) | `t3.json` | a script with `runOnWorktreeCreate` and `async: false` running `devflow setup --wait`: the agent starts once the workspace is ready. T3 Code has no hook when it deletes a worktree: what a deleted workspace leaves behind is found again by the clean-up (`devflow gc`) |
| `claude` ([Claude Code](https://code.claude.com)) | `.claude/settings.json` | `WorktreeCreate` runs `devflow integrate claude-create` (reads `{"name": …}`, creates workspace `worktree-NAME` non-interactively, prints only the worktree path as the last line of standard output; a workspace that is not ready fails the creation) and `WorktreeRemove` runs `devflow integrate claude-remove` (never forces: uncommitted work refuses the removal and Claude Code keeps the worktree; the Git branch is kept). A worktree has one creator, so a hook of someone else's on either event stops the integration |
| `conductor` ([Conductor](https://conductor.build)) | `.conductor/settings.toml` | `[scripts] setup = "devflow setup --wait"` and an `archive` script that removes the workspace and keeps its branch (it never forces: a dirty workspace or a failing teardown exits non-zero and says why; what Conductor does then is not documented). A `CONDUCTOR_PORT` present when a workspace is created becomes its app port (`DEVFLOW_PORT`) |
| `cursor` ([Cursor](https://cursor.com)) | `.cursor/worktrees.json` | `devflow setup --wait` first in `setup-worktree` (and in `setup-worktree-unix` when that exists: it wins on macOS and Linux). Cursor has no teardown and deletes worktrees past its limit (25 by default), even ones it did not create; the clean-up finds what they leave |

Codex runs a setup script from its local environment under `.codex/`, but documents no file format, so devflow writes nothing there. Its worktrees are detached HEAD: devflow adopts them under their directory name.

## The Git hook

The Git hook (`devflow hook install`, or `devflow init --hook`, or the setup wizard) covers any tool that uses Git, with limits: by default `git worktree add` waits for the whole setup; `[integrations] git_hook = "background"` starts the setup detached (its own process group, log in the worktree's Git directory) and returns at once, and the script or agent that needs the workspace calls `devflow wait`. A relative `core.hooksPath` that a new worktree does not have (husky's generated, ignored `.husky/_`) never fires there; `devflow doctor` says so and points to the host integration. The hook does nothing unless `devflow` is on the `PATH` of the process running Git.

Worktrees created by other tools (T3 Code, Claude Code, plain `git worktree add`) are prepared by a `post-checkout` hook: `devflow hook install` once, or `devflow init --hook`. The hook does nothing unless `devflow` is on the `PATH` of the process running Git.

## `.worktreeinclude`

`.worktreeinclude` at the repository root (gitignore syntax; Claude Code, Codex, Conductor and worktrunk read it too) lists local files a new worktree should get: the ones it matches **and** Git ignores are cloned with the `[worktree] copy` entries and reported like them. Tracked files are never duplicated, and an ignored directory is looked inside only when a pattern names it (`vendor/**/config.json`).

## When a host removes a worktree

Hosts that delete worktrees themselves (T3 Code, Cursor, Codex, Claude Code) give devflow no teardown hook, so a workspace's data branches, containers and daemons would stay behind with the receipt gone. devflow records what each workspace owns outside the worktree; `devflow list` and `devflow doctor` say when a workspace removed that way still owns something, and `devflow gc` lists them and removes the ones you name.

