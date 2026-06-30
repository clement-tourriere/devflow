import { useMemo, useState } from "react";
import type {
  FilledConfig,
  ProcessDaemonConfig,
  ProcessesConfig,
  ProcessPortConfig,
  PitchforkProcessConfig,
  PitchforkWebUiConfig,
} from "../../../types/config";
import FormField from "../components/FormField";
import TagList from "../components/TagList";

interface Props {
  config: FilledConfig;
  onChange: (config: FilledConfig) => void;
}

const DEFAULT_PITCHFORK_WEB_UI: PitchforkWebUiConfig = {
  enabled: false,
  bind_port: 3120,
  bind_address: "127.0.0.1",
  edit_mode: "warn",
};

const DEFAULT_PITCHFORK: PitchforkProcessConfig = {
  config_policy: "devflow-owned",
  external_daemons: "show",
  web_ui: DEFAULT_PITCHFORK_WEB_UI,
};

const DEFAULT_PROCESSES: ProcessesConfig = {
  provider: "native",
  auto_start: false,
  auto_stop: true,
  daemons: {},
};

const DEFAULT_DAEMON: ProcessDaemonConfig = {
  run: "",
  required: true,
  depends: [],
  env: {},
  watch: [],
};

function parseNumberList(value: string): number[] {
  return value
    .split(/[ ,]+/)
    .map((part) => part.trim())
    .filter(Boolean)
    .map((part) => Number.parseInt(part, 10))
    .filter((n) => Number.isInteger(n) && n > 0 && n <= 65535);
}

function portExpect(port?: ProcessPortConfig | null): number[] {
  if (typeof port === "number") return [port];
  if (Array.isArray(port)) return port;
  return port?.expect ?? [];
}

function portBump(port?: ProcessPortConfig | null): string {
  if (!port || typeof port === "number" || Array.isArray(port)) return "0";
  if (port.bump === true) return "true";
  if (port.bump === false || port.bump == null) return "0";
  return String(port.bump);
}

function makePort(expectText: string, bumpText: string): ProcessPortConfig | null {
  const expect = parseNumberList(expectText);
  if (expect.length === 0) return null;
  const trimmed = bumpText.trim().toLowerCase();
  let bump: boolean | number = false;
  if (trimmed === "true" || trimmed === "unlimited") {
    bump = true;
  } else {
    const parsed = Number.parseInt(trimmed || "0", 10);
    bump = Number.isFinite(parsed) && parsed > 0 ? parsed : false;
  }
  return { expect, bump };
}

function nextDaemonName(daemons: Record<string, ProcessDaemonConfig>, preferred?: string): string {
  const candidates = preferred
    ? [preferred, `${preferred}-copy`, ...["web", "api", "worker", "scheduler"]]
    : ["web", "api", "worker", "scheduler"];
  for (const candidate of candidates) {
    if (!daemons[candidate]) return candidate;
  }
  let i = 1;
  const prefix = preferred ? `${preferred}-copy` : "process";
  while (daemons[`${prefix}-${i}`]) i += 1;
  return `${prefix}-${i}`;
}

function KeyValueEditor({
  values,
  onChange,
  keyPlaceholder = "KEY",
  valuePlaceholder = "value",
}: {
  values: Record<string, string>;
  onChange: (values: Record<string, string>) => void;
  keyPlaceholder?: string;
  valuePlaceholder?: string;
}) {
  const entries = Object.entries(values);

  const updateKey = (oldKey: string, newKey: string) => {
    const trimmed = newKey.trim();
    if (!trimmed || (trimmed !== oldKey && values[trimmed] !== undefined)) return;
    const next: Record<string, string> = {};
    for (const [key, value] of entries) {
      next[key === oldKey ? trimmed : key] = value;
    }
    onChange(next);
  };

  const updateValue = (key: string, value: string) => {
    onChange({ ...values, [key]: value });
  };

  const remove = (key: string) => {
    const next = { ...values };
    delete next[key];
    onChange(next);
  };

  const add = () => {
    let i = entries.length + 1;
    let key = `VAR_${i}`;
    while (values[key] !== undefined) {
      i += 1;
      key = `VAR_${i}`;
    }
    onChange({ ...values, [key]: "" });
  };

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
      {entries.map(([key, value]) => (
        <div key={key} style={{ display: "grid", gridTemplateColumns: "180px 1fr auto", gap: 6 }}>
          <input
            value={key}
            onChange={(e) => updateKey(key, e.target.value)}
            placeholder={keyPlaceholder}
            className="mono"
            style={{ fontSize: 12 }}
          />
          <input
            value={value}
            onChange={(e) => updateValue(key, e.target.value)}
            placeholder={valuePlaceholder}
            className="mono"
            style={{ fontSize: 12 }}
          />
          <button className="btn" onClick={() => remove(key)} style={{ padding: "4px 8px", fontSize: 12 }}>
            ×
          </button>
        </div>
      ))}
      <button className="btn" onClick={add} style={{ alignSelf: "flex-start", fontSize: 12 }}>
        + Add variable
      </button>
    </div>
  );
}

function ProcessesSection({ config, onChange }: Props) {
  const processes = config.processes;
  const [expanded, setExpanded] = useState<string | null>(null);

  const daemonEntries = useMemo(
    () => Object.entries(processes?.daemons ?? {}),
    [processes?.daemons]
  );

  const enable = () => {
    onChange({ ...config, processes: { ...DEFAULT_PROCESSES } });
  };

  const disable = () => {
    onChange({ ...config, processes: null });
  };

  const updateProcesses = (patch: Partial<ProcessesConfig>) => {
    if (!processes) return;
    onChange({ ...config, processes: { ...processes, ...patch } });
  };

  const updatePitchfork = (patch: Partial<PitchforkProcessConfig>) => {
    if (!processes) return;
    updateProcesses({ pitchfork: { ...DEFAULT_PITCHFORK, ...processes.pitchfork, ...patch } });
  };

  const updatePitchforkWebUi = (patch: Partial<PitchforkWebUiConfig>) => {
    const current = processes?.pitchfork?.web_ui ?? DEFAULT_PITCHFORK_WEB_UI;
    updatePitchfork({ web_ui: { ...current, ...patch } });
  };

  const updateDaemon = (name: string, patch: Partial<ProcessDaemonConfig>) => {
    if (!processes) return;
    const current = processes.daemons[name] ?? DEFAULT_DAEMON;
    updateProcesses({
      daemons: {
        ...processes.daemons,
        [name]: { ...current, ...patch },
      },
    });
  };

  const renameDaemon = (oldName: string, newName: string) => {
    if (!processes) return;
    const trimmed = newName.trim();
    if (!trimmed || trimmed === oldName || processes.daemons[trimmed]) return;
    const next: Record<string, ProcessDaemonConfig> = {};
    for (const [name, daemon] of Object.entries(processes.daemons)) {
      next[name === oldName ? trimmed : name] = daemon;
    }
    updateProcesses({ daemons: next });
    setExpanded(trimmed);
  };

  const addDaemon = () => {
    if (!processes) return;
    const name = nextDaemonName(processes.daemons);
    updateProcesses({ daemons: { ...processes.daemons, [name]: { ...DEFAULT_DAEMON } } });
    setExpanded(name);
  };

  const duplicateDaemon = (name: string) => {
    if (!processes) return;
    const copyName = nextDaemonName(processes.daemons, name);
    updateProcesses({
      daemons: {
        ...processes.daemons,
        [copyName]: { ...(processes.daemons[name] ?? DEFAULT_DAEMON) },
      },
    });
    setExpanded(copyName);
  };

  const deleteDaemon = (name: string) => {
    if (!processes) return;
    const next = { ...processes.daemons };
    delete next[name];
    updateProcesses({ daemons: next });
    if (expanded === name) setExpanded(null);
  };

  if (!processes) {
    return (
      <div>
        <h2 style={{ fontSize: 16, fontWeight: 600, marginBottom: 16 }}>Processes</h2>
        <div
          style={{
            padding: 24,
            textAlign: "center",
            border: "1px dashed var(--border)",
            borderRadius: 8,
            color: "var(--text-secondary)",
          }}
        >
          <p style={{ marginBottom: 12 }}>
            No workspace processes configured. Add app servers, workers, and schedulers here.
          </p>
          <button className="btn btn-primary" onClick={enable}>
            Configure Processes
          </button>
        </div>
      </div>
    );
  }

  const pitchfork = { ...DEFAULT_PITCHFORK, ...processes.pitchfork };
  const webUi = { ...DEFAULT_PITCHFORK_WEB_UI, ...pitchfork.web_ui };

  return (
    <div>
      <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", marginBottom: 16 }}>
        <h2 style={{ fontSize: 16, fontWeight: 600 }}>Processes</h2>
        <button className="btn btn-danger" onClick={disable} style={{ fontSize: 12 }}>
          Remove Processes Config
        </button>
      </div>

      <div className="card" style={{ margin: "0 0 16px", padding: 14 }}>
        <FormField label="Runtime provider" description="native uses devflow's built-in runner; pitchfork embeds Pitchfork's supervisor/log runtime.">
          <select
            value={processes.provider}
            onChange={(e) => {
              const provider = e.target.value as "native" | "pitchfork";
              updateProcesses({
                provider,
                pitchfork: provider === "pitchfork" ? { ...DEFAULT_PITCHFORK, ...processes.pitchfork } : processes.pitchfork,
              });
            }}
            style={{ width: "100%", fontSize: 13 }}
          >
            <option value="native">native</option>
            <option value="pitchfork">pitchfork</option>
          </select>
        </FormField>

        <div style={{ display: "flex", gap: 24, flexWrap: "wrap" }}>
          <label style={{ display: "flex", alignItems: "center", gap: 8, fontSize: 13 }}>
            <input
              type="checkbox"
              checked={processes.auto_start}
              onChange={(e) => updateProcesses({ auto_start: e.target.checked })}
            />
            Auto-start after switch
          </label>
          <label style={{ display: "flex", alignItems: "center", gap: 8, fontSize: 13 }}>
            <input
              type="checkbox"
              checked={processes.auto_stop}
              onChange={(e) => updateProcesses({ auto_stop: e.target.checked })}
            />
            Auto-stop on remove
          </label>
        </div>
      </div>

      {processes.provider === "pitchfork" && (
        <div className="card" style={{ margin: "0 0 16px", padding: 14 }}>
          <h3 style={{ fontSize: 14, marginBottom: 12 }}>Pitchfork reconciliation</h3>
          <FormField label="Config policy" description="How devflow treats Pitchfork config files for devflow-managed processes.">
            <select
              value={pitchfork.config_policy}
              onChange={(e) => updatePitchfork({ config_policy: e.target.value as PitchforkProcessConfig["config_policy"] })}
              style={{ width: "100%", fontSize: 13 }}
            >
              <option value="devflow-owned">devflow-owned (recommended)</option>
              <option value="import">import from Pitchfork</option>
              <option value="mirror">mirror generated config</option>
              <option value="merge">merge manually (advanced)</option>
            </select>
          </FormField>
          <FormField label="External Pitchfork daemons" description="How to display daemons that were not created by devflow.">
            <select
              value={pitchfork.external_daemons}
              onChange={(e) => updatePitchfork({ external_daemons: e.target.value as PitchforkProcessConfig["external_daemons"] })}
              style={{ width: "100%", fontSize: 13 }}
            >
              <option value="hide">hide</option>
              <option value="show">show separately</option>
              <option value="importable">show with import actions</option>
            </select>
          </FormField>

          <FormField label="Pitchfork Web UI bridge" description="Loopback-only bridge to Pitchfork's own Web UI. .devflow.yml remains the source of truth by default.">
            <label style={{ display: "flex", alignItems: "center", gap: 8, fontSize: 13, marginBottom: 8 }}>
              <input
                type="checkbox"
                checked={webUi.enabled}
                onChange={(e) => updatePitchforkWebUi({ enabled: e.target.checked })}
              />
              Show/open Pitchfork Web UI actions
            </label>
            <div style={{ display: "grid", gridTemplateColumns: "1fr 120px 160px", gap: 8 }}>
              <input
                value={webUi.bind_address ?? "127.0.0.1"}
                onChange={(e) => updatePitchforkWebUi({ bind_address: e.target.value || "127.0.0.1" })}
                placeholder="127.0.0.1"
                className="mono"
                style={{ fontSize: 12 }}
              />
              <input
                type="number"
                min={1}
                max={65535}
                value={webUi.bind_port ?? 3120}
                onChange={(e) => updatePitchforkWebUi({ bind_port: Number.parseInt(e.target.value, 10) || 3120 })}
                style={{ fontSize: 12 }}
              />
              <select
                value={webUi.edit_mode}
                onChange={(e) => updatePitchforkWebUi({ edit_mode: e.target.value as PitchforkWebUiConfig["edit_mode"] })}
                style={{ fontSize: 12 }}
              >
                <option value="readonly">readonly</option>
                <option value="warn">warn on edits</option>
                <option value="merge">merge edits</option>
              </select>
            </div>
          </FormField>
        </div>
      )}

      <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", marginBottom: 8 }}>
        <h3 style={{ fontSize: 14 }}>Daemons</h3>
        <button className="btn btn-primary" onClick={addDaemon} style={{ fontSize: 12 }}>
          + Add daemon
        </button>
      </div>

      {daemonEntries.length === 0 ? (
        <div style={{ padding: 18, border: "1px dashed var(--border)", borderRadius: 8, color: "var(--text-secondary)", textAlign: "center" }}>
          No daemons configured yet.
        </div>
      ) : (
        <div style={{ display: "flex", flexDirection: "column", gap: 10 }}>
          {daemonEntries.map(([name, daemon]) => {
            const isExpanded = expanded === name;
            const expectText = portExpect(daemon.port).join(", ");
            const bumpText = portBump(daemon.port);
            return (
              <div key={name} className="card" style={{ margin: 0, padding: 12 }}>
                <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: 10 }}>
                  <div style={{ minWidth: 0 }}>
                    <strong>{name}</strong>
                    <div className="mono" style={{ fontSize: 11, color: "var(--text-muted)", whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>
                      {daemon.run || "No command yet"}
                    </div>
                  </div>
                  <div style={{ display: "flex", gap: 6, flexWrap: "wrap" }}>
                    <button className="btn" onClick={() => setExpanded(isExpanded ? null : name)} style={{ fontSize: 12 }}>
                      {isExpanded ? "Collapse" : "Edit"}
                    </button>
                    <button className="btn" onClick={() => duplicateDaemon(name)} style={{ fontSize: 12 }}>
                      Duplicate
                    </button>
                    <button className="btn btn-danger" onClick={() => deleteDaemon(name)} style={{ fontSize: 12 }}>
                      Delete
                    </button>
                  </div>
                </div>

                {isExpanded && (
                  <div style={{ marginTop: 14, paddingTop: 14, borderTop: "1px solid var(--border)" }}>
                    <FormField label="Name" required>
                      <input value={name} onChange={(e) => renameDaemon(name, e.target.value)} style={{ width: "100%", fontSize: 13 }} />
                    </FormField>
                    <FormField label="Run command" required description="Shell command executed in the workspace/worktree.">
                      <input
                        value={daemon.run}
                        onChange={(e) => updateDaemon(name, { run: e.target.value })}
                        placeholder="npm run dev"
                        className="mono"
                        style={{ width: "100%", fontSize: 13 }}
                      />
                    </FormField>
                    <FormField label="Working directory" description="Relative to the workspace root, or absolute. Empty means workspace root.">
                      <input
                        value={daemon.dir ?? ""}
                        onChange={(e) => updateDaemon(name, { dir: e.target.value || null })}
                        placeholder="."
                        style={{ width: "100%", fontSize: 13 }}
                      />
                    </FormField>
                    <div style={{ display: "grid", gridTemplateColumns: "1fr 160px", gap: 12 }}>
                      <FormField label="Expected ports" description="Comma-separated. Empty disables port/proxy integration.">
                        <input
                          value={expectText}
                          onChange={(e) => updateDaemon(name, { port: makePort(e.target.value, bumpText) })}
                          placeholder="3000"
                          style={{ width: "100%", fontSize: 13 }}
                        />
                      </FormField>
                      <FormField label="Port bump" description="0, count, or true">
                        <input
                          value={bumpText}
                          onChange={(e) => updateDaemon(name, { port: makePort(expectText, e.target.value) })}
                          placeholder="50"
                          style={{ width: "100%", fontSize: 13 }}
                        />
                      </FormField>
                    </div>
                    <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 12 }}>
                      <FormField label="Ready port">
                        <input
                          type="number"
                          value={daemon.ready_port ?? ""}
                          onChange={(e) => updateDaemon(name, { ready_port: e.target.value ? Number.parseInt(e.target.value, 10) : null })}
                          style={{ width: "100%", fontSize: 13 }}
                        />
                      </FormField>
                      <FormField label="Ready delay (seconds)">
                        <input
                          type="number"
                          value={daemon.ready_delay ?? ""}
                          onChange={(e) => updateDaemon(name, { ready_delay: e.target.value ? Number.parseInt(e.target.value, 10) : null })}
                          style={{ width: "100%", fontSize: 13 }}
                        />
                      </FormField>
                    </div>
                    <FormField label="Ready HTTP">
                      <input
                        value={daemon.ready_http ?? ""}
                        onChange={(e) => updateDaemon(name, { ready_http: e.target.value || null })}
                        placeholder="http://127.0.0.1:3000/health"
                        className="mono"
                        style={{ width: "100%", fontSize: 13 }}
                      />
                    </FormField>
                    <FormField label="Ready command">
                      <input
                        value={daemon.ready_cmd ?? ""}
                        onChange={(e) => updateDaemon(name, { ready_cmd: e.target.value || null })}
                        className="mono"
                        style={{ width: "100%", fontSize: 13 }}
                      />
                    </FormField>
                    <div style={{ display: "grid", gridTemplateColumns: "1fr 160px", gap: 12 }}>
                      <FormField label="Ready output regex">
                        <input
                          value={daemon.ready_output ?? ""}
                          onChange={(e) => updateDaemon(name, { ready_output: e.target.value || null })}
                          style={{ width: "100%", fontSize: 13 }}
                        />
                      </FormField>
                      <FormField label="Ready timeout">
                        <input
                          type="number"
                          value={daemon.ready_timeout ?? ""}
                          onChange={(e) => updateDaemon(name, { ready_timeout: e.target.value ? Number.parseInt(e.target.value, 10) : null })}
                          style={{ width: "100%", fontSize: 13 }}
                        />
                      </FormField>
                    </div>
                    <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 12 }}>
                      <FormField label="Retry attempts">
                        <input
                          type="number"
                          value={daemon.retry ?? ""}
                          onChange={(e) => updateDaemon(name, { retry: e.target.value ? Number.parseInt(e.target.value, 10) : null })}
                          style={{ width: "100%", fontSize: 13 }}
                        />
                      </FormField>
                      <FormField label="Stop timeout">
                        <input
                          type="number"
                          value={daemon.stop_timeout ?? ""}
                          onChange={(e) => updateDaemon(name, { stop_timeout: e.target.value ? Number.parseInt(e.target.value, 10) : null })}
                          style={{ width: "100%", fontSize: 13 }}
                        />
                      </FormField>
                    </div>
                    <FormField label="Shutdown signal">
                      <select
                        value={daemon.shutdown_signal ?? ""}
                        onChange={(e) => updateDaemon(name, { shutdown_signal: e.target.value || null })}
                        style={{ width: "100%", fontSize: 13 }}
                      >
                        <option value="">Default</option>
                        <option value="TERM">TERM</option>
                        <option value="INT">INT</option>
                        <option value="HUP">HUP</option>
                        <option value="QUIT">QUIT</option>
                        <option value="KILL">KILL</option>
                      </select>
                    </FormField>
                    <FormField label="Depends on" description="Processes that start before this one.">
                      <TagList values={daemon.depends ?? []} onChange={(values) => updateDaemon(name, { depends: values })} placeholder="worker" />
                    </FormField>
                    <FormField label="Watch globs" description="The devflow daemon restarts the process when these files change.">
                      <TagList values={daemon.watch ?? []} onChange={(values) => updateDaemon(name, { watch: values })} placeholder="src/**/*.ts" />
                    </FormField>
                    <FormField label="Environment" description="Values can use devflow MiniJinja templates such as {{ service['app-db'].url }}.">
                      <KeyValueEditor values={daemon.env ?? {}} onChange={(values) => updateDaemon(name, { env: values })} />
                    </FormField>
                    <label style={{ display: "flex", alignItems: "center", gap: 8, fontSize: 13 }}>
                      <input
                        type="checkbox"
                        checked={daemon.required ?? true}
                        onChange={(e) => updateDaemon(name, { required: e.target.checked })}
                      />
                      Required process (failure blocks lifecycle commands)
                    </label>
                  </div>
                )}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

export default ProcessesSection;
