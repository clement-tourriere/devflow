---
title: Installation
description: Install devflow with the install script or from source, update it with the same installer, and verify your system with devflow doctor.
sidebar:
  order: 1
---

## Requirements

- **Docker** or a compatible container runtime (OrbStack, Colima, …) for local services
- **Rust toolchain** only if building from source
- Git and Jujutsu repository support is embedded; the jj adapter uses **jj-lib 0.45.1**. Devflow creates unsigned workspace snapshots; use jj directly for signed commits.
- Optional: **bun** + Tauri prerequisites if you want to develop the desktop GUI

## Install script (recommended)

```bash
curl -fsSL https://raw.githubusercontent.com/clement-tourriere/devflow/main/scripts/install.sh | sh
```

The script downloads the latest [GitHub release](https://github.com/clement-tourriere/devflow/releases) binary for your platform, verifies its SHA-256 checksum, and installs it to `~/.local/bin`.

Supported platforms: Linux (x86_64, arm64) and macOS (Apple Silicon). On anything else, [install from source](#install-from-source).

Environment variables to customize the install:

| Variable | Default | Purpose |
| --- | --- | --- |
| `DEVFLOW_INSTALL_DIR` | `~/.local/bin` | Where the binary is installed |
| `DEVFLOW_VERSION` | `latest` | Pin a specific release tag (e.g. `v0.5.0`) |

## Install from source

```bash
git clone https://github.com/clement-tourriere/devflow.git
cd devflow
cargo install --path .
```

## Updating

Rerun the install script to install the latest verified release, or use your package manager. For source builds, run `cargo install --path . --force` with your chosen features. The self-update command has been retired.

## Verify

```bash
devflow --version
devflow doctor
```

`devflow doctor` checks Docker connectivity and your repository through the embedded Git/Jujutsu libraries, validates configuration, and reports which Copy-on-Write storage method your filesystem supports (APFS clones on macOS, ZFS/Btrfs/XFS reflinks on Linux, full-copy fallback elsewhere).

## Copy-on-Write storage

devflow auto-detects the best storage method available — no setup needed on macOS (APFS) or Btrfs/XFS:

| Filesystem | Platform | CoW method | Setup required |
| --- | --- | --- | --- |
| APFS | macOS | Native `clonefile` | None (automatic) |
| ZFS | Linux | Snapshots + clones | External ZFS provisioning |
| Btrfs | Linux | Reflink copy | None |
| XFS | Linux | Reflink copy | None (if created with reflink support) |
| ext4 / other | Any | Full copy (fallback) | None — works, just slower |

ZFS pools and permission delegation are managed outside devflow. Configure `local.data_root` to point at an existing dataset. Runtime ZFS operations use the system OpenZFS 2.x `libzfs` library; APFS, reflink, and ordinary copies use native filesystem libraries. The Git, jj, `zfs`, and `cp` executables are not needed for workspace and data operations. Install the system OpenZFS runtime and load its kernel module before using devflow. Leave `ZFS_MOUNT_HELPER` unset or `0`, and keep data datasets unshared (`sharenfs=off`, `sharesmb=off`). Native operations do not load modules or invoke mount/sharing helpers.

For example, after an administrator has created a `devflow` dataset mounted at `/var/lib/devflow/data`, delegate the operations needed for workspace snapshots and replacements:

```bash
sudo zfs allow "$USER" create,destroy,snapshot,clone,mount,mountpoint,promote,rename,rollback devflow
```

Set `local.data_root: /var/lib/devflow/data` and `local.storage: zfs` in the service configuration. The dataset and its mountpoint must be writable by the user running devflow.

Desktop launches inherit their environment. Set `DEVFLOW_TOOL_PATH` to an explicit platform-separated list of absolute tool directories when launching the desktop from a Dock or menu; devflow prepends these paths without running a login shell. Absolute executable paths in approved hooks are another option.

## Next steps

- [Quickstart](/devflow/getting-started/quickstart/) — initialize a project and create your first workspace
- [Shell integration](/devflow/getting-started/shell-integration/) — enable auto-`cd` into worktrees
