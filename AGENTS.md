# devflow for agents and automation

devflow gives each task its own Git worktree, copy-on-write payloads, a database branch and a generated env file. This guide is the automation contract. Humans start with the [README](README.md).

## Output contract

| Flag | Effect |
| --- | --- |
| `--json` | Print exactly one JSON document on stdout. Progress and hook output go to stderr; warnings are added to the document's `warnings` array. Never prompts. |
| `--non-interactive` (or `DEVFLOW_NON_INTERACTIVE=1`; also `0`, `true`, `false`, `yes`, `no`, `on`, `off`) | Never prompt. Untrusted hooks are skipped instead of asked about. |
| `-C DIR` | Run as if devflow was started in `DIR`. |

Every document has `"version"` (currently `1`) and `"command"`. With `--json` anywhere in the arguments before a `--`, every failure prints `{"version": 1, "command": …, "error": "…"}` and exits 1: a missing argument, a bad `-C` directory, a configuration problem and a refused operation alike. `command` is the subcommand that was asked for (`devflow` when none could be identified). When devflow knows the next step, the document also has `"hint"` (for example `run `devflow init` …` when there is no `devflow.toml`, or `devflow remove NAME --force` when only `--force` can override a protection). `--help` and `--version` still print their text.

| Exit code | Meaning |
| --- | --- |
| 0 | Done; for `switch` and `setup`, the workspace is `ready` |
| 1 | Error: usage, configuration, an unreachable server, a failed protection check. The message is in `error`. This includes a phase that fails after the worktree was created (for example an unreachable server during `switch -c`): the worktree is kept and named in the message; re-run `switch -c NAME` or `devflow setup` there. `doctor` also exits 1 when a check fails, but prints its normal document with `"ok": false` and no `error`. |
| 2 | The workspace is not ready. The worktree exists, but `status` is `needs_attention` (a required phase was skipped or failed) or `in_progress` (another process holds the receipt lock, or a crash left a phase started). The reason is in the document and in the receipt. When `switch` or `setup` finds the lock held, the document has `"status": "in_progress"` and a `detail` message instead of an `error`; wait and run the command again. |

`switch` and `setup` documents carry `project`, `created`, `skipped`, `status` (`ready`, `needs_attention` or `in_progress`), `path`, `workspace` (`name`, `branch`, `path`, …), the `receipt` with each phase's state and detail, `payload`, `hooks` and `warnings`. `switch` into an existing workspace reports the switch hooks it ran in `hooks`. `switch -c` also runs the project's `switch` hooks once the new workspace is `ready` (the same as every later `switch` into it) and reports them as `switch_hooks`; a failing or skipped switch hook is a warning. `setup --if-new` on a `ready` workspace returns only `{project, skipped: true, workspace, path}`, with no `status` or `receipt`. Passwords are replaced by `***` in every document and message except `devflow db url` and `devflow db branch create`. `db psql`, `shell-init`, `completions` and `gui` do not accept `--json`.

## Start a task

```sh
devflow --json --non-interactive switch -c agent/TASK > switch.json
status=$?                                   # 0 ready, 2 needs_attention, 1 error
path=$(jq -r '.workspace.path' switch.json)       # same value as .path
jq -r '.status' switch.json                       # ready | needs_attention | in_progress
```

1. Read `workspace.path` from the output and run every later command with that directory as the working directory. The worktree is outside the main checkout, under `worktree.dir` from `devflow.toml` (by default `../<repo>.worktrees/`), in a directory named after the sanitized workspace (`agent/a` becomes `agent-a`); `workspace.path` is authoritative, so do not build the path yourself.
2. The environment is in `<path>/.devflow/env`: `DATABASE_URL`, the `PG*` variables and `DEVFLOW_*` identifiers. `DATABASE_URL` and `PG*` are absent when the project has no `[postgres]` or the workspace was created with `--no-db`. Load it (`set -a; . .devflow/env; set +a`), or run through mise when the project's `mise.toml` has `[env] _.file = ".devflow/env"`. The directory ignores itself; never commit it.
3. Choose where code and data come from with `--from WS` (code; needs `-c`) and `--data-from WS|snapshot:NAME[@vN]|branch:ID|empty` (data). Both are fixed once chosen. `--data-from` defaults to the code source's database, or `empty` for the default workspace. If a local branch NAME already exists, `switch -c` checks it out and `--from` only sets the payload and data source.
4. `--no-db` and `--no-hooks` skip those phases, and the skip is recorded permanently: a later `devflow setup` leaves them skipped and the workspace stays `ready`. Only `setup --new-instance` runs the database, env and hooks phases again.
5. If the exit code is 2, read the reason from the document, fix it (usually trust, see below) and run `devflow --json --non-interactive setup` inside the worktree. It resumes at the first incomplete phase.

Work inside the worktree as usual: commit with Git, run tests, start daemons with pitchfork. Do not edit `.devflow/` or the receipt in the worktree's Git directory.

## Finish a task

Remove from the **main checkout**, not from inside the worktree: devflow refuses to remove the default workspace and the workspace that contains the caller's directory, and `--force` does not override either.

```sh
cd "$MAIN_CHECKOUT"
devflow --json --non-interactive remove agent/TASK --force
```

`--force` accepts a dirty or locked worktree and a failing `teardown` hook, and it skips untrusted `teardown` hooks with a warning, so a cleanup script that passes `--force` does not run them. Without it, a protection (dirty, locked, untrusted teardown hooks) stops the removal before anything changes; a failing `teardown` hook can only be detected after it ran, and it then stops the removal before the database branch or the worktree is touched. `--keep-data` leaves the database branch in place; `--delete-branch` also deletes the Git branch, and without `--force` it requires that branch to be merged into the default workspace's HEAD. Merge or push what you want to keep first.

## Trust in automation

`[hooks]` commands are arbitrary shell, so devflow runs them only for content a person has approved. Approval is the SHA-256 of the `[hooks]` section as merged with `devflow.local.toml`, stored per project checkout (project id and main-checkout path); a changed section needs approval again. It does not cover scripts the commands invoke.

| Situation | What happens |
| --- | --- |
| Hooks trusted | `setup` commands run in order in the worktree with the env applied |
| Setup hooks untrusted, and no prompt is possible (`--non-interactive`, `--json`, or stdin or stderr is not a terminal) | Hooks are skipped; the workspace is `needs_attention` and the worktree is kept |
| `switch` hooks untrusted, workspace `ready` | Not run; a warning is added and the switch succeeds |
| `teardown` hooks untrusted | `remove` is refused; `--force` skips them with a warning |
| `DEVFLOW_TRUST_HOOKS=1` | Trusts for that one run. Use it in CI, or for repositories whose hooks you have read. Do not set it for code you did not review. |

The usual setup is a one-time `devflow trust` by a person, which then covers every agent task in that project until `[hooks]` changes. `devflow --json trust` shows everything one approval covers (`scope`: the `setup`, `switch` and `teardown` commands, the `timeout` and the `digest`) and the trust state without approving anything; `devflow trust -y` (`--yes`) approves exactly that scope without asking and prints it, so run it only after reading it. `devflow trust --revoke` withdraws approval.

## Worktrees created by other tools

Install the Git hook once per repository: `devflow hook install`. It adds a `post-checkout` hook (respecting `core.hooksPath`) that runs `devflow setup --if-new` when a new branch is checked out into an empty worktree. That covers `git worktree add` and tools such as T3 Code and Claude Code that use the git CLI. Git does not copy `.git/hooks` on clone, so a fresh clone gets the hook only through `core.hooksPath` or a template directory.

- It does nothing when the worktree's receipt is `ready`, and resumes any other receipt.
- It does nothing unless `devflow` is on the `PATH` of the process that runs Git, which matters for GUI tools started outside your shell.
- `hook install` refuses to overwrite a `post-checkout` hook devflow did not write, and prints a one-line snippet to add to it instead.
- It always exits 0 so Git does not report the worktree as failed, and it stays silent when another devflow process is already setting the worktree up. Check `devflow --json list` for `needs_attention`, or read the setup logs under the worktree's Git directory (`devflow/logs/`).
- `git worktree add --orphan`, `--no-checkout` and libgit2-based tools skip the hook. Run `devflow setup` in those worktrees.
- Adopted worktrees take the default workspace as code and data source.
- `DEVFLOW_SETUP_ACTIVE=1` prevents re-entry from inside hooks.

## Sandboxes and forks

For VM sandboxes and forks, keep the admin credentials on the trusted side and give the sandbox only a branch URL: `devflow setup --database-url URL` attaches it, and `devflow setup --new-instance` (or an orchestrator-set `DEVFLOW_INSTANCE_ID`, honoured only by an explicit `devflow setup`) gives a forked worktree a fresh identity. Setup, the secrets a sandbox must not receive, and cleanup are in [docs/REMOTE.md](docs/REMOTE.md).

## Useful commands

| Task | Command |
| --- | --- |
| List workspaces with their status (`workspaces[]` of `{workspace, receipt, status, busy}`; `--all` lists every registered project as `projects[]`). It reads local receipts and never contacts the server: the database name, role and source are in `receipt.data` (URL password masked) | `devflow --json list` |
| Path of a workspace | `devflow --json path NAME` |
| Connection URL (prints the password) | `devflow --json db url [WS]` |
| Restore a workspace's data to its source (immediately: there is no confirmation prompt) | `devflow --json db reset [WS] [--to SRC]` |
| Save the current data as a reusable seed (NAME: 1-100 characters, no `@`, whitespace or control characters) | `devflow --json db snapshot create NAME [--from WS]` |
| Finish or roll back interrupted database operations | `devflow --json db reconcile` |
| Check Docker, the server, CoW capability (measured or declared), trust | `devflow --json doctor` |

When the PostgreSQL server is unreachable, `list`, `path`, `db url`, `switch` to a ready workspace and `remove --keep-data` do not need it and still work; operations that provision or drop data fail with an error.

## Environment variables

| Variable | Use |
| --- | --- |
| `DEVFLOW_TRUST_HOOKS=1` (or `true`) | Trust the `[hooks]` section for this run |
| `DEVFLOW_NON_INTERACTIVE=1` | Same as `--non-interactive` |
| `DEVFLOW_INSTANCE_ID` | Orchestrator override: the instance identity a forked worktree takes. Must be a UUID (anything else is ignored) and is refused if another worktree of the project already records it. Honoured only by an explicit `devflow setup` |
| `DEVFLOW_PG_ADMIN_URL` | Admin URL for external mode (trusted machines only) |
| `DEVFLOW_CONFIG_DIR`, `DEVFLOW_DATA_DIR` | Redirect the registry, trust and managed-server directories (useful for isolated tests) |
| `DEVFLOW_SETUP_ACTIVE` | Set to `1` by devflow inside hook commands; the `post-checkout` hook does nothing when it is `1`, so Git commands in a hook cannot start another setup |

Hook commands run with `sh -c` in the worktree with stdin closed, and also receive `DEVFLOW_PROJECT`, `DEVFLOW_PROJECT_ROOT`, `DEVFLOW_WORKSPACE`, `DEVFLOW_WORKSPACE_PATH`, `DEVFLOW_SOURCE_PATH` (absent for the default workspace), `DEVFLOW_WORKSPACE_ID` (the workspace's instance id; informational, also in `.devflow/env`) and `DEVFLOW_PHASE` (`setup`, `switch` or `teardown`). Git's hook context (`GIT_DIR`, `GIT_WORK_TREE`, …) is removed, and when the workspace has a database so are inherited libpq variables such as `PGHOST`, so `psql` in a hook reaches the workspace's own database. Hooks run at least once, so write them to be idempotent. There is no templating.

[`examples/agent-task.sh`](examples/agent-task.sh) implements this flow.
