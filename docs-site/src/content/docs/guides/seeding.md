---
title: Seeding data
description: Seed workspaces from PostgreSQL URLs, local dump files, or S3 backups.
sidebar:
  order: 4
---

Seed your workspaces with data from production dumps, live databases, or S3 backups — at service setup time or any time after.

## Seed sources

| Source | Form | Notes |
| --- | --- | --- |
| PostgreSQL URL | `postgresql://user:pass@host:5432/db` | live `pg_dump` from a running server |
| Local file | `./dump.sql`, `./backup.dump` | `.sql` via `psql`, other extensions via `pg_restore` (custom format) |
| S3 object | `s3://bucket/path/dump.sql` | credentials/region from standard AWS env vars |

## Seeding at service setup

```bash
devflow service add app-db --provider local --service-type postgres --from ./backup.sql
devflow service add app-db --provider local --service-type postgres \
  --from postgresql://readonly:pass@replica:5432/mydb
devflow service add app-db --provider local --service-type postgres \
  --from s3://my-bucket/backups/latest.dump
```

## Seeding an existing workspace

```bash
devflow service seed main --from dump.sql
devflow service seed feature/auth --from postgresql://readonly:pass@replica:5432/mydb
devflow service seed main --from s3://my-bucket/backups/latest.dump
devflow service seed main -s app-db --from dump.sql      # specific service
```

:::tip
Seed **main** once, then let branching do the rest — every new workspace clones main's data via CoW (or Postgres `TEMPLATE` for shared engines). Re-seed individual workspaces only when they need different data.
:::

## How it works

**From a PostgreSQL URL** — devflow runs `pg_dump` in an ephemeral Docker container against the source, downloads the dump to a temp file, and restores it into the target container with `pg_restore`. `localhost` URLs are rewritten to `host.docker.internal` automatically so the dump container can reach databases running on your host.

**From a local file** — `.sql` is piped through `psql`; any other extension is treated as custom format and restored with `pg_restore`.

**From S3** — the object is downloaded using standard AWS credentials (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_DEFAULT_REGION`/`AWS_REGION`), then restored like a local file.

## Resetting instead of re-seeding

For "give me a clean copy of the parent again", prefer:

```bash
devflow service reset feature/auth
```

It re-clones from the parent state — faster than re-restoring a dump, and exactly what agent retry loops want.
