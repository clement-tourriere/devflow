import { useState, useEffect, useRef } from "react";
import { Link, useNavigate } from "react-router-dom";
import {
  listProjects,
  removeProject,
  getProjectDetail,
} from "../../utils/invoke";
import type { ProjectEntry, ProjectDetail } from "../../types";
import ConfirmDialog from "../../components/ConfirmDialog";
import AddProjectModal, { type AddProjectModalHandle } from "../../components/AddProjectModal";
import { sortByRecent } from "../../utils/recentProjects";

interface ProjectRow extends ProjectEntry {
  detail?: ProjectDetail;
  missing?: boolean;
}

function ProjectList() {
  const navigate = useNavigate();
  const [projects, setProjects] = useState<ProjectRow[]>([]);
  const [removeTarget, setRemoveTarget] = useState<string | null>(null);
  const [search, setSearch] = useState("");
  const addModalRef = useRef<AddProjectModalHandle>(null);

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

  const handleProjectAdded = (projectPath: string) => {
    navigate(`/projects/${encodeURIComponent(projectPath)}`);
  };

  const sorted = sortByRecent(projects, (p) => p.path);
  const filtered = sorted.filter((p) => {
    if (!search) return true;
    const q = search.toLowerCase();
    return p.name.toLowerCase().includes(q) || p.path.toLowerCase().includes(q);
  });

  return (
    <div>
      <div className="flex items-center justify-between mb-4">
        <h1 className="page-title" style={{ marginBottom: 0 }}>
          Projects
        </h1>
        <div className="flex items-center gap-2">
          <input
            type="text"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            placeholder="Search projects..."
            className="search-input"
          />
          <button
            className="btn btn-primary"
            onClick={() => addModalRef.current?.open()}
          >
            Add Project
          </button>
        </div>
      </div>

      {projects.length === 0 ? (
        <div className="card" style={{ textAlign: "center", padding: 32 }}>
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
      ) : filtered.length === 0 ? (
        <div className="card" style={{ textAlign: "center", padding: 32 }}>
          <p style={{ color: "var(--text-muted)", fontSize: 14 }}>
            No projects match "{search}"
          </p>
        </div>
      ) : (
        <div>
          {filtered.map((p) => (
            <Link
              key={p.path}
              to={`/projects/${encodeURIComponent(p.path)}`}
              className="project-list-card"
            >
              <div className="flex items-center justify-between">
                <span
                  style={{
                    fontSize: 15,
                    fontWeight: 600,
                    color: p.missing ? "var(--text-muted)" : "var(--text-primary)",
                  }}
                >
                  {p.name}
                </span>
                <div className="flex items-center gap-2">
                  {p.missing && (
                    <span className="badge badge-danger">missing</span>
                  )}
                  {p.detail?.vcs_type && (
                    <span className="badge" style={{ fontSize: 11 }}>
                      {p.detail.vcs_type}
                    </span>
                  )}
                  {p.detail?.has_config && (
                    <span className="badge badge-info" style={{ fontSize: 11 }}>
                      {p.detail.worktree_enabled ? "worktree" : "branch"}
                    </span>
                  )}
                  <button
                    className="btn btn-danger"
                    onClick={(e) => {
                      e.preventDefault();
                      e.stopPropagation();
                      setRemoveTarget(p.path);
                    }}
                    style={{ padding: "2px 8px", fontSize: 11 }}
                  >
                    Remove
                  </button>
                </div>
              </div>
              <div
                style={{
                  marginTop: 4,
                  fontSize: 12,
                  color: "var(--text-muted)",
                  display: "flex",
                  alignItems: "center",
                  gap: 6,
                }}
              >
                <span className="mono" style={{ overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
                  {p.path}
                </span>
                {p.detail && (
                  <>
                    <span>{"\u00B7"}</span>
                    <span style={{ whiteSpace: "nowrap" }}>
                      {p.detail.workspace_count} workspace{p.detail.workspace_count !== 1 ? "s" : ""}
                    </span>
                    <span>{"\u00B7"}</span>
                    <span style={{ whiteSpace: "nowrap" }}>
                      {p.detail.service_count} service{p.detail.service_count !== 1 ? "s" : ""}
                    </span>
                  </>
                )}
              </div>
            </Link>
          ))}
        </div>
      )}

      <ConfirmDialog
        open={removeTarget !== null}
        onClose={() => setRemoveTarget(null)}
        onConfirm={handleRemove}
        title="Remove Project"
        message="Remove this project from devflow? This only unregisters it — no files will be deleted."
        confirmLabel="Remove"
        danger
      />

      <AddProjectModal ref={addModalRef} onProjectAdded={handleProjectAdded} />
    </div>
  );
}

export default ProjectList;
