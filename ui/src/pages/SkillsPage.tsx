import { useState, useEffect, useCallback, useRef } from "react";
import Markdown from "react-markdown";
import ConfirmDialog from "../components/ConfirmDialog";
import {
  userSkillList,
  userSkillInstall,
  userSkillRemove,
  userSkillUpdate,
  userSkillShow,
  userSkillCheckUpdates,
  skillSearch,
  skillSearchDetail,
} from "../utils/invoke";
import type {
  UserSkillInfo,
  SkillSearchResult,
  SkillSearchDetail,
  SkillDetail,
} from "../types";

type Mode = "installed" | "search" | "browse";

/** Strip YAML frontmatter (---...---) from SKILL.md content before rendering. */
function stripFrontmatter(md: string): string {
  return md.replace(/^---\n[\s\S]*?\n---\n?/, "");
}

export default function SkillsPage() {
  // Data
  const [installed, setInstalled] = useState<UserSkillInfo[]>([]);
  const [searchResults, setSearchResults] = useState<SkillSearchResult[]>([]);
  const [popularSkills, setPopularSkills] = useState<SkillSearchResult[]>([]);
  const [selectedSkill, setSelectedSkill] = useState<string | null>(null);
  const [skillDetail, setSkillDetail] = useState<SkillDetail | null>(null);
  const [searchDetail, setSearchDetail] = useState<SkillSearchDetail | null>(null);
  const [searchDetailLoading, setSearchDetailLoading] = useState(false);
  const [updatesAvailable, setUpdatesAvailable] = useState<string[]>([]);

  // UI
  const [mode, setMode] = useState<Mode>("installed");
  const [searchQuery, setSearchQuery] = useState("");
  const [loading, setLoading] = useState(true);
  const [popularLoading, setPopularLoading] = useState(false);
  const [searching, setSearching] = useState(false);
  const [actionLoading, setActionLoading] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  // Confirm dialog
  const [confirmRemove, setConfirmRemove] = useState<string | null>(null);
  const [removeLoading, setRemoveLoading] = useState(false);

  const searchTimeout = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Load popular skills on mount
  useEffect(() => {
    setPopularLoading(true);
    skillSearch("skill", 20)
      .then(setPopularSkills)
      .catch(() => setPopularSkills([]))
      .finally(() => setPopularLoading(false));
  }, []);

  const reload = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [skills, updates] = await Promise.all([
        userSkillList(),
        userSkillCheckUpdates().catch(() => [] as string[]),
      ]);
      setInstalled(skills);
      setUpdatesAvailable(updates);
      if (skills.length > 0 && !selectedSkill) {
        setSelectedSkill(skills[0].name);
      } else if (skills.length === 0 && mode === "installed") {
        setMode("browse");
      }
      // If we only have external skills (no managed), stay in installed mode
      if (skills.length > 0 && mode === "browse") {
        setMode("installed");
      }
    } catch (e) {
      setError(`Failed to load user skills: ${e}`);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    reload();
  }, [reload]);

  // Load skill detail when selection changes (installed mode)
  useEffect(() => {
    if (!selectedSkill || mode !== "installed") {
      setSkillDetail(null);
      return;
    }
    userSkillShow(selectedSkill)
      .then(setSkillDetail)
      .catch(() => setSkillDetail(null));
  }, [selectedSkill, mode]);

  // Load search detail when a search/browse result is selected
  useEffect(() => {
    if (!selectedSkill || (mode !== "search" && mode !== "browse")) {
      setSearchDetail(null);
      return;
    }
    const results = mode === "search" ? searchResults : popularSkills;
    const result = results.find((r) => r.id === selectedSkill);
    if (!result) {
      setSearchDetail(null);
      return;
    }
    setSearchDetailLoading(true);
    setSearchDetail(null);
    skillSearchDetail(result.source, result.name)
      .then(setSearchDetail)
      .catch(() => setSearchDetail(null))
      .finally(() => setSearchDetailLoading(false));
  }, [selectedSkill, mode, searchResults, popularSkills]);

  // Debounced search
  const handleSearchChange = (value: string) => {
    setSearchQuery(value);
    if (searchTimeout.current) clearTimeout(searchTimeout.current);
    if (!value.trim()) {
      setMode(installed.length > 0 ? "installed" : "browse");
      setSearchResults([]);
      return;
    }
    searchTimeout.current = setTimeout(async () => {
      setMode("search");
      setSearching(true);
      try {
        const results = await skillSearch(value.trim(), 20);
        setSearchResults(results);
      } catch (e) {
        setError(`Search failed: ${e}`);
      } finally {
        setSearching(false);
      }
    }, 300);
  };

  const handleInstall = async (identifier: string) => {
    setActionLoading(`install:${identifier}`);
    try {
      await userSkillInstall(identifier);
      await reload();
      setMode("installed");
      setSearchQuery("");
      setSearchResults([]);
    } catch (e) {
      alert(`Install failed: ${e}`);
    } finally {
      setActionLoading(null);
    }
  };

  const handleRemove = (name: string) => {
    setConfirmRemove(name);
  };

  const handleConfirmRemove = async () => {
    if (!confirmRemove) return;
    setRemoveLoading(true);
    try {
      await userSkillRemove(confirmRemove);
      if (selectedSkill === confirmRemove) setSelectedSkill(null);
      setConfirmRemove(null);
      await reload();
    } catch (e) {
      alert(`Remove failed: ${e}`);
    } finally {
      setRemoveLoading(false);
    }
  };

  const handleUpdate = async (name?: string) => {
    setActionLoading(name ? `update:${name}` : "update:all");
    try {
      await userSkillUpdate(name);
      await reload();
    } catch (e) {
      alert(`Update failed: ${e}`);
    } finally {
      setActionLoading(null);
    }
  };

  const handleBackToInstalled = () => {
    setMode(installed.length > 0 ? "installed" : "browse");
    setSearchQuery("");
    setSearchResults([]);
  };

  const formatSource = (skill: UserSkillInfo) => {
    if (!skill.managed && skill.external_agent) {
      return `external · ${skill.external_agent}`;
    }
    if (skill.source.type === "bundled") return "bundled";
    return `${skill.source.owner}/${skill.source.repo}`;
  };

  const formatInstalls = (n: number) => {
    if (n >= 1000) return `${(n / 1000).toFixed(1)}K`;
    return n.toString();
  };

  // Renders a list of search/browse results
  const renderResultList = (results: SkillSearchResult[]) =>
    results.map((r) => (
      <div
        key={r.id}
        onClick={() => setSelectedSkill(r.id)}
        style={{
          padding: "10px 12px",
          cursor: "pointer",
          borderBottom: "1px solid var(--border)",
          background:
            selectedSkill === r.id
              ? "var(--bg-tertiary)"
              : "transparent",
        }}
      >
        <div
          style={{
            fontWeight: 600,
            fontSize: 13,
            color: "var(--text-primary)",
          }}
        >
          {r.name}
        </div>
        <div
          style={{
            fontSize: 11,
            color: "var(--text-secondary)",
            marginTop: 2,
          }}
        >
          {r.source} · {formatInstalls(r.installs)} installs
        </div>
      </div>
    ));

  // Renders the detail panel for a search/browse result
  const renderSearchResultDetail = (result: SkillSearchResult) => (
    <div>
      <h2 style={{ margin: "0 0 8px 0", fontSize: 18 }}>{result.name}</h2>
      <div style={{ fontSize: 13, color: "var(--text-secondary)", marginBottom: 12 }}>
        Source: {result.source} · {formatInstalls(result.installs)} installs
      </div>
      {searchDetail?.description && (
        <p style={{ fontSize: 13, color: "var(--text-primary)", margin: "0 0 12px 0" }}>
          {searchDetail.description}
        </p>
      )}
      <button
        className="btn btn-primary"
        onClick={() => handleInstall(result.source + "/" + result.name)}
        disabled={actionLoading === `install:${result.source}/${result.name}`}
        style={{ marginBottom: 16 }}
      >
        {actionLoading === `install:${result.source}/${result.name}`
          ? "Installing..."
          : "Install (User)"}
      </button>
      {searchDetailLoading ? (
        <div style={{ color: "var(--text-muted)", fontSize: 13, marginTop: 16 }}>
          Loading preview...
        </div>
      ) : searchDetail?.content ? (
        <div
          style={{
            borderTop: "1px solid var(--border)",
            paddingTop: 16,
            fontSize: 14,
            lineHeight: 1.6,
          }}
          className="skill-content"
        >
          <Markdown>{stripFrontmatter(searchDetail.content)}</Markdown>
        </div>
      ) : null}
    </div>
  );

  if (error && installed.length === 0) {
    return (
      <div>
        <h1 style={{ fontSize: 24, marginBottom: 16 }}>Skills</h1>
        <p style={{ color: "var(--text-secondary)", marginBottom: 24, fontSize: 14 }}>
          User-scope skills are available across all projects and automatically symlinked into
          supported agents (OpenCode, Codex CLI).
        </p>
        <div className="card" style={{ marginTop: 24 }}>
          <p style={{ color: "var(--danger)" }}>{error}</p>
        </div>
      </div>
    );
  }

  return (
    <div>
      <h1 style={{ fontSize: 24, marginBottom: 4 }}>Skills</h1>
      <p style={{ color: "var(--text-secondary)", marginBottom: 16, fontSize: 14 }}>
        User-scope skills are available across all projects and automatically symlinked into
        supported agents.
        {installed.length > 0 && installed[0].agents.length > 0 && (
          <span style={{ marginLeft: 4 }}>
            Linked to:{" "}
            {installed[0].agents.map((agent, i) => (
              <span key={agent}>
                {i > 0 && ", "}
                <span className="badge badge-info" style={{ fontSize: 10 }}>{agent}</span>
              </span>
            ))}
          </span>
        )}
      </p>

      {/* Search bar */}
      <div className="card" style={{ padding: "12px 16px", marginBottom: 16 }}>
        <input
          className="search-input"
          type="text"
          placeholder="Search skills.sh marketplace..."
          value={searchQuery}
          onChange={(e) => handleSearchChange(e.target.value)}
          style={{ width: "100%", fontSize: 14 }}
        />
      </div>

      {/* Update notification banner */}
      {updatesAvailable.length > 0 && mode === "installed" && (
        <div
          className="card"
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            padding: "10px 16px",
            marginBottom: 16,
            borderLeft: "3px solid var(--warning)",
          }}
        >
          <div style={{ flex: 1, fontSize: 13 }}>
            <strong>{updatesAvailable.length} update(s) available</strong>
            <span style={{ color: "var(--text-secondary)", marginLeft: 8 }}>
              {updatesAvailable.join(", ")}
            </span>
          </div>
          <button
            className="btn btn-primary"
            onClick={() => handleUpdate()}
            disabled={actionLoading === "update:all"}
            style={{ fontSize: 12, padding: "4px 12px" }}
          >
            {actionLoading === "update:all" ? "Updating..." : "Update All"}
          </button>
        </div>
      )}

      {/* Two-panel layout */}
      <div style={{ display: "flex", gap: 16, minHeight: 500 }}>
        {/* Left panel — skill list */}
        <div className="card" style={{ width: "30%", padding: 0, overflow: "auto" }}>
          {mode === "search" ? (
            <>
              <div
                style={{
                  padding: "8px 12px",
                  borderBottom: "1px solid var(--border)",
                  fontSize: 12,
                  color: "var(--text-secondary)",
                  display: "flex",
                  justifyContent: "space-between",
                  alignItems: "center",
                }}
              >
                <span>
                  {searching
                    ? "Searching..."
                    : `${searchResults.length} result(s)`}
                </span>
                <button
                  className="btn-link"
                  onClick={handleBackToInstalled}
                  style={{ fontSize: 12 }}
                >
                  Back
                </button>
              </div>
              {renderResultList(searchResults)}
            </>
          ) : mode === "browse" ? (
            <>
              <div
                style={{
                  padding: "8px 12px",
                  borderBottom: "1px solid var(--border)",
                  fontSize: 12,
                  color: "var(--text-secondary)",
                  display: "flex",
                  justifyContent: "space-between",
                  alignItems: "center",
                }}
              >
                <span>
                  {popularLoading ? "Loading..." : "Popular Skills"}
                </span>
                {installed.length > 0 && (
                  <button
                    className="btn-link"
                    onClick={() => setMode("installed")}
                    style={{ fontSize: 12 }}
                  >
                    Installed
                  </button>
                )}
              </div>
              {renderResultList(popularSkills)}
            </>
          ) : (
            <>
              <div
                style={{
                  padding: "8px 12px",
                  borderBottom: "1px solid var(--border)",
                  fontSize: 12,
                  color: "var(--text-secondary)",
                  display: "flex",
                  justifyContent: "space-between",
                  alignItems: "center",
                }}
              >
                <span>
                  {loading
                    ? "Loading..."
                    : (() => {
                        const managed = installed.filter((s) => s.managed).length;
                        const external = installed.filter((s) => !s.managed).length;
                        const parts = [];
                        if (managed > 0) parts.push(`${managed} managed`);
                        if (external > 0) parts.push(`${external} external`);
                        return parts.length > 0 ? parts.join(", ") : "0 installed";
                      })()}
                </span>
                <button
                  className="btn-link"
                  onClick={() => setMode("browse")}
                  style={{ fontSize: 12 }}
                >
                  Browse
                </button>
              </div>
              {installed.length === 0 && !loading && (
                <div
                  style={{
                    padding: "20px 12px",
                    color: "var(--text-muted)",
                    fontSize: 13,
                    textAlign: "center",
                  }}
                >
                  No user-scope skills installed.
                  <br />
                  Search or browse to find skills.
                </div>
              )}
              {installed.map((s) => (
                <div
                  key={s.name}
                  onClick={() => setSelectedSkill(s.name)}
                  style={{
                    padding: "10px 12px",
                    cursor: "pointer",
                    borderBottom: "1px solid var(--border)",
                    background:
                      selectedSkill === s.name
                        ? "var(--bg-tertiary)"
                        : "transparent",
                  }}
                >
                  <div
                    style={{
                      display: "flex",
                      alignItems: "center",
                      gap: 6,
                    }}
                  >
                    <span
                      style={{
                        fontWeight: 600,
                        fontSize: 13,
                        color: "var(--text-primary)",
                      }}
                    >
                      {s.name}
                    </span>
                    {!s.managed && (
                      <span className="badge badge-muted" style={{ fontSize: 10 }}>
                        external
                      </span>
                    )}
                    {s.managed && updatesAvailable.includes(s.name) && (
                      <span className="badge badge-warning" style={{ fontSize: 10 }}>
                        update
                      </span>
                    )}
                  </div>
                  <div
                    style={{
                      fontSize: 11,
                      color: "var(--text-secondary)",
                      marginTop: 2,
                    }}
                  >
                    <span className={`badge badge-${!s.managed ? "muted" : s.source.type === "bundled" ? "muted" : "info"}`} style={{ fontSize: 10 }}>
                      {formatSource(s)}
                    </span>
                    {s.managed && (
                      <span style={{ marginLeft: 6 }} className="mono">
                        {s.content_hash.slice(0, 12)}
                      </span>
                    )}
                  </div>
                  {!s.managed && s.description && (
                    <div
                      style={{
                        fontSize: 11,
                        color: "var(--text-muted)",
                        marginTop: 3,
                        overflow: "hidden",
                        textOverflow: "ellipsis",
                        whiteSpace: "nowrap",
                      }}
                    >
                      {s.description}
                    </div>
                  )}
                </div>
              ))}
            </>
          )}
        </div>

        {/* Right panel — detail / search result */}
        <div className="card" style={{ flex: 1, padding: 16, overflow: "auto" }}>
          {(mode === "search" || mode === "browse") && selectedSkill ? (
            (() => {
              const results = mode === "search" ? searchResults : popularSkills;
              const result = results.find((r) => r.id === selectedSkill);
              if (!result) return <div style={{ color: "var(--text-muted)" }}>Select a skill</div>;
              return renderSearchResultDetail(result);
            })()
          ) : mode === "installed" && skillDetail ? (
            (() => {
              const selectedInfo = installed.find((s) => s.name === skillDetail.name);
              const isExternal = selectedInfo ? !selectedInfo.managed : false;
              return (
                <div>
                  <div className="flex items-center justify-between" style={{ marginBottom: 12 }}>
                    <h2 style={{ margin: 0, fontSize: 18 }}>{skillDetail.name}</h2>
                    {!isExternal && (
                      <div style={{ display: "flex", gap: 8 }}>
                        {updatesAvailable.includes(skillDetail.name) && (
                          <button
                            className="btn btn-primary"
                            onClick={() => handleUpdate(skillDetail.name)}
                            disabled={actionLoading === `update:${skillDetail.name}`}
                            style={{ fontSize: 12, padding: "4px 12px" }}
                          >
                            {actionLoading === `update:${skillDetail.name}` ? "Updating..." : "Update"}
                          </button>
                        )}
                        <button
                          className="btn btn-danger"
                          onClick={() => handleRemove(skillDetail.name)}
                          disabled={!!confirmRemove}
                          style={{ fontSize: 12, padding: "4px 12px" }}
                        >
                          Remove
                        </button>
                      </div>
                    )}
                  </div>
                  <div style={{ fontSize: 13, color: "var(--text-secondary)", marginBottom: 16 }}>
                    {isExternal ? (
                      <>
                        <span className="badge badge-muted">
                          external · {selectedInfo?.external_agent}
                        </span>
                        {selectedInfo?.description && (
                          <span style={{ marginLeft: 8 }}>{selectedInfo.description}</span>
                        )}
                        <span className="badge badge-muted" style={{ marginLeft: 8, fontSize: 10 }}>read-only</span>
                      </>
                    ) : (
                      <>
                        <span className={`badge badge-${skillDetail.source.type === "bundled" ? "muted" : "info"}`}>
                          {skillDetail.source.type === "bundled" ? "bundled" : `${skillDetail.source.owner}/${skillDetail.source.repo}`}
                        </span>
                        <span style={{ marginLeft: 8 }} className="mono">{skillDetail.content_hash.slice(0, 12)}</span>
                        <span style={{ marginLeft: 8 }}>Installed {skillDetail.installed_at}</span>
                        <span className="badge badge-muted" style={{ marginLeft: 8, fontSize: 10 }}>user scope</span>
                      </>
                    )}
                  </div>
                  <div
                    style={{
                      borderTop: "1px solid var(--border)",
                      paddingTop: 16,
                      fontSize: 14,
                      lineHeight: 1.6,
                    }}
                    className="skill-content"
                  >
                    <Markdown>{stripFrontmatter(skillDetail.content)}</Markdown>
                  </div>
                </div>
              );
            })()
          ) : (
            <div
              style={{
                color: "var(--text-muted)",
                display: "flex",
                alignItems: "center",
                justifyContent: "center",
                height: "100%",
                fontSize: 14,
              }}
            >
              {mode === "installed" && installed.length === 0
                ? "Search or browse to find and install user-scope skills"
                : "Select a skill to view details"}
            </div>
          )}
        </div>
      </div>

      {/* Confirm remove dialog */}
      <ConfirmDialog
        open={confirmRemove !== null}
        onClose={() => setConfirmRemove(null)}
        onConfirm={handleConfirmRemove}
        title="Remove User Skill"
        message={`Are you sure you want to remove the user-scope skill "${confirmRemove}"? This will remove the skill from all agents and projects that inherited it.`}
        confirmLabel="Remove"
        danger
        loading={removeLoading}
      />
    </div>
  );
}
