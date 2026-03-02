import { useState, useEffect } from "react";
import { useParams, Link } from "react-router-dom";
import {
  getConfigYaml,
  saveConfigYaml,
  validateConfigYaml,
} from "../../utils/invoke";

function ConfigEditor() {
  const { "*": splat } = useParams();
  const projectPath = splat ? decodeURIComponent(splat) : "";

  const [content, setContent] = useState("");
  const [original, setOriginal] = useState("");
  const [validationError, setValidationError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    if (!projectPath) return;
    getConfigYaml(projectPath)
      .then((yaml) => {
        setContent(yaml);
        setOriginal(yaml);
      })
      .catch(console.error);
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
      <Link
        to={`/projects/${encodeURIComponent(projectPath)}`}
        style={{
          color: "var(--text-muted)",
          textDecoration: "none",
          fontSize: 13,
          display: "inline-block",
          marginBottom: 8,
        }}
      >
        &larr; Back to project
      </Link>
      <h1 className="page-title">Configuration</h1>
      <p className="mono" style={{ color: "var(--text-secondary)", marginBottom: 16 }}>
        {projectPath}/.devflow.yml
      </p>

      <div className="card">
        <textarea
          value={content}
          onChange={(e) => {
            setContent(e.target.value);
            setSaved(false);
          }}
          style={{ minHeight: 400 }}
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
          <button
            className="btn"
            onClick={handleReset}
            disabled={!hasChanges}
          >
            Reset
          </button>
        </div>
      </div>
    </div>
  );
}

export default ConfigEditor;
