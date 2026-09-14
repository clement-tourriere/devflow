---
title: Retention and cache reuse
description: Preview resource cleanup, protect workspaces, and choose explicit cache policies.
---

Retention lives in core and uses the same rules across frontends. A cleanup plan protects the configured default, primary and materialized worktrees, Git locks, pins, active executions, unfinished operations, data parents, and baseline versions. Provider failures are returned individually and make cleanup unsuccessful.

```sh
devflow --json retention plan --service db --max-count 10
devflow retention pin agent/long-task
devflow retention expire agent/short-task 2026-12-01T00:00:00Z
devflow --json retention apply --service db --max-count 10
devflow retention unpin agent/long-task
```

The count budget applies to otherwise eligible service workspaces. Expiry is a minimum retention time; reaching it never overrides another protection. A pin remains until explicitly removed. Use `devflow remove` to retire a materialized code workspace. Git locks and devflow pins require explicit unlocking/unpinning even with `--force`.

Workspace operations share a project policy lease, allowing independent workspaces to prepare concurrently. Retention takes an exclusive lease while rechecking and applying its plan. A busy resource returns a retryable busy error instead of stealing an old lock.

## Artifact caches

Ignored files are not all interchangeable caches. Declare allowed artifact paths, their relevant input files, and a policy:

```yaml
worktree:
  copy_ignored: false
  caches:
    - path: node_modules
      inputs: [package.json, package-lock.json]
      policy: copy
      max_bytes: 2147483648
    - path: fixtures/compiled
      inputs: [fixtures/compiler-version.txt, fixtures/source.json]
      policy: shared_read_only
      max_bytes: 268435456
```

`copy` gives each workspace its own writable copy. `shared_read_only` links to a completed immutable artifact on Unix. Shared artifacts are copied into devflow's cache store and made read-only; original project files keep their permissions. Use `copy` for tools that modify their caches. An existing copy is reused only when its recorded fingerprint matches; remove an outdated or untracked target explicitly before rebuilding it. The size limit is per artifact, not a global disk quota.

Fingerprint inputs must cover every relevant dependency, compiler/toolchain version, and build option. Missing inputs, path escapes, and excessive artifact sizes fail setup visibly. Database files and agent permission directories are outside this cache policy. Baselines manage reusable data separately.

No prepared-clone pool is enabled. Use the repository's data benchmark harness to measure clone/reset latency, storage growth, process memory, and source availability for representative datasets before deciding whether a pool is justified.
