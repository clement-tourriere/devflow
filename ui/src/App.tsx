import { useEffect } from "react";
import { Routes, Route, Navigate } from "react-router-dom";
import Layout from "./components/Layout";
import Home from "./pages/Home";
import ProjectList from "./pages/projects/ProjectList";
import ProjectDetail from "./pages/projects/ProjectDetail";
import ProxyDashboard from "./pages/proxy/ProxyDashboard";
import HookManager from "./pages/hooks/HookManager";
import ConfigEditor from "./pages/config/ConfigEditor";
import DoctorPage from "./pages/doctor/DoctorPage";
import Settings from "./pages/settings/Settings";

const TEXT_INPUT_TYPES = new Set([
  "",
  "text",
  "search",
  "email",
  "url",
  "tel",
  "password",
  "number",
]);

function disableSmartTextFeatures(el: HTMLInputElement | HTMLTextAreaElement) {
  if (el instanceof HTMLInputElement) {
    const inputType = (el.type || "").toLowerCase();
    if (!TEXT_INPUT_TYPES.has(inputType)) {
      return;
    }
  }

  el.setAttribute("autocapitalize", "off");
  el.setAttribute("autocorrect", "off");
  el.setAttribute("spellcheck", "false");
  if (!el.hasAttribute("autocomplete")) {
    el.setAttribute("autocomplete", "off");
  }
}

function applySmartTextPrevention(root: ParentNode = document) {
  root
    .querySelectorAll<HTMLInputElement | HTMLTextAreaElement>("input, textarea")
    .forEach(disableSmartTextFeatures);
}

function App() {
  useEffect(() => {
    document.documentElement.setAttribute("autocapitalize", "off");
    document.documentElement.setAttribute("spellcheck", "false");
    document.body.setAttribute("autocapitalize", "off");
    document.body.setAttribute("autocorrect", "off");
    document.body.setAttribute("spellcheck", "false");

    applySmartTextPrevention(document);

    const observer = new MutationObserver((records) => {
      for (const record of records) {
        for (const node of record.addedNodes) {
          if (!(node instanceof HTMLElement)) {
            continue;
          }

          if (node instanceof HTMLInputElement || node instanceof HTMLTextAreaElement) {
            disableSmartTextFeatures(node);
          }

          applySmartTextPrevention(node);
        }
      }
    });

    observer.observe(document.body, {
      childList: true,
      subtree: true,
    });

    const handleFocusIn = (event: FocusEvent) => {
      const target = event.target;
      if (target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement) {
        disableSmartTextFeatures(target);
      }
    };

    document.addEventListener("focusin", handleFocusIn, true);

    return () => {
      observer.disconnect();
      document.removeEventListener("focusin", handleFocusIn, true);
    };
  }, []);

  return (
    <Routes>
      <Route path="/" element={<Layout />}>
        <Route index element={<Home />} />
        <Route path="projects" element={<ProjectList />} />
        <Route path="projects/*" element={<ProjectDetail />} />
        <Route path="proxy" element={<ProxyDashboard />} />
        <Route path="hooks/*" element={<HookManager />} />
        <Route path="config/*" element={<ConfigEditor />} />
        <Route path="setup/*" element={<DoctorPage />} />
        <Route path="settings" element={<Settings />} />
        <Route path="*" element={<Navigate to="/" replace />} />
      </Route>
    </Routes>
  );
}

export default App;
