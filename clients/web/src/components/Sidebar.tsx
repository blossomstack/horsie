import {
  Bot,
  CalendarClock,
  Container,
  ListFilter,
  Plus,
  Settings,
  ShieldCheck,
  Workflow,
} from "lucide-react";
import { useMemo, useState, type ReactNode } from "react";
import { Link, NavLink, useNavigate } from "react-router-dom";
import { cn } from "../lib/cn";
import { ProjectSwitcher } from "./ProjectSwitcher";
import {
  allTags,
  EMPTY_FILTER,
  filterIsActive,
  matchesTagFilter,
  reconcileFilter,
  type TagFilter,
} from "../lib/sessionTags";
import { usePersistentState } from "../hooks/usePersistentState";
import { useSessionList } from "../hooks/useSessions";
import { sessionTitle } from "../lib/format";
import { SubSessionRow } from "./SubSessionRow";
import { subSessionTree } from "../lib/subSessionTree";
import { SessionRow } from "./SessionRow";
import { TagFilterPanel } from "./TagFilterPanel";
import { ThemeToggle } from "./ThemeToggle";

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
          "flex items-center gap-2.5 rounded-[var(--radius-control)] px-2.5 py-2 text-[0.8125rem] transition-colors",
          // Fill only, like every other selected row in the app.
          isActive
            ? "bg-raised text-legend"
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
          // `.legend` rather than a hard-coded mono uppercase: these are
          // engraved labels by role, so they follow whatever the active skin
          // decided legends look like.
          "legend flex min-w-0 items-center gap-1.5 rounded-[var(--radius-control)] px-1.5 py-1.5 transition-colors",
          isActive
            ? "bg-raised !text-legend"
            : "hover:bg-raised hover:!text-legend",
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
  // Persisted for the same reason group order and collapse once were: an
  // arrangement that half survives a reload is worse than one that does not.
  const [savedFilter, setSavedFilter] = usePersistentState<TagFilter>(
    "horsie.session-tag-filter",
    EMPTY_FILTER,
    {
      deserialize: (raw) => {
        if (typeof raw !== "object" || raw === null) return undefined;
        const { require, exclude } = raw as Partial<TagFilter>;
        const ok = (v: unknown) =>
          Array.isArray(v) && v.every((x) => typeof x === "string");
        return ok(require) && ok(exclude)
          ? { require: require as string[], exclude: exclude as string[] }
          : undefined;
      },
    },
  );
  const [panelOpen, setPanelOpen] = useState(false);
  const [filterText, setFilterText] = useState("");

  const tags = useMemo(() => allTags(sessions ?? []), [sessions]);
  // Read through the live universe: a constraint naming a tag whose last
  // session was deleted would hide the rail with no visible cause, because the
  // chip that would explain it is not rendered either.
  const filter = useMemo(
    () => reconcileFilter(savedFilter, tags),
    [savedFilter, tags],
  );

  const needle = filterText.trim().toLowerCase();
  const shown = useMemo(
    () =>
      (sessions ?? [])
        .filter((s) => matchesTagFilter(s, filter))
        .filter(
          (s) =>
            !needle ||
            [sessionTitle(s.name), s.workflow ?? ""]
              .join(" ")
              .toLowerCase()
              .includes(needle),
        ),
    [sessions, filter, needle],
  );
  const navigate = useNavigate();

  return (
    <aside className="flex h-full w-[17.5rem] shrink-0 flex-col border-r bg-panel">
      {/* Nameplate. The lamp reports the rail's own link to the server, so a
          dead feed is visible before you click anything. Height is shared with
          the session and task-panel headers so the three columns line up. */}
      <div className="flex h-[3.25rem] shrink-0 items-center gap-2.5 border-b px-4">
        <Link
          to="/"
          data-testid="home-link"
          className="-mx-1 flex min-w-0 items-center gap-2.5 rounded-[var(--radius-control)] px-1 py-0.5 transition-colors hover:bg-raised"
        >
          <span
            aria-hidden
            className="flex h-6 w-6 shrink-0 items-center justify-center rounded-[4px] bg-orange font-mono text-[0.8125rem] font-bold text-orange-ink shadow-[var(--cap-lift)]"
          >
            h
          </span>
          <span className="font-mono text-[0.8125rem] font-semibold tracking-[0.16em] text-legend">
            HORSIE
          </span>
        </Link>
        {/* Only the fault state earns a place here, and it stays outside the
            link: it is status, not a destination. "N running" restated what
            every session row below already carries, and "Ready" labelled the
            absence of news. A dead server link is visible nowhere else —
            without it the first symptom is an empty session list that looks
            like an account with no sessions. */}
        {isError && (
          <span
            className="ml-auto flex shrink-0 items-center gap-2 text-red-ink"
            data-testid="rail-state"
          >
            <span className="lamp" aria-hidden />
            <span className="legend text-current">Offline</span>
          </span>
        )}
      </div>

      {/* The scope everything below belongs to, before the things in it. */}
      <ProjectSwitcher />

      {/* The things you keep, before the things you accumulate. */}
      <div className="space-y-px px-2 pt-3">
        <PrimaryLink
          to="/agents"
          testId="agents-link"
          icon={<Bot size={15} aria-hidden />}
          label="Agents"
        />
        <PrimaryLink
          to="/environments"
          testId="environments-link"
          icon={<Container size={15} aria-hidden />}
          label="Environments"
        />
        <PrimaryLink
          to="/routines"
          testId="routines-link"
          icon={<CalendarClock size={15} aria-hidden />}
          label="Routines"
        />
        <PrimaryLink
          to="/workflows"
          testId="workflows-link"
          icon={<Workflow size={15} aria-hidden />}
          label="Workflows"
        />
      </div>

      <div className="flex items-center justify-between pb-1.5 pl-4 pr-2 pt-4">
        <span className="legend">Sessions</span>
        <div className="flex items-center gap-0.5">
          {/* Nothing to filter by until a tag exists, and an empty panel
              behind a button is a control with no job. */}
          {tags.length > 0 && (
            <button
              className={cn(
                "key-icon !h-6 !w-6",
                // A filtered list must never look like the whole list: the one
                // failure mode of a collapsible filter is a short rail read as
                // an account that has lost its sessions.
                filterIsActive(filter) &&
                  "!bg-raised !text-legend shadow-[inset_0_0_0_1px_var(--rule-strong)]",
              )}
              onClick={() => setPanelOpen((v) => !v)}
              aria-expanded={panelOpen}
              data-testid="tag-filter-button"
              title="Filter by tag"
              aria-label="Filter by tag"
            >
              <ListFilter size={14} aria-hidden />
            </button>
          )}
          <button
            className="key-icon !h-6 !w-6"
            onClick={() => navigate("/")}
            data-testid="new-session-button"
            title="Start a new session"
            aria-label="Start a new session"
          >
            <Plus size={14} aria-hidden />
          </button>
        </div>
      </div>

      {panelOpen && tags.length > 0 && (
        <TagFilterPanel
          tags={tags}
          filter={filter}
          onChange={setSavedFilter}
        />
      )}

      {/* Only once there is enough to search. Below that the box is a control
          with nothing to do, taking rail height from the list itself — but it
          stays once it holds a filter, or deleting down to eight sessions
          would strand one with no way to clear it. */}
      {((sessions?.length ?? 0) > 8 || filterText !== "") && (
        <div className="px-2 pb-1.5">
          <input
            className="w-full rounded-[var(--radius-control)] border bg-panel px-2 py-1 text-[0.8125rem] text-legend outline-none placeholder:text-faint focus:border-[var(--rule-strong)]"
            value={filterText}
            onChange={(e) => setFilterText(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape") setFilterText("");
            }}
            placeholder="Filter sessions…"
            aria-label="Filter sessions"
            data-testid="session-filter"
          />
        </div>
      )}

      <nav
        className="flex-1 space-y-px overflow-y-auto px-2 pb-2"
        aria-label="Sessions"
      >
        {isLoading && <div className="legend px-2.5 py-6">Loading…</div>}
        {isError && (
          <p className="px-2.5 py-6 text-[0.8125rem] leading-relaxed text-red-ink">
            Can’t reach the server. Check that horsie-server is running, then
            reload.
          </p>
        )}
        {!isLoading && !isError && sessions?.length === 0 && (
          <p className="px-2.5 py-8 text-[0.8125rem] leading-relaxed text-faint">
            No sessions yet. Press <span className="text-legend">+</span> to
            start one.
          </p>
        )}
        {/* Two filters narrow one list, so an empty result has to name the one
            that emptied it. Otherwise a filtered rail reads as a lost account. */}
        {!isLoading &&
          !isError &&
          shown.length === 0 &&
          (sessions?.length ?? 0) > 0 &&
          (needle !== "" ? (
            <p
              className="px-2.5 py-8 text-[0.8125rem] leading-relaxed text-faint"
              data-testid="no-text-matches"
            >
              No session matches “{filterText.trim()}”.
            </p>
          ) : (
            <p
              className="px-2.5 py-8 text-[0.8125rem] leading-relaxed text-faint"
              data-testid="no-tag-matches"
            >
              No session matches these tags.
            </p>
          ))}
        {!isLoading &&
          !isError &&
          shown.map((s) => (
            <div key={s.id}>
              <SessionRow s={s} tags={tags} />
              {/* Sub sessions nest under the session they branched from. Built
                  from the flat, parent-linked list the registry holds, so
                  listing sessions still loads none of them. */}
              {subSessionTree(s.subSessions).map(({ subSession, depth, rails, last }) => (
                <SubSessionRow
                  key={subSession.id}
                  sessionId={s.id}
                  subSession={subSession}
                  depth={depth}
                  rails={rails}
                  last={last}
                />
              ))}
            </div>
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
