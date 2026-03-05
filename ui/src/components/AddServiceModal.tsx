import { useState, useCallback, useEffect } from "react";
import Modal from "./Modal";
import { discoverDockerContainers } from "../utils/invoke";
import type { DiscoveredContainer, AddServiceRequest } from "../types";

interface AddServiceModalProps {
  open: boolean;
  onClose: () => void;
  onAdd: (request: AddServiceRequest) => Promise<void>;
  projectPath?: string;
  /** Label for the submit button (default: "Add Service") */
  submitLabel?: string;
  /** Label while submitting (default: "Adding...") */
  submittingLabel?: string;
}

const defaultImageForType = (type: string) => {
  switch (type) {
    case "postgres": return "postgres:17";
    case "clickhouse": return "clickhouse/clickhouse-server:latest";
    case "mysql": return "mysql:8";
    default: return "";
  }
};

const defaultNameForType = (type: string) => {
  switch (type) {
    case "postgres": return "app-db";
    case "clickhouse": return "analytics";
    case "mysql": return "app-db";
    case "generic": return "cache";
    default: return "service";
  }
};

const SERVICE_TYPE_LABELS: Record<string, string> = {
  postgres: "PostgreSQL",
  clickhouse: "ClickHouse",
  mysql: "MySQL",
  generic: "Generic Docker",
};

function AddServiceModal({
  open,
  onClose,
  onAdd,
  projectPath,
  submitLabel = "Add Service",
  submittingLabel = "Adding...",
}: AddServiceModalProps) {
  const [svcType, setSvcType] = useState("postgres");
  const [provider, setProvider] = useState("local");
  const [name, setName] = useState("app-db");
  const [image, setImage] = useState("postgres:17");
  const [seed, setSeed] = useState("");
  const [autoWorkspace, setAutoWorkspace] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  // Docker discovery state
  const [discoveredContainers, setDiscoveredContainers] = useState<DiscoveredContainer[]>([]);
  const [discoveryLoading, setDiscoveryLoading] = useState(false);
  const [discoveryExpanded, setDiscoveryExpanded] = useState(true);
  const [selectedDiscovery, setSelectedDiscovery] = useState<string | null>(null);

  const runDiscovery = useCallback(async (serviceType: string) => {
    if (!projectPath) {
      setDiscoveredContainers([]);
      setSelectedDiscovery(null);
      setDiscoveryLoading(false);
      return;
    }

    setDiscoveryLoading(true);
    setDiscoveredContainers([]);
    setSelectedDiscovery(null);
    try {
      const containers = await discoverDockerContainers(serviceType, {
        projectPath,
      });
      setDiscoveredContainers(containers);
    } catch {
      setDiscoveredContainers([]);
    } finally {
      setDiscoveryLoading(false);
    }
  }, [projectPath]);

  // Reset form when modal opens
  useEffect(() => {
    if (open) {
      setSvcType("postgres");
      setProvider("local");
      setName("app-db");
      setImage("postgres:17");
      setSeed("");
      setAutoWorkspace(true);
      setError(null);
      setSelectedDiscovery(null);
      setDiscoveryExpanded(true);
      runDiscovery("postgres");
    }
  }, [open, runDiscovery]);

  const handleSelectDiscoveredContainer = (container: DiscoveredContainer) => {
    if (selectedDiscovery === container.container_id) {
      setSelectedDiscovery(null);
      setImage(defaultImageForType(svcType));
      setSeed("");
      setName(defaultNameForType(svcType));
      return;
    }
    setSelectedDiscovery(container.container_id);
    setImage(container.image);
    setSeed(container.connection_url);
    const derivedName = container.compose_service || container.container_name.replace(/[^a-zA-Z0-9-]/g, "-");
    setName(derivedName);
  };

  const handleServiceTypeChange = (type: string) => {
    setSvcType(type);
    setName(defaultNameForType(type));
    setImage(defaultImageForType(type));
    setSelectedDiscovery(null);
    if (type !== "postgres") {
      setProvider("local");
    }
    runDiscovery(type);
  };

  const handleSubmit = async () => {
    if (!name.trim()) {
      setError("Service name is required");
      return;
    }
    if (svcType === "generic" && !image.trim()) {
      setError("Docker image is required for generic services");
      return;
    }
    setError(null);
    setSubmitting(true);
    try {
      const request: AddServiceRequest = {
        name: name.trim(),
        service_type: svcType,
        provider_type: provider,
        auto_workspace: autoWorkspace,
        image: image.trim() || undefined,
        seed_from: seed.trim() || undefined,
      };
      await onAdd(request);
      onClose();
    } catch (e) {
      setError(`${e}`);
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <Modal open={open} onClose={onClose} title="Add Service" width={500}>
      {/* Service Type */}
      <div style={{ marginBottom: 16 }}>
        <label style={{ display: "block", marginBottom: 6, fontSize: 13, color: "var(--text-secondary)", fontWeight: 500 }}>
          Service Type
        </label>
        <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 8 }}>
          {(["postgres", "clickhouse", "mysql", "generic"] as const).map((t) => (
            <button
              key={t}
              onClick={() => handleServiceTypeChange(t)}
              style={{
                padding: "10px 12px",
                border: `2px solid ${svcType === t ? "var(--accent)" : "var(--border)"}`,
                borderRadius: 8,
                background: svcType === t ? "var(--bg-hover)" : "var(--bg-primary)",
                color: "var(--text-primary)",
                cursor: "pointer",
                textAlign: "left",
                fontSize: 14,
                fontWeight: svcType === t ? 600 : 400,
              }}
            >
              {SERVICE_TYPE_LABELS[t]}
            </button>
          ))}
        </div>
      </div>

      {/* Detected Docker Containers */}
      {(discoveryLoading || discoveredContainers.length > 0) && (
        <div style={{ marginBottom: 16 }}>
          <button
            onClick={() => setDiscoveryExpanded(!discoveryExpanded)}
            style={{
              display: "flex",
              alignItems: "center",
              gap: 6,
              background: "none",
              border: "none",
              color: "var(--text-secondary)",
              cursor: "pointer",
              padding: 0,
              fontSize: 13,
              fontWeight: 500,
              marginBottom: discoveryExpanded ? 8 : 0,
            }}
          >
            <span style={{ transform: discoveryExpanded ? "rotate(90deg)" : "rotate(0deg)", transition: "transform 0.15s", display: "inline-block" }}>&#9654;</span>
            Detected Docker Containers
            {discoveredContainers.length > 0 && (
              <span style={{ background: "var(--accent)", color: "#fff", borderRadius: 10, padding: "1px 7px", fontSize: 11, fontWeight: 600, marginLeft: 4 }}>
                {discoveredContainers.length}
              </span>
            )}
          </button>
          {discoveryExpanded && (
            <div>
              {discoveryLoading ? (
                <div style={{ color: "var(--text-muted)", fontSize: 13, padding: "8px 0" }}>
                  Scanning Docker containers...
                </div>
              ) : discoveredContainers.length === 0 ? (
                <div style={{ color: "var(--text-muted)", fontSize: 13, padding: "4px 0" }}>
                  No matching containers found.
                </div>
              ) : (
                <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
                  {discoveredContainers.map((c) => (
                    <button
                      key={c.container_id}
                      onClick={() => handleSelectDiscoveredContainer(c)}
                      style={{
                        display: "flex",
                        flexDirection: "column",
                        gap: 2,
                        padding: "8px 12px",
                        border: `2px solid ${selectedDiscovery === c.container_id ? "var(--accent)" : "var(--border)"}`,
                        borderRadius: 8,
                        background: selectedDiscovery === c.container_id ? "var(--bg-hover)" : "var(--bg-primary)",
                        color: "var(--text-primary)",
                        cursor: "pointer",
                        textAlign: "left",
                        fontSize: 13,
                      }}
                    >
                      <div style={{ fontWeight: 600 }}>
                        {c.container_name}
                        {c.is_compose && c.compose_project && (
                          <span style={{ fontWeight: 400, color: "var(--text-muted)", marginLeft: 6, fontSize: 11 }}>
                            compose: {c.compose_project}
                          </span>
                        )}
                      </div>
                      <div style={{ color: "var(--text-secondary)", fontSize: 12 }}>
                        {c.image} &middot; {c.host}:{c.port}
                        {c.username && <span> &middot; {c.username}</span>}
                      </div>
                    </button>
                  ))}
                </div>
              )}
            </div>
          )}
        </div>
      )}

      {/* Provider (only for postgres) */}
      {svcType === "postgres" && (
        <div style={{ marginBottom: 16 }}>
          <label style={{ display: "block", marginBottom: 4, fontSize: 13, color: "var(--text-secondary)", fontWeight: 500 }}>
            Provider
          </label>
          <select
            value={provider}
            onChange={(e) => setProvider(e.target.value)}
            style={{ width: "100%" }}
          >
            <option value="local">Local Docker</option>
            <option value="neon">Neon</option>
            <option value="dblab">DBLab</option>
            <option value="xata">Xata</option>
            <option value="postgres_template">PostgreSQL Template</option>
          </select>
        </div>
      )}

      {/* Name */}
      <div style={{ marginBottom: 12 }}>
        <label style={{ display: "block", marginBottom: 4, fontSize: 13, color: "var(--text-secondary)", fontWeight: 500 }}>
          Service Name
        </label>
        <input
          type="text"
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder="e.g. app-db"
          style={{ width: "100%" }}
        />
      </div>

      {/* Docker Image (only for local-ish providers) */}
      {(provider === "local" || svcType !== "postgres") && (
        <div style={{ marginBottom: 12 }}>
          <label style={{ display: "block", marginBottom: 4, fontSize: 13, color: "var(--text-secondary)", fontWeight: 500 }}>
            Docker Image
          </label>
          <input
            type="text"
            value={image}
            onChange={(e) => setImage(e.target.value)}
            placeholder={svcType === "generic" ? "e.g. redis:7" : defaultImageForType(svcType)}
            style={{ width: "100%" }}
          />
        </div>
      )}

      {/* Seed From */}
      <div style={{ marginBottom: 12 }}>
        <label style={{ display: "block", marginBottom: 4, fontSize: 13, color: "var(--text-secondary)", fontWeight: 500 }}>
          Seed From <span style={{ fontWeight: 400, color: "var(--text-muted)" }}>(optional)</span>
        </label>
        <input
          type="text"
          value={seed}
          onChange={(e) => setSeed(e.target.value)}
          placeholder="URL, file path, or s3://..."
          style={{ width: "100%" }}
        />
      </div>

      {/* Auto-workspace toggle */}
      <div style={{ marginBottom: 16 }}>
        <label style={{ display: "flex", alignItems: "center", gap: 8, fontSize: 14, cursor: "pointer" }}>
          <input
            type="checkbox"
            checked={autoWorkspace}
            onChange={(e) => setAutoWorkspace(e.target.checked)}
          />
          <span style={{ color: "var(--text-primary)" }}>Auto-workspace on git checkout</span>
        </label>
      </div>

      {/* Error message */}
      {error && (
        <div style={{ marginBottom: 12, padding: "8px 12px", background: "var(--danger-bg, rgba(255,0,0,0.1))", border: "1px solid var(--danger)", borderRadius: 6, color: "var(--danger)", fontSize: 13 }}>
          {error}
        </div>
      )}

      {/* Footer */}
      <div style={{ display: "flex", gap: 8, justifyContent: "flex-end" }}>
        <button className="btn" onClick={onClose}>
          Cancel
        </button>
        <button
          className="btn btn-primary"
          onClick={handleSubmit}
          disabled={!name.trim() || submitting}
        >
          {submitting ? submittingLabel : submitLabel}
        </button>
      </div>
    </Modal>
  );
}

export default AddServiceModal;
