import { useState, useEffect, useCallback } from "react";
import { useParams, Link } from "react-router-dom";
import { runDoctor, installVcsHooks, uninstallVcsHooks, pruneWorktrees, installAgentSkills, uninstallAgentSkills, checkAgentSkills } from "../../utils/invoke";
import type { DoctorReport, DoctorServiceReport, AgentSkillsStatus } from "../../types";

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
  const [skillsLoading, setSkillsLoading] = useState<"install" | "uninstall" | "update" | null>(null);
  const [skillStatus, setSkillStatus] = useState<AgentSkillsStatus | null>(null);

  const reloadDoctor = useCallback(async () => {
    if (!projectPath) {
      setLoading(false);
      return;
    }

    setLoading(true);
    try {
      const [results, skills] = await Promise.all([
        runDoctor(projectPath),
        checkAgentSkills(projectPath).catch(() => null),
      ]);
      setReport(results);
      setSkillStatus(skills);
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

  const handleInstallSkills = async () => {
    if (!projectPath || skillsLoading) return;
    setHookActionError(null);
    setSkillsLoading("install");
    try {
      await installAgentSkills(projectPath);
      await reloadDoctor();
    } catch (e) {
      setHookActionError(`Failed to install agent skills: ${e}`);
    } finally {
      setSkillsLoading(null);
    }
  };

  const handleUninstallSkills = async () => {
    if (!projectPath || skillsLoading) return;
    setHookActionError(null);
    setSkillsLoading("uninstall");
    try {
      await uninstallAgentSkills(projectPath);
      await reloadDoctor();
    } catch (e) {
      setHookActionError(`Failed to uninstall agent skills: ${e}`);
    } finally {
      setSkillsLoading(null);
    }
  };

  const handleUpdateSkills = async () => {
    if (!projectPath || skillsLoading) return;
    setHookActionError(null);
    setSkillsLoading("update");
    try {
      await installAgentSkills(projectPath);
      await reloadDoctor();
    } catch (e) {
      setHookActionError(`Failed to update agent skills: ${e}`);
    } finally {
      setSkillsLoading(null);
    }
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
            const isSkillsCheck = showHookActions && check.name === "Agent skills";
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
                    ) : isSkillsCheck ? (
                      skillStatus?.update_available && skillStatus.installed_skills.length > 0 ? (
                        <span style={{ display: "inline-flex", gap: 8, alignItems: "center" }}>
                          <button
                            className="btn btn-primary"
                            onClick={handleUpdateSkills}
                            disabled={skillsLoading !== null}
                            style={{ padding: "4px 12px", fontSize: 12 }}
                          >
                            {skillsLoading === "update" ? "Updating..." : "Update"}
                          </button>
                          <button
                            className="btn-link"
                            onClick={handleUninstallSkills}
                            disabled={skillsLoading !== null}
                            style={{ fontSize: 12, color: "var(--text-muted)", background: "none", border: "none", cursor: "pointer", textDecoration: "underline" }}
                          >
                            {skillsLoading === "uninstall" ? "Removing..." : "Uninstall"}
                          </button>
                        </span>
                      ) : check.available ? (
                        <button
                          className="btn-link"
                          onClick={handleUninstallSkills}
                          disabled={skillsLoading !== null}
                          style={{ fontSize: 12, color: "var(--text-muted)", background: "none", border: "none", cursor: "pointer", textDecoration: "underline" }}
                        >
                          {skillsLoading === "uninstall" ? "Removing..." : "Uninstall"}
                        </button>
                      ) : (
                        <button
                          className="btn btn-primary"
                          onClick={handleInstallSkills}
                          disabled={skillsLoading !== null}
                          style={{ padding: "4px 12px", fontSize: 12 }}
                        >
                          {skillsLoading === "install" ? "Installing..." : "Install"}
                        </button>
                      )
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
