import { useState, useEffect } from "react";
import { Link } from "react-router-dom";
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

interface ProjectInfo extends ProjectEntry {
  detail: ProjectDetail | null;
  containerCount: number;
}

function Home() {
  const [projects, setProjects] = useState<ProjectInfo[]>([]);
  const [proxyStatus, setProxyStatus] = useState<ProxyStatus | null>(null);
  const [containers, setContainers] = useState<ContainerEntry[]>([]);

  useEffect(() => {
    const load = async () => {
      const [projectList, proxy, containerList] = await Promise.all([
        listProjects().catch(() => [] as ProjectEntry[]),
        getProxyStatus().catch(() => null),
        listContainers().catch(() => [] as ContainerEntry[]),
      ]);

      setProxyStatus(proxy);
      setContainers(containerList);

      // Enrich each project with detail info
      const enriched = await Promise.all(
        projectList.map(async (p) => {
          const detail = await getProjectDetail(p.path).catch(() => null);
          const containerCount = containerList.filter(
            (c) => c.project === p.name
          ).length;
          return {
            ...p,
            detail,
            containerCount,
          };
        })
      );
      setProjects(enriched);
    };

    load();
  }, []);

  return (
    <div>
      <div className="flex items-center justify-between mb-4">
        <h1 className="page-title" style={{ marginBottom: 0 }}>
          Dashboard
        </h1>
        <Link to="/projects" className="btn btn-primary" style={{ fontSize: 13 }}>
          Add Project
        </Link>
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
          <Link to="/projects" className="btn btn-primary">
            Add Project
          </Link>
        </div>
      ) : (
        <div className="grid grid-2">
          {projects.map((p) => (
            <ProjectCard key={p.path} project={p} />
          ))}
        </div>
      )}
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
      {/* Project name + config status */}
      <div className="flex items-center justify-between" style={{ marginBottom: 6 }}>
        <span style={{ fontSize: 16, fontWeight: 600, color: "var(--text-primary)" }}>
          {project.name}
        </span>
        {d && (
          d.has_config ? (
            <span className="badge badge-success" style={{ fontSize: 11 }}>configured</span>
          ) : (
            <span className="badge badge-warning" style={{ fontSize: 11 }}>no config</span>
          )
        )}
      </div>

      {/* Path */}
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

      {/* Info row */}
      <div className="flex items-center gap-2" style={{ flexWrap: "wrap" }}>
        {d?.current_branch && (
          <span className="badge badge-info">{d.current_branch}</span>
        )}
        {d?.worktree_enabled && (
          <span className="badge badge-success" style={{ fontSize: 11 }}>worktrees</span>
        )}
        {d && (
          <span style={{ color: "var(--text-secondary)", fontSize: 13 }}>
            {d.branch_count} branch{d.branch_count !== 1 ? "es" : ""}
            {" \u00B7 "}
            {d.service_count} service{d.service_count !== 1 ? "s" : ""}
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
