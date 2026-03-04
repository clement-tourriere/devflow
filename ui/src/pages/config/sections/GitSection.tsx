import type { FilledConfig } from "../../../types/config";
import FormField from "../components/FormField";
import TagList from "../components/TagList";

interface Props {
  config: FilledConfig;
  onChange: (config: FilledConfig) => void;
}

function GitSection({ config, onChange }: Props) {
  const git = config.git;

  const updateGit = (patch: Partial<typeof git>) => {
    onChange({ ...config, git: { ...git, ...patch } });
  };

  return (
    <div>
      <h2 style={{ fontSize: 16, fontWeight: 600, marginBottom: 16 }}>Git Integration</h2>

      <FormField
        label="Auto-create on workspace"
        description="Automatically create service workspaces when switching git branches"
      >
        <label style={{ display: "flex", alignItems: "center", gap: 8, cursor: "pointer" }}>
          <input
            type="checkbox"
            checked={git.auto_create_on_workspace}
            onChange={(e) => updateGit({ auto_create_on_workspace: e.target.checked })}
          />
          <span style={{ fontSize: 13 }}>Enabled</span>
        </label>
      </FormField>

      <FormField
        label="Auto-switch on workspace"
        description="Automatically switch services when switching git branches"
      >
        <label style={{ display: "flex", alignItems: "center", gap: 8, cursor: "pointer" }}>
          <input
            type="checkbox"
            checked={git.auto_switch_on_workspace}
            onChange={(e) => updateGit({ auto_switch_on_workspace: e.target.checked })}
          />
          <span style={{ fontSize: 13 }}>Enabled</span>
        </label>
      </FormField>

      <FormField label="Main workspace" description="The primary workspace name (usually 'main' or 'master')">
        <input
          type="text"
          value={git.main_workspace}
          onChange={(e) => updateGit({ main_workspace: e.target.value })}
          style={{ width: "100%", fontSize: 13 }}
        />
      </FormField>

      <FormField
        label="Workspace filter regex"
        description="Only create workspaces for branches matching this regex (optional)"
      >
        <input
          type="text"
          value={git.workspace_filter_regex || ""}
          onChange={(e) => updateGit({ workspace_filter_regex: e.target.value || null })}
          placeholder="e.g. ^feature/.*"
          style={{ width: "100%", fontSize: 13 }}
        />
      </FormField>

      <FormField
        label="Exclude workspaces"
        description="Never create service workspaces for these branch names"
      >
        <TagList
          values={git.exclude_workspaces}
          onChange={(exclude_workspaces) => updateGit({ exclude_workspaces })}
          placeholder="Add branch name..."
        />
      </FormField>
    </div>
  );
}

export default GitSection;
