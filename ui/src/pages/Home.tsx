import { useState, useEffect, useRef } from "react";
import { Link, useNavigate } from "react-router-dom";
import {
  listProjects,
  getProjectDetail,
  getProxyStatus,
  listContainers,
} from "../utils/invoke";
import type {
  ProjectEntry,
  ProjectDetail,
  ProxyStatus,
  ContainerEntry,
} from "../types";
import AddProjectModal, { type AddProjectModalHandle } from "../components/AddProjectModal";
import { sortByRecent } from "../utils/recentProjects";
import {
  IconDashboard,
  IconPlus,
  IconProjects,
  IconDatabase,
  IconProxy,
} from "../components/icons";

interface ProjectInfo extends ProjectEntry {
  detail: ProjectDetail | null;
  containerCount: number;
}

const MAX_DASHBOARD_PROJECTS = 6;

function Home() {
  const navigate = useNavigate();
  const [projects, setProjects] = useState<ProjectInfo[]>([]);
  const [proxyStatus, setProxyStatus] = useState<ProxyStatus | null>(null);
  const [containers, setContainers] = useState<ContainerEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const addModalRef = useRef<AddProjectModalHandle>(null);

  const load = async () => {
    const [projectList, proxy, containerList] = await Promise.all([
      listProjects().catch(() => [] as ProjectEntry[]),
      getProxyStatus().catch(() => null),
      listContainers().catch(() => [] as ContainerEntry[]),
    ]);

    setProxyStatus(proxy);
    setContainers(containerList);

    const enriched = await Promise.all(
      projectList.map(async (p) => {
        const detail = await getProjectDetail(p.path).catch(() => null);
        const containerCount = containerList.filter(
          (c) => c.project === p.name
        ).length;
        return { ...p, detail, containerCount };
      })
    );
    setProjects(enriched);
    setLoading(false);
  };

  useEffect(() => {
    load();
  }, []);

  const totalWorkspaces = projects.reduce(
    (n, p) => n + (p.detail?.workspace_count ?? 0),
    0
  );
  const totalServices = projects.reduce(
    (n, p) => n + (p.detail?.service_count ?? 0),
    0
  );

  const handleProjectAdded = (projectPath: string) => {
    load();
    navigate(`/projects/${encodeURIComponent(projectPath)}`);
  };

  const sorted = sortByRecent(projects, (p) => p.path);
  const capped = sorted.slice(0, MAX_DASHBOARD_PROJECTS);
  const hasMore = projects.length > MAX_DASHBOARD_PROJECTS;

  return (
    <div>
      <div className="page-header">
        <div className="page-header-titles">
          <h1>
            <IconDashboard size={22} />
            Dashboard
          </h1>
        </div>
        <div className="page-header-actions">
          <button
            className="btn btn-primary"
            onClick={() => addModalRef.current?.open()}
          >
            <IconPlus size={15} />
            Add Project
          </button>
        </div>
      </div>

      {/* At-a-glance stats */}
      <div className="stat-grid">
        <Link to="/projects" className="stat-tile" style={{ textDecoration: "none", color: "inherit" }}>
          <div className="stat-label">
            <IconProjects size={13} /> Projects
          </div>
          <div className="stat-value">{projects.length}</div>
        </Link>
        <div className="stat-tile">
          <div className="stat-label">Workspaces</div>
          <div className="stat-value">{totalWorkspaces}</div>
        </div>
        <div className="stat-tile">
          <div className="stat-label">
            <IconDatabase size={13} /> Services
          </div>
          <div className="stat-value">{totalServices}</div>
        </div>
        <Link to="/proxy" className="stat-tile" style={{ textDecoration: "none", color: "inherit" }}>
          <div className="stat-label">
            <IconProxy size={13} /> Proxy
          </div>
          <div className="stat-value" style={{ display: "flex", alignItems: "center", gap: 8 }}>
            <span className={`status-dot ${proxyStatus?.running ? "running" : "stopped"}`} />
            <span style={{ fontSize: 16 }}>
              {proxyStatus?.running ? `${containers.length} routed` : "Stopped"}
            </span>
          </div>
        </Link>
      </div>

      {/* Projects grid */}
      {loading ? (
        <div className="grid grid-2">
          <div className="skeleton" style={{ height: 110 }} />
          <div className="skeleton" style={{ height: 110 }} />
        </div>
      ) : projects.length === 0 ? (
        <div className="card">
          <div className="empty-state">
            <div className="empty-state-icon">
              <IconProjects size={24} />
            </div>
            <div className="empty-state-title">Welcome to devflow</div>
            <div className="empty-state-desc">
              Add an existing devflow project or initialize a new one to manage
              isolated services, workspaces, hooks, and the proxy.
            </div>
            <button
              className="btn btn-primary"
              style={{ marginTop: 6 }}
              onClick={() => addModalRef.current?.open()}
            >
              <IconPlus size={15} />
              Add Project
            </button>
          </div>
        </div>
      ) : (
        <>
          <div className="grid grid-2">
            {capped.map((p) => (
              <ProjectCard key={p.path} project={p} />
            ))}
          </div>
          {hasMore && (
            <div style={{ textAlign: "center", marginTop: 8 }}>
              <Link
                to="/projects"
                style={{
                  color: "var(--text-muted)",
                  fontSize: 13,
                  textDecoration: "none",
                }}
              >
                View all projects ({projects.length}) &rarr;
              </Link>
            </div>
          )}
        </>
      )}

      <AddProjectModal ref={addModalRef} onProjectAdded={handleProjectAdded} />
    </div>
  );
}

function ProjectCard({ project }: { project: ProjectInfo }) {
  const d = project.detail;

  return (
    <Link
      to={`/projects/${encodeURIComponent(project.path)}`}
      className="card"
      style={{
        textDecoration: "none",
        color: "inherit",
        cursor: "pointer",
        transition: "border-color 0.15s",
        display: "block",
      }}
      onMouseEnter={(e) =>
        (e.currentTarget.style.borderColor = "var(--accent)")
      }
      onMouseLeave={(e) =>
        (e.currentTarget.style.borderColor = "var(--border)")
      }
    >
      <div className="flex items-center justify-between" style={{ marginBottom: 6 }}>
        <span style={{ fontSize: 16, fontWeight: 600, color: "var(--text-primary)" }}>
          {project.name}
        </span>
        {d && !d.has_config && (
          <span className="badge badge-warning" style={{ fontSize: 11 }}>
            no config
          </span>
        )}
      </div>

      <div
        className="mono"
        style={{
          color: "var(--text-muted)",
          fontSize: 12,
          marginBottom: 12,
          overflow: "hidden",
          textOverflow: "ellipsis",
          whiteSpace: "nowrap",
        }}
      >
        {project.path}
      </div>

      <div className="flex items-center gap-2" style={{ flexWrap: "wrap" }}>
        {d?.current_workspace && !d?.worktree_enabled && (
          <span className="badge" style={{ opacity: 0.7 }}>
            active: {d.current_workspace}
          </span>
        )}
        {d?.vcs_type && (
          <span className="badge" style={{ fontSize: 11 }}>
            vcs: {d.vcs_type}
          </span>
        )}
        {d?.has_config && (
          <span
            className="badge badge-info"
            style={{ fontSize: 11 }}
            title="Default creation mode. You can still choose branch or worktree when creating a workspace."
          >
            default: {d.worktree_enabled ? "worktree" : "branch"}
          </span>
        )}
        {d && (
          <span style={{ color: "var(--text-secondary)", fontSize: 13 }}>
            {d.workspace_count} workspace{d.workspace_count !== 1 ? "es" : ""}
            {" \u00B7 "}
            {d.service_count} service{d.service_count !== 1 ? "s" : ""}
            {d.hook_count > 0 && (
              <>
                {" \u00B7 "}
                {d.hook_count} hook{d.hook_count !== 1 ? "s" : ""}
              </>
            )}
          </span>
        )}
        {project.containerCount > 0 && (
          <span style={{ color: "var(--text-secondary)", fontSize: 13 }}>
            {" \u00B7 "}
            {project.containerCount} container{project.containerCount !== 1 ? "s" : ""}
          </span>
        )}
      </div>
    </Link>
  );
}

export default Home;
