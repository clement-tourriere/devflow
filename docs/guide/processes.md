# Processes

devflow does not supervise anything: [pitchfork](https://pitchfork.jdx.dev) runs a workspace's daemons and [mise](https://mise.jdx.dev) delivers toolchains. devflow decides whether it may *ask* pitchfork to start or stop a workspace's daemons, hands the daemons the workspace's environment, and reports what pitchfork says afterwards. Resting idle workspaces and forwarding one stable port are devflow's own, and both are opt-in.

- [Composing with pitchfork and mise](#composing-with-pitchfork-and-mise)
- [Starting and stopping, with your approval](#starting-and-stopping-with-your-approval)
- [Supported releases](#supported-releases)
- [Resting workspaces, and one stable address](#resting-workspaces-and-one-stable-address)

## Composing with pitchfork and mise

devflow's job ends at `.devflow/env`. Let mise load it for the shells and editors you use in the worktree:

```toml
# mise.toml
[tools]
bun = "1.2"

[env]
_.file = ".devflow/env"
```

Declare daemons in a committed `pitchfork.toml`. Run each command through `devflow exec`, which applies `.devflow/env` last, after mise (`mise = true`, for the toolchain) and anything else has set its own environment:

```toml
# pitchfork.toml
[daemons.api]
run = "devflow exec -- bun run dev"
mise = true
port = { expect = [3000], bump = true }
```

The daemon sees `DATABASE_URL` of the worktree it runs in, however mise is configured. devflow never puts the environment or a credential into a pitchfork file: pitchfork keeps daemon environments in its state file, so the file is read by `devflow exec` at the moment the command starts. `devflow` has to be on the `PATH` of the process that starts the daemons.

### Starting and stopping, with your approval

```sh
devflow pitchfork status              # installed? version, features, supervisor, proxy; what to run if not
devflow up                            # pitchfork start of this workspace's own daemons (by full id); shows what pitchfork reports afterwards
devflow ps [--all]                    # daemon, state, port, URL
devflow down                          # pitchfork stop of this workspace's running daemons (by full id)
```

- Each worktree gets a machine-local `pitchfork.local.toml` with only a unique `namespace` (`df-<project8>-<instance8>`) and a `worktree_label`, excluded through the repository's `info/exclude`, so daemons of different worktrees and repositories never collide. A `namespace` declared in the committed `pitchfork.toml` conflicts (pitchfork refuses disagreeing sibling files): devflow says so, writes nothing and will not start or stop through it.
- A workspace acts only through the namespace devflow assigned it. If its pitchfork files name another (say, another worktree's), `devflow up`, `down` and `remove` refuse ("the pitchfork namespace does not belong to this workspace"), `devflow ps` shows none of that namespace's daemons, and the edit drops the approval; `up` restores devflow's own file when it is merely stale.
- devflow starts and stops exactly the daemons of the workspace, by full `namespace/name` id, and never with `--local`: pitchfork's `--local` also selects the daemons of every parent directory's configuration. A daemon that `depends` on one outside the workspace's own files is refused, since pitchfork would start that one too.
- devflow starts daemons only for a `ready` workspace whose own `pitchfork.toml` is the one `devflow trust` approved, and never in bulk while a daemon starts on its own (`cron`, `watch`, `boot_start`, `auto`: a supervisor started anywhere in the repository would run those without devflow). Plain daemons can still be started by name, `devflow up --daemon api`. It stops daemons only while the file still matches the approval, because pitchfork runs lifecycle hooks (`on_stop`, …) from the current file; `devflow down --force` stops anyway. Readiness, namespace and approval are checked again right before pitchfork is run, so a file edited while devflow was asking pitchfork what runs is refused.
- It needs pitchfork 2.20.0 or later (`devflow pitchfork status` names the release it found and the one it needs). If pitchfork's state cannot be read, devflow does not assume that nothing runs: `down` and `remove` refuse, and `--force` goes on without knowing.
- `[pitchfork] autostart = true` in `devflow.toml` (`devflow init` proposes it, off by default) starts the daemons after `setup` and `switch` reach `ready`. Whether they started is reported next to the workspace's status, never as part of it.
- `devflow remove` stops the workspace's running daemons first, and goes on only once it has seen them stopped (`--force` otherwise). A supervisor that is down does not count as "nothing runs": if devflow started daemons here (the receipt records it) `down` and `remove` are refused until you run `pitchfork supervisor start` and then `devflow down` (or pass `--force`, which leaves them running and says so); if it started none, they go on with a warning. With no pitchfork on `PATH` at all `remove` goes on, but warns ("pitchfork isn't on PATH, so devflow couldn't check this workspace's daemons") when the workspace has devflow's namespace file or a recorded start: any daemon that is still running was not stopped.
- Installing pitchfork and turning on its proxy are offers. `devflow pitchfork install` runs `mise use -g pitchfork@latest` after you confirm (`--yes`), or prints brew and cargo instructions. The proxy (`pitchfork proxy setup` edits system DNS and trust and uses sudo) is only ever printed by `devflow pitchfork proxy` and `devflow doctor`; devflow never runs those commands.
- The desktop app's Daemons tab shows the same state with Start, Stop and Restart buttons gated by the same checks, a clickable URL for each daemon with a port, an **Install pitchfork** click that shows the command first (only when mise is there to run it), and copy-only commands for the proxy.

Ready checks, retries, schedules and proxy settings are pitchfork options; see its documentation.

## Supported releases

pitchfork and mise are optional and have an oldest supported release each, checked by `devflow doctor` (a release that is older, or whose version cannot be read, is reported with the page to upgrade it) and by the gates that rely on them: **pitchfork 2.20.0** (`devflow up`, `down` and `[pitchfork] autostart` refuse an older one), **mise 2026.4.18** (setup, switch and teardown hooks that run mise are not run with an older one), and, for the native checkout only, **Git 2.19.0**. The numbers are the releases of the options devflow passes, not a guess about what is recent; they are pinned in one place (`crates/devflow-core/src/tools.rs`).

## Resting workspaces, and one stable address

Both are opt-in and per user. devflow never suspends anything by itself and never keeps an "active workspace": parallel agents work in other worktrees, so only a person (or an agent) asking decides what rests.

- `devflow suspend [NAME]` stops the workspace's own containers (Valkey, Redis, MySQL: volumes kept) and its own pitchfork daemons, and records `suspended_at` in the receipt. The workspace's process lock is held from the first look to the record, so nothing starts its daemons, enters it or pins it in between, and a daemon pitchfork runs under the workspace's namespace is found even when its definition was removed and devflow never recorded starting it. The daemons go through the same gated stop as `devflow down`, with the same approval rules (a pitchfork configuration that changed since it was approved is refused, with the way to review it), never a raw kill; a state devflow cannot read is unknown, not stopped, and then neither the receipt nor the containers the daemons may be using are touched. The project's shared PostgreSQL server is never stopped. `devflow switch NAME` (or `devflow resume NAME`, the same activation without the change of directory) resumes the workspace: the containers start again with their data, the switch hooks run and `[pitchfork] autostart` starts its daemons; daemons that are not autostarted stay stopped until `devflow up`. Starting a suspended workspace's daemons or containers by hand (`devflow up`, the app) ends the rest too.
- `devflow suspend --idle 3d [--yes]` suspends every unpinned workspace not used for that long. Last use is the later of the last activation and the last `devflow exec` in the workspace (which touches `.devflow/active`: one `stat`, and at most one empty write every 30 seconds). The main checkout, the workspace you are in, a workspace already suspended and any whose receipt cannot be read are never selected; the selection is listed and confirmed first, and judged again under the workspace's locks right before anything stops (a workspace pinned, entered, used by `devflow exec` or replaced meanwhile is left alone and said so) (`--yes` skips the question and never widens the selection; without a terminal it is refused). `devflow pin NAME` keeps a workspace out of it, `devflow unpin NAME` lets it back in. `devflow list --wide` marks suspended and pinned workspaces, and so does the app, which also has Suspend, Resume and Pin buttons.
- `devflow focus NAME [--port PORT[=DAEMON]]…` forwards canonical loopback ports to the workspace in use: `--port 3000` reaches its `DEVFLOW_PORT`, `--port 8080=web` the port of its daemon `web`. Without `--port` it re-points the ports already forwarded, else the project's `[worktree] focus_ports = [3000]`. A small devflow process (one per project, started by `focus`, state, pid and log in the data directory under `focus/`) listens on `127.0.0.1` only, forwards to `127.0.0.1` only, and only to the ports it was told about. Moving the focus keeps the listening sockets; connections to the previous workspace are closed, so a client reconnects to the new one. `devflow focus` shows where it points and whether the target answers, `devflow focus --stop` ends it, a port already in use is an error that names it, and `switch` never moves it. While it runs the forwarder looks again, every couple of seconds, at the workspace it forwards to: when the project, the workspace's folder or its association with its Git directory is gone (a host that deletes only the worktree directory ends the focus too), its receipt names another instance (`setup --new-instance` also ends the focus before it replaces the identity), its port is no longer the one in the receipt or no longer reserved for it in the machine's port ledger, it stops by itself (`devflow focus` says why), because a port that went back to the pool can be given to another workspace. A forward to a daemon's port (`--port 8080=web`) is bound to the process that listened when it was made, so the daemon must be running: when pitchfork says it was restarted, stopped, gone or listens elsewhere, that forward ends (the others go on). The look at pitchfork is slower than the look at the files. A receipt, ledger or listing it cannot read, a daemon state devflow does not recognize, or a daemon missing from a listing that had entries pitchfork could not read, is not a reason to stop. A running forwarder whose state file is missing or unreadable refuses a removal's release (nothing is known about where it forwards), and the removal can be tried again. Publishing a focus waits behind the workspace's process lock, like a removal or a new identity, so none of them can interleave. Removing the workspace it points at (`remove`, a forced remove, or a clean-up of one a host removed) ends the focus with it, matched by project and workspace instance and reported in the result, so a port that is given to another workspace is never reached by a forward left behind. The focus ends before anything is deleted, after the workspace's record says it is being removed (a focus cannot be published for it meanwhile); a forwarder that cannot be seen to stop (devflow sends it no signal) refuses the removal, forced or not, and the port stays reserved. It is TCP only, which suits what it is for: an OAuth callback registered for `http://localhost:3000/…` and cookies scoped to `localhost:3000`. A PostgreSQL database name travels inside the protocol, so it cannot be re-pointed by a forward: connect to a workspace's database through its own `DATABASE_URL`.
