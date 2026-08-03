import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { BrowserRouter, Navigate, Route, Routes } from "react-router-dom";
import { useAuthStatus } from "./hooks/useAuth";
import { AgentEditPage } from "./pages/agents/AgentEditPage";
import { AgentsPage } from "./pages/agents/AgentsPage";
import { DeviceApprovalPage } from "./pages/DeviceApprovalPage";
import { LoginPage } from "./pages/LoginPage";
import { NewSessionView } from "./pages/NewSessionView";
import { RoutineDetailPage } from "./pages/routines/RoutineDetailPage";
import { RoutineEditPage } from "./pages/routines/RoutineEditPage";
import { RoutinesPage } from "./pages/routines/RoutinesPage";
import { SessionsLayout } from "./pages/SessionsLayout";
import { SessionView } from "./pages/SessionView";
import { SettingsLayout } from "./pages/settings/SettingsLayout";
import { ModelsSettings } from "./pages/settings/ModelsSettings";
import { RuntimesSettings } from "./pages/settings/RuntimesSettings";
import { IntegrationsSettings } from "./pages/settings/IntegrationsSettings";
import { MemorySettings } from "./pages/settings/MemorySettings";
import { SkillsSettings } from "./pages/settings/SkillsSettings";
import { AccountSettings } from "./pages/settings/AccountSettings";
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

/** Renders the login page instead of the app when the server wants a
 *  credential this browser does not have. */
function AuthGate({ children }: { children: ReactNode }) {
  const { data, isPending } = useAuthStatus();
  // Render nothing until the first status lands: flashing a login form at
  // someone who is already signed in is worse than a blank frame.
  if (isPending) return null;
  if (data?.enabled && !data.authenticated) return <LoginPage />;
  return <>{children}</>;
}

export default function App() {
  return (
    <QueryClientProvider client={client}>
      <BrowserRouter>
        <AuthGate>
          <Routes>
            <Route path="/" element={<SessionsLayout />}>
              <Route index element={<NewSessionView />} />
              <Route path="sessions/:id" element={<SessionView />} />
              <Route path="agents" element={<AgentsPage />} />
              <Route path="agents/new" element={<AgentEditPage />} />
              <Route path="agents/:name/edit" element={<AgentEditPage />} />
              <Route path="routines" element={<RoutinesPage />} />
              <Route path="routines/new" element={<RoutineEditPage />} />
              <Route path="routines/:name" element={<RoutineDetailPage />} />
              <Route
                path="routines/:name/edit"
                element={<RoutineEditPage />}
              />
              <Route path="auth/device" element={<DeviceApprovalPage />} />
              <Route path="settings" element={<SettingsLayout />}>
                <Route index element={<Navigate to="models" replace />} />
                <Route path="models" element={<ModelsSettings />} />
                <Route path="runtimes" element={<RuntimesSettings />} />
                <Route path="skills" element={<SkillsSettings />} />
                <Route path="memory" element={<MemorySettings />} />
                <Route path="integrations" element={<IntegrationsSettings />} />
                <Route path="account" element={<AccountSettings />} />
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
        </AuthGate>
      </BrowserRouter>
    </QueryClientProvider>
  );
}
