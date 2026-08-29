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
import { Trans, useTranslation } from "react-i18next";
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
          "flex items-center gap-2.5 rounded-[var(--radius-control)] px-2.5 py-1.5 text-[0.8125rem] transition-colors",
          // Fill only, like every other selected row in the app.
          isActive
            ? "bg-accent-quiet text-legend"
            : "text-dim hover:bg-raised hover:text-legend",
        )
      }
    >
      {icon}
      <span className="font-medium">{label}</span>
    </NavLink>
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
      className={({ isActive }) =>
        cn("key-icon shrink-0", isActive && "bg-accent-quiet text-legend")
      }
    >
      {icon}
    </NavLink>
  );
}

export function Sidebar() {
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
            <span className="legend text-current">{t("rail.offline")}</span>
          </span>
        )}
      </div>

      {/* The things you keep, before the things you accumulate. */}
      <div className="space-y-px px-2 pt-1">
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
        <span className="legend">{t("rail.sessions")}</span>
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
                  "!bg-accent-quiet !text-legend",
              )}
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
          <p className="px-2.5 py-8 text-[0.8125rem] leading-relaxed text-faint">
            <Trans
              i18nKey="rail.empty"
              components={{ key: <span className="text-legend" /> }}
            />
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
              {t("rail.noTextMatches", { query: filterText.trim() })}
            </p>
          ) : (
            <p
              className="px-2.5 py-8 text-[0.8125rem] leading-relaxed text-faint"
              data-testid="no-tag-matches"
            >
              {t("rail.noTagMatches")}
            </p>
          ))}
        {!isLoading &&
          !isError &&
          // Sessions only. A session's sub sessions are its *shape*, and the
          // graph draws that — lineage, what each one is doing, and what each
          // one spawned — so listing them here as well was a second structural
          // view of the same thing, and the one with less to say.
          shown.map((s) => <SessionRow key={s.id} s={s} tags={tags} />)}
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
