# devflow

devflow 1.0 creates Git worktrees, clones their payloads copy-on-write, branches PostgreSQL databases (one PostgreSQL 18 server per project, `CREATE DATABASE … TEMPLATE … STRATEGY FILE_COPY` with `file_copy_method = clone`), gives each workspace its own copy-on-write Valkey/Redis or MySQL 8.4 (preview) container, and runs trusted setup hooks. Processes and the proxy are pitchfork's job; toolchains and dependencies are mise's. Read [AGENTS.md](AGENTS.md) for the automation contract, [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the module map, and [docs/plans/2026-10-10-devflow-1.0.md](docs/plans/2026-10-10-devflow-1.0.md) for the authoritative design.

## Layout

| Path | Contents |
| --- | --- |
| `Cargo.toml`, `src/` | `devflow` CLI package: clap commands in `src/cli/` (`hooks.rs` and `env.rs` edit the configuration through core's `config_edit`), the JSON envelope in `src/cli/output.rs` (credential masking is `devflow_core::redact`) |
| `crates/devflow-core/` | All behavior. Blocking API (the `postgres` crate runs its own hidden runtime; nothing async is public), no GPUI types. Modules: `config`, `config_edit`, `conn`, `project`, `registry`, `worktree`, `cow`, `receipt`, `setup`, `hooks`, `scope`, `pitchfork_scope`, `trust`, `detect`, `propose`, `githook`, `fsutil`, `env_file`, `exec`, `redact`, `diag`, `error`, `subprocess`, `db/{admin,meta,steps,ops,reconcile,managed,scratch}`, `services/{catalog,docker,profile,resp,plan,exec,launch,locks,ops,recover,stop,helpers,probe,mysql,effects,workspace}`, `doctor`, `pitchfork`, `processes`, `paths` |
| `crates/devflow-gui/` | GPUI desktop app (`devflow-gui`), `gpui-kit` pinned to `=0.7.1` |
| `docs/` | `ARCHITECTURE.md`, `MIGRATING.md`, `REMOTE.md`, `plans/` |
| `examples/` | Sample `devflow.toml`, `devflow.external.toml`, `mise.toml`, `pitchfork.toml`, `agent-task.sh` |
| `tests/cli.rs`, `crates/devflow-core/tests/` | End-to-end CLI tests; core `workspace.rs` and Docker-only `db_postgres.rs`; `tests/services_docker.rs` and `tests/services_mysql_docker.rs` (Docker-only Valkey/Redis and MySQL tests) |
| `scripts/` | `install.sh`, `kingfisher.sh` (secret scanning), `sync-version.py` (release) |

Core owns lifecycle policy and identity. The CLI and GUI build typed requests, call core, and render results; they make no lifecycle decisions.

## Design rules

- Data lifetime is independent of code lifetime. `--from` (code) and `--data-from` (data) are separate and immutable once chosen.
- Never infer ownership from names. Branches, snapshots and roles live in the `devflow` metadata database (Valkey/Redis/MySQL containers and volumes in the services catalog, bound to the Docker daemon id; labels only corroborate); worktrees carry a receipt with an instance id. Database and role names are generated.
- Resumable, not transactional: record intent before every external effect, so a re-run converges. Hooks run at least once.
- Report what happened: CoW is `verified`, `copy` or `unknown`, never assumed.
- Credentials: passwords are redacted in all output except where revealing one is the command's purpose (`db url`, `db branch create`, `service url`, `env list --show-values`); the desktop app reveals a connection only on request; receipts, `admin.json`, the managed `compose.yaml` and `.devflow/env` are written 0600.
- Configuration is TOML only (`devflow.toml` or `.devflow.toml`, merged with git-ignored `devflow.local.toml`). Services are `[services.<key>]` tables selected by `engine`: `postgres`, `valkey`, `redis` and `mysql` (preview), with at most one `postgres` and one `mysql`. Without a 1.0 config, 0.x files (recognised by shape) produce an error that links `docs/MIGRATING.md`; never touch 0.x resources.
- Receipts (version 2) bind data per service (`services.<key>`) and record the configuration generation each binding and `.devflow/env` were applied under: adding or changing a service reopens `services`, `env` and `hooks` (deliberate `--no-db`/`--no-hooks` skips are kept). A version 1 receipt is migrated in place under the receipt lock, with no provisioning effect.
- Processes are pitchfork's: devflow starts or stops a workspace's daemons (`up`, `down`, `[pitchfork] autostart`) only for a `ready` workspace whose own pitchfork configuration matches the approved digest and has no daemon that starts on its own, stops them only while it still matches, never writes environment or secrets into pitchfork files (`.devflow/env` reaches a daemon through `run = "devflow exec -- CMD"`) and never runs `pitchfork proxy setup`. Hooks are plain `sh -c` commands (strings or `{name, run, timeout}` tables) gated by `devflow trust`, whose approval covers a versioned scope: the merged `[hooks]`, the `[env]` recipes and presets, and the project's pitchfork configuration (SHA-256 of canonical JSON, never generated values). No command templating; `[env]` values reference devflow-generated variables as `${NAME}`, one pass.
- The workspace's variables reach commands through `devflow exec -- CMD`, which applies `.devflow/env` last (over mise and the shell); generated hooks are `mise exec -- devflow exec -- CMD`. A setup hook that reaches mise any other way is checked (`mise env`/`mise exec -- env`) and refused when mise would override a variable.
- Exit code 2 means the workspace is not `ready` (`needs_attention` or `in_progress`); usage and other errors stay at 1.

## Checks

```sh
cargo fmt --check
cargo clippy --locked --workspace --all-targets -- -D warnings   # Linux: add --exclude devflow-gui
cargo test --locked -p devflow -p devflow-core
cargo test --locked -p devflow-gui                               # macOS: headless GUI tests
cargo test -p devflow-core --test db_postgres -- --ignored --test-threads=1   # needs Docker
cargo test -p devflow-core --test workspace -- --ignored managed_postgres     # needs Docker
cargo test -p devflow-core --test workspace -- --ignored pitchfork            # needs pitchfork on PATH
cargo test -p devflow --test services_docker -- --ignored --test-threads=1        # needs Docker
cargo test -p devflow --test services_mysql_docker -- --ignored --test-threads=1  # needs Docker
```

`devflow-gui` is outside the default workspace members; it pulls in GPUI, which on Linux needs system libraries, so build and test it on macOS (CI does). On Linux use `--exclude devflow-gui`. The Docker tests are ignored by default and use isolated resources; they must not assert `verified` CoW unless `DEVFLOW_EXPECT_COW=verified` is set. Live remote-server checks and large-dataset performance must be reported separately from unit tests.

Git hooks come from hk (`hk install`, config in `hk.pkl`); secret scanning is `scripts/kingfisher.sh`. Pre-commit runs rustfmt and clippy and then scans the staged content and file names, commit-msg scans the message, and pre-push scans the contents, names, messages and authors of every new commit, and tags. Mark a fake credential in a test with `// kingfisher:ignore` on its line or the line above; never bypass the hooks with `--no-verify` or `HK=0`.

`mise run ci` runs the format, clippy and test checks; clippy covers the GUI, so run it on macOS. There is no commit helper; use Git directly. Releases are cut with Commitizen (see CONTRIBUTING.md, Releases): `cz bump` replaces `## Unreleased` with notes generated from commit messages, so restore the curated notes afterwards; `scripts/sync-version.py` keeps `devflow`, `devflow-core`, `devflow-gui` and `Cargo.lock` on one version.
