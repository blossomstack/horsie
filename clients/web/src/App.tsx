import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { BrowserRouter, Navigate, Route, Routes } from "react-router-dom";
import { NewSessionView } from "./pages/NewSessionView";
import { SessionsLayout } from "./pages/SessionsLayout";
import { SessionView } from "./pages/SessionView";
import { SettingsLayout } from "./pages/settings/SettingsLayout";
import { ModelsSettings } from "./pages/settings/ModelsSettings";
import { RuntimesSettings } from "./pages/settings/RuntimesSettings";
import { IntegrationsSettings } from "./pages/settings/IntegrationsSettings";
import { MemorySettings } from "./pages/settings/MemorySettings";
import { SkillsSettings } from "./pages/settings/SkillsSettings";
import { AdminLayout } from "./pages/admin/AdminLayout";
import { ModelCardsPage } from "./pages/admin/ModelCardsPage";

const client = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 5_000,
      retry: 1,
      refetchOnWindowFocus: false,
    },
  },
});

export default function App() {
  return (
    <QueryClientProvider client={client}>
      <BrowserRouter>
        <Routes>
          <Route path="/" element={<SessionsLayout />}>
            <Route index element={<NewSessionView />} />
            <Route path="sessions/:id" element={<SessionView />} />
            <Route path="settings" element={<SettingsLayout />}>
              <Route index element={<Navigate to="models" replace />} />
              <Route path="models" element={<ModelsSettings />} />
              <Route path="runtimes" element={<RuntimesSettings />} />
              <Route path="skills" element={<SkillsSettings />} />
              <Route path="memory" element={<MemorySettings />} />
              <Route path="integrations" element={<IntegrationsSettings />} />
            </Route>
            {/* Pre-redesign paths, kept so old bookmarks keep working. */}
            <Route
              path="skills"
              element={<Navigate to="/settings/skills" replace />}
            />
            <Route
              path="memory"
              element={<Navigate to="/settings/memory" replace />}
            />
            <Route path="admin" element={<AdminLayout />}>
              <Route index element={<Navigate to="model-cards" replace />} />
              <Route path="model-cards" element={<ModelCardsPage />} />
            </Route>
          </Route>
        </Routes>
      </BrowserRouter>
    </QueryClientProvider>
  );
}
