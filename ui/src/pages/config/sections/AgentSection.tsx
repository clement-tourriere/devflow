import type { FilledConfig, AgentConfig } from "../../../types/config";
import FormField from "../components/FormField";

interface Props {
  config: FilledConfig;
  onChange: (config: FilledConfig) => void;
}

const DEFAULT_AGENT: AgentConfig = {
  auto_context: true,
};

function AgentSection({ config, onChange }: Props) {
  const agent = config.agent;

  const enable = () => {
    onChange({ ...config, agent: { ...DEFAULT_AGENT } });
  };

  const disable = () => {
    onChange({ ...config, agent: null });
  };

  const update = (patch: Partial<AgentConfig>) => {
    if (!agent) return;
    onChange({ ...config, agent: { ...agent, ...patch } });
  };

  if (!agent) {
    return (
      <div>
        <h2 style={{ fontSize: 16, fontWeight: 600, marginBottom: 16 }}>Agent</h2>
        <div
          style={{
            padding: 24,
            textAlign: "center",
            border: "1px dashed var(--border)",
            borderRadius: 8,
            color: "var(--text-secondary)",
          }}
        >
          <p style={{ marginBottom: 12 }}>AI agent integration is not configured.</p>
          <button className="btn btn-primary" onClick={enable}>
            Enable Agent Integration
          </button>
        </div>
      </div>
    );
  }

  return (
    <div>
      <h2 style={{ fontSize: 16, fontWeight: 600, marginBottom: 16 }}>Agent</h2>

      <FormField
        label="Auto-context"
        description="Automatically provide project context to the agent on launch"
      >
        <label style={{ display: "flex", alignItems: "center", gap: 8, cursor: "pointer" }}>
          <input
            type="checkbox"
            checked={agent.auto_context}
            onChange={(e) => update({ auto_context: e.target.checked })}
          />
          <span style={{ fontSize: 13 }}>Enabled</span>
        </label>
      </FormField>

      <div style={{ marginTop: 16 }}>
        <button
          className="btn btn-danger"
          onClick={disable}
          style={{ fontSize: 12 }}
        >
          Remove Agent Config
        </button>
      </div>
    </div>
  );
}

export default AgentSection;
