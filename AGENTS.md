# devflow for agents and automation

devflow gives each task its own Git worktree, copy-on-write payloads, a database branch and a generated env file. This page is the contract an agent or a script can rely on; [docs/agents-reference.md](docs/agents-reference.md) has every command's JSON document, its fields and its refusals. Humans start with the [README](README.md).

## Output

| Flag | Effect |
| --- | --- |
| `--json` | Print exactly one JSON document on stdout. Progress and hook output go to stderr; warnings go in `warnings`. Never prompts. |
| `--non-interactive` (or `DEVFLOW_NON_INTERACTIVE=1`) | Never prompt. Untrusted hooks are skipped instead of asked about. |
| `-C DIR` | Run as if devflow was started in `DIR`. |

Every document has `version` (currently `1`) and `command`. With `--json`, every failure prints `{"version": 1, "command": …, "error": "…"}` and exits 1: a missing argument, a bad configuration and a refused operation alike. `hint` is the first thing to try and `remedies` all of them, when devflow knows the way on. A command that can succeed in part (`doctor`, `up`, `down`, `suspend`) puts `ok` in its document and exits 1 when it is false.

```json
{"version": 1, "command": "list",
 "error": "no devflow.toml in /work/shop",
 "hint": "run `devflow init` in /work/shop (`devflow init --plan` shows the proposed setup first)",
 "remedies": [{"kind": "init", "root": "/work/shop", "label": "Set up devflow…", "command": "devflow init", "destructive": false}]}
```

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Done; for `switch` and `setup`, the workspace is `ready`. |
| 1 | Error: usage, configuration, an unreachable server, a refused protection. The message is in `error`. A phase that fails after the worktree was created keeps the worktree and names it: run `switch -c NAME` or `devflow setup` there again. |
| 2 | The workspace is not ready: `needs_attention` (a required phase was skipped or failed; the reason is in the document) or `in_progress` (another process holds it). Fix the reason, or wait, and run `devflow setup`. |

## Remedies

A remedy is data, not a sentence to parse: its `kind`, the target it acts on (always named: a workspace path, a project root, a service key, never "the current directory"), a `label`, the equivalent CLI `command` when there is one, and `destructive`. Messages never carry a command line; the fix travels as a remedy. The kinds an agent meets most are `setup` and `trust` (a workspace that is not ready), `use_default_recipes`, `retry`, `start_database`, `start_service`, `choose_workspace`, `choose_data_source`, `remove_workspace`, `clean_up`, `open_file`. New kinds appear over time, so ignore the ones you do not know; the reference lists every kind with its fields.

## The agent flow

```sh
devflow --json --non-interactive switch -c agent/TASK > switch.json   # exit 0 ready, 2 needs attention, 1 error
path=$(jq -r '.workspace.path' switch.json)                           # authoritative: do not build the path yourself
(cd "$path" && devflow exec -- npm test)                              # the workspace's variables, applied last
cd "$MAIN_CHECKOUT" && devflow --json --non-interactive remove agent/TASK --force
```

1. **Create.** `switch -c NAME` makes the worktree, clones the payload, branches the data, writes `.devflow/env` and runs the setup hooks. `--from WS` picks the code source, `--data-from WS|snapshot:NAME[@vN]|branch:ID|empty` the data source (both are fixed once chosen), `--base REF` where a new branch starts. A run that cannot prompt and names no data source takes `[data] noninteractive_from`, a seed snapshot you keep for it, or fails with a remedy.
2. **Wait.** For a setup another tool runs (a Git hook, a host script): `devflow wait [PATH] [--timeout D]` exits 0 `ready`, 2 not ready or still running at the timeout, 1 an error; `setup --wait` and `exec --wait -- CMD` wait in front of the work.
3. **Exec.** Run every command that needs the workspace's database through `devflow exec -- CMD` (inside mise: `mise exec -- devflow exec -- CMD`). It exits 1 rather than start the command without `.devflow/env`. Do not edit `.devflow/` or the receipt in the worktree's Git directory.
4. **Remove.** From the main checkout, never from inside the worktree: the default workspace and the one holding your directory are refused, `--force` or not. `--force` accepts a dirty or locked worktree and a failing teardown hook; `--keep-data` keeps the data branches.

Trust is the one step that is not yours: hooks and `[env]` recipes no person approved are skipped and the workspace is `needs_attention` (exit 2). A person runs `devflow trust` once; `devflow --json trust` shows what an approval covers and approves nothing; `DEVFLOW_TRUST_HOOKS=1` trusts a single run, for CI. To set a project up, do not write `devflow.toml` from guesses: `devflow --json init --plan > plan.json`, read it, `init --apply plan.json --accept ID --reject ID`, then edit hooks and `[env]` with `devflow hooks` and `devflow env` (comments kept, stale writes refused with `--if-unchanged SHA256`). [`examples/agent-task.sh`](examples/agent-task.sh) runs the flow with cleanup after a failure.

## Host integrations

`devflow integrate HOST` writes the host's project file so the host sets each new worktree up with devflow before its agent starts: `t3` (`t3.json`), `claude` (`.claude/settings.json`: `WorktreeCreate` and `WorktreeRemove` run `devflow integrate claude-create` and `claude-remove`), `conductor` (`.conductor/settings.toml`: `setup` and an `archive` that removes without forcing), `cursor` (`.cursor/worktrees.json`). It is additive, `--print` writes nothing, and a script that is not devflow's is reported with the line to add by hand (exit 1). Other tools that use Git are covered by `devflow hook install`. Hosts that delete worktrees themselves give devflow no teardown hook: `devflow gc` lists the workspaces whose worktree is gone and still own data, containers or daemons, and `devflow gc --remove ID… --yes` removes the ones you name.
