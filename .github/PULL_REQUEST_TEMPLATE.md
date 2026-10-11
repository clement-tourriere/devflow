## Summary

Brief description of what this PR does and why.

## Changes

- 

## Testing

How was this tested? (manual steps, new/updated tests, CI results)

## Checklist

- [ ] `cargo fmt --check` is clean
- [ ] `cargo clippy --locked --workspace --all-targets -- -D warnings` is clean (Linux: add `--exclude devflow-gui`)
- [ ] `cargo test --locked -p devflow -p devflow-core` passes
- [ ] GUI changes (macOS): `cargo test --locked -p devflow-gui` passes
- [ ] Database changes: `cargo test -p devflow-core --test db_postgres -- --ignored --test-threads=1` passes
- [ ] Updated `CHANGELOG.md` under Unreleased (if user-facing)
- [ ] Updated `README.md`, `AGENTS.md` or `docs/` (if behavior or the JSON contract changed)
