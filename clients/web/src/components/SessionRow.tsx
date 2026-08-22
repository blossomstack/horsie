import { Check } from "lucide-react";
import { useState } from "react";
import { NavLink, useMatch, useNavigate } from "react-router-dom";
import type { SessionSummary } from "../api/types";
import { sessionTitle } from "../lib/format";
import { askConfirm } from "../lib/confirm";
import { cn } from "../lib/cn";
import { normalizeTagName, sessionTags } from "../lib/sessionTags";
import { statusMeta } from "../lib/status";
import { useSetSessionTag } from "../hooks/useSessionTags";
import { useDeleteSession } from "../hooks/useSessions";
import { Menu, MenuItem } from "./Menu";
import { useRenameSession } from "../hooks/useSessions";
import { StatusDot } from "./StatusBadge";

/** One channel strip on the rail: lamp, name, and what the channel last did. */
export function SessionRow({
  s,
  tags,
}: {
  s: SessionSummary;
  /** Every tag in existence, so the menu can offer them all — not only the
   * ones this session already carries. */
  tags: string[];
}) {
  const title = sessionTitle(s.name);
  const meta = statusMeta(s.status);
  const setTag = useSetSessionTag();
  const del = useDeleteSession();
  const navigate = useNavigate();
  const rename = useRenameSession();
  const [draft, setDraft] = useState("");
  const mine = new Set(sessionTags(s));
  // Whether this row is the session on screen — including when what is on
  // screen is one of its sub sessions or one of its workflow's steps, both of
  // which live at `/sessions/:id/agents/:agentId`. The rail lists sessions
  // only, so this row is the only thing that can say where you are; matching
  // the exact path alone left a reader inside a sub session with no lit row at
  // all. `useMatch` rather than `useParams`, because the rail is mounted
  // outside the route that names one.
  const inside = useMatch("/sessions/:id/*")?.params.id;
  const open = (useMatch("/sessions/:id")?.params.id ?? inside) === s.id;

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

  // Typing a name nobody has used before is how a tag comes into existence:
  // there is no create step, because there is nothing to register.
  const submitTag = () => {
    const name = normalizeTagName(draft);
    if (!name) return;
    setTag.mutate({ id: s.id, tag: name, on: true });
    setDraft("");
  };

  return (
    // The menu is a sibling of the link, not a child: a button inside an
    // anchor is invalid, and assistive tech can't reliably reach it there.
    <div className="group relative">
      <NavLink
        to={`/sessions/${s.id}`}
        // Deliberately not `end`: a sub session lives at
        // `/sessions/:id/agents/:agentId`, a *descendant* of this path, and
        // this row is what says which session that sub session belongs to.
        // It was `end` while the rail drew a row per sub session — then two
        // rows claimed to be the one on screen — and the rail no longer does.
        data-testid="session-row"
        data-session-id={s.id}
        title={`${title} — ${meta.hint}`}
        className={({ isActive }) =>
          cn(
            // Room for the menu so a long title never runs under it. Always,
            // now that the menu is always there.
            "flex items-start gap-2.5 rounded-[var(--radius-control)] py-2 pl-2.5 pr-9 transition-colors",
            // The raised fill is the whole cue. The ring that used to sit on
            // top of it drew a that competed with the sub session rails
            // beneath for the same job.
            isActive
              ? "bg-accent-quiet text-legend"
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
              line of height to do it. Tags are the same bargain, which is why
              they live in the menu that edits them rather than on the row. */}
          {s.workflow && (
            <span className="legend mt-0.5 block truncate">{s.workflow}</span>
          )}
        </span>
      </NavLink>
      {/* Revealed on row hover; focus-within keeps it visible while its menu is
          open and the pointer has left. Delete always appears — it used to live
          only inside the session, so a session you did not want to open was one
          you could not get rid of. */}
      <span className="absolute right-1.5 top-1.5 opacity-0 transition-opacity focus-within:opacity-100 group-hover:opacity-100">
        <Menu label="Session actions" testId={`session-row-menu-${s.id}`}>
          {tags.map((t) => (
            <MenuItem
              key={t}
              // Tagging a session `web` and `urgent` is one edit, not two
              // trips back through the trigger.
              keepOpen
              testId={`toggle-tag-${t}`}
              onSelect={() =>
                setTag.mutate({ id: s.id, tag: t, on: !mine.has(t) })
              }
            >
              <span className="flex items-center gap-2">
                <Check
                  size={12}
                  aria-hidden
                  className={cn(
                    "shrink-0",
                    mine.has(t) ? "opacity-100" : "opacity-0",
                  )}
                />
                <span className="min-w-0 truncate">{t}</span>
              </span>
            </MenuItem>
          ))}
          <div className="px-2 py-1.5">
            <input
              data-testid="new-tag-input"
              aria-label="New tag"
              className="field !py-1 !text-[0.8125rem]"
              placeholder="New tag…"
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") submitTag();
              }}
            />
          </div>
          <div className="my-1 " role="separator" />
          {/* The header used to carry this as a click-to-edit title, which put
              an editable control where a page title goes. A rename is an
              action on a session, and this is where a session's actions are —
              in the same shape as the tag field above it, rather than a
              `window.prompt` that matches nothing else in the build. */}
          <div className="px-2 py-1.5">
            <input
              data-testid="session-title-input"
              aria-label="Rename session"
              className="field !py-1 !text-[0.8125rem]"
              placeholder="Rename…"
              defaultValue={title}
              onKeyDown={(e) => {
                if (e.key !== "Enter") return;
                const next = e.currentTarget.value.trim();
                if (next && next !== s.name) rename.mutate({ id: s.id, name: next });
              }}
            />
          </div>
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
