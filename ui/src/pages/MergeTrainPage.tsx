import { useState, useEffect, useCallback } from "react";
import {
  listProjects,
  trainStatus,
  trainAdd,
  trainRemove,
  trainRun,
  trainPause,
  trainResume,
  listWorkspaces,
  getConfigJson,
} from "../utils/invoke";
import type {
  ProjectEntry,
  MergeTrain,
  MergeTrainEntry,
  WorkspaceEntry,
} from "../types";
import type { MergeConfig } from "../types/config";

function MergeTrainPage() {
  const [projects, setProjects] = useState<ProjectEntry[]>([]);
  const [selectedProject, setSelectedProject] = useState<string>("");
  const [train, setTrain] = useState<MergeTrain | null>(null);
  const [workspaces, setWorkspaces] = useState<WorkspaceEntry[]>([]);
  const [selectedWorkspace, setSelectedWorkspace] = useState<string>("");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [runResults, setRunResults] = useState<MergeTrainEntry[] | null>(null);
  const [mergeConfig, setMergeConfig] = useState<MergeConfig | null>(null);
  const [overrideCleanup, setOverrideCleanup] = useState<boolean | null>(null);
  const [overrideStopOnFailure, setOverrideStopOnFailure] = useState<boolean>(true);

  useEffect(() => {
    listProjects().then(setProjects).catch(() => {});
  }, []);

  const refreshTrain = useCallback(async () => {
    if (!selectedProject) return;
    try {
      const t = await trainStatus(selectedProject);
      setTrain(t);
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  }, [selectedProject]);

  useEffect(() => {
    if (selectedProject) {
      refreshTrain();
      listWorkspaces(selectedProject)
        .then((resp) => setWorkspaces(resp.workspaces))
        .catch(() => {});
      getConfigJson(selectedProject)
        .then((cfg) => {
          setMergeConfig(cfg.merge ?? null);
          setOverrideCleanup(null);
        })
        .catch(() => setMergeConfig(null));
    }
  }, [selectedProject, refreshTrain]);

  const handleAdd = async () => {
    if (!selectedProject || !selectedWorkspace) return;
    setLoading(true);
    try {
      await trainAdd(selectedProject, selectedWorkspace);
      await refreshTrain();
      setSelectedWorkspace("");
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  };

  const handleRemove = async (workspace: string) => {
    if (!selectedProject) return;
    setLoading(true);
    try {
      await trainRemove(selectedProject, workspace);
      await refreshTrain();
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  };

  const handleRun = async () => {
    if (!selectedProject) return;
    setLoading(true);
    setRunResults(null);
    try {
      const cleanupFlag = overrideCleanup ?? undefined;
      const results = await trainRun(
        selectedProject,
        undefined,
        overrideStopOnFailure,
        cleanupFlag
      );
      setRunResults(results);
      await refreshTrain();
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  };

  const handlePause = async () => {
    if (!selectedProject) return;
    try {
      await trainPause(selectedProject);
      await refreshTrain();
    } catch (e) {
      setError(String(e));
    }
  };

  const handleResume = async () => {
    if (!selectedProject) return;
    try {
      await trainResume(selectedProject);
      await refreshTrain();
    } catch (e) {
      setError(String(e));
    }
  };

  const statusBadge = (status: string) => {
    const map: Record<string, string> = {
      queued: "badge-muted",
      checking: "badge-info",
      merging: "badge-warning",
      succeeded: "badge-success",
      failed: "badge-danger",
      "needs-rebase": "badge-warning",
      cancelled: "badge-muted",
    };
    return (
      <span className={`badge ${map[status] || "badge-muted"}`}>{status}</span>
    );
  };

  const availableWorkspaces = workspaces.filter(
    (w) =>
      !w.is_default &&
      !train?.entries.some((e) => e.workspace === w.name)
  );

  return (
    <div>
      <h1 className="page-title">Merge Train</h1>
      <p style={{ color: "var(--text-secondary)", marginBottom: 16 }}>
        Queue workspaces for sequential merge into a target with readiness
        checks.
      </p>

      {error && (
        <div
          style={{
            padding: 12,
            marginBottom: 16,
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

      <div className="card">
        <div className="card-title">Project</div>
        <select
          value={selectedProject}
          onChange={(e) => {
            setSelectedProject(e.target.value);
            setTrain(null);
            setRunResults(null);
          }}
          style={{ width: "100%" }}
        >
          <option value="">Select a project...</option>
          {projects.map((p) => (
            <option key={p.path} value={p.path}>
              {p.name}
            </option>
          ))}
        </select>
      </div>

      {selectedProject && (
        <>
          {/* Merge config summary */}
          <div className="card">
            <div className="card-title">Merge Settings</div>
            <div className="flex gap-2 items-center" style={{ flexWrap: "wrap" }}>
              <span className="badge">
                Strategy: {mergeConfig?.strategy || "merge"}
              </span>
              <span className="badge">
                Cleanup: {mergeConfig?.cleanup_after_merge ? "on" : "off"}
              </span>
              <span className="badge">
                Cascade rebase: {mergeConfig?.cascade_rebase ? "on" : "off"}
              </span>
              <span className="badge">
                Checks: {mergeConfig?.checks?.length ?? 0}
              </span>
            </div>
            <div style={{ marginTop: 12, display: "flex", gap: 16, alignItems: "center", fontSize: 13 }}>
              <label style={{ display: "flex", alignItems: "center", gap: 6, cursor: "pointer" }}>
                <input
                  type="checkbox"
                  checked={overrideCleanup ?? mergeConfig?.cleanup_after_merge ?? false}
                  onChange={(e) => setOverrideCleanup(e.target.checked)}
                />
                Cleanup after merge
              </label>
              <label style={{ display: "flex", alignItems: "center", gap: 6, cursor: "pointer" }}>
                <input
                  type="checkbox"
                  checked={overrideStopOnFailure}
                  onChange={(e) => setOverrideStopOnFailure(e.target.checked)}
                />
                Stop on failure
              </label>
              {overrideCleanup !== null && (
                <button
                  className="btn"
                  onClick={() => setOverrideCleanup(null)}
                  style={{ fontSize: 11, padding: "2px 8px" }}
                >
                  Reset overrides
                </button>
              )}
            </div>
          </div>

          <div className="card">
            <div className="card-title">Add Workspace</div>
            <div className="flex gap-2 items-center">
              <select
                value={selectedWorkspace}
                onChange={(e) => setSelectedWorkspace(e.target.value)}
                style={{ flex: 1 }}
              >
                <option value="">Select workspace...</option>
                {availableWorkspaces.map((w) => (
                  <option key={w.name} value={w.name}>
                    {w.name}
                  </option>
                ))}
              </select>
              <button
                className="btn btn-primary"
                onClick={handleAdd}
                disabled={!selectedWorkspace || loading}
              >
                Add to Train
              </button>
            </div>
          </div>

          <div className="card">
            <div
              className="flex items-center justify-between"
              style={{ marginBottom: 12 }}
            >
              <div className="card-title" style={{ margin: 0 }}>
                Queue
                {train && (
                  <span
                    className="badge"
                    style={{ marginLeft: 8, fontWeight: 400 }}
                  >
                    {train.status}
                  </span>
                )}
              </div>
              {train && (
                <div className="flex gap-2">
                  {train.status === "paused" ? (
                    <button className="btn" onClick={handleResume}>
                      Resume
                    </button>
                  ) : (
                    <button className="btn" onClick={handlePause}>
                      Pause
                    </button>
                  )}
                  <button
                    className="btn btn-primary"
                    onClick={handleRun}
                    disabled={
                      loading ||
                      train.status === "paused" ||
                      train.entries.length === 0
                    }
                  >
                    {loading ? "Running..." : "Run Train"}
                  </button>
                </div>
              )}
            </div>

            {!train || train.entries.length === 0 ? (
              <p style={{ color: "var(--text-muted)" }}>
                No workspaces in the queue.
              </p>
            ) : (
              <table className="table">
                <thead>
                  <tr>
                    <th>#</th>
                    <th>Workspace</th>
                    <th>Status</th>
                    <th>Error</th>
                    <th style={{ textAlign: "right" }}>Actions</th>
                  </tr>
                </thead>
                <tbody>
                  {train.entries.map((entry) => (
                    <tr key={entry.workspace}>
                      <td>{entry.position + 1}</td>
                      <td style={{ fontWeight: 500 }}>{entry.workspace}</td>
                      <td>{statusBadge(entry.status)}</td>
                      <td
                        style={{
                          color: "var(--danger)",
                          fontSize: 12,
                        }}
                      >
                        {entry.error || ""}
                      </td>
                      <td style={{ textAlign: "right" }}>
                        <button
                          className="btn btn-danger"
                          onClick={() => handleRemove(entry.workspace)}
                          disabled={loading}
                          style={{ padding: "4px 10px", fontSize: 12 }}
                        >
                          Remove
                        </button>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </div>

          {runResults && (
            <div className="card">
              <div className="card-title">Run Results</div>
              {runResults.map((r) => (
                <div
                  key={r.workspace}
                  style={{
                    padding: "8px 12px",
                    marginBottom: 4,
                    background:
                      r.status === "succeeded"
                        ? "rgba(63, 185, 80, 0.1)"
                        : "rgba(248, 81, 73, 0.1)",
                    border: `1px solid ${r.status === "succeeded" ? "var(--success)" : "var(--danger)"}`,
                    borderRadius: 6,
                    fontSize: 13,
                  }}
                >
                  <strong>{r.workspace}</strong> {statusBadge(r.status)}
                  {r.error && (
                    <span
                      style={{
                        marginLeft: 8,
                        fontSize: 12,
                        color: "var(--danger)",
                      }}
                    >
                      {r.error}
                    </span>
                  )}
                </div>
              ))}
            </div>
          )}
        </>
      )}
    </div>
  );
}

export default MergeTrainPage;
