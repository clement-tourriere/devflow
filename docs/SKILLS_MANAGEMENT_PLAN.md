# Skills Management System for devflow

**Status:** Planning  
**Created:** 2026-03-07  
**Scope:** Full implementation - CLI, TUI, GUI, skills.sh marketplace

## Overview

Transform devflow into a skill management platform where users can browse, search, install, and manage skills from the skills.sh marketplace and local bundled skills.

## Goals

1. **Skill Discovery** - Browse and search skills from skills.sh marketplace
2. **Per-Project Management** - Install, remove, update skills per project
3. **Version Control** - Each project can use different skill versions independently
4. **Multi-Interface** - Full support in CLI, TUI, and GUI
5. **Bundled Skills** - 4 devflow-specific skills included with installation

## Architecture

### Directory Structure

```
~/.local/share/devflow/
└── skills/                           # Global versioned skill cache
    └── brainstorming/
        ├── 1.0.0/
        │   └── SKILL.md
        ├── 1.1.0/
        │   └── SKILL.md
        └── latest → 1.1.0/           # Symlink to latest version

~/.config/devflow/
├── settings.yml                      # Global settings (marketplace URL, etc.)
└── skills/
    └── cache.json                    # Cached marketplace metadata

<project>/
├── .agents/
│   └── skills/                       # Installed skills (symlinks)
│       └── brainstorming → ~/.local/share/devflow/skills/brainstorming/1.0.0
└── .devflow/
    └── skills.json                   # Project skill manifest
```

### Project Manifest Format

**`.devflow/skills.json`:**
```json
{
  "version": 1,
  "skills": {
    "brainstorming": {
      "source": {
        "type": "github",
        "owner": "obra",
        "repo": "superpowers",
        "path": "skills/brainstorming"
      },
      "installed_version": "1.0.0",
      "installed_at": "2026-03-07T12:00:00Z",
      "latest_version": "1.1.0",
      "update_available": true
    },
    "workspace-list": {
      "source": {
        "type": "bundled"
      },
      "installed_version": "1.0.0",
      "installed_at": "2026-03-07T10:00:00Z"
    }
  }
}
```

### Update Behavior

- `devflow skill update brainstorming` - Updates ONLY this project's symlink to new version
- Other projects remain on their current versions until explicitly updated
- New installs default to latest version available in cache
- Version pinning: `devflow skill install brainstorming --version 1.0.0`

---

## Components

### 1. Core Rust Module

**Location:** `crates/devflow-core/src/skills/`

| File | Purpose |
|------|---------|
| `mod.rs` | Module exports, public API |
| `types.rs` | `Skill`, `SkillSource`, `SkillManifest`, `InstalledSkill` |
| `bundled.rs` | 4 bundled skills with version metadata |
| `cache.rs` | Global skill cache management |
| `installer.rs` | Install/remove/update skills to projects |
| `manifest.rs` | Project skill manifest (skills.json) CRUD |
| `marketplace.rs` | skills.sh API client (search, fetch, metadata) |
| `resolver.rs` | Resolve skill from various sources (bundled, github, local) |

#### Core Types

```rust
/// A skill with its metadata and content
pub struct Skill {
    pub name: String,
    pub description: String,
    pub version: String,
    pub source: SkillSource,
    pub content: String,
    pub author: Option<String>,
    pub tags: Vec<String>,
    pub installs: Option<u64>,
}

/// Where a skill comes from
pub enum SkillSource {
    /// Embedded in devflow binary
    Bundled,
    /// GitHub repository
    GitHub {
        owner: String,
        repo: String,
        path: Option<String>,
    },
    /// Local filesystem path
    Local { path: PathBuf },
}

/// A skill installed in a project
pub struct InstalledSkill {
    pub name: String,
    pub source: SkillSource,
    pub installed_version: String,
    pub installed_at: DateTime<Utc>,
    pub latest_version: Option<String>,
    pub update_available: bool,
}

/// Project's skill manifest
pub struct SkillManifest {
    pub version: u32,
    pub skills: IndexMap<String, InstalledSkill>,
}

/// Search result from marketplace
pub struct SkillSearchResult {
    pub id: String,
    pub name: String,
    pub description: String,
    pub source: String,  // e.g., "obra/superpowers"
    pub version: String,
    pub installs: u64,
    pub tags: Vec<String>,
}
```

#### Bundled Skills

Four skills embedded in the devflow binary:

1. **brainstorming** - Design before implementation (from obra/superpowers)
2. **workspace-list** - List devflow workspaces
3. **workspace-create** - Create devflow workspace with isolated services
4. **workspace-switch** - Switch to existing devflow workspace

---

### 2. CLI Commands

**Location:** `src/cli/skill.rs`

#### Commands

```bash
# List skills
devflow skill list                    # List installed skills in project
devflow skill list --available        # List all available (bundled + marketplace)
devflow skill list --updates          # Only skills with updates available
devflow --json skill list             # JSON output

# Search marketplace
devflow skill search <query>          # Search skills.sh
devflow skill search "test driven" --limit 10
devflow --json skill search brainstorm

# Install skills
devflow skill install <name>          # Install by name (searches bundled then marketplace)
devflow skill install <owner/repo>    # Install from specific GitHub repo
devflow skill install <owner/repo/path>  # Install specific skill from repo
devflow skill install brainstorming --version 1.0.0  # Pin version

# Remove skills
devflow skill remove <name>           # Remove skill from project
devflow skill remove --all            # Remove all skills

# Update skills
devflow skill update                  # Update all skills with updates available
devflow skill update <name>           # Update specific skill
devflow skill update --check          # Check for updates without applying

# Show skill details
devflow skill show <name>             # Show installed skill details
devflow skill show <name> --remote    # Show from marketplace (not installed)
```

#### Output Examples

```
$ devflow skill list
Installed skills for my-project:

  NAME              VERSION   SOURCE              UPDATE
  brainstorming     1.0.0     obra/superpowers    1.1.0 available
  workspace-list    1.0.0     bundled             -
  workspace-create  1.0.0     bundled             -

Run `devflow skill update` to install available updates.
```

```
$ devflow skill search "test"
Searching skills.sh for "test"...

  NAME                     SOURCE                    INSTALLS
  test-driven-development  obra/superpowers          19.9K
  webapp-testing           anthropics/skills         19.6K
  backend-testing          supercent-io/skills...     9.4K
  testing-strategies       supercent-io/skills...     9.3K

Run `devflow skill install <name>` to install.
```

```
$ devflow skill install obra/superpowers/systematic-debugging
Installing systematic-debugging from obra/superpowers...

  ✓ Fetched skill content
  ✓ Cached to ~/.local/share/devflow/skills/systematic-debugging/1.0.0/
  ✓ Installed to .agents/skills/systematic-debugging
  ✓ Updated .devflow/skills.json

systematic-debugging v1.0.0 installed successfully.
```

---

### 3. TUI Integration

**Location:** `src/tui/components/`

#### 3.1 Doctor Integration

Add skills section to existing Doctor component:

```
┌─────────────────────────────────────────────────────────┐
│ Setup: my-project                                       │
├─────────────────────────────────────────────────────────┤
│                                                         │
│ General                                                 │
│   ✓ Config file        Found .devflow.yml              │
│   ✓ VCS repository     Detected git repository         │
│   ✓ VCS hooks          devflow hooks installed         │
│   ~ Agent skills       3 installed, 1 update available │
│                              [u] Update  [m] Manage     │
│                                                         │
│ Services                                                │
│   ✓ app-db (postgres)  Running on port 5432            │
│                                                         │
└─────────────────────────────────────────────────────────┘
```

#### 3.2 Skills Management Mode (press 'm')

```
┌─────────────────────────────────────────────────────────┐
│ Skills Management                      [s] Search  [q] ←│
├─────────────────────────────────────────────────────────┤
│                                                         │
│ Installed (3)                                           │
│   ● brainstorming        obra/superpowers    1.0.0 ⬆    │
│   ○ workspace-list       bundled            1.0.0      │
│   ○ workspace-create     bundled            1.0.0      │
│                                                         │
│ [i] Install  [r] Remove  [u] Update  [Enter] Details   │
│                                                         │
├─────────────────────────────────────────────────────────┤
│ Search: [________________]                              │
│                                                         │
│ Available from marketplace                              │
│   ○ systematic-debugging  obra/superpowers    24.1K ⬇  │
│   ○ writing-plans         obra/superpowers    22.3K ⬇  │
│   ○ test-driven-devel...  obra/superpowers    19.9K ⬇  │
│                                                         │
└─────────────────────────────────────────────────────────┘
```

#### Key Bindings

| Key | Action |
|-----|--------|
| `m` | Open skills management (from doctor) |
| `s` | Focus search bar |
| `i` | Install selected skill |
| `r` | Remove selected skill |
| `u` | Update selected skill (or all if none selected) |
| `Enter` | Show skill details |
| `q` | Close skills management / go back |
| `j/k` | Navigate skill list |
| `Tab` | Switch between installed/available lists |

---

### 4. GUI Implementation

**Location:** `ui/src/pages/skills/`

#### 4.1 Skills Marketplace Page

**File:** `ui/src/pages/skills/SkillsMarketplace.tsx`

```
┌────────────────────────────────────────────────────────────────────┐
│ Skills Marketplace                                                  │
│                                                                     │
│ ┌────────────────────────────────────────────────────────────────┐ │
│ │ 🔍 Search skills... (e.g., brainstorming, testing, react)     │ │
│ └────────────────────────────────────────────────────────────────┘ │
│                                                                     │
│ Trending                    Browse All    My Installed              │
│                                                                     │
│ ┌─────────────────┐ ┌─────────────────┐ ┌─────────────────┐       │
│ │ brainstorming   │ │ systematic-     │ │ test-driven-    │       │
│ │                 │ │ debugging       │ │ development     │       │
│ │ Design before   │ │                 │ │                 │       │
│ │ implementation  │ │ 4-phase root    │ │ RED-GREEN-      │       │
│ │                 │ │ cause process   │ │ REFACTOR cycle  │       │
│ │ obra/superpowers│ │ obra/superpowers│ │ obra/superpowers│       │
│ │ 43.2K installs  │ │ 24.1K installs  │ │ 19.9K installs  │       │
│ │ [Install]       │ │ [Install]       │ │ [Install]       │       │
│ └─────────────────┘ └─────────────────┘ └─────────────────┘       │
│                                                                     │
└────────────────────────────────────────────────────────────────────┘
```

#### 4.2 Project Skills Component

**File:** `ui/src/pages/skills/ProjectSkills.tsx`

```
┌────────────────────────────────────────────────────────────────────┐
│ Installed Skills                              [+ Add Skill]        │
│                                                                     │
│ ┌────────────────────────────────────────────────────────────────┐ │
│ │ Skill          │ Version │ Source            │ Status    │ ... │ │
│ ├────────────────────────────────────────────────────────────────┤ │
│ │ brainstorming  │ 1.0.0   │ obra/superpowers  │ ⬆ 1.1.0   │ [↑] │ │
│ │ workspace-list │ 1.0.0   │ bundled           │ -         │ [×] │ │
│ │ workspace-     │ 1.0.0   │ bundled           │ -         │ [×] │ │
│ │   create       │         │                   │           │     │ │
│ └────────────────────────────────────────────────────────────────┘ │
│                                                                     │
│ [Update All]                                         1 update ready │
│                                                                     │
└────────────────────────────────────────────────────────────────────┘
```

#### 4.3 Skill Card Component

**File:** `ui/src/components/SkillCard.tsx`

```
┌─────────────────────────────────┐
│ brainstorming            ★ 4.2  │
│                                 │
│ Design before implementation.   │
│ Explores user intent,           │
│ requirements and design before  │
│ implementation.                 │
│                                 │
│ obra/superpowers                │
│ v1.0.0            43.2K installs│
│                                 │
│ #methodology #design #planning  │
│                                 │
│         [Install]               │
└─────────────────────────────────┘
```

#### 4.4 Skill Detail Modal

```
┌────────────────────────────────────────────────────────────────────┐
│ brainstorming                                              [×]    │
├────────────────────────────────────────────────────────────────────┤
│                                                                    │
│ obra/superpowers · v1.0.0 · 43.2K installs                        │
│                                                                    │
│ You MUST use this before any creative work - creating features,   │
│ building components, adding functionality, or modifying behavior. │
│ Explores user intent, requirements and design before impl.        │
│                                                                    │
│ Tags: methodology, design, planning                               │
│                                                                    │
│ ───────────────────────────────────────────────────────────────── │
│                                                                    │
│ When to use:                                                       │
│ • Starting a new feature or component                             │
│ • Making significant changes to existing code                     │
│ • Before any implementation work                                  │
│                                                                    │
│ ───────────────────────────────────────────────────────────────── │
│                                                                    │
│                          [Install] [View on GitHub]               │
│                                                                    │
└────────────────────────────────────────────────────────────────────┘
```

#### 4.5 Routes

```tsx
// ui/src/App.tsx
<Route path="skills" element={<SkillsMarketplace />} />
<Route path="projects/*/skills" element={<ProjectSkills />} />
```

#### 4.6 Integration with Project Detail

Add Skills tab to existing ProjectDetail page:

```tsx
// ui/src/pages/projects/ProjectDetail.tsx

<Tabs>
  <Tab label="Services" />
  <Tab label="Workspaces" />
  <Tab label="Hooks" />
  <Tab label="Config" />
  <Tab label="Skills" badge={updatesAvailable > 0 ? updatesAvailable : undefined} />
  <Tab label="Setup" />
</Tabs>
```

---

### 5. Tauri Commands

**Location:** `src-tauri/src/commands/skills.rs`

```rust
/// Search skills.sh marketplace
#[tauri::command]
pub async fn search_skills(
    query: String,
    limit: Option<usize>,
) -> Result<Vec<SkillSearchResult>, String>

/// List all available skills (bundled + cached marketplace)
#[tauri::command]
pub async fn list_available_skills() -> Result<Vec<SkillInfo>, String>

/// List skills installed in a project
#[tauri::command]
pub async fn list_installed_skills(
    project_path: String,
) -> Result<Vec<InstalledSkill>, String>

/// Install a skill to a project
#[tauri::command]
pub async fn install_skill(
    project_path: String,
    skill_id: String,
    source: Option<SkillSourceInput>,
    version: Option<String>,
) -> Result<InstalledSkill, String>

/// Remove a skill from a project
#[tauri::command]
pub async fn remove_skill(
    project_path: String,
    skill_name: String,
) -> Result<(), String>

/// Update a skill in a project
#[tauri::command]
pub async fn update_skill(
    project_path: String,
    skill_name: String,
) -> Result<InstalledSkill, String>

/// Check for available updates
#[tauri::command]
pub async fn check_skill_updates(
    project_path: String,
) -> Result<Vec<SkillUpdateInfo>, String>

/// Get detailed skill information
#[tauri::command]
pub async fn get_skill_detail(
    skill_id: String,
    source: SkillSourceInput,
) -> Result<SkillDetail, String>

/// Clear skill cache
#[tauri::command]
pub async fn clear_skill_cache() -> Result<(), String>
```

---

### 6. Global Settings

**Location:** `~/.config/devflow/settings.yml`

```yaml
# Skill marketplace configuration
skills:
  # Marketplace URL (default: skills.sh)
  marketplace_url: "https://skills.sh"
  
  # Cache time-to-live in seconds (default: 1 hour)
  cache_ttl: 3600
  
  # Enable remote skill sources
  enable_remote: true
  
  # Default install method (symlink or copy)
  install_method: "symlink"
  
  # Auto-check for updates on project open
  auto_check_updates: true
```

**GUI Settings Page:**

```
┌────────────────────────────────────────────────────────────────────┐
│ Settings                                                           │
│                                                                    │
│ ─── Skill Marketplace ─────────────────────────────────────────── │
│                                                                    │
│ Marketplace URL                                                    │
│ ┌────────────────────────────────────────────────────────────────┐│
│ │ https://skills.sh                                              ││
│ └────────────────────────────────────────────────────────────────┘│
│                                                                    │
│ Enable remote skills                    [Toggle: ON]              │
│ Allow installing skills from marketplace                           │
│                                                                    │
│ Auto-check for updates                  [Toggle: ON]              │
│ Check for skill updates when opening a project                    │
│                                                                    │
│ Cache TTL (seconds)                     [3600]                    │
│ How long to cache marketplace data                                │
│                                                                    │
│ Default install method                  [Symlink ▼]               │
│ How to install skills to projects                                 │
│                                                                    │
└────────────────────────────────────────────────────────────────────┘
```

---

### 7. skills.sh API Integration

#### 7.1 Discovery

Based on the skills.sh CLI, skills are served from GitHub repositories. The marketplace provides:
- Leaderboard with search
- Skill metadata (name, description, installs)
- Links to source repositories

#### 7.2 Fetching Flow

```
install_skill("obra/superpowers/brainstorming")
    │
    ▼
1. Parse source identifier
   → GitHub { owner: "obra", repo: "superpowers", path: "brainstorming" }
    │
    ▼
2. Construct GitHub raw URL
   → https://raw.githubusercontent.com/obra/superpowers/main/skills/brainstorming/SKILL.md
    │
    ▼
3. Fetch SKILL.md content
    │
    ▼
4. Parse frontmatter for metadata
   → name, description, version
    │
    ▼
5. Determine version
   → Use frontmatter version, or derive from commit SHA
    │
    ▼
6. Save to cache
   → ~/.local/share/devflow/skills/brainstorming/{version}/SKILL.md
    │
    ▼
7. Create project symlink
   → .agents/skills/brainstorming → cache/{version}
    │
    ▼
8. Update manifest
   → .devflow/skills.json
```

#### 7.3 Marketplace Client

```rust
pub struct MarketplaceClient {
    base_url: String,
    http_client: reqwest::Client,
    cache: Arc<RwLock<MarketplaceCache>>,
}

impl MarketplaceClient {
    /// Search for skills on the marketplace
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<SkillSearchResult>>;
    
    /// Fetch skill content from GitHub
    pub async fn fetch_skill(&self, source: &SkillSource) -> Result<Skill>;
    
    /// Check for updates (compare installed vs latest)
    pub async fn check_updates(&self, skill: &InstalledSkill) -> Result<Option<String>>;
    
    /// Refresh cached marketplace data
    pub async fn refresh_cache(&self) -> Result<()>;
}
```

---

## Implementation Phases

### Phase 1: Core Infrastructure (2-3 days)

**Priority:** Critical  
**Dependencies:** None

**Tasks:**
- [ ] Create `crates/devflow-core/src/skills/` module structure
- [ ] Implement core types (`Skill`, `SkillSource`, `InstalledSkill`, `SkillManifest`)
- [ ] Implement bundled skills with version metadata
- [ ] Implement global skill cache management
- [ ] Implement project skill installer/remover
- [ ] Implement skill manifest CRUD
- [ ] Migrate existing `agent.rs` functions to use new module
- [ ] Write unit tests for core functionality

**Deliverables:**
- Working skills module with bundled skills
- Cache management functions
- Project manifest management
- All tests passing

---

### Phase 2: CLI Implementation (1-2 days)

**Priority:** High  
**Dependencies:** Phase 1

**Tasks:**
- [ ] Create `src/cli/skill.rs` with command structure
- [ ] Implement `skill list` command
- [ ] Implement `skill search` command (local only for now)
- [ ] Implement `skill install` command
- [ ] Implement `skill remove` command
- [ ] Implement `skill update` command
- [ ] Implement `skill show` command
- [ ] Add `--json` output support to all commands
- [ ] Update `devflow agent skill` to use new system
- [ ] Write integration tests

**Deliverables:**
- Full CLI skill management
- JSON output for scripting
- CLI tests passing

---

### Phase 3: skills.sh Integration (1-2 days)

**Priority:** High  
**Dependencies:** Phase 1

**Tasks:**
- [ ] Implement marketplace client (`marketplace.rs`)
- [ ] Implement skills.sh search API integration
- [ ] Implement GitHub skill fetching
- [ ] Implement version detection from GitHub
- [ ] Implement update checking
- [ ] Add marketplace search to CLI `skill search`
- [ ] Handle network errors gracefully
- [ ] Add caching for marketplace responses

**Deliverables:**
- Working skills.sh integration
- Remote skill installation
- Update detection

---

### Phase 4: TUI Integration (1 day)

**Priority:** Medium  
**Dependencies:** Phases 1, 2

**Tasks:**
- [ ] Add skills section to Doctor component
- [ ] Implement skills management mode
- [ ] Add search functionality to TUI
- [ ] Implement install/remove/update keybindings
- [ ] Add skill detail view
- [ ] Handle loading states
- [ ] Handle error states

**Deliverables:**
- Skills management in TUI
- Keyboard-driven workflow

---

### Phase 5: GUI Implementation (2-3 days) - **PRIORITY**

**Priority:** Critical  
**Dependencies:** Phases 1, 3

**Tasks:**
- [ ] Create `SkillsMarketplace.tsx` page
- [ ] Create `ProjectSkills.tsx` component
- [ ] Create `SkillCard.tsx` component
- [ ] Create `SkillDetailModal.tsx` component
- [ ] Add skill-related types to `types/index.ts`
- [ ] Add Tauri command wrappers to `utils/invoke.ts`
- [ ] Add routes to `App.tsx`
- [ ] Add Skills tab to `ProjectDetail.tsx`
- [ ] Implement search with debounce
- [ ] Implement install/remove/update buttons
- [ ] Implement loading states
- [ ] Implement error handling
- [ ] Add update notification badges

**Deliverables:**
- Full GUI skill management
- Marketplace browsing
- Per-project skill management

---

### Phase 6: Polish & Settings (1 day)

**Priority:** Medium  
**Dependencies:** All phases

**Tasks:**
- [ ] Add skill settings to global settings
- [ ] Create settings UI for marketplace configuration
- [ ] Implement update notifications in GUI
- [ ] Add "updates available" badges
- [ ] Implement auto-check for updates on project open
- [ ] Add clear cache functionality
- [ ] Write user documentation
- [ ] Final testing and bug fixes

**Deliverables:**
- Settings UI
- Update notifications
- Documentation

---

## File Changes Summary

### New Files

| Path | Description |
|------|-------------|
| `crates/devflow-core/src/skills/mod.rs` | Skills module entry point |
| `crates/devflow-core/src/skills/types.rs` | Core skill types |
| `crates/devflow-core/src/skills/bundled.rs` | Bundled skill definitions |
| `crates/devflow-core/src/skills/cache.rs` | Global cache management |
| `crates/devflow-core/src/skills/installer.rs` | Install/remove logic |
| `crates/devflow-core/src/skills/manifest.rs` | Project manifest management |
| `crates/devflow-core/src/skills/marketplace.rs` | skills.sh API client |
| `crates/devflow-core/src/skills/resolver.rs` | Skill source resolution |
| `src/cli/skill.rs` | CLI skill commands |
| `src-tauri/src/commands/skills.rs` | Tauri commands for skills |
| `ui/src/pages/skills/SkillsMarketplace.tsx` | Marketplace browsing page |
| `ui/src/pages/skills/ProjectSkills.tsx` | Project skills management |
| `ui/src/components/SkillCard.tsx` | Skill card component |
| `ui/src/components/SkillDetailModal.tsx` | Skill detail modal |
| `ui/src/hooks/useSkills.ts` | React hooks for skill management |

### Modified Files

| Path | Changes |
|------|---------|
| `crates/devflow-core/src/lib.rs` | Export skills module |
| `crates/devflow-core/src/agent.rs` | Refactor to use skills module |
| `src/cli/mod.rs` | Add SkillCommands enum |
| `src/tui/app.rs` | Add skill-related actions |
| `src/tui/components/doctor.rs` | Add skills UI section |
| `src-tauri/src/commands/mod.rs` | Export skills commands |
| `src-tauri/src/lib.rs` | Register skill Tauri commands |
| `ui/src/App.tsx` | Add skill routes |
| `ui/src/types/index.ts` | Add skill-related types |
| `ui/src/utils/invoke.ts` | Add skill Tauri command wrappers |
| `ui/src/pages/projects/ProjectDetail.tsx` | Add Skills tab |

---

## Estimated Timeline

| Phase | Duration | Dependencies | Can Parallelize With |
|-------|----------|--------------|---------------------|
| Phase 1: Core Infrastructure | 2-3 days | None | - |
| Phase 2: CLI | 1-2 days | Phase 1 | Phase 3 |
| Phase 3: skills.sh Integration | 1-2 days | Phase 1 | Phase 2 |
| Phase 4: TUI | 1 day | Phases 1, 2 | Phase 5 |
| Phase 5: GUI | 2-3 days | Phase 1, 3 | Phase 4 |
| Phase 6: Polish | 1 day | All phases | - |
| **Total** | **8-12 days** | | |

**Parallelization opportunities:**
- Phases 2 (CLI) and 3 (skills.sh) can run in parallel after Phase 1
- Phases 4 (TUI) and 5 (GUI) can run in parallel after their dependencies

---

## Success Criteria

1. **CLI Complete** - All skill commands work with `--json` output
2. **GUI Complete** - Full marketplace browsing and project management
3. **TUI Complete** - Keyboard-driven skill management in doctor
4. **Version Management** - Projects can use different skill versions independently
5. **skills.sh Working** - Can search, install, and update from marketplace
6. **Bundled Skills** - 4 devflow skills available without internet
7. **Settings UI** - Marketplace URL and options configurable
8. **Tests Passing** - Unit and integration tests for all components

---

## Future Enhancements (Out of Scope)

- Custom skill repositories (self-hosted marketplaces)
- Skill dependencies and conflict resolution
- Skill ratings and reviews in GUI
- Skill creation wizard (`devflow skill init`)
- Skill publishing to marketplace
- Team/organization skill collections
- Skill usage analytics
- Skill templates for common patterns
