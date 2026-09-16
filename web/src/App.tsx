import { BrowserRouter, Routes, Route, Navigate } from "react-router-dom";
import { useEffect, useState } from "react";
import Layout from "./Layout";
import LogsPage from "./pages/LogsPage";
import DashboardsPage from "./pages/DashboardsPage";
import DashboardBuilderPage from "./pages/DashboardBuilderPage";
import DashboardViewerPage from "./pages/DashboardViewerPage";
import AiBuilderPage from "./pages/AiBuilderPage";
import PresetVolumePage from "./pages/PresetVolumePage";
import PresetErrorRatePage from "./pages/PresetErrorRatePage";
import PresetLatencyPage from "./pages/PresetLatencyPage";
import PresetAnomaliesPage from "./pages/PresetAnomaliesPage";
import AlertsPage from "./pages/AlertsPage";
import ApiKeysPage from "./pages/ApiKeysPage";
import PipelinePage from "./pages/PipelinePage";
import StoragePage from "./pages/StoragePage";
import ErrorsPage, { ErrorGroupDetailPage } from "./pages/ErrorsPage";
import { api, WhoamiResponse } from "./api";

/**
 * Top-level router. Before rendering any page we call /api/auth/whoami:
 *   - 200 → render the app (the Layout shows the signed-in user + logout).
 *   - 401 → hard-redirect to the server-rendered /login page. The api.ts
 *           helper also auto-redirects on any 401 from a data call, so a
 *           session that expires mid-page bounces the user back too.
 */
export default function App() {
  const [whoami, setWhoami] = useState<WhoamiResponse | null>(null);
  const [bootError, setBootError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    api
      .whoami()
      .then((w) => {
        if (!cancelled) setWhoami(w);
      })
      .catch((e) => {
        // 401 triggers the /login redirect inside api.ts; other errors surface here.
        if (!cancelled) setBootError(String(e));
      });
    return () => {
      cancelled = true;
    };
  }, []);

  if (bootError) {
    return (
      <div className="min-h-screen flex items-center justify-center text-tremor-content-subtle">
        failed to load session: {bootError}
      </div>
    );
  }
  if (!whoami) {
    return (
      <div className="min-h-screen flex items-center justify-center text-tremor-content-subtle">
        loading…
      </div>
    );
  }

  const isAdmin = whoami.scopes.includes("admin");

  return (
    <BrowserRouter>
      <Routes>
        <Route element={<Layout whoami={whoami} />}>
          <Route index element={<LogsPage />} />
          <Route path="dashboards">
            <Route index element={<DashboardsPage />} />
            <Route path="new" element={<DashboardBuilderPage />} />
            <Route path="ai" element={<AiBuilderPage />} />
            <Route path="edit/:id" element={<DashboardBuilderPage />} />
            <Route path="view/:id" element={<DashboardViewerPage />} />
            <Route path="preset/volume" element={<PresetVolumePage />} />
            <Route path="preset/error-rate" element={<PresetErrorRatePage />} />
            <Route path="preset/latency" element={<PresetLatencyPage />} />
            <Route path="preset/anomalies" element={<PresetAnomaliesPage />} />
          </Route>
          <Route path="alerts" element={<AlertsPage />} />
          <Route path="errors" element={<ErrorsPage />} />
          <Route path="errors/:fingerprint" element={<ErrorGroupDetailPage />} />
          <Route path="pipeline" element={<PipelinePage />} />
          <Route path="storage" element={<StoragePage />} />
          <Route
            path="api-keys"
            element={isAdmin ? <ApiKeysPage /> : <Navigate to="/" replace />}
          />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Route>
      </Routes>
    </BrowserRouter>
  );
}
