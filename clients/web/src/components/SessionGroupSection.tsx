import { ChevronDown, ChevronRight } from "lucide-react";
import { useEffect, useState } from "react";
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
}) {
  const [collapsed, setCollapsed] = useState(false);
  const [renaming, setRenaming] = useState(false);
  const [renameValue, setRenameValue] = useState(name);
  const [deleteArmed, setDeleteArmed] = useState(false);
  const [dropHint, setDropHint] = useState(false);
  const renameGroup = useRenameGroup();
  const deleteGroup = useDeleteGroup();
  const setAnnotations = useSetSessionAnnotations();

  // Two-step delete: the menu closes on select, so the armed state lives here
  // and self-resets if the confirm never comes.
  useEffect(() => {
    if (!deleteArmed) return;
    const t = setTimeout(() => setDeleteArmed(false), 3000);
    return () => clearTimeout(t);
  }, [deleteArmed]);

  const label = ungrouped ? "Ungrouped" : name;

  return (
    <div data-testid={`group-section-${name}`} data-group-name={name}>
      {!bare && (
        <div
          className={cn(
            "group/header flex items-center gap-1 rounded-[var(--radius-control)] px-1.5 py-1",
            dropHint && "bg-raised shadow-[inset_0_0_0_1px_var(--rule-strong)]",
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
          <button
            type="button"
            className="key-icon !h-5 !w-5 shrink-0"
            onClick={() => setCollapsed((v) => !v)}
            aria-label={collapsed ? `Expand ${label}` : `Collapse ${label}`}
          >
            {collapsed ? (
              <ChevronRight size={12} aria-hidden />
            ) : (
              <ChevronDown size={12} aria-hidden />
            )}
          </button>
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
            <span className="legend min-w-0 flex-1 truncate">{label}</span>
          )}
          {!ungrouped && !renaming && (
            <span className="opacity-0 transition-opacity focus-within:opacity-100 group-hover/header:opacity-100">
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
                {deleteArmed ? (
                  <MenuItem
                    danger
                    testId="confirm-delete-group-item"
                    onSelect={() => {
                      setDeleteArmed(false);
                      deleteGroup.mutate(name);
                    }}
                  >
                    Confirm delete?
                  </MenuItem>
                ) : (
                  <MenuItem
                    danger
                    testId="delete-group-item"
                    onSelect={() => setDeleteArmed(true)}
                  >
                    Delete
                  </MenuItem>
                )}
              </Menu>
            </span>
          )}
        </div>
      )}
      {(bare || !collapsed) &&
        sessions.map((s) => <SessionRow key={s.id} s={s} groups={groups} />)}
    </div>
  );
}
