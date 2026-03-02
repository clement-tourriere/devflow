import { useState, useEffect } from "react";
import { useParams, Link } from "react-router-dom";
import { runDoctor } from "../../utils/invoke";

interface DoctorCheck {
  name: string;
  available: boolean;
  detail: string;
}

interface ServiceReport {
  service: string;
  checks: DoctorCheck[];
}

function DoctorPage() {
  const { "*": splat } = useParams();
  const projectPath = splat ? decodeURIComponent(splat) : "";

  const [reports, setReports] = useState<ServiceReport[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!projectPath) return;
    setLoading(true);
    runDoctor(projectPath)
      .then((results) => {
        setReports(results as ServiceReport[]);
        setError(null);
      })
      .catch((e) => setError(`Doctor failed: ${e}`))
      .finally(() => setLoading(false));
  }, [projectPath]);

  return (
    <div>
      <Link
        to={`/projects/${encodeURIComponent(projectPath)}`}
        style={{
          color: "var(--text-muted)",
          textDecoration: "none",
          fontSize: 13,
          display: "inline-block",
          marginBottom: 8,
        }}
      >
        &larr; Back to project
      </Link>
      <h1 className="page-title">Doctor</h1>
      <p
        className="mono"
        style={{ color: "var(--text-secondary)", marginBottom: 16 }}
      >
        {projectPath}
      </p>

      {loading && <div>Running diagnostics...</div>}

      {error && (
        <div className="card">
          <p style={{ color: "var(--danger)" }}>{error}</p>
        </div>
      )}

      {!loading && !error && reports.length === 0 && (
        <div className="card">
          <p style={{ color: "var(--text-secondary)" }}>
            No services to diagnose.
          </p>
        </div>
      )}

      {!loading &&
        reports.map((report) => (
          <div className="card" key={report.service}>
            <div className="card-title">{report.service}</div>
            <table className="table">
              <thead>
                <tr>
                  <th style={{ width: 40 }}>Status</th>
                  <th>Check</th>
                  <th>Detail</th>
                </tr>
              </thead>
              <tbody>
                {report.checks.map((check) => (
                  <tr key={check.name}>
                    <td>
                      <span
                        style={{
                          color: check.available
                            ? "var(--success)"
                            : "var(--danger)",
                          fontWeight: 600,
                          fontSize: 16,
                        }}
                      >
                        {check.available ? "\u2713" : "\u2717"}
                      </span>
                    </td>
                    <td style={{ fontWeight: 500 }}>{check.name}</td>
                    <td
                      className="mono"
                      style={{
                        color: "var(--text-secondary)",
                        fontSize: 12,
                      }}
                    >
                      {check.detail}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ))}
    </div>
  );
}

export default DoctorPage;
