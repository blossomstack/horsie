import { NavLink } from "react-router-dom";
import type { SessionSummary } from "../api/types";
import { relativeTime, sessionTitle } from "../lib/format";
import { cn } from "../lib/cn";
import { statusMeta } from "../lib/status";
import { useSetSessionAnnotations } from "../hooks/useGroups";
import { Menu, MenuItem } from "./Menu";
import { StatusDot } from "./StatusBadge";

export const SESSION_DRAG_MIME = "application/x-horsie-session";
export const GROUP_DRAG_MIME = "application/x-horsie-group";

/** One channel strip on the rail: lamp, name, and what the channel last did. */
export function SessionRow({
  s,
  groups,
}: {
  s: SessionSummary;
  groups: string[];
}) {
  const title = sessionTitle(s.name);
  const meta = statusMeta(s.status);
  const setAnnotations = useSetSessionAnnotations();
  return (
    // The menu is a sibling of the link, not a child: a button inside an
    // anchor is invalid, and assistive tech can't reliably reach it there.
    <div className="group relative">
      <NavLink
        to={`/sessions/${s.id}`}
        data-testid="session-row"
        data-session-id={s.id}
        title={`${title} — ${meta.hint}`}
        draggable
        onDragStart={(e) => {
          e.dataTransfer.setData(SESSION_DRAG_MIME, s.id);
          e.dataTransfer.effectAllowed = "move";
        }}
        className={({ isActive }) =>
          cn(
            "flex items-start gap-2.5 rounded-[var(--radius-control)] py-2 pl-2.5 transition-colors",
            // Room for the menu so a long title never runs under it.
            groups.length ? "pr-9" : "pr-2.5",
            isActive
              ? "bg-raised text-legend shadow-[inset_0_0_0_1px_var(--rule-strong)]"
              : "text-dim hover:bg-raised hover:text-legend",
          )
        }
      >
        <StatusDot status={s.status} className="mt-[7px]" />
        <span className="min-w-0 flex-1">
          <span className="block truncate text-[0.8125rem] leading-5">
            {title}
          </span>
          <span className="legend mt-0.5 block truncate">
            {/* A run says which workflow it came from: the rail holds runs and
                ordinary sessions together, so the row has to say which it is. */}
            {s.workflow ? `${s.workflow} · ` : ""}
            {meta.label !== "—" ? `${meta.label} · ` : ""}
            {relativeTime(s.createdAt)}
          </span>
        </span>
      </NavLink>
      {/* Nothing to move a session to until a group exists, so the control
          stays out of the way until then. Revealed on row hover; focus-within
          keeps it visible while its menu is open and the pointer has left. */}
      {groups.length > 0 && (
        <span className="absolute right-1.5 top-1.5 opacity-0 transition-opacity focus-within:opacity-100 group-hover:opacity-100">
          <Menu label="Move session" testId={`session-row-menu-${s.id}`}>
            {groups.map((g) => (
              <MenuItem
                key={g}
                testId={`move-to-group-${g}`}
                onSelect={() =>
                  setAnnotations.mutate({
                    id: s.id,
                    set: [{ key: "group", value: g }],
                    remove: [],
                  })
                }
              >
                {g}
              </MenuItem>
            ))}
            <MenuItem
              testId="move-to-group-ungrouped"
              onSelect={() =>
                setAnnotations.mutate({ id: s.id, set: [], remove: ["group"] })
              }
            >
              Ungrouped
            </MenuItem>
          </Menu>
        </span>
      )}
    </div>
  );
}
