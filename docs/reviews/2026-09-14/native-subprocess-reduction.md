# Native operations and explicit execution

Devflow now uses native libraries for its internal workspace and data operations. Explicit command execution remains available: `exec`, shell hooks and conditions, managed application processes, terminals, sessions, the proxy companion, and configured executable provider plugins.

The production inventory fell from **57 to 12 process-construction sites** compared with `37a3a9a63ec561653504771603d372d2bc5a6b1b`. All 12 remaining sites implement these execution features. This counts source locations across platform branches, not processes launched per operation. It excludes executable-contract tests, Rust threads/tasks, dependencies' implementation details, and Docker API requests that execute programs inside containers. See the [current inventory](native-subprocess-inventory.json) and [historical audit](subprocess-audit.md). `tests/process_boundaries.rs` guards the permitted locations.

## Native replacements

| Operation | Implementation |
| --- | --- |
| Git repositories, worktrees and test fixtures | `git2` |
| Jujutsu repositories, bookmarks and workspaces | Pinned `jj-lib` 0.45.1 |
| Directory copies and CoW detection | Shared filesystem copier using `reflink-copy` and `filetime` |
| ZFS datasets, snapshots and replacement | Runtime-loaded OpenZFS `libzfs` API on Linux |
| Docker hooks and fixture cleanup | Bollard / Docker Engine API |
| Executable lookup | Native `which` library |
| Trusted certificate inspection | Native system certificate store |
| Provider catalog generation and benchmarks | Rust examples calling core APIs |

The jj adapter snapshots working copies, preserves raw bookmark identities and sparse patterns, respects ignore/tracking rules and immutable history, and imports/exports colocated Git changes. Forgetting a workspace untracks remote bookmarks without scheduling remote deletion. It preserves forced-removal snapshots for recreation. Stale working copies return recovery guidance. Use a compatible jj CLI alongside the pinned library when operating on the same repository; compatibility with every historical jj format is not asserted. Devflow's generated working-copy commits are unsigned and filesystem monitoring is native; configured GPG/SSH signing and Watchman programs are not invoked.

The ZFS adapter requires an already loaded `/dev/zfs` device, rejects mount-helper settings, and requires unshared datasets (`sharenfs=off`, `sharesmb=off`) so these operations cannot delegate to mount/modprobe/sharing programs. It loads system libraries only when used; other installations do not need ZFS headers or libraries. Its small C binding owns each native handle on a blocking worker. Unmount uses the Linux syscall directly, rename uses `libzfs_core`, and property updates suppress implicit mount/share actions. Workspace replacement requires leaf datasets; nested filesystems are not cloned. Recovery operates on typed mutations and checks snapshot origins before removing a failed replacement. Dataset queries distinguish absence from an API failure. Project/workspace file cleanup stops if dataset destruction fails. It does not follow dependent clones outside the owned subtree. The Linux CI recovery test runs its actual dataset mutations with an empty executable PATH. The interface follows the public [OpenZFS headers](https://github.com/openzfs/zfs/blob/zfs-2.3.3/include/libzfs.h); jj operations use the published [jj-lib API](https://docs.rs/jj-lib/0.45.1/jj_lib/).

## Simplifications and migrations

- Removed the commit wrapper, self-updater and automatic ZFS installer. Legacy commands return actionable migration errors. Commit with Git/your editor, update with the installer/package manager, and provision ZFS externally.
- Removed automatic privileged certificate installation and Linux Avahi subprocess publishing. Certificate trust setup is manual. Proxy hostnames default to `.localhost`; macOS Bonjour remains available for explicitly configured `.local` domains.
- Removed the generic OS notification wrapper. Old `notify` actions report their migration requirement; configure an approved shell hook or consume operation results.
- Removed login-shell PATH evaluation, port-based `lsof` process adoption, and automatic forwarding to pitchfork. The desktop accepts explicit `DEVFLOW_TOOL_PATH` directories. Legacy external process records give migration guidance.
- Removed unused desktop hook-editing IPC and its unused YAML dependency.
- Replaced Python benchmark/catalog wrappers and Git/Docker test setup with native Rust code. Historical review output remains archived.

The shared copier also fixes **R8** from the [third architecture review](architecture-pass-3.md): a failed PostgreSQL stage copy no longer deletes its live workspace parent. Copies preserve modes, timestamps and symlink entries, replace destination files without writing through existing links, and reject recursive destinations. Ordinary copies do not promise to preserve ownership, ACLs, extended attributes or hardlink topology. APFS uses whole-tree native cloning where available, then restores portable metadata; reflink fallback uses per-file native cloning.

## Validation

Validation includes complete Rust workspace and minimal-feature suites, strict Clippy, native jj lifecycle and Git interoperability tests, Git/jj CLI materialization with an empty executable PATH, staged-copy failure/retry tests, ZFS recovery fault injection, provider catalog checks, and frontend/documentation builds. The full workspace run passed 365 tests, with 14 environment-dependent tests ignored; the minimal build passed 267. The final focused storage run passed 10 tests (one live ZFS test ignored), including the additional native-helper guard. Strict Clippy, dependency checks, generated provider contracts, TypeScript and Vite builds passed, and the documentation build produced 31 pages.

The live Docker suite passed all 13 service tests during this implementation. The [native benchmark result](native-benchmark.json) exercised six clones and six resets from live and sealed-baseline origins at concurrency one and two, with successful fixture cleanup. It measures core API elapsed time and sampled TCP availability; it does not measure CLI process memory, SQL latency, or physical copy traffic and does not establish a performance improvement over a comparable previous run.

Live ZFS kernel behavior remains a Linux CI validation requirement; this macOS environment ran the recovery model, native jj and native filesystem tests. Live cloud services and an interactive desktop session were not exercised. Other findings from the third architecture review are not claimed fixed by this subprocess refactor.
