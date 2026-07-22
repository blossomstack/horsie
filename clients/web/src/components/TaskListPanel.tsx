import { ChevronsRight, Circle, CircleCheck, ListTodo, Loader2 } from "lucide-react";
import { useState } from "react";
import { TaskStatus, type TaskItem } from "../api/types";
import { cn } from "../lib/cn";

function StatusIcon({ status }: { status: TaskStatus }) {
  switch (status) {
    case TaskStatus.Completed:
      return <CircleCheck size={14} className="shrink-0 text-success" />;
    case TaskStatus.InProgress:
      return <Loader2 size={14} className="shrink-0 animate-spin text-accent" />;
    case TaskStatus.Pending:
      return <Circle size={14} className="shrink-0 text-faint" />;
  }
}

/** Collapsible side widget showing the agent's live `task_list` tool state.
 * Renders nothing until the agent has created a list at least once. */
export function TaskListPanel({ tasks }: { tasks: TaskItem[] }) {
  const [collapsed, setCollapsed] = useState(false);
  if (tasks.length === 0) return null;

  const done = tasks.filter((t) => t.status === TaskStatus.Completed).length;

  if (collapsed) {
    return (
      <aside className="flex shrink-0 flex-col items-center gap-1 border-l px-1.5 py-2.5">
        <button
          className="btn-icon flex-col gap-0.5"
          onClick={() => setCollapsed(false)}
          title="Show task list"
          data-testid="task-list-expand"
        >
          <ListTodo size={16} />
          <span className="text-[10px] font-medium leading-none text-muted">
            {done}/{tasks.length}
          </span>
        </button>
      </aside>
    );
  }

  return (
    <aside
      className="flex w-72 shrink-0 flex-col border-l"
      data-testid="task-list-panel"
    >
      <div className="flex items-center gap-2 border-b px-3 py-2.5">
        <ListTodo size={15} className="text-muted" />
        <h2 className="text-sm font-semibold text-text">Tasks</h2>
        <span className="text-xs text-faint" data-testid="task-list-progress">
          {done}/{tasks.length} done
        </span>
        <button
          className="btn-icon ml-auto !h-7 !w-7"
          onClick={() => setCollapsed(true)}
          title="Collapse"
          data-testid="task-list-collapse"
        >
          <ChevronsRight size={15} />
        </button>
      </div>
      <ul className="flex-1 space-y-1 overflow-y-auto px-3 py-2.5">
        {tasks.map((t) => (
          <li
            key={t.id}
            className={cn(
              "flex items-start gap-2 rounded-[var(--radius-sm)] px-1.5 py-1 text-sm",
              t.status === TaskStatus.InProgress && "bg-surface-2",
            )}
            data-testid="task-list-item"
            data-status={t.status}
          >
            <span className="mt-0.5">
              <StatusIcon status={t.status} />
            </span>
            <span
              className={cn(
                "min-w-0 break-words",
                t.status === TaskStatus.Completed
                  ? "text-faint line-through"
                  : "text-text",
              )}
            >
              {t.content}
            </span>
          </li>
        ))}
      </ul>
    </aside>
  );
}
