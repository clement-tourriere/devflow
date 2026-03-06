import type { FilledConfig, MergeConfig } from "../../../types/config";
import FormField from "../components/FormField";

interface Props {
  config: FilledConfig;
  onChange: (config: FilledConfig) => void;
}

const DEFAULT_MERGE: MergeConfig = {
  strategy: null,
  cleanup_after_merge: null,
  cascade_rebase: null,
  checks: [],
};

function MergeSection({ config, onChange }: Props) {
  const merge = config.merge;

  const enable = () => {
    onChange({ ...config, merge: { ...DEFAULT_MERGE } });
  };

  const disable = () => {
    onChange({ ...config, merge: null });
  };

  const update = (patch: Partial<MergeConfig>) => {
    onChange({
      ...config,
      merge: { ...merge, ...patch } as MergeConfig,
    });
  };

  if (!merge) {
    return (
      <div>
        <h2 style={{ fontSize: 16, fontWeight: 600, marginBottom: 16 }}>Merge</h2>
        <div
          style={{
            padding: 24,
            textAlign: "center",
            border: "1px dashed var(--border)",
            borderRadius: 8,
            color: "var(--text-secondary)",
          }}
        >
          <p style={{ marginBottom: 12 }}>Merge configuration is not set. Defaults will be used.</p>
          <button className="btn btn-primary" onClick={enable}>
            Configure Merge
          </button>
        </div>
      </div>
    );
  }

  const checksCount = merge.checks?.length ?? 0;

  return (
    <div>
      <h2 style={{ fontSize: 16, fontWeight: 600, marginBottom: 16 }}>Merge</h2>

      <FormField label="Strategy" description="How workspaces are merged into the target">
        <select
          value={merge.strategy || "merge"}
          onChange={(e) =>
            update({
              strategy: e.target.value as "merge" | "rebase",
            })
          }
          style={{ width: "100%", fontSize: 13 }}
        >
          <option value="merge">Merge (merge commit)</option>
          <option value="rebase">Rebase (rebase then fast-forward)</option>
        </select>
      </FormField>

      <FormField
        label="Cleanup after merge"
        description="Delete the source workspace, worktree, and service workspaces after a successful merge"
      >
        <label style={{ display: "flex", alignItems: "center", gap: 8, cursor: "pointer" }}>
          <input
            type="checkbox"
            checked={merge.cleanup_after_merge ?? false}
            onChange={(e) => update({ cleanup_after_merge: e.target.checked })}
          />
          Delete workspace after merge
        </label>
      </FormField>

      <FormField
        label="Cascade rebase"
        description="Automatically rebase child workspaces after merging into the target"
      >
        <label style={{ display: "flex", alignItems: "center", gap: 8, cursor: "pointer" }}>
          <input
            type="checkbox"
            checked={merge.cascade_rebase ?? false}
            onChange={(e) => update({ cascade_rebase: e.target.checked })}
          />
          Auto-rebase child workspaces
        </label>
      </FormField>

      <FormField
        label="Readiness checks"
        description="Merge readiness checks are configured in the YAML editor"
      >
        <span style={{ fontSize: 13, color: "var(--text-secondary)" }}>
          {checksCount === 0
            ? "No checks configured"
            : `${checksCount} check${checksCount !== 1 ? "s" : ""} configured`}
        </span>
      </FormField>

      <div style={{ marginTop: 16 }}>
        <button
          className="btn btn-danger"
          onClick={disable}
          style={{ fontSize: 12 }}
        >
          Remove Merge Config
        </button>
      </div>
    </div>
  );
}

export default MergeSection;
