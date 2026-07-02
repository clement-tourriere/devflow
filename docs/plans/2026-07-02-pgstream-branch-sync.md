# pgstream branch sync — design notes (deferred)

Status: **design only — implementation deliberately deferred** (2026-07-02).
The continuous child-sync mode has sharp edges (see Risks); nothing here
should ship until the staged plan's step-1 experience validates the tooling.

## Problem

A branched workspace database is a point-in-time copy. Two drift vectors
appear immediately after creation:

1. **Parent moves forward** — the parent branch gains migrations or fresh
   data; existing child workspaces keep serving stale schema/data.
2. **The branching point itself goes stale** — devflow's main workspace DB
   is seeded once (`devflow service seed main --from <url>`) and then drifts
   from the authoritative dev database it was seeded from. Observed on
   ward-runs-app: the seeded main DB was already behind the branch's
   migration files on day one; a post-create `manage.py migrate` hook papers
   over schema drift but not data drift.

Question: can [xataio/pgstream](https://github.com/xataio/pgstream) keep
devflow postgres workspaces synchronized from their parent(s) automatically?

## pgstream facts (verified 2026-07-02, v1.1.0)

- Go CLI/library, Apache-2.0, from Xata (devflow already ships a Xata
  provider). Distributed as binary, brew, or docker image.
- **pg→pg replication is first-class** (`pg2pg` docker profile, dedicated
  tutorials). Built on logical replication slots + the `wal2json` plugin
  (the only postgres output plugin supported today).
- **Replicates DDL** — its differentiator vs vanilla logical replication.
  Event triggers emit schema changes as logical messages into the WAL
  stream, so schema updates arrive in-order with the data that depends on
  them.
- **Snapshot-then-replicate bootstrap**: records an LSN, snapshots
  (pg_dump/pg_restore for schema, parallel `ctid`-range reads for data),
  then streams from the recorded LSN. Snapshot also runs standalone with no
  init/state on the source.
- **Transformers**: column-level value transformation (anonymization) in
  flight — useful when a parent is seeded from production-like data.
- Source requirements: `wal_level=logical`, `wal2json` installed,
  superuser-equivalent for `pgstream init`, and **every replicated table
  needs a PK or unique not-null column**.
- Constraints: one sequential listener per replication slot (single
  process per link); no row-level filtering; conflict handling on the
  target is effectively undocumented — the writer assumes the target is
  only written by the stream.

## Semantic model — the part that makes this dangerous

pgstream is **one-directional CDC, not a merge engine**. The moment a child
workspace is written locally (test rows, branch migrations), it diverges
from the parent; continuing to apply parent changes onto a diverged child
eventually collides: PK conflicts on inserts, DDL conflicts with branch
migrations touching the same tables, FK violations from partial application.
There is no upstream answer for this — any "follow the parent" feature is
only sound while the child is read-mostly relative to the replicated tables.

Consequences for the design:

- Continuous sync is safe for **follower** targets (nobody writes them
  locally) and *opt-in only* for real workspaces.
- On the first conflict the stream must **stop hard and surface** in
  `devflow process status` / GUI — never skip-and-continue, which would
  leave a silently half-applied database (worse than stale).
- Re-sync of a diverged child is not a pgstream job at all: with CoW,
  destroy + re-branch is already near-instant and has honest semantics
  (local changes are explicitly discarded).

## Where it fits devflow

Three integration tiers, in ascending risk:

### Tier 1 — pgstream as a seed engine (no WAL requirements)

`devflow service seed --engine pgstream` (or `seed.provider: pgstream`)
using standalone snapshot mode instead of pg_dump/psql:

- Parallel `ctid`-range reads outperform pg_dump on larger DBs.
- Transformers give seed-time anonymization for free.
- Zero source-side requirements (no `wal_level` change, no init, no slot).
- Failure mode is identical to today's seed (all-or-nothing into a fresh
  workspace DB) — the safe on-ramp to owning the pgstream binary,
  config rendering, and process supervision.

### Tier 2 — follower mode for the branching point

`main ← (pgstream follow) ← authoritative dev DB` (e.g. the project's
compose postgres, or a staging replica). devflow main becomes an
always-fresh branching point; `seed` becomes a one-time bootstrap; every
new workspace CoW-clones current data.

- Sound because devflow main's DB is read-mostly by construction (local
  migrations against main become unnecessary — the parent's migrations
  flow through the stream).
- Source must be reconfigured: ward-runs-app's compose postgres currently
  runs `wal_level=minimal, max_wal_senders=0` (dev-speed tuning) and the
  stock `pgvector/pgvector` image ships no `wal2json`. Needs a compose
  override + image layer — a per-project opt-in cost.

### Tier 3 — opt-in continuous parent→child follow

```yaml
services:
  - name: app-db
    type: local
    service_type: postgres
    sync:
      provider: pgstream
      from: parent            # or "main", or a postgres:// URL
      mode: follow            # follow | snapshot-on-create | manual
      on_conflict: stop       # stop (only sane default)
```

- One pgstream process per link, supervised by the controller daemon
  (reconcile/retry/status already exist there). pgstream is Go — unlike
  pitchfork it cannot be embedded; it runs as a managed subprocess or
  sidecar container per link.
- One replication slot per child on the parent. Slot lifecycle is owned by
  workspace lifecycle: `devflow remove` must drop the slot — an orphaned
  slot pins WAL retention and eats the parent's disk unboundedly.
  `max_replication_slots` / `max_wal_senders` sized from
  `behavior.max_workspaces`.
- Divergence policy: stop-and-surface (above). A `devflow service sync
  --status` view showing lag + stopped-on-conflict state is part of the MVP.

## CoW bootstrap trick (needs validation)

pgstream's own bootstrap does a logical snapshot, but devflow can do
better for `type: local` workspaces: create the replication slot on the
parent (which pins a consistent LSN), CoW-clone the data directory
immediately after, then start pgstream streaming from the slot into the
clone. The instant-clone property is preserved and pgstream only carries
the delta.

Open question to validate before Tier 3: whether pgstream can start from
an externally-satisfied snapshot ("target already matches slot LSN — skip
snapshot, stream only"). If its resume path can't express this, either
contribute the option upstream or fall back to letting pgstream logical-
snapshot (losing the CoW advantage for synced workspaces only).

Also note the cloned data dir carries the *parent's* replication state
(slots/origin); the clone must have inherited slots dropped on first boot
before its own stream attaches.

## Risks (why this is deferred)

- **Silent divergence corruption** if conflict handling is anything other
  than stop-hard. pgstream's writer behavior on conflicts is undocumented;
  must be tested empirically before any child-follow ships.
- **Orphaned replication slots** on parents → unbounded WAL growth. Slot
  teardown must be transactional with workspace removal, including the
  crash/kill paths (`devflow project destroy`, GUI deletes).
- **Source-side config drift**: Tier 2/3 require `wal_level=logical` +
  `wal2json` on sources devflow does not own (compose files, remote DBs).
- **Tables without PKs are silently ineligible** — needs a preflight check
  with a clear report, or replication quietly misses tables.
- **One more supervised binary** per link: version pinning, upgrade path,
  and macOS/Linux distribution need the same treatment as the CoW helper
  binaries.

## Staged plan

1. **Seed engine** (Tier 1). Go/no-go: seed of ward-runs-app's 128 MB DB
   with a transformer applied, faster or equal to pg_dump path, zero
   source changes.
2. **Main follower** (Tier 2) behind a `sync:` config flag, only for the
   main workspace, only from an explicit URL. Go/no-go: a week of compose→
   main streaming on ward-runs-app with migrations flowing through and
   zero manual intervention.
3. **Child follow** (Tier 3) — only after 1+2, with conflict-stop
   semantics, slot lifecycle wired into remove/destroy, and the CoW-LSN
   bootstrap validated or explicitly abandoned.

## Alternatives considered

- **Vanilla logical replication (pub/sub)** — no DDL replication; parent
  migrations would break subscriptions constantly. Rejected.
- **Periodic re-seed / re-clone** — already possible today; honest
  semantics but discards local changes and closes connections. Remains the
  recommended answer for diverged children regardless of pgstream.
- **`migrate` post-create hook** (shipped on ward-runs-app 2026-07-02) —
  covers schema drift at creation time; does nothing for data drift or
  already-created workspaces. Complementary, not competing.
