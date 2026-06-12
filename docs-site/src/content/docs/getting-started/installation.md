---
title: Installation
description: Install devflow with the install script or from source, keep it updated with devflow update, and verify your system with devflow doctor.
sidebar:
  order: 1
---

## Requirements

- **Docker** or a compatible container runtime (OrbStack, Colima, …) for local services
- **Rust toolchain** only if building from source
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

devflow can update itself in place — it downloads the new release binary, verifies its checksum, and atomically swaps the executable (rolling back if the new binary fails to run):

```bash
devflow update                   # Update to the latest release
devflow update --check           # Only check whether an update exists
devflow update --version 0.5.0   # Install a specific version
devflow update --force           # Reinstall the current version
```

If you installed from source, keep using `cargo install --path .` instead — `devflow update` would replace your custom build with the release binary.

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
