import { Outlet } from "react-router-dom";
import { Sidebar } from "../components/Sidebar";
import { useGlobalSessionFeed } from "../hooks/useSessions";

export function SessionsLayout() {
  // Single global SSE feed keeps the sidebar statuses live.
  useGlobalSessionFeed();
  return (
    <div className="flex h-full overflow-hidden">
      <Sidebar />
      <main className="min-w-0 flex-1">
        <Outlet />
      </main>
    </div>
  );
}
