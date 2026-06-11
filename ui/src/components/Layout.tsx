import { useState, useEffect, useRef, useCallback } from "react";
import { Outlet, NavLink, useNavigate } from "react-router-dom";
import { listen } from "@tauri-apps/api/event";
import { getProxyStatus, listProjects, getSettings } from "../utils/invoke";
import type { ProjectEntry, ProxyStatus, AppSettings } from "../types";
import TerminalPanel from "./TerminalPanel";
import { useTerminal } from "../context/TerminalContext";
import { sortByRecent } from "../utils/recentProjects";
import {
  IconDashboard,
  IconProjects,
  IconProxy,
  IconTerminal,
  IconSkills,
  IconMerge,
  IconSettings,
  IconFolder,
} from "./icons";

const MAX_SIDEBAR_PROJECTS = 8;

function Layout() {
  const [proxyStatus, setProxyStatus] = useState<ProxyStatus | null>(null);
  const [projects, setProjects] = useState<ProjectEntry[]>([]);
  const [sidebarOrder, setSidebarOrder] = useState<ProjectEntry[]>([]);
  const [smartMerge, setSmartMerge] = useState(false);
  const navigate = useNavigate();
  const navigateRef = useRef(navigate);
  navigateRef.current = navigate;
  const { isVisible, toggle, pendingTerminal, clearPending } = useTerminal();

  // Merge new project list into sidebar: keep existing order, append new ones, remove deleted
  const updateProjectList = useCallback((list: ProjectEntry[]) => {
    setProjects(list);
    setSidebarOrder((prev) => {
      if (prev.length === 0) {
        // First load — sort by recency
        return sortByRecent(list, (p) => p.path);
      }
      const newPaths = new Set(list.map((p) => p.path));
      const listByPath = new Map(list.map((p) => [p.path, p]));
      // Keep existing items in their current order, update data
      const kept = prev
        .filter((p) => newPaths.has(p.path))
        .map((p) => listByPath.get(p.path)!);
      const existingPaths = new Set(prev.map((p) => p.path));
      // Append genuinely new projects at the top
      const added = list.filter((p) => !existingPaths.has(p.path));
      return [...added, ...kept];
    });
  }, []);

  useEffect(() => {
    getProxyStatus()
      .then(setProxyStatus)
      .catch(() => setProxyStatus(null));

    listProjects()
      .then(updateProjectList)
      .catch(() => {});

    getSettings()
      .then((s) => setSmartMerge(s.smart_merge))
      .catch(() => {});

    // Listen for proxy status changes from backend
    const unlistenProxy = listen<ProxyStatus>("proxy-status-changed", (event) => {
      setProxyStatus(event.payload);
    });

    // Listen for tray navigation events
    const unlistenNav = listen<string>("navigate", (event) => {
      navigateRef.current(event.payload);
    });

    // Listen for settings changes (e.g. smart_merge toggle)
    const handleSettingsUpdate = (e: Event) => {
      const detail = (e as CustomEvent<AppSettings>).detail;
      if (detail) setSmartMerge(detail.smart_merge);
    };
    window.addEventListener("devflow:settings-updated", handleSettingsUpdate);

    // Listen for project list changes (add/remove)
    const handleProjectsChanged = () => {
      listProjects().then(updateProjectList).catch(() => {});
    };
    window.addEventListener("devflow:projects-changed", handleProjectsChanged);

    return () => {
      unlistenProxy.then((fn) => fn());
      unlistenNav.then((fn) => fn());
      window.removeEventListener("devflow:settings-updated", handleSettingsUpdate);
      window.removeEventListener("devflow:projects-changed", handleProjectsChanged);
    };
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Keyboard shortcut: Ctrl+` to toggle terminal
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.ctrlKey && e.key === "`") {
        e.preventDefault();
        toggle();
      }
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [toggle]);

  const navClass = ({ isActive }: { isActive: boolean }) =>
    `nav-item${isActive ? " active" : ""}`;

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="sidebar-header titlebar-drag">
          <span className="sidebar-brand-dot" />
          devflow
        </div>

        <nav className="sidebar-nav">
          <NavLink to="/" end className={navClass}>
            <IconDashboard size={16} />
            Dashboard
          </NavLink>

          <div className="nav-section">Projects</div>
          {sidebarOrder.slice(0, MAX_SIDEBAR_PROJECTS).map((p) => (
            <NavLink
              key={p.path}
              to={`/projects/${encodeURIComponent(p.path)}`}
              className={navClass}
            >
              <IconFolder size={16} />
              <span className="nav-item-label">{p.name}</span>
            </NavLink>
          ))}
          <NavLink to="/projects" end className={navClass}>
            <IconProjects size={16} />
            {projects.length > 0 ? `All projects (${projects.length})` : "Add project"}
          </NavLink>

          {smartMerge && (
            <>
              <div className="nav-section">Merge</div>
              <NavLink to="/merge-train" className={navClass}>
                <IconMerge size={16} />
                Merge Train
              </NavLink>
            </>
          )}

          <div className="nav-section">Infrastructure</div>
          <NavLink to="/proxy" className={navClass}>
            <IconProxy size={16} />
            Proxy
          </NavLink>
          <a
            className={`nav-item${isVisible ? " active" : ""}`}
            onClick={(e) => {
              e.preventDefault();
              toggle();
            }}
            style={{ cursor: "pointer" }}
          >
            <IconTerminal size={16} />
            Terminal
          </a>

          <div className="nav-section">Tools</div>
          <NavLink to="/skills" className={navClass}>
            <IconSkills size={16} />
            Skills
          </NavLink>
          <NavLink to="/settings" className={navClass}>
            <IconSettings size={16} />
            Settings
          </NavLink>
        </nav>

        <NavLink
          to="/proxy"
          className="proxy-indicator"
          style={{ textDecoration: "none" }}
          title={proxyStatus?.running ? "Proxy is running" : "Proxy is stopped"}
        >
          <span
            className={`proxy-dot ${proxyStatus?.running ? "running" : "stopped"}`}
          />
          Proxy {proxyStatus?.running ? "running" : "stopped"}
        </NavLink>
      </aside>

      <div className="main-area">
        <main className="content">
          <Outlet />
        </main>
        <TerminalPanel
          isVisible={isVisible}
          onToggle={toggle}
          pendingTerminal={pendingTerminal}
          onPendingTerminalHandled={clearPending}
        />
      </div>
    </div>
  );
}

export default Layout;
