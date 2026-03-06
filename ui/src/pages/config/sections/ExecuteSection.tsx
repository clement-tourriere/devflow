import type { FilledConfig, ExecuteConfig } from "../../../types/config";
import FormField from "../components/FormField";

interface Props {
  config: FilledConfig;
  onChange: (config: FilledConfig) => void;
}

const DEFAULT_EXECUTE: ExecuteConfig = {
  detach_command: null,
  multiplexer: null,
};

function ExecuteSection({ config, onChange }: Props) {
  const execute = config.execute;

  const enable = () => {
    onChange({ ...config, execute: { ...DEFAULT_EXECUTE } });
  };

  const disable = () => {
    onChange({ ...config, execute: null });
  };

  const update = (patch: Partial<ExecuteConfig>) => {
    if (!execute) return;
    onChange({ ...config, execute: { ...execute, ...patch } });
  };

  if (!execute) {
    return (
      <div>
        <h2 style={{ fontSize: 16, fontWeight: 600, marginBottom: 16 }}>Execute</h2>
        <div
          style={{
            padding: 24,
            textAlign: "center",
            border: "1px dashed var(--border)",
            borderRadius: 8,
            color: "var(--text-secondary)",
          }}
        >
          <p style={{ marginBottom: 12 }}>
            Multiplexer integration is not configured. Auto-detection (tmux, then zellij) will be used.
          </p>
          <button className="btn btn-primary" onClick={enable}>
            Configure Multiplexer
          </button>
        </div>
      </div>
    );
  }

  return (
    <div>
      <h2 style={{ fontSize: 16, fontWeight: 600, marginBottom: 16 }}>Execute</h2>

      <FormField
        label="Preferred multiplexer"
        description="Auto-detected if not set (tmux first, then zellij)"
      >
        <select
          value={execute.multiplexer || ""}
          onChange={(e) => update({ multiplexer: e.target.value || null })}
          style={{ width: "100%", fontSize: 13, padding: "6px 8px" }}
        >
          <option value="">Auto-detect</option>
          <option value="tmux">tmux</option>
          <option value="zellij">zellij</option>
        </select>
      </FormField>

      <FormField
        label="Custom detach command"
        description="Template with {session}, {dir}, {cmd} placeholders. Overrides multiplexer selection."
      >
        <input
          type="text"
          value={execute.detach_command || ""}
          onChange={(e) => update({ detach_command: e.target.value || null })}
          placeholder='e.g. tmux new-session -d -s {session} -c {dir} {cmd}'
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
        Used by <code>devflow switch --open</code> and <code>--detach</code>. If a custom command is set, it takes precedence over the multiplexer preference.
      </div>

      <div style={{ marginTop: 16 }}>
        <button
          className="btn btn-danger"
          onClick={disable}
          style={{ fontSize: 12 }}
        >
          Remove Execute Config
        </button>
      </div>
    </div>
  );
}

export default ExecuteSection;
