# Pitchfork runtime integration notes

## API verification

`pitchfork-cli` 2.14.0 publishes a Rust library target (`pitchfork_cli`) with enough public API for devflow to embed Pitchfork without shelling out to the `pitchfork` binary:

- `pitchfork_cli::supervisor::SUPERVISOR.run(RunOptions)` starts a daemon in-process.
- `pitchfork_cli::supervisor::SUPERVISOR.stop(&DaemonId)` stops a daemon in-process.
- `pitchfork_cli::daemon::{RunOptions, Daemon}` and `pitchfork_cli::daemon_id::DaemonId` are public.
- `pitchfork_cli::pitchfork_toml::{Dir, PortConfig, PortBump, ReadyHttp, Retry, StopConfig, StopSignal, WatchMode}` are public and reusable for run options.
- `pitchfork_cli::log_store::sqlite::LOG_STORE` plus the public `LogStore` trait allow direct log reads.

The integration therefore uses a real `PitchforkRuntime` for `processes.provider: pitchfork` and does not spawn the Pitchfork CLI.

## Gaps in Pitchfork's current library surface

The public surface is enough for start/stop/logs, but not a perfect reusable core API yet:

- `Supervisor::active_daemons`, `get_daemon`, `remove_daemon`, and `flush_state` are `pub(crate)`, so devflow keeps its own stable process state records for status/proxy/GUI.
- `IpcClient` is not publicly re-exported (`ipc::client` is `pub(crate)`), so external crates cannot use Pitchfork's client abstraction directly.
- `RunOptions` has readiness fields but no per-run `ready_timeout`; devflow preserves its timeout in the native runtime and records failures through its state layer for Pitchfork.
- Pitchfork's global statics are process-wide (`PITCHFORK_STATE_DIR`, `LOG_STORE`, `SUPERVISOR`), which is fine for devflow's controller daemon but is not yet an isolated multi-instance core.

If these gaps become blockers upstream, the smallest reusable `pitchfork-core` surface would expose a supervisor handle with `run`, `stop`, `status/list`, `logs`, explicit state/log paths, and per-run readiness timeout without relying on global process statics. For now, devflow can embed Pitchfork directly and keep devflow-owned desired/actual state as the stable integration boundary for the daemon, proxy, and GUI.
