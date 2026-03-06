import { useState, useEffect, useRef } from "react";
import {
  getSettings,
  saveSettings,
  listProjects,
  detectOrphanProjects,
  cleanupOrphanProject,
} from "../../utils/invoke";
import type {
  AppSettings,
  ProjectEntry,
  OrphanProjectEntry,
  OrphanCleanupResult,
  TerminalRenderer,
} from "../../types";

function Settings() {
  const [settings, setSettings] = useState<AppSettings | null>(null);
  const [projects, setProjects] = useState<ProjectEntry[]>([]);
  const [saveStatus, setSaveStatus] = useState<"idle" | "saving" | "saved" | "error">("idle");
  const loaded = useRef(false);

  // Orphan state
  const [orphans, setOrphans] = useState<OrphanProjectEntry[]>([]);
  const [orphanScanning, setOrphanScanning] = useState(false);
  const [orphanScanned, setOrphanScanned] = useState(false);
  const [orphanCleaning, setOrphanCleaning] = useState<string | null>(null);
  const [orphanResults, setOrphanResults] = useState<OrphanCleanupResult[]>([]);

  useEffect(() => {
    getSettings()
      .then((s) => {
        setSettings(s);
        // Mark loaded after initial state is set so auto-save doesn't fire on load
        requestAnimationFrame(() => {
          loaded.current = true;
        });
      })
      .catch(console.error);
    listProjects().then(setProjects).catch(() => {});
  }, []);

  // Auto-save with debounce
  useEffect(() => {
    if (!settings || !loaded.current) return;

    const timer = setTimeout(() => {
      setSaveStatus("saving");
      saveSettings(settings)
        .then(() => {
          window.dispatchEvent(
            new CustomEvent("devflow:settings-updated", { detail: settings })
          );
          setSaveStatus("saved");
          setTimeout(() => setSaveStatus("idle"), 1500);
        })
        .catch(() => {
          setSaveStatus("error");
          setTimeout(() => setSaveStatus("idle"), 3000);
        });
    }, 800);

    return () => clearTimeout(timer);
  }, [settings]);

  const handleScanOrphans = async () => {
    setOrphanScanning(true);
    setOrphanResults([]);
    try {
      const result = await detectOrphanProjects();
      setOrphans(result);
      setOrphanScanned(true);
    } catch (e) {
      alert(`Scan failed: ${e}`);
    } finally {
      setOrphanScanning(false);
    }
  };

  const handleCleanupOrphan = async (projectName: string) => {
    setOrphanCleaning(projectName);
    try {
      const result = await cleanupOrphanProject(projectName);
      setOrphanResults((prev) => [...prev, result]);
      setOrphans((prev) => prev.filter((o) => o.project_name !== projectName));
    } catch (e) {
      alert(`Cleanup failed: ${e}`);
    } finally {
      setOrphanCleaning(null);
    }
  };

  const handleCleanupAll = async () => {
    for (const orphan of orphans) {
      await handleCleanupOrphan(orphan.project_name);
    }
  };

  const setRenderer = (renderer: TerminalRenderer) => {
    setSettings((prev) => (prev ? { ...prev, terminal_renderer: renderer } : prev));
  };

  const setFontSize = (value: number) => {
    if (!Number.isFinite(value)) return;
    const clamped = Math.max(11, Math.min(24, Math.round(value)));
    setSettings((prev) =>
      prev ? { ...prev, terminal_font_size: clamped } : prev
    );
  };

  if (!settings) return <div>Loading...</div>;

  const prefersWebGpu = settings.terminal_renderer !== "webgl2";

  return (
    <div>
      <h1 className="page-title">Settings</h1>

      <div className="card">
        <div className="card-title">Registered Projects</div>
        <p style={{ color: "var(--text-secondary)", marginBottom: 12 }}>
          Projects managed by devflow. Add or remove from the Projects page.
        </p>
        {projects.length === 0 ? (
          <p style={{ color: "var(--text-muted)" }}>No projects registered.</p>
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>Name</th>
                <th>Path</th>
              </tr>
            </thead>
            <tbody>
              {projects.map((p) => (
                <tr key={p.path}>
                  <td>{p.name}</td>
                  <td className="mono" style={{ color: "var(--text-secondary)" }}>
                    {p.path}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>

      <div className="card">
        <div className="card-title">Orphan Cleanup</div>
        <p style={{ color: "var(--text-secondary)", marginBottom: 12 }}>
          Scan for orphaned projects whose directories no longer exist but still
          have leftover state (Docker containers, database state, local
          configuration).
        </p>
        <div className="flex gap-2" style={{ marginBottom: 12 }}>
          <button
            className="btn btn-primary"
            onClick={handleScanOrphans}
            disabled={orphanScanning}
          >
            {orphanScanning ? "Scanning..." : "Scan for Orphans"}
          </button>
          {orphans.length > 0 && (
            <button
              className="btn btn-danger"
              onClick={handleCleanupAll}
              disabled={orphanCleaning !== null}
            >
              Clean Up All ({orphans.length})
            </button>
          )}
        </div>

        {orphanScanned && orphans.length === 0 && orphanResults.length === 0 && (
          <div
            style={{
              padding: 12,
              background: "rgba(63, 185, 80, 0.1)",
              border: "1px solid var(--success)",
              borderRadius: 6,
              color: "var(--success)",
              fontSize: 13,
            }}
          >
            No orphaned projects found.
          </div>
        )}

        {orphans.length > 0 && (
          <table className="table">
            <thead>
              <tr>
                <th>Project</th>
                <th>Path</th>
                <th>Sources</th>
                <th>Details</th>
                <th>Actions</th>
              </tr>
            </thead>
            <tbody>
              {orphans.map((o) => (
                <tr key={o.project_name}>
                  <td style={{ fontWeight: 500 }}>{o.project_name}</td>
                  <td
                    className="mono"
                    style={{ color: "var(--text-secondary)", fontSize: 12 }}
                  >
                    {o.project_path ?? "—"}
                  </td>
                  <td>
                    {o.sources.map((s) => (
                      <span
                        key={s}
                        className="badge badge-warning"
                        style={{ marginRight: 4 }}
                      >
                        {s}
                      </span>
                    ))}
                  </td>
                  <td style={{ color: "var(--text-secondary)", fontSize: 12 }}>
                    {[
                      o.sqlite_workspace_count > 0 &&
                        `${o.sqlite_workspace_count} db ${o.sqlite_workspace_count === 1 ? "workspace" : "workspaces"}`,
                      o.container_names.length > 0 &&
                        `${o.container_names.length} container${o.container_names.length !== 1 ? "s" : ""}`,
                      o.local_state_service_count > 0 &&
                        `${o.local_state_service_count} service${o.local_state_service_count !== 1 ? "s" : ""}`,
                    ]
                      .filter(Boolean)
                      .join(", ") || "—"}
                  </td>
                  <td>
                    <button
                      className="btn btn-danger"
                      onClick={() => handleCleanupOrphan(o.project_name)}
                      disabled={orphanCleaning !== null}
                      style={{ padding: "4px 10px", fontSize: 12 }}
                    >
                      {orphanCleaning === o.project_name
                        ? "Cleaning..."
                        : "Clean Up"}
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}

        {orphanResults.length > 0 && (
          <div style={{ marginTop: 12 }}>
            <div
              style={{
                fontSize: 13,
                fontWeight: 500,
                color: "var(--text-secondary)",
                marginBottom: 8,
              }}
            >
              Cleanup Results
            </div>
            {orphanResults.map((r) => (
              <div
                key={r.project_name}
                style={{
                  padding: "8px 12px",
                  marginBottom: 4,
                  background:
                    r.errors.length > 0
                      ? "rgba(210, 153, 34, 0.1)"
                      : "rgba(63, 185, 80, 0.1)",
                  border: `1px solid ${r.errors.length > 0 ? "var(--warning)" : "var(--success)"}`,
                  borderRadius: 6,
                  fontSize: 13,
                }}
              >
                <strong>{r.project_name}</strong>:{" "}
                {[
                  r.containers_removed > 0 &&
                    `${r.containers_removed} container${r.containers_removed !== 1 ? "s" : ""} removed`,
                  r.sqlite_rows_deleted && "sqlite cleared",
                  r.local_state_cleared && "local state cleared",
                  r.data_dirs_removed > 0 &&
                    `${r.data_dirs_removed} data dir${r.data_dirs_removed !== 1 ? "s" : ""} removed`,
                ]
                  .filter(Boolean)
                  .join(", ") || "cleaned up"}
                {r.errors.length > 0 && (
                  <div style={{ color: "var(--warning)", marginTop: 4 }}>
                    {r.errors.map((e, i) => (
                      <div key={i}>Warning: {e}</div>
                    ))}
                  </div>
                )}
              </div>
            ))}
          </div>
        )}
      </div>

      <div className="card">
        <div className="card-title">Features</div>
        <p style={{ color: "var(--text-secondary)", marginBottom: 12 }}>
          Toggle experimental features on or off.
        </p>
        <div
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            gap: 12,
            padding: "10px 12px",
            border: "1px solid var(--border)",
            borderRadius: 8,
            background: "var(--bg-primary)",
          }}
        >
          <div>
            <div style={{ fontWeight: 500, marginBottom: 2 }}>Smart Merge</div>
            <div style={{ color: "var(--text-muted)", fontSize: 12 }}>
              Enable merge readiness checks, rebase, merge trains, and cascade
              notifications.
            </div>
          </div>
          <label
            style={{
              display: "inline-flex",
              alignItems: "center",
              gap: 8,
              color: "var(--text-secondary)",
              fontSize: 13,
            }}
          >
            <input
              type="checkbox"
              checked={settings.smart_merge}
              onChange={(e) =>
                setSettings((prev) =>
                  prev ? { ...prev, smart_merge: e.target.checked } : prev
                )
              }
            />
            {settings.smart_merge ? "On" : "Off"}
          </label>
        </div>
      </div>

      <div className="card">
        <div className="card-title">Terminal</div>
        <p style={{ color: "var(--text-secondary)", marginBottom: 12 }}>
          Embedded terminal rendering is powered by libghostty-vt with restty.
          Keep WebGPU enabled for best performance, with automatic fallback when
          unavailable.
        </p>
        <div
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            gap: 12,
            padding: "10px 12px",
            border: "1px solid var(--border)",
            borderRadius: 8,
            background: "var(--bg-primary)",
          }}
        >
          <div>
            <div style={{ fontWeight: 500, marginBottom: 2 }}>Prefer WebGPU</div>
            <div style={{ color: "var(--text-muted)", fontSize: 12 }}>
              Off forces WebGL2 renderer.
            </div>
          </div>
          <label
            style={{
              display: "inline-flex",
              alignItems: "center",
              gap: 8,
              color: "var(--text-secondary)",
              fontSize: 13,
            }}
          >
            <input
              type="checkbox"
              checked={prefersWebGpu}
              onChange={(e) =>
                setRenderer(e.target.checked ? "auto" : "webgl2")
              }
            />
            {prefersWebGpu ? "On" : "Off"}
          </label>
        </div>

        <div
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            gap: 12,
            padding: "10px 12px",
            border: "1px solid var(--border)",
            borderRadius: 8,
            background: "var(--bg-primary)",
            marginTop: 10,
          }}
        >
          <div>
            <div style={{ fontWeight: 500, marginBottom: 2 }}>Font Size</div>
            <div style={{ color: "var(--text-muted)", fontSize: 12 }}>
              Controls terminal text size across all tabs.
            </div>
          </div>
          <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
            <input
              type="range"
              min={11}
              max={24}
              value={settings.terminal_font_size}
              onChange={(e) => setFontSize(Number(e.target.value))}
            />
            <input
              type="number"
              min={11}
              max={24}
              value={settings.terminal_font_size}
              onChange={(e) => setFontSize(Number(e.target.value))}
              style={{ width: 64 }}
            />
            <span className="mono" style={{ fontSize: 12, minWidth: 40 }}>
              {settings.terminal_font_size}px
            </span>
          </div>
        </div>
      </div>

      {saveStatus !== "idle" && (
        <div
          style={{
            position: "fixed",
            bottom: 20,
            right: 20,
            padding: "8px 16px",
            borderRadius: 8,
            fontSize: 13,
            background:
              saveStatus === "error"
                ? "rgba(248, 81, 73, 0.15)"
                : "rgba(63, 185, 80, 0.15)",
            color: saveStatus === "error" ? "var(--danger)" : "var(--success)",
            border: `1px solid ${saveStatus === "error" ? "var(--danger)" : "var(--success)"}`,
            pointerEvents: "none",
            transition: "opacity 0.2s",
          }}
        >
          {saveStatus === "saving"
            ? "Saving..."
            : saveStatus === "saved"
              ? "Settings saved"
              : "Failed to save"}
        </div>
      )}
    </div>
  );
}

export default Settings;
