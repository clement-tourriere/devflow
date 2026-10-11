---
name: Bug Report
about: Report a bug to help us improve devflow
labels: bug
---

## Description

A clear description of the bug.

## Steps to Reproduce

1. Run `devflow ...`
2. ...
3. See error

## Expected Behavior

What you expected to happen.

## Actual Behavior

What actually happened. Include full error output if available.

## Environment

- **devflow version**: (`devflow --version`)
- **OS**: (e.g. macOS 14, Ubuntu 24.04)
- **Docker version**: (`docker --version`)
- **Postgres mode**: (managed, external, none) and server version
- **Filesystem / CoW**: (what `devflow doctor` reports; e.g. OrbStack, btrfs, ext4)
- **Shell**: (bash, zsh, fish)

## Config (if relevant)

```toml
# Paste relevant sections of devflow.toml (redact credentials)
```

## Logs

```
# Output of `devflow doctor` and the workspace's setup log (<git dir>/devflow/logs/). Redact passwords.
```
