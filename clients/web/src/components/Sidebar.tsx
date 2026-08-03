import {
  Bot,
  CalendarClock,
  Plus,
  Search,
  Settings,
  ShieldCheck,
} from "lucide-react";
import type { ReactNode } from "react";
import { useMemo, useState } from "react";
import { NavLink, useNavigate } from "react-router-dom";
import type { SessionSummary } from "../api/types";
import { relativeTime, sessionTitle } from "../lib/format";
import { cn } from "../lib/cn";
import { statusMeta } from "../lib/status";
import { useSessionList } from "../hooks/useSessions";
import { StatusDot } from "./StatusBadge";
import { ThemeToggle } from "./ThemeToggle";

/** One channel strip on the rail: lamp, name, and what the channel last did. */
function SessionRow({ s }: { s: SessionSummary }) {
  const title = sessionTitle(s.name);
  const meta = statusMeta(s.status);
  return (
    <NavLink
      to={`/sessions/${s.id}`}
      data-testid="session-row"
      data-session-id={s.id}
      title={`${title} — ${meta.hint}`}
      className={({ isActive }) =>
        cn(
          "group flex items-start gap-2.5 rounded-[var(--radius-control)] px-2.5 py-2 transition-colors",
          isActive
            ? "bg-raised text-legend shadow-[inset_0_0_0_1px_var(--rule-strong)]"
            : "text-dim hover:bg-raised hover:text-legend",
        )
      }
    >
      <StatusDot status={s.status} className="mt-[7px]" />
      <span className="min-w-0 flex-1">
        <span className="block truncate text-[13px] leading-5">{title}</span>
        <span className="legend mt-0.5 block truncate">
          {meta.label !== "—" ? `${meta.label} · ` : ""}
          {relativeTime(s.createdAt)}
        </span>
      </span>
    </NavLink>
  );
}

/** A standing destination, above the session list: the things you keep, as
 * opposed to the sessions you accumulate. */
function PrimaryLink({
  to,
  icon,
  label,
  testId,
}: {
  to: string;
  icon: ReactNode;
  label: string;
  testId: string;
}) {
  return (
    <NavLink
      to={to}
      data-testid={testId}
      className={({ isActive }) =>
        cn(
          "flex items-center gap-2.5 rounded-[var(--radius-control)] px-2.5 py-2 text-[13px] transition-colors",
          isActive
            ? "bg-raised text-legend shadow-[inset_0_0_0_1px_var(--rule-strong)]"
            : "text-dim hover:bg-raised hover:text-legend",
        )
      }
    >
      {icon}
      <span className="font-medium">{label}</span>
    </NavLink>
  );
}

/** Small mono link for the rail footer — server-level, visited rarely. */
function FooterLink({
  to,
  icon,
  label,
}: {
  to: string;
  icon: ReactNode;
  label: string;
}) {
  return (
    <NavLink
      to={to}
      data-testid={`${label.toLowerCase()}-link`}
      className={({ isActive }) =>
        cn(
          "flex min-w-0 items-center gap-1.5 rounded-[var(--radius-control)] px-1.5 py-1.5 font-mono text-[10px] font-medium uppercase tracking-[0.08em] transition-colors",
          isActive
            ? "bg-raised text-legend"
            : "text-faint hover:bg-raised hover:text-legend",
        )
      }
    >
      {icon}
      {label}
    </NavLink>
  );
}

export function Sidebar() {
  const { data: sessions, isLoading, isError } = useSessionList();
  const [query, setQuery] = useState("");
  const navigate = useNavigate();

  const filtered = useMemo(() => {
    if (!sessions) return [];
    const q = query.trim().toLowerCase();
    if (!q) return sessions;
    return sessions.filter(
      (s) =>
        (s.name ?? "").toLowerCase().includes(q) ||
        s.id.toLowerCase().includes(q),
    );
  }, [sessions, query]);

  const running = sessions?.filter((s) => statusMeta(s.status).busy).length ?? 0;

  return (
    <aside className="flex h-full w-[17.5rem] shrink-0 flex-col border-r bg-panel">
      {/* Nameplate. The lamp reports the rail's own link to the server, so a
          dead feed is visible before you click anything. Height is shared with
          the session and task-panel headers so the three columns line up. */}
      <div className="flex h-[3.25rem] shrink-0 items-center gap-2.5 border-b px-4">
        <span
          aria-hidden
          className="flex h-6 w-6 items-center justify-center rounded-[4px] bg-orange font-mono text-[13px] font-bold text-orange-ink shadow-[var(--cap-lift)]"
        >
          h
        </span>
        <span className="font-mono text-[13px] font-semibold tracking-[0.16em] text-legend">
          HORSIE
        </span>
        <span
          className={cn(
            "lamp ml-auto",
            isError
              ? "text-red-ink"
              : running > 0
                ? "lamp-live text-amber-ink"
                : "text-lamp-ok",
          )}
          aria-hidden
        />
        <span className="legend" data-testid="rail-state">
          {isError ? "Offline" : running > 0 ? `${running} running` : "Ready"}
        </span>
      </div>

      {/* The things you keep, before the things you accumulate. */}
      <div className="space-y-px px-2 pt-3">
        <PrimaryLink
          to="/agents"
          testId="agents-link"
          icon={<Bot size={15} aria-hidden />}
          label="Agents"
        />
        <PrimaryLink
          to="/routines"
          testId="routines-link"
          icon={<CalendarClock size={15} aria-hidden />}
          label="Routines"
        />
      </div>

      <div className="flex items-center gap-2 px-3 pt-4">
        <div className="relative min-w-0 flex-1">
          <Search
            size={13}
            className="pointer-events-none absolute left-2.5 top-1/2 -translate-y-1/2 text-faint"
            aria-hidden
          />
          <input
            className="field field-mono !py-1.5 !pl-8"
            placeholder="Search"
            aria-label="Search sessions"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            data-testid="session-search"
          />
        </div>
        <button
          className="key !px-2.5"
          onClick={() => navigate("/")}
          data-testid="new-session-button"
          title="Start a new session"
        >
          <Plus size={14} aria-hidden />
          New
        </button>
      </div>

      <div className="flex items-baseline justify-between px-4 pb-1.5 pt-4">
        <span className="legend">Sessions</span>
        <span className="legend" data-testid="session-count">
          {sessions?.length ?? 0}
        </span>
      </div>

      <nav
        className="flex-1 space-y-px overflow-y-auto px-2 pb-2"
        aria-label="Sessions"
      >
        {isLoading && <div className="legend px-2.5 py-6">Loading…</div>}
        {isError && (
          <p className="px-2.5 py-6 text-[13px] leading-relaxed text-red-ink">
            Can’t reach the server. Check that horsie-server is running, then
            reload.
          </p>
        )}
        {!isLoading && !isError && filtered.length === 0 && (
          <p className="px-2.5 py-8 text-[13px] leading-relaxed text-faint">
            {query ? (
              <>No session matches “{query}”.</>
            ) : (
              <>
                No sessions yet. Press <span className="text-legend">New</span>{" "}
                to start one.
              </>
            )}
          </p>
        )}
        {filtered.map((s) => (
          <SessionRow key={s.id} s={s} />
        ))}
      </nav>

      <div className="flex items-center gap-0.5 border-t px-1.5 py-2">
        <FooterLink
          to="/settings"
          icon={<Settings size={13} aria-hidden />}
          label="Settings"
        />
        <FooterLink
          to="/admin"
          icon={<ShieldCheck size={13} aria-hidden />}
          label="Admin"
        />
        <div className="ml-auto">
          <ThemeToggle />
        </div>
      </div>
    </aside>
  );
}
