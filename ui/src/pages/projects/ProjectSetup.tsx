import { useState, useEffect, useCallback } from "react";
import { useNavigate, useLocation } from "react-router-dom";
import AddServiceModal from "../../components/AddServiceModal";
import {
  addService,
  listServices,
  installVcsHooks,
  getProjectDetail,
} from "../../utils/invoke";
import type { ServiceEntry, AddServiceRequest, ProjectDetail } from "../../types";

type SetupStep = "services" | "integration" | "done";

function ProjectSetup() {
  const navigate = useNavigate();
  const location = useLocation();

  // Extract project path from URL — mirrors ProjectDetail pattern
  const pathSegments = location.pathname.replace("/projects/", "").replace("/setup", "");
  const projectPath = decodeURIComponent(pathSegments);

  const [step, setStep] = useState<SetupStep>("services");
  const [services, setServices] = useState<ServiceEntry[]>([]);
  const [showAddService, setShowAddService] = useState(false);
  const [detail, setDetail] = useState<ProjectDetail | null>(null);

  // Integration state
  const [hooksInstalled, setHooksInstalled] = useState(false);
  const [hooksLoading, setHooksLoading] = useState(false);
  const [hooksError, setHooksError] = useState<string | null>(null);
  const [shellCommand, setShellCommand] = useState<string>("");

  const loadData = useCallback(async () => {
    try {
      const [svcList, proj] = await Promise.all([
        listServices(projectPath),
        getProjectDetail(projectPath),
      ]);
      setServices(svcList);
      setDetail(proj);
    } catch (e) {
      console.error("Failed to load project data:", e);
    }
  }, [projectPath]);

  useEffect(() => {
    loadData();
    // Detect shell for eval command
    const shell = navigator.userAgent.toLowerCase().includes("mac") ? "zsh" : "bash";
    setShellCommand(`eval "$(devflow shell-init ${shell})"`);
  }, [loadData]);

  const handleAddService = async (request: AddServiceRequest) => {
    await addService(projectPath, request);
    await loadData();
  };

  const handleInstallHooks = async () => {
    setHooksLoading(true);
    setHooksError(null);
    try {
      await installVcsHooks(projectPath);
      setHooksInstalled(true);
    } catch (e) {
      setHooksError(`${e}`);
    } finally {
      setHooksLoading(false);
    }
  };

  const goToProject = () => {
    navigate(`/projects/${encodeURIComponent(projectPath)}`);
  };

  return (
    <div style={{ maxWidth: 600, margin: "0 auto" }}>
      <h1 className="page-title">Set Up Project</h1>
      <p style={{ color: "var(--text-secondary)", marginBottom: 24, fontSize: 14 }}>
        Configure services and integrations for{" "}
        <strong>{detail?.name || projectPath}</strong>
      </p>

      {/* Step indicator */}
      <div style={{ display: "flex", gap: 8, marginBottom: 32 }}>
        {(["services", "integration", "done"] as const).map((s, i) => (
          <div
            key={s}
            style={{
              flex: 1,
              height: 4,
              borderRadius: 2,
              background:
                (["services", "integration", "done"] as const).indexOf(step) >= i
                  ? "var(--accent)"
                  : "var(--border)",
              transition: "background 0.2s",
            }}
          />
        ))}
      </div>

      {/* Step 1: Services */}
      {step === "services" && (
        <div className="card" style={{ padding: 24 }}>
          <h2 style={{ fontSize: 18, marginBottom: 8 }}>Add Services</h2>
          <p style={{ color: "var(--text-secondary)", fontSize: 13, marginBottom: 16 }}>
            Add database or cache services. Each workspace gets its own isolated instance.
          </p>

          {services.length > 0 && (
            <div style={{ marginBottom: 16 }}>
              {services.map((svc) => (
                <div
                  key={svc.name}
                  style={{
                    display: "flex",
                    alignItems: "center",
                    gap: 8,
                    padding: "8px 12px",
                    background: "var(--bg-primary)",
                    border: "1px solid var(--border)",
                    borderRadius: 6,
                    marginBottom: 6,
                    fontSize: 13,
                  }}
                >
                  <span style={{ fontWeight: 600 }}>{svc.name}</span>
                  <span className="badge">{svc.service_type}</span>
                  <span className="badge" style={{ opacity: 0.7 }}>{svc.provider_type}</span>
                </div>
              ))}
            </div>
          )}

          <div style={{ display: "flex", gap: 8 }}>
            <button
              className="btn btn-primary"
              onClick={() => setShowAddService(true)}
            >
              {services.length === 0 ? "Add a Service" : "Add Another Service"}
            </button>
          </div>

          <div style={{ display: "flex", gap: 8, justifyContent: "flex-end", marginTop: 24 }}>
            <button className="btn" onClick={goToProject}>
              Skip Setup
            </button>
            <button className="btn btn-primary" onClick={() => setStep("integration")}>
              {services.length === 0 ? "Skip" : "Next"}
            </button>
          </div>
        </div>
      )}

      {/* Step 2: Integration */}
      {step === "integration" && (
        <div className="card" style={{ padding: 24 }}>
          <h2 style={{ fontSize: 18, marginBottom: 8 }}>Setup Integration</h2>
          <p style={{ color: "var(--text-secondary)", fontSize: 13, marginBottom: 20 }}>
            Configure Git hooks and shell integration for automatic workspace management.
          </p>

          {/* Git Hooks */}
          <div
            style={{
              padding: "12px 16px",
              background: "var(--bg-primary)",
              border: "1px solid var(--border)",
              borderRadius: 8,
              marginBottom: 12,
            }}
          >
            <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between" }}>
              <div>
                <div style={{ fontWeight: 600, fontSize: 14, marginBottom: 2 }}>Git Hooks</div>
                <div style={{ color: "var(--text-secondary)", fontSize: 12 }}>
                  Auto-sync services when you checkout a branch
                </div>
              </div>
              {hooksInstalled ? (
                <span className="badge badge-success">Installed</span>
              ) : (
                <button
                  className="btn btn-primary"
                  style={{ padding: "4px 16px", fontSize: 13 }}
                  onClick={handleInstallHooks}
                  disabled={hooksLoading}
                >
                  {hooksLoading ? "Installing..." : "Install"}
                </button>
              )}
            </div>
            {hooksError && (
              <div style={{ marginTop: 8, color: "var(--danger)", fontSize: 12 }}>
                {hooksError}
              </div>
            )}
          </div>

          {/* Shell Integration */}
          {detail?.worktree_enabled && (
            <div
              style={{
                padding: "12px 16px",
                background: "var(--bg-primary)",
                border: "1px solid var(--border)",
                borderRadius: 8,
                marginBottom: 12,
              }}
            >
              <div style={{ fontWeight: 600, fontSize: 14, marginBottom: 2 }}>Shell Integration</div>
              <div style={{ color: "var(--text-secondary)", fontSize: 12, marginBottom: 8 }}>
                Auto-cd into worktree directories when switching workspaces
              </div>
              <div
                className="mono"
                style={{
                  padding: "8px 12px",
                  background: "var(--bg-secondary)",
                  borderRadius: 4,
                  fontSize: 12,
                  userSelect: "all",
                  cursor: "text",
                }}
              >
                {shellCommand}
              </div>
              <div style={{ color: "var(--text-muted)", fontSize: 11, marginTop: 4 }}>
                Add this to your shell profile (~/.zshrc or ~/.bashrc)
              </div>
            </div>
          )}

          <div style={{ display: "flex", gap: 8, justifyContent: "flex-end", marginTop: 24 }}>
            <button className="btn" onClick={() => setStep("services")}>
              Back
            </button>
            <button className="btn btn-primary" onClick={() => setStep("done")}>
              Next
            </button>
          </div>
        </div>
      )}

      {/* Step 3: Done */}
      {step === "done" && (
        <div className="card" style={{ padding: 24, textAlign: "center" }}>
          <div style={{ fontSize: 32, marginBottom: 12 }}>&#10003;</div>
          <h2 style={{ fontSize: 18, marginBottom: 16 }}>Setup Complete</h2>

          <div
            style={{
              textAlign: "left",
              padding: "16px 20px",
              background: "var(--bg-primary)",
              border: "1px solid var(--border)",
              borderRadius: 8,
              marginBottom: 20,
              fontSize: 13,
            }}
          >
            <div style={{ marginBottom: 8 }}>
              <strong>Project:</strong>{" "}
              <span style={{ color: "var(--text-secondary)" }}>{detail?.name}</span>
            </div>
            <div style={{ marginBottom: 8 }}>
              <strong>Services:</strong>{" "}
              <span style={{ color: "var(--text-secondary)" }}>
                {services.length > 0
                  ? services.map((s) => `${s.name} (${s.service_type})`).join(", ")
                  : "none configured"}
              </span>
            </div>
            <div style={{ marginBottom: 8 }}>
              <strong>Hooks:</strong>{" "}
              <span style={{ color: "var(--text-secondary)" }}>
                {hooksInstalled ? "installed" : "not installed"}
              </span>
            </div>
            {detail?.worktree_enabled && (
              <div>
                <strong>Worktrees:</strong>{" "}
                <span style={{ color: "var(--text-secondary)" }}>enabled</span>
              </div>
            )}
          </div>

          <button className="btn btn-primary" onClick={goToProject} style={{ padding: "8px 24px" }}>
            Go to Project
          </button>
        </div>
      )}

      <AddServiceModal
        open={showAddService}
        onClose={() => setShowAddService(false)}
        onAdd={handleAddService}
        projectPath={projectPath}
      />
    </div>
  );
}

export default ProjectSetup;
