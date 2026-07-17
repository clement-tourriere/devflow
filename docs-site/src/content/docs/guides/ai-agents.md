---
title: AI agents & automation
description: Isolated environments per agent task — JSON contract, workspace helpers, AI commit messages, and CI patterns.
sidebar:
  order: 8
---

devflow is designed for autonomous agents as much as humans: every agent task can get its own worktree *and* its own database — fully isolated, instantly cloned, trivially cleaned up. `--json --non-interactive` makes every command machine-safe.

## The contract

- `--json` — exactly one structured document on stdout for supported
  machine-readable commands; `switch -x` nests command output under
  `execution`. Output interfaces (`shell-init`, `completions`, and `tui`)
  reject this flag.
- `--non-interactive` — no prompts. Unapproved hooks are **skipped with a visible warning** (never block); the JSON `hooks` summary reports them as `skipped`.
- `destroy` and `remove` require `--force` in `--json`/`--non-interactive` mode.
- Multi-provider `service create`/`service delete`/`switch` exit non-zero if **any** provider fails.
- `devflow --json capabilities` returns a machine-readable summary of these guarantees (plus CoW and VCS support) — detect at runtime instead of assuming.
- `devflow --json list` always returns a versioned tree document. Use a node's raw `name` for commands, its reported `service_key` for generated identifiers, and `worktree_path` for the agent workdir. Never reconstruct the key: a migrated workspace may retain a legacy value, while unresolved legacy ownership is blocked.

:::caution
`--no-verify` skips **all** hooks. Agents usually want hooks (they write `.env.local`, run migrations, trust mise) — use `--non-interactive` plus pre-approval instead.
:::

## The per-task pattern

```bash
WORKSPACE="agent/$TASK_ID"

# 1) Isolated environment (worktree/jj workspace + cloned DB + hooks)
OUTPUT=$(devflow --json --non-interactive switch -c "$WORKSPACE")

# 2) Worktree path = the agent's workdir for every subsequent tool call
WORKTREE=$(echo "$OUTPUT" | jq -r '.worktree_path // empty')
test -d "$WORKTREE"
cd "$WORKTREE"

# 3) Connection info
CONN=$(devflow --json service connection "$WORKSPACE" | jq -r '.connection_string')
export DATABASE_URL="$CONN"

# 4) Work …  then reset for retries:
devflow --json --non-interactive service reset "$WORKSPACE"

# 5) Cleanup from the primary checkout; a workspace cannot remove itself
PROJECT_ROOT=$(devflow --json list | jq -r '.project.root')
(cd "$PROJECT_ROOT" && devflow --json --non-interactive remove "$WORKSPACE" --force)
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

## Skills & context files

```bash
devflow agent skill      # install bundled workspace helper skills into .claude/skills/
```

The bundled workspace helpers teach agents to list/switch/create workspaces with the JSON contract. Context files follow open conventions:

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
        with:
          # devflow requires the primary checkout to have a named workspace.
          ref: ${{ github.head_ref }}
      - name: Install devflow
        run: |
          curl -fsSL https://raw.githubusercontent.com/clement-tourriere/devflow/main/scripts/install.sh | sh
          echo "$HOME/.local/bin" >> "$GITHUB_PATH"
      - name: Create preview environment
        run: |
          devflow --json --non-interactive init --name myapp
          devflow --json --non-interactive service add app-db --provider local --service-type postgres
          OUTPUT=$(DEVFLOW_APPROVE_HOOKS=1 devflow --json --non-interactive switch -c pr-${{ github.event.number }})
          WORKTREE=$(echo "$OUTPUT" | jq -r '.worktree_path // empty')
          test -d "$WORKTREE"
          echo "DEVFLOW_WORKTREE=$WORKTREE" >> "$GITHUB_ENV"
      - name: Migrate + test
        run: |
          CONN=$(devflow --json service connection pr-${{ github.event.number }} | jq -r '.connection_string')
          cd "$DEVFLOW_WORKTREE"
          DATABASE_URL="$CONN" npm run migrate
          DATABASE_URL="$CONN" npm test
      - name: Cleanup
        if: always()
        run: devflow --non-interactive remove pr-${{ github.event.number }} --force
```

Use `DEVFLOW_APPROVE_HOOKS=1` when CI should run configured lifecycle hooks. Without it, unapproved shell hooks are reported as skipped; `--no-verify` would skip every lifecycle hook, including non-shell setup actions.
