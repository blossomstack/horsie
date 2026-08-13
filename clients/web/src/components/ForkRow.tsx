import { NavLink, useMatch, useNavigate } from "react-router-dom";
import type { ForkView } from "../api/types";
import { askConfirm } from "../lib/confirm";
import { cn } from "../lib/cn";
import { relativeTime } from "../lib/format";
import { agentStatusMeta, statusMeta } from "../lib/status";
import { useDeleteFork } from "../hooks/useSessions";
import { Menu, MenuItem } from "./Menu";
import { StatusDot } from "./StatusBadge";

/**
 * One forked conversation, indented under the one it branched from.
 *
 * Its own row rather than a variant of `SessionRow`: a fork is not a session,
 * so it has no group to be moved between and no runtime to be told about, and
 * the two rows would have shared a name and almost none of their behaviour.
 *
 * The badge is the fork's own status, never a rollup — a session row says what
 * its main agent is doing, and this says what this conversation is doing. A
 * derived "something in here is running" is a second thing that can disagree
 * with the durable one after a crash.
 */
export function ForkRow({
  sessionId,
  fork,
  depth,
}: {
  sessionId: string;
  fork: ForkView;
  depth: number;
}) {
  const kind = agentStatusMeta(fork.status);
  const meta = statusMeta(kind);
  const del = useDeleteFork();
  const navigate = useNavigate();
  const to = `/sessions/${sessionId}/agents/${fork.id}`;
  const open = useMatch("/sessions/:id/agents/:agentId")?.params.agentId === fork.id;
  // Until the model names it. Not the id — that means nothing to a reader — and
  // not a made-up name either, so the row says what it is instead.
  const title = fork.title ?? "Untitled fork";

  const remove = async () => {
    if (!(await askConfirm(`Delete “${title}”? This cannot be undone.`))) return;
    try {
      await del.mutateAsync({ id: sessionId, forkId: fork.id });
      if (open) navigate(`/sessions/${sessionId}`);
    } catch {
      /* reported by the global failure notice */
    }
  };

  return (
    <div className="group relative">
      <NavLink
        to={to}
        data-testid="fork-row"
        data-fork-id={fork.id}
        data-depth={depth}
        title={`${title} — ${meta.hint}`}
        // Indented by lineage, and capped: past a few levels the indent costs
        // more readable width than it buys clarity, and the rail is narrow.
        style={{ paddingLeft: `${0.625 + Math.min(depth, 4) * 0.75}rem` }}
        className={({ isActive }) =>
          cn(
            "flex items-start gap-2.5 rounded-[var(--radius-control)] py-1.5 pr-9 transition-colors",
            isActive
              ? "bg-raised text-legend shadow-[inset_0_0_0_1px_var(--rule-strong)]"
              : "text-dim hover:bg-raised hover:text-legend",
          )
        }
      >
        {/* The branch glyph is what makes an indented row read as *from* the
            row above rather than merely after it. */}
        <span className="mt-[3px] select-none text-faint" aria-hidden>
          ⌐
        </span>
        <StatusDot status={kind} className="mt-[5px]" />
        <span className="min-w-0 flex-1">
          <span className="block truncate text-[0.8125rem] leading-5">
            {title}
          </span>
          <span className="legend mt-0.5 block truncate">
            {meta.label} · {relativeTime(fork.createdAtMs)}
          </span>
        </span>
      </NavLink>
      <span className="absolute right-1.5 top-1 opacity-0 transition-opacity focus-within:opacity-100 group-hover:opacity-100">
        <Menu label="Fork actions" testId={`fork-row-menu-${fork.id}`}>
          <MenuItem
            danger
            testId={`delete-fork-${fork.id}`}
            onSelect={() => void remove()}
          >
            Delete fork
          </MenuItem>
        </Menu>
      </span>
    </div>
  );
}
