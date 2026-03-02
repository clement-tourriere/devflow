import { useState, useEffect } from "react";
import { getSettings, saveSettings, listProjects } from "../../utils/invoke";
import type { AppSettings, ProjectEntry } from "../../types";

function Settings() {
  const [settings, setSettings] = useState<AppSettings | null>(null);
  const [projects, setProjects] = useState<ProjectEntry[]>([]);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    getSettings().then(setSettings).catch(console.error);
    listProjects().then(setProjects).catch(() => {});
  }, []);

  const handleSave = async () => {
    if (!settings) return;
    try {
      await saveSettings(settings);
      setSaved(true);
      setTimeout(() => setSaved(false), 2000);
    } catch (e) {
      alert(`Save failed: ${e}`);
    }
  };

  if (!settings) return <div>Loading...</div>;

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
        <div className="card-title">Application</div>
        <p style={{ color: "var(--text-secondary)", marginBottom: 12 }}>
          Application settings are stored locally and not synced.
        </p>
        <div className="flex gap-2">
          <button className="btn btn-primary" onClick={handleSave}>
            Save Settings
          </button>
        </div>
        {saved && (
          <div
            style={{
              marginTop: 8,
              padding: 12,
              background: "rgba(63, 185, 80, 0.1)",
              border: "1px solid var(--success)",
              borderRadius: 6,
              color: "var(--success)",
              fontSize: 13,
            }}
          >
            Settings saved.
          </div>
        )}
      </div>
    </div>
  );
}

export default Settings;
