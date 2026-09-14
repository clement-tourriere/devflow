# Subprocess audit

Reviewed commit: `37a3a9a63ec561653504771603d372d2bc5a6b1b`.

Follow-up: [native implementation and remaining boundaries](native-subprocess-reduction.md). This document and its 57-site inventory describe the original reviewed commit.

**Most internal command wrappers can be removed. A completely subprocess-free product would require removing several execution features.** The recommended boundary is native library/API calls for workspace and data management, with a small, explicit execution component for commands users actually ask devflow to run.

The [inventory](subprocess-inventory.json) identifies **57 production process-construction sites**, including mutually exclusive platform and feature branches. This is a source count, not 57 processes launched by one command. It includes the terminal's PTY command builder and excludes test modules, Python tooling, threads, async tasks, and Docker container exec requests. Rust `tokio::spawn` and thread workers are not subprocesses.

The Python `children.append(subprocess.Popen(...))` that prompted this audit belonged to a disposable review probe. That newly added harness has been removed from the proposed changes; its observed results remain in [the evidence document](architecture-pass-3-results.json). Previously committed review scripts, the benchmark, and the provider-document generator still use Python subprocesses. They are listed below as tooling to consolidate, not silently presented as already removed.

## Native replacements

| Current use | Source sites | Recommendation and acceptance requirement |
| --- | ---: | --- |
| Copying data, worktree payloads, and probing CoW with `cp` | 9 | Replace with one native filesystem module using the existing `reflink-copy` dependency, native filesystem APIs, and ordinary copies when appropriate. Preserve symlinks, permissions, directory metadata, failure recovery, and explicit storage mode semantics. Exercise copies with `cp` unavailable; measure large dependency trees because the current bulk-copy path was introduced for performance. |
| Looking up tools by launching `which` | 2 | Use the already-used native executable lookup approach. The core already depends on `which`; proxy can use the same library rather than invoking the executable. Tool lookup should never need another tool. |
| Minimal-build `docker exec` fallback | 1 | Remove the CLI fallback. Use the existing Bollard implementation when Docker support is compiled, and return a clear unsupported-feature error otherwise. Include this capability in the manifest. |
| Port-owner discovery through `lsof` | 1 | Use native process/socket inspection if port ownership remains necessary. Alternatively remove adoption of arbitrary existing listeners and track only processes devflow launched. Preserve ordinary socket availability checks. Never replace verified ownership with guessing a PID. |
| macOS login-shell invocation to obtain desktop `PATH` | 1 | Replace implicit shell startup with inherited environment plus explicit tool-path configuration. Shell version managers require a documented configuration/import path; hard-coded guesses are not an equivalent replacement. |
| Desktop notification commands (`osascript`, `notify-send`, PowerShell) | 3 platform alternatives | Remove the generic OS-notification hook action and report lifecycle events through CLI output and the desktop's notification adapter, or implement native OS APIs. The existing hook reports success even when notification delivery fails. |

Relevant implementations: [general data copy](../../../crates/devflow-core/src/services/mod.rs#L214), [PostgreSQL local copy](../../../crates/devflow-core/src/services/postgres/local/storage/local_driver.rs#L135), [worktree copy](../../../crates/devflow-core/src/workspace/worktree.rs#L336), [CoW probes](../../../crates/devflow-core/src/vcs/cow_worktree.rs#L78), [Docker hook](../../../crates/devflow-core/src/hooks/actions/docker_exec.rs#L108), [port lookup](../../../crates/devflow-core/src/processes/mod.rs#L2571), [desktop PATH](../../../src-tauri/src/main.rs#L66), and [notifications](../../../crates/devflow-core/src/hooks/actions/notify.rs#L34).

The copy consolidation is particularly valuable: the existing local PostgreSQL staging path also has a failure-safety defect described as R8 in [the architecture review](architecture-pass-3.md). Replacing only the `Command::new` expression would leave that defect intact.

## Features whose removal would eliminate wrappers

| Feature | Current process boundary | Recommendation |
| --- | --- | --- |
| `devflow commit` and AI commit message generation | Git CLI, editor, configurable shell command | Retire this convenience feature. Users and agents already have their VCS and preferred commit-generation tools. Replacing `git commit` with `git2` here would change signing and hook behavior, which is precisely why the current implementation invokes Git. |
| Detached tmux/zellij sessions | Shell template that launches a multiplexer | Retire devflow's multiplexer/session management if reducing subprocess responsibility is a priority. Let the agent harness or user's terminal own sessions; retain explicit foreground execution only if desired. |
| Automatic privileged machine setup | `sudo`/`pkexec`, ZFS setup, trust-store commands | Remove automatic installation/configuration and provide diagnostics plus explicit external setup instructions. This reduces both subprocesses and approval friction. The filesystem/provider still needs its own operational API. |
| Linux proxy mDNS publishing | One `avahi-publish` child per advertised entry | Prefer loopback-only proxy names and remove automatic LAN advertisement, or implement a native discovery adapter if LAN access is a real requirement. macOS already has a native mDNS path. |
| CLI self-update | Executes the downloaded binary to validate it | If self-update stays, its execution check has a purpose and should not be removed just to lower a count. Otherwise retire self-update and use package distribution. |
| Legacy Pitchfork record cleanup | Exact external supervisor command | Keep only as a temporary, explicit migration tool, or replace with manual migration instructions. Do not silently remove ownership records for a process that might still run. |

The proxy companion deserves reconsideration under this preference. Its [entry point](../../../src/proxy_main.rs#L1) currently includes the entire CLI, while the desktop invokes it for lifecycle commands. **Status and target queries already use HTTP**; they do not spawn the companion. There are two coherent options: an independently started optional proxy with an HTTP control API, or an optional in-process proxy library owned by one frontend. Re-executing the whole CLI for companion management is unnecessary coupling. The previous extraction should not be expanded in that form.

## Boundaries that need a deliberate product choice

- **Jujutsu:** the current adapter has eight `jj` command-construction sites. Git worktree management already uses `git2`; its only Git CLI call is the commit wrapper. To keep a process-free workspace backend, make Git the supported default and either remove jj or implement and validate an embedded jj adapter. Merely replacing command wrappers with another shell layer does not help.
- **ZFS:** runtime operations and installation/probing have separate command wrappers. Removing the installer is straightforward. Keeping ZFS operations without subprocesses requires native bindings and real Linux recovery tests; that is a significant platform commitment. Keep ZFS optional while evaluating that work, or remove it and retain native APFS/reflink/copy storage.
- **Executable provider plugins:** the protocol is defined around launching a program for each request. It cannot become subprocess-free without changing that extension model. Prefer built-in providers for the supported path; retire executable plugins if they are not a real requirement. An HTTP adapter would still rely on an externally managed service and should be described honestly.
- **User execution:** `exec`, shell hooks, shell conditions, managed app processes, and PTY terminals inherently launch external programs. Keeping these capabilities means retaining an explicit process boundary. They can share typed program/argument requests, cancellation, output handling, and completion records; they cannot be made in-process simply by refactoring Rust.
- **Container execution:** PostgreSQL tools such as `psql`, `pg_dump`, and `pg_restore` already run through Docker's API rather than the host Docker CLI. They still execute programs inside containers. Native SQL clients can cover some operations, but removing host subprocess wrappers is not a claim that database tools stop existing.

## Tooling cleanup

Move maintained performance measurements and provider-descriptor generation into Rust code that calls core APIs. The Python benchmark currently launches the CLI for every operation; the descriptor generator launches `capabilities`. Core benchmarks can call `baseline`, `switch_workspace`, and `service_operation` directly, while a Rust generator can serialize `services::descriptor::providers()` without launching devflow.

Keep a small CLI boundary test suite only if testing the real executable remains desired: output framing and exit-code behavior cannot be established solely through library calls. Existing Docker fixture cleanup can use Bollard, and Git fixture setup can use `git2`. Historical review evidence can remain as data and written reproduction steps rather than a growing collection of maintained process-launching scripts.

## Recommended order

1. Consolidate native copying and CoW probing, fixing R8 and preserving metadata, rollback, and performance. Replace `which` and remove the Docker CLI fallback.
2. Remove the commit wrapper, obsolete desktop handlers, unnecessary process-runtime forwarding, automatic privileged setup, and automatic proxy LAN publishing.
3. Centralize intentional user-command execution; make program/argv the default, with shell execution explicit. Remove multiplexer ownership and implicit desktop shell startup where their replacement is documented.
4. Decide whether jj, ZFS, executable plugins, and embedded terminals justify their process boundaries. Keep them outside the minimal workspace/data build in the meantime.
5. Add a source-boundary check preventing new internal command wrappers, with the explicit execution/adapters as the only exceptions. Use native core tests for ordinary behavior and a narrowly scoped executable test suite for the CLI contract.

This audit proposes these changes; it does not claim they have been implemented. Production source remains at the reviewed commit.
