import { Outlet, useLocation } from "react-router-dom";
import { Sidebar } from "../components/Sidebar";
import { RailProvider, useRail, useRailAutoClose } from "../components/rail";
import { useGlobalSessionFeed } from "../hooks/useSessions";
import { cn } from "../lib/cn";

function Shell() {
  // Single global SSE feed keeps the rail statuses live.
  useGlobalSessionFeed();
  const { open, setOpen } = useRail();
  const { pathname } = useLocation();
  useRailAutoClose(pathname);

  return (
    <div className="flex h-full overflow-hidden">
      {/* Scrim: only ever present while the drawer is open on a narrow screen. */}
      {open && (
        <button
          className="fixed inset-0 z-30 bg-[oklch(0.1_0.01_255/0.6)] md:hidden"
          onClick={() => setOpen(false)}
          aria-label="Close sessions"
          tabIndex={-1}
        />
      )}
      <div
        className={cn(
          "z-40 h-full shrink-0 transition-transform duration-200 ease-out max-md:fixed max-md:inset-y-0 max-md:left-0 max-md:shadow-[var(--panel-lift)]",
          open ? "max-md:translate-x-0" : "max-md:-translate-x-full",
        )}
      >
        <Sidebar />
      </div>
      <main className="min-w-0 flex-1">
        <Outlet />
      </main>
    </div>
  );
}

export function SessionsLayout() {
  return (
    <RailProvider>
      <Shell />
    </RailProvider>
  );
}
