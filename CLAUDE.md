# devflow

Devflow creates isolated development environments containing code worktrees, reusable data, and application processes. Read [AGENTS.md](AGENTS.md) for the automation contract and [docs/architecture.md](docs/architecture.md) for dependency boundaries and migrations.

The Rust workspace contains the CLI/TUI package, `devflow-core`, the optional proxy library, the terminal adapter, and the Tauri desktop application. The React frontend is in `ui`; the documentation site is in `docs-site`.

Core owns lifecycle policy and identity. Frontends submit typed requests and render results. Keep provider observation separate from mutation, preserve existing service keys, and use the operation journal before external effects. Never infer resource ownership from a display-name prefix.

Configuration accepts YAML and TOML. The desktop raw editor preserves the original format and checks for concurrent edits. Workspace file copying is explicit; broad ignored-file and AI configuration copying are off by default. Agent permission/config merging is retired.

For a task, use `devflow --json --non-interactive switch -c agent/TASK`. Read the returned materialized path before executing commands. Required setup gates execution in every output mode; skipped approvals retain the worktree and report `needs_attention`. Use `devflow exec --workspace agent/TASK -- PROGRAM ARGS` for literal arguments, or `--shell SCRIPT` for deliberate shell syntax.

Named PostgreSQL baselines pin reusable data independently of code provenance. `baseline prepare` imports, validates, and seals a version; `switch --data-from baseline:NAME@vN` and `service reset` preserve its exact generation. Redis/cache allocation and artifact reuse have separate explicit policies.

Useful checks:

```sh
cargo fmt --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo test --locked -p devflow --no-default-features
cargo test --locked -p devflow-core --all-features
cd ui && bun run build && bun test tests
```

Docker regressions are ignored by default and run explicitly with `cargo test -p devflow-core --test review_regressions -- --ignored --test-threads=1`. Their resources and state are isolated. Linux ZFS tests require a disposable pool. Live cloud/Jujutsu coverage and large-dataset performance must be reported separately from mocked/unit checks.

Default CLI features include local services and the TUI. Optional integrations are `proxy`, `service-neon`, `service-xata`, and `service-dblab`. Commit message generation uses an external command. Processes use the native runtime; the proxy runs as an optional companion shared by CLI and desktop.
