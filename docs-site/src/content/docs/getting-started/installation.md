---
title: Installation
description: Install devflow from source and verify your system with devflow doctor.
sidebar:
  order: 1
---

## Requirements

- **Rust toolchain** (for building from source)
- **Docker** or a compatible container runtime (OrbStack, Colima, …) for local services
- Optional: **bun** + Tauri prerequisites if you want to develop the desktop GUI

## Install from source

```bash
git clone https://github.com/clement-tourriere/devflow.git
cd devflow
cargo install --path .
```

Pre-built binaries are attached to [GitHub releases](https://github.com/clement-tourriere/devflow/releases) when available.

## Verify

```bash
devflow --version
devflow doctor
```

`devflow doctor` checks that Docker and your VCS (git/jj) are reachable, validates configuration, and reports which Copy-on-Write storage method your filesystem supports (APFS clones on macOS, ZFS/Btrfs/XFS reflinks on Linux, full-copy fallback elsewhere).

## Copy-on-Write storage

devflow auto-detects the best storage method available — no setup needed on macOS (APFS) or Btrfs/XFS:

| Filesystem | Platform | CoW method | Setup required |
| --- | --- | --- | --- |
| APFS | macOS | `cp -c` clone | None (automatic) |
| ZFS | Linux | Snapshots + clones | `devflow setup-zfs` |
| Btrfs | Linux | Reflink copy | None |
| XFS | Linux | Reflink copy | None (if created with reflink support) |
| ext4 / other | Any | Full copy (fallback) | None — works, just slower |

On Linux without a CoW filesystem, `devflow setup-zfs` creates a file-backed ZFS pool — no spare disk required. `devflow init` offers this automatically when ZFS tools are detected.

## Next steps

- [Quickstart](/devflow/getting-started/quickstart/) — initialize a project and create your first workspace
- [Shell integration](/devflow/getting-started/shell-integration/) — enable auto-`cd` into worktrees
