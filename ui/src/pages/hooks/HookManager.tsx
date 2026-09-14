import { useEffect, useState } from "react";
import { Link, useParams } from "react-router-dom";
import { listHooks, renderTemplate } from "../../utils/invoke";
import type { HookPhaseEntry } from "../../types";

export default function HookManager() {
  const { "*": path } = useParams();
  const projectPath = path ? decodeURIComponent(path) : "";
  const [phases, setPhases] = useState<HookPhaseEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [template, setTemplate] = useState("");
  const [workspace, setWorkspace] = useState("");
  const [rendered, setRendered] = useState<string | null>(null);
  useEffect(() => {
    let current = true;
    listHooks(projectPath).then(value => { if (current) setPhases(value); })
      .catch(error => { if (current) setError(String(error)); });
    return () => { current = false; };
  }, [projectPath]);
  const preview = async () => {
    try { setRendered(await renderTemplate(projectPath, template, workspace || undefined)); setError(null); }
    catch (error) { setError(String(error)); }
  };
  return <div>
    <Link to={`/projects/${encodeURIComponent(projectPath)}`}>&larr; Back to project</Link>
    <h1 className="page-title">Lifecycle hooks</h1>
    <p>Inspect the setup steps for this project. <Link to={`/config/${encodeURIComponent(projectPath)}`}>Edit configuration</Link></p>
    {error && <p role="alert" style={{ color: "var(--danger)" }}>{error}</p>}
    {phases.filter(phase => phase.hooks.length > 0).map(phase => <section className="card" key={phase.phase} style={{ marginTop: 16 }}>
      <h2>{phase.phase}</h2>
      {phase.hooks.map(hook => <div key={hook.name} style={{ marginBottom: 16 }}>
        <strong>{hook.name}</strong>{hook.background && <span> · background</span>}
        {hook.condition && <p>When: <code>{hook.condition}</code></p>}
        <pre style={{ whiteSpace: "pre-wrap", overflowWrap: "anywhere" }}>{hook.command}</pre>
      </div>)}
    </section>)}
    {phases.every(phase => phase.hooks.length === 0) && <p>No lifecycle hooks configured.</p>}
    <section className="card" style={{ marginTop: 20 }}>
      <h2>Preview a template</h2>
      <label>Workspace<input value={workspace} onChange={event => setWorkspace(event.target.value)} placeholder="Current workspace" /></label>
      <label>Template<textarea value={template} onChange={event => setTemplate(event.target.value)} /></label>
      <button className="btn" onClick={preview} disabled={!template.trim()}>Render</button>
      {rendered !== null && <pre style={{ whiteSpace: "pre-wrap" }}>{rendered}</pre>}
    </section>
  </div>;
}
