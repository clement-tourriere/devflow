import { useState, useEffect, useCallback } from "react";
import { useParams, Link } from "react-router-dom";
import { getConfigJson, saveConfigJson } from "../../utils/invoke";
import { withDefaults } from "../../types/config";
import type { FilledConfig } from "../../types/config";
import GeneralSection from "./sections/GeneralSection";
import GitSection from "./sections/GitSection";
import BehaviorSection from "./sections/BehaviorSection";
import WorktreeSection from "./sections/WorktreeSection";
import ServicesSection from "./sections/ServicesSection";
import AgentSection from "./sections/AgentSection";
import CommitSection from "./sections/CommitSection";
import MergeSection from "./sections/MergeSection";
import YamlEditor from "./YamlEditor";

type Section =
  | "general"
  | "git"
  | "behavior"
  | "worktree"
  | "services"
  | "agent"
  | "commit"
  | "merge"
  | "yaml";

const SECTIONS: { key: Section; label: string }[] = [
  { key: "general", label: "General" },
  { key: "git", label: "Git" },
  { key: "behavior", label: "Behavior" },
  { key: "worktree", label: "Worktrees" },
  { key: "services", label: "Services" },
  { key: "agent", label: "Agent" },
  { key: "commit", label: "Commit" },
  { key: "merge", label: "Merge" },
  { key: "yaml", label: "YAML" },
];

function ConfigEditor() {
  const { "*": splat } = useParams();
  const projectPath = splat ? decodeURIComponent(splat) : "";

  const [config, setConfig] = useState<FilledConfig | null>(null);
  const [originalJson, setOriginalJson] = useState("");
  const [activeSection, setActiveSection] = useState<Section>("general");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  const loadConfig = useCallback(() => {
    if (!projectPath) return;
    getConfigJson(projectPath)
      .then((raw) => {
        const cfg = withDefaults(raw);
        setConfig(cfg);
        setOriginalJson(JSON.stringify(cfg));
        setError(null);
      })
      .catch((e) => setError(String(e)));
  }, [projectPath]);

  useEffect(() => {
    loadConfig();
  }, [loadConfig]);

  const hasChanges = config ? JSON.stringify(config) !== originalJson : false;

  const handleSave = async () => {
    if (!config || !projectPath) return;
    setSaving(true);
    setError(null);
    try {
      await saveConfigJson(projectPath, config);
      setOriginalJson(JSON.stringify(config));
      setSaved(true);
      setTimeout(() => setSaved(false), 2000);
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  const handleReset = () => {
    if (!originalJson) return;
    setConfig(JSON.parse(originalJson));
    setError(null);
  };

  const handleChange = (updated: FilledConfig) => {
    setConfig(updated);
    setSaved(false);
  };

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

      <div
        style={{
          display: "grid",
          gridTemplateColumns: "200px 1fr",
          gap: 16,
          minHeight: 500,
        }}
      >
        {/* Sidebar */}
        <div
          style={{
            borderRight: "1px solid var(--border)",
            paddingRight: 16,
          }}
        >
          {SECTIONS.map(({ key, label }) => (
            <button
              key={key}
              onClick={() => setActiveSection(key)}
              style={{
                display: "block",
                width: "100%",
                textAlign: "left",
                padding: "8px 12px",
                marginBottom: 2,
                borderRadius: 6,
                border: "none",
                background:
                  activeSection === key
                    ? "var(--bg-tertiary)"
                    : "transparent",
                color:
                  activeSection === key
                    ? "var(--text-primary)"
                    : "var(--text-secondary)",
                fontWeight: activeSection === key ? 600 : 400,
                fontSize: 13,
                cursor: "pointer",
              }}
            >
              {label}
              {key === "yaml" && (
                <span
                  style={{
                    fontSize: 10,
                    color: "var(--text-muted)",
                    marginLeft: 6,
                  }}
                >
                  Advanced
                </span>
              )}
            </button>
          ))}
        </div>

        {/* Content */}
        <div style={{ minWidth: 0 }}>
          {error && (
            <div
              style={{
                marginBottom: 12,
                padding: 12,
                background: "rgba(248, 81, 73, 0.1)",
                border: "1px solid var(--danger)",
                borderRadius: 6,
                color: "var(--danger)",
                fontSize: 13,
              }}
            >
              {error}
            </div>
          )}

          {saved && (
            <div
              style={{
                marginBottom: 12,
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

          {activeSection === "yaml" ? (
            <YamlEditor projectPath={projectPath} onSaved={loadConfig} />
          ) : config ? (
            <>
              {activeSection === "general" && (
                <GeneralSection config={config} onChange={handleChange} />
              )}
              {activeSection === "git" && (
                <GitSection config={config} onChange={handleChange} />
              )}
              {activeSection === "behavior" && (
                <BehaviorSection config={config} onChange={handleChange} />
              )}
              {activeSection === "worktree" && (
                <WorktreeSection config={config} onChange={handleChange} />
              )}
              {activeSection === "services" && (
                <ServicesSection
                  config={config}
                  onChange={handleChange}
                  projectPath={projectPath}
                />
              )}
              {activeSection === "agent" && (
                <AgentSection config={config} onChange={handleChange} />
              )}
              {activeSection === "commit" && (
                <CommitSection config={config} onChange={handleChange} />
              )}
              {activeSection === "merge" && (
                <MergeSection config={config} onChange={handleChange} />
              )}

              {/* Save/Reset bar for GUI sections */}
              <div
                style={{
                  marginTop: 24,
                  paddingTop: 16,
                  borderTop: "1px solid var(--border)",
                  display: "flex",
                  gap: 8,
                  alignItems: "center",
                }}
              >
                <button
                  className="btn btn-primary"
                  onClick={handleSave}
                  disabled={!hasChanges || saving}
                >
                  {saving ? "Saving..." : "Save"}
                </button>
                <button
                  className="btn"
                  onClick={handleReset}
                  disabled={!hasChanges}
                >
                  Reset
                </button>
                {hasChanges && (
                  <span style={{ fontSize: 12, color: "var(--warning)" }}>
                    Unsaved changes
                  </span>
                )}
              </div>
            </>
          ) : (
            <div style={{ color: "var(--text-muted)", padding: 24 }}>
              Loading configuration...
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

export default ConfigEditor;
