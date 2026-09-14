# devflow repository review — 2026-09-14

Reviewed commit: `16c15243acd2116ebdd0e67185221bfa98e95e41` (0.7.1).

**The core idea is strong, but data isolation is not reliable enough yet for unattended, concurrent agent workflows.** I would prioritize ownership, successful data restoration, and recoverable workspace operations before adding more providers or interface features.

The most valuable product is a dependable way to turn an existing project's code and reusable data into an isolated, ready-to-use workspace. The CLI, agent integration, and GUI should all expose that same operation.

## Scope and evidence

This review covered the CLI, core workspace lifecycle, Git/Jujutsu abstractions, state and identity, service factories and backends, seeding, hooks, process management, proxy, desktop commands, frontend structure, documentation, and CI/release configuration. Critical paths received deeper inspection and targeted execution; this is not a claim that every code path was executed.

| Validation | Result |
|---|---|
| `cargo test --locked --workspace` | 357 passed; four Docker tests ignored by default |
| Explicit run of all four ignored shared-provider integration tests | Four passed |
| `bun run build` in `ui` | TypeScript and Vite passed; large-chunk warning |
| Five additional service invariant probes | All five failed, demonstrating defects below |
| CLI probes in disposable repositories | Reproduced identity collisions, broken JSON, missing hook summaries, symlink escape, process startup mismatch, and failed worktree recovery |

The service probes used disposable containers and temporary data. The local storage probe exercised the actual generic backend with a Redis fixture engine; it did not run the MySQL or ClickHouse engines. Cloud findings compare source with official API documentation; no cloud account operations were performed. Native desktop interaction, live Jujutsu workflows, filesystem-specific APFS/ZFS/XFS/Btrfs behavior, and concurrent clone stress were not exercised. Full release checks, clippy, and a fresh dependency audit were not run.

Reproduction material:

- [CLI probe source](cli_probes.py) and [captured command results](cli-results.json).
- [Service invariant probe source](service_invariants.rs) and [observed results](service-results.txt).

The service probes are archived outside the regular test suite because they intentionally assert behavior the current implementation violates. This review changes no application implementation.

## Findings, in priority order

P1 means fix before relying on broad unattended use. P2 means a functional or maintenance defect to address in the following stabilization work. “Reproduced” indicates executed evidence; “source” indicates a traced implementation defect or risk without an end-to-end reproduction.

### 1. P1 — Project cleanup can delete another project's databases

**Reproduced with shared PostgreSQL.** Create projects `review` and `review_child` in one shared server, each with workspace `main`. Listing `review` includes `review_child_main`. Destroying `review` drops both databases.

Ownership is inferred from the prefix `review_`, which also matches the neighboring project. Escaping SQL wildcard characters does not fix this ambiguity. Shared ClickHouse and RustFS have analogous prefix-based ownership logic; those cleanup cases were not separately executed.

Source: [shared PostgreSQL inventory and destruction](../../../crates/devflow-core/src/services/shared/mod.rs#L224), [project naming](../../../crates/devflow-core/src/services/shared/naming.rs#L33).

**Change:** use explicit project/service ownership records and stable identifiers. Deletion must match exact owned resources. Existing ambiguous namespaces need an explicit migration decision; silently adopting or deleting everything under a prefix is unsafe.

### 2. P1 — Local MySQL/ClickHouse data directories omit project identity

**Reproduced at the shared local-engine backend.** Two distinct projects with service `db` and workspace `main` mounted the same directory. The default root is global per engine, and the remaining path is only `{service}/{workspace}`. Container names include the project, creating the appearance of isolation while the underlying data is shared.

Deleting a workspace can remove the other project's data. Project destruction is broader still: it removes the entire service directory under the common root.

Source: [root and directory construction](../../../crates/devflow-core/src/services/local_engine.rs#L141), [project destruction](../../../crates/devflow-core/src/services/local_engine.rs#L637).

**Change:** key storage by project, service, and workspace identity, and verify ownership before mounting or deleting it. Add a migration that detects existing overlaps. Sharing a directory must never be an accidental consequence of matching display names.

### 3. P1 — Two named shared services resolve to the same resource

**Reproduced through both the factory and CLI.** A project with shared PostgreSQL services named `primary` and `analytics` returns the same connection URI for both.

The factory passes project identity and shared-container settings to the provider but omits the service name. The same omission appears in shared ClickHouse, Redis, and RustFS construction.

Source: [provider factory](../../../crates/devflow-core/src/services/factory.rs#L306).

**Change:** include service identity in resource ownership and naming. If intentional reuse is supported, make it an explicit reference to another service with defined write and deletion semantics. Reject accidental collisions during configuration validation.

### 4. P1 — Backend normalization collapses distinct workspace keys

**Reproduced.** Core produces distinct keys for `main` and `_main`. Shared PostgreSQL sanitizes both into `review_main`. Both CLI connection requests return that database.

The core's collision-resistant naming is undermined by a second lossy normalization step in the provider. A workspace that looks isolated can therefore modify or remove another workspace's database.

Source: [canonical key generation](../../../crates/devflow-core/src/config/loading.rs#L615), [provider sanitizer](../../../crates/devflow-core/src/services/shared/naming.rs#L12).

**Change:** define a canonical identifier contract that every backend preserves. Derive backend names from the complete identity, with a digest where necessary. Test the composition of core and backend naming, including leading/trailing underscores, repeated separators, case, Unicode, long names, and legacy keys.

### 5. P1 — Invalid PostgreSQL seeds are reported as successful

**Reproduced with a live PostgreSQL container.** A file containing `this is not a PostgreSQL dump` was supplied to the restore helper. The helper returned `Ok(())`.

Restore failures are rejected only when stderr contains selected phrases such as `FATAL` or `could not connect`; other nonzero exits become warnings. Plain SQL restoration also lacks `ON_ERROR_STOP`, so statement failures can escape exit-status detection.

Source: [restore handling](../../../crates/devflow-core/src/services/postgres/local/seed.rs#L302), [file restore](../../../crates/devflow-core/src/services/postgres/local/seed.rs#L349).

**Change:** propagate unsuccessful restore exits, make SQL statement errors fail the operation, and verify required data before publishing the baseline as ready. Seed into a staging database or new baseline so failed imports do not replace a usable one. Do not suppress failures through broad text matching.

### 6. P1 — Built-in file hooks can write outside the workspace through symlinks

**Reproduced using disposable directories.** A tracked directory symlink named `escape` pointed outside the worktree. A built-in `write-file` hook targeting `escape/marker.txt` overwrote the external marker and reported success.

The confinement check normalizes path components but never resolves symlink targets. The write then follows the symlink. Although devflow is not a general sandbox, this defeats the boundary its built-in file-action code explicitly promises.

Source: [path confinement](../../../crates/devflow-core/src/hooks/actions/mod.rs#L23), [file write](../../../crates/devflow-core/src/hooks/actions/write_file.rs#L20).

**Change:** resolve and validate existing ancestors, handle final-component symlinks, and use directory-relative/no-follow operations where needed to avoid a check/write race. Apply the same policy to all file actions and automatic environment-file writes.

### 7. P1 — Physical cloning lacks a complete parent availability/consistency guard

**Source; concurrency and failure injection not executed.** The generic local backend stops the parent and then propagates copy errors before restarting it. A failed copy can leave the parent's database stopped.

The PostgreSQL backend improves ordinary error handling by attempting a restart after the copy, but the stop/copy/restart sequence has no cross-process parent lock. Two agents cloning the same parent can overlap: one restarts it while the other is still copying. Metadata locks do not protect the whole physical operation. Cancellation, including desktop command timeouts, can also interrupt recovery.

Source: [generic clone sequence](../../../crates/devflow-core/src/services/local_engine.rs#L433), [PostgreSQL clone sequence](../../../crates/devflow-core/src/services/postgres/local/mod.rs#L323), [desktop timeout](../../../src-tauri/src/commands/workspaces.rs#L151).

**Change:** hold a cross-process lease for the complete parent quiesce/copy/restore sequence, persist recovery state, and make interrupted operations repairable. Prefer immutable, prepared baselines or database-supported snapshots where possible. Verify parent availability after success, copy failure, cancellation, and competing clone operations.

### 8. P2 — A missing worktree parent directory leaves an operation that cannot simply be retried

**Reproduced.** With `path_template: ../nested-workspaces/{workspace}` and no `nested-workspaces` directory:

1. Creation fails after leaving a branch and partial `.git/worktrees` metadata.
2. Creating the parent directory and retrying still fails with “directory exists.”
3. Pruning the disposable repository's stale Git metadata allows a retry, but the result has `parent: null`, losing creation provenance.

Source: [worktree creation](../../../crates/devflow-core/src/vcs/git.rs#L458), [directory setup](../../../crates/devflow-core/src/workspace/worktree.rs#L102), [registration order](../../../crates/devflow-core/src/workspace/switch.rs#L97).

**Change:** preflight/create parent directories, record intended provenance before mutations, and make incomplete worktree creation resumable. Repair only metadata owned by the interrupted operation; preserve pre-existing refs and worktrees.

### 9. P2 — The machine-readable CLI contract breaks in ordinary automation paths

**Reproduced in four ways:**

- `--json link main` prints `Running pre-switch hooks...` before the JSON document.
- A process readiness command emitting text puts that text directly before `process start` JSON.
- Early workspace errors emit no JSON document, only human-readable stderr.
- A skipped pre-switch hook produces a warning but is absent from the JSON phase summaries; reported phases each show zero skipped hooks.

Source: [link verbosity](../../../src/cli/workspace/link.rs#L22), [readiness subprocess output](../../../crates/devflow-core/src/processes/mod.rs#L3107), [pre-switch hook handling](../../../crates/devflow-core/src/workspace/switch.rs#L69).

**Change:** centralize versioned success/error serialization. Capture subprocess diagnostics or route them to stderr, and include every attempted lifecycle phase with success, skip reason, or failure. Return a materialized path on partial success and an explicit readiness state. An agent should not infer success from the absence of an error field.

### 10. P2 — Neon and Xata adapters use routes inconsistent with their documented APIs

**Source plus official API documentation; no live account test.** The adapters call paths ending in `/workspaces`. Their official APIs expose these resources as `/branches`. Neon response structs also expect `workspace`/`workspaces`, so correcting the URL alone is insufficient.

Source: [Neon adapter](../../../crates/devflow-core/src/services/postgres/neon.rs#L46), [Xata adapter](../../../crates/devflow-core/src/services/postgres/xata.rs#L80). Upstream contracts: [Neon list branches](https://api-docs.neon.tech/reference/listprojectbranches), [Xata branch API examples](https://xata.io/docs/platform/branch).

**Change:** keep upstream terminology in transport types and map it into devflow's workspace model at the adapter boundary. Add request/response contract tests, parent-ID resolution checks, and a small live smoke test before advertising working support. Exclude these adapters from defaults until validated.

### 11. P2 — Configured automatic process startup does nothing on a first child workspace

**Reproduced.** A configured, approved daemon with `auto_start: true` was not started when creating the first child of an idle parent. The command returned success, `processes_skipped: false`, and zero starts.

New workspaces inherit the parent's running/desired process set. If that set is empty, nothing starts. This conflicts with the README's promise that automatic startup starts configured processes.

Source: [startup selection](../../../crates/devflow-core/src/workspace/switch.rs#L264), [empty parent set](../../../crates/devflow-core/src/processes/mod.rs#L816), [documented behavior](../../../README.md#L163).

**Change:** distinguish “start configured processes” from “inherit the parent's running selection.” Make the policy explicit, choose a useful first-run default, and report why a configured process did not start.

### 12. P2 — HTTP proxy buffering prevents streamed responses

**Source.** The ordinary HTTP path collects the complete upstream response before returning it. An SSE stream that remains open never delivers its events through this path. Request bodies are also collected in memory. WebSocket upgrade handling is separate and does not solve HTTP streaming.

Source: [request buffering](../../../crates/devflow-proxy/src/server.rs#L321), [response buffering](../../../crates/devflow-proxy/src/server.rs#L384).

**Change:** stream request and response bodies with bounded buffering and cancellation propagation. Test time-to-first-event, large uploads, client disconnects, and a stream that never closes. If maintaining a proxy distracts from workspace reliability, offer stable URL integration through an optional existing proxy.

### 13. P2 — Desktop config editing disagrees with core configuration discovery

**Source.** Core supports YAML and TOML filenames; desktop config commands always read/write `.devflow.yml` and parse YAML. A valid project using another supported filename can fail to open in the editor, or a save can create a new YAML file that shadows the original.

Source: [desktop config commands](../../../src-tauri/src/commands/config.rs#L4), [core config loading](../../../crates/devflow-core/src/config/loading.rs).

**Change:** expose one config-document API from core, including the actual path, format, effective values, and source of overrides. Edit that document atomically. Preserve comments where possible. If multiple formats are not worth supporting end to end, deprecate them deliberately.

### 14. P2 — Release builds compile out warnings needed to diagnose partial setup

**Source.** CLI, core, and proxy enable `log/release_max_level_off`. Release builds therefore remove diagnostics such as failed environment-file writes, failed parent restarts, and provider cleanup failures that are only reported through logging.

Source: [CLI logging feature](../../../Cargo.toml#L61), [core logging feature](../../../crates/devflow-core/Cargo.toml#L48), [environment failure handling](../../../crates/devflow-core/src/workspace/switch.rs#L206).

**Change:** retain operational warnings/errors in release builds and return material failures in structured results. Logging alone must not determine whether an agent notices that its generated connection settings were not written.

### 15. P2 — Feature flags do not produce the implied minimal build

**Source; a separate minimal-feature build was not completed.** The root enables `devflow-core` without `default-features = false`, while core defaults enable local/cloud providers and LLM support. Turning off root defaults consequently does not turn off core defaults. The proxy is unconditional, and Pitchfork is an unconditional core dependency.

Source: [root features/dependencies](../../../Cargo.toml#L26), [core defaults and dependencies](../../../crates/devflow-core/Cargo.toml).

**Change:** make feature forwarding explicit and verify a small supported build matrix. Keep the basic Git/workspace/JSON path independent of optional cloud, AI, terminal, proxy, and supervisor functionality wherever practical.

### 16. P2 — Reusing a global container does not validate its configuration

**Source.** A running container with the configured global name is accepted immediately. Its image, published port, and credentials are not compared with the requesting project's settings. Different projects can configure the same global container name differently; the later project can receive connection details for settings the running server does not use.

Shared PostgreSQL inventory also calls `ensure_ready`, so an apparently observational operation can create/start infrastructure.

Source: [existing-container handling](../../../crates/devflow-core/src/services/shared/container.rs#L81), [inventory and connection details](../../../crates/devflow-core/src/services/shared/mod.rs#L224).

**Change:** define and validate a shared-engine identity/configuration contract. Either centralize engine configuration or reject incompatible reuse. Separate inspection from provisioning, and expose stopped/missing states through inventory without implicitly starting services.

## What is good and should remain

| Design | Why it is valuable |
|---|---|
| Every Git workspace has a materialized worktree | Agents get a concrete working directory; humans avoid repeatedly mutating a shared checkout |
| Common core workspace operations | A strong basis for matching CLI and GUI behavior instead of maintaining separate orchestrators |
| Raw VCS names separated from service keys | Correct direction for identity; the remaining provider transformations need fixing |
| Immutable parent provenance and versioned tree inventory | Makes branching history inspectable, including missing parents and deterministic display order |
| Conservative legacy-key migration | Refusing ambiguous ownership is preferable to silently attaching existing data to the wrong workspace |
| Removal preflight and partial-failure handling | Default/current/dirty workspaces are checked, and failed service deletion can preserve the worktree for recovery |
| Non-interactive hook policy with template approvals | Suitable for repeatable automation once every skipped/failed phase is reported |
| Provider/storage abstractions | Lets logical clones, physical clones, and external providers share lifecycle operations while retaining different capabilities |
| Atomic state replacement and mutation locking | Useful foundation; extend protection to external operations rather than replacing it with ad hoc state |
| Real CLI and Docker integration tests | More useful than only testing serializers or mocked providers; add adversarial ownership cases |
| Atomic Redis slot allocation and loopback shared-service bindings | Good concrete precautions already present |

Relevant implementations: [inventory](../../../crates/devflow-core/src/workspace/inventory.rs), [deletion lifecycle](../../../crates/devflow-core/src/workspace/delete.rs#L265), [state transactions](../../../crates/devflow-core/src/state/local_state.rs#L480), [Redis allocation](../../../crates/devflow-core/src/services/shared/redis.rs#L150).

The evidence supports improving this architecture incrementally. A wholesale rewrite would discard useful behavior and regression coverage.

## Simplify the product around reusable data and reliable workspaces

### Make code origin and data origin explicit

A Git branch is not a database snapshot. A person may want code from a feature branch and data from a known baseline; an agent retry may need the same code with a fresh clone of that baseline.

Persist code origin, data source/baseline identity, and workspace creation parent separately. Keep creation provenance immutable. For each service, report whether it was cloned, newly seeded, started empty, or intentionally reused, plus its readiness and clone strategy. These are proposed contract improvements, not claims about existing fields.

This makes the essential workflow clear: adopt a project, prepare or select reusable data, create a workspace, verify its services, work, reset selected data when needed, and remove only owned resources.

### Centralize the operation contract and recovery

Use one core plan and result type for creation, selection, reset, and removal, consumed by CLI, TUI, desktop, and agents. A small durable operation journal should track completed stages and recovery actions across VCS, services, generated files, and processes.

Return an operation identifier, workspace path, per-service readiness, baseline provenance, and actionable failure details. Make retries reuse that intent. Preserve useful worktrees after service failures, but do not run dependent migrations or processes as if all required services and environment writes succeeded.

The current switch path continues after partial service failures and gates post-service hooks on *any* service succeeding. That is a reason to express dependencies and readiness centrally, rather than making individual clients reconstruct the rules.

### Use one identity and ownership model everywhere

Project display names, branch names, service labels, paths, and database identifiers have different constraints. Keep user-facing names readable while storing explicit stable resource ownership. Build identifiers once from the complete project/service/workspace identity; retain mappings needed for legacy data.

Use exact ownership for discovery and cleanup. Include clone/reset/remove operations in the same model. A future explicit “reuse this service” option should describe shared ownership and forbid accidental deletion by consumers.

### Keep configuration editing and diagnostics in core

Expose both the effective configuration and where each value came from. Let the desktop edit the same document that the CLI loaded. Generate command references, capabilities, and agent instructions from tested contracts where possible.

For onboarding, make a cloned repository with a committed config easy to adopt. The current fresh-config probe required `link main` before `switch -c`; either adopt the primary checkout as part of initialization or make this prerequisite explicit and machine-readable.

### Make large-data behavior a first-class concern

File seeding currently reads the whole dump before copying it into the container. Stream imports and exports, report progress and cancellation, and record the baseline revision/checksum. Benchmark real data sizes and measure both clone duration and interruption to the source database.

An immutable prepared baseline is especially useful for many agents: it avoids repeatedly stopping a developer's active database and gives retries a reproducible starting point.

## What I would drop, defer, or make optional

These are scope recommendations, not deletions made by this review.

| Area | Recommendation |
|---|---|
| Built-in LLM/API fallback for commit messages | Remove from the default core; an external command integration is enough for the main product goal |
| Unvalidated cloud adapters | Hide from defaults until protocol and live smoke tests pass; keep the provider boundary |
| Two full process supervisors | Choose a preferred runtime and keep a thin adapter contract; avoid maintaining overlapping native and embedded-supervisor behavior |
| Proxy/TLS/CA management | Make optional; stable URLs are useful, but a custom networking stack should not be required for isolated data/worktrees |
| Implicit `mise trust -y` during worktree creation | Replace with an explicit configured lifecycle integration using the existing approval policy; core worktree creation should not silently change another tool's trust state |
| Broad synchronization of AI-tool settings/permissions | Prefer generated project guidance and a small documented integration contract; avoid owning every agent's configuration format |
| More GUI orchestration features | Defer until core results/recovery are dependable; retain high-value inventory, service status, logs, and connection access |
| Duplicate config formats and compatibility aliases | Retain only what can be supported consistently, then deprecate with a migration window |

The implicit tool-specific trust call is in [worktree setup](../../../crates/devflow-core/src/workspace/worktree.rs#L359). The process module is roughly 4,000 lines, and [ProjectDetail](../../../ui/src/pages/projects/ProjectDetail.tsx), [HookManager](../../../ui/src/pages/hooks/HookManager.tsx), and [HookEditModal](../../../ui/src/pages/hooks/HookEditModal.tsx) concentrate substantial orchestration/editor logic. Split by responsibility when changing these areas, rather than performing a cosmetic file-splitting campaign.

Keep CLI and JSON as the primary integration surface. Agents can use the same proven lifecycle as humans without requiring a separate agent orchestration product.

## Testing, documentation, and release gaps

The passing tests are useful but do not yet demonstrate the central isolation promise. Add a compact contract suite that every applicable provider must pass:

1. Seed a known marker, clone twice, change one child, and verify the baseline and sibling are unchanged.
2. Exercise projects `app`/`app_child`, multiple services of one engine, `main`/`_main`, long names, and ambiguous legacy ownership.
3. Run simultaneous clones from one parent; inject copy failure and cancellation; verify parent availability and retry behavior.
4. Fail one required provider or environment write; ensure dependent setup does not execute against stale connections.
5. Retry interrupted creation/removal while preserving original provenance and unrelated resources.
6. Test one JSON document for both success and errors, noisy subprocesses, all hook phases, and partial materialization.
7. Run the same config/path/identity cases through core and desktop commands; add a small frontend interaction suite for the critical workflow.
8. Test upstream API request/response contracts and minimal/default feature builds.

Specific maintenance observations:

- The [coverage job](../../../.github/workflows/ci.yml#L124) builds the entire workspace without the desktop system dependencies installed by the main Linux job, and without an explicit frontend build. It appears under-provisioned; verify it from a clean runner or scope coverage to the intended crates.
- The [release dependency list](../../../.github/workflows/ci.yml#L386) waits for filesystem integration jobs, but not the separate shared-provider Docker integration job. Make essential provider tests a release gate.
- [Frontend scripts](../../../ui/package.json) provide compilation/building but no test command. The build passed with a largest JavaScript asset of approximately 2.43 MB minified, 687 kB compressed. Investigate lazy-loading terminal/editor-heavy code after correctness work.
- [SECURITY.md](../../../SECURITY.md) identifies 0.3.x as current while the repository is 0.7.1. Refresh version/support and command examples alongside generated agent documentation.
- Linux filesystem CI is valuable; disabled macOS filesystem coverage and unexecuted Jujutsu/cloud paths should be reflected in support claims.

## Recommended sequence

**First: establish resource ownership and trustworthy data.** Fix findings 1–6, add the corresponding invariant tests, and plan migrations for existing namespaces before changing naming. Success means distinct projects/services/workspaces cannot accidentally share or delete each other's data, and invalid seeds never become ready baselines.

**Second: make lifecycle operations recoverable.** Protect cloning, journal interrupted operations, repair worktree retries, gate dependent setup on readiness, and complete the JSON error/phase contract. Success means an agent can retry after failure without guessing what already happened.

**Third: consolidate the product surface.** Align configuration editing, clarify process startup, validate or defer cloud providers, preserve release diagnostics, and make optional subsystems truly optional.

**Then: optimize the proven workflow.** Measure baseline preparation, clone latency, storage use, source downtime, and time until the application is ready. Expand engines and interfaces only when they can satisfy the same contract.

The highest-value next milestone is a demonstrably safe “existing project + reusable baseline → ready isolated workspace → reliable reset/removal” flow for one well-tested local database backend, shared consistently by humans and agents.
