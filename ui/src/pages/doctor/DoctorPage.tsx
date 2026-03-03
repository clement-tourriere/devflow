import { useState, useEffect } from "react";
import { useParams, Link } from "react-router-dom";
import { runDoctor } from "../../utils/invoke";
import type { DoctorReport, DoctorServiceReport } from "../../types";

function DoctorPage() {
  const { "*": splat } = useParams();
  const projectPath = splat ? decodeURIComponent(splat) : "";

  const [report, setReport] = useState<DoctorReport | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!projectPath) return;
    setLoading(true);
    runDoctor(projectPath)
      .then((results) => {
        setReport(results);
        setError(null);
      })
      .catch((e) => setError(`Doctor failed: ${e}`))
      .finally(() => setLoading(false));
  }, [projectPath]);

  const renderServiceReport = (entry: DoctorServiceReport, title: string) => (
    <div className="card" key={entry.service}>
      <div className="card-title">{title}</div>
      <table className="table">
        <thead>
          <tr>
            <th style={{ width: 40 }}>Status</th>
            <th>Check</th>
            <th>Detail</th>
          </tr>
        </thead>
        <tbody>
          {entry.checks.map((check) => (
            <tr key={check.name}>
              <td>
                <span
                  style={{
                    color: check.available ? "var(--success)" : "var(--danger)",
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
  );

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

      {!loading && !error && report &&
        renderServiceReport(
          { service: "__general__", checks: report.general },
          "General"
        )}

      {!loading && !error && report && report.services.length === 0 && (
        <div className="card">
          <p style={{ color: "var(--text-secondary)" }}>
            No services configured for this project.
          </p>
        </div>
      )}

      {!loading && !error && report &&
        report.services.map((serviceReport) =>
          renderServiceReport(serviceReport, serviceReport.service)
        )}
    </div>
  );
}

export default DoctorPage;
