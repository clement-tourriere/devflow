import type { FilledConfig, WorktreeConfig } from "../../../types/config";
import FormField from "../components/FormField";
import TagList from "../components/TagList";

interface Props {
  config: FilledConfig;
  onChange: (config: FilledConfig) => void;
}

function WorktreeSection({ config, onChange }: Props) {
  const wt = config.worktree;

  const updateWt = (patch: Partial<WorktreeConfig>) => {
    onChange({ ...config, worktree: { ...wt, ...patch } });
  };

  return (
    <div>
      <h2 style={{ fontSize: 16, fontWeight: 600, marginBottom: 16 }}>Worktrees</h2>

      <FormField
        label="Path template"
        description="Template for workspace directory paths. Supports {repo} and {workspace} placeholders."
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
        description="Also copy gitignored dependency/cache dirs like .venv, node_modules, and target. Leave off for fastest workspace creation."
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

      <FormField
        label="Copy AI configuration"
        description="Copy supported agent configuration directories into new workspaces"
      >
        <label style={{ display: "flex", alignItems: "center", gap: 8, cursor: "pointer" }}>
          <input
            type="checkbox"
            checked={wt.copy_ai_configs}
            onChange={(e) => updateWt({ copy_ai_configs: e.target.checked })}
          />
          <span style={{ fontSize: 13 }}>Copy .claude, .cursor, .opencode, and .agents</span>
        </label>
      </FormField>

      <FormField
        label="Additional AI directories"
        description="Extra repository-relative agent configuration directories to copy"
      >
        <TagList
          values={wt.extra_ai_dirs}
          onChange={(extra_ai_dirs) => updateWt({ extra_ai_dirs })}
          placeholder="e.g. .my-agent"
        />
      </FormField>

    </div>
  );
}

export default WorktreeSection;
