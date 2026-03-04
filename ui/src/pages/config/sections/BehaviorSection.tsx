import type { FilledConfig } from "../../../types/config";
import FormField from "../components/FormField";

interface Props {
  config: FilledConfig;
  onChange: (config: FilledConfig) => void;
}

function BehaviorSection({ config, onChange }: Props) {
  const behavior = config.behavior;

  const updateBehavior = (patch: Partial<typeof behavior>) => {
    onChange({ ...config, behavior: { ...behavior, ...patch } });
  };

  return (
    <div>
      <h2 style={{ fontSize: 16, fontWeight: 600, marginBottom: 16 }}>Behavior</h2>

      <FormField label="Max workspaces" description="Maximum number of service workspaces to keep">
        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          <label style={{ display: "flex", alignItems: "center", gap: 8, cursor: "pointer" }}>
            <input
              type="checkbox"
              checked={behavior.max_workspaces != null}
              onChange={(e) =>
                updateBehavior({ max_workspaces: e.target.checked ? 10 : null })
              }
            />
            <span style={{ fontSize: 13 }}>Limit</span>
          </label>
          {behavior.max_workspaces != null && (
            <input
              type="number"
              min={1}
              value={behavior.max_workspaces}
              onChange={(e) =>
                updateBehavior({
                  max_workspaces: parseInt(e.target.value) || 1,
                })
              }
              style={{ width: 80, fontSize: 13 }}
            />
          )}
        </div>
      </FormField>
    </div>
  );
}

export default BehaviorSection;
