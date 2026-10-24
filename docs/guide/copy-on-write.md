# Copy-on-write, measured

devflow never assumes that a clone shares blocks: it measures where it can, takes your word where it cannot (and says so), and reports `verified`, `copy` or `unknown`. `devflow doctor` shows the capability it found and whether it was measured or declared; `devflow service status` shows it for the container services (`--probe` measures again instead of using the cached result); `require_cow = true` on a service refuses a clone that would be a full copy. The numbers below are measurements, not promises.

Branch creation speed and disk cost depend on the filesystem under the data (for PostgreSQL, the data directory; for the container services, the Docker engine's volumes).

Benchmark (OrbStack, btrfs volume, PostgreSQL 18): a 392 MB database cloned in **0.93 s** and used **+0.2 MB** on disk. The default `WAL_LOG` strategy used +370 MB on the same database. `FILE_COPY` forces a checkpoint, so latency also depends on server activity. Your numbers will differ.

| PostgreSQL data directory on | Result |
| --- | --- |
| Managed server on OrbStack (named volume on btrfs) | `verified` |
| Linux btrfs, XFS with reflink, or ZFS 2.2+ | `verified` |
| ext4 | `copy`: a full copy per branch |
| Docker Desktop, or a macOS bind mount (virtiofs) | `copy`: a full copy per branch |
| External server | `unknown`, unless you declare `cow = "verified"` in `devflow.local.toml` |

Valkey/Redis branches (OrbStack, Docker volumes on btrfs, `valkey/valkey:8-alpine`): a dataset of 32,000 keys of 4 KiB (about 128 MiB, incompressible) cloned in **2.4 s end to end** (coordinator start, `BGSAVE` of the source, reflink of `dump.rdb`, container create and start, `AUTH`/`PING`) and took **+32 KiB** in the Docker VM (guest `df` before and after, an estimate). The source answered every write during the clone (no pause). Your numbers will differ. A source with nothing new since its last save is cloned from that save, with no `BGSAVE`: otherwise each new save rewrites the whole RDB while earlier clones still share the old one, which cost a full RDB (about +115 MiB here) per clone of an unchanged source before this was added.

MySQL branches (same engine, `mysql:8.4`, 64 MiB buffer pool): a data directory of 436 MiB (a 236 MiB table of 200,000 rows of about 1 KiB, plus InnoDB's redo, undo and doublewrite files) cloned in **6.8 s end to end**, with the source **stopped for 3.0 s** (clean shutdown, reflink, start, first authenticated answer); the copy itself took **+164 KiB** in the Docker VM and the whole operation **+113 MiB** (guest `df` before and after: the new server's own files, such as its temporary tablespaces, and the pages a running server rewrites). A snapshot of the same directory took 3.3 s with the source stopped for 3.1 s. An empty, idle MySQL branch holds about 200 MiB of the VM's disk (the initialized data directory and a running server) and about 125 MiB of memory. Creating the empty root branch takes 7-13 s (initialization, bootstrap, first start). These were measured on a busy machine: under a load average of about 40 the same clone took 15-26 s with the source stopped for 9-15 s, so the downtime tracks the host's I/O, and it grows with the dirty pages InnoDB flushes at shutdown.

A whole workspace, end to end (`devflow switch -c`, release build, Apple silicon, OrbStack): a project with a 324 MiB `node_modules`-like payload of 20,000 files, a managed PostgreSQL 18 server holding a 231 MB table and a Valkey cache of 32,000 keys of 4 KiB, with one setup hook.

| `switch -c` | Time | Disk added |
| --- | --- | --- |
| Warm (source server and cache running) | **5.1–5.4 s** (payload 1.5 s, both data clones about 3.5 s, env and hooks under 30 ms) | **+0.3 MiB** in the Docker VM for the database and the cache; **+5.8 MiB** on APFS for the payload (file metadata; measured on a dedicated volume, since the system volume's free space moves by more than that on its own) |
| Cold (source containers stopped) | **10.1 s** (the server's start and health check included) | same |

The first clone after the cache was loaded also wrote its first save (+120 MiB): that is the source's own data reaching disk, not a cost of the clone. Your numbers will differ with the filesystem, the dataset and the hooks.

On a filesystem that cannot share blocks, `FILE_COPY` is the slower choice for a small database: it forces a checkpoint and copies the files, where `WAL_LOG` does not (pytest-postgresql measured 6.7 s against 0.47 s on ext4 for a small database; [issue 1260](https://github.com/dbfixtures/pytest-postgresql/issues/1260)). devflow therefore uses `WAL_LOG` for a source under `wal_log_below` (default 256 MiB) unless sharing is `verified`, and `FILE_COPY` otherwise, and records which one each branch got.

Worktree payloads are cloned with `clonefile` on APFS and `FICLONE` on btrfs, XFS with reflink and ZFS 2.2+; on other filesystems they are copied.

`devflow doctor` reports the capability it found and whether it was measured or declared. Set `require_cow = true` under `[services.db]` to refuse branch creation when CoW is not verified. Without CoW, branches still work; they take longer and use the full database size.
