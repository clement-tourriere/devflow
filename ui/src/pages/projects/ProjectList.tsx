import { useState, useEffect, useRef } from "react";
import { toast } from "../../utils/notify";
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
import { IconProjects, IconPlus, IconTrash } from "../../components/icons";

interface ProjectRow extends ProjectEntry {
  detail?: ProjectDetail;
  missing?: boolean;
}

function ProjectList() {
  const navigate = useNavigate();
  const [projects, setProjects] = useState<ProjectRow[]>([]);
  const [loading, setLoading] = useState(true);
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
    } finally {
      setLoading(false);
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
      toast.error(`Failed to remove: ${e}`);
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
            <IconPlus size={15} />
            Add Project
          </button>
        </div>
      </div>

      {loading ? (
        <div>
          <div className="skeleton skeleton-row" />
          <div className="skeleton skeleton-row" />
          <div className="skeleton skeleton-row" />
        </div>
      ) : projects.length === 0 ? (
        <div className="card">
          <div className="empty-state">
            <div className="empty-state-icon">
              <IconProjects size={24} />
            </div>
            <div className="empty-state-title">No projects yet</div>
            <div className="empty-state-desc">
              Add a new or existing devflow project to manage its workspaces,
              services, hooks, and proxy from here.
            </div>
            <button
              className="btn btn-primary"
              style={{ marginTop: 6 }}
              onClick={() => addModalRef.current?.open()}
            >
              <IconPlus size={15} />
              Add your first project
            </button>
          </div>
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
                  {!p.missing && p.detail && !p.detail.has_config && (
                    <span className="badge badge-warning">no config</span>
                  )}
                  {p.detail?.vcs_type && (
                    <span className="badge" style={{ fontSize: 11 }}>
                      {p.detail.vcs_type}
                    </span>
                  )}
                  {p.detail?.has_config && (
                    <span className="badge badge-info" style={{ fontSize: 11 }}>
                      workspaces
                    </span>
                  )}
                  <button
                    className="icon-btn danger"
                    title="Remove from devflow"
                    onClick={(e) => {
                      e.preventDefault();
                      e.stopPropagation();
                      setRemoveTarget(p.path);
                    }}
                  >
                    <IconTrash size={15} />
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
