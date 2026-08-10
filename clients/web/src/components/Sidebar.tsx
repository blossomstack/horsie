import {
  Bot,
  CalendarClock,
  Check,
  Container,
  FolderPlus,
  Plus,
  Settings,
  ShieldCheck,
  Workflow,
  X,
} from "lucide-react";
import { useMemo, useState, type ReactNode } from "react";
import { NavLink, useNavigate } from "react-router-dom";
import { cn } from "../lib/cn";
import {
  partitionSessions,
  reconcileOrder,
  UNGROUPED,
  unionGroups,
} from "../lib/sessionGroups";
import { useCreateGroup, useGroupList } from "../hooks/useGroups";
import { usePersistentState } from "../hooks/usePersistentState";
import { useSessionList } from "../hooks/useSessions";
import { sessionTitle } from "../lib/format";
import { SessionGroupSection } from "./SessionGroupSection";
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
  const { data: registeredGroups } = useGroupList();
  const createGroup = useCreateGroup();
  const [addingGroup, setAddingGroup] = useState(false);
  const [newGroupName, setNewGroupName] = useState("");
  // Persisted section order; reconciled with live groups for display only —
  // written back solely on an explicit drag reorder.
  const [savedOrder, setSavedOrder] = usePersistentState<string[]>(
    "horsie.session-group-order",
    [],
    {
      deserialize: (raw) =>
        Array.isArray(raw) && raw.every((x) => typeof x === "string")
          ? (raw as string[])
          : undefined,
    },
  );
  // Which sections are shut. Persisted for the same reason the order is: an
  // arrangement that half survives a reload is worse than one that does not,
  // and collapsing a group is how a 41-session rail is made readable at all.
  const [collapsed, setCollapsed] = usePersistentState<string[]>(
    "horsie.session-groups-collapsed",
    [],
    {
      deserialize: (raw) =>
        Array.isArray(raw) && raw.every((x) => typeof x === "string")
          ? (raw as string[])
          : undefined,
    },
  );
  const [filter, setFilter] = useState("");
  const groups = useMemo(
    () => unionGroups(registeredGroups ?? [], sessions ?? []),
    [registeredGroups, sessions],
  );
  const ordered = useMemo(
    () => reconcileOrder(savedOrder, groups),
    [savedOrder, groups],
  );
  // Filtering happens before partitioning, so a group whose every session is
  // filtered out disappears with them rather than sitting there empty.
  const needle = filter.trim().toLowerCase();
  const shown = useMemo(() => {
    if (!needle) return sessions ?? [];
    return (sessions ?? []).filter((s) =>
      [sessionTitle(s.name), s.workflow ?? ""]
        .join(" ")
        .toLowerCase()
        .includes(needle),
    );
  }, [sessions, needle]);
  const parts = useMemo(
    () => partitionSessions(shown, groups),
    [shown, groups],
  );
  const navigate = useNavigate();

  const submitGroup = () => {
    const name = newGroupName.trim();
    if (!name) return;
    createGroup.mutate(name);
    setAddingGroup(false);
  };

  return (
    <aside className="flex h-full w-[17.5rem] shrink-0 flex-col border-r bg-panel">
      {/* Nameplate. The lamp reports the rail's own link to the server, so a
          dead feed is visible before you click anything. Height is shared with
          the session and task-panel headers so the three columns line up. */}
      <div className="flex h-[3.25rem] shrink-0 items-center gap-2.5 border-b px-4">
        <span
          aria-hidden
          className="flex h-6 w-6 items-center justify-center rounded-[4px] bg-orange font-mono text-[0.8125rem] font-bold text-orange-ink shadow-[var(--cap-lift)]"
        >
          h
        </span>
        <span className="font-mono text-[0.8125rem] font-semibold tracking-[0.16em] text-legend">
          HORSIE
        </span>
        {/* Only the fault state earns a place here. "N running" restated what
            every session row below already carries in its own status dot and
            word, and "Ready" labelled the absence of news. A dead server link
            is the one thing that is visible nowhere else — without it the
            first symptom is an empty session list that looks like an account
            with no sessions. */}
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

      {/* The + is the only session control here, so it carries the row's
          right edge; the group action sits to its left. */}
      <div className="flex items-center justify-between pb-1.5 pl-4 pr-2 pt-4">
        <span className="legend">Sessions</span>
        <div className="flex items-center gap-0.5">
          <button
            className="key-icon !h-6 !w-6"
            onClick={() => {
              setNewGroupName("");
              setAddingGroup(true);
            }}
            data-testid="new-group-button"
            title="Create a group"
            aria-label="Create a group"
          >
            <FolderPlus size={14} aria-hidden />
          </button>
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

      {/* Only once there is enough to search. Below that the box is a control
          with nothing to do, taking rail height from the list itself. */}
      {(sessions?.length ?? 0) > 8 && (
        <div className="px-2 pb-1.5">
          <input
            className="w-full rounded-[var(--radius-control)] border bg-panel px-2 py-1 text-[0.8125rem] text-legend outline-none placeholder:text-faint focus:border-[var(--rule-strong)]"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape") setFilter("");
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
        {/* Only when there is nothing at all to show. With a group present the
            sections speak for themselves, and the invitation would compete with
            them. */}
        {!isLoading && !isError && sessions?.length === 0 && !groups.length && (
          <p className="px-2.5 py-8 text-[0.8125rem] leading-relaxed text-faint">
            No sessions yet. Press <span className="text-legend">+</span> to
            start one.
          </p>
        )}
        {/* Enter and Escape still work, but they were the *only* way out: the
            box had no visible commit and no visible way to back out of one you
            opened by mistake. */}
        {addingGroup && (
          <div className="mx-1 mb-1 flex items-center gap-1">
            <input
              data-testid="group-name-input"
              className="min-w-0 flex-1 rounded-[var(--radius-control)] border bg-panel px-2 py-1 text-[0.8125rem] text-legend outline-none focus:border-[var(--rule-strong)]"
              placeholder="Group name"
              value={newGroupName}
              autoFocus
              onChange={(e) => setNewGroupName(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") submitGroup();
                else if (e.key === "Escape") setAddingGroup(false);
              }}
            />
            <button
              type="button"
              className="key key-go !h-7 !w-7 !p-0"
              disabled={newGroupName.trim() === ""}
              onClick={submitGroup}
              data-testid="create-group-confirm"
              title="Create group"
              aria-label="Create group"
            >
              <Check size={14} aria-hidden />
            </button>
            <button
              type="button"
              className="key-icon !h-7 !w-7"
              onClick={() => setAddingGroup(false)}
              data-testid="create-group-cancel"
              title="Cancel"
              aria-label="Cancel"
            >
              <X size={14} aria-hidden />
            </button>
          </div>
        )}
        {/* A filter that matches nothing has to say so, or it reads as an
            account whose sessions have gone. */}
        {!isLoading && !isError && needle !== "" && shown.length === 0 && (
          <p className="px-2.5 py-8 text-[0.8125rem] leading-relaxed text-faint">
            No session matches “{filter.trim()}”.
          </p>
        )}
        {!isLoading &&
          !isError &&
          ordered
            // While filtering, a section with no surviving rows is noise.
            .filter((g) => needle === "" || (parts.get(g)?.length ?? 0) > 0)
            .map((g) => (
            <SessionGroupSection
              key={g}
              name={g}
              sessions={parts.get(g) ?? []}
              groups={groups}
              ungrouped={g === UNGROUPED}
              // A filter that hides its matches inside a collapsed group has
              // found nothing as far as the user is concerned.
              collapsed={needle === "" && collapsed.includes(g)}
              onToggleCollapsed={() =>
                setCollapsed(
                  collapsed.includes(g)
                    ? collapsed.filter((x) => x !== g)
                    : [...collapsed, g],
                )
              }
              // Until a group exists there is nothing to be un-grouped from,
              // so the Ungrouped header would be a label with no job — and
              // grouping chrome nobody asked for. The rows render bare, and
              // the header appears the moment the first group does.
              bare={g === UNGROUPED && groups.length === 0}
              order={ordered}
              onReorder={setSavedOrder}
            />
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
