import { useState, useEffect } from "react";
import { Outlet, NavLink, useNavigate } from "react-router-dom";
import { listen } from "@tauri-apps/api/event";
import { getProxyStatus, listProjects } from "../utils/invoke";
import type { ProjectEntry, ProxyStatus } from "../types";
import TerminalPanel from "./TerminalPanel";
import { useTerminal } from "../context/TerminalContext";

function Layout() {
  const [proxyStatus, setProxyStatus] = useState<ProxyStatus | null>(null);
  const [projects, setProjects] = useState<ProjectEntry[]>([]);
  const navigate = useNavigate();
  const { isVisible, toggle, pendingTerminal, clearPending } = useTerminal();

  useEffect(() => {
    getProxyStatus()
      .then(setProxyStatus)
      .catch(() => setProxyStatus(null));

    listProjects()
      .then(setProjects)
      .catch(() => {});

    // Listen for proxy status changes from backend
    const unlistenProxy = listen<ProxyStatus>("proxy-status-changed", (event) => {
      setProxyStatus(event.payload);
    });

    // Listen for tray navigation events
    const unlistenNav = listen<string>("navigate", (event) => {
      navigate(event.payload);
    });

    return () => {
      unlistenProxy.then((fn) => fn());
      unlistenNav.then((fn) => fn());
    };
  }, [navigate]);

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

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="sidebar-header">devflow</div>

        <nav className="sidebar-nav">
          <div className="nav-section">Overview</div>
          <NavLink
            to="/"
            end
            className={({ isActive }) =>
              `nav-item${isActive ? " active" : ""}`
            }
          >
            Dashboard
          </NavLink>

          <div className="nav-section">Projects</div>
          <NavLink
            to="/projects"
            end
            className={({ isActive }) =>
              `nav-item${isActive ? " active" : ""}`
            }
          >
            All Projects
          </NavLink>
          {projects.map((p) => (
            <NavLink
              key={p.path}
              to={`/projects/${encodeURIComponent(p.path)}`}
              className={({ isActive }) =>
                `nav-item${isActive ? " active" : ""}`
              }
            >
              {p.name}
            </NavLink>
          ))}

          <div className="nav-section">Infrastructure</div>
          <NavLink
            to="/proxy"
            className={({ isActive }) =>
              `nav-item${isActive ? " active" : ""}`
            }
          >
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
            Terminal
          </a>

          <div className="nav-section">App</div>
          <NavLink
            to="/settings"
            className={({ isActive }) =>
              `nav-item${isActive ? " active" : ""}`
            }
          >
            Settings
          </NavLink>
        </nav>

        <div className="proxy-indicator">
          <span
            className={`proxy-dot ${proxyStatus?.running ? "running" : "stopped"}`}
          />
          Proxy: {proxyStatus?.running ? "Running" : "Stopped"}
        </div>
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
