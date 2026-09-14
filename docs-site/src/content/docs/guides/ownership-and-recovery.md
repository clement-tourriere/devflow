---
title: Data ownership and recovery
description: Choose code and data origins, adopt existing resources, and recover interrupted workspace setup.
sidebar:
  order: 4
---

Every service resource belongs to one project, named service, and workspace key. New shared databases, buckets, Redis allocations, and local data paths use the complete identity. Display-name prefixes never authorize cleanup. Read resource names and connection details from devflow output.

## Code and data origins

```bash
devflow --json --non-interactive switch -c agent/fix \
  --from main --data-from sanitized-baseline
```

`--from` selects the code parent. `--data-from` independently selects an existing data workspace; it defaults to the code parent. Both origins are recorded, and the data origin cannot change when retrying an existing workspace. Reset uses its recorded data parent. Local PostgreSQL creates an empty database when no data parent is supplied; it no longer selects an arbitrary existing workspace.

Service capabilities expose `data_strategy`:

| Strategy | Providers | Child data |
| --- | --- | --- |
| `physical_copy` | Local PostgreSQL, MySQL, ClickHouse | A consistent copy of the parent's directory; copy-on-write when supported |
| `template_copy` | Shared PostgreSQL | PostgreSQL database template copy |
| `remote_clone` | Optional Neon, Xata, DBLab adapters | Provider-managed branch or clone |
| `empty` | Shared Redis, ClickHouse, RustFS; generic containers | New empty resource |

Shared services use logical isolation within one engine. They do not provide separate authentication or a security boundary between mutually untrusted agents. Use separate containers or an external sandbox when that boundary is needed.

## Existing installations

Existing data is preserved. Resources without proven ownership are refused when they would otherwise be adopted or replaced. Do not delete ownership state to get past this check.

For shared resources, verify the old resource, then map the **service key** from command output to that exact database, bucket, or Redis allocation field:

```yaml
services:
  - name: app-db
    type: shared
    shared:
      legacy_resources:
        main: old_project_main
```

Creating that service workspace records the explicit adoption. A conflicting claim from another project or service is rejected. Redis mappings refer to the old allocation field, not its numeric database index.

Local PostgreSQL reports an existing project's UUID when adoption is required. Verify its data and set `local.legacy_project_id` for the owning service. Each old project can be adopted by one service only.

Old MySQL and ClickHouse directories lacked project ownership. They are intentionally not moved automatically. Export the old database with the engine's backup tool, configure a new absolute `data_root`, create the new workspace, and restore the export using the engine's client. Retain the old directory until the restored data has been verified.

Generic Docker containers also use complete project/service/workspace identities. Old containers are preserved and block replacement until their data has been verified and migrated. Explicit generic `volumes` entries retain their configured sharing behavior; separate containers do not make a shared bind mount or named volume private.

Project identity normally follows the canonical primary checkout path. To keep it stable across moves, set a unique `project_id` **before provisioning** and keep it unchanged. Adding or changing this value after provisioning selects a different owner and requires deliberate migration. Two projects must not share this value unless they intentionally represent the same data owner.

Keep devflow's configuration/state directory with its data. Shared ownership records are local to that directory; multiple controllers of the same Docker daemon must use the same ownership registry.

## Recovering setup

Switch and removal results include an `operation` with an ID, phase, status, origins, and worktree path. Retry the same command to resume incomplete setup. `post-create` remains pending until it completes. `devflow doctor` reports unfinished workspace operations.

`status: ok` means the command completed its automation contract. Check `operation.status: ready` before treating the environment as ready: unapproved hooks still allow workspace materialization but leave setup needing attention. Dependent hooks, processes, and JSON-mode `switch -x` execution are skipped while prerequisites are incomplete. Their results explain the reason.

Failed seeds return an error and remain failed until a successful seed retry. SQL restores stop on the first error and run in a transaction. Transfers stream instead of buffering entire dumps. Stop/start does not turn failed provisioning into successful provisioning.

Physical copies hold a cross-process parent lease. A failed copy or cancelled caller restores a previously running parent. Recovery intent survives process termination and is checked on the next clone attempt. Local filesystem copies stage new data before replacing the target; failed copies preserve the old directory.

Shared inventory and connection inspection validate an existing engine's image, credentials, mounts, and published ports without starting it. To start an engine, use a lifecycle command such as `service up`. A settings mismatch requires matching configuration or a separate container name.

## Smaller builds and explicit integrations

The default CLI includes local services, the native process supervisor, and the TUI. `cargo build --no-default-features` builds the core CLI. Add `proxy`, `service-neon`, `service-xata`, or `service-dblab` as needed; `--all-features` enables all integrations. Cloud adapters have local HTTP contract tests; validate credentials and behavior against your account before relying on them.

Commit settings are accepted only for migration diagnostics and omitted when saving normalized configuration. Use your VCS and preferred editor or agent directly.

`processes.start_policy: config` starts configured processes, including the first child of an idle parent. Set `inherit_parent` to copy the parent's selected process set.

Worktrees no longer implicitly run `mise trust`. Configure an approved lifecycle hook if needed. Broad ignored-file and AI-settings copying is deprecated and defaults off. Use explicit `worktree.copy_files` entries and fingerprinted caches. Tracked guide files and explicit guide generation remain available.

The desktop config editor uses the actual YAML/TOML file discovered by core. Source edits preserve comments and use validation plus concurrent-edit protection. Effective service overrides are never automatically saved into the shared document.

## Interrupted ZFS replacement

ZFS reset prepares a clone, persists cutover intent, and retains the old dataset until the replacement is mounted and committed. Retrying an interrupted operation uses that intent to restore the old generation before another attempt. If dependent clones prevent reclaiming an old generation, its exact recovery record is retained and reported by `service doctor`; inspect those dependencies before manual reclamation. Diagnostics never destroy datasets.

Linux CI injects failures on both sides of the snapshot/clone/rename/mount sequence against a disposable ZFS pool. The recovery model tests also run on other platforms.

## Preview and repair

```sh
devflow --json plan agent/fix --from main --data-from baseline:clean@2
devflow --json operation list
devflow --json operation inspect OPERATION_ID
devflow --json --non-interactive operation resume OPERATION_ID
devflow --json operation cancel OPERATION_ID
```

The plan contains the code revision, exact data origins, creation path, payloads, cache inputs, hooks and protections. It creates no worktree or database. Dirty jj sources report that an explicit snapshot is required. Switch records its resolved plan, and retries keep its selected origins.

Cancellation is restricted to operations proven to have made no resource changes. Rejected removals are cancelled automatically and do not block later execution. Partial cleanup retains its receipts and must be resumed. The desktop exposes pending operation details, resume/cancel actions and sealed baseline selection through the same core API.

Recreating a fully deleted workspace starts a new resource generation and can select another baseline. Removing only its code with `--keep-services` retains the original data binding and baseline protection. PostgreSQL descendants inherit an immutable engine image ID through every ancestor; incompatible existing containers are rejected before reuse.

Native jj preparation recovers stale workspaces while preserving local edits and conflicts. Inspection preserves Git staging and active operations. Failed Git or jj directory cleanup keeps a receipt so it can resume even after some workspace metadata has already been removed.

Git hook installation honors `core.hooksPath` and preserves hooks owned by other tools. Integrate `devflow git-hook` in an existing checkout hook and `devflow hook run pre-commit` in a pre-commit hook, or move the existing hook before installing devflow's managed scripts. Uninstall removes only recognized devflow scripts.
