import { useState, useEffect, useCallback } from "react";
import { toast } from "../../utils/notify";
import { reportWorkspaceResult } from "../../utils/results";
import { useParams, Link, useNavigate } from "react-router-dom";
import { listen } from "@tauri-apps/api/event";
import { open as openExternal } from "@tauri-apps/plugin-shell";
import {
  getProjectDetail,
  listWorkspaces,
  listServices,
  addService,
  createWorkspace,
  switchWorkspace,
  deleteWorkspace,
  listProcesses,
  startProcesses,
  stopProcesses,
  restartProcesses,
  getProcessLogs,
  startService,
  stopService,
  resetService,
  getServiceLogs,
  getConnectionInfo,
  listServiceWorkspaces,
  deleteServiceWorkspace,
  destroyService,
  removeProject,
  destroyProject,
  addOrInitProject,
  listContainers,
  getProxyStatus,
  runDoctor,
  getPitchforkBridgeInfo,
} from "../../utils/invoke";
import type {
  ProjectDetail as ProjectDetailType,
  WorkspaceEntry,
  ServiceEntry,
  ServiceWorkspaceInfo,
  ConnectionInfo,
  AddServiceRequest,
  ContainerEntry,
  ProxyStatus,
  WorkspaceCreationMode,
  ProcessStatus,
  ProcessResult,
  PitchforkBridgeInfo,
} from "../../types";
import Modal from "../../components/Modal";
import ConfirmDialog from "../../components/ConfirmDialog";
import AddServiceModal from "../../components/AddServiceModal";
import { useTerminal } from "../../context/TerminalContext";
import { recordProjectAccess } from "../../utils/recentProjects";
import ProjectSkillsTab from "./ProjectSkillsTab";
import { IconTerminal } from "../../components/icons";

interface WorkspaceSwitchedEvent {
  project_path: string;
  workspace_name: string;
}

function formatRelativeTime(iso?: string | null): string | null {
  if (!iso) return null;
  const ts = Date.parse(iso);
  if (Number.isNaN(ts)) return null;
  const seconds = Math.max(0, Math.floor((Date.now() - ts) / 1000));
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  return `${Math.floor(hours / 24)}d ago`;
}

function ProjectDetail() {
  const { "*": splat } = useParams();
  const projectPath = splat ? decodeURIComponent(splat) : "";
  const navigate = useNavigate();
  const { openTerminal } = useTerminal();

  const [detail, setDetail] = useState<ProjectDetailType | null>(null);
  const [workspaces, setWorkspaces] = useState<WorkspaceEntry[]>([]);
  const [services, setServices] = useState<ServiceEntry[]>([]);
  const [processStatuses, setProcessStatuses] = useState<ProcessStatus[]>([]);
  const [processWorkspace, setProcessWorkspace] = useState<string>("");
  const [selectedProcesses, setSelectedProcesses] = useState<Set<string>>(new Set());
  const [pitchforkBridge, setPitchforkBridge] = useState<PitchforkBridgeInfo | null>(null);
  const [currentWorkspace, setCurrentWorkspace] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [setupIssueCount, setSetupIssueCount] = useState(0);

  // Tab state — sync with URL hash
  const [activeTab, setActiveTab] = useState<"overview" | "skills">(
    window.location.hash === "#skills" ? "skills" : "overview"
  );

  // Service workspace tracking: { "service-name": ServiceWorkspaceInfo[] }
  const [serviceWorkspaces, setServiceWorkspaces] = useState<
    Record<string, ServiceWorkspaceInfo[]>
  >({});
  const [expandedServices, setExpandedServices] = useState<Set<string>>(
    new Set()
  );

  // Modal state
  const [showCreateWorkspace, setShowCreateWorkspace] = useState(false);
  const [newWorkspaceName, setNewWorkspaceName] = useState("");
  const [fromWorkspace, setFromWorkspace] = useState("");
  const [creationMode, setCreationMode] = useState<WorkspaceCreationMode>("branch");
  const [copyFiles, setCopyFiles] = useState<string[]>([]);
  const [copyIgnored, setCopyIgnored] = useState(false);
  const [sandboxed, setSandboxed] = useState(false);
  const [deletingWorkspace, setDeletingWorkspace] = useState<string | null>(null);
  const [connInfoWorkspace, setConnInfoWorkspace] = useState<string | null>(null);
  const [connInfo, setConnInfo] = useState<Record<string, ConnectionInfo>>({});
  const [connInfoContainers, setConnInfoContainers] = useState<Record<string, ContainerEntry>>({});
  const [logService, setLogService] = useState<{
    name: string;
    workspace: string;
  } | null>(null);
  const [logProcess, setLogProcess] = useState<{
    name: string;
    workspace: string;
  } | null>(null);
  const [logContent, setLogContent] = useState("");
  const [resetTarget, setResetTarget] = useState<{
    name: string;
    workspace: string;
  } | null>(null);
  const [deleteServiceWsTarget, setDeleteServiceWsTarget] = useState<{
    name: string;
    workspace: string;
  } | null>(null);
  const [destroyServiceTarget, setDestroyServiceTarget] = useState<string | null>(null);

  // Add Service modal state
  const [showAddService, setShowAddService] = useState(false);

  // Danger zone state
  const [showRemoveConfirm, setShowRemoveConfirm] = useState(false);
  const [showDestroyConfirm, setShowDestroyConfirm] = useState(false);
  const [destroyConfirmText, setDestroyConfirmText] = useState("");

  // Proxy state
  const [proxyStatus, setProxyStatus] = useState<ProxyStatus | null>(null);
  const [containers, setContainers] = useState<ContainerEntry[]>([]);

  // Loading state
  const [actionLoading, setActionLoading] = useState<string | null>(null);
  const isCreatingWorkspace = actionLoading === "create";
  const projectDefaultCreationMode: WorkspaceCreationMode = detail?.worktree_enabled
    ? "worktree"
    : "branch";

  const fetchProcesses = useCallback(
    async (workspaceName?: string) => {
      if (!projectPath || !workspaceName) {
        setProcessStatuses([]);
        return;
      }
      try {
        setProcessStatuses(await listProcesses(projectPath, workspaceName));
      } catch {
        setProcessStatuses([]);
      }
    },
    [projectPath]
  );

  const fetchServiceWorkspaces = useCallback(
    async (svcs: ServiceEntry[]) => {
      if (svcs.length === 0) {
        setServiceWorkspaces({});
        return;
      }
      const result: Record<string, ServiceWorkspaceInfo[]> = {};
      await Promise.all(
        svcs.map(async (s) => {
          try {
            result[s.name] = await listServiceWorkspaces(projectPath, s.name);
          } catch {
            result[s.name] = [];
          }
        })
      );
      setServiceWorkspaces(result);
    },
    [projectPath]
  );

  const reload = useCallback(async () => {
    if (!projectPath) return;
    try {
      const d = await getProjectDetail(projectPath);
      setDetail(d);
      setError(null);
    } catch (e) {
      setError(`Failed to load project: ${e}`);
      return;
    }
    let loadedServices: ServiceEntry[] = [];
    await Promise.all([
      listWorkspaces(projectPath)
        .then((b) => {
          setWorkspaces(b.workspaces);
          setCurrentWorkspace(b.current);
          const preferredWorkspace =
            processWorkspace ||
            b.current ||
            b.workspaces.find((w) => w.is_default)?.name ||
            b.workspaces[0]?.name ||
            "";
          if (preferredWorkspace && !processWorkspace) {
            setProcessWorkspace(preferredWorkspace);
          }
          if (preferredWorkspace) {
            void fetchProcesses(preferredWorkspace);
          }
        })
        .catch(() => {
          setWorkspaces([]);
          setCurrentWorkspace(null);
        }),
      listServices(projectPath)
        .then((svcs) => {
          setServices(svcs);
          loadedServices = svcs;
        })
        .catch(() => setServices([])),
      getProxyStatus()
        .then(setProxyStatus)
        .catch(() => setProxyStatus(null)),
      getPitchforkBridgeInfo(projectPath)
        .then(setPitchforkBridge)
        .catch(() => setPitchforkBridge(null)),
      listContainers()
        .then(setContainers)
        .catch(() => setContainers([])),
    ]);
    // Fetch service workspaces after we know the services
    await fetchServiceWorkspaces(loadedServices);
  }, [projectPath, fetchServiceWorkspaces, fetchProcesses, processWorkspace]);

  useEffect(() => {
    reload();
    if (projectPath) {
      recordProjectAccess(projectPath);
    }
  }, [reload, projectPath]);

  useEffect(() => {
    setSelectedProcesses(new Set());
    if (processWorkspace) {
      void fetchProcesses(processWorkspace);
    }
  }, [fetchProcesses, processWorkspace]);

  useEffect(() => {
    if (!processWorkspace || activeTab !== "overview") return;
    const timer = window.setInterval(() => {
      void fetchProcesses(processWorkspace);
    }, 5000);
    return () => window.clearInterval(timer);
  }, [activeTab, fetchProcesses, processWorkspace]);

  useEffect(() => {
    const unlisten = listen<WorkspaceSwitchedEvent>(
      "workspace-switched",
      (event) => {
        if (event.payload.project_path === projectPath) {
          setCurrentWorkspace(event.payload.workspace_name);
          void reload();
        }
      }
    );

    return () => {
      void unlisten.then((fn) => fn());
    };
  }, [projectPath, reload]);

  // Background doctor check for badge
  useEffect(() => {
    if (!detail?.has_config || !projectPath) return;
    runDoctor(projectPath)
      .then((report) => {
        const failCount = report.general.filter((c) => !c.available).length;
        setSetupIssueCount(failCount);
      })
      .catch(() => setSetupIssueCount(0));
  }, [detail?.has_config, projectPath]);

  // Sync tab state with URL hash
  useEffect(() => {
    const onHashChange = () => {
      setActiveTab(window.location.hash === "#skills" ? "skills" : "overview");
    };
    window.addEventListener("hashchange", onHashChange);
    return () => window.removeEventListener("hashchange", onHashChange);
  }, []);

  useEffect(() => {
    const newHash = activeTab === "skills" ? "#skills" : "";
    if (window.location.hash !== newHash) {
      window.history.replaceState(null, "", newHash || window.location.pathname + window.location.search);
    }
  }, [activeTab]);

  const handleCreateWorkspace = async () => {
    if (!newWorkspaceName.trim() || isCreatingWorkspace) return;
    setActionLoading("create");
    try {
      const result = await createWorkspace(
        projectPath,
        newWorkspaceName.trim(),
        fromWorkspace || undefined,
        creationMode,
        creationMode === "worktree" ? copyFiles : undefined,
        creationMode === "worktree" ? copyIgnored : undefined,
        sandboxed || undefined
      );
      reportWorkspaceResult(`Workspace "${newWorkspaceName.trim()}"`, result);
      setShowCreateWorkspace(false);
      setNewWorkspaceName("");
      setFromWorkspace("");
      setCreationMode(projectDefaultCreationMode);
      setCopyFiles(detail?.worktree_copy_files ?? []);
      setCopyIgnored(detail?.worktree_copy_ignored ?? false);
      setSandboxed(false);
      await reload();
    } catch (e) {
      toast.error(`${e}`);
    } finally {
      setActionLoading(null);
    }
  };

  const handleSwitchWorkspace = async (workspaceName: string) => {
    setActionLoading(`switch:${workspaceName}`);
    try {
      const result = await switchWorkspace(projectPath, workspaceName);
      reportWorkspaceResult(`Switched to "${workspaceName}"`, result);
      await reload();
    } catch (e) {
      toast.error(`${e}`);
    } finally {
      setActionLoading(null);
    }
  };

  const handleDeleteWorkspace = async () => {
    if (!deletingWorkspace) return;
    setActionLoading("delete");
    const target = deletingWorkspace;
    try {
      const result = await deleteWorkspace(projectPath, target);
      reportWorkspaceResult(`Workspace "${target}" deleted`, result);
      setDeletingWorkspace(null);
      await reload();
    } catch (e) {
      toast.error(`${e}`);
    } finally {
      setActionLoading(null);
    }
  };

  const handleConnectionInfo = async (
    serviceName: string,
    workspaceName: string
  ) => {
    setConnInfoWorkspace(workspaceName);
    try {
      const infos: Record<string, ConnectionInfo> = {};
      try {
        const info = await getConnectionInfo(
          projectPath,
          workspaceName,
          serviceName
        );
        infos[serviceName] = info as unknown as ConnectionInfo;
      } catch {
        // service may not have connection info
      }
      setConnInfo(infos);

      // Look up matching proxy containers for this service/workspace
      if (proxyStatus?.running && detail) {
        try {
          const allContainers = await listContainers();
          const matched: Record<string, ContainerEntry> = {};
          // Helper: sanitize name the same way Rust does (lowercase, non-alnum -> dash)
          const sanitize = (s: string) =>
            s.toLowerCase().replace(/[^a-z0-9]/g, "-").replace(/-{2,}/g, "-");
          for (const c of allContainers) {
            if (c.project === detail.name && c.service === serviceName) {
              // Prefer exact workspace match, but also accept if workspace is null (legacy containers)
              if (c.workspace === workspaceName || !c.workspace) {
                matched[serviceName] = c;
              }
            } else {
              // Fallback: match by container name pattern devflow-{sanitized_project}-{service}-{workspace}
              const expectedName = `devflow-${sanitize(detail.name)}-${sanitize(serviceName)}-${sanitize(workspaceName)}`;
              if (c.container_name === expectedName || c.container_name === `/${expectedName}`) {
                matched[serviceName] = c;
              }
            }
          }
          setConnInfoContainers(matched);
        } catch {
          setConnInfoContainers({});
        }
      } else {
        setConnInfoContainers({});
      }
    } catch (e) {
      console.error(e);
    }
  };

  const handleStartService = async (svcName: string, workspaceName: string) => {
    setActionLoading(`start:${svcName}:${workspaceName}`);
    try {
      await startService(projectPath, svcName, workspaceName);
      // Refresh workspaces for this service
      try {
        const workspaces = await listServiceWorkspaces(projectPath, svcName);
        setServiceWorkspaces((prev) => ({ ...prev, [svcName]: workspaces }));
      } catch {
        // ignore
      }
    } catch (e) {
      toast.error(`${e}`);
    } finally {
      setActionLoading(null);
    }
  };

  const handleStopService = async (svcName: string, workspaceName: string) => {
    setActionLoading(`stop:${svcName}:${workspaceName}`);
    try {
      await stopService(projectPath, svcName, workspaceName);
      // Refresh workspaces for this service
      try {
        const workspaces = await listServiceWorkspaces(projectPath, svcName);
        setServiceWorkspaces((prev) => ({ ...prev, [svcName]: workspaces }));
      } catch {
        // ignore
      }
    } catch (e) {
      toast.error(`${e}`);
    } finally {
      setActionLoading(null);
    }
  };

  const handleResetService = async () => {
    if (!resetTarget) return;
    setActionLoading("reset");
    try {
      await resetService(projectPath, resetTarget.name, resetTarget.workspace);
      // Refresh workspaces for this service
      try {
        const workspaces = await listServiceWorkspaces(
          projectPath,
          resetTarget.name
        );
        setServiceWorkspaces((prev) => ({
          ...prev,
          [resetTarget.name]: workspaces,
        }));
      } catch {
        // ignore
      }
      setResetTarget(null);
    } catch (e) {
      toast.error(`${e}`);
    } finally {
      setActionLoading(null);
    }
  };

  const handleViewLogs = async (svcName: string, workspaceName: string) => {
    setLogProcess(null);
    setLogService({ name: svcName, workspace: workspaceName });
    setLogContent("Loading...");
    try {
      const logs = await getServiceLogs(projectPath, svcName, workspaceName);
      setLogContent(logs || "(no log output)");
    } catch (e) {
      setLogContent(`Error: ${e}`);
    }
  };

  const summarizeProcessResults = (action: string, results: ProcessResult[]) => {
    const requiredFailures = results.filter((r) => !r.success && r.required);
    const optionalFailures = results.filter((r) => !r.success && !r.required);
    if (requiredFailures.length > 0 || optionalFailures.length > 0) {
      toast.warning(
        [...requiredFailures, ...optionalFailures]
          .map((r) => `${r.required ? "" : "optional "}${r.process}: ${r.message}`)
          .join("\n"),
        { title: `${action} completed with issues`, duration: 0 }
      );
    } else {
      toast.success(`${action} succeeded`);
    }
  };

  const processNames = (target?: string | string[]) =>
    Array.isArray(target) ? target : target ? [target] : [];

  const selectedProcessNames = () =>
    processStatuses
      .filter((p) => p.workspace === processWorkspace && selectedProcesses.has(p.process))
      .map((p) => p.process);

  const handleStartProcess = async (target?: string | string[], force = false) => {
    if (!processWorkspace) return;
    const names = processNames(target);
    const label = names.length === 1 ? names[0] : names.length > 1 ? `${names.length} selected` : "all";
    setActionLoading(`process-start:${label}`);
    try {
      const response = await startProcesses(projectPath, processWorkspace, names, force);
      summarizeProcessResults(names.length === 1 ? `Started ${names[0]}` : "Started processes", response.results);
      await fetchProcesses(processWorkspace);
    } catch (e) {
      toast.error(`${e}`);
    } finally {
      setActionLoading(null);
    }
  };

  const handleStopProcess = async (target?: string | string[]) => {
    if (!processWorkspace) return;
    const names = processNames(target);
    const label = names.length === 1 ? names[0] : names.length > 1 ? `${names.length} selected` : "all";
    setActionLoading(`process-stop:${label}`);
    try {
      const response = await stopProcesses(projectPath, processWorkspace, names);
      summarizeProcessResults(names.length === 1 ? `Stopped ${names[0]}` : "Stopped processes", response.results);
      await fetchProcesses(processWorkspace);
    } catch (e) {
      toast.error(`${e}`);
    } finally {
      setActionLoading(null);
    }
  };

  const handleRestartProcess = async (target?: string | string[]) => {
    if (!processWorkspace) return;
    const names = processNames(target);
    const label = names.length === 1 ? names[0] : names.length > 1 ? `${names.length} selected` : "all";
    setActionLoading(`process-restart:${label}`);
    try {
      const response = await restartProcesses(projectPath, processWorkspace, names);
      summarizeProcessResults(names.length === 1 ? `Restarted ${names[0]}` : "Restarted processes", response.results);
      await fetchProcesses(processWorkspace);
    } catch (e) {
      toast.error(`${e}`);
    } finally {
      setActionLoading(null);
    }
  };

  const handleOpenPitchforkWebUi = async () => {
    const url = pitchforkBridge?.web_ui_url || "http://127.0.0.1:3120";
    try {
      await openExternal(url);
      if (!pitchforkBridge?.web_ui_reachable) {
        toast.warning("Opened the configured Pitchfork Web UI URL, but it does not appear reachable yet.", {
          title: "Pitchfork Web UI",
        });
      }
    } catch (e) {
      toast.error(`${e}`, { title: "Failed to open Pitchfork Web UI" });
    }
  };

  const handleLaunchPitchforkTui = () => {
    if (!pitchforkBridge?.cli_available) {
      toast.warning("Install the external pitchfork CLI to launch its TUI.", { title: "Pitchfork TUI" });
      return;
    }
    openTerminal({ projectPath, workspaceName: processWorkspace || undefined, initialCommand: "pitchfork tui" });
  };

  const handleViewProcessLogs = async (name: string, workspaceName: string) => {
    setLogService(null);
    setLogProcess({ name, workspace: workspaceName });
    setLogContent("Loading...");
    try {
      const logs = await getProcessLogs(projectPath, workspaceName, name, 300);
      setLogContent(logs || "(no log output)");
    } catch (e) {
      setLogContent(`Error: ${e}`);
    }
  };

  const handleDeleteServiceWorkspace = async () => {
    if (!deleteServiceWsTarget) return;
    setActionLoading("delete-svc-ws");
    try {
      await deleteServiceWorkspace(
        projectPath,
        deleteServiceWsTarget.name,
        deleteServiceWsTarget.workspace
      );
      // Refresh workspaces for this service
      try {
        const ws = await listServiceWorkspaces(
          projectPath,
          deleteServiceWsTarget.name
        );
        setServiceWorkspaces((prev) => ({
          ...prev,
          [deleteServiceWsTarget.name]: ws,
        }));
      } catch {
        // ignore
      }
      setDeleteServiceWsTarget(null);
    } catch (e) {
      toast.error(`${e}`);
    } finally {
      setActionLoading(null);
    }
  };

  const handleDestroyService = async () => {
    if (!destroyServiceTarget) return;
    setActionLoading("destroy-service");
    try {
      await destroyService(projectPath, destroyServiceTarget);
      setDestroyServiceTarget(null);
      await reload();
    } catch (e) {
      toast.error(`${e}`);
    } finally {
      setActionLoading(null);
    }
  };

  const toggleServiceExpanded = (name: string) => {
    setExpandedServices((prev) => {
      const next = new Set(prev);
      if (next.has(name)) {
        next.delete(name);
      } else {
        next.add(name);
      }
      return next;
    });
  };

  const handleRemoveProject = async () => {
    setActionLoading("remove");
    try {
      await removeProject(projectPath);
      window.dispatchEvent(new CustomEvent("devflow:projects-changed"));
      navigate("/projects");
    } catch (e) {
      toast.error(`${e}`);
    } finally {
      setActionLoading(null);
      setShowRemoveConfirm(false);
    }
  };

  const handleDestroyProject = async () => {
    setActionLoading("destroy");
    try {
      await destroyProject(projectPath);
      await removeProject(projectPath);
      window.dispatchEvent(new CustomEvent("devflow:projects-changed"));
      navigate("/projects");
    } catch (e) {
      toast.error(`Destroy failed: ${e}`);
    } finally {
      setActionLoading(null);
      setShowDestroyConfirm(false);
      setDestroyConfirmText("");
    }
  };

  const handleAddService = async (request: AddServiceRequest) => {
    await addService(projectPath, request);
    await reload();
  };

  if (error) {
    return (
      <div className="card" style={{ marginTop: 24 }}>
        <p style={{ color: "var(--danger)" }}>{error}</p>
        <p className="mono" style={{ color: "var(--text-muted)", fontSize: 12, marginTop: 8 }}>
          Path: {projectPath || "(empty)"}
        </p>
      </div>
    );
  }

  if (!detail) {
    return (
      <div>
        <div className="skeleton" style={{ height: 28, width: 220, marginBottom: 20 }} />
        <div className="stat-grid">
          <div className="skeleton" style={{ height: 70 }} />
          <div className="skeleton" style={{ height: 70 }} />
          <div className="skeleton" style={{ height: 70 }} />
        </div>
        <div className="skeleton" style={{ height: 200 }} />
      </div>
    );
  }

  const currentWorkspaceEntry = workspaces.find((w) => w.is_current);
  const showActiveWorkspaceBadge = Boolean(
    currentWorkspace && currentWorkspaceEntry && !currentWorkspaceEntry.worktree_path
  );

  // Map a service-workspace state string to a status-dot class.
  const stateDotClass = (state: string | null): string => {
    switch ((state || "").toLowerCase()) {
      case "running":
        return "running";
      case "provisioning":
        return "provisioning";
      case "failed":
      case "error":
        return "error";
      default:
        return "stopped";
    }
  };

  // Join per-workspace service status so the workspaces table shows, at a
  // glance, which services are provisioned/running for each workspace —
  // instead of forcing the user to expand the per-service accordions below.
  const normalizeWs = (s: string) =>
    s.toLowerCase().replace(/[^a-z0-9]/g, "-").replace(/-{2,}/g, "-");
  const serviceChipsFor = (workspaceName: string) => {
    const wsKey = normalizeWs(workspaceName);
    return services.map((svc) => {
      const infos = serviceWorkspaces[svc.name] || [];
      const match = infos.find(
        (i) => i.name === workspaceName || normalizeWs(i.name) === wsKey
      );
      return { service: svc.name, state: match?.state ?? null, provisioned: !!match };
    });
  };

  const selectedProcessNamesForWorkspace = selectedProcessNames();
  const allProcessNamesForWorkspace = processStatuses
    .filter((p) => p.workspace === processWorkspace)
    .map((p) => p.process);
  const allProcessesSelected =
    allProcessNamesForWorkspace.length > 0 &&
    allProcessNamesForWorkspace.every((name) => selectedProcesses.has(name));
  const hasPitchforkProcesses =
    pitchforkBridge?.enabled || processStatuses.some((p) => p.runtime === "pitchfork");

  return (
    <div>
      {/* Header */}
      <div className="flex items-center justify-between mb-4">
        <div>
          <h1 className="page-title" style={{ marginBottom: 4 }}>
            {detail.name}
          </h1>
          <div className="flex items-center gap-2">
            <span
              className="mono"
              style={{ color: "var(--text-secondary)", fontSize: 12 }}
            >
              {detail.path}
            </span>
            {showActiveWorkspaceBadge && (
              <span className="badge" style={{ opacity: 0.7 }}>
                active: {currentWorkspace}
              </span>
            )}
            {!detail.has_config && (
              <span className="badge badge-warning">no config</span>
            )}
            {detail.vcs_type && (
              <span className="badge">vcs: {detail.vcs_type}</span>
            )}
            {detail.has_config && (
              <span
                className="badge badge-info"
                title="Default creation mode. You can still choose branch or worktree when creating a workspace."
              >
                default: {detail.worktree_enabled ? "worktree" : "branch"}
              </span>
            )}
          </div>
        </div>
        <div className="flex gap-2">
          <Link
            to={`/hooks/${encodeURIComponent(projectPath)}`}
            className="btn"
            style={{ display: "inline-flex", alignItems: "center", gap: 6 }}
          >
            Hooks
            {detail.hook_count > 0 && (
              <span
                className="badge badge-info"
                style={{ fontSize: 10, minWidth: 18, textAlign: "center", padding: "1px 5px" }}
              >
                {detail.hook_count}
              </span>
            )}
          </Link>
          <Link
            to={`/config/${encodeURIComponent(projectPath)}`}
            className="btn"
          >
            Config
          </Link>
          {detail.has_config && (
            <>
              <Link
                to={`/onboard/${encodeURIComponent(projectPath)}`}
                className="btn"
              >
                Setup wizard
              </Link>
              <Link
                to={`/setup/${encodeURIComponent(projectPath)}`}
                className="btn"
                style={{ display: "inline-flex", alignItems: "center", gap: 6 }}
              >
                Doctor
                {setupIssueCount > 0 && (
                  <span
                    className="badge badge-warning"
                    style={{ fontSize: 10, minWidth: 18, textAlign: "center", padding: "1px 5px" }}
                  >
                    {setupIssueCount}
                  </span>
                )}
              </Link>
            </>
          )}
        </div>
      </div>

      {/* Config missing banner */}
      {!detail.has_config && (
        <div
          className="card"
          style={{
            borderColor: "var(--warning)",
            borderWidth: 1,
            borderStyle: "solid",
            marginBottom: 16,
          }}
        >
          <div style={{ display: "flex", alignItems: "flex-start", gap: 16, justifyContent: "space-between" }}>
            <div>
              <div style={{ fontWeight: 600, marginBottom: 4, color: "var(--warning)" }}>
                Configuration file missing
              </div>
              <p style={{ color: "var(--text-secondary)", fontSize: 13, margin: 0 }}>
                This project is registered in devflow but its <code>.devflow.yml</code> configuration file is missing.
                Without it, workspaces and services cannot be managed. You can re-initialize the project or remove it from devflow.
              </p>
            </div>
            <div className="flex gap-2" style={{ flexShrink: 0 }}>
              <button
                className="btn btn-primary"
                onClick={async () => {
                  setActionLoading("reinit");
                  try {
                    await addOrInitProject(projectPath, detail.name);
                    await reload();
                  } catch (e) {
                    toast.error(`Failed to initialize: ${e}`);
                  } finally {
                    setActionLoading(null);
                  }
                }}
                disabled={actionLoading === "reinit"}
              >
                {actionLoading === "reinit" ? "Initializing..." : "Initialize Project"}
              </button>
              <button
                className="btn btn-danger"
                onClick={async () => {
                  setActionLoading("remove");
                  try {
                    await removeProject(projectPath);
                    window.dispatchEvent(new CustomEvent("devflow:projects-changed"));
                    navigate("/projects");
                  } catch (e) {
                    toast.error(`Failed to remove: ${e}`);
                  } finally {
                    setActionLoading(null);
                  }
                }}
                disabled={actionLoading === "remove"}
              >
                {actionLoading === "remove" ? "Removing..." : "Remove"}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Tab bar */}
      <div style={{
        display: "flex",
        borderBottom: "1px solid var(--border)",
        marginBottom: 16,
        gap: 0,
      }}>
        <button
          onClick={() => setActiveTab("overview")}
          style={{
            padding: "8px 16px",
            fontSize: 14,
            fontWeight: activeTab === "overview" ? 600 : 400,
            color: activeTab === "overview" ? "var(--text-primary)" : "var(--text-muted)",
            background: "none",
            border: "none",
            borderBottom: activeTab === "overview" ? "2px solid var(--accent)" : "2px solid transparent",
            cursor: "pointer",
            marginBottom: -1,
          }}
        >
          Overview
        </button>
        <button
          onClick={() => setActiveTab("skills")}
          style={{
            padding: "8px 16px",
            fontSize: 14,
            fontWeight: activeTab === "skills" ? 600 : 400,
            color: activeTab === "skills" ? "var(--text-primary)" : "var(--text-muted)",
            background: "none",
            border: "none",
            borderBottom: activeTab === "skills" ? "2px solid var(--accent)" : "2px solid transparent",
            cursor: "pointer",
            marginBottom: -1,
          }}
        >
          Skills
        </button>
      </div>

      {/* Skills tab */}
      {activeTab === "skills" && <ProjectSkillsTab projectPath={projectPath} />}

      {/* Overview tab */}
      {activeTab === "overview" && !detail.has_config && (
        <div className="card" style={{ textAlign: "center", padding: 32 }}>
          <p style={{ color: "var(--text-muted)", fontSize: 14 }}>
            Initialize the project configuration to manage workspaces and services.
          </p>
        </div>
      )}
      {activeTab === "overview" && detail.has_config && (<>

      {/* Workspaces card */}
      <div className="card">
        <div className="flex items-center justify-between mb-4">
          <span className="card-title" style={{ marginBottom: 0 }}>
            Workspaces
          </span>
          <button
            className="btn btn-primary"
            onClick={() => {
              const defaultWorkspace = workspaces.find((b) => b.is_default);
              setFromWorkspace(defaultWorkspace?.name ?? "");
              setNewWorkspaceName("");
              setCreationMode(projectDefaultCreationMode);
              setCopyFiles(detail.worktree_copy_files);
              setCopyIgnored(detail.worktree_copy_ignored);
              setShowCreateWorkspace(true);
            }}
            style={{ padding: "4px 12px", fontSize: 13 }}
          >
            Create Workspace
          </button>
        </div>
        {workspaces.length === 0 ? (
          <p style={{ color: "var(--text-secondary)" }}>No workspaces found.</p>
        ) : (
          <table className="table" style={{ tableLayout: "fixed", width: "100%" }}>
            <thead>
              <tr>
                <th style={{ width: "22%" }}>Workspace</th>
                <th style={{ width: "16%" }}>Tags</th>
                <th style={{ width: "22%" }}>Services</th>
                <th style={{ width: "11%" }}>Parent</th>
                <th style={{ width: "10%" }}>Created</th>
                <th style={{ textAlign: "right", width: "19%" }}>Actions</th>
              </tr>
            </thead>
            <tbody>
              {workspaces.map((b) => (
                <tr key={b.name}>
                  <td style={{ overflow: "hidden" }} title={b.name}>
                    <div>
                      <span style={{ fontWeight: b.is_default ? 600 : 400 }}>
                        {b.name}
                      </span>
                    </div>
                    {b.worktree_path && (
                      <span
                        className="mono"
                        style={{
                          color: "var(--text-muted)",
                          fontSize: 11,
                          display: "block",
                          overflow: "hidden",
                          textOverflow: "ellipsis",
                          whiteSpace: "nowrap",
                        }}
                        title={b.worktree_path}
                      >
                        {b.worktree_path}
                      </span>
                    )}
                  </td>
                  <td>
                    <div className="flex gap-1" style={{ flexWrap: "wrap" }}>
                      {b.worktree_path ? (
                        <span className="badge" style={{ fontSize: 10 }}>git worktree</span>
                      ) : (
                        <span className="badge" style={{ fontSize: 10 }}>git branch</span>
                      )}
                      {b.is_current && !b.worktree_path && (
                        <span className="badge badge-success" style={{ fontSize: 10 }}>active</span>
                      )}
                      {b.is_default && (
                        <span className="badge badge-info" style={{ fontSize: 10 }}>default</span>
                      )}
                      {b.execution_status && (
                        <span
                          className={`badge${b.execution_status === "running" ? " badge-success" : ""}`}
                          style={{ fontSize: 10 }}
                          title={b.executed_command ? `Command: ${b.executed_command}` : undefined}
                        >
                          {b.execution_status}
                        </span>
                      )}
                      {b.sandboxed && (
                        <span className="badge" style={{ fontSize: 10 }} title="Sandboxed: restricted filesystem and command access">sandboxed</span>
                      )}
                    </div>
                  </td>
                  <td>
                    {services.length === 0 ? (
                      <span style={{ color: "var(--text-muted)", fontSize: 12 }}>
                        —
                      </span>
                    ) : (
                      <div className="svc-chips">
                        {serviceChipsFor(b.name).map((chip) => (
                          <span
                            key={chip.service}
                            className={`svc-chip${chip.state === "running" ? " running" : ""}`}
                            title={
                              chip.provisioned
                                ? `${chip.service}: ${chip.state ?? "unknown"}`
                                : `${chip.service}: not provisioned for this workspace`
                            }
                          >
                            <span
                              className={`status-dot ${
                                chip.provisioned ? stateDotClass(chip.state) : "stopped"
                              }`}
                            />
                            {chip.service}
                          </span>
                        ))}
                      </div>
                    )}
                  </td>
                  <td style={{ color: "var(--text-muted)", fontSize: 13 }}>
                    {b.parent || "-"}
                  </td>
                  <td style={{ color: "var(--text-muted)", fontSize: 12 }}>
                    {b.created_at || "-"}
                  </td>
                  <td style={{ textAlign: "right" }}>
                    <div className="flex gap-2" style={{ justifyContent: "flex-end", flexWrap: "wrap", rowGap: 6 }}>
                      {!b.is_current && !b.worktree_path && (
                        <button
                          className="btn"
                          style={{ padding: "2px 10px", fontSize: 12 }}
                          onClick={() => handleSwitchWorkspace(b.name)}
                          disabled={actionLoading === `switch:${b.name}`}
                          title={`Switch to workspace ${b.name}`}
                        >
                          {actionLoading === `switch:${b.name}` ? "..." : "Switch"}
                        </button>
                      )}
                      <button
                        className="btn"
                        style={{ padding: "2px 10px", fontSize: 12 }}
                        onClick={() =>
                          openTerminal({
                            projectPath,
                            workspaceName: b.name,
                          })
                        }
                        title="Open terminal"
                      >
                        <IconTerminal size={14} />
                      </button>
                      <button
                        className="btn"
                        style={{
                          width: 28,
                          height: 24,
                          padding: 0,
                          justifyContent: "center",
                        }}
                          onClick={() => {
                            setFromWorkspace(b.name);
                            setNewWorkspaceName("");
                            setCreationMode(projectDefaultCreationMode);
                            setCopyFiles(detail.worktree_copy_files);
                            setCopyIgnored(detail.worktree_copy_ignored);
                            setShowCreateWorkspace(true);
                          }}
                        title="Workspace from this workspace"
                        aria-label={`Workspace from ${b.name}`}
                      >
                        <svg
                          viewBox="0 0 16 16"
                          width="12"
                          height="12"
                          fill="none"
                          stroke="currentColor"
                          strokeWidth="1.5"
                          strokeLinecap="round"
                          strokeLinejoin="round"
                          aria-hidden="true"
                        >
                          <circle cx="4" cy="3" r="2" />
                          <circle cx="12" cy="8" r="2" />
                          <circle cx="4" cy="13" r="2" />
                          <path d="M6 3h2a4 4 0 0 1 4 4" />
                          <path d="M4 5v6" />
                        </svg>
                      </button>
                      {!b.is_default && (
                        <button
                          className="btn btn-danger"
                          style={{
                            width: 28,
                            height: 24,
                            padding: 0,
                            justifyContent: "center",
                          }}
                          onClick={() => setDeletingWorkspace(b.name)}
                          title={`Delete workspace ${b.name}`}
                          aria-label={`Delete workspace ${b.name}`}
                        >
                          <svg
                            viewBox="0 0 16 16"
                            width="12"
                            height="12"
                            fill="none"
                            stroke="currentColor"
                            strokeWidth="1.5"
                            strokeLinecap="round"
                            strokeLinejoin="round"
                            aria-hidden="true"
                          >
                            <path d="M2.5 4h11" />
                            <path d="M6 2.5h4" />
                            <path d="M5 4v8.5h6V4" />
                            <path d="M7 6.5v4" />
                            <path d="M9 6.5v4" />
                          </svg>
                        </button>
                      )}
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>

      {/* Processes card */}
      <div className="card">
        <div className="flex items-center justify-between mb-4" style={{ gap: 12, flexWrap: "wrap" }}>
          <div>
            <span className="card-title" style={{ marginBottom: 0 }}>
              Processes
            </span>
            <div style={{ color: "var(--text-muted)", fontSize: 12, marginTop: 3 }}>
              Workspace-scoped app servers and workers managed by devflow.
            </div>
          </div>
          <div className="flex items-center gap-2" style={{ flexWrap: "wrap" }}>
            <select
              className="input"
              value={processWorkspace}
              onChange={(e) => setProcessWorkspace(e.target.value)}
              style={{ width: 180, height: 32, fontSize: 13 }}
            >
              {workspaces.map((w) => (
                <option key={w.name} value={w.name}>{w.name}</option>
              ))}
            </select>
            <button
              className="btn"
              onClick={() => fetchProcesses(processWorkspace)}
              disabled={!processWorkspace}
              style={{ padding: "4px 10px", fontSize: 13 }}
            >
              Refresh
            </button>
            <button
              className="btn btn-primary"
              onClick={() => handleStartProcess(undefined, false)}
              disabled={!processWorkspace || actionLoading === "process-start:all"}
              style={{ padding: "4px 10px", fontSize: 13 }}
            >
              {actionLoading === "process-start:all" ? "Starting…" : "Start all"}
            </button>
            <button
              className="btn"
              onClick={() => handleRestartProcess(undefined)}
              disabled={!processWorkspace || actionLoading === "process-restart:all"}
              style={{ padding: "4px 10px", fontSize: 13 }}
            >
              Restart all
            </button>
            <button
              className="btn"
              onClick={() => handleStopProcess(undefined)}
              disabled={!processWorkspace || actionLoading === "process-stop:all"}
              style={{ padding: "4px 10px", fontSize: 13 }}
            >
              Stop all
            </button>
            {selectedProcessNamesForWorkspace.length > 0 && (
              <>
                <button
                  className="btn"
                  onClick={() => handleStartProcess(selectedProcessNamesForWorkspace, false)}
                  disabled={!processWorkspace || actionLoading?.startsWith("process-start:")}
                  style={{ padding: "4px 10px", fontSize: 13 }}
                >
                  Start selected
                </button>
                <button
                  className="btn"
                  onClick={() => handleRestartProcess(selectedProcessNamesForWorkspace)}
                  disabled={!processWorkspace || actionLoading?.startsWith("process-restart:")}
                  style={{ padding: "4px 10px", fontSize: 13 }}
                >
                  Restart selected
                </button>
                <button
                  className="btn"
                  onClick={() => handleStopProcess(selectedProcessNamesForWorkspace)}
                  disabled={!processWorkspace || actionLoading?.startsWith("process-stop:")}
                  style={{ padding: "4px 10px", fontSize: 13 }}
                >
                  Stop selected
                </button>
              </>
            )}
            {hasPitchforkProcesses && (
              <>
                <button
                  className="btn"
                  onClick={handleOpenPitchforkWebUi}
                  disabled={!pitchforkBridge?.web_ui_enabled}
                  title={
                    pitchforkBridge?.web_ui_enabled
                      ? `${pitchforkBridge.web_ui_reachable ? "Open" : "Open configured URL (not reachable yet)"}: ${pitchforkBridge.web_ui_url}`
                      : "Enable processes.pitchfork.web_ui in the config editor first"
                  }
                  style={{ padding: "4px 10px", fontSize: 13 }}
                >
                  Pitchfork Web UI
                </button>
                <button
                  className="btn"
                  onClick={handleLaunchPitchforkTui}
                  disabled={!pitchforkBridge?.cli_available}
                  title={pitchforkBridge?.cli_available ? "Open pitchfork tui in devflow's terminal" : "External pitchfork CLI not found on PATH"}
                  style={{ padding: "4px 10px", fontSize: 13 }}
                >
                  Pitchfork TUI
                </button>
              </>
            )}
          </div>
        </div>
        {!processWorkspace ? (
          <p style={{ color: "var(--text-muted)", fontSize: 13 }}>
            Create or select a workspace to manage processes.
          </p>
        ) : processStatuses.length === 0 ? (
          <div style={{ textAlign: "center", padding: 16 }}>
            <p style={{ color: "var(--text-secondary)", marginBottom: 8 }}>
              No processes configured or recorded for this workspace.
            </p>
            <p style={{ color: "var(--text-muted)", fontSize: 13, margin: 0 }}>
              Add a <code>processes.daemons</code> section to <code>.devflow.yml</code> to run app servers and workers without Docker.
            </p>
          </div>
        ) : (
          <table className="table" style={{ tableLayout: "fixed", width: "100%" }}>
            <thead>
              <tr>
                <th style={{ width: "4%" }}>
                  <input
                    type="checkbox"
                    checked={allProcessesSelected}
                    onChange={(e) =>
                      setSelectedProcesses(
                        e.target.checked ? new Set(allProcessNamesForWorkspace) : new Set()
                      )
                    }
                    aria-label="Select all processes"
                  />
                </th>
                <th style={{ width: "16%" }}>Process</th>
                <th style={{ width: "12%" }}>Status</th>
                <th style={{ width: "12%" }}>PID</th>
                <th style={{ width: "16%" }}>Ports</th>
                <th style={{ width: "22%" }}>URL</th>
                <th style={{ textAlign: "right", width: "20%" }}>Actions</th>
              </tr>
            </thead>
            <tbody>
              {processStatuses.map((p) => {
                const isRunning = p.status === "running" || p.status === "ready";
                const isStopped = !isRunning;
                return (
                  <tr key={`${p.workspace}:${p.process}`}>
                    <td>
                      <input
                        type="checkbox"
                        checked={selectedProcesses.has(p.process)}
                        onChange={(e) => {
                          setSelectedProcesses((prev) => {
                            const next = new Set(prev);
                            if (e.target.checked) next.add(p.process);
                            else next.delete(p.process);
                            return next;
                          });
                        }}
                        aria-label={`Select ${p.process}`}
                      />
                    </td>
                    <td style={{ overflow: "hidden" }}>
                      <div className="flex items-center gap-1" style={{ flexWrap: "wrap" }}>
                        <span style={{ fontWeight: 600 }}>{p.process}</span>
                        {p.runtime && <span className="badge" style={{ fontSize: 10 }}>{p.runtime}</span>}
                        {!p.required && <span className="badge" style={{ fontSize: 10 }}>optional</span>}
                      </div>
                      <div
                        className="mono"
                        title={p.command}
                        style={{ color: "var(--text-muted)", fontSize: 11, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}
                      >
                        {p.command}
                      </div>
                      {p.pitchfork_id && (
                        <div
                          className="mono"
                          title={p.pitchfork_id}
                          style={{ color: "var(--text-muted)", fontSize: 10, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}
                        >
                          {p.pitchfork_id}
                        </div>
                      )}
                    </td>
                    <td>
                      <StatusBadge state={p.status} />
                      {formatRelativeTime(p.started_at) && (
                        <div style={{ color: "var(--text-muted)", fontSize: 11, marginTop: 4 }}>
                          {formatRelativeTime(p.started_at)}
                        </div>
                      )}
                      {p.desired_state && p.desired_state !== p.status && (
                        <div style={{ color: "var(--text-muted)", fontSize: 11, marginTop: 4 }}>
                          desired: {p.desired_state}
                        </div>
                      )}
                      {p.retry_count > 0 && (
                        <div style={{ color: "var(--text-muted)", fontSize: 11, marginTop: 4 }}>
                          retries: {p.retry_count}
                        </div>
                      )}
                    </td>
                    <td className="mono" style={{ color: "var(--text-secondary)", fontSize: 12 }}>
                      {p.pid ?? "-"}
                    </td>
                    <td className="mono" style={{ color: "var(--text-secondary)", fontSize: 12 }}>
                      {p.ports.length ? p.ports.join(", ") : "-"}
                    </td>
                    <td style={{ overflow: "hidden" }}>
                      {p.urls.length > 0 ? (
                        <a
                          href={p.urls[0]}
                          target="_blank"
                          rel="noreferrer"
                          className="mono"
                          style={{ fontSize: 12, color: "var(--accent)", display: "block", overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}
                          title={p.urls[0]}
                        >
                          {p.urls[0]}
                        </a>
                      ) : (
                        <span style={{ color: "var(--text-muted)", fontSize: 12 }}>-</span>
                      )}
                      {p.last_error && (
                        <div style={{ color: "var(--danger)", fontSize: 11, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }} title={p.last_error}>
                          {p.last_error}
                        </div>
                      )}
                    </td>
                    <td style={{ textAlign: "right" }}>
                      <div className="flex gap-1" style={{ justifyContent: "flex-end", flexWrap: "wrap" }}>
                        {isStopped && (
                          <button
                            className="btn"
                            style={{ width: 28, height: 24, padding: 0, justifyContent: "center" }}
                            onClick={() => handleStartProcess(p.process, false)}
                            disabled={actionLoading === `process-start:${p.process}`}
                            title="Start"
                          >
                            <svg viewBox="0 0 16 16" width="12" height="12" aria-hidden="true">
                              <path d="M4 2.5v11l9-5.5z" fill="currentColor" />
                            </svg>
                          </button>
                        )}
                        {isRunning && (
                          <button
                            className="btn"
                            style={{ width: 28, height: 24, padding: 0, justifyContent: "center" }}
                            onClick={() => handleStopProcess(p.process)}
                            disabled={actionLoading === `process-stop:${p.process}`}
                            title="Stop"
                          >
                            <svg viewBox="0 0 16 16" width="12" height="12" aria-hidden="true">
                              <rect x="3" y="3" width="10" height="10" rx="1" fill="currentColor" />
                            </svg>
                          </button>
                        )}
                        <button
                          className="btn"
                          style={{ width: 28, height: 24, padding: 0, justifyContent: "center" }}
                          onClick={() => handleRestartProcess(p.process)}
                          disabled={actionLoading === `process-restart:${p.process}`}
                          title="Restart"
                        >
                          <svg viewBox="0 0 16 16" width="12" height="12" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
                            <path d="M2 8a6 6 0 0 1 10.2-4.2" />
                            <path d="M14 8a6 6 0 0 1-10.2 4.2" />
                            <path d="M10 2l2.2 1.8L14 2" />
                            <path d="M6 14l-2.2-1.8L2 14" />
                          </svg>
                        </button>
                        {p.runtime === "pitchfork" && (
                          <button
                            className="btn"
                            style={{ width: 28, height: 24, padding: 0, justifyContent: "center", fontSize: 10 }}
                            onClick={() => handleStartProcess(p.process, true)}
                            disabled={actionLoading === `process-start:${p.process}`}
                            title="Force restart through Pitchfork"
                          >
                            !
                          </button>
                        )}
                        <button
                          className="btn"
                          style={{ width: 28, height: 24, padding: 0, justifyContent: "center" }}
                          onClick={() => handleViewProcessLogs(p.process, p.workspace)}
                          title="Logs"
                        >
                          <svg viewBox="0 0 16 16" width="12" height="12" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
                            <rect x="2" y="2" width="12" height="12" rx="2" />
                            <path d="M5 9l2 2 2-2" />
                            <path d="M5 6h6" />
                          </svg>
                        </button>
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </div>

      {/* Services card */}
      <div className="card">
        <div className="flex items-center justify-between mb-4">
          <span className="card-title" style={{ marginBottom: 0 }}>
            Services
          </span>
          {detail.has_config && (
            <button
              className="btn btn-primary"
              onClick={() => setShowAddService(true)}
              style={{ padding: "4px 12px", fontSize: 13 }}
            >
              Add Service
            </button>
          )}
        </div>
        {services.length === 0 ? (
          <div style={{ textAlign: "center", padding: 16 }}>
            <p style={{ color: "var(--text-secondary)", marginBottom: 8 }}>
              No services configured yet.
            </p>
            <p style={{ color: "var(--text-muted)", fontSize: 13, marginBottom: 12 }}>
              Add a database service (PostgreSQL, ClickHouse, MySQL, etc.) to
              enable isolated workspaces.
            </p>
            {detail.has_config ? (
              <button
                className="btn btn-primary"
                onClick={() => setShowAddService(true)}
              >
                Add Service
              </button>
            ) : (
              <Link
                to={`/config/${encodeURIComponent(projectPath)}`}
                className="btn btn-primary"
              >
                Edit Config
              </Link>
            )}
          </div>
        ) : (
          <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
            {services.map((s) => {
              const isExpanded = expandedServices.has(s.name);
              const branchList = serviceWorkspaces[s.name] || [];
              const runningCount = branchList.filter(
                (b) => b.state === "running"
              ).length;
              return (
                <div
                  key={s.name}
                  style={{
                    border: "1px solid var(--border)",
                    borderRadius: 8,
                    overflow: "hidden",
                  }}
                >
                  {/* Service header — clickable to toggle */}
                  <div
                    onClick={() => toggleServiceExpanded(s.name)}
                    style={{
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "space-between",
                      padding: "10px 14px",
                      cursor: "pointer",
                      background: isExpanded
                        ? "var(--bg-tertiary)"
                        : "var(--bg-primary)",
                      transition: "background 0.15s",
                    }}
                  >
                    <div className="flex items-center gap-2">
                      <span
                        style={{
                          display: "inline-block",
                          width: 16,
                          textAlign: "center",
                          color: "var(--text-muted)",
                          fontSize: 12,
                          transition: "transform 0.15s",
                          transform: isExpanded
                            ? "rotate(90deg)"
                            : "rotate(0deg)",
                        }}
                      >
                        &#9654;
                      </span>
                      <span style={{ fontWeight: 600, fontSize: 14 }}>
                        {s.name}
                      </span>
                      <span
                        className="badge"
                        style={{
                          background: "var(--bg-tertiary)",
                          color: "var(--text-muted)",
                          fontSize: 11,
                        }}
                      >
                        {s.service_type}
                      </span>
                      <span
                        className="badge"
                        style={{
                          background: "var(--bg-tertiary)",
                          color: "var(--text-muted)",
                          fontSize: 11,
                        }}
                      >
                        {s.provider_type}
                      </span>
                      {s.auto_workspace && (
                        <span className="badge badge-info" style={{ fontSize: 11 }}>
                          auto-workspace
                        </span>
                      )}
                    </div>
                    <div className="flex items-center gap-2">
                      <span
                        style={{
                          color: "var(--text-muted)",
                          fontSize: 12,
                        }}
                      >
                        {branchList.length === 1
                          ? "1 workspace"
                          : `${branchList.length} workspaces`}
                      </span>
                      {runningCount > 0 && (
                        <span className="badge badge-success" style={{ fontSize: 11 }}>
                          {runningCount} running
                        </span>
                      )}
                      {s.provider_type === "local" && (
                        <button
                          className="btn btn-danger"
                          style={{ width: 28, height: 24, padding: 0, justifyContent: "center" }}
                          onClick={(e) => {
                            e.stopPropagation();
                            setDestroyServiceTarget(s.name);
                          }}
                          title={`Destroy service ${s.name}`}
                        >
                          <svg viewBox="0 0 16 16" width="12" height="12" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
                            <path d="M2.5 4h11" />
                            <path d="M6 2.5h4" />
                            <path d="M5 4v8.5h6V4" />
                            <path d="M7 6.5v4" />
                            <path d="M9 6.5v4" />
                          </svg>
                        </button>
                      )}
                    </div>
                  </div>

                  {/* Expanded body — workspace table */}
                  {isExpanded && (
                    <div style={{ borderTop: "1px solid var(--border)" }}>
                      {branchList.length === 0 ? (
                        <div
                          style={{
                            padding: "12px 14px",
                            color: "var(--text-muted)",
                            fontSize: 13,
                          }}
                        >
                          No workspaces yet. Create a workspace above to provision
                          this service.
                        </div>
                      ) : (
                        <table className="table" style={{ marginBottom: 0 }}>
                          <thead>
                            <tr>
                              <th>Workspace</th>
                              <th>Status</th>
                              <th>Parent</th>
                              <th>Database</th>
                              <th style={{ textAlign: "right" }}>Actions</th>
                            </tr>
                          </thead>
                          <tbody>
                            {branchList.map((b) => {
                              const isRunning = b.state === "running";
                              const isStopped =
                                b.state === "stopped" ||
                                b.state === "failed" ||
                                b.state === null ||
                                b.state === undefined;
                              return (
                                <tr key={b.name}>
                                  <td>
                                    <span>
                                      {b.name}
                                    </span>
                                  </td>
                                  <td>
                                    <StatusBadge state={b.state} />
                                  </td>
                                  <td
                                    style={{
                                      color: "var(--text-muted)",
                                      fontSize: 13,
                                    }}
                                  >
                                    {b.parent_workspace || "-"}
                                  </td>
                                  <td
                                    className="mono"
                                    style={{
                                      color: "var(--text-secondary)",
                                      fontSize: 12,
                                    }}
                                  >
                                    {b.database_name}
                                  </td>
                                  <td style={{ textAlign: "right" }}>
                                    <div
                                      className="flex gap-1"
                                      style={{
                                        justifyContent: "flex-end",
                                      }}
                                    >
                                      {/* Connect */}
                                      <button
                                        className="btn"
                                        style={{ width: 28, height: 24, padding: 0, justifyContent: "center" }}
                                        onClick={(e) => {
                                          e.stopPropagation();
                                          handleConnectionInfo(s.name, b.name);
                                        }}
                                        title="Connection info"
                                      >
                                        <svg viewBox="0 0 16 16" width="12" height="12" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
                                          <path d="M6 10l-3 3" />
                                          <path d="M10 6l3-3" />
                                          <path d="M9 7l-2 2" />
                                          <circle cx="4.5" cy="11.5" r="2" />
                                          <circle cx="11.5" cy="4.5" r="2" />
                                        </svg>
                                      </button>
                                      {s.provider_type === "local" && (
                                        <>
                                          {/* Start */}
                                          {isStopped && (
                                            <button
                                              className="btn"
                                              style={{ width: 28, height: 24, padding: 0, justifyContent: "center" }}
                                              onClick={(e) => {
                                                e.stopPropagation();
                                                handleStartService(s.name, b.name);
                                              }}
                                              disabled={actionLoading === `start:${s.name}:${b.name}`}
                                              title="Start"
                                            >
                                              <svg viewBox="0 0 16 16" width="12" height="12" aria-hidden="true">
                                                <path d="M4 2.5v11l9-5.5z" fill="currentColor" />
                                              </svg>
                                            </button>
                                          )}
                                          {/* Stop */}
                                          {isRunning && (
                                            <button
                                              className="btn"
                                              style={{ width: 28, height: 24, padding: 0, justifyContent: "center" }}
                                              onClick={(e) => {
                                                e.stopPropagation();
                                                handleStopService(s.name, b.name);
                                              }}
                                              disabled={actionLoading === `stop:${s.name}:${b.name}`}
                                              title="Stop"
                                            >
                                              <svg viewBox="0 0 16 16" width="12" height="12" aria-hidden="true">
                                                <rect x="3" y="3" width="10" height="10" rx="1" fill="currentColor" />
                                              </svg>
                                            </button>
                                          )}
                                          {/* Reset */}
                                          <button
                                            className="btn"
                                            style={{ width: 28, height: 24, padding: 0, justifyContent: "center" }}
                                            onClick={(e) => {
                                              e.stopPropagation();
                                              setResetTarget({ name: s.name, workspace: b.name });
                                            }}
                                            title="Reset"
                                          >
                                            <svg viewBox="0 0 16 16" width="12" height="12" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
                                              <path d="M2 8a6 6 0 0 1 10.2-4.2" />
                                              <path d="M14 8a6 6 0 0 1-10.2 4.2" />
                                              <path d="M10 2l2.2 1.8L14 2" />
                                              <path d="M6 14l-2.2-1.8L2 14" />
                                            </svg>
                                          </button>
                                          {/* Logs */}
                                          <button
                                            className="btn"
                                            style={{ width: 28, height: 24, padding: 0, justifyContent: "center" }}
                                            onClick={(e) => {
                                              e.stopPropagation();
                                              handleViewLogs(s.name, b.name);
                                            }}
                                            title="Logs"
                                          >
                                            <svg viewBox="0 0 16 16" width="12" height="12" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
                                              <rect x="2" y="2" width="12" height="12" rx="2" />
                                              <path d="M5 9l2 2 2-2" />
                                              <path d="M5 6h6" />
                                            </svg>
                                          </button>
                                          {/* Delete service workspace */}
                                          <button
                                            className="btn btn-danger"
                                            style={{ width: 28, height: 24, padding: 0, justifyContent: "center" }}
                                            onClick={(e) => {
                                              e.stopPropagation();
                                              setDeleteServiceWsTarget({ name: s.name, workspace: b.name });
                                            }}
                                            title={`Delete workspace ${b.name}`}
                                          >
                                            <svg viewBox="0 0 16 16" width="12" height="12" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
                                              <path d="M2.5 4h11" />
                                              <path d="M6 2.5h4" />
                                              <path d="M5 4v8.5h6V4" />
                                              <path d="M7 6.5v4" />
                                              <path d="M9 6.5v4" />
                                            </svg>
                                          </button>
                                        </>
                                      )}
                                    </div>
                                  </td>
                                </tr>
                              );
                            })}
                          </tbody>
                        </table>
                      )}
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        )}
      </div>

      {/* Proxy Domains card */}
      {proxyStatus?.running && (() => {
        const sanitize = (s: string) =>
          s.toLowerCase().replace(/[^a-z0-9]/g, "-").replace(/-{2,}/g, "-");
        const sanitizedName = sanitize(detail.name);
        const projectContainers = containers.filter(
          (c) =>
            c.project === detail.name ||
            c.container_name.startsWith(`devflow-${sanitizedName}-`)
        );
        if (projectContainers.length === 0) return null;
        return (
          <div className="card">
            <span className="card-title" style={{ marginBottom: 12 }}>
              Proxy Domains
            </span>
            <table className="table" style={{ fontSize: 13 }}>
              <thead>
                <tr>
                  <th>Domain</th>
                  <th>Service</th>
                  <th>Workspace</th>
                </tr>
              </thead>
              <tbody>
                {projectContainers.map((c) => (
                  <tr key={c.domain}>
                    <td>
                      <a
                        href={c.endpoint_url}
                        target="_blank"
                        rel="noopener noreferrer"
                        title={c.endpoint_url}
                        style={{
                          color: "var(--accent)",
                          textDecoration: "none",
                        }}
                      >
                        {c.domain}
                      </a>
                    </td>
                    <td>{c.service || "-"}</td>
                    <td>{c.workspace || "-"}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        );
      })()}

      {/* Danger Zone */}
      <div
        className="card"
        style={{
          borderColor: "var(--danger)",
          borderWidth: 1,
          borderStyle: "solid",
        }}
      >
        <span className="card-title" style={{ color: "var(--danger)", marginBottom: 0 }}>
          Danger Zone
        </span>

        <div
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            padding: "12px 0",
            borderBottom: "1px solid var(--border)",
          }}
        >
          <div>
            <div style={{ fontWeight: 500, marginBottom: 2 }}>
              Remove from devflow
            </div>
            <div style={{ color: "var(--text-muted)", fontSize: 13 }}>
              Remove this project from devflow's registry. Does not delete any
              files.
            </div>
          </div>
          <button
            className="btn btn-danger"
            style={{ marginLeft: 16, whiteSpace: "nowrap" }}
            onClick={() => setShowRemoveConfirm(true)}
            disabled={actionLoading === "remove"}
          >
            {actionLoading === "remove" ? "Removing..." : "Remove"}
          </button>
        </div>

        <div
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            padding: "12px 0",
          }}
        >
          <div>
            <div style={{ fontWeight: 500, marginBottom: 2 }}>
              Destroy project
            </div>
            <div style={{ color: "var(--text-muted)", fontSize: 13 }}>
              Tear down all devflow services, worktrees, hooks, and config
              files. This is irreversible.
            </div>
          </div>
          <button
            className="btn btn-danger"
            style={{ marginLeft: 16, whiteSpace: "nowrap" }}
            onClick={() => setShowDestroyConfirm(true)}
            disabled={actionLoading === "destroy"}
          >
            {actionLoading === "destroy" ? "Destroying..." : "Destroy"}
          </button>
        </div>
      </div>

      </>)}

      {/* Destroy Project Confirmation */}
      <Modal
        open={showDestroyConfirm}
        onClose={() => {
          setShowDestroyConfirm(false);
          setDestroyConfirmText("");
        }}
        title="Destroy Project"
        width={480}
      >
        <p style={{ color: "var(--text-secondary)", marginBottom: 12 }}>
          This will permanently destroy all devflow services, worktrees, hooks,
          and config files for <strong>{detail?.name}</strong>. This action
          cannot be undone.
        </p>
        <p style={{ color: "var(--text-secondary)", marginBottom: 8, fontSize: 13 }}>
          Type <strong>{detail?.name}</strong> to confirm:
        </p>
        <input
          type="text"
          value={destroyConfirmText}
          onChange={(e) => setDestroyConfirmText(e.target.value)}
          placeholder={detail?.name}
          style={{ width: "100%", marginBottom: 16 }}
          autoFocus
        />
        <div style={{ display: "flex", gap: 8, justifyContent: "flex-end" }}>
          <button
            className="btn"
            onClick={() => {
              setShowDestroyConfirm(false);
              setDestroyConfirmText("");
            }}
            disabled={actionLoading === "destroy"}
          >
            Cancel
          </button>
          <button
            className="btn btn-danger"
            onClick={handleDestroyProject}
            disabled={
              destroyConfirmText !== detail?.name ||
              actionLoading === "destroy"
            }
          >
            {actionLoading === "destroy"
              ? "Destroying..."
              : "Destroy Project"}
          </button>
        </div>
      </Modal>

      {/* Create Workspace Modal */}
      <Modal
        open={showCreateWorkspace}
        onClose={() => {
          if (!isCreatingWorkspace) {
            setShowCreateWorkspace(false);
          }
        }}
        title="Create Workspace"
      >
        <div style={{ marginBottom: 12 }}>
          <label
            style={{
              display: "block",
              marginBottom: 4,
              fontSize: 13,
              color: "var(--text-secondary)",
            }}
          >
            Workspace name
          </label>
          <input
            type="text"
            value={newWorkspaceName}
            onChange={(e) => setNewWorkspaceName(e.target.value)}
            placeholder="feature/my-workspace"
            style={{ width: "100%" }}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !isCreatingWorkspace) {
                handleCreateWorkspace();
              }
            }}
            autoFocus
            autoCapitalize="off"
            autoCorrect="off"
            spellCheck={false}
          />
        </div>
        <div style={{ marginBottom: 16 }}>
          <label
            style={{
              display: "block",
              marginBottom: 4,
              fontSize: 13,
              color: "var(--text-secondary)",
            }}
          >
            Creation method
          </label>
          <select
            value={creationMode}
            onChange={(e) => setCreationMode(e.target.value as WorkspaceCreationMode)}
            style={{
              width: "100%",
              background: "var(--bg-primary)",
              color: "var(--text-primary)",
              border: "1px solid var(--border)",
              borderRadius: 6,
              padding: "6px 12px",
              fontSize: 14,
            }}
          >
            <option value="worktree">
              Git worktree{detail.worktree_enabled ? " (default)" : ""}
            </option>
            <option value="branch">
              Git branch{!detail.worktree_enabled ? " (default)" : ""}
            </option>
          </select>
          <p style={{ marginTop: 6, color: "var(--text-muted)", fontSize: 12 }}>
            The project default is preselected.
          </p>
        </div>
        {creationMode === "worktree" && detail.worktree_copy_files.length > 0 && (
          <div style={{ marginBottom: 16 }}>
            <label
              style={{
                display: "block",
                marginBottom: 4,
                fontSize: 13,
                color: "var(--text-secondary)",
              }}
            >
              Files copied to worktree
            </label>
            <div
              style={{
                background: "var(--bg-secondary)",
                border: "1px solid var(--border)",
                borderRadius: 6,
                padding: "6px 10px",
                fontSize: 12,
                fontFamily: "monospace",
              }}
            >
              {detail.worktree_copy_files.map((f, i) => (
                <label
                  key={i}
                  style={{
                    display: "flex",
                    alignItems: "center",
                    gap: 8,
                    padding: "2px 0",
                    cursor: "pointer",
                    color: copyFiles.includes(f)
                      ? "var(--text-secondary)"
                      : "var(--text-muted)",
                  }}
                >
                  <input
                    type="checkbox"
                    checked={copyFiles.includes(f)}
                    onChange={(e) => {
                      if (e.target.checked) {
                        setCopyFiles((prev) => [...prev, f]);
                      } else {
                        setCopyFiles((prev) => prev.filter((x) => x !== f));
                      }
                    }}
                  />
                  {f}
                </label>
              ))}
            </div>
            <p style={{ marginTop: 4, color: "var(--text-muted)", fontSize: 12 }}>
              Uncheck to skip copying a file (configured in .devflow.yml).
            </p>
          </div>
        )}
        {creationMode === "worktree" && (
          <div style={{ marginBottom: 16 }}>
            <label
              style={{
                display: "flex",
                alignItems: "center",
                gap: 8,
                fontSize: 13,
                color: "var(--text-secondary)",
                cursor: "pointer",
              }}
            >
              <input
                type="checkbox"
                checked={copyIgnored}
                onChange={(e) => setCopyIgnored(e.target.checked)}
              />
              Copy gitignored files (node_modules, target, etc.)
            </label>
            <p style={{ marginTop: 4, color: "var(--text-muted)", fontSize: 12 }}>
              Reflink-copies ignored directories from the main worktree for faster setup.
            </p>
          </div>
        )}
        <div style={{ marginBottom: 16 }}>
          <label
            style={{
              display: "flex",
              alignItems: "center",
              gap: 8,
              fontSize: 13,
              color: "var(--text-secondary)",
              cursor: "pointer",
            }}
          >
            <input
              type="checkbox"
              checked={sandboxed}
              onChange={(e) => setSandboxed(e.target.checked)}
            />
            Sandboxed workspace
          </label>
          <p style={{ marginTop: 4, color: "var(--text-muted)", fontSize: 12 }}>
            Restricts filesystem access and blocks dangerous commands (git push, npm publish, etc.).
          </p>
        </div>
        <div style={{ marginBottom: 16 }}>
          <label
            style={{
              display: "block",
              marginBottom: 4,
              fontSize: 13,
              color: "var(--text-secondary)",
            }}
          >
            From workspace (optional)
          </label>
          <select
            value={fromWorkspace}
            onChange={(e) => setFromWorkspace(e.target.value)}
            style={{
              width: "100%",
              background: "var(--bg-primary)",
              color: "var(--text-primary)",
              border: "1px solid var(--border)",
              borderRadius: 6,
              padding: "6px 12px",
              fontSize: 14,
            }}
          >
            {workspaces.map((b) => (
              <option key={b.name} value={b.name}>
                {b.name}{b.is_default ? " (default)" : ""}
              </option>
            ))}
          </select>
        </div>
        <div style={{ display: "flex", gap: 8, justifyContent: "flex-end" }}>
          <button
            className="btn"
            onClick={() => setShowCreateWorkspace(false)}
            disabled={isCreatingWorkspace}
          >
            Cancel
          </button>
          <button
            className="btn btn-primary"
            onClick={handleCreateWorkspace}
            disabled={!newWorkspaceName.trim() || isCreatingWorkspace}
          >
            {isCreatingWorkspace ? "Creating..." : "Create"}
          </button>
        </div>
        {isCreatingWorkspace && (
          <p style={{ marginTop: 10, color: "var(--text-muted)", fontSize: 12 }}>
            Workspace creation is running and cannot be interrupted once started.
          </p>
        )}
      </Modal>

      {/* Remove Project Confirmation */}
      <ConfirmDialog
        open={showRemoveConfirm}
        onClose={() => setShowRemoveConfirm(false)}
        onConfirm={handleRemoveProject}
        title="Remove Project"
        message={`Remove "${detail?.name}" from devflow? This only removes it from devflow's registry and does not delete project files.`}
        confirmLabel="Remove"
        danger
        loading={actionLoading === "remove"}
      />

      {/* Delete Workspace Confirmation */}
      <ConfirmDialog
        open={deletingWorkspace !== null}
        onClose={() => setDeletingWorkspace(null)}
        onConfirm={handleDeleteWorkspace}
        title="Delete Workspace"
        message={`Delete workspace "${deletingWorkspace}"? This will also delete associated service workspaces.`}
        confirmLabel="Delete"
        danger
        loading={actionLoading === "delete"}
      />

      {/* Reset Service Confirmation */}
      <ConfirmDialog
        open={resetTarget !== null}
        onClose={() => setResetTarget(null)}
        onConfirm={handleResetService}
        title="Reset Service"
        message={`Reset service "${resetTarget?.name}" for workspace "${resetTarget?.workspace}"? This will destroy all data for this workspace.`}
        confirmLabel="Reset"
        danger
        loading={actionLoading === "reset"}
      />

      {/* Delete Service Workspace Confirmation */}
      <ConfirmDialog
        open={deleteServiceWsTarget !== null}
        onClose={() => setDeleteServiceWsTarget(null)}
        onConfirm={handleDeleteServiceWorkspace}
        title="Delete Service Workspace"
        message={`Delete workspace "${deleteServiceWsTarget?.workspace}" for service "${deleteServiceWsTarget?.name}"? This will destroy all data for this workspace.`}
        confirmLabel="Delete"
        danger
        loading={actionLoading === "delete-svc-ws"}
      />

      {/* Destroy Service Confirmation */}
      <ConfirmDialog
        open={destroyServiceTarget !== null}
        onClose={() => setDestroyServiceTarget(null)}
        onConfirm={handleDestroyService}
        title="Destroy Service"
        message={`Destroy service "${destroyServiceTarget}"? This permanently deletes all workspaces, containers, data, and removes it from config.`}
        confirmLabel="Destroy"
        danger
        loading={actionLoading === "destroy-service"}
      />

      {/* Connection Info Modal */}
      <Modal
        open={connInfoWorkspace !== null}
        onClose={() => {
          setConnInfoWorkspace(null);
          setConnInfo({});
          setConnInfoContainers({});
        }}
        title={`Connection Info — ${connInfoWorkspace}`}
        width={560}
      >
        {Object.keys(connInfo).length === 0 ? (
          <p style={{ color: "var(--text-secondary)" }}>Loading...</p>
        ) : (
          Object.entries(connInfo).map(([svcName, info]) => (
            <div key={svcName} style={{ marginBottom: 16 }}>
              <div
                style={{
                  fontWeight: 600,
                  marginBottom: 8,
                  fontSize: 14,
                }}
              >
                {svcName}
              </div>
              <table className="table" style={{ fontSize: 13 }}>
                <tbody>
                  <ConnRow label="Host" value={info.host} copyable />
                  <ConnRow label="Port" value={String(info.port)} copyable />
                  <ConnRow label="Database" value={info.database} copyable />
                  <ConnRow label="User" value={info.user} copyable />
                  <ConnRow
                    label="Password"
                    value={info.password || "-"}
                    secret
                  />
                  {info.connection_string && (
                    <ConnRow
                      label="Connection String"
                      value={info.connection_string}
                      secret
                    />
                  )}
                  {info.connection_string && connInfoContainers[svcName] && (
                    <ConnRow
                      label="Proxy Connection"
                      value={info.connection_string.replace(
                        `${info.host}:${info.port}`,
                        `${connInfoContainers[svcName].domain}:${info.port}`
                      )}
                      secret
                    />
                  )}
                  {connInfoContainers[svcName] && (
                    <ConnRow
                      label="Proxy Domain"
                      value={connInfoContainers[svcName].domain}
                      copyable
                    />
                  )}
                </tbody>
              </table>
            </div>
          ))
        )}
      </Modal>

      {/* Logs Modal */}
      <Modal
        open={logService !== null || logProcess !== null}
        onClose={() => {
          setLogService(null);
          setLogProcess(null);
        }}
        title={
          logProcess
            ? `Process logs — ${logProcess.name} (${logProcess.workspace})`
            : `Service logs — ${logService?.name} (${logService?.workspace})`
        }
        width={700}
      >
        <pre
          className="mono"
          style={{
            background: "var(--bg-primary)",
            border: "1px solid var(--border)",
            borderRadius: 6,
            padding: 12,
            maxHeight: 400,
            overflowY: "auto",
            whiteSpace: "pre-wrap",
            wordBreak: "break-all",
            fontSize: 12,
            lineHeight: 1.4,
            color: "var(--text-secondary)",
          }}
        >
          {logContent}
        </pre>
      </Modal>

      {/* Add Service Modal */}
      <AddServiceModal
        open={showAddService}
        onClose={() => setShowAddService(false)}
        onAdd={handleAddService}
        projectPath={projectPath}
      />
    </div>
  );
}

function StatusBadge({ state }: { state: string | null }) {
  if (state === "running" || state === "ready") {
    return <span className="badge badge-success">{state}</span>;
  }
  if (state === "not_started") {
    return (
      <span
        className="badge"
        style={{ background: "var(--bg-tertiary)", color: "var(--text-muted)" }}
      >
        not started
      </span>
    );
  }
  if (state === "failed") {
    return <span className="badge badge-danger">failed</span>;
  }
  if (state === "provisioning") {
    return <span className="badge badge-warning">provisioning</span>;
  }
  if (state === "stopped") {
    return (
      <span
        className="badge"
        style={{ background: "var(--bg-tertiary)", color: "var(--text-muted)" }}
      >
        stopped
      </span>
    );
  }
  return (
    <span
      className="badge"
      style={{ background: "var(--bg-tertiary)", color: "var(--text-muted)" }}
    >
      -
    </span>
  );
}

function ConnRow({
  label,
  value,
  secret = false,
  copyable = false,
}: {
  label: string;
  value: string;
  secret?: boolean;
  copyable?: boolean;
}) {
  const [revealed, setRevealed] = useState(false);
  const [copied, setCopied] = useState(false);

  const handleCopy = () => {
    navigator.clipboard.writeText(value);
    setCopied(true);
    setTimeout(() => setCopied(false), 1500);
  };

  return (
    <tr>
      <td style={{ color: "var(--text-muted)", width: 140, fontWeight: 500 }}>
        {label}
      </td>
      <td>
        <div className="flex items-center gap-2">
          <span className="mono" style={{ color: "var(--text-primary)" }}>
            {secret && !revealed ? "••••••••" : value}
          </span>
          {secret && (
            <button
              onClick={() => setRevealed(!revealed)}
              style={{
                background: "none",
                border: "none",
                color: "var(--accent)",
                cursor: "pointer",
                fontSize: 11,
                padding: 0,
              }}
            >
              {revealed ? "hide" : "show"}
            </button>
          )}
          {(copyable || secret) && (
            <button
              onClick={handleCopy}
              style={{
                background: "none",
                border: "none",
                color: "var(--accent)",
                cursor: "pointer",
                fontSize: 11,
                padding: 0,
              }}
            >
              {copied ? "copied!" : "copy"}
            </button>
          )}
        </div>
      </td>
    </tr>
  );
}

export default ProjectDetail;
