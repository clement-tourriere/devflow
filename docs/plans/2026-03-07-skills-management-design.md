# Skills Management System Design

**Status:** Approved
**Created:** 2026-03-07
**Scope:** Core module + CLI (Phase 1 & 2). TUI/GUI deferred.

## Decisions

- Full Rust reimplementation (no wrapping of `npx skills`)
- Content-hash versioning (not semver)
- 3 workspace skills embedded as offline fallback + published in `skills/` at repo root
- No bundled third-party skills (brainstorming etc. are recommended, not shipped)
- No migration from old system (clean break)
- skills.sh search API: `GET /api/search?q=<query>&limit=<n>`
- Skill content fetched from GitHub raw URLs

## Architecture

### Directory Layout

```
# Global cache (one per machine)
~/.local/share/devflow/skills/
  {owner}/{repo}/{skill-name}/{content-hash-prefix}/SKILL.md

# Per-project
<project>/
  .agents/skills/{name} -> symlink to cache
  .claude/skills/{name} -> ../../.agents/skills/{name}
  .devflow/skills.lock   # committed lock file
```

### Lock File (`.devflow/skills.lock`)

```json
{
  "version": 1,
  "skills": {
    "devflow-workspace-list": {
      "source": { "type": "bundled" },
      "content_hash": "a1b2c3...",
      "installed_at": "2026-03-07T12:00:00Z"
    },
    "brainstorming": {
      "source": { "type": "github", "owner": "obra", "repo": "superpowers", "path": "skills/brainstorming" },
      "content_hash": "f6e5d4...",
      "installed_at": "2026-03-07T14:00:00Z"
    }
  }
}
```

### Fetching Flow

1. Search: `GET https://skills.sh/api/search?q=<query>&limit=<n>`
2. Content: `GET https://raw.githubusercontent.com/{owner}/{repo}/main/skills/{name}/SKILL.md`
3. Hash content (SHA-256), cache to `~/.local/share/devflow/skills/...`
4. Symlink `.agents/skills/{name}` -> cache path
5. Symlink `.claude/skills/{name}` -> `../../.agents/skills/{name}`
6. Update `.devflow/skills.lock`

### Multi-Agent Support

`.agents/skills/` is canonical. Symlinks created for `.claude/skills/` only (other agents read `.agents/skills/` natively).

### Error Handling

- Offline: cache-only mode; embedded fallback for bundled skills
- GitHub rate limits: 60 req/hr unauthenticated, cache aggressively (1h TTL), support GITHUB_TOKEN
- Partial failures: atomic install (cache write, then symlink)

## CLI Commands

```
devflow skill list [--available] [--updates]
devflow skill search <query> [--limit N]
devflow skill install <identifier> [--skill <name>]
devflow skill remove <name>
devflow skill update [<name>] [--check]
devflow skill show <name> [--remote]
```

All commands support `--json` output.
