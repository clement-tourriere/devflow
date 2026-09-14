# Remaining review fixes

Implementation follow-up to [pass three](architecture-pass-3.md) and [pass four](architecture-pass-4.md). Explicit commands, shell hooks, terminals, managed processes, sessions, companion lifecycle and executable plugins remain supported. Internal Git/jj, filesystem, Docker and ZFS operations use native libraries.

## Findings addressed

| Finding | Change | Verification |
| --- | --- | --- |
| R1 | Separate stable workspace identity from creation generation. Full removal clears data bindings; retaining services retains their exact origin and protects the baseline. | Live PostgreSQL removal/recreation selects v2 after v1; retained data rejects a different origin and survives a matching retry. |
| R2 | Persist immutable PostgreSQL image IDs and inherit them through ordinary descendants. Validate an existing container's image, ownership, environment, mounts and ports before reuse. | Live fixture retags its private image alias to Redis; descendants still use the sealed PostgreSQL image. Incompatible running/stopped containers are rejected. |
| R3 | Fingerprint destination inputs, treat missing/incompatible inputs as cache misses, and preserve internal relative symlinks. Source checkouts keep their own caches. | Cache contracts cover different branch inputs, old matching artifacts, mutable-copy independence, read-only sharing, executable symlinks and rejected escapes. |
| R4 | Policy rejection becomes a terminal no-effect cancellation, restoring desired state. Explicit cancellation accepts only proven no-effect failures, including identified legacy removal rejections. | CLI execution remains available after rejected removal; core cancellation refuses partially applied operations. |
| R5 | Preflight all worktrees before teardown, report per-resource failures, and retain configuration/ownership for retry. Stage configuration deletion with restoration on failure. | Dirty teardown is rejected; permission-denied cleanup retains state and succeeds on retry. The test also exposed and fixed Git's early metadata deletion. |
| R6 | Each execution has a durable ID, state, completion result and verified process identity. Detached runners claim ownership before launch is reported successful. | Failed spawn, concurrent runs, cancellation, failed/fake launchers, detached completion and refusal to replay completed records. |
| R7 | Foreground, detached and one-shot controllers share one OS lease and verified owner identity. Status probes cannot cause false duplicate-owner startup failures. | CLI ownership/start/stop checks plus deterministic status-probe contention regression. |
| R8 | Staged PostgreSQL copying preserves the previous live directory. | Already implemented in the preceding commit; native copy failure/retry tests remain passing. |
| R9 | Separate jj inspection from snapshot/import/export. Tree-only snapshots and bookmark changes preserve the Git index; active Git operations block incompatible mutations. | Partial staging survives inspection, snapshots and bookmark creation; a real conflicting Git merge retains its index and operation files. |
| R10 | Copy payloads through directory capabilities, reject destination escapes, publish completed entries and propagate failures. Retry copies before readiness. | Default `.env` escape, parent symlink, unsupported socket, symlink preservation and CLI setup retry. |
| R11 | Enforce unshared leaf datasets on ZFS snapshot sources and replacement targets before cutover. | Linux fixture covers nested `pgdata` rejection and preservation of the old target; live execution requires Linux CI. |
| R12 | Recover stale jj workspaces through operation reconciliation and three-way tree merging. | Parent updates preserve child edits; overlapping edits remain native conflicts and repeated recovery is stable. |
| R13 | Git and jj share durable directory-cleanup receipts, with verified filesystem identity and staged-removal progress. Registration remains available until traversal succeeds; receipts stay discoverable after deregistration, and generation checks protect reused registrations. | Permission failure/retry, partial traversal after metadata loss, replacement-directory preservation, and retry after deregistration. |
| R14 | Honor `core.hooksPath`; preflight installation and replace/uninstall only recognized devflow hooks. | Existing project hooks survive, installation does not partially overwrite the hook set, and managed hooks remain updateable. |
| R15 | Generated checkout scripts dispatch directly to native `devflow git-hook` discovery. | Script regression rejects internal `git rev-parse` wrappers; the process boundary test remains unchanged at 12 permitted construction sites. |
| R16 | Release publication explicitly depends on native ZFS fault-injection tests. | CI YAML parsed and the dependency checked. Both normal ZFS integration and recovery tests remain required jobs. |

## Architecture changes

- Workspace planning resolves the code revision, exact data origin, creation path, payloads, cache input specifications, hooks and protections before effects. Switch records and consumes the plan. Retries preserve its selected code/data origins and path; a regression advances the parent between attempts and verifies the original revision is still used. Cache eligibility is checked against the materialized destination. Dirty jj sources explicitly report that a snapshot is required.
- CLI and desktop service creation/deletion/destruction submit shared core requests with journals and verified outcomes. Operation inspection, resume and constrained cancellation use the same core APIs. The desktop offers sealed baseline selection and pending-operation recovery controls.
- Removed unused parsed-config IPC, its TypeScript schema mirror, an unused guide-generation alias and an obsolete plugin scaffold branch. Simplified the single native process-runtime abstraction while retaining ownership/migration diagnostics.
- The proxy has a standalone command tree. Releases package/checksum both binaries, gate publication on ZFS recovery, and optionally sign both assets. `DEVFLOW_INSTALL_PROXY=1` installs the companion; the standard CLI delegates proxy commands to it. Foreground output stays streamed; JSON startup requires `--daemon`.
- Deprecated broad ignored-file and AI-directory copying in favor of explicit payloads and validated caches. Corrected maintained CLI, architecture, installation, cache and worktree guidance, including the `.localhost` default and native jj recovery.

## Verification

- `cargo test --locked --workspace --all-features`: 388 passed, zero failures. The 15 environment-dependent tests are ignored by default; the 14 Docker tests were executed separately below.
- 14 live Docker tests passed: 10 review regressions and 4 shared-engine lifecycles. The baseline-generation fixture passed again after the final cleanup receipt changes.
- Strict Clippy passed for the full workspace (`--all-targets --all-features`) and the minimal CLI (`--all-targets --no-default-features`), both with `-D warnings`. Rust formatting passed.
- Standard-build proxy delegation regression passed: one test covering forwarded flags, streamed output and structured errors. The standalone proxy build passed; the standard CLI successfully delegated a read-only `--json proxy trust info` request to the real companion.
- Installer fixture passed successful installation and checksum rejection while preserving an existing proxy binary.
- UI TypeScript and production build passed; 4 UI result-handling tests passed.
- Documentation build passed: 31 pages. Generated LLM-document freshness, shell syntax and diff checks passed.

Release validation labels: Linux real-project use (including ZFS, XFS and Btrfs), Neon/DBLab/Xata live accounts, and interactive desktop sessions are **untested**. Local validation uses macOS. Automated Linux filesystem checks remain release-gating CI jobs and run separately; their results do not imply real-project or live-cloud validation. See the [v0.8.0 release notes](../../releases/v0.8.0.md).
