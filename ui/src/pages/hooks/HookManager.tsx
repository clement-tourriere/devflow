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
  getRecipes,
  installRecipe,
  installRecipes,
} from "../../utils/invoke";
import type { HookPhaseEntry, HookInfo, ActionTypeInfo, TriggerMapping, RecipeInfo } from "../../types";
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
  const [recipes, setRecipes] = useState<RecipeInfo[]>([]);
  const [installingRecipe, setInstallingRecipe] = useState<string | null>(null);
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
  const [rightTab, setRightTab] = useState<"preview" | "variables" | "triggers" | "recipes">("preview");

  const loadData = useCallback(() => {
    if (!projectPath) return;
    listHooks(projectPath).then(setPhases).catch(console.error);
    getHookVariables(projectPath)
      .then(setVariables)
      .catch(() => {});
    getActionTypes().then(setActionTypes).catch(console.error);
    getTriggerMappings(projectPath).then(setTriggerMappings).catch(() => {});
    getRecipes().then(setRecipes).catch(() => {});
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

  const handleInstallRecipe = async (recipeName: string) => {
    setInstallingRecipe(recipeName);
    try {
      const result = await installRecipe(projectPath, recipeName);
      if (result.hooks_added > 0) {
        setRunResult(`Recipe installed: ${result.hooks_added} hook(s) added, ${result.hooks_skipped} skipped`);
      } else {
        setRunResult(`Recipe already installed (${result.hooks_skipped} hook(s) skipped)`);
      }
      loadData();
    } catch (e) {
      setRunResult(`Install failed: ${String(e)}`);
    } finally {
      setInstallingRecipe(null);
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
              {(["preview", "variables", "triggers", "recipes"] as const).map((tab) => (
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
                      : tab === "triggers"
                        ? "Triggers"
                        : "Recipes"}
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
                <VariableBrowser variables={variables} />
              )}

              {rightTab === "recipes" && (
                <>
                  {/* Setup Profiles */}
                  <RecipeProfiles
                    projectPath={projectPath}
                    recipes={recipes}
                    phases={phases}
                    onInstalled={(result) => {
                      if (result.hooks_added > 0) {
                        setRunResult(`Profile installed: ${result.hooks_added} hook(s) added, ${result.hooks_skipped} skipped`);
                      } else {
                        setRunResult(`Profile already installed (${result.hooks_skipped} hook(s) skipped)`);
                      }
                      loadData();
                    }}
                    onError={(e) => setRunResult(`Install failed: ${String(e)}`)}
                  />

                  <div style={{ fontSize: 12, marginBottom: 8, color: "var(--text-secondary)" }}>
                    Pre-built hook configurations you can install with one click.
                  </div>
                  {recipes.length === 0 ? (
                    <p style={{ color: "var(--text-secondary)", fontSize: 13 }}>
                      No recipes available.
                    </p>
                  ) : (
                    <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
                      {(() => {
                        const categories = Array.from(new Set(recipes.map((r) => r.category)));
                        return categories.map((cat) => (
                          <div key={cat}>
                            <div
                              style={{
                                fontSize: 10,
                                textTransform: "uppercase",
                                color: "var(--text-muted)",
                                fontWeight: 600,
                                marginBottom: 4,
                              }}
                            >
                              {cat}
                            </div>
                            {recipes
                              .filter((r) => r.category === cat)
                              .map((recipe) => {
                                const isInstalled = recipe.hooks_preview.every((h) =>
                                  phases.some(
                                    (p) =>
                                      p.phase === h.phase &&
                                      p.hooks.some((hook) => hook.name === h.hook_name)
                                  )
                                );
                                const isInstalling = installingRecipe === recipe.name;
                                return (
                                  <div
                                    key={recipe.name}
                                    style={{
                                      padding: "10px 12px",
                                      border: "1px solid var(--border)",
                                      borderRadius: 8,
                                      background: "var(--bg-primary)",
                                      marginBottom: 4,
                                    }}
                                  >
                                    <div
                                      style={{
                                        display: "flex",
                                        justifyContent: "space-between",
                                        alignItems: "center",
                                      }}
                                    >
                                      <span style={{ fontWeight: 600, fontSize: 13 }}>
                                        {recipe.name}
                                      </span>
                                      {isInstalled ? (
                                        <span className="badge badge-success" style={{ fontSize: 10 }}>
                                          installed
                                        </span>
                                      ) : (
                                        <button
                                          className="btn btn-primary"
                                          style={{ fontSize: 11, padding: "2px 10px" }}
                                          disabled={isInstalling}
                                          onClick={() => handleInstallRecipe(recipe.name)}
                                        >
                                          {isInstalling ? "Installing..." : "Install"}
                                        </button>
                                      )}
                                    </div>
                                    <div
                                      style={{
                                        fontSize: 12,
                                        color: "var(--text-secondary)",
                                        marginTop: 4,
                                      }}
                                    >
                                      {recipe.description}
                                    </div>
                                    <div style={{ marginTop: 6 }}>
                                      {recipe.hooks_preview.map((h) => (
                                        <div
                                          key={`${h.phase}:${h.hook_name}`}
                                          className="mono"
                                          style={{
                                            fontSize: 11,
                                            color: "var(--text-muted)",
                                            padding: "2px 0",
                                          }}
                                        >
                                          <span className="badge badge-info" style={{ fontSize: 9, marginRight: 4 }}>
                                            {h.phase}
                                          </span>
                                          {h.hook_name}
                                        </div>
                                      ))}
                                    </div>
                                  </div>
                                );
                              })}
                          </div>
                        ));
                      })()}
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
          recipes={recipes}
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

// ── Variable Browser ────────────────────────────────────────────────

const VARIABLE_CATEGORIES: {
  label: string;
  vars: { key: string; expr: string; description?: string }[];
}[] = [
  {
    label: "Workspace",
    vars: [
      { key: "workspace", expr: "{{ workspace }}", description: "Current workspace name" },
      { key: "default_workspace", expr: "{{ default_workspace }}", description: "Default workspace (e.g. main)" },
      { key: "name", expr: "{{ name }}", description: "Project name" },
      { key: "repo", expr: "{{ repo }}", description: "Repository name" },
      { key: "worktree_path", expr: "{{ worktree_path }}", description: "Path to worktree directory" },
    ],
  },
  {
    label: "VCS",
    vars: [
      { key: "commit", expr: "{{ commit }}", description: "Full commit hash" },
      { key: "short_commit", expr: "{{ short_commit }}", description: "Short commit hash" },
      { key: "target", expr: "{{ target }}", description: "Target workspace for merge" },
      { key: "base", expr: "{{ base }}", description: "Base workspace" },
      { key: "previous_workspace", expr: "{{ previous_workspace }}", description: "Previously checked-out workspace" },
    ],
  },
  {
    label: "Trigger",
    vars: [
      { key: "trigger_source", expr: "{{ trigger_source }}", description: "What triggered the hook (vcs, cli, gui, auto)" },
      { key: "vcs_event", expr: "{{ vcs_event }}", description: "VCS event name" },
    ],
  },
  {
    label: "Services",
    vars: [
      { key: "service[name].host", expr: "{{ service['SERVICE_NAME'].host }}", description: "Service host" },
      { key: "service[name].port", expr: "{{ service['SERVICE_NAME'].port }}", description: "Service port" },
      { key: "service[name].database", expr: "{{ service['SERVICE_NAME'].database }}", description: "Database name" },
      { key: "service[name].user", expr: "{{ service['SERVICE_NAME'].user }}", description: "Service user" },
      { key: "service[name].url", expr: "{{ service['SERVICE_NAME'].url }}", description: "Full connection URL" },
    ],
  },
];

function VariableBrowser({ variables }: { variables: Record<string, unknown> }) {
  const [search, setSearch] = useState("");
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>({});
  const [copied, setCopied] = useState<string | null>(null);
  const [syntaxOpen, setSyntaxOpen] = useState(false);

  const searchLower = search.toLowerCase();

  const copyToClipboard = (expr: string) => {
    navigator.clipboard.writeText(expr).then(() => {
      setCopied(expr);
      setTimeout(() => setCopied(null), 1500);
    });
  };

  // Build live service entries from actual variables
  const serviceVars: { key: string; expr: string; value?: string }[] = [];
  if (variables.service && typeof variables.service === "object") {
    const services = variables.service as Record<string, Record<string, unknown>>;
    for (const [svcName, svcData] of Object.entries(services)) {
      for (const field of ["host", "port", "database", "user", "url"]) {
        if (svcData[field] !== undefined) {
          serviceVars.push({
            key: `service['${svcName}'].${field}`,
            expr: `{{ service['${svcName}'].${field} }}`,
            value: String(svcData[field]),
          });
        }
      }
    }
  }

  return (
    <div style={{ fontSize: 12 }}>
      <input
        type="text"
        value={search}
        onChange={(e) => setSearch(e.target.value)}
        placeholder="Search variables..."
        style={{ width: "100%", fontSize: 12, marginBottom: 8 }}
      />

      {Object.keys(variables).length === 0 ? (
        <p style={{ color: "var(--text-secondary)", fontSize: 13 }}>
          No variables available.
        </p>
      ) : (
        <>
          {VARIABLE_CATEGORIES.map((cat) => {
            const isServices = cat.label === "Services";
            const items: { key: string; expr: string; value?: string }[] = isServices
              ? serviceVars.length > 0
                ? serviceVars
                : cat.vars
              : cat.vars.map((v) => ({
                  key: v.key,
                  expr: v.expr,
                  value:
                    variables[v.key] !== undefined
                      ? typeof variables[v.key] === "object"
                        ? JSON.stringify(variables[v.key])
                        : String(variables[v.key])
                      : undefined,
                }));

            const filtered = items.filter(
              (v) =>
                !searchLower ||
                v.key.toLowerCase().includes(searchLower) ||
                v.expr.toLowerCase().includes(searchLower)
            );
            if (filtered.length === 0) return null;

            const isCollapsed = collapsed[cat.label];

            return (
              <div key={cat.label} style={{ marginBottom: 6 }}>
                <div
                  onClick={() =>
                    setCollapsed((p) => ({ ...p, [cat.label]: !p[cat.label] }))
                  }
                  style={{
                    fontSize: 10,
                    textTransform: "uppercase",
                    color: "var(--text-muted)",
                    fontWeight: 600,
                    padding: "4px 0",
                    cursor: "pointer",
                    display: "flex",
                    alignItems: "center",
                    gap: 4,
                    userSelect: "none",
                  }}
                >
                  <span style={{ fontSize: 8 }}>{isCollapsed ? "\u25B6" : "\u25BC"}</span>
                  {cat.label}
                  <span style={{ fontWeight: 400, fontSize: 10 }}>({filtered.length})</span>
                </div>
                {!isCollapsed &&
                  filtered.map((v) => (
                    <div
                      key={v.key}
                      style={{
                        padding: "3px 0",
                        borderBottom: "1px solid var(--border)",
                        display: "flex",
                        alignItems: "center",
                        gap: 6,
                      }}
                    >
                      <div style={{ flex: 1, minWidth: 0 }}>
                        <div
                          className="mono"
                          style={{ color: "var(--accent)", fontSize: 12 }}
                        >
                          {v.key}
                        </div>
                        <div
                          className="mono"
                          style={{
                            color: "var(--text-muted)",
                            fontSize: 10,
                            overflow: "hidden",
                            textOverflow: "ellipsis",
                            whiteSpace: "nowrap",
                          }}
                        >
                          {v.expr}
                        </div>
                        {v.value !== undefined && (
                          <div
                            className="mono"
                            style={{
                              color: "var(--text-secondary)",
                              fontSize: 10,
                              overflow: "hidden",
                              textOverflow: "ellipsis",
                              whiteSpace: "nowrap",
                            }}
                          >
                            = {v.value}
                          </div>
                        )}
                      </div>
                      <button
                        onClick={() => copyToClipboard(v.expr)}
                        title="Copy expression"
                        style={{
                          background: "none",
                          border: "1px solid var(--border)",
                          borderRadius: 4,
                          padding: "2px 6px",
                          cursor: "pointer",
                          fontSize: 10,
                          color:
                            copied === v.expr
                              ? "var(--success, #3fb950)"
                              : "var(--text-secondary)",
                          flexShrink: 0,
                        }}
                      >
                        {copied === v.expr ? "Copied!" : "Copy"}
                      </button>
                    </div>
                  ))}
              </div>
            );
          })}

          {/* Syntax Quick Reference */}
          <div style={{ marginTop: 8, borderTop: "1px solid var(--border)", paddingTop: 6 }}>
            <div
              onClick={() => setSyntaxOpen((p) => !p)}
              style={{
                fontSize: 11,
                color: "var(--text-muted)",
                cursor: "pointer",
                userSelect: "none",
                display: "flex",
                alignItems: "center",
                gap: 4,
              }}
            >
              <span style={{ fontSize: 8 }}>{syntaxOpen ? "\u25BC" : "\u25B6"}</span>
              Syntax Help
            </div>
            {syntaxOpen && (
              <div
                className="mono"
                style={{
                  marginTop: 4,
                  padding: 8,
                  background: "var(--bg-primary)",
                  borderRadius: 6,
                  fontSize: 10,
                  color: "var(--text-secondary)",
                  lineHeight: 1.6,
                }}
              >
                <div><strong>Filters:</strong> sanitize, sanitize_db, hash_port, lower, upper, replace(a,b), truncate(n)</div>
                <div><strong>Syntax:</strong> {"{{ var }}"}, {"{{ var | filter }}"}, {"{{ service['name'].url }}"}</div>
              </div>
            )}
          </div>
        </>
      )}
    </div>
  );
}

// ── Recipe Profiles ─────────────────────────────────────────────────

const RECIPE_PROFILES = [
  {
    name: "Local Dev",
    description: "Install deps + copy .env + trust mise + run migrations",
    recipes: ["install-deps", "local-dev-setup", "db-migrate"],
  },
  {
    name: "Docker Dev",
    description: "Manage Docker Compose lifecycle automatically",
    recipes: ["docker-compose"],
  },
];

function RecipeProfiles({
  projectPath,
  recipes,
  phases,
  onInstalled,
  onError,
}: {
  projectPath: string;
  recipes: RecipeInfo[];
  phases: HookPhaseEntry[];
  onInstalled: (result: { hooks_added: number; hooks_skipped: number }) => void;
  onError: (e: unknown) => void;
}) {
  const [installingProfile, setInstallingProfile] = useState<string | null>(null);

  const isProfileInstalled = (recipeNames: string[]) => {
    return recipeNames.every((name) => {
      const recipe = recipes.find((r) => r.name === name);
      if (!recipe) return false;
      return recipe.hooks_preview.every((h) =>
        phases.some(
          (p) =>
            p.phase === h.phase &&
            p.hooks.some((hook) => hook.name === h.hook_name)
        )
      );
    });
  };

  const handleInstallProfile = async (profileName: string, recipeNames: string[]) => {
    setInstallingProfile(profileName);
    try {
      const result = await installRecipes(projectPath, recipeNames);
      onInstalled(result);
    } catch (e) {
      onError(e);
    } finally {
      setInstallingProfile(null);
    }
  };

  return (
    <div style={{ marginBottom: 12 }}>
      <div
        style={{
          fontSize: 10,
          textTransform: "uppercase",
          color: "var(--text-muted)",
          fontWeight: 600,
          marginBottom: 6,
        }}
      >
        Quick Setup Profiles
      </div>
      <div style={{ display: "flex", gap: 8, marginBottom: 12 }}>
        {RECIPE_PROFILES.map((profile) => {
          const installed = isProfileInstalled(profile.recipes);
          const isInstalling = installingProfile === profile.name;
          return (
            <div
              key={profile.name}
              style={{
                flex: 1,
                padding: "10px 12px",
                border: "1px solid var(--border)",
                borderRadius: 8,
                background: installed
                  ? "rgba(63, 185, 80, 0.06)"
                  : "var(--bg-primary)",
              }}
            >
              <div style={{ fontWeight: 600, fontSize: 13, marginBottom: 2 }}>
                {profile.name}
              </div>
              <div
                style={{
                  fontSize: 11,
                  color: "var(--text-secondary)",
                  marginBottom: 6,
                }}
              >
                {profile.description}
              </div>
              {installed ? (
                <span className="badge badge-success" style={{ fontSize: 10 }}>
                  installed
                </span>
              ) : (
                <button
                  className="btn btn-primary"
                  style={{ fontSize: 11, padding: "2px 10px" }}
                  disabled={isInstalling}
                  onClick={() =>
                    handleInstallProfile(profile.name, profile.recipes)
                  }
                >
                  {isInstalling ? "Installing..." : "Install Profile"}
                </button>
              )}
            </div>
          );
        })}
      </div>
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
