import { useState, useEffect } from "react";
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

function ProxyDashboard() {
  const [status, setStatus] = useState<ProxyStatus | null>(null);
  const [containers, setContainers] = useState<ContainerEntry[]>([]);
  const [certStatus, setCertStatus] = useState<CertificateStatus | null>(null);
  const [loading, setLoading] = useState(false);
  const [filter, setFilter] = useState("");

  const refresh = () => {
    getProxyStatus().then(setStatus).catch(() => setStatus(null));
    listContainers().then(setContainers).catch(() => setContainers([]));
    getCertificateStatus().then(setCertStatus).catch(() => {});
  };

  useEffect(refresh, []);

  const handleToggleProxy = async () => {
    setLoading(true);
    try {
      if (status?.running) {
        await stopProxy();
      } else {
        await startProxy();
      }
      refresh();
    } catch (e) {
      alert(`Proxy error: ${e}`);
    } finally {
      setLoading(false);
    }
  };

  const handleInstallCert = async () => {
    try {
      await installCertificate();
      refresh();
    } catch (e) {
      alert(`Certificate error: ${e}`);
    }
  };

  const handleRemoveCert = async () => {
    try {
      await removeCertificate();
      refresh();
    } catch (e) {
      alert(`Certificate error: ${e}`);
    }
  };

  const filtered = containers.filter(
    (c) =>
      !filter ||
      c.domain.includes(filter) ||
      c.container_name.includes(filter) ||
      (c.project && c.project.includes(filter))
  );

  return (
    <div>
      <h1 className="page-title">Proxy</h1>

      <div className="grid grid-2">
        <div className="card">
          <div className="card-title">Status</div>
          <div className="flex items-center gap-2 mb-4">
            <span
              className={`proxy-dot ${status?.running ? "running" : "stopped"}`}
            />
            <span>{status?.running ? "Running" : "Stopped"}</span>
          </div>
          {status?.running && (
            <div style={{ marginBottom: 12, color: "var(--text-secondary)", fontSize: 13 }}>
              HTTPS port: {status.https_port} &middot; HTTP port: {status.http_port}
            </div>
          )}
          <button
            className={`btn ${status?.running ? "btn-danger" : "btn-primary"}`}
            onClick={handleToggleProxy}
            disabled={loading}
          >
            {loading ? "..." : status?.running ? "Stop Proxy" : "Start Proxy"}
          </button>
        </div>

        <div className="card">
          <div className="card-title">Certificate Authority</div>
          {certStatus ? (
            <>
              <div className="flex items-center gap-2 mb-4">
                {certStatus.installed ? (
                  <span className="badge badge-success">Trusted</span>
                ) : certStatus.exists ? (
                  <span className="badge badge-warning">Not Trusted</span>
                ) : (
                  <span className="badge badge-danger">Not Generated</span>
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
                  <button className="btn btn-primary" onClick={handleInstallCert}>
                    Install to System
                  </button>
                )}
                {certStatus.installed && (
                  <button className="btn btn-danger" onClick={handleRemoveCert}>
                    Remove Trust
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
            <p style={{ color: "var(--text-secondary)" }}>Loading...</p>
          )}
        </div>
      </div>

      <div className="card">
        <div className="flex items-center justify-between mb-4">
          <span className="card-title" style={{ marginBottom: 0 }}>
            Containers ({filtered.length})
          </span>
          <input
            type="text"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            placeholder="Filter containers..."
            style={{ width: 240 }}
          />
        </div>
        {filtered.length === 0 ? (
          <p style={{ color: "var(--text-secondary)" }}>
            {containers.length === 0
              ? "No containers detected."
              : "No containers match the filter."}
          </p>
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>Domain</th>
                <th>Container</th>
                <th>Upstream</th>
                <th>Project</th>
                <th>Branch</th>
              </tr>
            </thead>
            <tbody>
              {filtered.map((c) => (
                <tr key={c.domain}>
                  <td>
                    <a
                      href={c.https_url}
                      target="_blank"
                      style={{ color: "var(--accent)", textDecoration: "none" }}
                    >
                      {c.domain}
                    </a>
                  </td>
                  <td>{c.container_name}</td>
                  <td className="mono" style={{ color: "var(--text-muted)", fontSize: 12 }}>
                    {c.container_ip}:{c.port}
                  </td>
                  <td>{c.project || "-"}</td>
                  <td>{c.branch || "-"}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </div>
  );
}

export default ProxyDashboard;
