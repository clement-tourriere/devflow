import { useState, useEffect } from "react";
import { useParams, Link } from "react-router-dom";
import {
  listHooks,
  renderTemplate,
  getHookVariables,
} from "../../utils/invoke";
import type { HookPhaseEntry } from "../../types";

function HookManager() {
  const { "*": path } = useParams();
  const projectPath = path ? decodeURIComponent(path) : "";

  const [phases, setPhases] = useState<HookPhaseEntry[]>([]);
  const [variables, setVariables] = useState<Record<string, unknown>>({});
  const [template, setTemplate] = useState("");
  const [rendered, setRendered] = useState<string | null>(null);
  const [renderError, setRenderError] = useState<string | null>(null);
  const [workspaceName, setBranchName] = useState("");

  useEffect(() => {
    if (!projectPath) return;
    listHooks(projectPath).then(setPhases).catch(console.error);
    getHookVariables(projectPath)
      .then(setVariables)
      .catch(() => {});
  }, [projectPath]);

  const handleRender = async () => {
    if (!template.trim()) return;
    setRenderError(null);
    try {
      const result = await renderTemplate(
        projectPath,
        template,
        workspaceName || undefined
      );
      setRendered(result);
    } catch (e) {
      setRenderError(String(e));
      setRendered(null);
    }
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
      <h1 className="page-title">Hooks</h1>
      <p className="mono" style={{ color: "var(--text-secondary)", marginBottom: 16 }}>
        {projectPath}
      </p>

      <div className="grid grid-2">
        <div className="card">
          <div className="card-title">Hook Phases</div>
          {phases.length === 0 ? (
            <p style={{ color: "var(--text-secondary)" }}>No hooks configured.</p>
          ) : (
            phases.map((phase) => (
              <div key={phase.phase} style={{ marginBottom: 16 }}>
                <div style={{ fontWeight: 600, marginBottom: 8, color: "var(--accent)" }}>
                  {phase.phase}
                </div>
                <table className="table">
                  <thead>
                    <tr>
                      <th>Name</th>
                      <th>Command</th>
                      <th>Type</th>
                    </tr>
                  </thead>
                  <tbody>
                    {phase.hooks.map((h) => (
                      <tr key={h.name}>
                        <td>{h.name}</td>
                        <td
                          className="mono"
                          style={{ color: "var(--text-secondary)", fontSize: 12 }}
                        >
                          {h.command}
                        </td>
                        <td>
                          {h.is_extended ? (
                            <span className="badge badge-info">extended</span>
                          ) : (
                            <span className="badge badge-success">simple</span>
                          )}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            ))
          )}
        </div>

        <div>
          <div className="card">
            <div className="card-title">Template Preview</div>
            <div className="flex gap-2 mb-4">
              <input
                type="text"
                value={workspaceName}
                onChange={(e) => setBranchName(e.target.value)}
                placeholder="Workspace name (optional)"
                style={{ flex: 1 }}
              />
            </div>
            <textarea
              value={template}
              onChange={(e) => setTemplate(e.target.value)}
              placeholder="Enter a MiniJinja template... e.g. {{ workspace }}"
              style={{ marginBottom: 8 }}
            />
            <button className="btn btn-primary" onClick={handleRender}>
              Render
            </button>

            {rendered !== null && (
              <div
                style={{
                  marginTop: 12,
                  padding: 12,
                  background: "var(--bg-primary)",
                  border: "1px solid var(--border)",
                  borderRadius: 6,
                }}
              >
                <div
                  style={{
                    fontSize: 11,
                    textTransform: "uppercase",
                    color: "var(--text-muted)",
                    marginBottom: 4,
                  }}
                >
                  Output
                </div>
                <pre className="mono" style={{ whiteSpace: "pre-wrap" }}>
                  {rendered}
                </pre>
              </div>
            )}

            {renderError && (
              <div
                style={{
                  marginTop: 12,
                  padding: 12,
                  background: "rgba(248, 81, 73, 0.1)",
                  border: "1px solid var(--danger)",
                  borderRadius: 6,
                  color: "var(--danger)",
                  fontSize: 13,
                }}
              >
                {renderError}
              </div>
            )}
          </div>

          <div className="card">
            <div className="card-title">Available Variables</div>
            {Object.keys(variables).length === 0 ? (
              <p style={{ color: "var(--text-secondary)" }}>No variables available.</p>
            ) : (
              <table className="table">
                <thead>
                  <tr>
                    <th>Variable</th>
                    <th>Value</th>
                  </tr>
                </thead>
                <tbody>
                  {Object.entries(variables).map(([key, val]) => (
                    <tr key={key}>
                      <td className="mono" style={{ color: "var(--accent)" }}>
                        {key}
                      </td>
                      <td
                        className="mono"
                        style={{ color: "var(--text-secondary)", fontSize: 12 }}
                      >
                        {typeof val === "object" ? JSON.stringify(val) : String(val)}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

export default HookManager;
