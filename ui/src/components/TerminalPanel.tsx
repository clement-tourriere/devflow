import { useState, useEffect, useCallback, useRef, useMemo } from "react";
import { listen } from "@tauri-apps/api/event";
import Terminal from "./Terminal";
import Modal from "./Modal";
import {
  createTerminal as createTerminalInvoke,
  closeTerminal as closeTerminalInvoke,
  listTerminals,
  listProjects,
  listBranches,
  listServices,
  getSettings,
} from "../utils/invoke";
import type {
  AppSettings,
  BranchEntry,
  ProjectEntry,
  ServiceEntry,
  TerminalExitEvent,
  TerminalRenderer,
  TerminalSessionInfo,
} from "../types";

const MIN_PANEL_HEIGHT = 120;
const DEFAULT_PANEL_HEIGHT = 300;
const STORAGE_KEY = "devflow-terminal-panel-height";

interface TerminalPanelProps {
  isVisible: boolean;
  onToggle: () => void;
  pendingTerminal: {
    projectPath?: string;
    branchName?: string;
    serviceName?: string;
  } | null;
  onPendingTerminalHandled: () => void;
}

function normalizeRenderer(value?: string): TerminalRenderer {
  if (value === "webgpu" || value === "webgl2") return value;
  return "auto";
}

function normalizeFontSize(value?: number): number {
  if (typeof value !== "number" || Number.isNaN(value)) return 14;
  return Math.max(11, Math.min(24, Math.round(value)));
}

function TerminalPanel({
  isVisible,
  onToggle,
  pendingTerminal,
  onPendingTerminalHandled,
}: TerminalPanelProps) {
  const [tabs, setTabs] = useState<TerminalSessionInfo[]>([]);
  const [activeTabId, setActiveTabId] = useState<string | null>(null);
  const [panelHeight, setPanelHeight] = useState<number>(() => {
    const saved = localStorage.getItem(STORAGE_KEY);
    return saved ? parseInt(saved, 10) : DEFAULT_PANEL_HEIGHT;
  });
  const [isResizing, setIsResizing] = useState(false);
  const [terminalRenderer, setTerminalRenderer] = useState<TerminalRenderer>("auto");
  const [terminalFontSize, setTerminalFontSize] = useState(14);

  // Launcher state
  const [showLauncher, setShowLauncher] = useState(false);
  const [launcherProjects, setLauncherProjects] = useState<ProjectEntry[]>([]);
  const [launcherProjectPath, setLauncherProjectPath] = useState("");
  const [launcherBranches, setLauncherBranches] = useState<BranchEntry[]>([]);
  const [launcherBranchName, setLauncherBranchName] = useState("");
  const [launcherServices, setLauncherServices] = useState<ServiceEntry[]>([]);
  const [launcherServiceName, setLauncherServiceName] = useState("");
  const [launcherProjectFilter, setLauncherProjectFilter] = useState("");
  const [launcherBranchFilter, setLauncherBranchFilter] = useState("");
  const [launcherServiceFilter, setLauncherServiceFilter] = useState("");
  const [launcherLoadingProjects, setLauncherLoadingProjects] = useState(false);
  const [launcherLoadingTargets, setLauncherLoadingTargets] = useState(false);
  const [launcherError, setLauncherError] = useState<string | null>(null);

  const panelRef = useRef<HTMLDivElement>(null);

  const filteredProjects = useMemo(() => {
    const query = launcherProjectFilter.trim().toLowerCase();
    if (!query) return launcherProjects;
    return launcherProjects.filter(
      (project) =>
        project.name.toLowerCase().includes(query) ||
        project.path.toLowerCase().includes(query)
    );
  }, [launcherProjects, launcherProjectFilter]);

  const filteredBranches = useMemo(() => {
    const query = launcherBranchFilter.trim().toLowerCase();
    if (!query) return launcherBranches;
    return launcherBranches.filter((branch) =>
      branch.name.toLowerCase().includes(query)
    );
  }, [launcherBranches, launcherBranchFilter]);

  const filteredServices = useMemo(() => {
    const query = launcherServiceFilter.trim().toLowerCase();
    if (!query) return launcherServices;
    return launcherServices.filter(
      (service) =>
        service.name.toLowerCase().includes(query) ||
        service.service_type.toLowerCase().includes(query) ||
        service.provider_type.toLowerCase().includes(query)
    );
  }, [launcherServices, launcherServiceFilter]);

  const selectedProject = useMemo(
    () => launcherProjects.find((project) => project.path === launcherProjectPath) ?? null,
    [launcherProjects, launcherProjectPath]
  );

  const previewLabel = useMemo(() => {
    const projectName = selectedProject?.name ?? "terminal";
    if (!launcherBranchName) return projectName;
    if (launcherServiceName) {
      return `${launcherServiceName}:${projectName}/${launcherBranchName}`;
    }
    return `${projectName}/${launcherBranchName}`;
  }, [selectedProject, launcherBranchName, launcherServiceName]);

  // Load existing terminal sessions
  useEffect(() => {
    listTerminals()
      .then((sessions) => {
        setTabs(sessions);
        if (sessions.length > 0) {
          setActiveTabId((prev) => prev ?? sessions[0].id);
        }
      })
      .catch(() => {});
  }, []);

  // Load terminal renderer setting
  useEffect(() => {
    getSettings()
      .then((settings) => {
        setTerminalRenderer(normalizeRenderer(settings.terminal_renderer));
        setTerminalFontSize(normalizeFontSize(settings.terminal_font_size));
      })
      .catch(() => {});
  }, []);

  // React to runtime settings updates
  useEffect(() => {
    const handleSettingsUpdated = (event: Event) => {
      const detail = (event as CustomEvent<AppSettings>).detail;
      setTerminalRenderer(normalizeRenderer(detail?.terminal_renderer));
      setTerminalFontSize(normalizeFontSize(detail?.terminal_font_size));
    };

    window.addEventListener("devflow:settings-updated", handleSettingsUpdated);
    return () => {
      window.removeEventListener("devflow:settings-updated", handleSettingsUpdated);
    };
  }, []);

  const removeTabLocally = useCallback(
    (sessionId: string) => {
      setTabs((prev) => {
        if (!prev.some((tab) => tab.id === sessionId)) {
          return prev;
        }

        const next = prev.filter((tab) => tab.id !== sessionId);

        setActiveTabId((prevActive) => {
          if (prevActive === sessionId) {
            return next.length > 0 ? next[next.length - 1].id : null;
          }
          if (prevActive && next.some((tab) => tab.id === prevActive)) {
            return prevActive;
          }
          return next.length > 0 ? next[next.length - 1].id : null;
        });

        if (next.length === 0 && isVisible) {
          onToggle();
        }

        return next;
      });
    },
    [isVisible, onToggle]
  );

  // Auto-close tabs when shell exits (Ctrl+D / exit)
  useEffect(() => {
    const unlisten = listen<TerminalExitEvent>("terminal-exit", (event) => {
      const sessionId = event.payload.session_id;
      removeTabLocally(sessionId);
      closeTerminalInvoke(sessionId).catch(() => {
        // Session may already be cleaned up.
      });
    });

    return () => {
      unlisten.then((fn) => fn());
    };
  }, [removeTabLocally]);

  const handleCreateTab = useCallback(async () => {
    try {
      const session = await createTerminalInvoke();
      setTabs((prev) => [...prev, session]);
      setActiveTabId(session.id);
    } catch (e) {
      console.error("Failed to create terminal:", e);
    }
  }, []);

  const handleCreateTargetedTab = useCallback(
    async (projectPath?: string, branchName?: string, serviceName?: string) => {
      try {
        const session = await createTerminalInvoke(projectPath, branchName, serviceName);
        setTabs((prev) => [...prev, session]);
        setActiveTabId(session.id);
      } catch (e) {
        console.error("Failed to create terminal:", e);
      }
    },
    []
  );

  // Open requests from other parts of the app (project/service actions)
  useEffect(() => {
    if (!pendingTerminal) return;

    const open = async () => {
      await handleCreateTargetedTab(
        pendingTerminal.projectPath,
        pendingTerminal.branchName,
        pendingTerminal.serviceName
      );
      onPendingTerminalHandled();
    };

    open();
  }, [pendingTerminal, handleCreateTargetedTab, onPendingTerminalHandled]);

  const openLauncher = useCallback(async () => {
    setShowLauncher(true);
    setLauncherError(null);
    setLauncherProjectFilter("");
    setLauncherBranchFilter("");
    setLauncherServiceFilter("");
    setLauncherLoadingProjects(true);

    try {
      const projects = await listProjects();
      setLauncherProjects(projects);

      if (projects.length === 0) {
        setLauncherProjectPath("");
        setLauncherBranches([]);
        setLauncherBranchName("");
        setLauncherServices([]);
        setLauncherServiceName("");
        setLauncherError("No projects registered yet.");
        return;
      }

      const initialProjectPath =
        launcherProjectPath && projects.some((p) => p.path === launcherProjectPath)
          ? launcherProjectPath
          : projects[0].path;
      setLauncherProjectPath(initialProjectPath);
    } catch (e) {
      setLauncherError(`Failed to load projects: ${e}`);
    } finally {
      setLauncherLoadingProjects(false);
    }
  }, [launcherProjectPath]);

  useEffect(() => {
    if (!showLauncher || !launcherProjectPath) return;

    let cancelled = false;
    setLauncherLoadingTargets(true);
    setLauncherError(null);

    Promise.all([
      listBranches(launcherProjectPath),
      listServices(launcherProjectPath),
    ])
      .then(([branchResponse, services]) => {
        if (cancelled) return;

        const branches = branchResponse.branches;
        setLauncherBranches(branches);
        setLauncherServices(services);

        setLauncherBranchName((prev) => {
          if (prev && branches.some((b) => b.name === prev)) return prev;
          const defaultBranch = branches.find((b) => b.is_default);
          return defaultBranch?.name ?? branches[0]?.name ?? "";
        });

        setLauncherServiceName((prev) =>
          prev && services.some((s) => s.name === prev) ? prev : ""
        );
      })
      .catch((e) => {
        if (cancelled) return;
        setLauncherBranches([]);
        setLauncherBranchName("");
        setLauncherServices([]);
        setLauncherServiceName("");
        setLauncherError(`Failed to load branch targets: ${e}`);
      })
      .finally(() => {
        if (!cancelled) setLauncherLoadingTargets(false);
      });

    return () => {
      cancelled = true;
    };
  }, [showLauncher, launcherProjectPath]);

  useEffect(() => {
    if (!showLauncher) return;
    if (filteredProjects.length === 0) return;
    if (!filteredProjects.some((project) => project.path === launcherProjectPath)) {
      setLauncherProjectPath(filteredProjects[0].path);
    }
  }, [showLauncher, filteredProjects, launcherProjectPath]);

  useEffect(() => {
    if (!showLauncher) return;
    if (filteredBranches.length === 0) return;
    if (!filteredBranches.some((branch) => branch.name === launcherBranchName)) {
      setLauncherBranchName(filteredBranches[0].name);
    }
  }, [showLauncher, filteredBranches, launcherBranchName]);

  useEffect(() => {
    if (!showLauncher) return;
    if (launcherServiceName && !filteredServices.some((s) => s.name === launcherServiceName)) {
      setLauncherServiceName("");
    }
  }, [showLauncher, filteredServices, launcherServiceName]);

  const handleCreateFromLauncher = useCallback(async () => {
    if (!launcherProjectPath) {
      setLauncherError("Choose a project first.");
      return;
    }
    if (!launcherBranchName) {
      setLauncherError("Choose a branch first.");
      return;
    }

    await handleCreateTargetedTab(
      launcherProjectPath,
      launcherBranchName,
      launcherServiceName || undefined
    );
    setShowLauncher(false);
  }, [
    launcherProjectPath,
    launcherBranchName,
    launcherServiceName,
    handleCreateTargetedTab,
  ]);

  const handleCloseTab = useCallback(
    async (sessionId: string, e: React.MouseEvent) => {
      e.stopPropagation();
      removeTabLocally(sessionId);
      try {
        await closeTerminalInvoke(sessionId);
      } catch {
        // Session may already be closed
      }
    },
    [removeTabLocally]
  );

  const handleCloseAll = useCallback(async () => {
    for (const tab of tabs) {
      try {
        await closeTerminalInvoke(tab.id);
      } catch {
        // ignore
      }
    }
    setTabs([]);
    setActiveTabId(null);
    onToggle();
  }, [tabs, onToggle]);

  const handleResizeStart = useCallback(
    (e: React.MouseEvent) => {
      e.preventDefault();
      setIsResizing(true);
      const startY = e.clientY;
      const startHeight = panelHeight;

      const onMouseMove = (moveEvent: MouseEvent) => {
        const delta = startY - moveEvent.clientY;
        const newHeight = Math.max(MIN_PANEL_HEIGHT, startHeight + delta);
        setPanelHeight(newHeight);
      };

      const onMouseUp = () => {
        setIsResizing(false);
        document.removeEventListener("mousemove", onMouseMove);
        document.removeEventListener("mouseup", onMouseUp);

        const el = panelRef.current;
        if (el) {
          localStorage.setItem(STORAGE_KEY, el.offsetHeight.toString());
        }
      };

      document.addEventListener("mousemove", onMouseMove);
      document.addEventListener("mouseup", onMouseUp);
    },
    [panelHeight]
  );

  useEffect(() => {
    localStorage.setItem(STORAGE_KEY, panelHeight.toString());
  }, [panelHeight]);

  if (!isVisible) return null;

  return (
    <>
      <div
        ref={panelRef}
        className="terminal-panel"
        style={{ height: panelHeight }}
      >
        <div
          className="terminal-panel-resize"
          onMouseDown={handleResizeStart}
          style={{ cursor: isResizing ? "ns-resize" : undefined }}
        />

        <div className="terminal-panel-tabs">
          <div className="terminal-panel-tab-list">
            {tabs.map((tab) => (
              <div
                key={tab.id}
                className={`terminal-panel-tab ${
                  activeTabId === tab.id ? "active" : ""
                }`}
                onClick={() => setActiveTabId(tab.id)}
              >
                <span
                  className={`terminal-tab-dot ${
                    tab.status === "Running" ? "running" : "exited"
                  }`}
                />
                <span className="terminal-tab-label">{tab.label}</span>
                <button
                  className="terminal-tab-close"
                  onClick={(e) => handleCloseTab(tab.id, e)}
                  title="Close terminal"
                >
                  ×
                </button>
              </div>
            ))}
            <button
              className="terminal-panel-add"
              onClick={openLauncher}
              title="Open branch terminal"
            >
              +
            </button>
          </div>

          <div className="terminal-panel-actions" style={{ gap: 6 }}>
            <span
              className="badge"
              style={{
                fontSize: 10,
                color: "var(--text-muted)",
                borderColor: "var(--border)",
              }}
              title="restty renderer"
            >
              {terminalRenderer === "webgl2"
                ? "WebGL2"
                : terminalRenderer === "webgpu"
                  ? "WebGPU"
                  : "Auto"}
            </span>
            <span
              className="badge"
              style={{
                fontSize: 10,
                color: "var(--text-muted)",
                borderColor: "var(--border)",
              }}
              title="terminal font size"
            >
              {terminalFontSize}px
            </span>
            <button
              className="terminal-panel-action-btn"
              onClick={handleCreateTab}
              title="New shell terminal"
            >
              {">_"}
            </button>
            <button
              className="terminal-panel-action-btn"
              onClick={onToggle}
              title="Minimize"
            >
              _
            </button>
            <button
              className="terminal-panel-action-btn"
              onClick={handleCloseAll}
              title="Close all"
            >
              ×
            </button>
          </div>
        </div>

        <div className="terminal-panel-content">
          {tabs.length === 0 ? (
            <div className="terminal-panel-empty">
              <p>No terminals open</p>
              <div className="flex gap-2">
                <button className="btn btn-primary" onClick={openLauncher}>
                  Open Branch Terminal
                </button>
                <button className="btn" onClick={handleCreateTab}>
                  New Shell
                </button>
              </div>
            </div>
          ) : (
            tabs.map((tab) => (
              <Terminal
                key={`${tab.id}:${terminalRenderer}:${terminalFontSize}`}
                sessionId={tab.id}
                isActive={tab.id === activeTabId}
                renderer={terminalRenderer}
                fontSize={terminalFontSize}
              />
            ))
          )}
        </div>
      </div>

      <Modal
        open={showLauncher}
        onClose={() => setShowLauncher(false)}
        title="Open Branch Terminal"
        width={680}
      >
        {launcherLoadingProjects ? (
          <div className="terminal-launcher-loading">Loading projects...</div>
        ) : (
          <div className="terminal-launcher">
            <div className="terminal-launcher-intro">
              <div className="terminal-launcher-intro-title">
                Focus your terminal on one branch
              </div>
              <div className="terminal-launcher-intro-subtitle">
                Pick a project, choose a branch, and optionally attach a service
                environment.
              </div>
            </div>

            <div className="terminal-launcher-grid">
              <div className="terminal-launcher-field">
                <div className="terminal-launcher-field-head">
                  <span>Project</span>
                  <span className="terminal-launcher-count">
                    {filteredProjects.length}/{launcherProjects.length}
                  </span>
                </div>
                <input
                  type="text"
                  className="terminal-launcher-filter"
                  placeholder="Filter projects by name or path"
                  value={launcherProjectFilter}
                  onChange={(e) => setLauncherProjectFilter(e.target.value)}
                />
                <select
                  value={launcherProjectPath}
                  onChange={(e) => setLauncherProjectPath(e.target.value)}
                  className="terminal-launcher-select"
                  disabled={filteredProjects.length === 0}
                >
                  {filteredProjects.length === 0 ? (
                    <option value="">No project matches filter</option>
                  ) : (
                    filteredProjects.map((project) => (
                      <option key={project.path} value={project.path}>
                        {project.name}
                      </option>
                    ))
                  )}
                </select>
                <div className="terminal-launcher-meta mono">
                  {selectedProject?.path ?? "No project selected"}
                </div>
              </div>

              <div className="terminal-launcher-field">
                <div className="terminal-launcher-field-head">
                  <span>Branch</span>
                  <span className="terminal-launcher-count">
                    {filteredBranches.length}/{launcherBranches.length}
                  </span>
                </div>
                <input
                  type="text"
                  className="terminal-launcher-filter"
                  placeholder="Filter branches"
                  value={launcherBranchFilter}
                  onChange={(e) => setLauncherBranchFilter(e.target.value)}
                  disabled={launcherLoadingTargets}
                />
                <select
                  value={launcherBranchName}
                  onChange={(e) => setLauncherBranchName(e.target.value)}
                  className="terminal-launcher-select"
                  disabled={launcherLoadingTargets || filteredBranches.length === 0}
                >
                  {launcherLoadingTargets ? (
                    <option value="">Loading branches...</option>
                  ) : filteredBranches.length === 0 ? (
                    <option value="">No branch matches filter</option>
                  ) : (
                    filteredBranches.map((branch) => (
                      <option key={branch.name} value={branch.name}>
                        {branch.name}
                        {branch.is_default ? "  * default" : ""}
                      </option>
                    ))
                  )}
                </select>
              </div>

              <div className="terminal-launcher-field">
                <div className="terminal-launcher-field-head">
                  <span>Service Environment (optional)</span>
                  <span className="terminal-launcher-count">
                    {filteredServices.length}/{launcherServices.length}
                  </span>
                </div>
                <input
                  type="text"
                  className="terminal-launcher-filter"
                  placeholder="Filter services"
                  value={launcherServiceFilter}
                  onChange={(e) => setLauncherServiceFilter(e.target.value)}
                  disabled={launcherLoadingTargets}
                />
                <select
                  value={launcherServiceName}
                  onChange={(e) => setLauncherServiceName(e.target.value)}
                  className="terminal-launcher-select"
                  disabled={launcherLoadingTargets}
                >
                  <option value="">Default shell environment</option>
                  {filteredServices.map((service) => (
                    <option key={service.name} value={service.name}>
                      {service.name} ({service.service_type}/{service.provider_type})
                    </option>
                  ))}
                </select>
              </div>
            </div>

            <div className="terminal-launcher-preview">
              <div className="terminal-launcher-preview-row">
                <span>Tab label</span>
                <span className="mono">{previewLabel}</span>
              </div>
              <div className="terminal-launcher-preview-row">
                <span>Branch</span>
                <span className="mono">{launcherBranchName || "-"}</span>
              </div>
              <div className="terminal-launcher-preview-row">
                <span>Service scope</span>
                <span className="mono">{launcherServiceName || "default"}</span>
              </div>
            </div>

            {launcherError && (
              <div className="terminal-launcher-error">{launcherError}</div>
            )}

            <div className="terminal-launcher-actions">
              <button className="btn" onClick={() => setShowLauncher(false)}>
                Cancel
              </button>
              <button
                className="btn btn-primary"
                onClick={handleCreateFromLauncher}
                disabled={
                  launcherLoadingTargets || !launcherProjectPath || !launcherBranchName
                }
              >
                Open Terminal
              </button>
            </div>
          </div>
        )}
      </Modal>
    </>
  );
}

export default TerminalPanel;
