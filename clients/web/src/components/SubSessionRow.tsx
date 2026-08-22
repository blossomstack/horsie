import { NavLink, useMatch, useNavigate } from "react-router-dom";
import type { SubSessionView } from "../api/types";
import { askConfirm } from "../lib/confirm";
import { cn } from "../lib/cn";
import { agentStatusMeta, statusMeta } from "../lib/status";
import { useDeleteSubSession } from "../hooks/useSessions";
import { Menu, MenuItem } from "./Menu";
import { StatusDot } from "./StatusBadge";

/**
 * How many lineage columns are drawn before the indent costs more readable
 * width than it buys clarity. Past this the rows stop stepping right; the
 * elbow still says the row is a sub session, which is the part that matters.
 */
const MAX_RAILS = 4;

/**
 * One branched session, indented under the one it branched from.
 *
 * Its own row rather than a variant of `SessionRow`: a sub session is not a session,
 * so it has no group to be moved between and no runtime to be told about, and
 * the two rows would have shared a name and almost none of their behaviour.
 *
 * The badge is the sub session's own status, never a rollup — a session row says what
 * its main agent is doing, and this says what this session is doing. A
 * derived "something in here is running" is a second thing that can disagree
 * with the durable one after a crash.
 */
export function SubSessionRow({
  sessionId,
  subSession,
  depth,
  rails,
  last,
}: {
  sessionId: string;
  subSession: SubSessionView;
  depth: number;
  rails: boolean[];
  last: boolean;
}) {
  const kind = agentStatusMeta(subSession.status);
  const meta = statusMeta(kind);
  const del = useDeleteSubSession();
  const navigate = useNavigate();
  const to = `/sessions/${sessionId}/agents/${subSession.id}`;
  const open = useMatch("/sessions/:id/agents/:agentId")?.params.agentId === subSession.id;
  // Until the model names it. Not the id — that means nothing to a reader — and
  // not a made-up name either, so the row says what it is instead.
  const title = subSession.title ?? "Untitled subSession";

  const remove = async () => {
    if (!(await askConfirm(`Delete “${title}”? This cannot be undone.`))) return;
    try {
      await del.mutateAsync({ id: sessionId, subSessionId: subSession.id });
      if (open) navigate(`/sessions/${sessionId}`);
    } catch {
      /* reported by the global failure notice */
    }
  };

  return (
    // `items-stretch`, and the lineage drawn outside the link: a rail has to
    // span the row's full height and meet the next row's with no gap, which a
    // glyph on a text baseline cannot do — it would leave a dashed column of
    // disconnected marks. Drawn beside the link rather than inside it so the
    // selected fill stops at the session and the lineage stays lineage.
    <div className="group relative flex items-stretch pl-2.5">
      {rails.slice(0, MAX_RAILS).map((rail, i) => (
        <span
          key={i}
          aria-hidden
          className={cn(
            "w-3 shrink-0",
            // A rail only where an ancestor still has a row below this one.
            // Everywhere else the column is blank, or the line would run
            // through rows that are not this sub session's lineage.
            rail && "border-l border-rule",
          )}
        />
      ))}
      {/* The elbow: down from the row above, then right into this row. It
          stops at the title's centre line when nothing follows at this level
          (└) and carries on down when a sibling does (├). */}
      <span aria-hidden className="relative w-3 shrink-0">
        <span
          className={cn(
            "absolute left-0 top-0 w-px bg-[var(--rule)]",
            last ? "h-4" : "h-full",
          )}
        />
        <span className="absolute left-0 top-4 h-px w-full bg-[var(--rule)]" />
      </span>
      <NavLink
        to={to}
        data-testid="subSession-row"
        data-subSession-id={subSession.id}
        data-depth={depth}
        title={`${title} — ${meta.hint}`}
        className={({ isActive }) =>
          cn(
            "flex min-w-0 flex-1 items-start gap-2.5 rounded-[var(--radius-control)] py-1.5 pl-2 pr-9 transition-colors",
            // The fill alone says which row is open. A ring on top of it drew
            // a second border beside the lineage rails, and the two read as
            // competing structure rather than one selected row.
            isActive
              ? "bg-raised text-legend"
              : "text-dim hover:bg-raised hover:text-legend",
          )
        }
      >
        <StatusDot status={kind} className="mt-[5px]" />
        {/* Title only, as a session row is: the dot beside it already carries
            the status, and a subSession sitting under the session it came from
            does not need to repeat "Idle · just now" on a second line. */}
        <span className="min-w-0 flex-1">
          <span className="block truncate text-[0.8125rem] leading-5">
            {title}
          </span>
        </span>
      </NavLink>
      <span className="absolute right-1.5 top-1 opacity-0 transition-opacity focus-within:opacity-100 group-hover:opacity-100">
        <Menu label="SubSession actions" testId={`subSession-row-menu-${subSession.id}`}>
          <MenuItem
            danger
            testId={`delete-subSession-${subSession.id}`}
            onSelect={() => void remove()}
          >
            Delete subSession
          </MenuItem>
        </Menu>
      </span>
    </div>
  );
}
