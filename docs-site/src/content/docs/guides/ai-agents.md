---
title: AI agents & automation
description: Isolated environments per agent task — JSON contract, skills, sandboxing, AI commit messages, and CI patterns.
sidebar:
  order: 7
---

devflow is designed for autonomous agents as much as humans: every agent task can get its own worktree *and* its own database — fully isolated, instantly cloned, trivially cleaned up. `--json --non-interactive` makes every command machine-safe.

## The contract

- `--json` — structured output on stdout.
- `--non-interactive` — no prompts. Unapproved hooks are **skipped with a visible warning** (never block); the JSON `hooks` summary reports them as `skipped`.
- `destroy` and `remove` require `--force` in `--json`/`--non-interactive` mode.
- Multi-provider `service create`/`service delete`/`switch` exit non-zero if **any** provider fails.
- `devflow --json capabilities` returns a machine-readable summary of these guarantees (plus CoW support, worktree mode, …) — detect at runtime instead of assuming.

:::caution
`--no-verify` skips **all** hooks. Agents usually want hooks (they write `.env.local`, run migrations, trust mise) — use `--non-interactive` plus pre-approval instead.
:::

## The per-task pattern

```bash
WORKSPACE="agent/$TASK_ID"

# 1) Isolated environment (branch + worktree + cloned DB + hooks)
OUTPUT=$(devflow --json --non-interactive switch -c "$WORKSPACE")

# 2) Worktree path = the agent's workdir for every subsequent tool call
WORKTREE=$(echo "$OUTPUT" | jq -r '.worktree_path // empty')
[ -n "$WORKTREE" ] && cd "$WORKTREE"

# 3) Connection info
CONN=$(devflow --json service connection "$WORKSPACE" | jq -r '.connection_string')
export DATABASE_URL="$CONN"

# 4) Work …  then reset for retries:
devflow --json --non-interactive service reset "$WORKSPACE"

# 5) Cleanup
devflow --json --non-interactive remove "$WORKSPACE" --force
```

Ready-made versions: `examples/agent-bootstrap.sh` (idempotent repo setup) and `examples/agent-task.sh` (task-scoped environment).

## Hook approvals for agents

Approvals are keyed on the **command template** and the **canonical project root** — approve once (from anywhere) and it covers every workspace, including agent-created worktrees:

```bash
devflow hook approvals add "mise trust"
devflow hook approvals add "npm run migrate"
# or blanket-approve for a run:
DEVFLOW_APPROVE_HOOKS=1 devflow --json --non-interactive switch -c agent/t42
```

If a JSON result shows `skipped > 0` in a hook phase, an approval is missing.

## Launching agents in workspaces

```bash
devflow switch -c agent/fix-login -x claude -- -p 'Fix the login timeout bug'
devflow switch -c agent/fix-login -x codex
devflow switch -c agent/fix-login -x claude --detach    # in a tmux/zellij session
devflow agent status                                    # tracked agent workspaces
devflow agent context --format json                     # project/services/connections for the agent
```

`-x` runs the command inside the workspace's worktree; `--detach` puts it in a detached multiplexer session so you can run several agents in parallel and attach later.

## Sandboxed workspaces

For higher-risk tasks, create the workspace sandboxed — platform-aware filesystem/command restrictions applied to its hooks and executed commands:

```bash
devflow switch -c agent/experiment --sandboxed
devflow switch -c trusted-task --no-sandbox     # opt out if sandbox is the default
```

## Skills & context files

```bash
devflow agent skill      # install bundled workspace skills into .claude/skills/
devflow skill list       # full skills management (search/install/remove/update via skills.sh)
```

The bundled skills teach agents to list/switch/create workspaces with the JSON contract. Context files follow open conventions:

| File | Audience |
| --- | --- |
| `AGENTS.md` | agent-first onboarding (Cursor, OpenCode, Copilot read it) |
| `CLAUDE.md` | Claude Code project context |
| `llms.txt` / `llms-full.txt` | machine-readable index / full flat context dump |

Agent-modified configs in worktrees flow back with `devflow sync-ai-configs` (union-merges `.claude/settings.local.json` permissions; additive copies elsewhere) — or automatically via the `sync-ai-configs` recipe.

## AI commit messages

```bash
devflow commit --ai             # generate Conventional Commit message and commit
devflow commit --ai --edit      # review in $EDITOR first
devflow commit --ai --dry-run   # print only
```

Generation prefers an external CLI, falling back to any OpenAI-compatible API:

```yaml
commit:
  generation:
    command: "claude -p --model haiku"     # preferred
    # api_url: "http://localhost:11434/v1" # fallback (Ollama: no key needed for localhost)
    # model: "llama3.1"
```

Environment equivalents: `DEVFLOW_COMMIT_COMMAND`, `DEVFLOW_LLM_API_URL`, `DEVFLOW_LLM_MODEL`, `DEVFLOW_LLM_API_KEY`. Diffs over 32 KB are truncated with a notice to the model.

## CI example (GitHub Actions)

```yaml
jobs:
  preview:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: cargo install --path .
      - name: Create preview environment
        run: |
          devflow --json --non-interactive init myapp
          devflow --json --non-interactive switch -c pr-${{ github.event.number }} --no-verify
      - name: Migrate + test
        run: |
          CONN=$(devflow --json connection pr-${{ github.event.number }} | jq -r '.connection_string')
          DATABASE_URL="$CONN" npm run migrate && DATABASE_URL="$CONN" npm test
      - name: Cleanup
        if: always()
        run: devflow --non-interactive remove pr-${{ github.event.number }} --force
```

(`--no-verify` is appropriate in CI when hooks aren't pre-approved; use `DEVFLOW_APPROVE_HOOKS=1` if you want them to run.)
