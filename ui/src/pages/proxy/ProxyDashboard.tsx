import { useState, useEffect, useCallback } from "react";
import { listen } from "@tauri-apps/api/event";
import { toast } from "../../utils/notify";
import {
  getProxyStatus,
  startProxy,
  stopProxy,
  listContainers,
  getCertificateStatus,
  installCertificate,
  removeCertificate,
} from "../../utils/invoke";
import type { ProxyStatus, ContainerEntry, CertificateStatus } from "../../types";
import { IconProxy, IconRefresh, IconCopy, IconExternal } from "../../components/icons";

function ProxyDashboard() {
  const [status, setStatus] = useState<ProxyStatus | null>(null);
  const [containers, setContainers] = useState<ContainerEntry[]>([]);
  const [certStatus, setCertStatus] = useState<CertificateStatus | null>(null);
  const [loading, setLoading] = useState(false);
  const [filter, setFilter] = useState("");

  const refresh = useCallback(() => {
    getProxyStatus().then(setStatus).catch(() => setStatus(null));
    listContainers().then(setContainers).catch(() => setContainers([]));
    getCertificateStatus().then(setCertStatus).catch(() => {});
  }, []);

  // Initial load + live updates: poll while mounted and react to backend
  // status events, so the dashboard reflects containers coming and going
  // without a manual refresh.
  useEffect(() => {
    refresh();
    const interval = setInterval(refresh, 4000);
    const unlisten = listen<ProxyStatus>("proxy-status-changed", (e) => {
      setStatus(e.payload);
      listContainers().then(setContainers).catch(() => {});
    });
    return () => {
      clearInterval(interval);
      unlisten.then((fn) => fn());
    };
  }, [refresh]);

  const handleToggleProxy = async () => {
    setLoading(true);
    try {
      if (status?.running) {
        await stopProxy();
        toast.success("Proxy stopped");
      } else {
        await startProxy();
        toast.success("Proxy started");
      }
      refresh();
    } catch (e) {
      toast.error(`Proxy error: ${e}`);
    } finally {
      setLoading(false);
    }
  };

  const handleInstallCert = async () => {
    try {
      await installCertificate();
      toast.success("Certificate installed to system trust store");
      refresh();
    } catch (e) {
      toast.error(`Certificate error: ${e}`);
    }
  };

  const handleRemoveCert = async () => {
    try {
      await removeCertificate();
      toast.success("Certificate trust removed");
      refresh();
    } catch (e) {
      toast.error(`Certificate error: ${e}`);
    }
  };

  const copy = (text: string, label: string) => {
    navigator.clipboard?.writeText(text).then(
      () => toast.success(`Copied ${label}`),
      () => toast.error("Copy failed"),
    );
  };

  const filtered = containers.filter((c) => {
    if (!filter) return true;
    const q = filter.toLowerCase();
    return (
      c.domain.toLowerCase().includes(q) ||
      c.container_name.toLowerCase().includes(q) ||
      (c.project && c.project.toLowerCase().includes(q))
    );
  });

  const isHttp = (c: ContainerEntry) =>
    c.endpoint_url?.startsWith("http://") || c.endpoint_url?.startsWith("https://");

  return (
    <div>
      <div className="page-header">
        <div className="page-header-titles">
          <h1>
            <IconProxy size={22} />
            Proxy
          </h1>
          <div className="page-header-sub">
            Auto-discovered containers served over HTTPS via *.localhost
          </div>
        </div>
        <div className="page-header-actions">
          <button className="btn" onClick={refresh} title="Refresh">
            <IconRefresh size={15} />
            Refresh
          </button>
          <button
            className={`btn ${status?.running ? "btn-danger" : "btn-primary"}`}
            onClick={handleToggleProxy}
            disabled={loading}
          >
            {loading ? "…" : status?.running ? "Stop proxy" : "Start proxy"}
          </button>
        </div>
      </div>

      <div className="grid grid-2">
        <div className="card">
          <div className="card-title">Status</div>
          <div className="status-line" style={{ marginBottom: 10 }}>
            <span className={`status-dot ${status?.running ? "running" : "stopped"}`} />
            <span style={{ fontWeight: 600 }}>
              {status?.running ? "Running" : "Stopped"}
            </span>
          </div>
          {status?.running && (
            <div style={{ color: "var(--text-secondary)", fontSize: 13 }}>
              HTTPS {status.https_port} · HTTP {status.http_port}
            </div>
          )}
        </div>

        <div className="card">
          <div className="card-title">Certificate Authority</div>
          {certStatus ? (
            <>
              <div className="status-line" style={{ marginBottom: 10 }}>
                {certStatus.installed ? (
                  <span className="badge badge-success">Trusted</span>
                ) : certStatus.exists ? (
                  <span className="badge badge-warning">Not trusted</span>
                ) : (
                  <span className="badge badge-danger">Not generated</span>
                )}
              </div>
              {certStatus.exists && (
                <p
                  className="mono"
                  style={{ color: "var(--text-muted)", fontSize: 12, marginBottom: 12 }}
                >
                  {certStatus.path}
                </p>
              )}
              <div className="flex gap-2">
                {certStatus.exists && !certStatus.installed && (
                  <button className="btn btn-primary btn-sm" onClick={handleInstallCert}>
                    Install to system
                  </button>
                )}
                {certStatus.installed && (
                  <button className="btn btn-danger btn-sm" onClick={handleRemoveCert}>
                    Remove trust
                  </button>
                )}
              </div>
              {certStatus.info && (
                <p style={{ color: "var(--text-secondary)", fontSize: 12, marginTop: 8 }}>
                  {certStatus.info}
                </p>
              )}
            </>
          ) : (
            <div className="skeleton skeleton-text" style={{ width: "60%" }} />
          )}
        </div>
      </div>

      <div className="card">
        <div className="flex items-center justify-between mb-4">
          <span className="card-title" style={{ marginBottom: 0 }}>
            Containers ({filtered.length})
          </span>
          <input
            type="search"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            placeholder="Filter targets…"
            className="search-input"
          />
        </div>
        {filtered.length === 0 ? (
          <div className="empty-state">
            <div className="empty-state-icon">
              <IconProxy size={24} />
            </div>
            <div className="empty-state-title">
              {containers.length === 0 ? "No proxy targets detected" : "No matches"}
            </div>
            <div className="empty-state-desc">
              {containers.length === 0
                ? status?.running
                  ? "Start a Docker container or a devflow-managed process with a port to see it routed here."
                  : "Start the proxy to begin auto-discovering Docker containers and devflow processes."
                : `Nothing matches “${filter}”.`}
            </div>
          </div>
        ) : (
          <div className="table-card">
            <table className="table">
              <thead>
                <tr>
                  <th>Domain</th>
                  <th>Target</th>
                  <th>Upstream</th>
                  <th>Project</th>
                  <th>Workspace</th>
                  <th style={{ textAlign: "right" }}>Actions</th>
                </tr>
              </thead>
              <tbody>
                {filtered.map((c) => (
                  <tr key={c.domain}>
                    <td>
                      {isHttp(c) ? (
                        <a
                          href={c.endpoint_url}
                          target="_blank"
                          rel="noreferrer"
                          title={c.endpoint_url}
                          style={{ color: "var(--accent)", textDecoration: "none" }}
                        >
                          {c.domain}
                        </a>
                      ) : (
                        <span title="Non-HTTP service (TCP)">{c.domain}</span>
                      )}
                    </td>
                    <td style={{ overflow: "hidden", textOverflow: "ellipsis" }}>
                      {c.container_name.startsWith("process:") ? c.container_name.replace("process:", "process ") : c.container_name}
                    </td>
                    <td className="mono" style={{ color: "var(--text-muted)", fontSize: 12 }}>
                      {c.container_ip}:{c.port}
                    </td>
                    <td>{c.project || "-"}</td>
                    <td>{c.workspace || "-"}</td>
                    <td>
                      <div className="row-actions">
                        <button
                          className="icon-btn"
                          title="Copy domain"
                          onClick={() => copy(c.domain, "domain")}
                        >
                          <IconCopy size={15} />
                        </button>
                        {isHttp(c) && (
                          <a
                            className="icon-btn"
                            href={c.endpoint_url}
                            target="_blank"
                            rel="noreferrer"
                            title="Open in browser"
                          >
                            <IconExternal size={15} />
                          </a>
                        )}
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </div>
  );
}

export default ProxyDashboard;
