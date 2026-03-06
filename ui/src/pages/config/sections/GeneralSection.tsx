import type { FilledConfig } from "../../../types/config";
import FormField from "../components/FormField";

interface Props {
  config: FilledConfig;
  onChange: (config: FilledConfig) => void;
}

function GeneralSection({ config, onChange }: Props) {
  return (
    <div>
      <h2 style={{ fontSize: 16, fontWeight: 600, marginBottom: 16 }}>General</h2>

      <FormField label="Project Name" description="Name used to identify this project in devflow">
        <input
          type="text"
          value={config.name || ""}
          onChange={(e) =>
            onChange({ ...config, name: e.target.value || null })
          }
          placeholder="Derived from directory name if empty"
          style={{ width: "100%", fontSize: 13 }}
        />
      </FormField>

      <FormField label="Default VCS" description="Version control system preference for this project">
        <select
          value={config.default_vcs || ""}
          onChange={(e) =>
            onChange({
              ...config,
              default_vcs: (e.target.value || null) as FilledConfig["default_vcs"],
            })
          }
          style={{ width: "100%", fontSize: 13 }}
        >
          <option value="">Auto-detect</option>
          <option value="git">Git</option>
          <option value="jj">Jujutsu (jj)</option>
        </select>
      </FormField>
    </div>
  );
}

export default GeneralSection;
