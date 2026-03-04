import type { FilledConfig, NamingStrategy } from "../../../types/config";
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

      <FormField
        label="Auto cleanup"
        description="Automatically remove service workspaces when git branches are deleted"
      >
        <label style={{ display: "flex", alignItems: "center", gap: 8, cursor: "pointer" }}>
          <input
            type="checkbox"
            checked={behavior.auto_cleanup}
            onChange={(e) => updateBehavior({ auto_cleanup: e.target.checked })}
          />
          <span style={{ fontSize: 13 }}>Enabled</span>
        </label>
      </FormField>

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

      <FormField
        label="Naming strategy"
        description="How service workspace names are derived from branch names"
      >
        <select
          value={behavior.naming_strategy}
          onChange={(e) =>
            updateBehavior({ naming_strategy: e.target.value as NamingStrategy })
          }
          style={{ width: "100%", fontSize: 13 }}
        >
          <option value="prefix">Prefix (default)</option>
          <option value="suffix">Suffix</option>
          <option value="replace">Replace</option>
        </select>
        <div style={{ fontSize: 11, color: "var(--text-muted)", marginTop: 4 }}>
          {behavior.naming_strategy === "prefix" && "Example: devflow_feature-login"}
          {behavior.naming_strategy === "suffix" && "Example: feature-login_devflow"}
          {behavior.naming_strategy === "replace" && "Example: feature-login"}
        </div>
      </FormField>
    </div>
  );
}

export default BehaviorSection;
