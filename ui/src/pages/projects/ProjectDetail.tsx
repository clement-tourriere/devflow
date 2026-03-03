import { useState, useEffect, useCallback } from "react";
import { useParams, Link, useNavigate } from "react-router-dom";
import {
  getProjectDetail,
  listBranches,
  listServices,
  addService,
  createBranch,
  deleteBranch,
  startService,
  stopService,
  resetService,
  getServiceLogs,
  getConnectionInfo,
  listServiceBranches,
  removeProject,
  destroyProject,
  listContainers,
  getProxyStatus,
} from "../../utils/invoke";
import type {
  ProjectDetail as ProjectDetailType,
  BranchEntry,
  ServiceEntry,
  ServiceBranchInfo,
  ConnectionInfo,
  AddServiceRequest,
  ContainerEntry,
  ProxyStatus,
} from "../../types";
import Modal from "../../components/Modal";
import ConfirmDialog from "../../components/ConfirmDialog";
import { useTerminal } from "../../context/TerminalContext";

function ProjectDetail() {
  const { "*": splat } = useParams();
  const projectPath = splat ? decodeURIComponent(splat) : "";
  const navigate = useNavigate();
  const { openTerminal } = useTerminal();

  const [detail, setDetail] = useState<ProjectDetailType | null>(null);
  const [branches, setBranches] = useState<BranchEntry[]>([]);
  const [services, setServices] = useState<ServiceEntry[]>([]);
  const [currentBranch, setCurrentBranch] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  // Service branch tracking: { "service-name": ServiceBranchInfo[] }
  const [serviceBranches, setServiceBranches] = useState<
    Record<string, ServiceBranchInfo[]>
  >({});
  const [expandedServices, setExpandedServices] = useState<Set<string>>(
    new Set()
  );

  // Modal state
  const [showCreateBranch, setShowCreateBranch] = useState(false);
  const [newBranchName, setNewBranchName] = useState("");
  const [fromBranch, setFromBranch] = useState("");
  const [deletingBranch, setDeletingBranch] = useState<string | null>(null);
  const [connInfoBranch, setConnInfoBranch] = useState<string | null>(null);
  const [connInfo, setConnInfo] = useState<Record<string, ConnectionInfo>>({});
  const [connInfoContainers, setConnInfoContainers] = useState<Record<string, ContainerEntry>>({});
  const [logService, setLogService] = useState<{
    name: string;
    branch: string;
  } | null>(null);
  const [logContent, setLogContent] = useState("");
  const [resetTarget, setResetTarget] = useState<{
    name: string;
    branch: string;
  } | null>(null);

  // Add Service modal state
  const [showAddService, setShowAddService] = useState(false);
  const [addSvcType, setAddSvcType] = useState("postgres");
  const [addSvcProvider, setAddSvcProvider] = useState("local");
  const [addSvcName, setAddSvcName] = useState("app-db");
  const [addSvcImage, setAddSvcImage] = useState("postgres:17");
  const [addSvcSeed, setAddSvcSeed] = useState("");
  const [addSvcAutoBranch, setAddSvcAutoBranch] = useState(true);
  const [addSvcError, setAddSvcError] = useState<string | null>(null);

  // Danger zone state
  const [showDestroyConfirm, setShowDestroyConfirm] = useState(false);
  const [destroyConfirmText, setDestroyConfirmText] = useState("");

  // Proxy state
  const [proxyStatus, setProxyStatus] = useState<ProxyStatus | null>(null);
  const [containers, setContainers] = useState<ContainerEntry[]>([]);

  // Loading state
  const [actionLoading, setActionLoading] = useState<string | null>(null);

  const fetchServiceBranches = useCallback(
    async (svcs: ServiceEntry[]) => {
      if (svcs.length === 0) {
        setServiceBranches({});
        return;
      }
      const result: Record<string, ServiceBranchInfo[]> = {};
      await Promise.all(
        svcs.map(async (s) => {
          try {
            result[s.name] = await listServiceBranches(projectPath, s.name);
          } catch {
            result[s.name] = [];
          }
        })
      );
      setServiceBranches(result);
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
      listBranches(projectPath)
        .then((b) => {
          setBranches(b.branches);
          setCurrentBranch(b.current);
        })
        .catch(() => {
          setBranches([]);
          setCurrentBranch(null);
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
      listContainers()
        .then(setContainers)
        .catch(() => setContainers([])),
    ]);
    // Fetch service branches after we know the services
    await fetchServiceBranches(loadedServices);
  }, [projectPath, fetchServiceBranches]);

  useEffect(() => {
    reload();
  }, [reload]);

  const handleCreateBranch = async () => {
    if (!newBranchName.trim()) return;
    setActionLoading("create");
    try {
      await createBranch(
        projectPath,
        newBranchName.trim(),
        fromBranch || undefined
      );
      setShowCreateBranch(false);
      setNewBranchName("");
      setFromBranch("");
      await reload();
    } catch (e) {
      alert(`Failed to create branch: ${e}`);
    } finally {
      setActionLoading(null);
    }
  };

  const handleDeleteBranch = async () => {
    if (!deletingBranch) return;
    setActionLoading("delete");
    try {
      await deleteBranch(projectPath, deletingBranch);
      setDeletingBranch(null);
      await reload();
    } catch (e) {
      alert(`Failed to delete branch: ${e}`);
    } finally {
      setActionLoading(null);
    }
  };

  const handleConnectionInfo = async (
    serviceName: string,
    branchName: string
  ) => {
    setConnInfoBranch(branchName);
    try {
      const infos: Record<string, ConnectionInfo> = {};
      try {
        const info = await getConnectionInfo(
          projectPath,
          branchName,
          serviceName
        );
        infos[serviceName] = info as unknown as ConnectionInfo;
      } catch {
        // service may not have connection info
      }
      setConnInfo(infos);

      // Look up matching proxy containers for this service/branch
      if (proxyStatus?.running && detail) {
        try {
          const allContainers = await listContainers();
          const matched: Record<string, ContainerEntry> = {};
          // Helper: sanitize name the same way Rust does (lowercase, non-alnum -> dash)
          const sanitize = (s: string) =>
            s.toLowerCase().replace(/[^a-z0-9]/g, "-").replace(/-{2,}/g, "-");
          for (const c of allContainers) {
            if (c.project === detail.name && c.service === serviceName) {
              // Prefer exact branch match, but also accept if branch is null (legacy containers)
              if (c.branch === branchName || !c.branch) {
                matched[serviceName] = c;
              }
            } else {
              // Fallback: match by container name pattern devflow-{sanitized_project}-{service}-{branch}
              const expectedName = `devflow-${sanitize(detail.name)}-${sanitize(serviceName)}-${sanitize(branchName)}`;
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

  const handleStartService = async (svcName: string, branchName: string) => {
    setActionLoading(`start:${svcName}:${branchName}`);
    try {
      await startService(projectPath, svcName, branchName);
      // Refresh branches for this service
      try {
        const branches = await listServiceBranches(projectPath, svcName);
        setServiceBranches((prev) => ({ ...prev, [svcName]: branches }));
      } catch {
        // ignore
      }
    } catch (e) {
      alert(`Failed to start service: ${e}`);
    } finally {
      setActionLoading(null);
    }
  };

  const handleStopService = async (svcName: string, branchName: string) => {
    setActionLoading(`stop:${svcName}:${branchName}`);
    try {
      await stopService(projectPath, svcName, branchName);
      // Refresh branches for this service
      try {
        const branches = await listServiceBranches(projectPath, svcName);
        setServiceBranches((prev) => ({ ...prev, [svcName]: branches }));
      } catch {
        // ignore
      }
    } catch (e) {
      alert(`Failed to stop service: ${e}`);
    } finally {
      setActionLoading(null);
    }
  };

  const handleResetService = async () => {
    if (!resetTarget) return;
    setActionLoading("reset");
    try {
      await resetService(projectPath, resetTarget.name, resetTarget.branch);
      // Refresh branches for this service
      try {
        const branches = await listServiceBranches(
          projectPath,
          resetTarget.name
        );
        setServiceBranches((prev) => ({
          ...prev,
          [resetTarget.name]: branches,
        }));
      } catch {
        // ignore
      }
      setResetTarget(null);
    } catch (e) {
      alert(`Failed to reset service: ${e}`);
    } finally {
      setActionLoading(null);
    }
  };

  const handleViewLogs = async (svcName: string, branchName: string) => {
    setLogService({ name: svcName, branch: branchName });
    setLogContent("Loading...");
    try {
      const logs = await getServiceLogs(projectPath, svcName, branchName);
      setLogContent(logs || "(no log output)");
    } catch (e) {
      setLogContent(`Error: ${e}`);
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
      navigate("/projects");
    } catch (e) {
      alert(`Failed to remove project: ${e}`);
    } finally {
      setActionLoading(null);
    }
  };

  const handleDestroyProject = async () => {
    setActionLoading("destroy");
    try {
      await destroyProject(projectPath);
      await removeProject(projectPath);
      navigate("/projects");
    } catch (e) {
      alert(`Destroy failed: ${e}`);
    } finally {
      setActionLoading(null);
      setShowDestroyConfirm(false);
      setDestroyConfirmText("");
    }
  };

  const defaultImageForType = (type: string) => {
    switch (type) {
      case "postgres": return "postgres:17";
      case "clickhouse": return "clickhouse/clickhouse-server:latest";
      case "mysql": return "mysql:8";
      default: return "";
    }
  };

  const defaultNameForType = (type: string) => {
    switch (type) {
      case "postgres": return "app-db";
      case "clickhouse": return "analytics";
      case "mysql": return "app-db";
      case "generic": return "cache";
      default: return "service";
    }
  };

  const openAddServiceModal = () => {
    setAddSvcType("postgres");
    setAddSvcProvider("local");
    setAddSvcName("app-db");
    setAddSvcImage("postgres:17");
    setAddSvcSeed("");
    setAddSvcAutoBranch(true);
    setAddSvcError(null);
    setShowAddService(true);
  };

  const handleServiceTypeChange = (type: string) => {
    setAddSvcType(type);
    setAddSvcName(defaultNameForType(type));
    setAddSvcImage(defaultImageForType(type));
    // Non-postgres types only support local provider
    if (type !== "postgres") {
      setAddSvcProvider("local");
    }
  };

  const handleAddService = async () => {
    if (!addSvcName.trim()) {
      setAddSvcError("Service name is required");
      return;
    }
    if (addSvcType === "generic" && !addSvcImage.trim()) {
      setAddSvcError("Docker image is required for generic services");
      return;
    }
    setAddSvcError(null);
    setActionLoading("add-service");
    try {
      const request: AddServiceRequest = {
        name: addSvcName.trim(),
        service_type: addSvcType,
        provider_type: addSvcProvider,
        auto_branch: addSvcAutoBranch,
        image: addSvcImage.trim() || undefined,
        seed_from: addSvcSeed.trim() || undefined,
      };
      await addService(projectPath, request);
      setShowAddService(false);
      await reload();
    } catch (e) {
      setAddSvcError(`${e}`);
    } finally {
      setActionLoading(null);
    }
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

  if (!detail) return <div>Loading...</div>;

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
            {currentBranch && !detail.worktree_enabled && (
              <span className="badge" style={{ opacity: 0.7 }}>HEAD: {currentBranch}</span>
            )}
            {detail.has_config ? (
              <span className="badge badge-success">configured</span>
            ) : (
              <span className="badge badge-warning">no config</span>
            )}
            {detail.vcs_type && (
              <span className="badge">{detail.vcs_type}</span>
            )}
            {detail.worktree_enabled ? (
              <span className="badge badge-info">worktrees</span>
            ) : (
              <span className="badge">checkout mode</span>
            )}
          </div>
        </div>
        <div className="flex gap-2">
          <Link
            to={`/hooks/${encodeURIComponent(projectPath)}`}
            className="btn"
          >
            Hooks
          </Link>
          <Link
            to={`/config/${encodeURIComponent(projectPath)}`}
            className="btn"
          >
            Config
          </Link>
          {detail.has_config && (
            <Link
              to={`/doctor/${encodeURIComponent(projectPath)}`}
              className="btn"
            >
              Doctor
            </Link>
          )}
        </div>
      </div>

      {/* Branches card */}
      <div className="card">
        <div className="flex items-center justify-between mb-4">
          <span className="card-title" style={{ marginBottom: 0 }}>
            Branches
          </span>
          <button
            className="btn btn-primary"
            onClick={() => {
              const defaultBranch = branches.find((b) => b.is_default);
              setFromBranch(defaultBranch?.name ?? "");
              setNewBranchName("");
              setShowCreateBranch(true);
            }}
            style={{ padding: "4px 12px", fontSize: 13 }}
          >
            Create Branch
          </button>
        </div>
        {branches.length === 0 ? (
          <p style={{ color: "var(--text-secondary)" }}>No branches found.</p>
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>Branch</th>
                <th>Parent</th>
                <th>Created</th>
                <th>Worktree</th>
                <th style={{ textAlign: "right" }}>Actions</th>
              </tr>
            </thead>
            <tbody>
              {branches.map((b) => (
                <tr key={b.name}>
                  <td>
                    <span style={{ fontWeight: b.is_default ? 600 : 400 }}>
                      {b.name}
                    </span>
                    {b.is_default && (
                      <span
                        className="badge badge-info"
                        style={{ marginLeft: 6 }}
                      >
                        default
                      </span>
                    )}
                    {!detail.worktree_enabled && b.is_current && (
                      <span
                        className="badge"
                        style={{ marginLeft: 6, fontSize: 10, opacity: 0.7 }}
                      >
                        HEAD
                      </span>
                    )}
                    {b.agent_status && (
                      <span
                        className={`badge${b.agent_status === "running" ? " badge-success" : ""}`}
                        style={{ marginLeft: 6 }}
                        title={b.agent_tool ? `Agent: ${b.agent_tool}` : undefined}
                      >
                        {b.agent_status}
                      </span>
                    )}
                  </td>
                  <td style={{ color: "var(--text-muted)", fontSize: 13 }}>
                    {b.parent || "-"}
                  </td>
                  <td style={{ color: "var(--text-muted)", fontSize: 12 }}>
                    {b.created_at || "-"}
                  </td>
                  <td
                    className="mono"
                    style={{ color: "var(--text-muted)", fontSize: 12 }}
                  >
                    {b.worktree_path || "-"}
                  </td>
                  <td style={{ textAlign: "right" }}>
                    <div className="flex gap-2" style={{ justifyContent: "flex-end" }}>
                      <button
                        className="btn"
                        style={{ padding: "2px 10px", fontSize: 12 }}
                        onClick={() =>
                          openTerminal({
                            projectPath,
                            branchName: b.name,
                          })
                        }
                        title="Open terminal"
                      >
                        &gt;_
                      </button>
                      <button
                        className="btn"
                        style={{ padding: "2px 10px", fontSize: 12 }}
                        onClick={() => {
                          setFromBranch(b.name);
                          setNewBranchName("");
                          setShowCreateBranch(true);
                        }}
                      >
                        Branch from
                      </button>
                      {!b.is_default && (
                        <button
                          className="btn btn-danger"
                          style={{ padding: "2px 10px", fontSize: 12 }}
                          onClick={() => setDeletingBranch(b.name)}
                        >
                          Delete
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

      {/* Services card */}
      <div className="card">
        <div className="flex items-center justify-between mb-4">
          <span className="card-title" style={{ marginBottom: 0 }}>
            Services
          </span>
          {detail.has_config && (
            <button
              className="btn btn-primary"
              onClick={openAddServiceModal}
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
              enable branching.
            </p>
            {detail.has_config ? (
              <button
                className="btn btn-primary"
                onClick={openAddServiceModal}
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
              const branchList = serviceBranches[s.name] || [];
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
                      {s.auto_branch && (
                        <span className="badge badge-info" style={{ fontSize: 11 }}>
                          auto-branch
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
                        {branchList.length} branch
                        {branchList.length !== 1 ? "es" : ""}
                      </span>
                      {runningCount > 0 && (
                        <span className="badge badge-success" style={{ fontSize: 11 }}>
                          {runningCount} running
                        </span>
                      )}
                    </div>
                  </div>

                  {/* Expanded body — branch table */}
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
                          No branches yet. Create a branch above to provision
                          this service.
                        </div>
                      ) : (
                        <table className="table" style={{ marginBottom: 0 }}>
                          <thead>
                            <tr>
                              <th>Branch</th>
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
                                    {b.parent_branch || "-"}
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
                                      className="flex gap-2"
                                      style={{
                                        justifyContent: "flex-end",
                                      }}
                                    >
                                      <button
                                        className="btn"
                                        style={{
                                          padding: "2px 10px",
                                          fontSize: 12,
                                        }}
                                        onClick={(e) => {
                                          e.stopPropagation();
                                          openTerminal({
                                            projectPath,
                                            branchName: b.name,
                                            serviceName: s.name,
                                          });
                                        }}
                                      >
                                        Terminal
                                      </button>
                                      <button
                                        className="btn"
                                        style={{
                                          padding: "2px 10px",
                                          fontSize: 12,
                                        }}
                                        onClick={(e) => {
                                          e.stopPropagation();
                                          handleConnectionInfo(
                                            s.name,
                                            b.name
                                          );
                                        }}
                                      >
                                        Connect
                                      </button>
                                      {s.provider_type === "local" && (
                                        <>
                                          {isStopped && (
                                            <button
                                              className="btn"
                                              style={{
                                                padding: "2px 10px",
                                                fontSize: 12,
                                              }}
                                              onClick={(e) => {
                                                e.stopPropagation();
                                                handleStartService(
                                                  s.name,
                                                  b.name
                                                );
                                              }}
                                              disabled={
                                                actionLoading ===
                                                `start:${s.name}:${b.name}`
                                              }
                                            >
                                              {actionLoading ===
                                              `start:${s.name}:${b.name}`
                                                ? "..."
                                                : "Start"}
                                            </button>
                                          )}
                                          {isRunning && (
                                            <button
                                              className="btn"
                                              style={{
                                                padding: "2px 10px",
                                                fontSize: 12,
                                              }}
                                              onClick={(e) => {
                                                e.stopPropagation();
                                                handleStopService(
                                                  s.name,
                                                  b.name
                                                );
                                              }}
                                              disabled={
                                                actionLoading ===
                                                `stop:${s.name}:${b.name}`
                                              }
                                            >
                                              {actionLoading ===
                                              `stop:${s.name}:${b.name}`
                                                ? "..."
                                                : "Stop"}
                                            </button>
                                          )}
                                          <button
                                            className="btn"
                                            style={{
                                              padding: "2px 10px",
                                              fontSize: 12,
                                            }}
                                            onClick={(e) => {
                                              e.stopPropagation();
                                              setResetTarget({
                                                name: s.name,
                                                branch: b.name,
                                              });
                                            }}
                                          >
                                            Reset
                                          </button>
                                          <button
                                            className="btn"
                                            style={{
                                              padding: "2px 10px",
                                              fontSize: 12,
                                            }}
                                            onClick={(e) => {
                                              e.stopPropagation();
                                              handleViewLogs(
                                                s.name,
                                                b.name
                                              );
                                            }}
                                          >
                                            Logs
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
                  <th>Branch</th>
                </tr>
              </thead>
              <tbody>
                {projectContainers.map((c) => (
                  <tr key={c.domain}>
                    <td>
                      <a
                        href={c.https_url}
                        target="_blank"
                        rel="noopener noreferrer"
                        style={{
                          color: "var(--accent)",
                          textDecoration: "none",
                        }}
                      >
                        {c.domain}
                      </a>
                    </td>
                    <td>{c.service || "-"}</td>
                    <td>{c.branch || "-"}</td>
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
            onClick={handleRemoveProject}
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

      {/* Create Branch Modal */}
      <Modal
        open={showCreateBranch}
        onClose={() => setShowCreateBranch(false)}
        title="Create Branch"
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
            Branch name
          </label>
          <input
            type="text"
            value={newBranchName}
            onChange={(e) => setNewBranchName(e.target.value)}
            placeholder="feature/my-branch"
            style={{ width: "100%" }}
            onKeyDown={(e) => e.key === "Enter" && handleCreateBranch()}
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
            From branch (optional)
          </label>
          <select
            value={fromBranch}
            onChange={(e) => setFromBranch(e.target.value)}
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
            {branches.map((b) => (
              <option key={b.name} value={b.name}>
                {b.name}{b.is_default ? " (default)" : ""}
              </option>
            ))}
          </select>
        </div>
        <div style={{ display: "flex", gap: 8, justifyContent: "flex-end" }}>
          <button
            className="btn"
            onClick={() => setShowCreateBranch(false)}
          >
            Cancel
          </button>
          <button
            className="btn btn-primary"
            onClick={handleCreateBranch}
            disabled={!newBranchName.trim() || actionLoading === "create"}
          >
            {actionLoading === "create" ? "Creating..." : "Create"}
          </button>
        </div>
      </Modal>

      {/* Delete Branch Confirmation */}
      <ConfirmDialog
        open={deletingBranch !== null}
        onClose={() => setDeletingBranch(null)}
        onConfirm={handleDeleteBranch}
        title="Delete Branch"
        message={`Delete branch "${deletingBranch}"? This will also delete associated service branches.`}
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
        message={`Reset service "${resetTarget?.name}" for branch "${resetTarget?.branch}"? This will destroy all data for this branch.`}
        confirmLabel="Reset"
        danger
        loading={actionLoading === "reset"}
      />

      {/* Connection Info Modal */}
      <Modal
        open={connInfoBranch !== null}
        onClose={() => {
          setConnInfoBranch(null);
          setConnInfo({});
          setConnInfoContainers({});
        }}
        title={`Connection Info — ${connInfoBranch}`}
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
        open={logService !== null}
        onClose={() => setLogService(null)}
        title={`Logs — ${logService?.name} (${logService?.branch})`}
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
      <Modal
        open={showAddService}
        onClose={() => setShowAddService(false)}
        title="Add Service"
        width={500}
      >
        {/* Service Type */}
        <div style={{ marginBottom: 16 }}>
          <label style={{ display: "block", marginBottom: 6, fontSize: 13, color: "var(--text-secondary)", fontWeight: 500 }}>
            Service Type
          </label>
          <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 8 }}>
            {(["postgres", "clickhouse", "mysql", "generic"] as const).map((t) => (
              <button
                key={t}
                onClick={() => handleServiceTypeChange(t)}
                style={{
                  padding: "10px 12px",
                  border: `2px solid ${addSvcType === t ? "var(--accent)" : "var(--border)"}`,
                  borderRadius: 8,
                  background: addSvcType === t ? "var(--bg-hover)" : "var(--bg-primary)",
                  color: "var(--text-primary)",
                  cursor: "pointer",
                  textAlign: "left",
                  fontSize: 14,
                  fontWeight: addSvcType === t ? 600 : 400,
                }}
              >
                {{ postgres: "PostgreSQL", clickhouse: "ClickHouse", mysql: "MySQL", generic: "Generic Docker" }[t]}
              </button>
            ))}
          </div>
        </div>

        {/* Provider (only for postgres) */}
        {addSvcType === "postgres" && (
          <div style={{ marginBottom: 16 }}>
            <label style={{ display: "block", marginBottom: 4, fontSize: 13, color: "var(--text-secondary)", fontWeight: 500 }}>
              Provider
            </label>
            <select
              value={addSvcProvider}
              onChange={(e) => setAddSvcProvider(e.target.value)}
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
              <option value="local">Local Docker</option>
              <option value="neon">Neon</option>
              <option value="dblab">DBLab</option>
              <option value="xata">Xata</option>
              <option value="postgres_template">PostgreSQL Template</option>
            </select>
          </div>
        )}

        {/* Name */}
        <div style={{ marginBottom: 12 }}>
          <label style={{ display: "block", marginBottom: 4, fontSize: 13, color: "var(--text-secondary)", fontWeight: 500 }}>
            Service Name
          </label>
          <input
            type="text"
            value={addSvcName}
            onChange={(e) => setAddSvcName(e.target.value)}
            placeholder="e.g. app-db"
            style={{ width: "100%" }}
          />
        </div>

        {/* Docker Image (only for local-ish providers) */}
        {(addSvcProvider === "local" || addSvcType !== "postgres") && (
          <div style={{ marginBottom: 12 }}>
            <label style={{ display: "block", marginBottom: 4, fontSize: 13, color: "var(--text-secondary)", fontWeight: 500 }}>
              Docker Image
            </label>
            <input
              type="text"
              value={addSvcImage}
              onChange={(e) => setAddSvcImage(e.target.value)}
              placeholder={addSvcType === "generic" ? "e.g. redis:7" : defaultImageForType(addSvcType)}
              style={{ width: "100%" }}
            />
          </div>
        )}

        {/* Seed From */}
        <div style={{ marginBottom: 12 }}>
          <label style={{ display: "block", marginBottom: 4, fontSize: 13, color: "var(--text-secondary)", fontWeight: 500 }}>
            Seed From <span style={{ fontWeight: 400, color: "var(--text-muted)" }}>(optional)</span>
          </label>
          <input
            type="text"
            value={addSvcSeed}
            onChange={(e) => setAddSvcSeed(e.target.value)}
            placeholder="URL, file path, or s3://..."
            style={{ width: "100%" }}
          />
        </div>

        {/* Auto-branch toggle */}
        <div style={{ marginBottom: 16 }}>
          <label style={{ display: "flex", alignItems: "center", gap: 8, fontSize: 14, cursor: "pointer" }}>
            <input
              type="checkbox"
              checked={addSvcAutoBranch}
              onChange={(e) => setAddSvcAutoBranch(e.target.checked)}
            />
            <span style={{ color: "var(--text-primary)" }}>Auto-branch on git checkout</span>
          </label>
        </div>

        {/* Error message */}
        {addSvcError && (
          <div style={{ marginBottom: 12, padding: "8px 12px", background: "var(--danger-bg, rgba(255,0,0,0.1))", border: "1px solid var(--danger)", borderRadius: 6, color: "var(--danger)", fontSize: 13 }}>
            {addSvcError}
          </div>
        )}

        {/* Footer */}
        <div style={{ display: "flex", gap: 8, justifyContent: "flex-end" }}>
          <button className="btn" onClick={() => setShowAddService(false)}>
            Cancel
          </button>
          <button
            className="btn btn-primary"
            onClick={handleAddService}
            disabled={!addSvcName.trim() || actionLoading === "add-service"}
          >
            {actionLoading === "add-service" ? "Adding..." : "Add Service"}
          </button>
        </div>
      </Modal>
    </div>
  );
}

function StatusBadge({ state }: { state: string | null }) {
  if (state === "running") {
    return <span className="badge badge-success">running</span>;
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
