import {
  Bot,
  CalendarClock,
  Container,
  Inbox,
  ListChecks,
  ListFilter,
  Plus,
  PanelLeftClose,
  Settings,
  ShieldCheck,
  Trash2,
  Workflow,
  X,
} from "lucide-react";
import { useEffect, useMemo, useState, type ReactNode } from "react";
import { Trans, useTranslation } from "react-i18next";
import { Link, NavLink, useMatch, useNavigate } from "react-router-dom";
import { cn } from "../lib/cn";
import { askConfirm } from "../lib/confirm";
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
import { useInbox } from "../hooks/useInbox";
import {
  useDeleteSessions,
  useSessionList,
} from "../hooks/useSessions";
import { sessionTitle } from "../lib/format";
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
  badge,
}: {
  to: string;
  icon: ReactNode;
  label: string;
  testId: string;
  /** Right-aligned count, for a destination that accumulates. */
  badge?: ReactNode;
}) {
  return (
    <NavLink
      to={to}
      data-testid={testId}
      className="row row-quiet text-[0.8125rem]"
    >
      {icon}
      <span className="font-medium">{label}</span>
      {badge}
    </NavLink>
  );
}

/**
 * What the inbox is holding: unread messages, or — louder — the questions an
 * agent has stopped on.
 *
 * One number rather than two. They mean different things, and only `openAsks`
 * means somebody is standing still, so it takes the badge whenever there is
 * one and gets a fill to say so; an unread notice costs nothing and reads as
 * a plain count. Both numbers are in the accessible name, because the quiet
 * one is otherwise invisible while the loud one is shown.
 */
function InboxBadge() {
  const { t } = useTranslation();
  const { data } = useInbox();
  const unread = data?.unread ?? 0;
  const openAsks = data?.openAsks ?? 0;
  if (unread === 0 && openAsks === 0) return null;
  return (
    <span
      data-testid="inbox-badge"
      data-open-asks={openAsks}
      aria-label={t("inbox.badgeLabel", { unread, openAsks })}
      className={cn(
        "ml-auto shrink-0 text-[0.6875rem] tabular-nums",
        openAsks > 0
          ? "rounded-full bg-live-quiet px-1.5 py-px font-medium text-live-ink"
          : "text-faint",
      )}
    >
      {openAsks > 0 ? openAsks : unread}
    </span>
  );
}

/** Icon-only footer link — server-level, visited rarely. The word lives in
 * the tooltip and the accessible name, which is where a label belongs when
 * the destination is one of a few fixed icons you learn once. */
function FooterLink({
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
      title={label}
      aria-label={label}
      // The active one is a key holding a value, and `NavLink` already says
      // which one that is.
      className="key-icon shrink-0"
    >
      {icon}
    </NavLink>
  );
}

export function Sidebar({ onHide }: { onHide?: () => void }) {
  const { t } = useTranslation();
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
  const [selecting, setSelecting] = useState(false);
  const [selected, setSelected] = useState<Set<string>>(new Set());

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
  const openSessionId = useMatch("/sessions/:id/*")?.params.id;
  const del = useDeleteSessions();
  const allShownSelected =
    shown.length > 0 && shown.every((session) => selected.has(session.id));

  useEffect(() => {
    const visible = new Set(shown.map((session) => session.id));
    setSelected((current) => {
      const next = new Set([...current].filter((id) => visible.has(id)));
      return next.size === current.size ? current : next;
    });
  }, [shown]);

  const toggleSelectionMode = () => {
    setSelecting((current) => !current);
    setSelected(new Set());
    setPanelOpen(false);
  };

  const toggleSession = (id: string) => {
    setSelected((current) => {
      const next = new Set(current);
      if (!next.delete(id)) next.add(id);
      return next;
    });
  };

  const toggleAllShown = () => {
    setSelected(
      allShownSelected ? new Set() : new Set(shown.map((session) => session.id)),
    );
  };

  const removeSelected = async () => {
    const ids = [...selected];
    if (
      !(await askConfirm(t("rail.confirmDeleteSelected", { count: ids.length })))
    )
      return;

    try {
      await del.mutateAsync(ids);
      if (openSessionId && selected.has(openSessionId)) navigate("/");
      setSelected(new Set());
      setSelecting(false);
    } catch {
      /* reported by the global failure notice */
    }
  };

  return (
    <aside className="column-edge-r flex h-full w-[17.5rem] shrink-0 flex-col bg-chassis">
      {/* Nameplate. The lamp reports the rail's own link to the server, so a
          dead feed is visible before you click anything. Height is shared with
          the session and task-panel headers so the three columns line up. */}
      <div className="flex h-[var(--header-h)] shrink-0 items-center bar-scroll gap-2.5 px-3">
        <Link
          to="/"
          data-testid="home-link"
          className="-mx-1 flex min-w-0 items-center gap-2.5 px-1 py-0.5"
        >
          <span
            aria-hidden
            className="flex h-6 w-6 shrink-0 items-center justify-center rounded-[4px] bg-accent font-mono text-[0.8125rem] font-bold text-accent-ink"
          >
            h
          </span>
          {/* i18n-ignore: the product's name, not a word to translate. */}
          <span className="font-mono text-[0.8125rem] font-semibold tracking-[0.16em] text-legend">
            HORSIE
          </span>
        </Link>
        {/* The fault state stays outside the link: it is status, not a
            destination. A dead server link is visible nowhere else — without
            it the first symptom is an empty session list that looks like an
            account with no sessions. */}
        <button
          className="key-icon ml-auto hidden shrink-0 md:flex"
          onClick={onHide}
          data-testid="hide-sidebar-button"
          title={t("rail.hideSessions")}
          aria-label={t("rail.hideSessions")}
        >
          <PanelLeftClose size={16} aria-hidden />
        </button>
        {isError && (
          <span
            className="ml-auto flex shrink-0 items-center gap-2 text-red-ink"
            data-testid="rail-state"
          >
            <span className="lamp" aria-hidden />
            <span className="legend text-current">{t("rail.offline")}</span>
          </span>
        )}
      </div>

      {/* The things you keep, before the things you accumulate. */}
      <div className="space-y-px px-2 pt-1">
        {/* First: it is the only rail destination that can be holding a
            stopped agent. */}
        <PrimaryLink
          to="/inbox"
          testId="inbox-link"
          icon={<Inbox size={15} aria-hidden />}
          label={t("nav.inbox")}
          badge={<InboxBadge />}
        />
        <PrimaryLink
          to="/agents"
          testId="agents-link"
          icon={<Bot size={15} aria-hidden />}
          label={t("nav.agents")}
        />
        <PrimaryLink
          to="/environments"
          testId="environments-link"
          icon={<Container size={15} aria-hidden />}
          label={t("nav.environments")}
        />
        <PrimaryLink
          to="/routines"
          testId="routines-link"
          icon={<CalendarClock size={15} aria-hidden />}
          label={t("nav.routines")}
        />
        <PrimaryLink
          to="/workflows"
          testId="workflows-link"
          icon={<Workflow size={15} aria-hidden />}
          label={t("nav.workflows")}
        />
      </div>

      <div className="flex items-center justify-between pb-1 pl-3.5 pr-2 pt-3">
        <span className="legend">
          {selecting
            ? t("rail.selected", { count: selected.size })
            : t("rail.sessions")}
        </span>
        <div className="flex items-center gap-0.5">
          {!selecting && (
            <>
              {/* Nothing to filter by until a tag exists, and an empty panel
                  behind a button is a control with no job. */}
              {tags.length > 0 && (
                <button
                  className="key-icon !h-6 !w-6"
                  data-marked={filterIsActive(filter) ? "true" : undefined}
                  onClick={() => setPanelOpen((v) => !v)}
                  aria-expanded={panelOpen}
                  data-testid="tag-filter-button"
                  title={t("rail.filterByTag")}
                  aria-label={t("rail.filterByTag")}
                >
                  <ListFilter size={14} aria-hidden />
                </button>
              )}
              <button
                className="key-icon !h-6 !w-6"
                onClick={() => navigate("/")}
                data-testid="new-session-button"
                title={t("rail.newSession")}
                aria-label={t("rail.newSession")}
              >
                <Plus size={14} aria-hidden />
              </button>
            </>
          )}
          {(sessions?.length ?? 0) > 0 && (
            <button
              className="key-icon !h-6 !w-6"
              onClick={toggleSelectionMode}
              aria-pressed={selecting}
              data-testid="session-selection-button"
              title={t(selecting ? "rail.exitSelection" : "rail.selectSessions")}
              aria-label={t(
                selecting ? "rail.exitSelection" : "rail.selectSessions",
              )}
            >
              {selecting ? (
                <X size={14} aria-hidden />
              ) : (
                <ListChecks size={14} aria-hidden />
              )}
            </button>
          )}
        </div>
      </div>

      {selecting && (
        <div
          className="flex items-center gap-2 px-2 pb-1.5"
          data-testid="session-selection-toolbar"
        >
          <label className="row row-quiet min-w-0 flex-1 px-2 py-1.5 text-[0.75rem]">
            <input
              type="checkbox"
              checked={allShownSelected}
              ref={(node) => {
                if (node)
                  node.indeterminate = selected.size > 0 && !allShownSelected;
              }}
              onChange={toggleAllShown}
              aria-label={t("rail.selectAllSessions")}
              data-testid="select-all-sessions"
            />
            <span className="truncate">{t("rail.selectAll")}</span>
          </label>
          <button
            type="button"
            className="key key-stop key-sm shrink-0"
            onClick={() => void removeSelected()}
            disabled={selected.size === 0 || del.isPending}
            data-testid="delete-selected-sessions"
          >
            <Trash2 size={13} aria-hidden />
            {t("common.delete")}
          </button>
        </div>
      )}

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
            className="field !py-1 !text-[0.8125rem]"
            value={filterText}
            onChange={(e) => setFilterText(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape") setFilterText("");
            }}
            placeholder={t("rail.filterPlaceholder")}
            aria-label={t("rail.filterSessions")}
            data-testid="session-filter"
          />
        </div>
      )}

      <nav
        className="flex-1 space-y-px overflow-y-auto px-2 pb-2"
        aria-label={t("rail.sessions")}
      >
        {isLoading && (
          <div className="legend px-2.5 py-6">{t("common.loading")}</div>
        )}
        {isError && (
          <p className="px-2.5 py-6 text-[0.8125rem] leading-relaxed text-red-ink">
{t("rail.unreachable")}
          </p>
        )}
        {!isLoading && !isError && sessions?.length === 0 && (
          <p className="empty">
            <Trans i18nKey="rail.empty" components={{ key: <strong /> }} />
          </p>
        )}
        {/* Two filters narrow one list, so an empty result has to name the one
            that emptied it. Otherwise a filtered rail reads as a lost account. */}
        {!isLoading &&
          !isError &&
          shown.length === 0 &&
          (sessions?.length ?? 0) > 0 &&
          (needle !== "" ? (
            <p className="empty" data-testid="no-text-matches">
              {t("rail.noTextMatches", { query: filterText.trim() })}
            </p>
          ) : (
            <p className="empty" data-testid="no-tag-matches">
              {t("rail.noTagMatches")}
            </p>
          ))}
        {!isLoading &&
          !isError &&
          // Sessions only. A session's sub sessions are its *shape*, and the
          // graph draws that — lineage, what each one is doing, and what each
          // one spawned — so listing them here as well was a second structural
          // view of the same thing, and the one with less to say.
          shown.map((s) => (
            <SessionRow
              key={s.id}
              s={s}
              tags={tags}
              selected={selected.has(s.id)}
              onToggle={selecting ? () => toggleSession(s.id) : undefined}
            />
          ))}
      </nav>

      {/* The scope everything above belongs to, and the server-level
          destinations, on one strip. The switcher used to sit under the
          nameplate with the word "Project" over it — two rows of rail height
          for one string that is also in the URL. */}
      <div className="bar-scroll flex items-center gap-0.5 px-1.5 py-1.5">
        <ProjectSwitcher />
        <FooterLink
          to="/settings"
          testId="settings-link"
          icon={<Settings size={14} aria-hidden />}
          label={t("nav.settings")}
        />
        <FooterLink
          to="/admin"
          testId="admin-link"
          icon={<ShieldCheck size={14} aria-hidden />}
          label={t("nav.admin")}
        />
        <ThemeToggle />
      </div>
    </aside>
  );
}
