# Architecture and compatibility

The core application layer owns lifecycle policy. CLI/TUI and desktop commands collect typed input and render outcomes. Provider adapters implement resource operations; they do not decide retention policy.

- `application`: service operations, seed, retention plans/apply, and operation recovery.
- `workspace`: VCS materialization, lifecycle hooks, processes, inventory, and durable operations.
- `state`: stable project/workspace/service/resource identities, desired state, data bindings, pins, and baseline references. Provider SQLite/files contain backend observations and exact ownership records.
- `baseline`: local PostgreSQL prepare, seal, version binding, and dependency-aware deletion.
- `execution`: explicit program/argv or shell requests, separate durable execution records and verified runner ownership.
- `controller`: one owner lease for foreground, detached and one-shot reconciliation.
- `services::descriptor`: versioned supported-provider/build manifest; runtime operation capabilities are negotiated for plugins.
- `companion`: optional proxy executable and loopback API. The desktop does not embed proxy networking.

Existing service keys and resource namespaces are retained. `devflow project relocate OLD NEW` explicitly updates a registered path without renaming resources. The desktop project list comes from the core registry; hiding a project does not delete its data or ownership records.

Operations persist intent before external effects, checkpoint typed steps and per-service outcomes, and keep the most recent 100 operation-history records per project. Unfinished active records are retained separately. `devflow operation inspect ID` shows the saved request and checkpoints; `resume ID` supports switch, remove, service creation/deletion/destruction, service lifecycle operations, and cleanup. `cancel ID` only discards failures proven to have made no resource changes. SQL imports and baseline preparation require inspecting partial effects and retrying their original explicit command or replacing the failed generation. Shell hooks must tolerate retries; devflow makes no exactly-once guarantee.

Required hooks run to completion during lifecycle setup, including hooks previously marked `background: true`. `required: false` opts out of readiness gating. Optional background hooks write completion receipts under the config directory's `hook-runs` directory. A process killed before it can update a receipt may leave a running receipt; its PID and lifecycle receipt ID provide the recovery context.

## Retired surface

| Old interface | Replacement |
| --- | --- |
| `agent.auto_context` | Explicit `agent context`; old settings are accepted with a migration warning and omitted on save. |
| `worktree.respect_gitignore` / `--no-respect-gitignore` | `worktree.copy_ignored` / `--copy-ignored`; the old CLI spelling remains an alias. |
| `template_from_time` | No advertised timestamp-clone capability; use an exact baseline version. |
| `sync-ai-configs` and its hook action | Generated guides and explicit file allowlists. Old configured actions are removed with a warning. No permission arrays are merged. |
| Commit settings and commit command | Use the VCS and preferred editor/agent directly; no embedded commit helper. |
| Embedded Pitchfork and GUI web bridge | Native processes; a narrow adapter stops exact legacy external-daemon records. |
| Duplicate structured config/recipe GUI | Source-format-preserving raw editor and hook inspection. |
| Desktop-owned proxy | Optional `devflow-proxy` companion, shared with CLI clients. |

Cloud providers remain optional/experimental; Jujutsu remains behind the existing VCS adapter. Breadth expansion is paused until platform/live-service contract coverage establishes the required behavior. Ownership migrations are deliberately retained.

Provider types and the reference catalog are generated with `cargo run -p devflow-core --example generate_provider_manifest` directly from the core descriptors; CI checks them for drift. SQL baseline bundles and prepared-clone pools remain separate future extensions.


PostgreSQL directory copying and CoW probing share `devflow_core::fs_copy`, backed by native `reflink-copy`, `filetime`, and filesystem calls. Data replacement stages a complete copy before publishing it; a failed local PostgreSQL copy cannot remove the live directory. Git fixtures and worktree management use `git2`, Docker fixtures use Bollard, and provider contracts are generated directly from the core catalog.

The process boundary test permits only explicit user commands, hooks, managed processes, terminals, sessions, companion lifecycle, and configured executable providers. Git and jj workspace materialization and payload copying are tested with an empty executable PATH. `capabilities.process_boundaries` exposes these boundaries to automation.

Jujutsu uses pinned `jj-lib` 0.45.1 for repository transactions, snapshots, bookmarks, and workspace registration. Its native adapter respects ignored files, snapshot tracking rules, immutable history, and colocated Git imports/exports. Devflow-created working-copy commits are unsigned and do not invoke configured signing or filesystem-monitor programs; use jj directly for signed commits. Inspection preserves Git staging and active merges/rebases. Explicit preparation recovers stale working copies through jj operation/tree merging, preserving local edits and conflicts. Git and jj directory cleanup keep identity-checked receipts until deletion and workspace deregistration complete.

ZFS storage loads the system OpenZFS 2.x `libzfs` ABI at runtime on Linux. It requires an already loaded device, disables helper-backed mounting and requires unshared leaf datasets for both snapshot sources and replacement targets. No development headers or ZFS runtime are required for other storage drivers. A narrow binding owns native handles on blocking workers and the replacement journal operates on typed dataset mutations. Failed dataset destruction stops filesystem cleanup. Linux CI exercises real replacement failures with an empty executable PATH.

Run PostgreSQL measurements with `cargo run -p devflow-core --example benchmark_data -- --sizes-mib 1,16,64 --concurrency 1,4 --output benchmark.json`. This uses disposable resources and measures core API calls, not CLI startup or per-invocation peak RSS. Allocated blocks are not physical bytes copied for CoW filesystems; TCP availability samples are not SQL latency.

`workspace::plan` resolves code revision, exact baseline references, worktree path and setup requirements before effects; switch stores and consumes that plan. Retries preserve the selected code/data origins and creation path while recording repaired setup configuration. Artifact reuse fingerprints the destination checkout and only publishes artifacts from a source with matching inputs. Missing or incompatible inputs produce a cache miss.

Workspace control separates stable logical identity from creation generation. Deleting code and data clears the binding and rotates the generation; retaining services retains their origin and baseline protection. Local PostgreSQL stores an immutable image ID per generation, inherits it through ordinary descendants, and validates existing container image, environment, ownership, mounts and ports before reuse.

The desktop submits core service mutation requests, offers sealed baseline selection, and displays pending operation inspection/resume/cancellation. Its config editor uses source text with validation and concurrent-edit protection; the unused parsed-config IPC and TypeScript schema mirror are removed. The optional proxy has a standalone executable and checksum-verified release/install path.
