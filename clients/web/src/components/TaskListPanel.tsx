import { ChevronsRight, Circle, CircleCheck } from "lucide-react";
import { TaskStatus, type TaskItem } from "../api/types";
import { cn } from "../lib/cn";

function StatusIcon({ status }: { status: TaskStatus }) {
  switch (status) {
    case TaskStatus.Completed:
      return (
        <CircleCheck size={13} className="shrink-0 text-lamp-ok" aria-hidden />
      );
    case TaskStatus.InProgress:
      return <span className="lamp lamp-live mt-1 text-amber-ink" aria-hidden />;
    case TaskStatus.Pending:
      return <Circle size={13} className="shrink-0 text-faint" aria-hidden />;
  }
}

const STATUS_WORD: Record<TaskStatus, string> = {
  [TaskStatus.Completed]: "Done",
  [TaskStatus.InProgress]: "Running",
  [TaskStatus.Pending]: "Queued",
};

/**
 * The agent's live `task_list` state as a plan readout.
 *
 * Visibility is not this component's decision any more — the session header
 * owns it, so the panel is reachable on every session rather than appearing
 * only once the agent happened to use the tool and vanishing again whenever
 * the server offloaded the session.
 */
export function TaskListPanel({
  tasks,
  onClose,
}: {
  tasks: TaskItem[];
  onClose: () => void;
}) {
  const done = tasks.filter((t) => t.status === TaskStatus.Completed).length;

  return (
    <aside
      // Below lg the plan overlays the transcript instead of taking a column
      // from it — a third column at that width leaves nothing to read.
      className="flex w-64 shrink-0 flex-col border-l bg-panel max-lg:absolute max-lg:inset-y-0 max-lg:right-0 max-lg:z-20 max-lg:shadow-[var(--panel-lift)]"
      data-testid="task-list-panel"
    >
      <div className="flex h-[3.25rem] shrink-0 items-center gap-2 border-b px-3">
        <h2 className="legend !text-dim">Plan</h2>
        {tasks.length > 0 && (
          <span className="readout text-[11px]" data-testid="task-list-progress">
            {done}/{tasks.length} done
          </span>
        )}
        <button
          className="key-icon ml-auto !h-7 !w-7"
          onClick={onClose}
          title="Hide the plan"
          aria-label="Hide the plan"
          data-testid="task-list-collapse"
        >
          <ChevronsRight size={14} aria-hidden />
        </button>
      </div>

      {tasks.length === 0 ? (
        <p
          className="px-3 py-6 text-center text-xs leading-relaxed text-faint"
          data-testid="task-list-empty"
        >
          No plan yet. The agent writes one here when a task is big enough to
          need steps.
        </p>
      ) : (
        <ul className="flex-1 space-y-0.5 overflow-y-auto p-2">
          {tasks.map((t) => (
            <li
              key={t.id}
              className={cn(
                "flex items-start gap-2 rounded-[var(--radius-chip)] px-1.5 py-1.5 text-[13px] leading-snug",
                t.status === TaskStatus.InProgress && "bg-raised",
              )}
              data-testid="task-list-item"
              data-status={t.status}
            >
              <span className="mt-0.5 flex w-3.5 shrink-0 justify-center">
                <StatusIcon status={t.status} />
              </span>
              <span
                className={cn(
                  "min-w-0 break-words",
                  t.status === TaskStatus.Completed
                    ? "text-faint line-through"
                    : "text-legend",
                )}
              >
                {t.content}
              </span>
              <span className="sr-only">{STATUS_WORD[t.status]}</span>
            </li>
          ))}
        </ul>
      )}
    </aside>
  );
}
