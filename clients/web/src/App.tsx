import {
  MutationCache,
  QueryClient,
  QueryClientProvider,
} from "@tanstack/react-query";
import { ApiRequestError } from "./api/client";
import { i18n } from "./i18n";
import { pushMutationError } from "./api/mutationErrors";
import { ConfirmDialog } from "./components/ConfirmDialog";
import { MutationErrors } from "./components/MutationErrors";
import { useEffect } from "react";
import type { ReactNode } from "react";
import { BrowserRouter, Navigate, Route, Routes } from "react-router-dom";
import { useAuthStatus } from "./hooks/useAuth";
import { AgentsPage } from "./pages/agents/AgentsPage";
import { DeviceApprovalPage } from "./pages/DeviceApprovalPage";
import { EnvironmentsPage } from "./pages/environments/EnvironmentsPage";
import { InboxPage } from "./pages/inbox/InboxPage";
import { LoginPage } from "./pages/LoginPage";
import { NewSessionView } from "./pages/NewSessionView";
import { RoutinesPage } from "./pages/routines/RoutinesPage";
import { WorkflowEditPage } from "./pages/workflows/WorkflowEditPage";
import { WorkflowsPage } from "./pages/workflows/WorkflowsPage";
import { SessionsLayout } from "./pages/SessionsLayout";
import { SessionView } from "./pages/SessionView";
import { SettingsLayout } from "./pages/settings/SettingsLayout";
import { ModelsSettings } from "./pages/settings/ModelsSettings";
import { ModelCardsSettings } from "./pages/settings/ModelCardsSettings";
import { RuntimesSettings } from "./pages/settings/RuntimesSettings";
import { IntegrationsSettings } from "./pages/settings/IntegrationsSettings";
import { MemorySettings } from "./pages/settings/MemorySettings";
import { SkillsSettings } from "./pages/settings/SkillsSettings";
import { AccountSettings } from "./pages/settings/AccountSettings";
import { AppearanceSettings } from "./pages/settings/AppearanceSettings";
import { AdminLayout } from "./pages/admin/AdminLayout";
import { GithubAppPage } from "./pages/admin/GithubAppPage";
import { NotFoundPage } from "./pages/NotFoundPage";
import { ProjectRedirect, projectFromPath } from "./pages/ProjectScope";
import { ProjectsSettings } from "./pages/settings/ProjectsSettings";

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
        error instanceof Error ? error.message : i18n.t("common.writeFailed"),
      );
    },
  }),
  defaultOptions: {
    queries: {
      staleTime: 5_000,
      // One retry, but never against an answer. A 4xx is the server saying it
      // understood and refused; asking again cannot change it, and doing so
      // turned every dead session id into a burst of repeated 404s behind a
      // page that had already decided to render as if nothing were wrong.
      retry: (count, error) =>
        !(
          error instanceof ApiRequestError &&
          error.status >= 400 &&
          error.status < 500
        ) && count < 1,
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

/**
 * The app, rooted at one project.
 *
 * The project lives in the router's **basename** rather than in a route
 * parameter, and that is what lets every `to="/agents"` in the tree stay
 * written the way it was: under a basename of `/p/<id>`, an absolute path is
 * absolute *within the project*. The alternative was prefixing forty-eight link
 * targets by hand, each of which would be a silent escape from the scope if it
 * were missed.
 *
 * It also makes switching projects a full page load, which is the right shape
 * rather than a compromise: nothing carries over — no query cache, no component
 * state, no in-flight request against the project just left. The client's
 * isolation ends up as structural as the server's.
 */
function ProjectApp({ project }: { project: string }) {
  return (
    <BrowserRouter basename={`/p/${project}`}>
      <AuthGate>
        <Routes>
            <Route path="/" element={<SessionsLayout />}>
              <Route index element={<NewSessionView />} />
              <Route path="sessions/:id" element={<SessionView />} />
              {/* One agent of a session, full page: a workflow step, or a
                  subagent, which had no page of its own before. */}
              <Route path="sessions/:id/agents/:agentId" element={<SessionView />} />
              <Route path="inbox" element={<InboxPage />} />
              {/* The roster and what is selected in it are one page, so the
                  name is a parameter of that page rather than a route to
                  somewhere else — and so is editing, which is a third thing the
                  panel can hold rather than a page you leave the roster for.
                  The workflow editor is the exception: it has a list and a
                  panel of its own, and a second roster beside those is one
                  column too many, so it takes the width. */}
              <Route path="agents" element={<AgentsPage />} />
              <Route path="agents/new" element={<AgentsPage editing />} />
              <Route path="agents/:name" element={<AgentsPage />} />
              <Route path="agents/:name/edit" element={<AgentsPage editing />} />
              <Route path="environments" element={<EnvironmentsPage />} />
              <Route
                path="environments/new"
                element={<EnvironmentsPage editing />}
              />
              <Route path="environments/:name" element={<EnvironmentsPage />} />
              <Route
                path="environments/:name/edit"
                element={<EnvironmentsPage editing />}
              />
              <Route path="workflows" element={<WorkflowsPage />} />
              <Route path="workflows/new" element={<WorkflowEditPage />} />
              <Route path="workflows/:name" element={<WorkflowsPage />} />
              <Route path="workflows/:name/edit" element={<WorkflowEditPage />} />
              <Route path="routines" element={<RoutinesPage />} />
              <Route path="routines/new" element={<RoutinesPage editing />} />
              <Route path="routines/:name" element={<RoutinesPage />} />
              <Route path="routines/:name/edit" element={<RoutinesPage editing />} />
              <Route path="settings" element={<SettingsLayout />}>
                <Route index element={<Navigate to="models" replace />} />
                <Route path="models" element={<ModelsSettings />} />
                <Route path="model-cards" element={<ModelCardsSettings />} />
                <Route path="runtimes" element={<RuntimesSettings />} />
                <Route path="skills" element={<SkillsSettings />} />
                <Route path="memory" element={<MemorySettings />} />
                <Route path="integrations" element={<IntegrationsSettings />} />
                <Route path="appearance" element={<AppearanceSettings />} />
                <Route path="account" element={<AccountSettings />} />
                <Route path="projects" element={<ProjectsSettings />} />
              </Route>
              {/* Pre-redesign paths, kept so old bookmarks keep working.
                  Relative targets, because the settings page they point at is
                  inside this project rather than at the root. */}
              <Route path="skills" element={<Navigate to="settings/skills" replace />} />
              <Route path="memory" element={<Navigate to="settings/memory" replace />} />
              <Route
                path="admin/model-cards"
                element={<Navigate to="settings/model-cards" replace />}
              />
              <Route path="admin" element={<AdminLayout />}>
                <Route index element={<Navigate to="github-app" replace />} />
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
  );
}

/**
 * The pages that belong to the account rather than to a project: approving a
 * device code, and choosing which project to open.
 *
 * Rooted at `/` with no basename, because the device-approval link is printed
 * by the CLI from the server's own origin (`{base}/auth/device`) and has no
 * project to name — the person following it has not chosen one yet.
 */
function AccountApp() {
  return (
    <BrowserRouter>
      <AuthGate>
        <Routes>
          <Route path="/auth/device" element={<DeviceApprovalPage />} />
          <Route path="*" element={<ProjectRedirect />} />
        </Routes>
      </AuthGate>
      <MutationErrors />
      <ConfirmDialog />
    </BrowserRouter>
  );
}

export default function App() {
  // Read once, from the URL this document was loaded with. A project switch is
  // a navigation, so this runs again with the new one.
  const project = projectFromPath(window.location.pathname);
  return (
    <QueryClientProvider client={client}>
      {project ? <ProjectApp project={project} /> : <AccountApp />}
    </QueryClientProvider>
  );
}
