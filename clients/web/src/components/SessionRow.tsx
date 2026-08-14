import { NavLink, useMatch, useNavigate } from "react-router-dom";
import type { SessionSummary } from "../api/types";
import { sessionTitle } from "../lib/format";
import { askConfirm } from "../lib/confirm";
import { cn } from "../lib/cn";
import { statusMeta } from "../lib/status";
import { useSetSessionAnnotations } from "../hooks/useGroups";
import { useDeleteSession } from "../hooks/useSessions";
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
  const del = useDeleteSession();
  const navigate = useNavigate();
  // Whether this row is the session on screen. `useMatch` rather than
  // `useParams`, because the rail is mounted outside the route that names one.
  const open = useMatch("/sessions/:id")?.params.id === s.id;

  const remove = async () => {
    if (!(await askConfirm(`Delete “${title}”? This cannot be undone.`))) return;
    try {
      await del.mutateAsync(s.id);
      // Deleting the session on screen leaves a view of something that no
      // longer exists, so it steps back to the new-session page.
      if (open) navigate("/");
    } catch {
      /* reported by the global failure notice */
    }
  };

  return (
    // The menu is a sibling of the link, not a child: a button inside an
    // anchor is invalid, and assistive tech can't reliably reach it there.
    <div className="group relative">
      <NavLink
        to={`/sessions/${s.id}`}
        // A fork lives at `/sessions/:id/agents/:forkId`, which is a
        // *descendant* of this path — so without `end` opening a fork lit its
        // parent session up as well and two rows claimed to be the one on
        // screen. `open` below already made this distinction; the styling
        // simply never got it.
        end
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
            // Room for the menu so a long title never runs under it. Always,
            // now that the menu is always there.
            "flex items-start gap-2.5 rounded-[var(--radius-control)] py-2 pl-2.5 pr-9 transition-colors",
            // The raised fill is the whole cue. The ring that used to sit on
            // top of it drew a border that competed with the fork rails
            // beneath for the same job.
            isActive
              ? "bg-raised text-legend"
              : "text-dim hover:bg-raised hover:text-legend",
          )
        }
      >
        <StatusDot status={s.status} className="mt-[7px]" />
        <span className="min-w-0 flex-1">
          <span className="block truncate text-[0.8125rem] leading-5">
            {title}
          </span>
          {/* A run says which workflow it came from: the rail holds runs and
              ordinary sessions together, so the row has to say which it is.
              Nothing else does — the status is already the dot beside the
              title, and the age is on the session itself for anyone who wants
              it. Spelling both out under every row gave a list of "Idle · just
              now" that said the same thing on every line and cost a second
              line of height to do it. */}
          {s.workflow && (
            <span className="legend mt-0.5 block truncate">{s.workflow}</span>
          )}
        </span>
      </NavLink>
      {/* Revealed on row hover; focus-within keeps it visible while its menu is
          open and the pointer has left. The moves only appear once there is
          somewhere to move to, but Delete always does — it used to live only
          inside the session, so a session you did not want to open was one you
          could not get rid of. */}
      <span className="absolute right-1.5 top-1.5 opacity-0 transition-opacity focus-within:opacity-100 group-hover:opacity-100">
        <Menu label="Session actions" testId={`session-row-menu-${s.id}`}>
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
          {groups.length > 0 && (
            <>
              <MenuItem
                testId="move-to-group-ungrouped"
                onSelect={() =>
                  setAnnotations.mutate({ id: s.id, set: [], remove: ["group"] })
                }
              >
                Ungrouped
              </MenuItem>
              <div className="my-1 border-t" role="separator" />
            </>
          )}
          <MenuItem
            danger
            testId={`delete-session-${s.id}`}
            onSelect={() => void remove()}
          >
            Delete session
          </MenuItem>
        </Menu>
      </span>
    </div>
  );
}
