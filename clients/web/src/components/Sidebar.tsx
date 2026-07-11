import { MessageSquarePlus, Plus, Search, Settings } from "lucide-react";
import { useMemo, useState } from "react";
import { NavLink, useNavigate } from "react-router-dom";
import type { SessionSummary } from "../api/types";
import { relativeTime } from "../lib/format";
import { cn } from "../lib/cn";
import { useSessionList } from "../hooks/useSessions";
import { NewSessionModal } from "./NewSessionModal";
import { StatusDot } from "./StatusBadge";
import { ThemeToggle } from "./ThemeToggle";

function SessionRow({ s }: { s: SessionSummary }) {
  const title = s.name?.trim() || `Session ${s.id.slice(0, 8)}`;
  return (
    <NavLink
      to={`/sessions/${s.id}`}
      className={({ isActive }) =>
        cn(
          "group flex items-center gap-2.5 rounded-[var(--radius)] px-2.5 py-2 text-sm transition-colors",
          isActive
            ? "bg-surface-3 text-text"
            : "text-muted hover:bg-surface-2 hover:text-text",
        )
      }
    >
      <StatusDot status={s.status} />
      <div className="min-w-0 flex-1">
        <div className="truncate font-medium">{title}</div>
        <div className="truncate text-xs text-faint">
          {relativeTime(s.createdAt)}
          {s.lastError ? ` · ${s.lastError}` : ""}
        </div>
      </div>
    </NavLink>
  );
}

export function Sidebar() {
  const { data: sessions, isLoading, isError } = useSessionList();
  const [modal, setModal] = useState(false);
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

  return (
    <aside
      className="flex h-full w-72 shrink-0 flex-col border-r"
      style={{ background: "var(--surface)" }}
    >
      <div className="flex items-center gap-2 px-4 py-3.5">
        <div
          className="flex h-7 w-7 items-center justify-center rounded-lg text-sm font-bold text-accent-fg"
          style={{ background: "var(--accent)" }}
        >
          h
        </div>
        <span className="text-[15px] font-semibold tracking-tight text-text">
          horsie
        </span>
        <button
          className="btn-primary ml-auto !px-2.5 !py-1.5 text-xs"
          onClick={() => setModal(true)}
        >
          <Plus size={15} />
          New
        </button>
      </div>

      <div className="px-3 pb-2">
        <div className="relative">
          <Search
            size={14}
            className="pointer-events-none absolute left-2.5 top-1/2 -translate-y-1/2 text-faint"
          />
          <input
            className="input !py-1.5 !pl-8 text-sm"
            placeholder="Search sessions"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
          />
        </div>
      </div>

      <nav className="flex-1 space-y-0.5 overflow-y-auto px-2 py-1">
        {isLoading && (
          <div className="px-2 py-6 text-center text-sm text-faint">
            Loading…
          </div>
        )}
        {isError && (
          <div className="px-2 py-6 text-center text-sm text-error">
            Can’t reach the server.
          </div>
        )}
        {!isLoading && !isError && filtered.length === 0 && (
          <div className="flex flex-col items-center gap-2 px-3 py-10 text-center">
            <MessageSquarePlus size={22} className="text-faint" />
            <p className="text-sm text-faint">
              {query ? "No matches." : "No sessions yet."}
            </p>
          </div>
        )}
        {filtered.map((s) => (
          <SessionRow key={s.id} s={s} />
        ))}
      </nav>

      <div className="flex items-center justify-between border-t px-3 py-2">
        <NavLink
          to="/settings"
          className={({ isActive }) =>
            cn(
              "flex items-center gap-1.5 rounded-[var(--radius)] px-2 py-1.5 text-xs font-medium transition-colors",
              isActive
                ? "bg-surface-3 text-text"
                : "text-muted hover:bg-surface-2 hover:text-text",
            )
          }
        >
          <Settings size={14} />
          Settings
        </NavLink>
        <div className="flex items-center gap-1">
          <span className="text-xs text-faint">
            {sessions?.length ?? 0} session{sessions?.length === 1 ? "" : "s"}
          </span>
          <ThemeToggle />
        </div>
      </div>

      <NewSessionModal
        open={modal}
        onOpenChange={setModal}
        onCreated={(id) => navigate(`/sessions/${id}`)}
      />
    </aside>
  );
}
