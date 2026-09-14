# Architecture and compatibility

The core application layer owns lifecycle policy. CLI/TUI and desktop commands collect typed input and render outcomes. Provider adapters implement resource operations; they do not decide retention policy.

- `application`: service operations, seed, retention plans/apply, and operation recovery.
- `workspace`: VCS materialization, lifecycle hooks, processes, inventory, and durable operations.
- `state`: stable project/workspace/service/resource identities, desired state, data bindings, pins, and baseline references. Provider SQLite/files contain backend observations and exact ownership records.
- `baseline`: local PostgreSQL prepare, seal, version binding, and dependency-aware deletion.
- `execution`: explicit program/argv or shell requests in a ready materialized workspace.
- `services::descriptor`: versioned supported-provider/build manifest; runtime operation capabilities are negotiated for plugins.
- `companion`: optional proxy executable and loopback API. The desktop does not embed proxy networking.

Existing service keys and resource namespaces are retained. `devflow project relocate OLD NEW` explicitly updates a registered path without renaming resources. The desktop project list comes from the core registry; hiding a project does not delete its data or ownership records.

Operations persist intent before external effects, checkpoint typed steps and per-service outcomes, and keep the most recent 100 operation-history records per project. Unfinished active records are retained separately. `devflow operation inspect ID` shows the saved request and checkpoints; `resume ID` supports switch, remove, service lifecycle operations, and cleanup. SQL imports and baseline preparation require inspecting partial effects and retrying their original explicit command or replacing the failed generation. Shell hooks must tolerate retries; devflow makes no exactly-once guarantee.

Required hooks run to completion during lifecycle setup, including hooks previously marked `background: true`. `required: false` opts out of readiness gating. Optional background hooks write completion receipts under the config directory's `hook-runs` directory. A process killed before it can update a receipt may leave a running receipt; its PID and lifecycle receipt ID provide the recovery context.

## Retired surface

| Old interface | Replacement |
| --- | --- |
| `agent.auto_context` | Explicit `agent context`; old settings are accepted with a migration warning and omitted on save. |
| `worktree.respect_gitignore` / `--no-respect-gitignore` | `worktree.copy_ignored` / `--copy-ignored`; the old CLI spelling remains an alias. |
| `template_from_time` | No advertised timestamp-clone capability; use an exact baseline version. |
| `sync-ai-configs` and its hook action | Generated guides and explicit file allowlists. Old configured actions are removed with a warning. No permission arrays are merged. |
| Commit API credentials/model/base URL | `commit.generation.command` or `DEVFLOW_COMMIT_COMMAND`. No embedded LLM API backend. |
| Embedded Pitchfork and GUI web bridge | Native processes; a narrow adapter stops exact legacy external-daemon records. |
| Duplicate structured config/recipe GUI | Source-format-preserving raw editor and hook inspection. |
| Desktop-owned proxy | Optional `devflow-proxy` companion, shared with CLI clients. |

Cloud providers remain optional/experimental; Jujutsu remains behind the existing VCS adapter. Breadth expansion is paused until platform/live-service contract coverage establishes the required behavior. Ownership migrations are deliberately retained.

Provider types and the reference catalog are generated with `scripts/generate-provider-manifest.py` from `devflow --json capabilities`; CI checks them for drift. SQL baseline bundles and prepared-clone pools remain separate future extensions.
