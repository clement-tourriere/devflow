# GUI Pitchfork Process Control Plan

## Current state

Devflow already has first-class GUI control for configured `processes.daemons`:

- Tauri commands: `list_processes`, `start_processes`, `stop_processes`, `restart_processes`, `get_process_logs` in `src-tauri/src/commands/processes.rs`.
- React project detail view: a **Processes** card in `ui/src/pages/projects/ProjectDetail.tsx` with workspace selection, status table, start/stop/restart actions, log modal, ports, and proxy URL display.
- Core runtime dispatch: `devflow_core::processes::runtime_for_config()` uses `processes.provider`; when it is `pitchfork`, the same GUI buttons control Pitchfork-backed processes through the embedded `PitchforkRuntime`.

So the GUI can already pilot Pitchfork-managed processes for a workspace. What is missing is richer Pitchfork-specific UX and easy editing of process definitions.

## Pitchfork upstream capabilities to account for

Pitchfork ships its own:

- TUI (`pitchfork tui`): dashboard, live status, CPU/memory, search, batch operations, config editor, and real-time logs.
- Web UI/API: disabled by default; can be started on a loopback port, exposes `/api/daemons`, `/api/daemons/{id}/start|stop|restart|enable|disable`, SSE log tailing, process tree, namespaces, proxies, and config editing.

Devflow currently embeds Pitchfork for process supervision, but devflow does not start or embed Pitchfork's Web UI/TUI. Devflow also keeps its own process records as the stable integration boundary for workspace identity, proxy URLs, GUI status, and cleanup.

## Goals

1. Keep devflow's GUI as the primary, workspace-aware control plane.
2. Show clearly when a process is backed by Pitchfork and expose Pitchfork identifiers/diagnostics.
3. Add easy, safe editing for `processes.provider` and `processes.daemons` without forcing users into raw YAML.
4. Optionally bridge to Pitchfork's own Web UI/TUI for advanced Pitchfork-native views without making it required.
5. Preserve devflow semantics: service URL templating, worktree-aware working directories, hook approvals, daemon desired state, proxy records, and workspace cleanup.

## Non-goals

- Do not replace devflow process config with `pitchfork.toml` as the source of truth.
- Do not require the external `pitchfork` CLI for normal devflow GUI process control.
- Do not expose Pitchfork's Web UI on non-loopback addresses from devflow.
- Do not let Pitchfork config edits silently diverge from `.devflow.yml` process definitions.

## Phase 1 — Make existing control obvious

Status: partially implemented.

- Add `runtime` and `pitchfork_id` to `ProcessStatus` returned by core/Tauri.
- Show a `pitchfork` badge in the GUI process table.
- Keep existing start/stop/restart/log flows as the default controls.
- Update GUI docs to mention the Processes card and its Pitchfork support.

Validation:

- Configure `processes.provider: pitchfork`.
- Start a process from the GUI.
- Confirm the row shows `pitchfork`, PID, resolved port, proxy URL, and logs.
- Stop/restart from the GUI and verify state changes.

## Phase 2 — Process definition editor

Add a dedicated **Processes** section to the config editor.

Fields:

- Top-level: `provider` (`native`/`pitchfork`), `auto_start`, `auto_stop`.
- Per daemon: `name`, `run`, `dir`, `required`, `depends`, `port.expect`, `port.bump`, readiness (`ready_port`, `ready_http`, `ready_cmd`, `ready_output`, `ready_delay`, `ready_timeout`), `watch`, `retry`, `stop_timeout`, `shutdown_signal`, and `env` key/value templates.

UX requirements:

- Add/edit/delete daemon forms.
- Duplicate daemon action for worker variants.
- Inline MiniJinja preview for env values using the selected workspace.
- Validation before save: unique names, non-empty `run`, valid ports, valid dependency names, no self-dependency, valid provider.
- Preserve raw YAML mode for advanced fields and comments caveat.

Implementation notes:

- Extend `ui/src/types/config.ts` with `ProcessesConfig`, `ProcessDaemonConfig`, `ProcessPortConfig`.
- Add `ui/src/pages/config/sections/ProcessesSection.tsx`.
- Add a `processes` tab in `ConfigEditor.tsx`.
- Prefer reusable small inputs over one large monolithic form.

## Phase 3 — Better live process telemetry

Improve the Processes card beyond current status/log snapshots.

Features:

- Poll or subscribe while the project page is open.
- Show runtime, desired state, started time/uptime, retry count, last error.
- Add `Force restart`/`Start with force` for stuck Pitchfork records.
- Add process tree if available from Pitchfork Web API or a devflow-core helper.
- Add CPU/memory if available without making the GUI depend on Pitchfork internals.
- Add batch selection, mirroring the Pitchfork TUI's multi-select operations.

Implementation options:

- Devflow-owned path: extend `ProcessStatus` from devflow records plus `sysinfo`/process inspection.
- Pitchfork API path: when provider is `pitchfork` and the API is running on loopback, enrich rows by matching `pitchfork_id` to `/api/daemons`.

Prefer devflow-owned telemetry first; use Pitchfork API enrichment opportunistically.

## Reconciliation policy — devflow config vs Pitchfork config

Yes, devflow can reconcile the two worlds, but it should do it through explicit policy modes rather than silently treating two config files as equal.

Recommended policy modes:

```yaml
processes:
  provider: pitchfork
  pitchfork:
    config_policy: devflow-owned   # devflow-owned | import | mirror | merge
    external_daemons: show         # hide | show | importable
    web_ui:
      enabled: false
      edit_mode: warn             # readonly | warn | merge
```

### `devflow-owned` (default)

`.devflow.yml` is the source of truth. Devflow ignores `pitchfork.toml` for devflow-managed process namespaces and only uses Pitchfork as a supervisor/log runtime.

Behavior:

- Devflow-generated Pitchfork IDs/namespaces are tagged as devflow-managed.
- External Pitchfork daemons can be shown in a separate "External Pitchfork" group, but they are not cleaned up with devflow workspaces unless imported.
- Pitchfork Web UI/TUI can still stop/restart active daemons, but config edits are treated as external drift.

This is safest and should remain the default.

### `import`

One-way migration from `pitchfork.toml` into `.devflow.yml`.

Behavior:

- GUI reads Pitchfork daemon config and offers "Import as devflow process".
- The imported process becomes a `processes.daemons.<name>` entry.
- After import, `.devflow.yml` owns it.

This is useful for people already using Pitchfork before adopting devflow.

### `mirror`

One-way generated Pitchfork config from `.devflow.yml`.

Behavior:

- Devflow renders a generated Pitchfork config file for the selected workspace/project.
- The file has a strong header and hash, e.g. "generated by devflow; edit `.devflow.yml` instead".
- Pitchfork's own TUI/Web UI can discover available daemons more naturally.
- Manual edits to the generated file are detected as drift and overwritten only after warning/confirmation.

This gives Pitchfork-native UI visibility while avoiding bidirectional merge ambiguity.

### `merge` (advanced/experimental)

Two-way reconciliation with explicit diffs and conflicts.

Behavior:

- Devflow generates the expected Pitchfork config from `.devflow.yml` and stores a hash.
- If Pitchfork config changed, GUI computes a three-way diff: previous generated config, current generated config, current Pitchfork config.
- Simple fields can be pulled back into `.devflow.yml`: `run`, `dir`, `port`, readiness, `watch`, `retry`, stop signal/timeout.
- Template-sensitive fields are conflict-prone and require user choice: env values containing rendered service URLs, workspace-specific workdirs, proxy slugs, IDs/namespaces.
- No automatic merge for devflow service templates like `{{ service['app-db'].url }}` unless the edited value still matches a reversible template.

This should be opt-in because Pitchfork config is usually workspace/rendered state while devflow config is project/template state.

### Conflict rules

- Devflow-managed namespace + `.devflow.yml` changed: devflow wins by default; offer "re-render Pitchfork config".
- Devflow-managed namespace + Pitchfork config changed: show drift; offer "import selected changes" or "discard Pitchfork edits".
- Both changed: require manual diff review.
- External Pitchfork daemon: show separately; offer import/adopt, never delete during devflow cleanup by default.

## Phase 4 — Optional Pitchfork Web UI bridge

Add an **Open Pitchfork Web UI** affordance, but keep it optional.

Design:

- Detect whether Pitchfork Web UI/API is already listening on configured loopback ports.
- If not running, offer either:
  - instructions to run `pitchfork supervisor start --force` with web settings, or
  - a devflow-managed "start Pitchfork Web UI" action if the embedded/public API can safely start it.
- Open the URL in the system browser, or embed it in a Tauri webview only after confirming security and route isolation.

Important constraints:

- Pitchfork Web UI is owned by Pitchfork's supervisor process and reads its settings at supervisor startup.
- Devflow's embedded Pitchfork runtime does not currently make the Web UI part of the devflow controller contract.
- If Pitchfork Web UI config editing is enabled, use the reconciliation policy above. Default to `devflow-owned` + `warn`, not silent merge.
- Loopback only by default; if non-loopback is ever supported, require token handling and explicit user confirmation.

## Phase 5 — Optional Pitchfork TUI launcher

Add a convenience launcher for users who prefer Pitchfork's terminal dashboard.

Options:

- Open `pitchfork tui` in the configured terminal app when the external CLI is installed.
- Or document the exact command and namespace mapping from the selected devflow workspace.

Caveat:

- Devflow-generated Pitchfork daemon IDs use devflow's project hash/workspace namespace. The TUI will show those IDs; the GUI should display/copy `pitchfork_id` so users can correlate rows.

## Risks and mitigations

- **State divergence**: Pitchfork Web UI/TUI may edit Pitchfork config while devflow uses `.devflow.yml`. Mitigate with source-of-truth warnings and avoid bidirectional config sync until explicitly designed.
- **Two supervisors**: Starting Pitchfork's standalone supervisor while devflow embeds Pitchfork can create confusion. Mitigate by detecting existing Pitchfork supervisor/Web UI and documenting the boundary.
- **Security**: Web UI/API can control local processes. Bind to loopback by default; never auto-expose to LAN.
- **API instability**: Some Pitchfork internals are not public reusable core APIs yet. Keep devflow's existing process state as the durable boundary.
- **Long-running GUI actions**: Start/readiness can take time. Keep actions cancellable or show progress/toasts.

## Suggested first implementation slice

1. Finish Phase 1 and tests.
2. Add `ProcessesSection` to the config editor for top-level provider/auto-start and a minimal daemon form (`name`, `run`, `port`, `env`, `required`).
3. Add polling refresh to the Processes card while visible.
4. Defer Pitchfork Web UI/TUI launching to a separate, explicit feature flag or settings item.
