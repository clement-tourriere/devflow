import { useState, useEffect, useCallback } from "react";
import { useParams, Link } from "react-router-dom";
import {
  listHooks,
  renderTemplate,
  getHookVariables,
  getActionTypes,
  runHook,
  saveHooks,
  getTriggerMappings,
} from "../../utils/invoke";
import type { HookPhaseEntry, HookInfo, ActionTypeInfo, TriggerMapping } from "../../types";
import HookEditModal from "./HookEditModal";

const PHASE_CATEGORIES: Record<string, string[]> = {
  Workspace: [
    "pre-switch",
    "post-create",
    "post-start",
    "post-switch",
    "pre-remove",
    "post-remove",
  ],
  Commit: ["pre-commit", "pre-merge", "post-merge", "post-rewrite"],
  Service: [
    "pre-service-create",
    "post-service-create",
    "pre-service-delete",
    "post-service-delete",
    "post-service-switch",
  ],
};

function HookManager() {
  const { "*": path } = useParams();
  const projectPath = path ? decodeURIComponent(path) : "";

  const [phases, setPhases] = useState<HookPhaseEntry[]>([]);
  const [variables, setVariables] = useState<Record<string, unknown>>({});
  const [actionTypes, setActionTypes] = useState<ActionTypeInfo[]>([]);
  const [triggerMappings, setTriggerMappings] = useState<TriggerMapping[]>([]);
  const [selectedPhase, setSelectedPhase] = useState<string | null>(null);
  const [template, setTemplate] = useState("");
  const [rendered, setRendered] = useState<string | null>(null);
  const [renderError, setRenderError] = useState<string | null>(null);
  const [workspaceName, setWorkspaceName] = useState("");
  const [showModal, setShowModal] = useState(false);
  const [editingHook, setEditingHook] = useState<HookInfo | undefined>(undefined);
  const [runningHook, setRunningHook] = useState<string | null>(null);
  const [runResult, setRunResult] = useState<string | null>(null);
  const [deletingHook, setDeletingHook] = useState<string | null>(null);
  const [rightTab, setRightTab] = useState<"preview" | "variables" | "triggers">("preview");

  const loadData = useCallback(() => {
    if (!projectPath) return;
    listHooks(projectPath).then(setPhases).catch(console.error);
    getHookVariables(projectPath)
      .then(setVariables)
      .catch(() => {});
    getActionTypes().then(setActionTypes).catch(console.error);
    getTriggerMappings(projectPath).then(setTriggerMappings).catch(() => {});
  }, [projectPath]);

  useEffect(() => {
    loadData();
  }, [loadData]);

  const selectedPhaseData = phases.find((p) => p.phase === selectedPhase);

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

  const handleRunHook = async (phase: string, hookName: string) => {
    const key = `${phase}:${hookName}`;
    setRunningHook(key);
    setRunResult(null);
    try {
      const result = await runHook(projectPath, phase, hookName, workspaceName || undefined);
      if (result.errors.length > 0) {
        setRunResult(`Failed: ${result.errors.join(", ")}`);
      } else {
        setRunResult(`Succeeded: ${result.succeeded}, Skipped: ${result.skipped}`);
      }
    } catch (e) {
      setRunResult(`Error: ${String(e)}`);
    } finally {
      setRunningHook(null);
    }
  };

  const handleDeleteHook = async (phaseName: string, hookName: string) => {
    const key = `${phaseName}:${hookName}`;
    setDeletingHook(key);
    try {
      const currentPhases = await listHooks(projectPath);
      const hooksObj: Record<string, Record<string, unknown>> = {};

      for (const p of currentPhases) {
        hooksObj[p.phase] = {};
        for (const h of p.hooks) {
          if (p.phase === phaseName && h.name === hookName) continue;
          hooksObj[p.phase][h.name] = h.raw as Record<string, unknown>;
        }
      }

      await saveHooks(projectPath, hooksObj);
      loadData();
    } catch (e) {
      setRunResult(`Delete failed: ${String(e)}`);
    } finally {
      setDeletingHook(null);
    }
  };

  const handleEditHook = (hook: HookInfo) => {
    setEditingHook(hook);
    setShowModal(true);
  };

  const hookCountForPhase = (phaseName: string) => {
    const p = phases.find((ph) => ph.phase === phaseName);
    return p ? p.hooks.length : 0;
  };

  const hookTypeBadge = (hook: HookInfo) => {
    if (hook.action_type) {
      return <span className="badge badge-info">{hook.action_type}</span>;
    }
    if (hook.is_extended) {
      return <span className="badge badge-warning">extended</span>;
    }
    return <span className="badge badge-success">simple</span>;
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
      <p
        className="mono"
        style={{ color: "var(--text-secondary)", marginBottom: 16 }}
      >
        {projectPath}
      </p>

      <div style={{ display: "grid", gridTemplateColumns: "200px 1fr 320px", gap: 16 }}>
        {/* Left Panel: Phase List */}
        <div className="card" style={{ padding: 0 }}>
          <div
            style={{
              padding: "12px 16px",
              borderBottom: "1px solid var(--border)",
              fontWeight: 600,
              fontSize: 13,
            }}
          >
            Phases
          </div>
          {Object.entries(PHASE_CATEGORIES).map(([category, phaseNames]) => (
            <div key={category}>
              <div
                style={{
                  padding: "8px 16px 4px",
                  fontSize: 11,
                  textTransform: "uppercase",
                  color: "var(--text-muted)",
                  fontWeight: 600,
                }}
              >
                {category}
              </div>
              {phaseNames.map((phaseName) => {
                const count = hookCountForPhase(phaseName);
                return (
                  <div
                    key={phaseName}
                    onClick={() => setSelectedPhase(phaseName)}
                    style={{
                      padding: "6px 16px",
                      cursor: "pointer",
                      fontSize: 13,
                      background:
                        selectedPhase === phaseName
                          ? "var(--bg-selected, rgba(88,166,255,0.15))"
                          : "transparent",
                      color:
                        selectedPhase === phaseName
                          ? "var(--accent)"
                          : "var(--text-primary)",
                      display: "flex",
                      justifyContent: "space-between",
                      alignItems: "center",
                    }}
                  >
                    <span>{phaseName}</span>
                    {count > 0 && (
                      <span
                        className="badge badge-info"
                        style={{ fontSize: 10, minWidth: 18, textAlign: "center" }}
                      >
                        {count}
                      </span>
                    )}
                  </div>
                );
              })}
            </div>
          ))}
        </div>

        {/* Center Panel: Selected Phase Hooks */}
        <div className="card">
          {!selectedPhase ? (
            <div style={{ color: "var(--text-secondary)", padding: 24, textAlign: "center" }}>
              Select a phase from the left panel to view its hooks.
            </div>
          ) : (
            <>
              <div
                style={{
                  display: "flex",
                  justifyContent: "space-between",
                  alignItems: "center",
                  marginBottom: 16,
                }}
              >
                <div className="card-title" style={{ margin: 0 }}>
                  {selectedPhase}
                </div>
                <button
                  className="btn btn-primary"
                  style={{ fontSize: 12, padding: "4px 12px" }}
                  onClick={() => {
                    setEditingHook(undefined);
                    setShowModal(true);
                  }}
                >
                  + Add Hook
                </button>
              </div>

              {!selectedPhaseData || selectedPhaseData.hooks.length === 0 ? (
                <p style={{ color: "var(--text-secondary)" }}>
                  No hooks configured for this phase.
                </p>
              ) : (
                <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
                  {selectedPhaseData.hooks.map((hook) => {
                    const hookKey = `${selectedPhase}:${hook.name}`;
                    const isRunning = runningHook === hookKey;
                    const isDeleting = deletingHook === hookKey;
                    return (
                      <div
                        key={hook.name}
                        style={{
                          padding: "10px 12px",
                          border: "1px solid var(--border)",
                          borderRadius: 8,
                          background: "var(--bg-primary)",
                        }}
                      >
                        <div
                          style={{
                            display: "flex",
                            justifyContent: "space-between",
                            alignItems: "center",
                          }}
                        >
                          <div style={{ display: "flex", alignItems: "center", gap: 8, minWidth: 0 }}>
                            <span style={{ fontWeight: 600, fontSize: 13 }}>
                              {hook.name}
                            </span>
                            {hookTypeBadge(hook)}
                          </div>
                          <div style={{ display: "flex", gap: 2, flexShrink: 0 }}>
                            <IconButton
                              icon="edit"
                              title="Edit"
                              onClick={() => handleEditHook(hook)}
                            />
                            <IconButton
                              icon="play"
                              title="Run"
                              disabled={runningHook !== null}
                              loading={isRunning}
                              onClick={() => handleRunHook(selectedPhase, hook.name)}
                            />
                            <IconButton
                              icon="trash"
                              title="Delete"
                              variant="danger"
                              disabled={deletingHook !== null}
                              loading={isDeleting}
                              onClick={() => {
                                if (confirm(`Delete hook "${hook.name}"?`)) {
                                  handleDeleteHook(selectedPhase, hook.name);
                                }
                              }}
                            />
                          </div>
                        </div>
                        <div
                          className="mono"
                          style={{
                            fontSize: 11,
                            color: "var(--text-muted)",
                            marginTop: 4,
                            overflow: "hidden",
                            textOverflow: "ellipsis",
                            whiteSpace: "nowrap",
                          }}
                        >
                          {hook.command}
                        </div>
                        {hook.condition && (
                          <div
                            style={{
                              marginTop: 4,
                              fontSize: 11,
                              color: "var(--text-secondary)",
                              display: "flex",
                              alignItems: "center",
                              gap: 4,
                            }}
                            title={hook.condition}
                          >
                            <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" style={{ opacity: 0.6, flexShrink: 0 }}>
                              <polyline points="4 17 10 11 4 5" /><line x1="12" y1="19" x2="20" y2="19" />
                            </svg>
                            <span className="mono" style={{ overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
                              {hook.condition}
                            </span>
                          </div>
                        )}
                      </div>
                    );
                  })}
                </div>
              )}

              {runResult && (
                <div
                  style={{
                    marginTop: 12,
                    padding: 8,
                    fontSize: 12,
                    borderRadius: 6,
                    background: runResult.startsWith("Error") || runResult.startsWith("Failed")
                      ? "rgba(248, 81, 73, 0.1)"
                      : "rgba(63, 185, 80, 0.1)",
                    color: runResult.startsWith("Error") || runResult.startsWith("Failed")
                      ? "var(--danger)"
                      : "var(--success, #3fb950)",
                  }}
                >
                  {runResult}
                </div>
              )}
            </>
          )}
        </div>

        {/* Right Panel: Tools */}
        <div>
          <div className="card" style={{ padding: 0 }}>
            <div style={{ display: "flex", borderBottom: "1px solid var(--border)" }}>
              {(["preview", "variables", "triggers"] as const).map((tab) => (
                <button
                  key={tab}
                  onClick={() => setRightTab(tab)}
                  style={{
                    flex: 1,
                    padding: "8px 4px",
                    fontSize: 12,
                    border: "none",
                    borderBottom: rightTab === tab ? "2px solid var(--accent)" : "2px solid transparent",
                    background: "transparent",
                    color:
                      rightTab === tab ? "var(--accent)" : "var(--text-secondary)",
                    cursor: "pointer",
                    fontWeight: rightTab === tab ? 600 : 400,
                  }}
                >
                  {tab === "preview"
                    ? "Preview"
                    : tab === "variables"
                      ? "Variables"
                      : "Triggers"}
                </button>
              ))}
            </div>

            <div style={{ padding: 16 }}>
              {rightTab === "preview" && (
                <>
                  <div className="flex gap-2 mb-4">
                    <input
                      type="text"
                      value={workspaceName}
                      onChange={(e) => setWorkspaceName(e.target.value)}
                      placeholder="Workspace (optional)"
                      style={{ flex: 1, fontSize: 12 }}
                    />
                  </div>
                  <textarea
                    value={template}
                    onChange={(e) => setTemplate(e.target.value)}
                    placeholder="Enter a MiniJinja template..."
                    style={{ marginBottom: 8, fontSize: 12, minHeight: 60 }}
                  />
                  <button className="btn btn-primary" onClick={handleRender} style={{ fontSize: 12 }}>
                    Render
                  </button>

                  {rendered !== null && (
                    <div
                      style={{
                        marginTop: 12,
                        padding: 8,
                        background: "var(--bg-primary)",
                        border: "1px solid var(--border)",
                        borderRadius: 6,
                      }}
                    >
                      <div
                        style={{
                          fontSize: 10,
                          textTransform: "uppercase",
                          color: "var(--text-muted)",
                          marginBottom: 4,
                        }}
                      >
                        Output
                      </div>
                      <pre className="mono" style={{ whiteSpace: "pre-wrap", fontSize: 12 }}>
                        {rendered}
                      </pre>
                    </div>
                  )}
                  {renderError && (
                    <div
                      style={{
                        marginTop: 12,
                        padding: 8,
                        background: "rgba(248, 81, 73, 0.1)",
                        border: "1px solid var(--danger)",
                        borderRadius: 6,
                        color: "var(--danger)",
                        fontSize: 12,
                      }}
                    >
                      {renderError}
                    </div>
                  )}
                </>
              )}

              {rightTab === "variables" && (
                <>
                  {Object.keys(variables).length === 0 ? (
                    <p style={{ color: "var(--text-secondary)", fontSize: 13 }}>
                      No variables available.
                    </p>
                  ) : (
                    <div style={{ fontSize: 12 }}>
                      {Object.entries(variables).map(([key, val]) => (
                        <div
                          key={key}
                          style={{
                            padding: "4px 0",
                            borderBottom: "1px solid var(--border)",
                            display: "flex",
                            gap: 8,
                          }}
                        >
                          <span
                            className="mono"
                            style={{ color: "var(--accent)", minWidth: 100, flexShrink: 0 }}
                          >
                            {key}
                          </span>
                          <span
                            className="mono"
                            style={{
                              color: "var(--text-secondary)",
                              wordBreak: "break-all",
                            }}
                          >
                            {typeof val === "object" ? JSON.stringify(val) : String(val)}
                          </span>
                        </div>
                      ))}
                    </div>
                  )}
                </>
              )}

              {rightTab === "triggers" && (
                <>
                  <div style={{ fontSize: 12, marginBottom: 8, color: "var(--text-secondary)" }}>
                    VCS Event &rarr; Devflow Phase(s)
                  </div>
                  {triggerMappings.length === 0 ? (
                    <p style={{ color: "var(--text-secondary)", fontSize: 13 }}>
                      No trigger mappings configured.
                    </p>
                  ) : (
                    <>
                      {/* Show triggers relevant to the selected phase first */}
                      {selectedPhase && (() => {
                        const relevant = triggerMappings.filter((m) =>
                          m.phases.includes(selectedPhase)
                        );
                        if (relevant.length === 0) return (
                          <div style={{
                            padding: "8px 10px",
                            marginBottom: 8,
                            fontSize: 12,
                            color: "var(--text-muted)",
                            background: "var(--bg-primary)",
                            borderRadius: 6,
                          }}>
                            No VCS trigger fires <span className="mono" style={{ color: "var(--accent)" }}>{selectedPhase}</span>
                          </div>
                        );
                        return (
                          <div style={{
                            padding: "8px 10px",
                            marginBottom: 8,
                            fontSize: 12,
                            background: "rgba(88,166,255,0.06)",
                            border: "1px solid rgba(88,166,255,0.15)",
                            borderRadius: 6,
                          }}>
                            <span className="mono" style={{ color: "var(--accent)" }}>{selectedPhase}</span>
                            <span style={{ color: "var(--text-secondary)" }}> is triggered by: </span>
                            {relevant.map((m) => (
                              <span key={m.vcs_event} className="mono" style={{ color: "var(--text-primary)" }}>
                                git {m.vcs_event}{relevant.indexOf(m) < relevant.length - 1 ? ", " : ""}
                              </span>
                            ))}
                          </div>
                        );
                      })()}
                      {triggerMappings.map((m) => {
                        const isActive = selectedPhase && m.phases.includes(selectedPhase);
                        return (
                          <div
                            key={m.vcs_event}
                            style={{
                              padding: "6px 0",
                              borderBottom: "1px solid var(--border)",
                              fontSize: 12,
                              opacity: selectedPhase && !isActive ? 0.45 : 1,
                            }}
                          >
                            <span className="mono" style={{ color: "var(--text-primary)" }}>
                              git {m.vcs_event}
                            </span>
                            <span style={{ color: "var(--text-muted)", margin: "0 8px" }}>&rarr;</span>
                            {m.phases.map((p) => (
                              <span
                                key={p}
                                className={`badge ${p === selectedPhase ? "badge-success" : "badge-info"}`}
                                style={{ fontSize: 10, marginRight: 4 }}
                              >
                                {p}
                              </span>
                            ))}
                          </div>
                        );
                      })}
                    </>
                  )}
                </>
              )}
            </div>
          </div>
        </div>
      </div>

      {showModal && (
        <HookEditModal
          projectPath={projectPath}
          phase={selectedPhase || "post-create"}
          actionTypes={actionTypes}
          editingHook={editingHook}
          onClose={() => {
            setShowModal(false);
            setEditingHook(undefined);
          }}
          onSaved={() => {
            setShowModal(false);
            setEditingHook(undefined);
            loadData();
          }}
        />
      )}
    </div>
  );
}

const ICONS = {
  edit: (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <path d="M11 4H4a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7" />
      <path d="M18.5 2.5a2.121 2.121 0 0 1 3 3L12 15l-4 1 1-4 9.5-9.5z" />
    </svg>
  ),
  play: (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <polygon points="5 3 19 12 5 21 5 3" />
    </svg>
  ),
  trash: (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <polyline points="3 6 5 6 21 6" />
      <path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" />
    </svg>
  ),
} as const;

function IconButton({
  icon,
  title,
  onClick,
  disabled,
  loading,
  variant,
}: {
  icon: keyof typeof ICONS;
  title: string;
  onClick: () => void;
  disabled?: boolean;
  loading?: boolean;
  variant?: "danger";
}) {
  const color =
    variant === "danger" ? "var(--danger)" : "var(--text-secondary)";
  const hoverColor =
    variant === "danger" ? "var(--danger)" : "var(--text-primary)";

  return (
    <button
      onClick={onClick}
      disabled={disabled}
      title={title}
      style={{
        background: "none",
        border: "1px solid transparent",
        borderRadius: 6,
        padding: 4,
        cursor: disabled ? "default" : "pointer",
        color,
        opacity: disabled || loading ? 0.4 : 0.7,
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        transition: "opacity 0.15s, color 0.15s, border-color 0.15s",
      }}
      onMouseEnter={(e) => {
        if (!disabled) {
          e.currentTarget.style.opacity = "1";
          e.currentTarget.style.color = hoverColor;
          e.currentTarget.style.borderColor = "var(--border)";
        }
      }}
      onMouseLeave={(e) => {
        e.currentTarget.style.opacity = disabled || loading ? "0.4" : "0.7";
        e.currentTarget.style.color = color;
        e.currentTarget.style.borderColor = "transparent";
      }}
    >
      {loading ? (
        <svg
          width="14"
          height="14"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
          style={{ animation: "spin 1s linear infinite" }}
        >
          <path d="M21 12a9 9 0 1 1-6.219-8.56" />
        </svg>
      ) : (
        ICONS[icon]
      )}
    </button>
  );
}

export default HookManager;
