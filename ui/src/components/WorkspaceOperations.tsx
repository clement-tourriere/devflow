import { useCallback, useEffect, useState } from "react";
import { cancelOperation, listOperations, resumeOperation } from "../utils/invoke";
import { toast } from "../utils/notify";
import type { WorkspaceOperation } from "../types";

export default function WorkspaceOperations({ projectPath, revision, onUpdated }: {
  projectPath: string; revision: unknown; onUpdated: () => Promise<void>;
}) {
  const [operations, setOperations] = useState<WorkspaceOperation[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const reload = useCallback(async () => {
    try {
      setOperations((await listOperations(projectPath)).filter((op) => !["ready", "cancelled"].includes(op.status)));
      setError(null);
    } catch (error) { setError(String(error)); }
  }, [projectPath]);
  useEffect(() => { void reload(); }, [reload, revision]);
  const act = async (operation: WorkspaceOperation, cancel: boolean) => {
    setBusy(operation.id);
    try {
      if (cancel) await cancelOperation(projectPath, operation.id);
      else await resumeOperation(projectPath, operation.id);
      await reload();
      await onUpdated();
    } catch (error) { toast.error(String(error)); await reload(); }
    finally { setBusy(null); }
  };
  if (!error && operations.length === 0) return null;
  return <div className="card" style={{marginBottom: 16}}>
    <h3>Workspace recovery</h3>
    {error && <p role="alert">Could not load operations: {error}</p>}
    {operations.map((operation) => <div key={operation.id} style={{marginTop: 12}}>
      <strong>{operation.workspace}</strong>: {operation.kind} — {operation.status} ({operation.phase})
      <div style={{display: "flex", gap: 8, marginTop: 6}}>
        <button className="btn" disabled={busy !== null} onClick={() => void act(operation, false)}>{busy === operation.id ? "Working…" : "Resume"}</button>
        {operation.mutation_started === false && <button className="btn" disabled={busy !== null} onClick={() => void act(operation, true)}>Cancel unused operation</button>}
      </div>
      <details><summary>Inspect operation</summary><pre style={{overflowX: "auto", maxHeight: 300}}>{JSON.stringify(operation, null, 2)}</pre></details>
    </div>)}
  </div>;
}
