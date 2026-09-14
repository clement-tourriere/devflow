# Second architecture pass — implementation

The authorized implementation builds on the first review changes already present in the working tree. It fixes the four reproduced contracts, removes redundant integrations and editors, centralizes lifecycle policy and identities, and delivers reusable PostgreSQL baselines. Existing resource ownership migrations and service keys remain supported.

## Delivered

- [x] Typed core lifecycle outcomes and program/argument execution shared by CLI, TUI and desktop. Human and JSON modes gate execution on the same readiness result. Required background hooks finish before dependent commands; optional hooks report warnings and durable completion receipts.
- [x] Explicit provider capabilities for start, stop, reset, seed, logs and snapshots. Unsupported resets fail. Shared engine health checks observe; controllers explicitly start engines.
- [x] Recoverable ZFS dataset replacement with durable cutover intent, rollback, retained previous generations and read-only diagnostics. Model fault tests pass; a real Linux ZFS failure-injection test and dedicated CI job are included.
- [x] Removed inert settings, timestamp capability claims, unused discovery/Docker helpers, duplicate guide IPC, AI policy merging, the built-in LLM API backend, embedded Pitchfork and its web bridge. Native supervision and external commit commands remain.
- [x] Reduced desktop configuration to a format-preserving raw editor and hook inspection. Provider choices use the compiled capability catalog; unsupported operations and unavailable providers are hidden. Frontend descriptor types and provider reference facts are generated from the CLI manifest.
- [x] Stable project/workspace/service/resource identities and a shared core project registry. Explicit project relocation preserves identities, namespaces and operation history. Desktop visibility is a core attribute rather than another project list.
- [x] Durable operation requests, typed checkpoints, attempts, fingerprints and bounded history. Inspection and resume cover switch/removal and service start/stop/reset/cleanup. Failed imports remain visible and require explicit repair or replacement.
- [x] Versioned local PostgreSQL baselines: snapshot exact input bytes, import, optional migration/validation, record source and engine fingerprints, stop and seal. Children retain exact version bindings; reset uses the recorded generation and sealed image identity. Removed version numbers are never reused; live dependencies block deletion.
- [x] Previewable retention with default/current/materialized-worktree protections, pins, expiry, Git locks, active executions, operation leases, data dependencies and count budgets. Failure results retain configuration and ownership state for retry.
- [x] Explicit fingerprinted caches with writable workspace copies or shared read-only artifacts, input/path checks, per-artifact byte budgets, stale-generation rejection and failed-stage cleanup.
- [x] Optional proxy companion with a shared loopback API and exclusive runtime owner. The desktop no longer embeds the proxy. Legacy runtime/CA locations remain the default; `DEVFLOW_PROXY_DIR` supports isolated runtime files.
- [x] Cross-process port reservations for PostgreSQL, generic containers, MySQL and ClickHouse. The concurrency benchmark exposed the previous process-only race; OS leases now remain held until container startup completes and release on failure or exit.

The repeated Docker run also exposed a restart race: a container could report running while its database entrypoint was still starting. Clone recovery now waits for the provider readiness command before quiescing and before releasing its recovery lease. The complete Docker suite passed after this fix.

## Validation

| Check | Result |
| --- | --- |
| All-features Rust workspace | 363 tests passed; platform/Docker tests are explicit opt-ins. Includes CLI, TUI, desktop command adapters, core, cloud HTTP fixtures and proxy tests. |
| Default CLI/core | 322 tests passed. |
| Minimal CLI/core (`--no-default-features`) | 267 tests passed. |
| Docker regressions and shared engines | 13 passed, including local PostgreSQL, MySQL/ClickHouse isolation, Redis, RustFS and baseline lifecycle. The final reset test also verifies the actual container requests the sealed image ID. |
| Frontend | TypeScript check, production build, and 4 behavioral tests passed. The large terminal chunk remains lazy-loaded. |
| Documentation | 31-page production build and generated LLM/provider document checks passed. |
| Clippy / formatting / dependencies | All-target all-feature Clippy passed; formatting and diff checks passed; `cargo machete` reports no unused dependencies after removing the unused CLI `which` dependency. |
| Original four probes | All corrected; captured in [architecture-probe-results-after.json](architecture-probe-results-after.json). Historical failing results remain unchanged. |
| Companion runtime | Actual executable became ready, served the targets endpoint, rejected a second owner even on different ports, and shut down cleanly. See [proxy-companion-results.json](proxy-companion-results.json). No CA trust installation, mDNS or Docker network attachment was performed. |
| Data benchmark | Eight successful measurements across 1/4 MiB fixtures, concurrency 1/2, and mutable-workspace/sealed-baseline origins. See [data-benchmark.json](data-benchmark.json). |

The benchmark uses debug binaries and small disposable fixtures. It is a concurrency and behavior check, not a production throughput claim. It reports clone/reset timings, CLI peak RSS, logical/allocated storage growth and sampled source TCP availability; physical bytes copied are explicitly unknown on CoW filesystems. All four baseline cases recorded zero sampled interruption to the active source. Source interruption was observed in the mutable-source cases; exact timings are retained in the measurement artifact. These measurements do not justify a prepared-clone pool, so no pool was added.

The source inventory fell from 75,305 to 72,340 lines across the same Rust/TypeScript roots, including inline tests: a net reduction of 2,965 lines. The desktop source fell from 13,119 to 8,778 lines. Counts include the new core modules and generated provider interface; see [architecture-inventory-after.json](architecture-inventory-after.json).

## Compatibility and usage

```sh
# Prepare reproducible data independently of a developer's active workspace.
devflow baseline prepare clean --service db --from fixtures/base.sql --validate fixtures/check.sql
devflow --json --non-interactive switch -c agent/task --from main --data-from baseline:clean@v1
devflow exec --workspace agent/task -- pytest -k 'login or signup'
devflow service reset agent/task --service db

# Inspect/retry an incomplete operation and preview cleanup.
devflow operation list --history
devflow operation inspect OPERATION_ID
devflow --json --non-interactive operation resume OPERATION_ID
devflow --json retention plan --service db --max-count 10
```

Old inert/retired configuration fields are accepted with migration warnings and omitted from normalized saves. Existing raw documents preserve their format and comments. Replace `worktree.respect_gitignore` with the actual `copy_ignored` policy; replace API commit settings with `commit.generation.command`; migrate Pitchfork daemon definitions to native processes; replace policy-merging hooks with generated guides and explicit file allowlists. Old plugins must implement protocol v1 negotiation. Existing Redis configurations without a generic container block retain their shared allocation to avoid silently abandoning data; newly configured Redis can use separate containers.

Core retains the existing configuration wire format and migrates state incrementally; this is not a forced conversion of every backend store into one database. Provider SQLite/resource records still own backend observations. Explicit SQL imports and arbitrary hooks can have partial effects; retries are not exactly-once operations. Baseline origins remain fixed for a workspace; create a new workspace to use another version. Multi-service baseline bundles and warm clone pools are deliberately outside this first baseline implementation.

See [architecture and migration details](../../ARCHITECTURE.md), [baselines](../../../docs-site/src/content/docs/guides/baselines.md), [retention and caches](../../../docs-site/src/content/docs/guides/retention-and-caches.md), and [plugin protocol](../../../docs-site/src/content/docs/guides/plugins.md).

## Platform limits

The Linux ZFS kernel/mount test was added but cannot run on this macOS host. Cloud tests use local HTTP fixtures, not live accounts. Native Jujutsu workspace execution was not validated here. These adapters remain optional/experimental where indicated. The Tauri backend compiled and its command tests passed; an interactive desktop session was not driven end to end.

The original review artifacts are preserved with this implementation. The unrelated `.pi/` directory is excluded from the implementation commit.
