import { useState, useEffect } from "react";
import { Link, useNavigate } from "react-router-dom";
import { open } from "@tauri-apps/plugin-dialog";
import {
  listProjects,
  removeProject,
  addOrInitProject,
  getProjectDetail,
  detectVcsInfo,
  detectGitBranches,
} from "../../utils/invoke";
import type { ProjectEntry, ProjectDetail, VcsInfo, GitBranchInfo } from "../../types";
import ConfirmDialog from "../../components/ConfirmDialog";
import Modal from "../../components/Modal";

interface ProjectRow extends ProjectEntry {
  detail?: ProjectDetail;
  missing?: boolean;
}

/** Minimal sanitization: trim whitespace and cap length. */
function sanitizeProjectName(raw: string): string {
  return raw.trim().slice(0, 100);
}

/** Extract the directory basename from a full path. */
function basenameFromPath(path: string): string {
  const parts = path.replace(/[\\/]+$/, "").split(/[\\/]/);
  return parts[parts.length - 1] || "unknown";
}

function ProjectList() {
  const navigate = useNavigate();
  const [projects, setProjects] = useState<ProjectRow[]>([]);
  const [removeTarget, setRemoveTarget] = useState<string | null>(null);

  // Unified modal state
  const [modalOpen, setModalOpen] = useState(false);
  const [selectedPath, setSelectedPath] = useState("");
  const [projectName, setProjectName] = useState("");
  const [modalError, setModalError] = useState("");
  const [modalLoading, setModalLoading] = useState(false);

  // Detection state
  const [vcsInfo, setVcsInfo] = useState<VcsInfo | null>(null);
  const [hasExistingConfig, setHasExistingConfig] = useState(false);
  const [hasWorktreeConfig, setHasWorktreeConfig] = useState(false);

  // Branch detection state
  const [branchInfo, setBranchInfo] = useState<GitBranchInfo | null>(null);
  const [selectedBranch, setSelectedBranch] = useState<string>("");
  const [branchSearch, setBranchSearch] = useState("");

  // User selections
  const [selectedVcs, setSelectedVcs] = useState<string>("git");
  const [branchingMode, setBranchingMode] = useState<"worktree" | "workspace">("worktree");

  const loadProjects = async () => {
    try {
      const list = await listProjects();
      const rows: ProjectRow[] = await Promise.all(
        list.map(async (p) => {
          try {
            const detail = await getProjectDetail(p.path);
            return { ...p, detail };
          } catch {
            return { ...p, missing: true };
          }
        })
      );
      setProjects(rows);
    } catch (e) {
      console.error(e);
    }
  };

  useEffect(() => {
    loadProjects();
  }, []);

  const openAddProjectModal = async () => {
    try {
      const selected = await open({ directory: true, multiple: false });
      if (!selected) return;
      const dirPath = selected as string;
      const defaultName = sanitizeProjectName(basenameFromPath(dirPath));
      setSelectedPath(dirPath);
      setProjectName(defaultName);
      setModalError("");

      // Detect VCS info
      let detectedVcsInfo: VcsInfo | null = null;
      try {
        detectedVcsInfo = await detectVcsInfo(dirPath);
        setVcsInfo(detectedVcsInfo);
        setSelectedVcs(detectedVcsInfo.existing_vcs || detectedVcsInfo.available_tools[0] || "git");
      } catch {
        setVcsInfo(null);
        setSelectedVcs("git");
      }

      // Detect branches if VCS exists
      if (detectedVcsInfo?.existing_vcs) {
        try {
          const info = await detectGitBranches(dirPath);
          setBranchInfo(info);
          setSelectedBranch(info.default_branch || "");
        } catch {
          setBranchInfo(null);
        }
      }

      // Detect existing config
      try {
        const detail = await getProjectDetail(dirPath);
        setHasExistingConfig(detail.has_config);
        setHasWorktreeConfig(detail.worktree_enabled);
        // Default branching mode based on existing config
        if (detail.has_config && detail.worktree_enabled) {
          setBranchingMode("worktree");
        }
      } catch {
        setHasExistingConfig(false);
        setHasWorktreeConfig(false);
      }

      setModalOpen(true);
    } catch (e) {
      console.error("Failed to open directory picker:", e);
    }
  };

  const handleModalClose = () => {
    setModalOpen(false);
    setSelectedPath("");
    setProjectName("");
    setModalError("");
    setModalLoading(false);
    setVcsInfo(null);
    setSelectedVcs("git");
    setBranchingMode("worktree");
    setHasExistingConfig(false);
    setHasWorktreeConfig(false);
    setBranchInfo(null);
    setSelectedBranch("");
    setBranchSearch("");
  };

  const handleModalSubmit = async () => {
    const normalized = sanitizeProjectName(projectName);
    if (!normalized) {
      setModalError("Project name cannot be empty.");
      return;
    }

    setModalLoading(true);
    setModalError("");
    try {
      // Pass VCS preference only when no VCS already exists
      const vcsPref = vcsInfo?.existing_vcs ? undefined : selectedVcs;
      await addOrInitProject(selectedPath, normalized, vcsPref, branchingMode === "worktree", selectedBranch || undefined);
      window.dispatchEvent(new CustomEvent("devflow:projects-changed"));
      const projectDetailPath = `/projects/${encodeURIComponent(selectedPath)}`;
      handleModalClose();
      navigate(projectDetailPath);
    } catch (e) {
      setModalError(`${e}`);
    } finally {
      setModalLoading(false);
    }
  };

  const handleRemove = async () => {
    if (!removeTarget) return;
    try {
      await removeProject(removeTarget);
      setRemoveTarget(null);
      window.dispatchEvent(new CustomEvent("devflow:projects-changed"));
      await loadProjects();
    } catch (e) {
      alert(`Failed to remove: ${e}`);
    }
  };

  const normalized = sanitizeProjectName(projectName);

  // Determine which sections to show in the modal
  const showVcsSelector = vcsInfo && !vcsInfo.existing_vcs && !hasExistingConfig;
  const showWorktreeSelector = !hasExistingConfig || (hasExistingConfig && !hasWorktreeConfig);

  return (
    <div>
      <div className="flex items-center justify-between mb-4">
        <h1 className="page-title" style={{ marginBottom: 0 }}>
          Projects
        </h1>
        <button
          className="btn btn-primary"
          onClick={openAddProjectModal}
        >
          Add Project
        </button>
      </div>

      <div className="card">
        {projects.length === 0 ? (
          <div style={{ textAlign: "center", padding: 32 }}>
            <p
              style={{
                color: "var(--text-secondary)",
                marginBottom: 8,
                fontSize: 16,
              }}
            >
              No projects registered yet.
            </p>
            <p style={{ color: "var(--text-muted)", fontSize: 13 }}>
              Select a directory to add a new or existing devflow project.
            </p>
          </div>
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>Name</th>
                <th>Path</th>
                <th>Workspace</th>
                <th>Services</th>
                <th>VCS</th>
                <th>Default Mode</th>
                <th>Actions</th>
              </tr>
            </thead>
            <tbody>
              {projects.map((p) => (
                <tr key={p.path}>
                  <td>
                    <Link
                      to={`/projects/${encodeURIComponent(p.path)}`}
                      style={{
                        color: p.missing ? "var(--text-muted)" : "var(--accent)",
                        textDecoration: "none",
                        fontWeight: 500,
                      }}
                    >
                      {p.name}
                    </Link>
                    {p.missing && (
                      <span
                        className="badge badge-danger"
                        style={{ marginLeft: 8 }}
                      >
                        missing
                      </span>
                    )}
                  </td>
                  <td
                    className="mono"
                    style={{ color: "var(--text-secondary)", fontSize: 12 }}
                  >
                    {p.path}
                  </td>
                  <td>
                    {p.detail?.current_workspace && !p.detail?.worktree_enabled && (
                      <span className="badge" style={{ opacity: 0.7 }}>
                        active: {p.detail.current_workspace}
                      </span>
                    )}
                  </td>
                  <td style={{ color: "var(--text-secondary)" }}>
                    {p.detail?.service_count ?? "-"}
                  </td>
                  <td>
                    {p.detail?.vcs_type ? (
                      <span className="badge">{p.detail.vcs_type}</span>
                    ) : (
                      <span style={{ color: "var(--text-muted)" }}>-</span>
                    )}
                  </td>
                  <td>
                    {p.detail?.worktree_enabled ? (
                      <span
                        className="badge badge-info"
                        title="Default creation mode. You can still choose branch or worktree when creating a workspace."
                      >
                        worktree
                      </span>
                    ) : p.detail?.has_config ? (
                      <span
                        className="badge"
                        title="Default creation mode. You can still choose branch or worktree when creating a workspace."
                      >
                        branch
                      </span>
                    ) : (
                      <span style={{ color: "var(--text-muted)" }}>-</span>
                    )}
                  </td>
                  <td>
                    <button
                      className="btn btn-danger"
                      onClick={() => setRemoveTarget(p.path)}
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

      {/* Remove Project Confirmation */}
      <ConfirmDialog
        open={removeTarget !== null}
        onClose={() => setRemoveTarget(null)}
        onConfirm={handleRemove}
        title="Remove Project"
        message="Remove this project from devflow? This only unregisters it — no files will be deleted."
        confirmLabel="Remove"
        danger
      />

      {/* Unified Add Project Modal */}
      <Modal
        open={modalOpen}
        onClose={handleModalClose}
        title="Add Project"
        width={520}
      >
        {/* Existing config notice */}
        {hasExistingConfig && (
          <div
            style={{
              marginBottom: 16,
              padding: "8px 12px",
              background: "var(--info-bg, rgba(0,120,255,0.08))",
              border: "1px solid var(--info, #3b82f6)",
              borderRadius: 6,
              fontSize: 13,
              color: "var(--text-secondary)",
            }}
          >
            Existing <strong className="mono">.devflow.yml</strong> detected — your configuration will be preserved.
          </div>
        )}

        {/* Directory */}
        <div style={{ marginBottom: 16 }}>
          <label
            style={{
              display: "block",
              marginBottom: 6,
              fontSize: 13,
              color: "var(--text-secondary)",
              fontWeight: 500,
            }}
          >
            Directory
          </label>
          <div
            className="mono"
            style={{
              padding: "8px 12px",
              background: "var(--bg-primary)",
              border: "1px solid var(--border)",
              borderRadius: 6,
              fontSize: 13,
              color: "var(--text-primary)",
              wordBreak: "break-all",
            }}
          >
            {selectedPath}
          </div>
        </div>

        {/* Project Name */}
        <div style={{ marginBottom: 4 }}>
          <label
            style={{
              display: "block",
              marginBottom: 6,
              fontSize: 13,
              color: "var(--text-secondary)",
              fontWeight: 500,
            }}
          >
            Project Name
          </label>
          <input
            type="text"
            value={projectName}
            onChange={(e) => {
              setProjectName(e.target.value);
              setModalError("");
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !modalLoading) handleModalSubmit();
            }}
            placeholder="e.g. my-project"
            maxLength={100}
            autoFocus
            style={{ width: "100%" }}
          />
        </div>

        {/* VCS selection (only when no VCS exists and no config) */}
        {showVcsSelector && vcsInfo && vcsInfo.available_tools.length > 1 && (
          <div style={{ marginBottom: 16 }}>
            <label
              style={{
                display: "block",
                marginBottom: 6,
                fontSize: 13,
                color: "var(--text-secondary)",
                fontWeight: 500,
              }}
            >
              Version Control
            </label>
            <div className="flex gap-2">
              {vcsInfo.available_tools.map((tool) => (
                <button
                  key={tool}
                  className={`btn${selectedVcs === tool ? " btn-primary" : ""}`}
                  style={{ padding: "4px 16px", fontSize: 13 }}
                  onClick={() => setSelectedVcs(tool)}
                  type="button"
                >
                  {tool}
                </button>
              ))}
            </div>
          </div>
        )}

        {/* VCS info display (when VCS already exists) */}
        {vcsInfo?.existing_vcs && (
          <div style={{ marginBottom: 16 }}>
            <label
              style={{
                display: "block",
                marginBottom: 6,
                fontSize: 13,
                color: "var(--text-secondary)",
                fontWeight: 500,
              }}
            >
              Version Control
            </label>
            <div
              style={{
                display: "flex",
                alignItems: "center",
                gap: 8,
                fontSize: 13,
              }}
            >
              <span className="badge badge-info">
                {vcsInfo.existing_vcs}
              </span>
              <span style={{ color: "var(--text-muted)" }}>
                already initialized
              </span>
            </div>
          </div>
        )}

        {/* Main branch selector (when VCS exists and branches detected) */}
        {vcsInfo?.existing_vcs && branchInfo && (
          <div style={{ marginBottom: 16 }}>
            <label
              style={{
                display: "block",
                marginBottom: 6,
                fontSize: 13,
                color: "var(--text-secondary)",
                fontWeight: 500,
              }}
            >
              Main Branch
            </label>
            {branchInfo.branches.length > 0 ? (
              <>
                {branchInfo.branches.length > 5 && (
                  <input
                    type="text"
                    value={branchSearch}
                    onChange={(e) => setBranchSearch(e.target.value)}
                    placeholder="Search branches..."
                    style={{ width: "100%", marginBottom: 6, fontSize: 13 }}
                  />
                )}
                <div
                  style={{
                    maxHeight: 140,
                    overflowY: "auto",
                    border: "1px solid var(--border)",
                    borderRadius: 6,
                    background: "var(--bg-primary)",
                  }}
                >
                  {branchInfo.branches
                    .filter((b) =>
                      !branchSearch || b.toLowerCase().includes(branchSearch.toLowerCase())
                    )
                    .map((branch) => {
                      const isDefault = branch === branchInfo.default_branch;
                      const isSelected = branch === selectedBranch;
                      return (
                        <div
                          key={branch}
                          onClick={() => setSelectedBranch(branch)}
                          style={{
                            padding: "6px 10px",
                            cursor: "pointer",
                            fontSize: 13,
                            display: "flex",
                            alignItems: "center",
                            gap: 8,
                            background: isSelected
                              ? "var(--accent-bg, rgba(59,130,246,0.12))"
                              : "transparent",
                            borderBottom: "1px solid var(--border)",
                          }}
                        >
                          <span
                            className="mono"
                            style={{
                              fontWeight: isSelected ? 600 : 400,
                              color: isSelected
                                ? "var(--accent)"
                                : "var(--text-primary)",
                            }}
                          >
                            {branch}
                          </span>
                          {isDefault && (
                            <span
                              className="badge badge-info"
                              style={{ fontSize: 10, padding: "1px 6px" }}
                            >
                              detected
                            </span>
                          )}
                        </div>
                      );
                    })}
                </div>
              </>
            ) : (
              <input
                type="text"
                value={selectedBranch}
                onChange={(e) => setSelectedBranch(e.target.value)}
                placeholder="e.g. main"
                style={{ width: "100%", fontSize: 13 }}
              />
            )}
          </div>
        )}

        {/* Branching mode (show when no config or config without worktree) */}
        {showWorktreeSelector && (
          <div style={{ marginBottom: 16 }}>
            <label
              style={{
                display: "block",
                marginBottom: 6,
                fontSize: 13,
                color: "var(--text-secondary)",
                fontWeight: 500,
              }}
            >
              Workspace Mode
            </label>
            <div className="flex gap-2">
              <button
                className={`btn${branchingMode === "worktree" ? " btn-primary" : ""}`}
                style={{ padding: "4px 16px", fontSize: 13 }}
                onClick={() => setBranchingMode("worktree")}
                type="button"
              >
                worktree
              </button>
              <button
                className={`btn${branchingMode === "workspace" ? " btn-primary" : ""}`}
                style={{ padding: "4px 16px", fontSize: 13 }}
                onClick={() => setBranchingMode("workspace")}
                type="button"
              >
                checkout
              </button>
            </div>
            <div
              style={{
                marginTop: 6,
                fontSize: 12,
                color: "var(--text-muted)",
              }}
            >
              {branchingMode === "worktree"
                ? "Each workspace gets its own directory — parallel work without stashing."
                : `Branches share a single directory — switches in place like ${
                    (vcsInfo?.existing_vcs || selectedVcs) === "jj" ? "jj edit" : "git checkout"
                  }.`}
            </div>
          </div>
        )}

        {/* Error */}
        {modalError && (
          <div
            style={{
              marginBottom: 12,
              padding: "8px 12px",
              background: "var(--danger-bg, rgba(255,0,0,0.1))",
              border: "1px solid var(--danger)",
              borderRadius: 6,
              color: "var(--danger)",
              fontSize: 13,
            }}
          >
            {modalError}
          </div>
        )}

        {/* Footer */}
        <div style={{ display: "flex", gap: 8, justifyContent: "flex-end" }}>
          <button className="btn" onClick={handleModalClose}>
            Cancel
          </button>
          <button
            className="btn btn-primary"
            onClick={handleModalSubmit}
            disabled={!normalized || modalLoading}
          >
            {modalLoading ? "Adding..." : "Add Project"}
          </button>
        </div>
      </Modal>
    </div>
  );
}

export default ProjectList;
