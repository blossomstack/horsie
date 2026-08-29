import { Circle, CircleCheck } from "lucide-react";
import { TaskStatus, type TaskItem } from "../api/types";
import { cn } from "../lib/cn";
import { SidePanel } from "./SidePanel";
import { useTranslation } from "react-i18next";

function StatusIcon({ status }: { status: TaskStatus }) {
  switch (status) {
    case TaskStatus.Completed:
      return (
        <CircleCheck size={13} className="shrink-0 text-lamp-ok" aria-hidden />
      );
    case TaskStatus.InProgress:
      return <span className="lamp lamp-live mt-1 text-live-ink" aria-hidden />;
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
 * Visibility is not this component's decision — the session header owns it, so
 * the panel is reachable on every session rather than appearing only once the
 * agent happened to use the tool and vanishing again whenever the server
 * offloaded the session.
 */
export function TaskListPanel({
  tasks,
  onClose,
}: {
  tasks: TaskItem[];
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const done = tasks.filter((t) => t.status === TaskStatus.Completed).length;

  return (
    <SidePanel
      legend={t("taskList.legend")}
      readout={
        tasks.length > 0 ? (
          <span className="readout text-[0.6875rem]" data-testid="task-list-progress">
            {t("taskList.progress", { done, total: tasks.length })}
          </span>
        ) : undefined
      }
      onClose={onClose}
      closeLabel={t("taskList.hide")}
      testId="task-list-panel"
      closeTestId="task-list-collapse"
    >
        {tasks.length === 0 ? (
          <p
            className="px-3 py-6 text-center text-xs leading-relaxed text-faint"
            data-testid="task-list-empty"
          >
{t("taskList.empty")}
          </p>
        ) : (
          <ul className="flex-1 space-y-0.5 overflow-y-auto p-2">
            {tasks.map((t) => (
              <li
                key={t.id}
                className={cn(
                  "flex items-start gap-2 rounded-[var(--radius-chip)] px-1.5 py-1.5 text-[0.8125rem] leading-snug",
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
    </SidePanel>
  );
}
