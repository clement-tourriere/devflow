# GUI Skills Tab Bug Fixes Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Fix 4 bugs in the ProjectSkillsTab GUI component: missing descriptions, same-name highlighting collision, empty default view, and broken confirm dialog.

**Architecture:** All 4 fixes are UI-focused. Bug 2 requires adding the `id` field through the Rust Tauri backend. Bug 1 requires fetching SKILL.md from GitHub when a search result is selected (on-demand). Bug 3 auto-loads popular skills on mount. Bug 4 replaces `confirm()` with the existing `ConfirmDialog` component.

**Tech Stack:** React 18, TypeScript, Tauri v2, Rust, skills.sh API

---

### Task 1: Add `id` field to SkillSearchResult (Rust + TS)

The skills.sh API returns a unique `id` field (e.g. `"github/awesome-copilot/git-commit"`) that we currently discard. We need it as a unique key in the UI.

**Files:**
- Modify: `src-tauri/src/commands/skills.rs:26-30` (SkillSearchResult struct)
- Modify: `src-tauri/src/commands/skills.rs:57-64` (skill_search mapping)
- Modify: `ui/src/types/index.ts:313-317` (SkillSearchResult interface)

**Step 1: Add `id` field to Rust SkillSearchResult**

In `src-tauri/src/commands/skills.rs`, change:
```rust
#[derive(Serialize)]
pub struct SkillSearchResult {
    pub name: String,
    pub source: String,
    pub installs: u64,
}
```
to:
```rust
#[derive(Serialize)]
pub struct SkillSearchResult {
    pub id: String,
    pub name: String,
    pub source: String,
    pub installs: u64,
}
```

**Step 2: Pass `id` through in skill_search mapping**

In the same file, change the mapping in `skill_search`:
```rust
.map(|r| SkillSearchResult {
    name: r.name,
    source: r.source,
    installs: r.installs,
})
```
to:
```rust
.map(|r| SkillSearchResult {
    id: r.id,
    name: r.name,
    source: r.source,
    installs: r.installs,
})
```

**Step 3: Add `id` field to TypeScript SkillSearchResult**

In `ui/src/types/index.ts`, change:
```typescript
export interface SkillSearchResult {
  name: string;
  source: string;
  installs: number;
}
```
to:
```typescript
export interface SkillSearchResult {
  id: string;
  name: string;
  source: string;
  installs: number;
}
```

---

### Task 2: Fix same-name highlighting in search results

Use the new `id` field as the unique key for selection and React `key` prop.

**Files:**
- Modify: `ui/src/pages/projects/ProjectSkillsTab.tsx`

**Step 1: Change selectedSkill state type semantics**

The `selectedSkill` state already holds a `string | null`. In "installed" mode it stores the skill name (which is unique per-project). In "search" mode, we now store the `id` instead.

In `ProjectSkillsTab.tsx`, change the search result list item's `onClick` and highlighting (around line 240-271):

Change:
```tsx
key={r.name + r.source}
onClick={() => setSelectedSkill(r.name)}
...
background:
  selectedSkill === r.name
    ? "var(--bg-tertiary)"
    : "transparent",
```
to:
```tsx
key={r.id}
onClick={() => setSelectedSkill(r.id)}
...
background:
  selectedSkill === r.id
    ? "var(--bg-tertiary)"
    : "transparent",
```

**Step 2: Fix the detail panel search result lookup**

Around line 362, change:
```tsx
const result = searchResults.find((r) => r.name === selectedSkill);
```
to:
```tsx
const result = searchResults.find((r) => r.id === selectedSkill);
```

**Step 3: Fix the install button identifier**

The `handleInstall` call currently uses `result.source + "/" + result.name`. This is correct because `source` is already `"owner/repo"` format. No change needed here.

---

### Task 3: Show descriptions for search results

The skills.sh API does NOT return descriptions. We need to fetch the SKILL.md from GitHub on-demand when a search result is selected, parse its frontmatter, and extract the description.

**Files:**
- Modify: `src-tauri/src/commands/skills.rs` — add new `skill_search_detail` command
- Modify: `src-tauri/src/commands/mod.rs` — register new command
- Modify: `src-tauri/src/main.rs` — register in generate_handler
- Modify: `ui/src/utils/invoke.ts` — add `skillSearchDetail` wrapper
- Modify: `ui/src/types/index.ts` — add `SkillSearchDetail` type
- Modify: `ui/src/pages/projects/ProjectSkillsTab.tsx` — fetch + display description

**Step 1: Add SkillSearchDetail struct and command in Rust**

In `src-tauri/src/commands/skills.rs`, add after `SkillSearchResult`:
```rust
#[derive(Serialize)]
pub struct SkillSearchDetail {
    pub name: String,
    pub source: String,
    pub description: String,
    pub content: String,
}
```

Add new command:
```rust
#[tauri::command]
pub async fn skill_search_detail(source: String, name: String) -> Result<SkillSearchDetail, String> {
    // source is "owner/repo" format
    let parts: Vec<&str> = source.split('/').collect();
    if parts.len() != 2 {
        return Err(format!("Invalid source '{}'. Expected 'owner/repo'.", source));
    }
    let (owner, repo) = (parts[0], parts[1]);
    let skill = marketplace::fetch_skill(owner, repo, &name)
        .await
        .map_err(crate::commands::format_error)?;

    Ok(SkillSearchDetail {
        name: skill.name,
        source,
        description: skill.description,
        content: skill.content,
    })
}
```

**Step 2: Register the new command**

In `src-tauri/src/main.rs`, add `skills::skill_search_detail` to the `generate_handler!` macro.

**Step 3: Add TypeScript types and invoke wrapper**

In `ui/src/types/index.ts`, add:
```typescript
export interface SkillSearchDetail {
  name: string;
  source: string;
  description: string;
  content: string;
}
```

In `ui/src/utils/invoke.ts`, add:
```typescript
export const skillSearchDetail = (source: string, name: string) =>
  invoke<SkillSearchDetail>("skill_search_detail", { source, name });
```

**Step 4: Fetch and display description in ProjectSkillsTab**

Add state for search detail:
```typescript
const [searchDetail, setSearchDetail] = useState<SkillSearchDetail | null>(null);
const [searchDetailLoading, setSearchDetailLoading] = useState(false);
```

Add effect to load detail when a search result is selected:
```typescript
useEffect(() => {
  if (!selectedSkill || mode !== "search") {
    setSearchDetail(null);
    return;
  }
  const result = searchResults.find((r) => r.id === selectedSkill);
  if (!result) {
    setSearchDetail(null);
    return;
  }
  setSearchDetailLoading(true);
  skillSearchDetail(result.source, result.name)
    .then(setSearchDetail)
    .catch(() => setSearchDetail(null))
    .finally(() => setSearchDetailLoading(false));
}, [selectedSkill, mode, searchResults]);
```

Update the right panel for search mode to show description and content preview:
```tsx
{mode === "search" && selectedSkill ? (
  (() => {
    const result = searchResults.find((r) => r.id === selectedSkill);
    if (!result) return <div style={{ color: "var(--text-muted)" }}>Select a skill</div>;
    return (
      <div>
        <h2 style={{ margin: "0 0 8px 0", fontSize: 18 }}>{result.name}</h2>
        <div style={{ fontSize: 13, color: "var(--text-secondary)", marginBottom: 12 }}>
          Source: {result.source} · {formatInstalls(result.installs)} installs
        </div>
        {searchDetail?.description && (
          <p style={{ fontSize: 13, color: "var(--text-primary)", marginBottom: 12 }}>
            {searchDetail.description}
          </p>
        )}
        <button
          className="btn btn-primary"
          onClick={() => handleInstall(result.source + "/" + result.name)}
          disabled={actionLoading === `install:${result.source}/${result.name}`}
          style={{ marginBottom: 16 }}
        >
          {actionLoading === `install:${result.source}/${result.name}`
            ? "Installing..."
            : "Install"}
        </button>
        {searchDetailLoading ? (
          <div style={{ color: "var(--text-muted)", fontSize: 13, marginTop: 16 }}>Loading preview...</div>
        ) : searchDetail?.content ? (
          <div
            style={{
              borderTop: "1px solid var(--border)",
              paddingTop: 16,
              fontSize: 14,
              lineHeight: 1.6,
            }}
            className="skill-content"
          >
            <Markdown>{searchDetail.content}</Markdown>
          </div>
        ) : null}
      </div>
    );
  })()
)
```

Also show description snippet in the left-panel search result list items (after source line):
```tsx
{searchDetail && searchDetail.name === r.name && searchDetail.source === result.source && searchDetail.description && (
  <div style={{ fontSize: 11, color: "var(--text-muted)", marginTop: 2, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
    {searchDetail.description}
  </div>
)}
```

Actually, showing description in the left panel only for the selected result is inconsistent. Better to only show it in the detail panel on the right. Keep left panel as-is.

---

### Task 4: Default popular skills view on mount

When the component mounts and no search query is entered, show popular skills instead of "No skills installed / Search to find skills".

**Files:**
- Modify: `ui/src/pages/projects/ProjectSkillsTab.tsx`

**Step 1: Add popular skills state and loading**

```typescript
const [popularSkills, setPopularSkills] = useState<SkillSearchResult[]>([]);
const [popularLoading, setPopularLoading] = useState(false);
```

**Step 2: Load popular skills on mount**

Add a useEffect that fires once on mount:
```typescript
useEffect(() => {
  setPopularLoading(true);
  skillSearch("skill", 20)
    .then(setPopularSkills)
    .catch(() => setPopularSkills([]))
    .finally(() => setPopularLoading(false));
}, []);
```

**Step 3: Add "browse" mode**

Change the Mode type:
```typescript
type Mode = "installed" | "search" | "browse";
```

Set initial mode to "browse" when no skills are installed, or "installed" when skills exist. Update the reload callback to set initial mode:
```typescript
if (skills.length > 0 && !selectedSkill) {
  setSelectedSkill(skills[0].name);
} else if (skills.length === 0) {
  setMode("browse");
}
```

**Step 4: Render popular skills in left panel**

When mode is "browse", show popular skills list with a header "Popular Skills". The items look identical to search results but clicking one shows its detail.

When the user clears search input:
- If installed skills exist → go to "installed" mode
- If no installed skills → go to "browse" mode

Update `handleSearchChange`:
```typescript
if (!value.trim()) {
  setMode(installed.length > 0 ? "installed" : "browse");
  setSearchResults([]);
  return;
}
```

Update `handleBackToInstalled`:
```typescript
const handleBackToInstalled = () => {
  setMode(installed.length > 0 ? "installed" : "browse");
  setSearchQuery("");
  setSearchResults([]);
};
```

**Step 5: Handle browse mode in detail panel**

The right panel in browse mode works just like search mode — selecting a popular skill loads its detail via `skillSearchDetail`.

---

### Task 5: Replace confirm() with ConfirmDialog

**Files:**
- Modify: `ui/src/pages/projects/ProjectSkillsTab.tsx`

**Step 1: Import ConfirmDialog**

```typescript
import ConfirmDialog from "../../components/ConfirmDialog";
```

**Step 2: Add confirm dialog state**

```typescript
const [confirmRemove, setConfirmRemove] = useState<string | null>(null);
const [removeLoading, setRemoveLoading] = useState(false);
```

**Step 3: Replace handleRemove**

Change from:
```typescript
const handleRemove = async (name: string) => {
  if (!confirm(`Remove skill "${name}"?`)) return;
  setActionLoading(`remove:${name}`);
  try {
    await skillRemove(projectPath, name);
    if (selectedSkill === name) setSelectedSkill(null);
    await reload();
  } catch (e) {
    alert(`Remove failed: ${e}`);
  } finally {
    setActionLoading(null);
  }
};
```
to:
```typescript
const handleRemove = (name: string) => {
  setConfirmRemove(name);
};

const handleConfirmRemove = async () => {
  if (!confirmRemove) return;
  setRemoveLoading(true);
  try {
    await skillRemove(projectPath, confirmRemove);
    if (selectedSkill === confirmRemove) setSelectedSkill(null);
    setConfirmRemove(null);
    await reload();
  } catch (e) {
    alert(`Remove failed: ${e}`);
  } finally {
    setRemoveLoading(false);
  }
};
```

**Step 4: Add ConfirmDialog to JSX**

At the end of the component, before closing `</div>`:
```tsx
<ConfirmDialog
  open={confirmRemove !== null}
  onClose={() => setConfirmRemove(null)}
  onConfirm={handleConfirmRemove}
  title="Remove Skill"
  message={`Are you sure you want to remove the skill "${confirmRemove}"? This will delete the skill files from the project.`}
  confirmLabel="Remove"
  danger
  loading={removeLoading}
/>
```

---

### Task 6: Build and verify

**Step 1: Build Tauri backend**

Run: `cargo build -p devflow-desktop`
Expected: compiles with no errors

**Step 2: Build UI**

Run: `bun run build` (from `ui/`)
Expected: builds with no errors

**Step 3: Commit**

```bash
git add -A && git commit --no-verify -m "fix(gui): fix 4 skills tab bugs — descriptions, selection, popular view, confirm dialog"
```
