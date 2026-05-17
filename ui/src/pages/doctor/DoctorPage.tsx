import { useState, useEffect, useCallback } from "react";
import { useParams, Link } from "react-router-dom";
import { runDoctor, installVcsHooks, uninstallVcsHooks, pruneWorktrees, generateAgentGuides } from "../../utils/invoke";
import type { DoctorCheck, DoctorReport, DoctorServiceReport } from "../../types";

const summarizeChecks = (checks: DoctorCheck[]) => {
  const total = checks.length;
  const passed = checks.filter((check) => check.available).length;
  return { total, passed, failed: total - passed };
};

function DoctorPage() {
  const { "*": splat } = useParams();
  const projectPath = splat ? decodeURIComponent(splat) : "";

  const [report, setReport] = useState<DoctorReport | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [hookActionLoading, setHookActionLoading] = useState<
    "install" | "remove" | null
  >(null);
  const [hookActionError, setHookActionError] = useState<string | null>(null);
  const [pruneLoading, setPruneLoading] = useState(false);
  const [guidesGenerateLoading, setGuidesGenerateLoading] = useState(false);

  const reloadDoctor = useCallback(async () => {
    if (!projectPath) {
      setLoading(false);
      return;
    }

    setLoading(true);
    try {
      const results = await runDoctor(projectPath);
      setReport(results);
      setError(null);
    } catch (e) {
      setError(`Doctor failed: ${e}`);
    } finally {
      setLoading(false);
    }
  }, [projectPath]);

  useEffect(() => {
    reloadDoctor();
  }, [reloadDoctor]);

  const handleInstallHooks = async () => {
    if (!projectPath || hookActionLoading) return;
    setHookActionError(null);
    setHookActionLoading("install");
    try {
      await installVcsHooks(projectPath);
      await reloadDoctor();
    } catch (e) {
      setHookActionError(`Failed to install hooks: ${e}`);
    } finally {
      setHookActionLoading(null);
    }
  };

  const handleRemoveHooks = async () => {
    if (!projectPath || hookActionLoading) return;
    setHookActionError(null);
    setHookActionLoading("remove");
    try {
      await uninstallVcsHooks(projectPath);
      await reloadDoctor();
    } catch (e) {
      setHookActionError(`Failed to remove hooks: ${e}`);
    } finally {
      setHookActionLoading(null);
    }
  };

  const handlePruneWorktrees = async () => {
    if (!projectPath || pruneLoading) return;
    setPruneLoading(true);
    try {
      await pruneWorktrees(projectPath);
      await reloadDoctor();
    } catch (e) {
      setHookActionError(`Failed to prune worktrees: ${e}`);
    } finally {
      setPruneLoading(false);
    }
  };

  const handleGenerateGuides = async () => {
    if (!projectPath || guidesGenerateLoading) return;
    setHookActionError(null);
    setGuidesGenerateLoading(true);
    try {
      await generateAgentGuides(projectPath);
      await reloadDoctor();
    } catch (e) {
      setHookActionError(`Failed to generate agent guides: ${e}`);
    } finally {
      setGuidesGenerateLoading(false);
    }
  };

  const renderSummary = (report: DoctorReport) => {
    const general = summarizeChecks(report.general);
    const serviceChecks = report.services.flatMap((service) => service.checks);
    const services = summarizeChecks(serviceChecks);
    const total = general.total + services.total;
    const passed = general.passed + services.passed;
    const failed = total - passed;

    return (
      <div className="card" style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: 16 }}>
        <div>
          <div className="card-title" style={{ marginBottom: 4 }}>Health Summary</div>
          <div style={{ color: "var(--text-secondary)", fontSize: 13 }}>
            {passed}/{total} checks passing · {general.failed} setup issue{general.failed === 1 ? "" : "s"} · {services.failed} service issue{services.failed === 1 ? "" : "s"}
          </div>
          <div className="mono" style={{ color: "var(--text-muted)", fontSize: 12, marginTop: 4 }}>
            Automation gate: devflow --json doctor --strict · service-only: devflow --json service doctor --strict
          </div>
        </div>
        <span className={`badge ${failed === 0 ? "badge-success" : "badge-warning"}`} style={{ fontSize: 12 }}>
          {failed === 0 ? "All good" : `${failed} action${failed === 1 ? "" : "s"} needed`}
        </span>
      </div>
    );
  };

  const renderServiceReport = (
    entry: DoctorServiceReport,
    title: string,
    showHookActions = false
  ) => (
    <div className="card" key={entry.service}>
      <div className="card-title">{title}</div>
      <table className="table">
        <thead>
          <tr>
            <th style={{ width: 40 }}>Status</th>
            <th>Check</th>
            <th>Detail</th>
            {showHookActions && <th style={{ width: 120, textAlign: "right" }}>Actions</th>}
          </tr>
        </thead>
        <tbody>
          {entry.checks.map((check) => {
            const isHooksCheck = showHookActions && check.name === "VCS hooks";
            const isWorktreeCheck = showHookActions && check.name === "Worktree metadata";
            const isGuidesCheck = showHookActions && check.name === "Agent guides";
            return (
              <tr key={check.name}>
                <td>
                  <span
                    style={{
                      color: check.available ? "var(--success)" : "var(--danger)",
                      fontWeight: 600,
                      fontSize: 16,
                    }}
                  >
                    {check.available ? "\u2713" : "\u2717"}
                  </span>
                </td>
                <td style={{ fontWeight: 500 }}>{check.name}</td>
                <td
                  className="mono"
                  style={{
                    color: "var(--text-secondary)",
                    fontSize: 12,
                  }}
                >
                  {check.detail}
                </td>
                {showHookActions && (
                  <td style={{ textAlign: "right" }}>
                    {isHooksCheck ? (
                      check.available ? (
                        <button
                          className="btn-link"
                          onClick={handleRemoveHooks}
                          disabled={hookActionLoading !== null}
                          style={{ fontSize: 12, color: "var(--text-muted)", background: "none", border: "none", cursor: "pointer", textDecoration: "underline" }}
                        >
                          {hookActionLoading === "remove" ? "Removing..." : "Uninstall"}
                        </button>
                      ) : (
                        <button
                          className="btn btn-primary"
                          onClick={handleInstallHooks}
                          disabled={hookActionLoading !== null}
                          style={{ padding: "4px 12px", fontSize: 12 }}
                        >
                          {hookActionLoading === "install" ? "Installing..." : "Install"}
                        </button>
                      )
                    ) : isWorktreeCheck && !check.available ? (
                      <button
                        className="btn"
                        onClick={handlePruneWorktrees}
                        disabled={pruneLoading}
                        style={{ padding: "4px 10px", fontSize: 12 }}
                      >
                        {pruneLoading ? "Pruning..." : "Prune"}
                      </button>
                    ) : isGuidesCheck ? (
                      <button
                        className={check.available ? "btn" : "btn btn-primary"}
                        onClick={handleGenerateGuides}
                        disabled={guidesGenerateLoading}
                        style={{ padding: "4px 12px", fontSize: 12 }}
                      >
                        {guidesGenerateLoading ? "Generating..." : check.available ? "Regenerate" : "Generate"}
                      </button>
                    ) : (
                      <span style={{ color: "var(--text-muted)", fontSize: 12 }}>-</span>
                    )}
                  </td>
                )}
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );

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
      <h1 className="page-title">Setup</h1>
      <p
        className="mono"
        style={{ color: "var(--text-secondary)", marginBottom: 16 }}
      >
        {projectPath}
      </p>

      {loading && <div>Running diagnostics...</div>}

      {error && (
        <div className="card">
          <p style={{ color: "var(--danger)" }}>{error}</p>
        </div>
      )}

      {hookActionError && (
        <div className="card">
          <p style={{ color: "var(--danger)" }}>{hookActionError}</p>
        </div>
      )}

      {!loading && !error && report && renderSummary(report)}

      {!loading && !error && report &&
        renderServiceReport(
          { service: "__general__", checks: report.general },
          "General",
          true
        )}

      {!loading && !error && report && report.services.length === 0 && (
        <div className="card">
          <p style={{ color: "var(--text-secondary)" }}>
            No services configured for this project.
          </p>
        </div>
      )}

      {!loading && !error && report &&
        report.services.map((serviceReport) =>
          renderServiceReport(serviceReport, serviceReport.service)
        )}
    </div>
  );
}

export default DoctorPage;
