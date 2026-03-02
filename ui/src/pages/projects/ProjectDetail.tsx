import { useState, useEffect, useCallback } from "react";
import { useParams, Link, useNavigate } from "react-router-dom";
import {
  getProjectDetail,
  listBranches,
  listServices,
  addService,
  createBranch,
  switchBranch,
  deleteBranch,
  startService,
  stopService,
  resetService,
  getServiceLogs,
  getConnectionInfo,
  removeProject,
  destroyProject,
} from "../../utils/invoke";
import type {
  ProjectDetail as ProjectDetailType,
  BranchEntry,
  ServiceEntry,
  ConnectionInfo,
  AddServiceRequest,
} from "../../types";
import Modal from "../../components/Modal";
import ConfirmDialog from "../../components/ConfirmDialog";

function ProjectDetail() {
  const { "*": splat } = useParams();
  const projectPath = splat ? decodeURIComponent(splat) : "";
  const navigate = useNavigate();

  const [detail, setDetail] = useState<ProjectDetailType | null>(null);
  const [branches, setBranches] = useState<BranchEntry[]>([]);
  const [services, setServices] = useState<ServiceEntry[]>([]);
  const [currentBranch, setCurrentBranch] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  // Modal state
  const [showCreateBranch, setShowCreateBranch] = useState(false);
  const [newBranchName, setNewBranchName] = useState("");
  const [fromBranch, setFromBranch] = useState("");
  const [deletingBranch, setDeletingBranch] = useState<string | null>(null);
  const [connInfoBranch, setConnInfoBranch] = useState<string | null>(null);
  const [connInfo, setConnInfo] = useState<Record<string, ConnectionInfo>>({});
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

  // Loading state
  const [actionLoading, setActionLoading] = useState<string | null>(null);

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
    listBranches(projectPath)
      .then((b) => {
        setBranches(b.branches);
        setCurrentBranch(b.current);
      })
      .catch(() => {
        setBranches([]);
        setCurrentBranch(null);
      });
    listServices(projectPath)
      .then(setServices)
      .catch(() => setServices([]));
  }, [projectPath]);

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

  const handleSwitchBranch = async (name: string) => {
    setActionLoading(`switch:${name}`);
    try {
      await switchBranch(projectPath, name);
      await reload();
    } catch (e) {
      alert(`Failed to switch branch: ${e}`);
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

  const handleConnectionInfo = async (branchName: string) => {
    setConnInfoBranch(branchName);
    try {
      const infos: Record<string, ConnectionInfo> = {};
      for (const svc of services) {
        try {
          const info = await getConnectionInfo(
            projectPath,
            branchName,
            svc.name
          );
          infos[svc.name] = info as unknown as ConnectionInfo;
        } catch {
          // skip services without connection info
        }
      }
      setConnInfo(infos);
    } catch (e) {
      console.error(e);
    }
  };

  const handleStartService = async (svcName: string) => {
    if (!currentBranch) return;
    setActionLoading(`start:${svcName}`);
    try {
      await startService(projectPath, svcName, currentBranch);
    } catch (e) {
      alert(`Failed to start service: ${e}`);
    } finally {
      setActionLoading(null);
    }
  };

  const handleStopService = async (svcName: string) => {
    if (!currentBranch) return;
    setActionLoading(`stop:${svcName}`);
    try {
      await stopService(projectPath, svcName, currentBranch);
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
      setResetTarget(null);
    } catch (e) {
      alert(`Failed to reset service: ${e}`);
    } finally {
      setActionLoading(null);
    }
  };

  const handleViewLogs = async (svcName: string) => {
    if (!currentBranch) return;
    setLogService({ name: svcName, branch: currentBranch });
    setLogContent("Loading...");
    try {
      const logs = await getServiceLogs(projectPath, svcName, currentBranch);
      setLogContent(logs || "(no log output)");
    } catch (e) {
      setLogContent(`Error: ${e}`);
    }
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
            {currentBranch && (
              <span className="badge badge-info">{currentBranch}</span>
            )}
            {detail.has_config ? (
              <span className="badge badge-success">configured</span>
            ) : (
              <span className="badge badge-warning">no config</span>
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
            onClick={() => setShowCreateBranch(true)}
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
                <th>Status</th>
                <th>Worktree</th>
                <th style={{ textAlign: "right" }}>Actions</th>
              </tr>
            </thead>
            <tbody>
              {branches.map((b) => (
                <tr key={b.name}>
                  <td>
                    <span style={{ fontWeight: b.is_current ? 600 : 400 }}>
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
                  </td>
                  <td>
                    {b.is_current ? (
                      <span className="badge badge-success">current</span>
                    ) : null}
                  </td>
                  <td
                    className="mono"
                    style={{ color: "var(--text-muted)", fontSize: 12 }}
                  >
                    {b.worktree_path || "-"}
                  </td>
                  <td style={{ textAlign: "right" }}>
                    <div className="flex gap-2" style={{ justifyContent: "flex-end" }}>
                      {!b.is_current && (
                        <button
                          className="btn"
                          style={{ padding: "2px 10px", fontSize: 12 }}
                          onClick={() => handleSwitchBranch(b.name)}
                          disabled={actionLoading === `switch:${b.name}`}
                        >
                          {actionLoading === `switch:${b.name}`
                            ? "..."
                            : "Switch"}
                        </button>
                      )}
                      {detail.has_config && (
                        <button
                          className="btn"
                          style={{ padding: "2px 10px", fontSize: 12 }}
                          onClick={() => handleConnectionInfo(b.name)}
                        >
                          Connect
                        </button>
                      )}
                      {!b.is_current && !b.is_default && (
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
          <table className="table">
            <thead>
              <tr>
                <th>Name</th>
                <th>Type</th>
                <th>Provider</th>
                <th>Auto</th>
                <th style={{ textAlign: "right" }}>Actions</th>
              </tr>
            </thead>
            <tbody>
              {services.map((s) => (
                <tr key={s.name}>
                  <td style={{ fontWeight: 500 }}>{s.name}</td>
                  <td>{s.service_type}</td>
                  <td>{s.provider_type}</td>
                  <td>
                    {s.auto_branch ? (
                      <span className="badge badge-success">yes</span>
                    ) : (
                      <span className="badge badge-warning">no</span>
                    )}
                  </td>
                  <td style={{ textAlign: "right" }}>
                    <div
                      className="flex gap-2"
                      style={{ justifyContent: "flex-end" }}
                    >
                      <button
                        className="btn"
                        style={{ padding: "2px 10px", fontSize: 12 }}
                        onClick={() => handleStartService(s.name)}
                        disabled={
                          !currentBranch ||
                          actionLoading === `start:${s.name}`
                        }
                      >
                        {actionLoading === `start:${s.name}`
                          ? "..."
                          : "Start"}
                      </button>
                      <button
                        className="btn"
                        style={{ padding: "2px 10px", fontSize: 12 }}
                        onClick={() => handleStopService(s.name)}
                        disabled={
                          !currentBranch ||
                          actionLoading === `stop:${s.name}`
                        }
                      >
                        {actionLoading === `stop:${s.name}` ? "..." : "Stop"}
                      </button>
                      <button
                        className="btn"
                        style={{ padding: "2px 10px", fontSize: 12 }}
                        onClick={() =>
                          currentBranch &&
                          setResetTarget({
                            name: s.name,
                            branch: currentBranch,
                          })
                        }
                        disabled={!currentBranch}
                      >
                        Reset
                      </button>
                      <button
                        className="btn"
                        style={{ padding: "2px 10px", fontSize: 12 }}
                        onClick={() => handleViewLogs(s.name)}
                        disabled={!currentBranch}
                      >
                        Logs
                      </button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>

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
            <option value="">Current branch</option>
            {branches.map((b) => (
              <option key={b.name} value={b.name}>
                {b.name}
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
                  <ConnRow label="Host" value={info.host} />
                  <ConnRow label="Port" value={String(info.port)} />
                  <ConnRow label="Database" value={info.database} />
                  <ConnRow label="User" value={info.user} />
                  <ConnRow
                    label="Password"
                    value={info.password || "-"}
                    secret
                  />
                  {info.connection_string && (
                    <ConnRow
                      label="Connection String"
                      value={info.connection_string}
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
