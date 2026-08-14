import { ChevronDown, ChevronRight } from "lucide-react";
import { useId, useState } from "react";
import type { SessionSummary } from "../api/types";
import { cn } from "../lib/cn";
import { moveBefore } from "../lib/sessionGroups";
import {
  useDeleteGroup,
  useRenameGroup,
  useSetSessionAnnotations,
} from "../hooks/useGroups";
import { Menu, MenuItem } from "./Menu";
import { GROUP_DRAG_MIME, SESSION_DRAG_MIME, SessionRow } from "./SessionRow";
import { ForkRow } from "./ForkRow";
import { forkTree } from "../lib/forkTree";

/** One sidebar section: a group (or the Ungrouped sentinel) with its session
 * rows. The header is the collapse toggle, the drop target for sessions, and
 * the drag handle for group reorder. */
export function SessionGroupSection({
  name,
  sessions,
  groups,
  ungrouped,
  bare = false,
  order,
  onReorder,
  collapsed,
  onToggleCollapsed,
}: {
  name: string;
  sessions: SessionSummary[];
  groups: string[];
  ungrouped: boolean;
  /** Render the rows with no header — for an Ungrouped section that is the
   * only section, where the header would name a distinction nobody has made. */
  bare?: boolean;
  order: string[];
  onReorder: (next: string[]) => void;
  /** Held by the rail, not here, because it is persisted: group *order*
   * already survived a reload and collapse did not, so half of an arrangement
   * came back and half of it did not. */
  collapsed: boolean;
  onToggleCollapsed: () => void;
}) {
  const [renaming, setRenaming] = useState(false);
  const [renameValue, setRenameValue] = useState(name);
  const [confirmingDelete, setConfirmingDelete] = useState(false);
  const [dropHint, setDropHint] = useState(false);
  const renameGroup = useRenameGroup();
  const deleteGroup = useDeleteGroup();
  const setAnnotations = useSetSessionAnnotations();
  const bodyId = useId();

  const label = ungrouped ? "Ungrouped" : name;

  return (
    <div data-testid={`group-section-${name}`} data-group-name={name}>
      {/* The confirm takes the header's place rather than living in the menu.
          A menu closes on select, so the second step of a two-step delete was
          behind a click that first had to reopen the menu — a confirm nobody
          could reach without knowing it was there. */}
      {!bare && confirmingDelete && (
        <div
          className="mb-1 rounded-[var(--radius-control)] bg-raised px-2 py-1.5 shadow-[inset_0_0_0_1px_var(--rule-strong)]"
          data-testid={`group-delete-confirm-${name}`}
          onKeyDown={(e) => {
            if (e.key === "Escape") setConfirmingDelete(false);
          }}
        >
          <p className="text-[0.8125rem] leading-relaxed text-legend">
            Delete <span className="font-mono">{name}</span>? Its sessions move
            to Ungrouped.
          </p>
          <div className="mt-1.5 flex items-center gap-1.5">
            <button
              type="button"
              className="key key-stop !px-2 !py-1"
              data-testid="confirm-delete-group-item"
              onClick={() => {
                setConfirmingDelete(false);
                deleteGroup.mutate(name);
              }}
            >
              Delete
            </button>
            <button
              type="button"
              className="key key-blank !px-2 !py-1"
              data-testid="cancel-delete-group-item"
              autoFocus
              onClick={() => setConfirmingDelete(false)}
            >
              Cancel
            </button>
          </div>
        </div>
      )}
      {!bare && !confirmingDelete && (
        <div
          className={cn(
            "group/header flex items-center gap-1 rounded-[var(--radius-control)] px-1.5 py-1",
            dropHint
              ? "bg-raised shadow-[inset_0_0_0_1px_var(--rule-strong)]"
              : "hover:bg-raised",
          )}
          draggable={!renaming}
          onDragStart={(e) => {
            e.dataTransfer.setData(GROUP_DRAG_MIME, name);
            e.dataTransfer.effectAllowed = "move";
          }}
          onDragOver={(e) => {
            const t = e.dataTransfer.types;
            if (t.includes(SESSION_DRAG_MIME) || t.includes(GROUP_DRAG_MIME)) {
              e.preventDefault();
              setDropHint(true);
            }
          }}
          onDragLeave={() => setDropHint(false)}
          onDrop={(e) => {
            setDropHint(false);
            const sessionId = e.dataTransfer.getData(SESSION_DRAG_MIME);
            if (sessionId) {
              e.preventDefault();
              setAnnotations.mutate(
                ungrouped
                  ? { id: sessionId, set: [], remove: ["group"] }
                  : {
                      id: sessionId,
                      set: [{ key: "group", value: name }],
                      remove: [],
                    },
              );
              return;
            }
            const dragged = e.dataTransfer.getData(GROUP_DRAG_MIME);
            if (dragged && dragged !== name) {
              e.preventDefault();
              onReorder(moveBefore(order, dragged, name));
            }
          }}
        >
          {renaming ? (
            <input
              data-testid="group-rename-input"
              className="min-w-0 flex-1 rounded-[var(--radius-control)] border bg-panel px-1.5 py-0.5 text-[0.8125rem] text-legend outline-none focus:border-[var(--rule-strong)]"
              value={renameValue}
              autoFocus
              onChange={(e) => setRenameValue(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  const next = renameValue.trim();
                  if (next && next !== name) {
                    renameGroup.mutate({ oldName: name, name: next });
                  }
                  setRenaming(false);
                } else if (e.key === "Escape") {
                  setRenaming(false);
                }
              }}
              onBlur={() => setRenaming(false)}
            />
          ) : (
            // The toggle *is* the row: it spans the free width so the empty
            // space beside a short name collapses the group too. The menu is
            // its sibling, not its child, so `...` still opens the menu.
            <button
              type="button"
              className="flex min-w-0 flex-1 cursor-pointer items-center gap-1 text-left"
              onClick={onToggleCollapsed}
              aria-expanded={!collapsed}
              aria-controls={bodyId}
              data-testid={`group-toggle-${name}`}
            >
              <span className="legend min-w-0 truncate">{label}</span>
              {collapsed ? (
                <ChevronRight size={12} className="shrink-0 text-faint" aria-hidden />
              ) : (
                <ChevronDown size={12} className="shrink-0 text-faint" aria-hidden />
              )}
            </button>
          )}
          {!ungrouped && !renaming && (
            <span className="ml-auto shrink-0 opacity-0 transition-opacity focus-within:opacity-100 group-hover/header:opacity-100">
              <Menu
                label={`${label} actions`}
                testId={`group-menu-button-${name}`}
              >
                <MenuItem
                  testId="rename-group-item"
                  onSelect={() => {
                    setRenameValue(name);
                    setRenaming(true);
                  }}
                >
                  Rename
                </MenuItem>
                <MenuItem
                  danger
                  testId="delete-group-item"
                  onSelect={() => setConfirmingDelete(true)}
                >
                  Delete
                </MenuItem>
              </Menu>
            </span>
          )}
        </div>
      )}
      <div id={bodyId}>
        {(bare || !collapsed) &&
          sessions.map((s) => (
            <div key={s.id}>
              <SessionRow s={s} groups={groups} />
              {/* Forks nest under the conversation they branched from. Built
                  from the flat, parent-linked list the registry holds, so
                  listing sessions still loads none of them. */}
              {forkTree(s.forks).map(({ fork, depth, rails, last }) => (
                <ForkRow
                  key={fork.id}
                  sessionId={s.id}
                  fork={fork}
                  depth={depth}
                  rails={rails}
                  last={last}
                />
              ))}
            </div>
          ))}
      </div>
    </div>
  );
}
