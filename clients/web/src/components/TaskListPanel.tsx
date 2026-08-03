import { ChevronsRight, Circle, CircleCheck, ListTodo } from "lucide-react";
import { useState } from "react";
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

/** The agent's live `task_list` state as a plan readout. Renders nothing until
 * the agent has created a list at least once. */
export function TaskListPanel({ tasks }: { tasks: TaskItem[] }) {
  // Below lg the plan opens over the transcript instead of stealing a column,
  // and it starts collapsed to its done/total badge — hiding it outright left
  // narrow screens with no plan and no way to ask for one.
  const [collapsed, setCollapsed] = useState(
    typeof window !== "undefined" && window.matchMedia("(max-width: 1023px)").matches,
  );
  if (tasks.length === 0) return null;

  const done = tasks.filter((t) => t.status === TaskStatus.Completed).length;

  if (collapsed) {
    return (
      <aside className="flex shrink-0 flex-col items-center border-l bg-panel px-1.5 py-2.5">
        <button
          className="key-icon !h-auto !w-auto flex-col gap-1 px-1.5 py-2"
          onClick={() => setCollapsed(false)}
          title="Show the agent's task list"
          data-testid="task-list-expand"
        >
          <ListTodo size={15} aria-hidden />
          <span className="readout text-[10px] leading-none">
            {done}/{tasks.length}
          </span>
        </button>
      </aside>
    );
  }

  return (
    <aside
      className="flex w-64 shrink-0 flex-col border-l bg-panel max-lg:absolute max-lg:inset-y-0 max-lg:right-0 max-lg:z-20 max-lg:shadow-[var(--panel-lift)]"
      data-testid="task-list-panel"
    >
      <div className="flex items-center gap-2 border-b px-3 py-3">
        <h2 className="legend !text-dim">Plan</h2>
        <span className="readout text-[11px]" data-testid="task-list-progress">
          {done}/{tasks.length} done
        </span>
        <button
          className="key-icon ml-auto !h-7 !w-7"
          onClick={() => setCollapsed(true)}
          title="Collapse the task list"
          aria-label="Collapse the task list"
          data-testid="task-list-collapse"
        >
          <ChevronsRight size={14} aria-hidden />
        </button>
      </div>
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
    </aside>
  );
}
