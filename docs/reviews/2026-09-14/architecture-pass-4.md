# Fourth architecture review

Follow-up: the findings below are addressed in [remaining-fixes.md](remaining-fixes.md). This file retains the original review evidence.

Reviewed commit: **`e96442ce046d9becd0f45b79b3cee33c88b1ccde`** (`refactor!: use native libraries for internal workspace and data operations`). The requested implementation was committed before this review. This pass adds review evidence; it does not implement the findings below.

**Keep the native direction, but prioritize state preservation and isolation before adding features.** The adapter changes removed substantial incidental execution, yet native calls alone do not preserve the behavior of the commands they replace. A normal jj inspection currently destroys Git staging/merge state, and configured payload copying can escape a worktree. The earlier baseline generation defects also remain open.

There are **eight new findings**: five reproduced through native core APIs, plus three established by source inspection. Six disposable fixture scenarios produced the [captured observations](architecture-pass-4-results.json). The [Rust probe source](architecture-pass-4-probes.rs) uses core, git2, and filesystem APIs; it launches no programs. Findings continue the numbering from [pass three](architecture-pass-3.md).

## New correctness findings

### R9 — P1: Inspecting a colocated jj repository destroys Git staging and merge state

Reproduced two independent cases through `JjRepository::list_workspaces()`:

- A file contained `base` in HEAD, `staged` in the index, and `unstaged` in the working directory. After listing, the index contained **`base`**; the working file still contained `unstaged`. The staged version was removed from the index. The probe explicitly reloads the index from disk to avoid libgit2's cached index.
- A real conflicting merge created with libgit2 had repository state **Merge**, conflict entries, and `MERGE_HEAD`. Listing returned success, but repository state became **Clean**, conflict entries disappeared, and `MERGE_HEAD` was deleted.

[`State::load`](../../../crates/devflow-core/src/vcs/jj_native.rs#L219) always snapshots. Its [mutable snapshot path](../../../crates/devflow-core/src/vcs/jj_native.rs#L500) calls `git::reset_head`, whose implementation resets the index and clears Git operation files. The same reset also appears in the generic transaction finisher, including bookmark operations. Thus callers that intend to inspect or update metadata can perform a destructive Git reset.

**Change:** distinguish a snapshot of the existing working-copy commit from an actual working-copy checkout. The upstream jj CLI's mutable snapshot path uses `git::update_intent_to_add` and ref export; it reserves full reset behavior for transitions that require it. Audit the generic `finish` path too. Add regression coverage for staged-only content, partial staging, merge conflicts and active rebase state around inspection and bookmark operations. Do not fix only the particular `list` caller.

### R10 — P1: Payload copying can write outside its destination worktree

The probe configured `copy_files: [payload/secret]`, with the destination's `payload` entry pointing to a separate fixture directory. Copying created **`outside/secret`**, containing the source bytes, and reported one copied entry.

This also affects the **default `.env` copy**: a dangling destination `.env` symlink pointing outside the worktree was followed, creating the external file and reporting success. All data used in these probes was synthetic.

[`copy_worktree_payload`](../../../crates/devflow-core/src/workspace/worktree.rs#L177) uses `Path::exists()`, which follows symlinks and treats a dangling link as absent. Its [file path](../../../crates/devflow-core/src/workspace/worktree.rs#L210) then calls `reflink_or_copy` on ambient paths. Parent traversal and fallback byte copying can follow the destination links. The shared directory copier's checks do not protect this separate file path. This can be triggered by a symlink committed on the branch being materialized.

**Change:** put payload reads and writes behind directory capabilities, validate relative paths, preserve symlink entries deliberately, and publish replacements without following destination links. Extend the existing [`WorkspaceFiles`](../../../crates/devflow-core/src/files.rs) confinement approach to bulk payload copying. Return typed copy outcomes: directory failures currently only log a warning while the copied count increases. Test the default `.env` path, nested parent links, dangling links, existing hardlinks, and failed copies through the actual worktree setup flow.

### R11 — P1, source-confirmed: ZFS cloning does not enforce its leaf-only rule on the source

The native adapter says nested filesystems are unsupported, but [`require_workspace_dataset`](../../../crates/devflow-core/src/services/postgres/local/storage/zfs_native.rs#L371) is applied to datasets being renamed or mounted. It is **not applied to the parent being snapshotted**. [`clone_from_parent`](../../../crates/devflow-core/src/services/postgres/local/storage/zfs_driver.rs#L141) checks the source directory and layout, then the [snapshot call](../../../crates/devflow-core/src/services/postgres/local/storage/zfs_native.rs#L314) passes `recursive = 0`.

A source with a nested dataset under `pgdata` can therefore produce a clone without that nested dataset's contents. The child can still pass the `child_data_dir.is_dir()` check because the underlying mountpoint directory exists. This conclusion follows from the call sequence and OpenZFS's separate treatment of descendant snapshots; it has **not** been reproduced on a live pool here. [OpenZFS snapshot documentation](https://openzfs.github.io/openzfs-docs/man/master/8/zfs-snapshot.8.html) describes recursive snapshotting as a separate operation.

**Change:** reject unsupported source layouts before snapshot/cutover, checking the actual source dataset as well as the replacement target. Supporting recursive snapshots alone would be insufficient: cloning and recovery would also need a complete descendant mapping. For now, keep leaf-only support and enforce it. Add a Linux fixture with a nested `pgdata` dataset and assert rejection preserves the old target and source data.

### R12 — P2: Ordinary parent edits strand native jj children behind CLI-only recovery

Create a jj child from the primary workspace, edit the primary's tracked file, and inspect the primary. Opening the child through `JjRepository::new` then fails with:

```text
Jujutsu working copy is stale; run 'jj workspace update-stale' before retrying
```

The child still contains its old file. Snapshotting the mutable primary rebases its descendants, but the child's working copy is not updated. The [freshness match](../../../crates/devflow-core/src/vcs/jj_native.rs#L462) deliberately rejects this state. That protects against overwriting work, but makes a common devflow-created transition require an executable that the native workflow is advertised as not needing.

**Change:** implement native stale-workspace recovery using jj's operation and working-copy model, preserving local edits and conflicts. Expose unresolved cases as structured repair information that inventory can still display. Test clean and dirty children, sibling operations and interrupted updates. Avoid a forced checkout as a shortcut.

### R13 — P2: Failed jj directory deletion discards the registration needed for retry

The probe made the parent of a disposable child directory non-writable, then removed that workspace. Directory removal failed with permission denied. However, `worktree_path("feature")` already returned no registration. After restoring permissions, retry failed with **“No jj workspace is registered”**, while the directory still existed.

[`remove_worktree`](../../../crates/devflow-core/src/vcs/jj.rs#L450) snapshots and forgets the workspace before calling `remove_dir_all`. A directory failure therefore leaves physical state that the adapter can no longer address through its normal removal interface.

**Change:** persist a removal receipt containing the verified repository/workspace identity, saved snapshot and exact directory before forgetting. Resume cleanup from that receipt when registration is gone. Simply reversing the calls is insufficient: filesystem removal itself can partially remove `.jj` before failing. Add failures during directory traversal and after forgetting, then verify retry completes without discovering ownership by path prefix.

### R14 — P2: Installing Git hooks overwrites existing project hooks

The probe installed pre-existing `pre-commit` and `post-checkout` hooks containing project checks, then called `GitRepository::install_hooks`. Both were replaced with devflow's scripts; neither original check remained.

The [Git installer](../../../crates/devflow-core/src/vcs/git.rs#L589) writes both files unconditionally. The jj installer has the same pattern for its colocated `post-checkout` hook. These paths also assume `.git/hooks` rather than honoring a configured hook manager's path.

**Change:** preserve hooks owned by other tools. Use a small integration shim or provide the invocation for the project's hook manager; replace an existing file only when its exact managed ownership is established. Cover native Git, colocated jj, configured hook paths and uninstall behavior. Hook execution itself remains useful; destructive installation is the problem.

### R15 — P2: Generated hook scripts still contain internal Git command wrappers

The [generated post-checkout script](../../../crates/devflow-core/src/vcs/git.rs#L11) invokes `git rev-parse` twice to discover repository paths, including primary checkouts where no lifecycle action follows. The probe inspected the installed script and found both calls.

The **12 Rust process-construction sites** remain a correct lexical count, and the boundary test passes. That count does not establish that internal wrappers are gone: generated shell programs are outside what it checks. These two commands are devflow's internal discovery work, within the scope the user asked to replace.

**Change:** move repository/worktree detection and payload setup into the existing native [`git-hook` handler](../../../src/cli/git_hook.rs). Keep the shell entry point as a small dispatcher. Add coverage for the generated script as well as Rust construction sites; retain explicitly requested shell hooks and application execution.

### R16 — P2: Release publication does not wait for ZFS failure-recovery tests

The [`zfs-integration-tests` job](../../../.github/workflows/ci.yml#L124) runs the native failure-injection test with an empty executable PATH. The [release dependency list](../../../.github/workflows/ci.yml#L431) includes the separate ordinary `integration-linux-zfs` job, but not this recovery job, directly or transitively.

Consequently, the workflow can publish a release even if native replacement recovery fails. This is particularly significant because the current local validation platform cannot execute that test.

**Change:** make the recovery job a publication dependency. Keep ordinary PostgreSQL-on-ZFS integration and replacement fault injection as distinct checks; they establish different behavior. Require the supported native library/platform matrix to pass before claiming production ZFS recovery coverage.

## Earlier findings still open

The source files responsible for R1–R7 are unchanged between `37a3a9a` and the reviewed commit. Their earlier reproductions remain relevant; they were not replayed against live Docker or controller processes during this pass.

| Finding | Current consequence | Priority |
| --- | --- | --- |
| R1 | Reusing a deleted workspace name can retain the old baseline binding despite requesting a new version. | P1 |
| R2 | A transitive baseline descendant can use a mutable project image; reusing an existing container ignores the requested image. | P1 |
| R3 | Artifact eligibility fingerprints source inputs without comparing destination branch inputs. | P2 |
| R4 | A policy-rejected removal leaves an unfinished operation that blocks later execution. | P2 |
| R5 | Project destruction can report success and remove ownership/configuration after worktree cleanup fails. | P2 |
| R6 | Failed command launch leaves a fictitious running execution record. | P2 |
| R7 | Foreground controller launches lack exclusive ownership and truthful running status. | P2 |

**R8 is fixed.** The shared native copier preserves live PostgreSQL data and sibling files when staging fails; its regression and successful retry passed again in this review.

## What to keep, simplify and drop

Keep materialized worktrees, separate code/data origins, exact baseline versions, stable service keys, explicit ownership/adoption, required-hook readiness and machine-readable outcomes. Those directly serve humans and agents reusing environments. Keep the native filesystem/Docker/Git direction and the narrow ZFS binding, subject to the contracts above. Keep explicit execution as agreed.

| Surface | Recommendation and evidence |
| --- | --- |
| Structured desktop config IPC | Remove `get_config_json`, `save_config_json`, `get_config_yaml` and their unused client wrappers. The active editor consumes the document/text API. Remove its unused parsed-config projection before deleting the TypeScript config mirror and re-export. Preserve text validation and concurrent-edit protection. |
| `ProcessRuntime` abstraction | Simplify the single-implementation forwarding layer into direct native runtime operations. `runtime_for_config` only constructs `NativeProcessRuntime`. Keep exact legacy-record handling and migration errors; they protect real resources. |
| Compatibility leftovers | Drop the unused `installAgentSkills` UI alias and obsolete generated `cleanup_old_workspaces` plugin branch. Avoid expanding obsolete protocol paths. |
| Generated Git integration | Reduce it to a safe dispatcher, fixing R14/R15. Support the existing hook manager rather than owning and replacing every hook file. |
| Optional proxy | Give it a standalone entry point and an actual release/install path. `src/proxy_main.rs` still includes all of `main.rs`; release packaging still ships only `devflow`. Keep networking outside the default data/workspace path. |
| Broad payload copying | Deprecate indiscriminate ignored/AI-directory copying in favor of explicit payloads and validated caches. The defaults already disable broad copying. Close R10 and R3 before presenting any cache as safe to reuse. |
| Hand-maintained guidance | Remove stale claims about AI-generated commits in README and the deleted `commit.rs`/“jj CLI” entries in the LLM generator. Generated-doc freshness passes while those source descriptions remain wrong. Some proxy comments/docs still call `.local` the default. |

There is no usage evidence here to justify deleting a cloud provider, native jj support, explicit terminals or lifecycle hooks. Optional compilation for the large embedded jj dependency is worth evaluating for minimal builds; it must give an explicit unavailable-adapter error for `.jj` repositories rather than silently using Git.

## Architecture changes to prioritize

The main problem is inconsistent ownership of state transitions. More crates, providers or generic frameworks would not solve it.

1. **Separate inspection from mutation.** A VCS inspection should return one consistent repository/workspace view. Today, `JjRepository::list_workspaces()` calls five `State::load` paths, each capable of snapshot/import/export. Hold one state per request and make snapshot, checkout, stale recovery and ref mutation explicit operations. Inventory must remain useful when repair is needed.
2. **Make resource generation a first-class lifecycle boundary.** A workspace name and stable logical identity can survive reuse; data bindings and engine identities must belong to a specific creation generation. A retry retains that generation. A fresh creation gets the requested one. Teardown records must retain enough information to finish cleanup, even after a VCS registration disappears.
3. **Finish moving mutations into core.** CLI service create/delete/destroy, desktop service deletion and orphan cleanup still make separate policy/outcome decisions. Frontends should submit typed requests and render per-resource outcomes. Preflight rejections must be terminal no-effect results; partial mutations must retain actionable receipts. This addresses R1/R4/R5/R13 together.
4. **Use one resolved workspace plan.** Include code revision, exact data references and engine identity, destination cache inputs, required setup and protection checks. Presentation and execution must consume the same resolution, with validation before effects. This is the most valuable next feature for agents: an inspectable answer to “what will I reuse, and what will change?”
5. **Give execution and reconciliation explicit ownership.** Track each execution separately through starting/running/failed/exited states with verifiable process/session identity. Give the controller one OS-backed owner lease shared by foreground and detached starts. A workspace-level string cannot safely represent multiple executions or an interrupted spawn.
6. **Expose repair through the same application API.** Add constrained operation repair/cancellation for verified no-effect failures, and native jj stale recovery. Extend doctor to compare requested data/engine bindings with observed resources. Then expose baseline selection and operation inspection/resume in the desktop using those same requests.

The remaining large modules identify good seams for this work, not reasons for a mechanical file split: `src/cli/service.rs` is 2,707 lines, `processes/mod.rs` 2,951, and `ProjectDetail.tsx` 2,818. Extract policy into core first; divide UI code by workspace, service and process behavior afterward.

Fix R9/R10/R11 and the existing R1/R2 data-generation problems first. Follow with recovery/ownership defects and the safe removals above. Defer baseline bundles, clone pools, more providers and additional supervisors until those invariants pass failure tests. Existing small-dataset measurements do not establish a need for more infrastructure.

## Verification and limits

- Native review probes: staging, real conflicting Git merge, stale jj child, failed jj removal/retry, existing-hook installation, and file-copy confinement including the default `.env` case. Five correctness findings were reproduced; installed-script inspection also confirmed two remaining wrappers. Fixture directories were removed and no real secrets were used.
- Core all-feature unit suite: **260 passed, one live ZFS test ignored**.
- CLI review contracts: **13 passed**; process boundary test: **one passed**.
- UI result handling: **four passed**.
- Rust formatting and generated LLM-document freshness: passed.
- Source review covered core lifecycle/state/baselines/storage/VCS/execution, CLI/TUI and desktop entry points, UI consumers, proxy distribution and CI dependencies. OpenZFS headers/implementation and the pinned jj library/CLI source were checked for the affected semantics.

These passing tests do not contradict the findings: the committed suites lack these transition cases. This pass did not repeat live PostgreSQL/Docker, cloud, interactive desktop or full UI/docs build validation; the previous implementation's results remain in its report. The new ZFS source-layout finding and release dependency finding are source-confirmed, not live Linux results. A suspected libzfs mount-table cache issue was not established: the inspected library initializes that cache disabled.

To repeat the native observations at the reviewed commit, temporarily copy `architecture-pass-4-probes.rs` to `crates/devflow-core/examples/review_native.rs`, then run `cargo run --locked -p devflow-core --example review_native` and remove the temporary example afterward. Use an ordinary user account for the permission-denied fixture. The probe supplies isolated jj configuration and reports observations as JSON; a `probe_error` means fixture setup failed, not that a finding passed or failed.
