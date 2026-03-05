import { useState } from "react";
import type { FilledConfig, NamedServiceConfig } from "../../../types/config";
import type { AddServiceRequest } from "../../../types";
import AddServiceModal from "../../../components/AddServiceModal";

interface Props {
  config: FilledConfig;
  onChange: (config: FilledConfig) => void;
  projectPath?: string;
}

const SERVICE_TYPE_COLORS: Record<string, string> = {
  postgres: "var(--accent)",
  clickhouse: "var(--warning)",
  mysql: "#f0883e",
  generic: "var(--text-muted)",
};

function ServicesSection({ config, onChange, projectPath }: Props) {
  const services = config.services || [];
  const [adding, setAdding] = useState(false);

  const handleAdd = async (request: AddServiceRequest) => {
    const newService: NamedServiceConfig = {
      name: request.name,
      type: request.provider_type,
      service_type: request.service_type,
      auto_workspace: request.auto_workspace ?? true,
      default: false,
    };
    // Set provider-specific config based on type
    if (request.provider_type === "local" && request.image) {
      newService.local = { image: request.image };
    }
    const updated = [...services, newService];
    onChange({ ...config, services: updated });
  };

  const handleDelete = (index: number) => {
    const updated = services.filter((_, i) => i !== index);
    onChange({ ...config, services: updated.length > 0 ? updated : null });
  };

  return (
    <div>
      <div
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          marginBottom: 16,
        }}
      >
        <h2 style={{ fontSize: 16, fontWeight: 600 }}>Services</h2>
        <button className="btn btn-primary" onClick={() => setAdding(true)}>
          + Add Service
        </button>
      </div>

      {services.length === 0 && (
        <div
          style={{
            padding: 24,
            textAlign: "center",
            border: "1px dashed var(--border)",
            borderRadius: 8,
            color: "var(--text-secondary)",
          }}
        >
          No services configured. Add a service to get started.
        </div>
      )}

      <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
        {services.map((svc, i) => (
          <div
            key={i}
            className="card"
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              padding: "10px 14px",
              margin: 0,
            }}
          >
            <div style={{ display: "flex", alignItems: "center", gap: 10 }}>
              <span style={{ fontWeight: 600, fontSize: 14 }}>{svc.name}</span>
              <span
                className="badge"
                style={{
                  background: SERVICE_TYPE_COLORS[svc.service_type] || "var(--text-muted)",
                  color: "#fff",
                  fontSize: 10,
                }}
              >
                {svc.service_type}
              </span>
              <span
                className="badge badge-info"
                style={{ fontSize: 10 }}
              >
                {svc.type}
              </span>
              {svc.default && (
                <span className="badge badge-success" style={{ fontSize: 10 }}>default</span>
              )}
            </div>
            <div style={{ display: "flex", gap: 6 }}>
              <button
                className="btn btn-danger"
                onClick={() => handleDelete(i)}
                style={{ fontSize: 11, padding: "3px 8px" }}
              >
                Delete
              </button>
            </div>
          </div>
        ))}
      </div>

      <AddServiceModal
        open={adding}
        onClose={() => setAdding(false)}
        onAdd={handleAdd}
        projectPath={projectPath}
      />
    </div>
  );
}

export default ServicesSection;
