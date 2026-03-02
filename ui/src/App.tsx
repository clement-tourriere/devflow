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

function App() {
  return (
    <Routes>
      <Route path="/" element={<Layout />}>
        <Route index element={<Home />} />
        <Route path="projects" element={<ProjectList />} />
        <Route path="projects/*" element={<ProjectDetail />} />
        <Route path="proxy" element={<ProxyDashboard />} />
        <Route path="hooks/*" element={<HookManager />} />
        <Route path="config/*" element={<ConfigEditor />} />
        <Route path="doctor/*" element={<DoctorPage />} />
        <Route path="settings" element={<Settings />} />
        <Route path="*" element={<Navigate to="/" replace />} />
      </Route>
    </Routes>
  );
}

export default App;
