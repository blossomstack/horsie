import { Check } from "lucide-react";
import { useRef, useState } from "react";
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
import { useTranslation } from "react-i18next";

/** One channel strip on the rail: lamp, name, and what the channel last did. */
export function SessionRow({
  s,
  tags,
  selected,
  onToggle,
}: {
  s: SessionSummary;
  /** Every tag in existence, so the menu can offer them all — not only the
   * ones this session already carries. */
  tags: string[];
  selected?: boolean;
  onToggle?: () => void;
}) {
  const title = sessionTitle(s.name);
  const meta = statusMeta(s.status);
  const setTag = useSetSessionTag();
  const del = useDeleteSession();
  const navigate = useNavigate();
  const rename = useRenameSession();
  const [draft, setDraft] = useState("");
  const { t } = useTranslation();
  const [renaming, setRenaming] = useState(false);
  // Set by Escape so the blur that may follow it stays quiet. Firefox fires
  // blur when the focused field is removed; Chrome and Safari do not, so the
  // browser the e2e drives is the one that cannot catch this missing.
  const abandoned = useRef(false);
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

  if (onToggle)
    return (
      <div
        className="row row-quiet items-start px-2.5 py-2"
        data-active={selected ? "true" : undefined}
        data-testid="session-row"
        data-session-id={s.id}
      >
        <input
          type="checkbox"
          checked={selected}
          onChange={onToggle}
          aria-label={t("rail.selectSession", { title })}
          data-testid={`session-select-${s.id}`}
          className="mt-[3px]"
        />
        <StatusDot status={s.status} className="mt-[7px]" />
        <button
          type="button"
          className="min-w-0 flex-1 text-left"
          onClick={onToggle}
        >
          <span className="block truncate text-[0.8125rem] leading-5">
            {title}
          </span>
          {s.workflow && (
            <span className="legend mt-0.5 block truncate">{s.workflow}</span>
          )}
        </button>
      </div>
    );

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

  const commitRename = (value: string) => {
    const next = value.trim();
    setRenaming(false);
    if (next && next !== s.name) rename.mutate({ id: s.id, name: next });
  };

  // Renaming takes over the row itself rather than opening a field inside the
  // menu: the name is edited where the name is, so what you type sits in the
  // list next to the names it has to be told apart from.
  if (renaming)
    return (
      <div className="px-0.5 py-1">
        <input
          data-testid="session-title-input"
          aria-label={t("sessionRow.renameSession")}
          className="field !py-1 !text-[0.8125rem]"
          defaultValue={title}
          // The whole name is selected, because a rename usually replaces it.
          autoFocus
          onFocus={(e) => e.currentTarget.select()}
          // Clicking away is a commit, not a discard — the row is gone from
          // under the pointer either way, and losing the typing to a stray
          // click is the worse of the two.
          onBlur={(e) => {
            if (!abandoned.current) commitRename(e.currentTarget.value);
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter") commitRename(e.currentTarget.value);
            // Escape discards. An unnamed session's field shows "New
            // session", so a commit on the way out would name it that.
            if (e.key === "Escape") {
              abandoned.current = true;
              setRenaming(false);
            }
          }}
        />
      </div>
    );

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
        // Rest, hover, selected and selected-under-the-pointer are all
        // `.row`'s business now — `aria-current="page"`, which NavLink sets
        // on the active link, is what the stylesheet reads. The only thing
        // left here is the room the hover menu needs so a long title never
        // runs under it.
        className="row row-quiet items-start py-2 pl-2.5 pr-9"
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
        <Menu label={t("sessionRow.actions")} testId={`session-row-menu-${s.id}`}>
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
              aria-label={t("sessionRow.newTag")}
              className="field !py-1 !text-[0.8125rem]"
              placeholder={t("sessionRow.newTagPlaceholder")}
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") submitTag();
              }}
            />
          </div>
          <div className="my-1 " role="separator" />
          {/* An action, not a field: the menu offered a second text input
              directly under the tag one, so the two read as a pair of things
              to fill in and it was never clear which name you were typing.
              This one names what it does and hands the editing to the row. */}
          <MenuItem
            testId={`rename-session-${s.id}`}
            onSelect={() => {
              abandoned.current = false;
              setRenaming(true);
            }}
          >
            {t("sessionRow.rename")}
          </MenuItem>
          <MenuItem
            danger
            testId={`delete-session-${s.id}`}
            onSelect={() => void remove()}
          >
            {t("common.delete")}
          </MenuItem>
        </Menu>
      </span>
    </div>
  );
}
