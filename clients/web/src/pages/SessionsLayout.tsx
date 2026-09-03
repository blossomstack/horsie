import { Outlet, useLocation } from "react-router-dom";
import { CollapsedSidebar, Sidebar } from "../components/Sidebar";
import { RailProvider, useRail, useRailAutoClose } from "../components/rail";
import { usePersistentState } from "../hooks/usePersistentState";
import { useGlobalSessionFeed } from "../hooks/useSessions";
import { cn } from "../lib/cn";
import { useTranslation } from "react-i18next";

function Shell() {
  // Single global SSE feed keeps the rail statuses live.
  useGlobalSessionFeed();
  const { t } = useTranslation();
  const { open, setOpen } = useRail();
  const [sidebarHidden, setSidebarHidden] = usePersistentState(
    "horsie.sidebar-hidden",
    false,
  );
  const { pathname } = useLocation();
  useRailAutoClose(pathname);

  return (
    <div className="flex h-full overflow-hidden">
      {/* First in the document, and the first thing Tab reaches. The rail is a
          long control: four destinations, two session actions, a filter, a
          header and a menu per group, and a link and a menu per session — so
          without this the composer is roughly the hundredth stop on a busy
          account. */}
      <a href="#main" className="skip-link" data-testid="skip-to-main">
{t("layout.skipToContent")}
      </a>
      {/* Scrim: only ever present while the drawer is open on a narrow screen. */}
      {open && (
        <button
          className="fixed inset-0 z-30 bg-scrim md:hidden"
          onClick={() => setOpen(false)}
          aria-label={t("layout.closeSessions")}
          tabIndex={-1}
        />
      )}
      {/* `invisible` as well as translated: a drawer parked off-canvas is still
          in the tab order, so on a phone the whole rail sat between the page
          and the keyboard while being nowhere on screen. `visibility` is
          animatable and flips only at the end of a transition, so the slide out
          still plays in full before the rail goes away. */}
      <div
        className={cn(
          "relative z-40 h-full shrink-0 overflow-hidden transition-[width,transform,visibility] duration-200 ease-out max-md:fixed max-md:inset-y-0 max-md:left-0 max-md:shadow-[var(--float)]",
          sidebarHidden ? "md:w-12" : "md:w-[17.5rem]",
          open ? "max-md:translate-x-0" : "max-md:invisible max-md:-translate-x-full",
        )}
      >
        <div className={cn("h-full", sidebarHidden && "md:invisible")}>
          <Sidebar onHide={() => setSidebarHidden(true)} />
        </div>
        {sidebarHidden && (
          <CollapsedSidebar onShow={() => setSidebarHidden(false)} />
        )}
      </div>
      {/* `tabIndex={-1}` is what makes the skip link land: an anchor to a
          container the browser cannot focus moves the scroll and leaves the
          keyboard where it was. No ring on it — a 2px outline round the whole
          column is a lot of chrome for a stop you leave on the next Tab, and
          that Tab lands on a real control that draws its own. */}
      <main id="main" tabIndex={-1} className="min-w-0 flex-1 bg-panel outline-none">
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
