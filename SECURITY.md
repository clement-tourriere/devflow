# Security Policy

## Supported versions

| Version | Supported |
| --- | --- |
| 1.x | Yes |
| 0.x | No. Upgrade using [docs/MIGRATING.md](docs/MIGRATING.md). |

## Reporting a vulnerability

Report vulnerabilities privately:

1. **Do not** open a public GitHub issue.
2. **Email** clement.tourriere@gmail.com with the subject "devflow security".
3. Include a description, steps to reproduce, the potential impact, and a suggested fix if you have one.

You should receive an acknowledgment within 48 hours. The goal is a fix within 7 days of a confirmed vulnerability.

## Security model

### Hooks and trust

- `[hooks]` commands (`setup`, `switch`, `teardown`) are shell commands run with `sh -c` in the worktree. They can do anything your user can.
- devflow runs them only for content you approved. Trust is the SHA-256 of the `[hooks]` section after merging `devflow.local.toml` (commands and timeout), stored per project checkout in `trust.toml`. A changed section needs approval again. Review the commands that `devflow trust` shows.
- Trust covers the commands, not the scripts they invoke. A trusted `bun run db:migrate` runs whatever `package.json` says.
- In `--json` and `--non-interactive` modes, and when stdin or stderr is not a terminal, untrusted hooks are skipped and the workspace is marked `needs_attention`. `DEVFLOW_TRUST_HOOKS=1` (or `true`) trusts for one run; use it only for repositories whose hooks you have read.
- `teardown` hooks run on `devflow remove` and need the same trust: untrusted teardown hooks refuse the removal, and `--force` removes the workspace without running them.
- devflow is not a sandbox. Use your agent harness, containers or VMs when you need hard isolation.

### Database isolation

- Each project has one PostgreSQL 18 server. The **admin (provisioner)** role owns every devflow database and the `devflow` metadata database; it can read and change everything in the project. Never give it to a sandbox.
- Objects in all branch and snapshot databases of a project belong to one `NOLOGIN` **project owner** role that never holds `CONNECT`. Each branch has its own `LOGIN` role that can connect to its own database only. A branch role cannot connect to sibling branches or snapshots of the same or another project.
- New databases are created closed (`ALLOW_CONNECTIONS false`), have their ACLs normalized, and are opened to the branch login last. The `devflow` metadata database is closed the same way. Snapshots are not templates, so only the admin can clone them.
- Logins and owners have no elevated attributes (no superuser, `CREATEDB`, `CREATEROLE`, replication or `BYPASSRLS`) and no `ADMIN OPTION`. A session acting as the owner cannot cancel or terminate a sibling branch's sessions.
- On an external server devflow changes only databases it created. Restricting access to every other database on that server (`pg_hba.conf`, or revoking `PUBLIC` `CONNECT`) is the operator's responsibility. A managed server revokes `PUBLIC` `CONNECT` on `postgres` and `template1`.
- Replication machinery (subscriptions, background workers) is outside the cloning contract.
- Roles that cannot be dropped are disabled (`NOLOGIN`, no password) and recorded for cleanup.
- Branch roles can use the server's resources (connections, disk, CPU). devflow adds no quotas in 1.0.
- Database and role names are generated; devflow never derives ownership from a name.

### Credentials

- Branch URLs contain passwords. They are written to `.devflow/env` and to the workspace receipt, both with mode 0600. `.devflow/` ignores itself, so it does not appear in `git status`; never commit it.
- A managed server's superuser password is in `admin.json` and `compose.yaml` (both mode 0600) under `<data dir>/postgres/<project id>/`; the data dir is `$XDG_DATA_HOME/devflow` (macOS: `~/Library/Application Support/devflow`) or `DEVFLOW_DATA_DIR`.
- An external server's admin URL belongs in the git-ignored `devflow.local.toml` or in `DEVFLOW_PG_ADMIN_URL`, never in `devflow.toml`.
- Command output and `--json` documents redact passwords (`***`) except `devflow db url` and `devflow db branch create`, which exist to hand out credentials. Do not log their output. `db branch create --instance-id` rotates the password of an existing branch, which invalidates URLs handed out earlier.
- `devflow db psql` passes the connection (password included) to `psql` through its environment, never its command line.

### Redaction contract

Redaction guards against accidental disclosure; it is not a secret scanner.

- **Guaranteed:** every credential devflow knows — generated branch and admin passwords and the password of an attached `--database-url` — is masked by exact match wherever devflow shows hook output: the terminal, the GUI and the persisted setup logs. devflow's own JSON and GUI fields are redacted by structure.
- **Best effort:** other credentials in hook output are masked by pattern: URL credentials and secret query parameters (`password`, `%70assword`, `api_key`, …), and `KEY=value` / `KEY: value` where the key contains `password`, `passwd`, `secret`, `token`, `api_key` or `apikey` (shell exports, `env` listings, conninfo strings, YAML, serialized JSON). When unsure where a value ends, more is masked.
- **Not covered:** unencoded spaces inside a URL password, `.pgpass` lines, values that span several lines, other databases' credential formats (for example SQL Server `pwd=`), and hooks that deliberately obfuscate what they print. Hooks are trusted code: one that wants to reveal a secret can.

### Filesystem

- `worktree.copy` paths must be relative and stay inside both the source and the new worktree: `..` and symlink escapes are rejected. Entries are staged under a temporary name and renamed into place; existing destination entries are never overwritten.
- Worktree paths that already exist are refused, never reused.

### Network and remote use

- **devflow 1.0 has no TLS support.** A managed server is published on `127.0.0.1` by default. Binding to another address, or using an external server across machines, sends database traffic unencrypted. Use a private network such as Tailscale or WireGuard, and do not expose the port to the internet.
- Access to the Docker engine that runs a managed server is superuser access to its database. Do not give sandboxes a Docker socket, context or SSH key for the data host.
- Sandboxes should receive only their branch URL. See [docs/REMOTE.md](docs/REMOTE.md).

## Repository hygiene

The repository's Git hooks and CI scan every commit, commit message and tag for secrets with kingfisher before they are published; see [CONTRIBUTING.md](CONTRIBUTING.md#secrets).

## Dependencies

Dependabot proposes Cargo and GitHub Actions updates, and CI runs `cargo audit` on every push. To run it locally:

```sh
cargo install cargo-audit
cargo audit
```
