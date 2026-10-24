# Contributing to devflow

Thanks for helping. devflow 1.0 is deliberately small: Git worktrees, copy-on-write payloads, data branches (PostgreSQL, Valkey, Redis, MySQL, SQLite), trusted setup hooks, the loop around agent hosts, and a desktop app. Features that pitchfork (processes, proxy), mise (toolchains, dependencies, tasks) or Mr. Boxington (Rust build caches) already provide belong in those tools. Read [docs/plans/2026-10-10-devflow-1.0.md](docs/plans/2026-10-10-devflow-1.0.md) before proposing a new capability.

## Getting started

1. Fork and clone the repository.
2. Install the Rust version in `rust-toolchain.toml`, Git, Docker (for the database tests), and [mise](https://mise.jdx.dev/) 2026.8.16 or newer. Releases need [Commitizen](https://commitizen-tools.github.io/commitizen/) and a GPG key; `cargo audit`, shellcheck and pitchfork are needed only for the matching CI steps and ignored tests.
3. `mise install`, then `hk install` for the Git hooks (`hk install --mise` if mise is not activated in your shell; see [Secrets](#secrets)), then `mise run build` and `mise run test`.

The project configures a mise Cargo wrapper for [Mr. Boxington](https://mr-boxington.jdx.dev/setup) (`mbx`) so checkouts share compilation results. Cargo commands run through it in `mise run` tasks, `mise exec -- cargo ...`, and shells with mise activation. Check it with `mise exec -- mbx doctor`.

```sh
cargo build                                              # devflow + devflow-core
cargo fmt --check
cargo clippy --locked --workspace --all-targets -- -D warnings   # Linux: add --exclude devflow-gui
cargo test --locked -p devflow -p devflow-core
cargo test -p devflow-core --test db_postgres -- --ignored --test-threads=1   # Docker required
cargo build -p devflow-gui                               # desktop app (macOS)
cargo test --locked -p devflow-gui                       # headless GUI tests (macOS)
```

`mise run lint`, `check` and `ci` cover the whole workspace, GUI included; on Linux run the cargo commands with `--exclude devflow-gui`.

Multi-feature builds and many worktrees can fill the disk; `mbx clean` reclaims space.

## Project structure

| Path | Description |
| --- | --- |
| `Cargo.toml`, `src/` | The `devflow` CLI: argument parsing, prompts, JSON output (`src/cli/`, one module per command family) |
| `crates/devflow-core` | All behavior: configuration and its effective form per workspace, worktrees and payloads, receipts and the ledger, setup and activation, hooks, trust, the env file, `db` (PostgreSQL), `services` and `sqlite` (the other data services), `detect` and `propose` (`init`), `integrate` (agent hosts), `gc`, `suspend`, `focus`, `doctor`, pitchfork and process gating, redaction, typed errors and remedies. The full module map is in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) |
| `crates/devflow-gui` | GPUI desktop app |
| `tests/`, `crates/devflow-core/tests/` | End-to-end CLI tests (`cli.rs`, `cli_edit.rs`, `integrations.rs`, `agent_task.rs`), the documentation link check (`docs.rs`), core workspace, ledger, recipes, process, SQLite and schema tests, and the Docker database and service tests |
| `docs/` | The user guide (`guide/`, one page per topic), the automation reference, architecture, migration guide, the generated JSON Schemas, design plans |
| `examples/` | Sample configuration and an agent script |
| `scripts/` | Installer, secret scanning, release version sync |
| `mise.toml`, `hk.pkl`, `.cz.toml` | Tools and tasks, Git hooks, release configuration |

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the module map and `CLAUDE.md` for working rules.

## Making changes

- Branch from `main` with a descriptive name: `feature/`, `fix/`, `docs/`.
- Core owns policy; the CLI and GUI render. Put lifecycle decisions in `devflow-core`, which keeps a blocking API (nothing async is public) and no UI types.
- Record intent before external effects, and make operations safe to re-run.
- Use `anyhow::Context` rather than `.unwrap()`, and `anyhow::bail!` rather than `panic!` in library code.
- Document public items with `///` comments.
- A change to `crates/devflow-core/src/config.rs` (a key, a default, a doc comment) changes the JSON Schema of `devflow.toml`. Regenerate the committed copies and commit them with the change: `cargo run -q -p devflow -- config schema > docs/devflow.schema.json` and `cargo run -q -p devflow -- config schema --local > docs/devflow.local.schema.json`. A test fails with those lines when they are out of date.

### Commit messages

[Conventional Commits](https://www.conventionalcommits.org/), checked in CI for pull requests:

```
feat(db): reset a branch to its recorded source
fix(setup): resume after an interrupted payload phase
docs: describe remote data hosts
test(trust): cover changed hook sections
```

Scopes in use: `cli`, `core`, `config`, `worktree`, `cow`, `setup`, `hooks`, `trust`, `db`, `doctor`, `gui`, `ci`, `docs`, `deps`, `release`.

### Testing

- Unit tests live beside the code in `#[cfg(test)] mod tests`.
- `tests/cli.rs` runs the `devflow` binary end to end; `crates/devflow-core/tests/workspace.rs` covers worktrees, setup and removal without Docker.
- Database behavior is tested against real PostgreSQL 18 in Docker. Those tests are ignored by default (`-- --ignored --test-threads=1`) and use isolated containers and state: `--test db_postgres`, plus `--test workspace -- --ignored managed_postgres` for the managed server end to end. `--test workspace -- --ignored pitchfork` needs pitchfork on `PATH`.
- Valkey/Redis branches have their own Docker tests: `cargo test -p devflow --test services_docker -- --ignored --test-threads=1` (needs `valkey/valkey:8-alpine` and `debian:stable-slim`; `redis:8-alpine` for one test). Each test uses its own project id and data directory, and puts `DEVFLOW_TEST_LABEL` on every container, volume and helper devflow creates (`dev.devflow.test=<run>`), and a guard removes everything carrying the label even after a panic. Interrupted runs can be cleaned with `docker ps -aq --filter label=dev.devflow.test | xargs docker rm -f` and the same filter for `docker volume`. `DEVFLOW_TEST_COPY_DELAY_SECS`, `DEVFLOW_TEST_COPY_TIMEOUT_SECS` and `DEVFLOW_TEST_NO_REFLINK` make the copy slow, short or reflink-less for the kill, deadline and `require_cow` tests.
- MySQL has its own: `cargo test -p devflow --test services_mysql_docker -- --ignored --test-threads=1` (needs `mysql:8.4` and `debian:stable-slim`). CI runs both service suites on Linux, next to the PostgreSQL one.
- `scripts/bench-workspaces.sh` is a manual benchmark (not run by CI): five workspaces on a scratch project with PostgreSQL and Valkey (`--mysql` adds MySQL), printing the cold and warm `switch -c` times, the memory of every container (`docker stats`), the growth of the Docker VM's disk and of the volumes, and a MySQL source's downtime, as a table and JSON (`--json FILE`). It needs Docker, `jq` and the images the suites use, pulls nothing, keeps its registry and data in a temporary directory and removes what it created. Run it on an idle engine: other containers add noise to the memory and disk figures.
- `scripts/smoke-test.sh DEVFLOW [VERSION]` is the quick start CI runs against an installed release (`init`, `switch -c`, `list`, `remove` in a scratch repository); run it on a local build to see what it checks.
- `tests/docs.rs` checks the documentation: every relative link in a Markdown file must resolve to a file and, when it names one, a heading (GitHub's spelling of it), and so must every `…/blob/main/…` or raw-GitHub link into this repository that the code, scripts and examples carry (the migration guide, the guide for checkouts devflow does not make, the schemas). Move a document or rename a heading and it fails with `file:line: target`; the fix is the link, not the test.
- The GUI has headless tests (`cargo test --locked -p devflow-gui`, macOS); native visual checks are manual.
- Ext4 runners cannot share blocks, so CI cannot assert `verified` CoW. Verify CoW on OrbStack or a Linux btrfs/XFS host and report it separately from the mocked and unit checks. Set `DEVFLOW_EXPECT_COW=verified` to make the Docker tests assert it.
- Use `DEVFLOW_CONFIG_DIR` and `DEVFLOW_DATA_DIR` to isolate tests from your real registry and managed servers.
- The GUI needs macOS (GPUI on Linux requires system libraries and Vulkan). Linux rendering is not advertised until it has been checked.

### Secrets

Nothing that looks like a credential may be committed or pushed. The [hk](https://hk.jdx.dev) hooks in `hk.pkl` call `scripts/kingfisher.sh`, which runs [kingfisher](https://github.com/mongodb/kingfisher) without contacting any provider or applying its size and archive limits, and prints only the rule, location and commit of a finding, with the secret masked:

- **pre-commit** runs rustfmt and clippy (fixes are staged), then scans every changed file in the index as Git stores it, binaries and archives included, and the changed file names. **commit-msg** scans the message.
- **pre-push** scans everything the push publishes after the audited commit named in the script: file contents (deleted ones too), file names, commit messages and author fields, and tag annotations. A secret added in one commit and deleted in the next still blocks the push, whichever remote already has it. Shallow clones are refused; run `git fetch --unshallow`. CI runs the same scan before any other job.

`hk check` scans the whole working tree. A scan that runs longer than `KINGFISHER_TIMEOUT` seconds (600) fails; `KINGFISHER_DEBUG=1` shows kingfisher's own errors. Tests need fake credentials; mark each one with a `// kingfisher:ignore` comment on its line or the line above. If a real secret was committed, rotate it first, then rewrite the unpushed commits; once it has been pushed, removing it from history does not make it safe again.

### Pull requests

1. CI passes: the secret scan, Conventional Commits, formatting, clippy, tests, the Docker database and container-service tests, the GUI clippy, build and tests on macOS, shellcheck of the scripts, and `cargo audit`.
2. Update the page of the [guide](docs/guide/README.md) that describes the behavior (and the [README](README.md) only when the tour changes), `AGENTS.md` for a change to the automation contract (the envelope, exit codes, remedies, the flow) and [docs/agents-reference.md](docs/agents-reference.md) for a document shape, a field or a refusal, `docs/ARCHITECTURE.md` for a module, and `CLAUDE.md` when the layout or the working rules change. Keep README (about 12 KB) and `AGENTS.md` (about 5 KB) short: detail goes to the guide and the reference.
3. Add a `CHANGELOG.md` entry under `## Unreleased`, in the theme it belongs to (create one only when none fits). Describe the feature as it ships; do not add an entry for a fix to something that has not been released.

## Releases

On the tag, CI builds the artifacts, publishes them, and then installs them with `scripts/install.sh` on Linux amd64, Linux arm64 and macOS and runs `scripts/smoke-test.sh` (the installed `--version` must equal the tag). The same job runs by hand from the Actions tab (`workflow_dispatch`, with the release tag as a required input such as `v1.0.0`; `latest` is not accepted) to check a published release, after it has verified that the release holds every asset. Until the first 1.x release is published, the latest release is a 0.x that reads another configuration format, so the README says so and the installer warns after installing one.

Releases are cut with Commitizen from `main`; the `pre_bump_hooks` run `scripts/sync-version.py`, which keeps the three crates and `Cargo.lock` on one version, and the tag is GPG-signed. `mise run release` always bumps the patch version. `cz bump` replaces the curated `## Unreleased` notes with a list generated from commit messages, so put them back:

```sh
cz bump --yes --increment MAJOR        # or MINOR / PATCH
git show HEAD^:CHANGELOG.md | sed "s/^## Unreleased$/## v$(cz version -p) ($(date +%F))/" > CHANGELOG.md
git commit --amend --no-edit CHANGELOG.md
git tag -f -s "v$(cz version -p)" -m "v$(cz version -p)"
```

## Reporting issues

Use the [bug report template](.github/ISSUE_TEMPLATE/bug_report.md) and include `devflow --version`, the output of `devflow --json doctor`, and the relevant `devflow.toml` sections with credentials removed. Security problems go through [SECURITY.md](SECURITY.md), not public issues.

## License

By contributing, you agree that your contributions will be licensed under the MIT License.
