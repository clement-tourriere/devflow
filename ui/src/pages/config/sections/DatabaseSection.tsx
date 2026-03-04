import { useState } from "react";
import type { FilledConfig, AuthMethod } from "../../../types/config";
import FormField from "../components/FormField";

interface Props {
  config: FilledConfig;
  onChange: (config: FilledConfig) => void;
}

const ALL_AUTH_METHODS: { value: AuthMethod; label: string }[] = [
  { value: "password", label: "Password" },
  { value: "pgpass", label: "pgpass file" },
  { value: "environment", label: "Environment variables" },
  { value: "service", label: "Service" },
  { value: "prompt", label: "Prompt" },
  { value: "system", label: "System" },
];

function DatabaseSection({ config, onChange }: Props) {
  const db = config.database;
  const [showPassword, setShowPassword] = useState(false);
  const [authExpanded, setAuthExpanded] = useState(false);

  const updateDb = (patch: Partial<typeof db>) => {
    onChange({ ...config, database: { ...db, ...patch } });
  };

  const updateAuth = (patch: Partial<typeof db.auth>) => {
    onChange({
      ...config,
      database: { ...db, auth: { ...db.auth, ...patch } },
    });
  };

  const toggleAuthMethod = (method: AuthMethod) => {
    const methods = db.auth.methods.includes(method)
      ? db.auth.methods.filter((m) => m !== method)
      : [...db.auth.methods, method];
    updateAuth({ methods });
  };

  return (
    <div>
      <h2 style={{ fontSize: 16, fontWeight: 600, marginBottom: 16 }}>Database</h2>

      <div className="grid grid-2">
        <FormField label="Host" description="PostgreSQL server hostname">
          <input
            type="text"
            value={db.host}
            onChange={(e) => updateDb({ host: e.target.value })}
            style={{ width: "100%", fontSize: 13 }}
          />
        </FormField>

        <FormField label="Port" description="PostgreSQL server port">
          <input
            type="number"
            value={db.port}
            onChange={(e) => updateDb({ port: parseInt(e.target.value) || 5432 })}
            style={{ width: "100%", fontSize: 13 }}
          />
        </FormField>
      </div>

      <div className="grid grid-2">
        <FormField label="User" description="PostgreSQL username">
          <input
            type="text"
            value={db.user}
            onChange={(e) => updateDb({ user: e.target.value })}
            style={{ width: "100%", fontSize: 13 }}
          />
        </FormField>

        <FormField label="Password" description="PostgreSQL password (optional)">
          <div style={{ display: "flex", gap: 6 }}>
            <input
              type={showPassword ? "text" : "password"}
              value={db.password || ""}
              onChange={(e) => updateDb({ password: e.target.value || null })}
              style={{ flex: 1, fontSize: 13 }}
              placeholder="Not set"
            />
            <button
              className="btn"
              onClick={() => setShowPassword(!showPassword)}
              style={{ fontSize: 11, padding: "4px 8px" }}
            >
              {showPassword ? "Hide" : "Show"}
            </button>
          </div>
        </FormField>
      </div>

      <FormField label="Template database" description="PostgreSQL template used for creating workspace databases">
        <input
          type="text"
          value={db.template_database}
          onChange={(e) => updateDb({ template_database: e.target.value })}
          style={{ width: "100%", fontSize: 13 }}
        />
      </FormField>

      <FormField label="Database prefix" description="Prefix for workspace database names">
        <input
          type="text"
          value={db.database_prefix}
          onChange={(e) => updateDb({ database_prefix: e.target.value })}
          style={{ width: "100%", fontSize: 13 }}
        />
      </FormField>

      {/* Auth subsection */}
      <div
        style={{
          marginTop: 16,
          border: "1px solid var(--border)",
          borderRadius: 8,
          overflow: "hidden",
        }}
      >
        <button
          onClick={() => setAuthExpanded(!authExpanded)}
          style={{
            width: "100%",
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            padding: "10px 12px",
            background: "var(--bg-tertiary)",
            border: "none",
            color: "var(--text-primary)",
            cursor: "pointer",
            fontSize: 13,
            fontWeight: 600,
          }}
        >
          <span>Authentication</span>
          <span style={{ fontSize: 11, color: "var(--text-muted)" }}>
            {authExpanded ? "▲" : "▼"} {db.auth.methods.length} method{db.auth.methods.length !== 1 ? "s" : ""}
          </span>
        </button>

        {authExpanded && (
          <div style={{ padding: 12 }}>
            <FormField label="Methods" description="Authentication methods to try, in order">
              <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
                {ALL_AUTH_METHODS.map(({ value, label }) => (
                  <label
                    key={value}
                    style={{ display: "flex", alignItems: "center", gap: 8, cursor: "pointer" }}
                  >
                    <input
                      type="checkbox"
                      checked={db.auth.methods.includes(value)}
                      onChange={() => toggleAuthMethod(value)}
                    />
                    <span style={{ fontSize: 13 }}>{label}</span>
                  </label>
                ))}
              </div>
            </FormField>

            {db.auth.methods.includes("pgpass") && (
              <FormField label="pgpass file" description="Path to pgpass file">
                <input
                  type="text"
                  value={db.auth.pgpass_file || ""}
                  onChange={(e) => updateAuth({ pgpass_file: e.target.value || null })}
                  placeholder="~/.pgpass"
                  style={{ width: "100%", fontSize: 13 }}
                />
              </FormField>
            )}

            {db.auth.methods.includes("service") && (
              <FormField label="Service name" description="pg_service.conf service name">
                <input
                  type="text"
                  value={db.auth.service_name || ""}
                  onChange={(e) => updateAuth({ service_name: e.target.value || null })}
                  style={{ width: "100%", fontSize: 13 }}
                />
              </FormField>
            )}

            <FormField label="Prompt for password" description="Interactively prompt for password when needed">
              <label style={{ display: "flex", alignItems: "center", gap: 8, cursor: "pointer" }}>
                <input
                  type="checkbox"
                  checked={db.auth.prompt_for_password}
                  onChange={(e) => updateAuth({ prompt_for_password: e.target.checked })}
                />
                <span style={{ fontSize: 13 }}>Enabled</span>
              </label>
            </FormField>
          </div>
        )}
      </div>
    </div>
  );
}

export default DatabaseSection;
