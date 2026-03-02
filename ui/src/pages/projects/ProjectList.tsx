import { useState, useEffect } from "react";
import { Link } from "react-router-dom";
import { open } from "@tauri-apps/plugin-dialog";
import {
  listProjects,
  addProject,
  removeProject,
  initProject,
  getProjectDetail,
} from "../../utils/invoke";
import type { ProjectEntry, ProjectDetail } from "../../types";
import ConfirmDialog from "../../components/ConfirmDialog";
import Modal from "../../components/Modal";

interface ProjectRow extends ProjectEntry {
  detail?: ProjectDetail;
}

/** Normalize a project name: lowercase, collapse non-alnum to dashes, max 63 chars. */
function normalizeProjectName(raw: string): string {
  return raw
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 63);
}

/** Extract the directory basename from a full path. */
function basenameFromPath(path: string): string {
  const parts = path.replace(/[\\/]+$/, "").split(/[\\/]/);
  return parts[parts.length - 1] || "unknown";
}

function ProjectList() {
  const [projects, setProjects] = useState<ProjectRow[]>([]);
  const [removeTarget, setRemoveTarget] = useState<string | null>(null);

  // Modal state for Init / Add
  const [modalMode, setModalMode] = useState<"init" | "add" | null>(null);
  const [selectedPath, setSelectedPath] = useState("");
  const [projectName, setProjectName] = useState("");
  const [modalError, setModalError] = useState("");
  const [modalLoading, setModalLoading] = useState(false);

  const loadProjects = async () => {
    try {
      const list = await listProjects();
      const rows: ProjectRow[] = await Promise.all(
        list.map(async (p) => {
          try {
            const detail = await getProjectDetail(p.path);
            return { ...p, detail };
          } catch {
            return p;
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

  const openDirectoryThenModal = async (mode: "init" | "add") => {
    try {
      const selected = await open({ directory: true, multiple: false });
      if (!selected) return;
      const dirPath = selected as string;
      const defaultName = normalizeProjectName(basenameFromPath(dirPath));
      setSelectedPath(dirPath);
      setProjectName(defaultName);
      setModalError("");
      setModalMode(mode);
    } catch (e) {
      console.error("Failed to open directory picker:", e);
    }
  };

  const handleModalClose = () => {
    setModalMode(null);
    setSelectedPath("");
    setProjectName("");
    setModalError("");
    setModalLoading(false);
  };

  const handleModalSubmit = async () => {
    const normalized = normalizeProjectName(projectName);
    if (!normalized) {
      setModalError("Project name cannot be empty.");
      return;
    }

    setModalLoading(true);
    setModalError("");
    try {
      if (modalMode === "init") {
        await initProject(selectedPath, normalized);
      } else {
        await addProject(selectedPath, normalized);
      }
      handleModalClose();
      await loadProjects();
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
      await loadProjects();
    } catch (e) {
      alert(`Failed to remove: ${e}`);
    }
  };

  const normalized = normalizeProjectName(projectName);
  const nameChanged = projectName !== normalized && normalized.length > 0;

  return (
    <div>
      <div className="flex items-center justify-between mb-4">
        <h1 className="page-title" style={{ marginBottom: 0 }}>
          Projects
        </h1>
        <div className="flex gap-2">
          <button
            className="btn btn-primary"
            onClick={() => openDirectoryThenModal("add")}
          >
            Add Existing Project
          </button>
          <button
            className="btn"
            onClick={() => openDirectoryThenModal("init")}
          >
            Initialize New Project
          </button>
        </div>
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
              Add an existing devflow project or initialize a new one.
            </p>
          </div>
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>Name</th>
                <th>Path</th>
                <th>Branch</th>
                <th>Services</th>
                <th>Config</th>
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
                        color: "var(--accent)",
                        textDecoration: "none",
                        fontWeight: 500,
                      }}
                    >
                      {p.name}
                    </Link>
                  </td>
                  <td
                    className="mono"
                    style={{ color: "var(--text-secondary)", fontSize: 12 }}
                  >
                    {p.path}
                  </td>
                  <td>
                    {p.detail?.current_branch && (
                      <span className="badge badge-info">
                        {p.detail.current_branch}
                      </span>
                    )}
                  </td>
                  <td style={{ color: "var(--text-secondary)" }}>
                    {p.detail?.service_count ?? "-"}
                  </td>
                  <td>
                    {p.detail?.has_config ? (
                      <span className="badge badge-success">configured</span>
                    ) : (
                      <span className="badge badge-warning">no config</span>
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

      {/* Init / Add Project Modal */}
      <Modal
        open={modalMode !== null}
        onClose={handleModalClose}
        title={modalMode === "init" ? "Initialize New Project" : "Add Existing Project"}
        width={520}
      >
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
            maxLength={63}
            autoFocus
            style={{ width: "100%" }}
          />
        </div>

        {/* Normalized preview */}
        <div
          style={{
            minHeight: 22,
            marginBottom: 12,
            fontSize: 12,
            color: "var(--text-muted)",
          }}
        >
          {nameChanged && (
            <span>
              Will be saved as: <strong className="mono">{normalized}</strong>
            </span>
          )}
        </div>

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
            {modalLoading
              ? modalMode === "init"
                ? "Initializing..."
                : "Adding..."
              : modalMode === "init"
                ? "Initialize"
                : "Add Project"}
          </button>
        </div>
      </Modal>
    </div>
  );
}

export default ProjectList;
