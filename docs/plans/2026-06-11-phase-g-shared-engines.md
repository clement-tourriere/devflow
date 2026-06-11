# Phase G — Shared global engines + logical isolation (the vision)

Goal: devflow as a local controller that keeps **one global, optimized container per engine** (Postgres, Redis, RustFS, ClickHouse) and provisions **project/workspace-scoped logical boundaries on the fly** (`CREATE DATABASE project_ws`, a RustFS bucket, a Redis DB index), instead of one physical container per workspace. Coexists with the current CoW `local` providers as a per-service choice.

**Object storage = RustFS** (Rust-native, S3-compatible, Apache-2.0) — not MinIO.

## Architecture fit (verified against current code)

- New provider selected by `type: shared` on a `service_type: postgres` (and later `redis`, `rustfs`, `clickhouse`) entry → `ProviderType::Shared` dispatched in `services/factory.rs::create_postgres_provider`.
- Implements the existing `ServiceProvider` trait (`services/mod.rs`) — most methods have defaults, so a shared provider only implements create/delete/list/exists/switch/connection_info/doctor/test_connection.
- **Source of truth for logical resources is the engine itself** (`pg_database` for postgres, bucket list for RustFS, allocated indices for Redis) — no new SQLite tables needed for the postgres cut. This sidesteps the migration/refcount complexity for v1; a dedicated registry can come later if cross-engine GC needs it.
- Global container is **never per-workspace**: `stop/reset/destroy_project` operate on logical resources only and leave the shared engine running for other projects.
- Reuses: `local_docker::sanitize_name_component`, `inspect_container_status`, `pick_available_port`; bollard exec pattern from `postgres/local/docker.rs`; the `rust-s3` crate already in-tree (for RustFS).
- Identity: project name comes from the now-unified `config.project_name()` (Phase F), so a worktree and its main repo share logical databases/buckets — exactly right for the vision.

## Config surface

```yaml
services:
  - name: app-db
    type: shared            # NEW — logical isolation in a global container
    service_type: postgres
    auto_workspace: true
    shared:
      image: postgres:17        # default: postgres:17
      port: 5432                # fixed well-known port (default 5432)
      container_name: ...       # default: devflow-shared-postgres
      user: postgres            # default: postgres
      password: postgres        # default: postgres
      template_branching: true  # CREATE DATABASE ... TEMPLATE parent (default true)
```

## Work breakdown & status

- **G1 — SharedPostgresProvider** (this session): `services/shared/` module.
  - `naming.rs` — pure, unit-tested: logical DB name = `sanitize(project)_sanitize(workspace)`, postgres-identifier-safe (lowercase, `_`, ≤63 bytes, no leading digit); SQL builders; project-prefix matching for list/GC.
  - `container.rs` — `ensure_global_container()` (pull image, create+start fixed-port container, 409-tolerant), `docker_exec_capture()` (run a command, capture stdout+exit), readiness wait.
  - `postgres.rs` — `SharedPostgresProvider` impl: create→`CREATE DATABASE [TEMPLATE parent]`, delete→terminate+`DROP DATABASE`, exists/list via `pg_database`, switch→ensure+info, connection_info, doctor, test_connection.
  - `config.rs` additions: `SharedConfig`, `ProviderType::Shared`, dispatch wiring.
  - Unit tests for all pure logic (naming, SQL, prefix match, config defaults).
- **G2 — RustFSProvider** (next): global `rustfs/rustfs` container (S3 + console ports), bucket-per-workspace `{project}-{ws}` via `rust-s3`; connection info exposes endpoint/bucket/keys. (Verify the current `rustfs/rustfs` image env/ports at implementation time.)
- **G3 — SharedRedisProvider**: logical DB index allocation (0–15) or key-prefix mode; tracked in a small state file or derived.
- **G4 — devflow.toml**: parse TOML by extension in `Config::from_file`/`find_config_file` (add `toml` dep); minimal schema; `devflow init --toml`.
- **G5 — Controller daemon**: fold reconcile into the proxy process (already has Docker events + JSON API + pidfile); git-hook paths call the daemon when alive.
- **G6 — ClickHouse logical isolation**: `CREATE DATABASE` via existing exec plumbing (sidesteps the untested CoW path).

## Non-goals (v1)

- No new SQLite registry / refcounted engine GC (engine stays up; logical resources are the engine's own state).
- Global container is not auto-removed; `devflow` never tears down a shared engine implicitly.
- Concurrency: rely on `CREATE DATABASE IF NOT EXISTS`-style idempotency and Docker 409-tolerance; a cross-process lock can come with the daemon (G5).

## Status

| Step | Status |
|------|--------|
| G1 SharedPostgresProvider | ✅ done (2026-06-11) — `services/shared/{mod,container,naming}.rs`, `SharedServiceConfig` + `ProviderType::Shared` + factory dispatch, CLI `service add` menu entry, CLAUDE.md docs, 11 unit tests. `type: shared` postgres now keeps one `devflow-shared-postgres` container (named volume, unless-stopped) and provisions `CREATE DATABASE project_ws [TEMPLATE parent]` per workspace; delete = terminate+DROP; destroy = drop project's DBs only (container stays). 224 tests + clippy green. |
| G2 RustFS | ✅ done (2026-06-11) — `services/shared/rustfs.rs`, `RustFsProvider` for `service_type: rustfs` (also `s3`/`objectstorage`). One global `rustfs/rustfs:latest` container (S3 :9000 + console :9001, named volume, default creds rustfsadmin), bucket-per-workspace `{project}-{ws}` via in-tree `rust-s3` 0.37 (create/exists/list_buckets/delete + empty-on-delete). Verified image env/ports against RustFS docs (RUSTFS_ACCESS_KEY/SECRET_KEY, /data command arg). Container spec gained `cmd` + `extra_port`. S3-safe bucket naming + tests. 228 tests + clippy green. **Untested against a live RustFS daemon** — pure naming logic is unit-tested; the S3/container paths need an integration run. |
| G3 shared Redis | planned |
| G4 devflow.toml | ✅ done (2026-06-11) — `Config::from_file` parses by extension (TOML vs YAML); `find_config_file` discovers `.devflow.toml`/`devflow.toml`; added `toml` 0.8 dep; from_file round-trip test. **Read-only**: `devflow init` and the GUI still write YAML (TOML serialization of the untagged hook enums is the follow-up). |
| G5 daemon | planned |
| G6 ClickHouse logical | ✅ done (2026-06-11) — `services/shared/clickhouse.rs`, `SharedClickHouseProvider` selected by `service_type: clickhouse` + `type: shared`. One global `clickhouse/clickhouse-server` container (HTTP :8123 only — native :9000 omitted to avoid clashing with shared RustFS), `CREATE DATABASE` per workspace via `clickhouse-client` exec; list via `system.databases`; HTTP connection string. No TEMPLATE branching (ClickHouse lacks it). 230 tests + clippy green. **Untested against a live ClickHouse daemon.** |

**G1 follow-ups not done:** non-interactive `service add` hardcodes `provider_type="local"` (only the interactive menu offers `shared`); two `shared` postgres services in one project would collide on DB names (single global container, `service_name` retained but unused for now); no Docker integration test (pure logic is unit-tested; the exec/container paths need a live daemon).
