---
title: Worktrees
description: How devflow manages Git worktrees — path templates, file copying, AI tool configs, CoW acceleration, and safety guarantees.
sidebar:
  order: 2
---

Git worktrees let you have multiple refs checked out simultaneously in different directories. They are devflow's only Git workspace model: the primary checkout is the default workspace, and `devflow switch` materializes every additional workspace as a linked worktree. Jujutsu projects use equivalent native workspaces.

## Why worktrees?

- **True parallel development** — work on two features at once without stashing.
- **Instant context switching** — switching workspace = changing directory; no rebuild, each worktree keeps its own `node_modules`, build cache, virtualenv.
- **PR reviews without disruption** — check out a review in a new worktree while your feature (and its database) keeps running.
- **Parallel AI agents** — each agent task gets its own directory *and* its own database; agents can't trample each other. See [AI agents](/devflow/guides/ai-agents/).

## Configuration

```yaml
worktree:
  path_template: "../{repo}.{workspace}"  # where worktrees are created
  copy_files:                             # files/dirs copied from the main worktree
    - .env.local
    - .env
  copy_ignored: false                     # also copy gitignored entries (node_modules, …)
  copy_ai_configs: true                   # copy .claude/, .cursor/, .opencode/, .agents/
  extra_ai_dirs: []                       # additional AI tool dirs to copy
```

| Field | Default | Effect |
| --- | --- | --- |
| `path_template` | `../{repo}.{workspace}` | Placeholders: `{repo}` (config `name:` or project directory name), `{workspace}` (collision-safe service key), `{branch}` (legacy alias for `{workspace}`). Relative to the project root. |
| `copy_files` | `[.env, .env.local]` | Files **or directories** copied from the main worktree into each new one when present. Reflink/CoW copy when the filesystem supports it. |
| `copy_ignored` | `false` | Copies gitignored entries too — as collapsed top-level entries (`node_modules` as one unit, not file-by-file), in parallel. Great for warm caches; costs disk on non-CoW filesystems. |
| `copy_ai_configs` | `true` | Copies AI tool config dirs (`.claude`, `.cursor`, `.opencode`, `.agents`) so agents and editors keep their settings in every worktree. |
| `extra_ai_dirs` | `[]` | Additional directories to treat like AI config dirs. |

### Path normalization

`{workspace}` uses the collision-safe `service_key` ([identity details](/devflow/concepts/workspaces/#workspace-identity)), not the raw VCS name.

```
workspace feature/Auth  +  template ../{repo}.{workspace}
→ ../my-project.feature_auth_cc2526bd757f
```

:::note
The exact suffix is implementation-defined; consume `worktree_path` from command output or inventory instead of calculating it yourself.
:::

## What happens on creation

`devflow switch -c feature/x`:

1. Reuses the existing worktree if one is already checked out for that branch.
2. Creates the VCS ref if needed (from `--from <parent>` or your current context).
3. Creates the worktree via libgit2 — tracked files only, instant. Stale worktree metadata for the same name is pruned automatically when its directory no longer exists.
4. Copies `copy_files`, then AI config dirs, then (if `copy_ignored`) gitignored entries — all with parallel reflink copies (APFS clones / Btrfs-XFS reflinks; full copy elsewhere).
5. Registers the raw name, collision-safe service key, immutable creation parent, and worktree path in local state.
6. Creates/switches service workspaces and runs hooks **inside the new worktree** — `post-create` hooks like `npm ci` or write-env target the right directory.
7. Emits `DEVFLOW_CD=<path>` so the [shell wrapper](/devflow/getting-started/shell-integration/) moves you there.

## Hooks are worktree-aware

- Hook **working directory** is the target workspace directory.
- `{{ worktree_path }}` is available in templates.
- `is_worktree` / `not_worktree` [conditions](/devflow/reference/hooks/#conditions) let hooks opt in or out of worktree context.

## Manually created worktrees

`git worktree add ../myapp.hotfix hotfix` works too: the devflow post-checkout hook detects worktree context and runs the same setup (file copying + service workspace creation + hooks). To trigger it explicitly from inside a worktree:

```bash
devflow worktree-setup
```

:::caution
The hook-driven setup path currently copies `copy_files` and gitignored entries but **not** AI config dirs — run `devflow switch <workspace>` once (or copy `.claude/` manually) if your agents need their configs in a hand-made worktree.
:::

## Safety on removal

`devflow remove <ws>` and GUI/TUI deletion run a non-mutating preflight first. They refuse dirty worktrees and protect the default/current workspace; `--force` explicitly accepts dirty-worktree or partial-cleanup risk.

After preflight, devflow runs removal hooks while the directory still exists, stops processes, and deletes service instances. Only after those steps succeed does it remove the worktree, delete its VCS ref, and unregister state. A service deletion failure therefore leaves the code and worktree available for retry. GUI force deletion is a separate second confirmation.

## Syncing AI configs back

Changes agents make to `.claude/settings.local.json` (approved permissions, etc.) in a worktree can be synced back to the main checkout:

```bash
devflow sync-ai-configs
```

Permission arrays are unioned and deduplicated; other AI config files are copied only if missing in main. The `sync-ai-configs` [hook recipe](/devflow/reference/hooks/#recipes) automates this before workspace removal.

## Related

- [Worktree workflows](/devflow/guides/worktrees/) — daily flow, multiplexer sessions, pruning, troubleshooting
- [Reference: configuration](/devflow/reference/configuration/#worktree)
