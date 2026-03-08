# Per-Project Skills Fixes Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Fix three GUI issues: (1) Vite dev-server reloads when skills are installed/removed, (2) Skills page should be per-project (tab inside ProjectDetail), not global, (3) DoctorPage skill buttons become a link to the Skills tab.

**Architecture:** Move SkillsPage from standalone route into a `ProjectSkillsTab` component embedded in ProjectDetail via a horizontal tab bar. Use URL hash (`#skills`) for deep-linking. Replace DoctorPage skill action buttons with a "Manage Skills" link. Fix Vite watcher to ignore devflow metadata directories.

**Tech Stack:** React 18, TypeScript, Vite, react-router-dom v6, Tauri v2, react-markdown

---

### Task 1: Fix Vite watcher ignore list

**Files:**
- Modify: `ui/vite.config.ts`

**Step 1: Add ignored directories**

In `ui/vite.config.ts`, add `"**/.agents/**"`, `"**/.claude/**"`, `"**/.devflow/**"` to `server.watch.ignored`:

```typescript
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    open: false,
    watch: {
      ignored: ["**/src-tauri/**", "**/.agents/**", "**/.claude/**", "**/.devflow/**"],
    },
  },
});
```

**Step 2: Commit**

```bash
git add ui/vite.config.ts
git commit --no-verify -m "fix(gui): ignore .agents/.claude/.devflow in Vite watcher

Prevents Vite dev-server from triggering full-page reloads when
skills are installed, removed, or updated — these operations write
to .agents/skills/ and .claude/skills/ in the project root."
```

---

### Task 2: Create ProjectSkillsTab component

**Files:**
- Create: `ui/src/pages/projects/ProjectSkillsTab.tsx`

**Step 1: Create the component**

Adapt `SkillsPage.tsx` into `ProjectSkillsTab.tsx` that:
- Accepts `projectPath: string` as a prop (no more `window.location.search` parsing)
- Removes the outer `<h1>Skills</h1>` page title (tab context provides this)
- Keeps all state, effects, handlers, and JSX otherwise identical
- Exports as default

```typescript
interface ProjectSkillsTabProps {
  projectPath: string;
}

export default function ProjectSkillsTab({ projectPath }: ProjectSkillsTabProps) {
  // ... same state and logic as SkillsPage, minus the projectPath derivation
}
```

**Step 2: Commit**

```bash
git add ui/src/pages/projects/ProjectSkillsTab.tsx
git commit --no-verify -m "feat(gui): create ProjectSkillsTab component

Adapts SkillsPage into a reusable component that receives projectPath
as a prop, for embedding inside ProjectDetail's tab system."
```

---

### Task 3: Add horizontal tab bar to ProjectDetail

**Files:**
- Modify: `ui/src/pages/projects/ProjectDetail.tsx`

**Step 1: Add tab state and imports**

- Import `ProjectSkillsTab` from `./ProjectSkillsTab`
- Add `activeTab` state: `useState<"overview" | "skills">` initialized from `window.location.hash === "#skills" ? "skills" : "overview"`
- Add `useEffect` that listens to `hashchange` event and updates `activeTab`
- Add `useEffect` that sets `window.location.hash` when `activeTab` changes

**Step 2: Add tab bar after header**

After the header `<div className="flex items-center justify-between mb-4">` block, insert a horizontal tab bar:

```tsx
{/* Tab bar */}
<div style={{
  display: "flex",
  borderBottom: "1px solid var(--border)",
  marginBottom: 16,
  gap: 0,
}}>
  <button
    onClick={() => setActiveTab("overview")}
    style={{
      padding: "8px 16px",
      fontSize: 14,
      fontWeight: activeTab === "overview" ? 600 : 400,
      color: activeTab === "overview" ? "var(--text-primary)" : "var(--text-muted)",
      background: "none",
      border: "none",
      borderBottom: activeTab === "overview" ? "2px solid var(--accent)" : "2px solid transparent",
      cursor: "pointer",
      marginBottom: -1,
    }}
  >
    Overview
  </button>
  <button
    onClick={() => setActiveTab("skills")}
    style={{
      padding: "8px 16px",
      fontSize: 14,
      fontWeight: activeTab === "skills" ? 600 : 400,
      color: activeTab === "skills" ? "var(--text-primary)" : "var(--text-muted)",
      background: "none",
      border: "none",
      borderBottom: activeTab === "skills" ? "2px solid var(--accent)" : "2px solid transparent",
      cursor: "pointer",
      marginBottom: -1,
    }}
  >
    Skills
  </button>
</div>
```

**Step 3: Wrap existing content in Overview tab, add Skills tab**

Wrap all existing cards (skills banner, Workspaces, Services, Proxy Domains, Danger Zone) in `{activeTab === "overview" && (<>...</>)}`.

Add after: `{activeTab === "skills" && <ProjectSkillsTab projectPath={projectPath} />}`

**Step 4: Remove skills staleness banner and related state**

Remove from ProjectDetail:
- State: `skillStatus`, `skillsUpdating`, `skillsDismissed`
- Handler: `handleUpdateSkills`
- `useEffect` line that calls `checkAgentSkills` (keep the `runDoctor` line)
- The JSX block for "Agent skills update notification" banner
- Imports: `checkAgentSkills`, `installAgentSkills` from invoke
- Type import: `AgentSkillsStatus`

**Step 5: Commit**

```bash
git add ui/src/pages/projects/ProjectDetail.tsx
git commit --no-verify -m "feat(gui): add tab bar to ProjectDetail with Skills tab

Adds Overview/Skills horizontal tab bar. Skills tab embeds
ProjectSkillsTab for full per-project skill management. Removes
the old skills staleness banner in favor of the dedicated tab.
URL hash #skills enables deep-linking."
```

---

### Task 4: Remove standalone /skills route and sidebar link

**Files:**
- Modify: `ui/src/App.tsx`
- Modify: `ui/src/components/Layout.tsx`
- Delete: `ui/src/pages/skills/SkillsPage.tsx`

**Step 1: Remove route from App.tsx**

Remove the import of `SkillsPage` and the `<Route path="skills" element={<SkillsPage />} />` line.

**Step 2: Remove sidebar link from Layout.tsx**

Remove the entire "Tools" section:
```tsx
<div className="nav-section">Tools</div>
<NavLink to="/skills" ...>Skills</NavLink>
```

**Step 3: Delete SkillsPage.tsx**

Delete `ui/src/pages/skills/SkillsPage.tsx`.

**Step 4: Commit**

```bash
git rm ui/src/pages/skills/SkillsPage.tsx
git add ui/src/App.tsx ui/src/components/Layout.tsx
git commit --no-verify -m "refactor(gui): remove standalone /skills route and sidebar link

Skills are now managed per-project via the Skills tab in ProjectDetail.
The global /skills route and sidebar nav entry are no longer needed."
```

---

### Task 5: DoctorPage skills row becomes link

**Files:**
- Modify: `ui/src/pages/doctor/DoctorPage.tsx`

**Step 1: Replace skill action buttons with "Manage Skills" link**

In the `renderServiceReport` function, replace the `isSkillsCheck` branch (lines ~205-243) with a single link:

```tsx
) : isSkillsCheck ? (
  <Link
    to={`/projects/${encodeURIComponent(projectPath)}#skills`}
    style={{ fontSize: 12, color: "var(--accent)", textDecoration: "none" }}
  >
    Manage Skills
  </Link>
```

**Step 2: Remove unused skills state and handlers**

Remove:
- State: `skillsLoading`, `skillStatus` (the useState lines)
- Handlers: `handleInstallSkills`, `handleUninstallSkills`, `handleUpdateSkills`
- The `checkAgentSkills` call from `reloadDoctor` (keep `runDoctor`)
- Imports: `installAgentSkills`, `uninstallAgentSkills`, `checkAgentSkills` from invoke
- Type import: `AgentSkillsStatus`

**Step 3: Commit**

```bash
git add ui/src/pages/doctor/DoctorPage.tsx
git commit --no-verify -m "refactor(gui): replace DoctorPage skill buttons with link to Skills tab

The Setup page now shows a 'Manage Skills' link pointing to the
per-project Skills tab instead of inline install/update/uninstall buttons."
```

---

### Task 6: Verify build

**Step 1: Run UI build**

```bash
cd ui && bun run build
```

Expected: Build succeeds with no TypeScript errors.

**Step 2: Check for dead imports**

Verify no unused imports remain in modified files. Fix if needed.

**Step 3: Final commit (if fixes needed)**

Only if build revealed issues.
