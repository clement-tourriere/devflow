import { useState } from "react";
import type { FilledConfig, CommitConfig, CommitGenerationConfig } from "../../../types/config";
import FormField from "../components/FormField";

interface Props {
  config: FilledConfig;
  onChange: (config: FilledConfig) => void;
}

const DEFAULT_COMMIT: CommitConfig = {
  generation: {
    command: null,
    api_key: null,
    api_url: null,
    model: null,
  },
};

function CommitSection({ config, onChange }: Props) {
  const commit = config.commit;
  const gen = commit?.generation;
  const [showApiKey, setShowApiKey] = useState(false);

  const enable = () => {
    onChange({ ...config, commit: { ...DEFAULT_COMMIT } });
  };

  const disable = () => {
    onChange({ ...config, commit: null });
  };

  const updateGen = (patch: Partial<CommitGenerationConfig>) => {
    onChange({
      ...config,
      commit: {
        ...commit,
        generation: { ...gen, ...patch } as CommitGenerationConfig,
      },
    });
  };

  if (!commit) {
    return (
      <div>
        <h2 style={{ fontSize: 16, fontWeight: 600, marginBottom: 16 }}>Commit Generation</h2>
        <div
          style={{
            padding: 24,
            textAlign: "center",
            border: "1px dashed var(--border)",
            borderRadius: 8,
            color: "var(--text-secondary)",
          }}
        >
          <p style={{ marginBottom: 12 }}>AI commit message generation is not configured.</p>
          <button className="btn btn-primary" onClick={enable}>
            Enable Commit Generation
          </button>
        </div>
      </div>
    );
  }

  return (
    <div>
      <h2 style={{ fontSize: 16, fontWeight: 600, marginBottom: 16 }}>Commit Generation</h2>

      <FormField
        label="Command"
        description="External CLI command for generating commit messages (takes precedence over API)"
      >
        <input
          type="text"
          value={gen?.command || ""}
          onChange={(e) => updateGen({ command: e.target.value || null })}
          placeholder='e.g. claude -p --model haiku'
          style={{ width: "100%", fontSize: 13 }}
        />
      </FormField>

      <div
        style={{
          marginTop: 8,
          marginBottom: 16,
          padding: 8,
          background: "var(--bg-tertiary)",
          borderRadius: 6,
          fontSize: 11,
          color: "var(--text-muted)",
        }}
      >
        If a command is set, the API fields below are used as fallback only.
      </div>

      <FormField label="API URL" description="OpenAI-compatible API endpoint">
        <input
          type="text"
          value={gen?.api_url || ""}
          onChange={(e) => updateGen({ api_url: e.target.value || null })}
          placeholder="http://localhost:11434/v1"
          style={{ width: "100%", fontSize: 13 }}
        />
      </FormField>

      <FormField label="API Key" description="API key for the LLM endpoint">
        <div style={{ display: "flex", gap: 6 }}>
          <input
            type={showApiKey ? "text" : "password"}
            value={gen?.api_key || ""}
            onChange={(e) => updateGen({ api_key: e.target.value || null })}
            placeholder="Not set"
            style={{ flex: 1, fontSize: 13 }}
          />
          <button
            className="btn"
            onClick={() => setShowApiKey(!showApiKey)}
            style={{ fontSize: 11, padding: "4px 8px" }}
          >
            {showApiKey ? "Hide" : "Show"}
          </button>
        </div>
      </FormField>

      <FormField label="Model" description="LLM model name">
        <input
          type="text"
          value={gen?.model || ""}
          onChange={(e) => updateGen({ model: e.target.value || null })}
          placeholder="e.g. llama3"
          style={{ width: "100%", fontSize: 13 }}
        />
      </FormField>

      <div style={{ marginTop: 16 }}>
        <button
          className="btn btn-danger"
          onClick={disable}
          style={{ fontSize: 12 }}
        >
          Remove Commit Config
        </button>
      </div>
    </div>
  );
}

export default CommitSection;
