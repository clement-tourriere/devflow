# Contributing to devflow

Thank you for your interest in contributing to devflow! This document provides guidelines and information for contributors.

## Getting Started

1. **Fork and clone** the repository
2. **Install prerequisites**: Rust 1.70+, Docker
3. **Build**: `cargo build`
4. **Test**: `cargo test --workspace`

## Development Setup

```bash
git clone https://github.com/clement-tourriere/devflow.git
cd devflow

# Build all crates
cargo build

# Run all tests
cargo test --workspace

# Check formatting and lints
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
```

### Optional: Desktop GUI

```bash
# Requires bun + Tauri CLI
mise run gui        # Dev mode
mise run gui:build  # Production bundle
```

## Project Structure

| Crate | Description |
|---|---|
| `devflow` (root) | CLI binary + TUI dashboard |
| `crates/devflow-core` | Shared library: config, services, VCS, hooks, state |
| `crates/devflow-proxy` | Native HTTPS reverse proxy |
| `crates/devflow-terminal` | PTY management for desktop GUI |
| `src-tauri/` | Tauri 2 desktop app backend |
| `ui/` | React frontend for desktop app |

See `CLAUDE.md` for a detailed file-level map.

## Making Changes

### Branching

- Create a branch from `main` for your work
- Use descriptive branch names: `feature/`, `fix/`, `docs/`

### Commit Messages

We use [Conventional Commits](https://www.conventionalcommits.org/):

```
feat(proxy): add Firefox trust via enterprise policies
fix(workspace): surface post-create hook failures
docs: add CONTRIBUTING.md
test(hooks): add executor approval tests
refactor(config): split into submodules
```

Valid scopes include: `workspace`, `service`, `proxy`, `hooks`, `config`, `tui`, `gui`, `agent`, `skills`, `vcs`, `cli`.

### Code Style

- Run `cargo fmt` before committing
- All `cargo clippy` warnings must be resolved
- Use `anyhow::Context` for error context, not bare `.unwrap()`
- Prefer `anyhow::bail!` over `panic!` in library code
- Add `/// doc comments` to all public items
- Use feature flags for optional functionality

### Testing

- Add unit tests for new logic in `#[cfg(test)] mod tests { ... }`
- Use `#[tokio::test]` for async tests
- Integration tests go in `scripts/integration-test.sh`
- Run the full suite: `cargo test --workspace`

### Pull Requests

1. Ensure CI passes (formatting, clippy, tests, audit)
2. Update `docs/CLI.md` if you add/change CLI commands
3. Update `CLAUDE.md` if you change project structure
4. Add a changelog entry if the change is user-facing

## Feature Flags

When adding new functionality, consider whether it should be feature-gated:

| Flag | Controls |
|---|---|
| `service-local` | Docker-based local providers |
| `service-neon` | Neon cloud provider |
| `service-dblab` | DBLab provider |
| `service-xata` | Xata provider |
| `llm` | AI commit message generation |
| `skills` | Skill management system |
| `tui` | Terminal UI dashboard |

## Reporting Issues

Use the [bug report template](.github/ISSUE_TEMPLATE/bug_report.md) and include:
- `devflow --version` output
- `devflow doctor` output
- Relevant `.devflow.yml` sections (redact credentials)

## License

By contributing, you agree that your contributions will be licensed under the MIT License.
