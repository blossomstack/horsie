import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { BrowserRouter, Route, Routes } from "react-router-dom";
import { NewSessionView } from "./pages/NewSessionView";
import { SessionsLayout } from "./pages/SessionsLayout";
import { SessionView } from "./pages/SessionView";
import { ModelsSettings } from "./pages/settings/ModelsSettings";
import { RuntimesSettings } from "./pages/settings/RuntimesSettings";
import { IntegrationsSettings } from "./pages/settings/IntegrationsSettings";
import { MemoryPage } from "./pages/MemoryPage";
import { SkillsPage } from "./pages/SkillsPage";
import { AdminPage } from "./pages/AdminPage";

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
            <Route path="settings" element={<ModelsSettings />} />
            <Route path="settings/runtimes" element={<RuntimesSettings />} />
            <Route
              path="settings/integrations"
              element={<IntegrationsSettings />}
            />
            <Route path="skills" element={<SkillsPage />} />
            <Route path="memory" element={<MemoryPage />} />
            <Route path="admin" element={<AdminPage />} />
          </Route>
        </Routes>
      </BrowserRouter>
    </QueryClientProvider>
  );
}
