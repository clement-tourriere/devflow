# TUI Redesign Implementation Plan

## Overview
Implement the 3-phase plan: branch registry, TUI redesign from 6 tabs to 3 tabs, and tree visualization.

## Execution Order

### Phase 1a: DevflowBranch + Branch Registry in local_state.rs

**File:** `src/state/local_state.rs`

Add `DevflowBranch` struct after the `LocalState` struct:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevflowBranch {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}
```

Add `branches` field to `ProjectState`:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub branches: Option<Vec<DevflowBranch>>,
```

Add CRUD methods to `LocalStateManager`:

- `get_branches(&self, project_path: &Path) -> Vec<DevflowBranch>` - returns empty vec if none
- `register_branch(&mut self, project_path: &Path, branch: DevflowBranch) -> Result<()>` - upserts by name
- `unregister_branch(&mut self, project_path: &Path, name: &str) -> Result<()>` - removes by name
- `get_branch(&self, project_path: &Path, name: &str) -> Option<DevflowBranch>` - single branch lookup

Note: `set_current_branch` must be updated to preserve existing `branches` field (currently only preserves `services`).

### Phase 1b: Update handle_switch_command in cli.rs

**File:** `src/cli.rs`

In `handle_switch_command` (line ~3911): after successful switch, call `state_manager.register_branch()` with parent info from the current branch context. Also call `state_manager.set_current_branch()`.

### Phase 1c: Update handle_remove_command in cli.rs

**File:** `src/cli.rs`

In `handle_remove_command` (line ~4354): after successful branch removal, call `state_manager.unregister_branch()`.

### Phase 2a: Create environments.rs (Tree View Component)

**File:** `src/tui/components/environments.rs` (NEW)

This is the main component that replaces both `branches.rs` and `services.rs`.

**Data model:**
- Receives `BranchesData` (already has `EnrichedBranch` with parent info from services)
- Builds a tree structure from parent-child relationships
- Flattens tree into display rows with depth/indent info

**Tree node struct:**

```rust
struct TreeNode {
    branch: EnrichedBranch,
    depth: usize,
    is_last_sibling: bool,
    ancestor_has_next: Vec<bool>, // for drawing tree lines
    collapsed: bool,
}
```

**Layout:** Horizontal split - tree list (55%) | detail panel (45%)

**Tree rendering:** Unicode box chars (├──, └──, │) with depth-based indentation. Each node shows:
- Tree lines + branch name
- Service status badges `[svc:running]`
- Worktree path if present
- `*` marker for current branch

**Detail panel:** Shows for selected branch:
- Branch name, current/default status
- Worktree path
- All services with state, database, parent, connection info
- Available actions

**Key bindings:**
- j/k: Navigate tree
- Enter: Switch to branch
- c: Create branch
- d: Delete branch
- S: Start all services for branch
- x: Stop all services for branch
- R: Reset service (with service picker if multiple)
- l: View logs (with service picker)
- /: Filter
- Space: Collapse/expand node
- r: Refresh

### Phase 2b: Create system.rs (Consolidated System Tab)

**File:** `src/tui/components/system.rs` (NEW)

Consolidates Config + Hooks + Doctor into one tab with sub-sections.

**Layout:** Sub-section picker at top (1/2/3 or Tab to cycle), content below.

**Sub-sections:**
1. Config - embeds ConfigViewComponent rendering logic
2. Hooks - embeds HooksComponent rendering logic
3. Doctor - embeds DoctorComponent rendering logic

Each sub-section's data/state is maintained independently. The System component delegates key events to the active sub-section.

### Phase 2c: Update logs.rs

**File:** `src/tui/components/logs.rs`

Add a service/branch picker header when no logs are loaded:
- Show list of services from `BranchesData`
- Let user select service + branch to view logs
- Store a reference to available services/branches

New update handler: listen for `DataPayload::Branches` to populate the picker.

### Phase 2d: Rewire app.rs (6 tabs -> 3 tabs)

**File:** `src/tui/app.rs`

Replace:
- `branches: BranchesComponent` + `services: ServicesComponent` -> `environments: EnvironmentsComponent`
- `config_view` + `hooks_view` + `doctor` -> `system: SystemComponent`
- `logs: LogsComponent` stays

Update:
- `tab_names = vec!["Environments", "System", "Logs"]`
- All `match self.active_tab` blocks: 0=environments, 1=system, 2=logs
- `switch_tab` blur/focus: 3 cases instead of 6
- `dispatch_action`: send to 3 components
- `handle_key_event` delegation: 3 cases
- `render_content`: 3 cases
- Tab number keys: 1/2/3 only (remove 4/5/6)
- `ViewLogs` action switches to tab 2 (was 5)

### Phase 2e: Update context.rs

**File:** `src/tui/context.rs`

Update `DevflowContext::new()`:
- Read `current_branch` from `LocalStateManager` instead of relying on VCS snapshot's `is_current`
- Read branch registry and pass to `fetch_branches_bg` to build tree

Update `fetch_branches_bg`:
- Accept branch registry data
- Use registry for parent-child relationships (not just service parent data)
- Mark `is_current` based on LocalStateManager, not VCS HEAD

New methods:
- `register_branch(&mut self, name: &str, parent: Option<&str>, worktree: Option<&str>) -> Result<()>`
- `unregister_branch(&mut self, name: &str) -> Result<()>`
- `get_branch_registry(&self) -> Vec<DevflowBranch>`

### Phase 2f: Update theme.rs, action.rs, help.rs

**theme.rs:**
- Add tree drawing colors: `TREE_LINE`, `TREE_COLLAPSED`
- Update `tab_hints()` for 3-tab structure:
  - 0: "j/k:Navigate  Enter:Switch  c:Create  d:Delete  S:Start  x:Stop  Space:Expand  /:Filter  r:Refresh"
  - 1: "1:Config  2:Hooks  3:Doctor  j/k:Scroll  r:Refresh"
  - 2: "j/k:Scroll  g/G:Top/Bottom  PgUp/PgDn:Page  Tab:Pick service  r:Refresh"

**action.rs:**
- Add `CollapseToggle(String)` action for tree nodes
- Add `SelectSubSection(usize)` action for System tab
- Keep all existing actions (they're still needed)

**help.rs:**
- Update help popup sections for 3-tab structure
- Update section titles: "Environments Tab", "System Tab", "Logs Tab"

### Cleanup: Delete old components, update mod.rs

**Delete:**
- `src/tui/components/branches.rs`
- `src/tui/components/services.rs`

**Update `src/tui/components/mod.rs`:**
```rust
pub mod config_view;
pub mod doctor;
pub mod environments;
pub mod help;
pub mod hooks;
pub mod logs;
pub mod system;
```

Remove `pub mod branches;` and `pub mod services;`.

Note: `config_view.rs`, `hooks.rs`, `doctor.rs` stay as files but their `Component` trait impl may not be used directly by `app.rs` anymore — `system.rs` will use their internal logic (either by embedding the structs or copying the render methods).

### Build & Fix

Run `cargo build` and fix any compilation errors iteratively.

## File Change Summary

| File | Action |
|------|--------|
| `src/state/local_state.rs` | MODIFY - add DevflowBranch, branches field, CRUD methods |
| `src/cli.rs` | MODIFY - register/unregister branches on switch/remove |
| `src/tui/components/environments.rs` | CREATE - tree view main component |
| `src/tui/components/system.rs` | CREATE - consolidated config+hooks+doctor |
| `src/tui/app.rs` | MODIFY - 6 tabs -> 3 tabs |
| `src/tui/context.rs` | MODIFY - LocalStateManager integration |
| `src/tui/action.rs` | MODIFY - add new actions |
| `src/tui/theme.rs` | MODIFY - tree colors, 3-tab hints |
| `src/tui/components/help.rs` | MODIFY - update for 3-tab structure |
| `src/tui/components/logs.rs` | MODIFY - add service/branch picker |
| `src/tui/components/mod.rs` | MODIFY - swap module declarations |
| `src/tui/components/branches.rs` | DELETE |
| `src/tui/components/services.rs` | DELETE |
