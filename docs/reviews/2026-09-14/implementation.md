# Review implementation

The [original review](review.md) and its captured results describe the starting commit. This document records the authorized implementation. Production changes are in the working tree; no release or deployment was performed.

## Completed changes

| Review area | Implementation |
| --- | --- |
| 1–4: ownership and identity | Shared resources have an exact ownership registry with explicit legacy adoption. Local storage and containers include project, named service, and raw workspace identity. Local PostgreSQL and generic Docker also hash identities before naming containers; `main` and `_main` stay distinct. Cleanup selects owned records or exact labels rather than display-name prefixes. |
| 5: seeding | File/S3 transfers and remote dump relay stream with bounded buffers. SQL and archive restores stop on errors and use a transaction. Temporary dumps stay private and are readable by the restore client in containers with an unprivileged server UID. Failed imports persist as failed across stop/start; explicit seeding can repair a stopped failed workspace. |
| 6: file actions | Built-in writes, copies, replacements, and generated environment files use directory capabilities and atomic replacement. Parent symlinks cannot escape the workspace. |
| 7: cloning | Cross-process leases protect parent and target operations. Parent restart survives copy errors and caller cancellation; an interrupted process leaves recovery intent for the next attempt. Local filesystem copies stage the new directory before replacing existing data. |
| 8: worktrees | Missing parent directories are created, intent is recorded before VCS mutation, and failed attempts clean only their own Git metadata/ref. Retrying preserves creation provenance and pending `post-create` work. |
| 9: automation | Central stdout serialization produces one JSON success/error document, including early failures. Lifecycle results include pre-hooks and skip reasons. Partial setup retains its materialized path and operation status. |
| 10: cloud adapters | Neon/Xata transport uses the documented branch routes, response envelopes, parent IDs, and returned connection credentials. Loopback HTTP contract tests cover both adapters. Cloud adapters are optional. |
| 11: processes | `processes.start_policy: config` starts configured processes for the first child of an idle parent. `inherit_parent` explicitly selects the old behavior. Native processes are the default; Pitchfork is optional. |
| 12: proxy | HTTP request and response bodies stream. A regression test verifies that an upload chunk and an SSE event arrive before their streams end. |
| 13: configuration | Core exposes the discovered document path, format, source text, effective values, and service override sources. Desktop edits that YAML/TOML document, checks for stale edits, and saves atomically. Raw edits preserve comments; structured edits serialize the project configuration. |
| 14: diagnostics | Release builds retain operational warnings/errors. Required service or environment-write failures reach structured results and block dependent setup. |
| 15: feature boundaries | Root/core defaults and forwarding are explicit. Minimal CLI builds exclude local services, TUI, proxy, cloud, Pitchfork, and API LLM support. The default includes local services/native processes/TUI. External commit generators work without API LLM support. TUI proxy controls report the missing feature when it is excluded. |
| 16: shared containers | Existing shared engines are checked for compatible identity, image, environment, mounts, and loopback ports. Inventory, connection inspection, and diagnostics observe without creating or starting an engine. |

Additional contract and maintenance work:

- Independent `--from` and `--data-from` origins persist in workspace state and operation intent. Service capabilities report their data strategy.
- Switch/remove operations have durable IDs, phases, readiness, and error details. Doctor reports unfinished operations. Required setup gates dependent hooks/processes and JSON-mode command execution.
- Generic Docker start/switch/retry share readiness checks and correctly resume paused containers. Generic providers report no cloned data parent.
- Implicit `mise trust` was removed. Broad AI tool settings copying defaults off; explicit lifecycle hooks and guide generation remain available.
- Agent guides and the example handoff check readiness. Generated LLM documentation covers origins, ownership, and recovery; its generator no longer searches for or builds a binary it never uses and works with macOS grep.
- Frontend routes and the terminal load on demand. The initial JavaScript entry fell from about 2.43 MB to 185 KB; the approximately 2.07 MB terminal chunk loads when opened.
- CI tests minimal/default/all-feature builds and frontend results. Coverage is scoped to CLI/core/proxy, and releases now depend on shared-provider Docker tests and the feature matrix as well as Linux filesystem checks. Supported-version documentation now covers 0.7.x.

## Verification

| Check | Result |
| --- | --- |
| Minimal CLI/core tests | 267 passed |
| Default workspace tests | 361 passed |
| All-feature workspace tests, including Docker | 377 passed: 365 unit/CLI/contract tests and 12 Docker tests |
| Strict all-feature Clippy, all targets | Passed with warnings denied |
| Strict default Clippy, all targets | Passed with warnings denied |
| Frontend tests | 4 passed, 8 assertions |
| Frontend production build | Passed; terminal remains a large, deferred chunk |
| Documentation site | Built 28 pages |
| Generated LLM documents | Regenerated; consistency check passed |
| Rust formatting, shell syntax, and diff whitespace | Passed |

The Docker regressions cover neighboring projects, multiple services, raw-name collisions, local mount separation, strict import rollback, shared settings mismatch, parent recovery after failure/cancellation, and a real PostgreSQL baseline → concurrent children → isolated writes → reset → failed import → repair flow. Four additional lifecycle tests cover shared PostgreSQL, ClickHouse, Redis, and RustFS. Each Docker test uses a temporary ownership/configuration directory.

The complete Rust/Docker command is:

```bash
cargo test --locked --workspace --all-features -- --include-ignored --test-threads=1
```

## Migration and limits

Read the [ownership and recovery guide](../../../docs-site/src/content/docs/guides/ownership-and-recovery.md) before using existing service data. Legacy shared resources require exact mappings; legacy PostgreSQL projects require explicit UUID adoption. Old MySQL/ClickHouse data and generic containers require a deliberate migration. Existing data is not automatically renamed or removed. Keep ownership state with the data and set a stable `project_id` before provisioning if the checkout will move.

Validation ran on macOS with Docker. Live Neon/Xata accounts, S3 credentials, Jujutsu, and Linux ZFS/Btrfs/XFS environments were not exercised locally. Their existing platform gates remain relevant; HTTP contract tests are not live cloud verification. Large-dataset throughput and source downtime were not benchmarked. Configured generic bind mounts/volumes retain their sharing behavior, and shared engines remain logical isolation rather than an authentication boundary.

Cloud transport references: [Neon branch creation](https://api-docs.neon.tech/reference/createprojectbranch), [Neon connection URI](https://api-docs.neon.tech/reference/getconnectionuri), [Xata branch creation](https://xata.io/docs/api-reference/branches/create-a-new-branch), and [Xata branch credentials](https://xata.io/docs/api-reference/branches/retrieve-branch-credentials).
