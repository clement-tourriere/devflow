# Skills Management: TUI, GUI & CLI Integration Design

**Date:** 2026-03-08
**Status:** Approved
**Scope:** GUI marketplace page, TUI skills tab, legacy migration, CLI cleanup
**Branch:** `feature/skills-management`

## Context

The core skills module (Tasks 1-8) and CLI commands (Task 9) are complete. The skills system works standalone — `devflow skill list`, `devflow skill install owner/repo`, etc. function without `.devflow.yml`. All 18 unit tests pass.

**Current state:**

| Surface | New Skills System (`devflow skill`) | Legacy Agent Skills (`devflow agent skill`) |
|---------|--------------------------------------|---------------------------------------------|
| CLI     | 6 commands, `--json`, standalone     | Single install command                      |
| TUI     | Not integrated                       | One-button install in Doctor sub-tab        |
| GUI     | Not integrated                       | Install/update in ProjectDetail + DoctorPage |

**Goal:** Full integration across all three surfaces, with the GUI as the flagship experience.

## Architecture

### Shared Rust Backend

All three surfaces (CLI, TUI, GUI) call into the same `devflow_core::skills` module. No duplication of business logic.

```
┌─────────┐  ┌─────────┐  ┌──────────────┐
│   CLI   │  │   TUI   │  │ GUI (Tauri)  │
│ skill.rs│  │skills.rs│  │ commands/    │
└────┬────┘  └────┬────┘  │  skills.rs   │
     │            │        └──────┬───────┘
     └────────────┼───────────────┘
                  │
        ┌─────────▼─────────┐
        │  devflow_core::   │
        │  skills::*        │
        │  - marketplace    │
        │  - installer      │
        │  - manifest       │
        │  - cache          │
        │  - bundled        │
        └───────────────────┘
```

## GUI: Skills Marketplace Page

### Route

New route: `/skills` — accessible from the sidebar navigation.

Optional project scoping: `/skills?project=/path/to/project` — when accessed from a project context, shows installed skills for that project. When accessed without project context, uses cwd.

### Layout

Two-panel design (consistent with other devflow pages):

```
┌─────────────────────────────────────────────────────────────┐
│ [Search: ________________________] [🔍]                     │
├─────────────────┬───────────────────────────────────────────┤
│                 │                                           │
│  INSTALLED (3)  │  Skill Detail / Search Results            │
│                 │                                           │
│  ▸ workspace-   │  # devflow-workspace-list                 │
│    list         │                                           │
│    bundled      │  Source: bundled                           │
│                 │  Hash: f7971f3c1839                        │
│  workspace-     │  Installed: 2026-03-08 14:30              │
│    switch       │                                           │
│    bundled      │  ---                                      │
│                 │  ## When to use                            │
│  workspace-     │  - You need to see which workspaces...    │
│    create       │  - You want to check service statuses...  │
│    bundled      │                                           │
│                 │  ## Instructions                           │
│                 │  1. Run `devflow --json list`...           │
│                 │                                           │
├─────────────────┴───────────────────────────────────────────┤
│ ⚠ 1 update available                        [Update All]   │
└─────────────────────────────────────────────────────────────┘
```

When searching:

```
┌─────────────────────────────────────────────────────────────┐
│ [Search: brainstorming___________] [🔍]                     │
├─────────────────┬───────────────────────────────────────────┤
│                 │                                           │
│  SEARCH (5)     │  brainstorming                            │
│                 │  Source: obra/superpowers                  │
│  ▸ brainstorm-  │  Installs: 43.4K                          │
│    ing          │                                           │
│    obra/super.. │  [Install]                                │
│    43.4K        │                                           │
│                 │                                           │
│  fun-brain-     │                                           │
│    storming     │                                           │
│    roin-orca/.. │                                           │
│    14.7K        │                                           │
│                 │                                           │
│                 │                                           │
│                 │                                           │
├─────────────────┼───────────────────────────────────────────┤
│  [← Back to installed]                                      │
└─────────────────────────────────────────────────────────────┘
```

### Components

- `SkillsMarketplace.tsx` — main page component with state management
- Uses existing `Modal.tsx` for confirmation dialogs (remove skill)
- Uses `react-markdown` for rendering SKILL.md content as rich markdown
- Search is debounced (300ms) to avoid excessive API calls

### State

Component-local `useState` (consistent with existing pages):

```typescript
// Data
const [installed, setInstalled] = useState<InstalledSkillInfo[]>([]);
const [searchResults, setSearchResults] = useState<SkillSearchResult[]>([]);
const [selectedSkill, setSelectedSkill] = useState<string | null>(null);
const [skillContent, setSkillContent] = useState<string | null>(null);

// UI state
const [searchQuery, setSearchQuery] = useState("");
const [isSearching, setIsSearching] = useState(false);
const [mode, setMode] = useState<"installed" | "search">("installed");
const [loading, setLoading] = useState(true);
const [actionLoading, setActionLoading] = useState<string | null>(null);
const [error, setError] = useState<string | null>(null);
const [updateAvailable, setUpdateAvailable] = useState(false);
```

### Tauri Commands (Rust)

New file: `src-tauri/src/commands/skills.rs`

```rust
#[tauri::command]
pub async fn skill_list(project_path: String) -> Result<Vec<InstalledSkillInfo>, String>

#[tauri::command]
pub async fn skill_search(query: String, limit: usize) -> Result<Vec<SkillSearchResult>, String>

#[tauri::command]
pub async fn skill_install(project_path: String, identifier: String) -> Result<String, String>

#[tauri::command]
pub async fn skill_remove(project_path: String, name: String) -> Result<(), String>

#[tauri::command]
pub async fn skill_update(project_path: String, name: Option<String>) -> Result<Vec<String>, String>

#[tauri::command]
pub async fn skill_show(project_path: String, name: String) -> Result<SkillDetail, String>

#[tauri::command]
pub async fn skill_check_updates(project_path: String) -> Result<Vec<String>, String>
```

### TypeScript Types

```typescript
export interface InstalledSkillInfo {
  name: string;
  source: SkillSource;
  content_hash: string;
  installed_at: string;
}

export interface SkillSource {
  type: "bundled" | "github";
  owner?: string;
  repo?: string;
  path?: string;
}

export interface SkillSearchResult {
  name: string;
  source: string;
  installs: number;
  skill_id?: string;
}

export interface SkillDetail {
  name: string;
  source: SkillSource;
  content_hash: string;
  installed_at: string;
  content: string;  // raw SKILL.md content
}
```

### Sidebar Navigation

Add to Layout.tsx sidebar, in the "Tools" or "App" section:

```typescript
<NavLink to="/skills" className={navLinkClass}>
  Skills
</NavLink>
```

### Dependency

Add `react-markdown` to `ui/package.json` for rich SKILL.md rendering.

## TUI: Skills Tab

### Tab Registration

New tab at index 5: "Skills" (key `6` / `F6`).

### Layout

Two-panel horizontal split (30/70), consistent with Services and Logs tabs:

```
┌─ Skills (3) ───────────┬─ devflow-workspace-list ─────────────────┐
│                        │                                           │
│ >> workspace-list      │  Source: bundled                          │
│    workspace-switch    │  Hash:   f7971f3c1839                    │
│    workspace-create    │  Installed: 2026-03-08 14:30             │
│                        │                                           │
│                        │  ---                                      │
│                        │                                           │
│                        │  ## When to use                           │
│                        │                                           │
│                        │  - You need to see which workspaces...   │
│                        │  - You want to check service statuses... │
│                        │                                           │
│                        │  ## Instructions                          │
│                        │                                           │
│                        │  1. Run `devflow --json list`...         │
│                        │                                           │
├────────────────────────┴───────────────────────────────────────────┤
│ j/k:Navigate  /:Search  d:Remove  u:Update  U:Update All  r:Refresh│
└────────────────────────────────────────────────────────────────────┘
```

Search mode (after pressing `/`):

```
┌─ Search: "brainstorm" ─┬─ brainstorming ──────────────────────────┐
│                        │                                           │
│ >> brainstorming       │  Source: obra/superpowers                 │
│    43.4K installs      │  Installs: 43,417                        │
│                        │                                           │
│    fun-brainstorming   │  Press Enter to install                  │
│    14.7K installs      │                                           │
│                        │                                           │
│    brainstorming       │                                           │
│    612 installs        │                                           │
│                        │                                           │
├────────────────────────┴───────────────────────────────────────────┤
│ j/k:Navigate  Enter:Install  Esc:Back  r:Refresh                  │
└────────────────────────────────────────────────────────────────────┘
```

### Component

New file: `src/tui/components/skills.rs`

```rust
pub struct SkillsComponent {
    installed: Vec<InstalledSkillEntry>,
    search_results: Vec<SearchResultEntry>,
    selected_index: usize,
    list_state: ListState,
    mode: SkillsMode,  // Installed | Search
    search_query: String,
    detail_scroll: u16,
    loading: bool,
    skill_content: Option<String>,
}

enum SkillsMode {
    Installed,
    Search,
}
```

### New Actions

In `action.rs`:

```rust
// New DataPayload variant
DataPayload::Skills(SkillsData),

// New SkillsData struct
pub struct SkillsData {
    pub installed: Vec<InstalledSkillEntry>,
}

pub struct InstalledSkillEntry {
    pub name: String,
    pub source_label: String,
    pub content_hash: String,
    pub installed_at: String,
    pub content: Option<String>,
}

pub struct SearchResultEntry {
    pub name: String,
    pub source: String,
    pub installs: u64,
}

// New action variants
Action::SkillSearch { query: String },
Action::SkillSearchResults(Vec<SearchResultEntry>),
Action::SkillInstall { identifier: String },
Action::SkillRemove { name: String },
Action::SkillUpdate { name: Option<String> },
Action::SkillDetail { name: String, content: String },
```

### Key Bindings

| Key | Mode | Action |
|-----|------|--------|
| `j` / `↓` | Both | Navigate down |
| `k` / `↑` | Both | Navigate up |
| `J` | Both | Scroll detail down |
| `K` | Both | Scroll detail up |
| `/` | Installed | Enter search mode (ShowInput dialog) |
| `Enter` | Search | Install selected search result |
| `d` | Installed | Remove selected skill (ShowConfirm) |
| `u` | Installed | Update selected skill |
| `U` | Installed | Update all skills |
| `r` | Both | Refresh data |
| `Esc` | Search | Return to installed mode |

### Background Tasks

All network operations go through `bg_tx`:

```rust
// In app.rs
fn spawn_skill_search(&self, query: String) {
    let tx = self.bg_tx.clone();
    tokio::spawn(async move {
        match marketplace::search(&query, 20).await {
            Ok(results) => {
                let entries = results.into_iter().map(|r| SearchResultEntry { ... }).collect();
                let _ = tx.send(Action::SkillSearchResults(entries));
            }
            Err(e) => { let _ = tx.send(Action::Error(e.to_string())); }
        }
    });
}
```

## Legacy Migration

### Tauri Commands

Update existing `install_agent_skills`, `check_agent_skills`, `uninstall_agent_skills` in `src-tauri/src/commands/services.rs` to delegate to `devflow_core::skills::installer::*` instead of `devflow_core::agent::*`.

### ProjectDetail Page

Update the skill status banner (lines 601-647) to call new `skill_check_updates` command. Update the "Update" button to call `skill_update`.

### DoctorPage

Update the "Agent skills" section (lines 150-241) to use new commands. The install button should call `skill_install` for bundled skills.

### TUI Doctor Sub-Tab

Update `Action::InstallAgentSkills` handling in `app.rs` to call `devflow_core::skills::installer::install_bundled_skills` instead of `devflow_core::agent::install_agent_skills`.

## CLI Cleanup

Minimal — update `devflow agent skill` help text:

```rust
#[command(
    about = "Install bundled workspace skills (use `devflow skill` for full management)",
    ...
)]
```

## Dependencies

### GUI
- `react-markdown` — SKILL.md rendering (new npm dependency)

### TUI
- No new Rust dependencies (ratatui already available)

### Tauri
- No new Rust dependencies (skills feature already in default features)

## File Changes Summary

### New Files
- `src-tauri/src/commands/skills.rs` — Tauri skill commands
- `ui/src/pages/skills/SkillsMarketplace.tsx` — GUI marketplace page
- `src/tui/components/skills.rs` — TUI skills tab component

### Modified Files
- `src-tauri/src/main.rs` — register new Tauri commands
- `src-tauri/src/commands/mod.rs` — add `pub mod skills;`
- `src-tauri/src/commands/services.rs` — migrate legacy commands
- `ui/src/App.tsx` — add `/skills` route
- `ui/src/components/Layout.tsx` — add sidebar nav link
- `ui/src/utils/invoke.ts` — add invoke wrappers
- `ui/src/types/index.ts` — add TypeScript types
- `ui/package.json` — add `react-markdown` dependency
- `ui/src/pages/projects/ProjectDetail.tsx` — update skill banner
- `ui/src/pages/doctor/DoctorPage.tsx` — update skill section
- `src/tui/app.rs` — add Skills tab (7 integration points)
- `src/tui/action.rs` — add Skills actions and data payloads
- `src/tui/components/mod.rs` — add `pub mod skills;`
- `src/tui/theme.rs` — add tab hint for index 5
- `src/cli/mod.rs` — update agent skill help text

## Non-Goals

- No skill authoring/publishing from within devflow (use skills.sh directly)
- No skill dependency management (skills are independent)
- No skill marketplace account/auth (anonymous read-only)
