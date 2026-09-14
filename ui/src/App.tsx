import { useEffect, lazy, Suspense } from "react";
import { Routes, Route, Navigate } from "react-router-dom";
import Layout from "./components/Layout";
const Home = lazy(() => import("./pages/Home"));
const ProjectList = lazy(() => import("./pages/projects/ProjectList"));
const ProjectDetail = lazy(() => import("./pages/projects/ProjectDetail"));
const ProjectSetup = lazy(() => import("./pages/projects/ProjectSetup"));
const ProxyDashboard = lazy(() => import("./pages/proxy/ProxyDashboard"));
const HookManager = lazy(() => import("./pages/hooks/HookManager"));
const ConfigEditor = lazy(() => import("./pages/config/ConfigEditor"));
const DoctorPage = lazy(() => import("./pages/doctor/DoctorPage"));
const Settings = lazy(() => import("./pages/settings/Settings"));

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
    <Suspense fallback={<p role="status">Loading…</p>}>
    <Routes>
      <Route path="/" element={<Layout />}>
        <Route index element={<Home />} />
        <Route path="projects" element={<ProjectList />} />
        {/* Onboarding wizard on its own prefix: a splat must be the terminal
            segment in React Router v6, so the old nested-splat setup route
            never matched and the wizard was unreachable. */}
        <Route path="onboard/*" element={<ProjectSetup />} />
        <Route path="projects/*" element={<ProjectDetail />} />
        <Route path="proxy" element={<ProxyDashboard />} />
        <Route path="hooks/*" element={<HookManager />} />
        <Route path="config/*" element={<ConfigEditor />} />
        <Route path="setup/*" element={<DoctorPage />} />
        <Route path="settings" element={<Settings />} />
        <Route path="*" element={<Navigate to="/" replace />} />
      </Route>
    </Routes>
    </Suspense>
  );
}

export default App;
