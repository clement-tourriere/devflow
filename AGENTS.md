# devflow for AI Agents

This guide is for autonomous coding agents and CI runners.

## Goal

Use devflow to create an isolated development branch environment per task, with machine-readable output and deterministic behavior.

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
devflow --json --non-interactive switch "$BRANCH" --no-verify
devflow --json service connection "$BRANCH"
```

## Suggested Agent Loop

```bash
# 1) Create/switch isolated environment for this task
devflow --json --non-interactive switch "agent/$TASK_ID" --no-verify

# 2) Read connection info and run the task
CONN=$(devflow --json service connection "agent/$TASK_ID" | jq -r '.connection_string')

# 3) Optional reset for retries
devflow --json --non-interactive service reset "agent/$TASK_ID"

# 4) Cleanup when done
devflow --json --non-interactive service delete "agent/$TASK_ID"
```

## Automation Contract

- Multi-provider `service create`, `service delete`, and `switch` return non-zero exit code when any provider fails.
- `destroy` and `remove` require `--force` in `--non-interactive` or `--json` mode.
- Unapproved hooks fail in non-interactive mode.
- Use `devflow --json capabilities` for a machine-readable summary of guarantees.
