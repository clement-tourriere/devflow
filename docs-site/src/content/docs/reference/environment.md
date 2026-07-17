---
title: Environment variables
description: Every DEVFLOW_* environment variable and what it overrides.
sidebar:
  order: 4
---

Environment variables sit at the **top** of the [config hierarchy](/devflow/reference/configuration/#hierarchy) — above `.devflow.local.yml` and `.devflow.yml`.

## Behavior toggles

| Variable | Description | Default |
| --- | --- | --- |
| `DEVFLOW_DISABLED=true` | disable devflow entirely | `false` |
| `DEVFLOW_SKIP_HOOKS=true` | skip Git-hook execution | `false` |
| `DEVFLOW_AUTO_CREATE=false` | override `git.auto_create_on_workspace` | config value |
| `DEVFLOW_BRANCH_FILTER_REGEX=…` | override `git.workspace_filter_regex` | config value |
| `DEVFLOW_DISABLED_BRANCHES=main,release/*` | disable for specific workspaces (comma-separated) | — |
| `DEVFLOW_CURRENT_BRANCH_DISABLED=true` | disable for the current workspace only | `false` |
| `DEVFLOW_CONTEXT_BRANCH=…` | override the context workspace used as default parent (CI) | auto-detected |

## Hooks & automation

| Variable | Description | Default |
| --- | --- | --- |
| `DEVFLOW_APPROVE_HOOKS=1` | auto-approve config-file hooks (CI / agent runs) | `false` |
| `DEVFLOW_BACKGROUND_HOOK_TIMEOUT=30` | seconds to await `background: true` hooks before CLI exit | `30` |

devflow also **exports** variables into the processes it spawns — shell hook commands, `devflow switch -x`/`--open` sessions, and GUI terminals: `DEVFLOW_WORKSPACE` (raw VCS workspace name), `DEVFLOW_WORKSPACE_KEY` (backend service/path key), and `DEVFLOW_BRANCH` (compatibility alias for the raw name). See [hooks reference](/devflow/reference/hooks/#injected-environment-variables).

## Storage

| Variable | Description | Default |
| --- | --- | --- |
| `DEVFLOW_ZFS_DATASET=…` | force a specific ZFS dataset | auto-detected |

## AI commit messages

| Variable | Description | Default |
| --- | --- | --- |
| `DEVFLOW_COMMIT_COMMAND=…` | external CLI for message generation (e.g. `claude -p`) — preferred path | — |
| `DEVFLOW_LLM_API_KEY=…` | API key for the OpenAI-compatible fallback | — (not required for localhost URLs) |
| `DEVFLOW_LLM_API_URL=…` | endpoint URL (e.g. `http://localhost:11434/v1` for Ollama) | OpenAI |
| `DEVFLOW_LLM_MODEL=…` | model name | `gpt-4o-mini` |

## Internal

| Variable | Description |
| --- | --- |
| `DEVFLOW_SHELL_INTEGRATION=1` | set by the shell wrapper when invoking devflow; enables `DEVFLOW_CD` emission paths |

AWS credentials for [S3 seeding](/devflow/guides/seeding/) use the standard `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_DEFAULT_REGION` (or `AWS_REGION`) variables.
