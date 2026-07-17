import { useState, forwardRef, useImperativeHandle } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import {
  addOrInitProject,
  detectVcsInfo,
  detectVcsWorkspaces,
  getProjectDetail,
} from "../utils/invoke";
import type { VcsInfo, VcsWorkspaceInfo } from "../types";
import Modal from "./Modal";

export interface AddProjectModalHandle {
  open: () => void;
}

interface Props {
  onProjectAdded?: (projectPath: string) => void;
}

function sanitizeProjectName(raw: string): string {
  return raw.trim().slice(0, 100);
}

function basenameFromPath(path: string): string {
  const parts = path.replace(/[\\/]+$/, "").split(/[\\/]/);
  return parts[parts.length - 1] || "unknown";
}

const AddProjectModal = forwardRef<AddProjectModalHandle, Props>(
  ({ onProjectAdded }, ref) => {
    const [modalOpen, setModalOpen] = useState(false);
    const [selectedPath, setSelectedPath] = useState("");
    const [projectName, setProjectName] = useState("");
    const [modalError, setModalError] = useState("");
    const [modalLoading, setModalLoading] = useState(false);

    const [vcsInfo, setVcsInfo] = useState<VcsInfo | null>(null);
    const [hasExistingConfig, setHasExistingConfig] = useState(false);

    const [workspaceInfo, setWorkspaceInfo] = useState<VcsWorkspaceInfo | null>(null);
    const [selectedWorkspace, setSelectedWorkspace] = useState<string>("");
    const [workspaceSearch, setWorkspaceSearch] = useState("");

    const [selectedVcs, setSelectedVcs] = useState<string>("git");

    const resetState = () => {
      setModalOpen(false);
      setSelectedPath("");
      setProjectName("");
      setModalError("");
      setModalLoading(false);
      setVcsInfo(null);
      setSelectedVcs("git");
      setHasExistingConfig(false);
      setWorkspaceInfo(null);
      setSelectedWorkspace("");
      setWorkspaceSearch("");
    };

    const openModal = async () => {
      try {
        const selected = await open({ directory: true, multiple: false });
        if (!selected) return;
        const dirPath = selected as string;
        const defaultName = sanitizeProjectName(basenameFromPath(dirPath));
        setSelectedPath(dirPath);
        setProjectName(defaultName);
        setModalError("");

        let detectedVcsInfo: VcsInfo | null = null;
        try {
          detectedVcsInfo = await detectVcsInfo(dirPath);
          setVcsInfo(detectedVcsInfo);
          setSelectedVcs(detectedVcsInfo.existing_vcs || detectedVcsInfo.available_tools[0] || "git");
        } catch {
          setVcsInfo(null);
          setSelectedVcs("git");
        }

        if (detectedVcsInfo?.existing_vcs) {
          try {
            const info = await detectVcsWorkspaces(dirPath);
            setWorkspaceInfo(info);
            setSelectedWorkspace(info.default_workspace || "");
          } catch {
            setWorkspaceInfo(null);
          }
        }

        try {
          const detail = await getProjectDetail(dirPath);
          setHasExistingConfig(detail.has_config);
        } catch {
          setHasExistingConfig(false);
        }

        setModalOpen(true);
      } catch (e) {
        console.error("Failed to open directory picker:", e);
      }
    };

    useImperativeHandle(ref, () => ({ open: openModal }));

    const handleSubmit = async () => {
      const normalized = sanitizeProjectName(projectName);
      if (!normalized) {
        setModalError("Project name cannot be empty.");
        return;
      }

      setModalLoading(true);
      setModalError("");
      try {
        const vcsPref = vcsInfo?.existing_vcs ? undefined : selectedVcs;
        await addOrInitProject(
          selectedPath,
          normalized,
          vcsPref,
          selectedWorkspace || undefined,
        );
        window.dispatchEvent(new CustomEvent("devflow:projects-changed"));
        const path = selectedPath;
        resetState();
        onProjectAdded?.(path);
      } catch (e) {
        setModalError(`${e}`);
      } finally {
        setModalLoading(false);
      }
    };

    const normalized = sanitizeProjectName(projectName);
    const showVcsSelector = vcsInfo && !vcsInfo.existing_vcs && !hasExistingConfig;

    return (
      <Modal
        open={modalOpen}
        onClose={resetState}
        title="Add Project"
        width={520}
      >
        {hasExistingConfig && (
          <div
            style={{
              marginBottom: 16,
              padding: "8px 12px",
              background: "var(--info-bg, rgba(0,120,255,0.08))",
              border: "1px solid var(--info, #3b82f6)",
              borderRadius: 6,
              fontSize: 13,
              color: "var(--text-secondary)",
            }}
          >
            Existing <strong className="mono">.devflow.yml</strong> detected — your configuration will be preserved.
          </div>
        )}

        <div style={{ marginBottom: 16 }}>
          <label
            style={{
              display: "block",
              marginBottom: 6,
              fontSize: 13,
              color: "var(--text-secondary)",
              fontWeight: 500,
            }}
          >
            Directory
          </label>
          <div
            className="mono"
            style={{
              padding: "8px 12px",
              background: "var(--bg-primary)",
              border: "1px solid var(--border)",
              borderRadius: 6,
              fontSize: 13,
              color: "var(--text-primary)",
              wordBreak: "break-all",
            }}
          >
            {selectedPath}
          </div>
        </div>

        <div style={{ marginBottom: 4 }}>
          <label
            style={{
              display: "block",
              marginBottom: 6,
              fontSize: 13,
              color: "var(--text-secondary)",
              fontWeight: 500,
            }}
          >
            Project Name
          </label>
          <input
            type="text"
            value={projectName}
            onChange={(e) => {
              setProjectName(e.target.value);
              setModalError("");
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !modalLoading) handleSubmit();
            }}
            placeholder="e.g. my-project"
            maxLength={100}
            autoFocus
            style={{ width: "100%" }}
          />
        </div>

        {showVcsSelector && vcsInfo && vcsInfo.available_tools.length > 1 && (
          <div style={{ marginBottom: 16 }}>
            <label
              style={{
                display: "block",
                marginBottom: 6,
                fontSize: 13,
                color: "var(--text-secondary)",
                fontWeight: 500,
              }}
            >
              Version Control
            </label>
            <div className="flex gap-2">
              {vcsInfo.available_tools.map((tool) => (
                <button
                  key={tool}
                  className={`btn${selectedVcs === tool ? " btn-primary" : ""}`}
                  style={{ padding: "4px 16px", fontSize: 13 }}
                  onClick={() => setSelectedVcs(tool)}
                  type="button"
                >
                  {tool}
                </button>
              ))}
            </div>
          </div>
        )}

        {vcsInfo?.existing_vcs && (
          <div style={{ marginBottom: 16 }}>
            <label
              style={{
                display: "block",
                marginBottom: 6,
                fontSize: 13,
                color: "var(--text-secondary)",
                fontWeight: 500,
              }}
            >
              Version Control
            </label>
            <div
              style={{
                display: "flex",
                alignItems: "center",
                gap: 8,
                fontSize: 13,
              }}
            >
              <span className="badge badge-info">
                {vcsInfo.existing_vcs}
              </span>
              <span style={{ color: "var(--text-muted)" }}>
                already initialized
              </span>
            </div>
          </div>
        )}

        {vcsInfo?.existing_vcs && workspaceInfo && (
          <div style={{ marginBottom: 16 }}>
            <label
              style={{
                display: "block",
                marginBottom: 6,
                fontSize: 13,
                color: "var(--text-secondary)",
                fontWeight: 500,
              }}
            >
              Default Workspace
            </label>
            {workspaceInfo.workspaces.length > 0 ? (
              <>
                {workspaceInfo.workspaces.length > 5 && (
                  <input
                    type="text"
                    value={workspaceSearch}
                    onChange={(e) => setWorkspaceSearch(e.target.value)}
                    placeholder="Search workspaces..."
                    style={{ width: "100%", marginBottom: 6, fontSize: 13 }}
                  />
                )}
                <div
                  style={{
                    maxHeight: 140,
                    overflowY: "auto",
                    border: "1px solid var(--border)",
                    borderRadius: 6,
                    background: "var(--bg-primary)",
                  }}
                >
                  {workspaceInfo.workspaces
                    .filter((workspace) =>
                      !workspaceSearch || workspace.toLowerCase().includes(workspaceSearch.toLowerCase())
                    )
                    .map((workspace) => {
                      const isDefault = workspace === workspaceInfo.default_workspace;
                      const isSelected = workspace === selectedWorkspace;
                      return (
                        <div
                          key={workspace}
                          onClick={() => setSelectedWorkspace(workspace)}
                          style={{
                            padding: "6px 10px",
                            cursor: "pointer",
                            fontSize: 13,
                            display: "flex",
                            alignItems: "center",
                            gap: 8,
                            background: isSelected
                              ? "var(--accent-bg, rgba(59,130,246,0.12))"
                              : "transparent",
                            borderBottom: "1px solid var(--border)",
                          }}
                        >
                          <span
                            className="mono"
                            style={{
                              fontWeight: isSelected ? 600 : 400,
                              color: isSelected
                                ? "var(--accent)"
                                : "var(--text-primary)",
                            }}
                          >
                            {workspace}
                          </span>
                          {isDefault && (
                            <span
                              className="badge badge-info"
                              style={{ fontSize: 10, padding: "1px 6px" }}
                            >
                              detected
                            </span>
                          )}
                        </div>
                      );
                    })}
                </div>
              </>
            ) : (
              <input
                type="text"
                value={selectedWorkspace}
                onChange={(e) => setSelectedWorkspace(e.target.value)}
                placeholder="e.g. main"
                style={{ width: "100%", fontSize: 13 }}
              />
            )}
          </div>
        )}

        <div
          style={{
            marginBottom: 16,
            padding: "8px 12px",
            border: "1px solid var(--border)",
            borderRadius: 6,
            color: "var(--text-muted)",
            fontSize: 12,
          }}
        >
          Each workspace gets its own directory so work can run in parallel without
          changing the project checkout in place.
        </div>

        {modalError && (
          <div
            style={{
              marginBottom: 12,
              padding: "8px 12px",
              background: "var(--danger-bg, rgba(255,0,0,0.1))",
              border: "1px solid var(--danger)",
              borderRadius: 6,
              color: "var(--danger)",
              fontSize: 13,
            }}
          >
            {modalError}
          </div>
        )}

        <div style={{ display: "flex", gap: 8, justifyContent: "flex-end" }}>
          <button className="btn" onClick={resetState}>
            Cancel
          </button>
          <button
            className="btn btn-primary"
            onClick={handleSubmit}
            disabled={!normalized || modalLoading}
          >
            {modalLoading ? "Adding..." : "Add Project"}
          </button>
        </div>
      </Modal>
    );
  }
);

AddProjectModal.displayName = "AddProjectModal";

export default AddProjectModal;
