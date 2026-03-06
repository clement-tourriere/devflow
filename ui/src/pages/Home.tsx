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
  };

  useEffect(() => {
    load();
  }, []);

  const handleProjectAdded = (projectPath: string) => {
    load();
    navigate(`/projects/${encodeURIComponent(projectPath)}`);
  };

  const sorted = sortByRecent(projects, (p) => p.path);
  const capped = sorted.slice(0, MAX_DASHBOARD_PROJECTS);
  const hasMore = projects.length > MAX_DASHBOARD_PROJECTS;

  return (
    <div>
      <div className="flex items-center justify-between mb-4">
        <h1 className="page-title" style={{ marginBottom: 0 }}>
          Dashboard
        </h1>
        <button
          className="btn btn-primary"
          style={{ fontSize: 13 }}
          onClick={() => addModalRef.current?.open()}
        >
          Add Project
        </button>
      </div>

      {/* Proxy status strip */}
      <Link
        to="/proxy"
        style={{
          display: "flex",
          alignItems: "center",
          gap: 10,
          padding: "10px 16px",
          background: "var(--bg-secondary)",
          border: "1px solid var(--border)",
          borderRadius: 8,
          marginBottom: 16,
          textDecoration: "none",
          color: "var(--text-primary)",
        }}
      >
        <span
          className={`proxy-dot ${proxyStatus?.running ? "running" : "stopped"}`}
        />
        <span style={{ fontWeight: 500 }}>
          Proxy: {proxyStatus?.running ? "Running" : "Stopped"}
        </span>
        {proxyStatus?.running && (
          <>
            <span className="badge badge-info" style={{ fontSize: 11 }}>
              HTTPS:{proxyStatus.https_port}
            </span>
            {proxyStatus.ca_installed ? (
              <span className="badge badge-success" style={{ fontSize: 11 }}>
                CA Trusted
              </span>
            ) : (
              <span className="badge badge-warning" style={{ fontSize: 11 }}>
                CA Not Trusted
              </span>
            )}
          </>
        )}
        {containers.length > 0 && (
          <span style={{ color: "var(--text-muted)", fontSize: 13 }}>
            {containers.length} container{containers.length !== 1 ? "s" : ""}
          </span>
        )}
        <span
          style={{
            marginLeft: "auto",
            color: "var(--text-muted)",
            fontSize: 12,
          }}
        >
          View Proxy
        </span>
      </Link>

      {/* Projects grid */}
      {projects.length === 0 ? (
        <div className="card" style={{ textAlign: "center", padding: 32 }}>
          <p style={{ color: "var(--text-secondary)", marginBottom: 8 }}>
            No projects registered yet.
          </p>
          <p
            style={{
              color: "var(--text-muted)",
              fontSize: 13,
              marginBottom: 16,
            }}
          >
            Add an existing devflow project or initialize a new one to get
            started.
          </p>
          <button
            className="btn btn-primary"
            onClick={() => addModalRef.current?.open()}
          >
            Add Project
          </button>
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
            setup needed
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
