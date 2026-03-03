# devflow for AI Agents

This guide is for autonomous coding agents and CI runners.

## Goal

Use devflow to create an isolated development workspace environment per task, with machine-readable output and deterministic behavior.

## Recommended Flags

- `--json`: structured output on stdout
- `--non-interactive`: disable prompts in automation
- `--no-verify` on `switch`: skip lifecycle hooks when approval prompts are possible

## Bootstrap a Repository

```bash
./examples/agent-bootstrap.sh
```

Equivalent manual flow:

```bash
devflow --json --non-interactive init "$(basename "$PWD")"
devflow --json install-hooks
devflow --json capabilities
```

## Start Work on a New Subject

```bash
TASK_ID="issue-123"
./examples/agent-task.sh "$TASK_ID"
```

Equivalent manual flow:

```bash
BRANCH="agent/$TASK_ID"
devflow --json --non-interactive switch -c "$BRANCH" --no-verify
devflow --json service connection "$BRANCH"
```

## Agent Commands

devflow includes built-in agent management commands:

```bash
# Start an AI agent in an isolated workspace (launches in tmux if available)
devflow agent start fix-login -- 'Fix the login timeout bug'
devflow agent start fix-login --command codex
devflow agent start fix-login --dry-run          # Preview without executing

# Check agent workspaces
devflow agent status
devflow --json agent status

# Get project context (workspace info, services, connections)
devflow agent context
devflow agent context --format json
devflow agent context --workspace feature/auth

# Generate AI tool skills/rules
devflow agent skill                               # All tools
devflow agent skill --target claude               # .claude/skills/devflow/SKILL.md
devflow agent skill --target cursor               # .cursor/rules/devflow.md
devflow agent skill --target opencode             # AGENTS.md

# Generate AGENTS.md
devflow agent docs
```

## Hook Inspection

Agents can inspect hooks and template variables without running them:

```bash
# Show all template variables for the current workspace
devflow hook vars
devflow --json hook vars

# Render a template string
devflow hook render "DATABASE_URL={{ service['app-db'].url }}"

# Explain what a hook phase does
devflow hook explain post-create
```

## Suggested Agent Loop

```bash
# 1) Create/switch isolated environment for this task
devflow --json --non-interactive switch -c "agent/$TASK_ID" --no-verify

# 2) Read connection info and run the task
CONN=$(devflow --json service connection "agent/$TASK_ID" | jq -r '.connection_string')

# 3) Optional reset for retries
devflow --json --non-interactive service reset "agent/$TASK_ID"

# 4) Cleanup when done
devflow --json --non-interactive service delete "agent/$TASK_ID"
```

## AI Commit Messages

```bash
# Generate commit message via external CLI tool (preferred)
devflow commit --ai

# Configure in .devflow.yml:
# commit:
#   generation:
#     command: "claude -p --model haiku"
#
# Or via environment:
# DEVFLOW_COMMIT_COMMAND="claude -p --model haiku"
# DEVFLOW_LLM_API_KEY=sk-...  (OpenAI-compatible API fallback)
```

## Automation Contract

- Multi-provider `service create`, `service delete`, and `switch` return non-zero exit code when any provider fails.
- `destroy` and `remove` require `--force` in `--non-interactive` or `--json` mode.
- Unapproved hooks fail in non-interactive mode.
- Use `devflow --json capabilities` for a machine-readable summary of guarantees.
