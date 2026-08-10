import {
  MutationCache,
  QueryClient,
  QueryClientProvider,
} from "@tanstack/react-query";
import { pushMutationError } from "./api/mutationErrors";
import { ConfirmDialog } from "./components/ConfirmDialog";
import { MutationErrors } from "./components/MutationErrors";
import { useEffect } from "react";
import type { ReactNode } from "react";
import { BrowserRouter, Navigate, Route, Routes } from "react-router-dom";
import { useAuthStatus } from "./hooks/useAuth";
import { AgentEditPage } from "./pages/agents/AgentEditPage";
import { AgentsPage } from "./pages/agents/AgentsPage";
import { DeviceApprovalPage } from "./pages/DeviceApprovalPage";
import { EnvironmentEditPage } from "./pages/environments/EnvironmentEditPage";
import { EnvironmentsPage } from "./pages/environments/EnvironmentsPage";
import { LoginPage } from "./pages/LoginPage";
import { NewSessionView } from "./pages/NewSessionView";
import { RoutineDetailPage } from "./pages/routines/RoutineDetailPage";
import { RoutineEditPage } from "./pages/routines/RoutineEditPage";
import { RoutinesPage } from "./pages/routines/RoutinesPage";
import { WorkflowDetailPage } from "./pages/workflows/WorkflowDetailPage";
import { WorkflowEditPage } from "./pages/workflows/WorkflowEditPage";
import { WorkflowsPage } from "./pages/workflows/WorkflowsPage";
import { SessionsLayout } from "./pages/SessionsLayout";
import { SessionView } from "./pages/SessionView";
import { SettingsLayout } from "./pages/settings/SettingsLayout";
import { ModelsSettings } from "./pages/settings/ModelsSettings";
import { RuntimesSettings } from "./pages/settings/RuntimesSettings";
import { IntegrationsSettings } from "./pages/settings/IntegrationsSettings";
import { MemorySettings } from "./pages/settings/MemorySettings";
import { SkillsSettings } from "./pages/settings/SkillsSettings";
import { AccountSettings } from "./pages/settings/AccountSettings";
import { AppearanceSettings } from "./pages/settings/AppearanceSettings";
import { AdminLayout } from "./pages/admin/AdminLayout";
import { ModelCardsPage } from "./pages/admin/ModelCardsPage";
import { GithubAppPage } from "./pages/admin/GithubAppPage";
import { NotFoundPage } from "./pages/NotFoundPage";

const client = new QueryClient({
  // Every failed write reports itself. Handled here rather than at each call
  // site because a call-site list is a denylist: it silently re-opens the
  // moment someone adds a mutation and forgets an `onError`, which is exactly
  // how ~8 of them came to swallow the server's message.
  mutationCache: new MutationCache({
    onError: (error, _variables, _context, mutation) => {
      // A site that already renders the failure inline says so, so the same
      // error is not reported twice in two places.
      if (mutation.options.meta?.inlineError) return;
      pushMutationError(
        error instanceof Error ? error.message : "The write failed.",
      );
    },
  }),
  defaultOptions: {
    queries: {
      staleTime: 5_000,
      retry: 1,
      refetchOnWindowFocus: false,
    },
  },
});

/** Renders the login page instead of the app when the server wants a
 *  credential this browser does not have.
 *
 *  When identity is managed by a layer in front of the server there is no login
 *  page to render — signing in happens somewhere else entirely — so this
 *  navigates there instead. */
function AuthGate({ children }: { children: ReactNode }) {
  const { data, isPending } = useAuthStatus();
  const needsSignIn = !!data?.enabled && !data.authenticated;

  useEffect(() => {
    if (needsSignIn && data?.external && data.loginUrl) {
      window.location.assign(data.loginUrl);
    }
  }, [needsSignIn, data?.external, data?.loginUrl]);

  // Render nothing until the first status lands: flashing a login form at
  // someone who is already signed in is worse than a blank frame. Same while
  // the redirect above is in flight.
  if (isPending) return null;
  if (needsSignIn) {
    return data.external ? null : <LoginPage />;
  }
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
              {/* One agent of a session, full page: a workflow step, or a
                  subagent, which had no page of its own before. */}
              <Route path="sessions/:id/agents/:agentId" element={<SessionView />} />
              <Route path="agents" element={<AgentsPage />} />
              <Route path="agents/new" element={<AgentEditPage />} />
              <Route path="agents/:name/edit" element={<AgentEditPage />} />
              <Route path="environments" element={<EnvironmentsPage />} />
              <Route path="environments/new" element={<EnvironmentEditPage />} />
              <Route
                path="environments/:name/edit"
                element={<EnvironmentEditPage />}
              />
              <Route path="workflows" element={<WorkflowsPage />} />
              <Route path="workflows/new" element={<WorkflowEditPage />} />
              <Route path="workflows/:name" element={<WorkflowDetailPage />} />
              <Route path="workflows/:name/edit" element={<WorkflowEditPage />} />
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
                <Route path="appearance" element={<AppearanceSettings />} />
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
                <Route path="github-app" element={<GithubAppPage />} />
              </Route>
              {/* Inside the layout, so an unmatched route keeps the rail.
                Without this, anything unrouted rendered an empty document. */}
              <Route path="*" element={<NotFoundPage />} />
            </Route>
          </Routes>
        </AuthGate>
        <MutationErrors />
        <ConfirmDialog />
      </BrowserRouter>
    </QueryClientProvider>
  );
}
