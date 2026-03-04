import type { FilledConfig, WorktreeConfig } from "../../../types/config";
import FormField from "../components/FormField";
import TagList from "../components/TagList";

interface Props {
  config: FilledConfig;
  onChange: (config: FilledConfig) => void;
}

const DEFAULT_WORKTREE: WorktreeConfig = {
  enabled: true,
  path_template: "../{repo}.{workspace}",
  copy_files: [".env", ".env.local"],
  copy_ignored: true,
  respect_gitignore: true,
};

function WorktreeSection({ config, onChange }: Props) {
  const wt = config.worktree;

  const enableWorktree = () => {
    onChange({ ...config, worktree: { ...DEFAULT_WORKTREE } });
  };

  const disableWorktree = () => {
    onChange({ ...config, worktree: null });
  };

  const updateWt = (patch: Partial<WorktreeConfig>) => {
    if (!wt) return;
    onChange({ ...config, worktree: { ...wt, ...patch } });
  };

  if (!wt) {
    return (
      <div>
        <h2 style={{ fontSize: 16, fontWeight: 600, marginBottom: 16 }}>Worktrees</h2>
        <div
          style={{
            padding: 24,
            textAlign: "center",
            border: "1px dashed var(--border)",
            borderRadius: 8,
            color: "var(--text-secondary)",
          }}
        >
          <p style={{ marginBottom: 12 }}>Worktree management is not configured.</p>
          <button className="btn btn-primary" onClick={enableWorktree}>
            Enable Worktrees
          </button>
        </div>
      </div>
    );
  }

  return (
    <div>
      <h2 style={{ fontSize: 16, fontWeight: 600, marginBottom: 16 }}>Worktrees</h2>

      <FormField label="Enabled" description="Create git worktrees instead of switching branches">
        <label style={{ display: "flex", alignItems: "center", gap: 8, cursor: "pointer" }}>
          <input
            type="checkbox"
            checked={wt.enabled}
            onChange={(e) => updateWt({ enabled: e.target.checked })}
          />
          <span style={{ fontSize: 13 }}>Enabled</span>
        </label>
      </FormField>

      <FormField
        label="Path template"
        description="Template for worktree directory paths. Supports {repo} and {workspace} placeholders."
      >
        <input
          type="text"
          value={wt.path_template}
          onChange={(e) => updateWt({ path_template: e.target.value })}
          style={{ width: "100%", fontSize: 13 }}
        />
      </FormField>

      <FormField
        label="Copy files"
        description="Files to copy from the main worktree into each new worktree"
      >
        <TagList
          values={wt.copy_files}
          onChange={(copy_files) => updateWt({ copy_files })}
          placeholder="e.g. .env.local"
        />
      </FormField>

      <FormField
        label="Copy ignored files"
        description="Also copy git-ignored files (e.g. .env.local)"
      >
        <label style={{ display: "flex", alignItems: "center", gap: 8, cursor: "pointer" }}>
          <input
            type="checkbox"
            checked={wt.copy_ignored}
            onChange={(e) => updateWt({ copy_ignored: e.target.checked })}
          />
          <span style={{ fontSize: 13 }}>Enabled</span>
        </label>
      </FormField>

      <FormField
        label="Respect .gitignore"
        description="Exclude gitignored files from worktrees (saves disk space)"
      >
        <label style={{ display: "flex", alignItems: "center", gap: 8, cursor: "pointer" }}>
          <input
            type="checkbox"
            checked={wt.respect_gitignore}
            onChange={(e) => updateWt({ respect_gitignore: e.target.checked })}
          />
          <span style={{ fontSize: 13 }}>Enabled</span>
        </label>
      </FormField>

      <div style={{ marginTop: 16 }}>
        <button
          className="btn btn-danger"
          onClick={disableWorktree}
          style={{ fontSize: 12 }}
        >
          Remove Worktree Config
        </button>
      </div>
    </div>
  );
}

export default WorktreeSection;
