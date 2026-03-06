import { useState, useEffect } from "react";
import {
  getConfigYaml,
  saveConfigYaml,
  validateConfigYaml,
} from "../../utils/invoke";

interface Props {
  projectPath: string;
  /** Called after a successful save so the parent can reload structured config */
  onSaved?: () => void;
}

function YamlEditor({ projectPath, onSaved }: Props) {
  const [content, setContent] = useState("");
  const [original, setOriginal] = useState("");
  const [validationError, setValidationError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  const load = () => {
    getConfigYaml(projectPath)
      .then((yaml) => {
        setContent(yaml);
        setOriginal(yaml);
        setValidationError(null);
      })
      .catch(console.error);
  };

  useEffect(() => {
    if (!projectPath) return;
    load();
  }, [projectPath]);

  const handleValidate = async () => {
    try {
      const result = await validateConfigYaml(content);
      if (result.valid) {
        setValidationError(null);
        alert("Configuration is valid.");
      } else {
        setValidationError(result.error || "Unknown validation error");
      }
    } catch (e) {
      setValidationError(String(e));
    }
  };

  const handleSave = async () => {
    try {
      const result = await validateConfigYaml(content);
      if (!result.valid) {
        setValidationError(result.error || "Invalid configuration");
        return;
      }
      setValidationError(null);
      await saveConfigYaml(projectPath, content);
      setOriginal(content);
      setSaved(true);
      setTimeout(() => setSaved(false), 2000);
      onSaved?.();
    } catch (e) {
      alert(`Save failed: ${e}`);
    }
  };

  const handleReset = () => {
    setContent(original);
    setValidationError(null);
  };

  const hasChanges = content !== original;

  return (
    <div>
      <h2 style={{ fontSize: 16, fontWeight: 600, marginBottom: 16 }}>YAML Editor</h2>
      <p style={{ fontSize: 12, color: "var(--text-muted)", marginBottom: 12 }}>
        Edit the raw .devflow.yml file directly. Changes made here will not preserve formatting from the GUI editor.
      </p>

      <textarea
        value={content}
        onChange={(e) => {
          setContent(e.target.value);
          setSaved(false);
        }}
        style={{ minHeight: 400, fontFamily: "monospace", fontSize: 13 }}
      />

      {validationError && (
        <div
          style={{
            marginTop: 8,
            padding: 12,
            background: "rgba(248, 81, 73, 0.1)",
            border: "1px solid var(--danger)",
            borderRadius: 6,
            color: "var(--danger)",
            fontSize: 13,
          }}
        >
          {validationError}
        </div>
      )}

      {saved && (
        <div
          style={{
            marginTop: 8,
            padding: 12,
            background: "rgba(63, 185, 80, 0.1)",
            border: "1px solid var(--success)",
            borderRadius: 6,
            color: "var(--success)",
            fontSize: 13,
          }}
        >
          Configuration saved.
        </div>
      )}

      <div className="flex gap-2" style={{ marginTop: 12 }}>
        <button className="btn" onClick={handleValidate}>
          Validate
        </button>
        <button
          className="btn btn-primary"
          onClick={handleSave}
          disabled={!hasChanges}
        >
          Save
        </button>
        <button className="btn" onClick={handleReset} disabled={!hasChanges}>
          Reset
        </button>
      </div>
    </div>
  );
}

export default YamlEditor;
