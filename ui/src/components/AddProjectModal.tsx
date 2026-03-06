import { useState, forwardRef, useImperativeHandle } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import {
  addOrInitProject,
  detectVcsInfo,
  detectGitBranches,
  getProjectDetail,
} from "../utils/invoke";
import type { VcsInfo, GitBranchInfo } from "../types";
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
    const [hasWorktreeConfig, setHasWorktreeConfig] = useState(false);

    const [branchInfo, setBranchInfo] = useState<GitBranchInfo | null>(null);
    const [selectedBranch, setSelectedBranch] = useState<string>("");
    const [branchSearch, setBranchSearch] = useState("");

    const [selectedVcs, setSelectedVcs] = useState<string>("git");
    const [branchingMode, setBranchingMode] = useState<"worktree" | "workspace">("worktree");

    const resetState = () => {
      setModalOpen(false);
      setSelectedPath("");
      setProjectName("");
      setModalError("");
      setModalLoading(false);
      setVcsInfo(null);
      setSelectedVcs("git");
      setBranchingMode("worktree");
      setHasExistingConfig(false);
      setHasWorktreeConfig(false);
      setBranchInfo(null);
      setSelectedBranch("");
      setBranchSearch("");
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
            const info = await detectGitBranches(dirPath);
            setBranchInfo(info);
            setSelectedBranch(info.default_branch || "");
          } catch {
            setBranchInfo(null);
          }
        }

        try {
          const detail = await getProjectDetail(dirPath);
          setHasExistingConfig(detail.has_config);
          setHasWorktreeConfig(detail.worktree_enabled);
          if (detail.has_config && detail.worktree_enabled) {
            setBranchingMode("worktree");
          }
        } catch {
          setHasExistingConfig(false);
          setHasWorktreeConfig(false);
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
        await addOrInitProject(selectedPath, normalized, vcsPref, branchingMode === "worktree", selectedBranch || undefined);
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
    const showWorktreeSelector = !hasExistingConfig || (hasExistingConfig && !hasWorktreeConfig);

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

        {vcsInfo?.existing_vcs && branchInfo && (
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
              Main Branch
            </label>
            {branchInfo.branches.length > 0 ? (
              <>
                {branchInfo.branches.length > 5 && (
                  <input
                    type="text"
                    value={branchSearch}
                    onChange={(e) => setBranchSearch(e.target.value)}
                    placeholder="Search branches..."
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
                  {branchInfo.branches
                    .filter((b) =>
                      !branchSearch || b.toLowerCase().includes(branchSearch.toLowerCase())
                    )
                    .map((branch) => {
                      const isDefault = branch === branchInfo.default_branch;
                      const isSelected = branch === selectedBranch;
                      return (
                        <div
                          key={branch}
                          onClick={() => setSelectedBranch(branch)}
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
                            {branch}
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
                value={selectedBranch}
                onChange={(e) => setSelectedBranch(e.target.value)}
                placeholder="e.g. main"
                style={{ width: "100%", fontSize: 13 }}
              />
            )}
          </div>
        )}

        {showWorktreeSelector && (
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
              Workspace Mode
            </label>
            <div className="flex gap-2">
              <button
                className={`btn${branchingMode === "worktree" ? " btn-primary" : ""}`}
                style={{ padding: "4px 16px", fontSize: 13 }}
                onClick={() => setBranchingMode("worktree")}
                type="button"
              >
                worktree
              </button>
              <button
                className={`btn${branchingMode === "workspace" ? " btn-primary" : ""}`}
                style={{ padding: "4px 16px", fontSize: 13 }}
                onClick={() => setBranchingMode("workspace")}
                type="button"
              >
                checkout
              </button>
            </div>
            <div
              style={{
                marginTop: 6,
                fontSize: 12,
                color: "var(--text-muted)",
              }}
            >
              {branchingMode === "worktree"
                ? "Each workspace gets its own directory — parallel work without stashing."
                : `Branches share a single directory — switches in place like ${
                    (vcsInfo?.existing_vcs || selectedVcs) === "jj" ? "jj edit" : "git checkout"
                  }.`}
            </div>
          </div>
        )}

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
