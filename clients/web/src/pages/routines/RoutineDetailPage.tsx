import { CalendarClock, Pencil, Play } from "lucide-react";
import { Link, useParams } from "react-router-dom";
import { EmptyState } from "../../components/EmptyState";
import { StatusDot } from "../../components/StatusBadge";
import { ApiRequestError } from "../../api/client";
import { useState } from "react";
import { absoluteTime, relativeTime, sessionTitle } from "../../lib/format";
import { describeSchedule } from "../../lib/schedule";
import {
  useRoutine,
  useRoutineSessions,
  useRunRoutine,
} from "../../hooks/useRoutines";

export function RoutineDetailPage() {
  const { name } = useParams<{ name: string }>();
  const { data: routine, isLoading, isError } = useRoutine(name);
  const { data: runs } = useRoutineSessions(name);
  const run = useRunRoutine();
  const [error, setError] = useState<string | null>(null);

  if (isLoading) {
    return <p className="px-6 py-4 text-sm text-faint">Loading…</p>;
  }
  if (isError || !routine) {
    return (
      <p className="px-6 py-4 text-sm text-error">No such routine: {name}.</p>
    );
  }

  const handleRun = async () => {
    setError(null);
    try {
      await run.mutateAsync(routine.name);
    } catch (e) {
      setError(e instanceof ApiRequestError ? e.message : "Failed to run.");
    }
  };

  return (
    <div className="flex h-full flex-col" data-testid="routine-detail-page">
      <div className="flex items-center gap-3 border-b px-6 py-4">
        <h1 className="font-mono text-[15px] font-semibold text-text">
          {routine.name}
        </h1>
        {!routine.enabled && (
          <span className="rounded-full border px-2 py-0.5 text-[11px] text-faint">
            paused
          </span>
        )}
        <Link
          to={`/routines/${encodeURIComponent(routine.name)}/edit`}
          className="btn-outline ml-auto !px-2.5 !py-1.5 text-xs"
          data-testid="edit-routine-link"
        >
          <Pencil size={15} />
          Edit
        </Link>
        <button
          className="btn-primary !px-2.5 !py-1.5 text-xs"
          onClick={handleRun}
          disabled={run.isPending}
          data-testid="run-routine-button"
        >
          <Play size={15} />
          {run.isPending ? "Starting…" : "Run now"}
        </button>
      </div>

      <div className="flex-1 overflow-y-auto px-6 py-4">
        <div className="mx-auto w-full max-w-3xl space-y-5">
          {routine.description && (
            <p className="text-sm text-muted">{routine.description}</p>
          )}

          <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1.5 text-sm">
            <dt className="text-faint">Agent</dt>
            <dd className="text-text">
              <Link
                className="hover:underline"
                to={`/agents/${encodeURIComponent(routine.agent)}/edit`}
              >
                {routine.agent}
              </Link>
            </dd>
            <dt className="text-faint">Runs</dt>
            <dd className="text-text">{describeSchedule(routine.schedule)}</dd>
            <dt className="text-faint">Next</dt>
            <dd className="text-text">
              {routine.nextRunAtMs === undefined ? (
                <span className="text-faint">not scheduled</span>
              ) : (
                <span title={absoluteTime(routine.nextRunAtMs)}>
                  {relativeTime(routine.nextRunAtMs)}
                </span>
              )}
            </dd>
          </dl>

          <div>
            <div className="mb-1 text-xs font-medium text-muted">Prompt</div>
            <pre className="whitespace-pre-wrap rounded-[var(--radius)] border bg-surface-2 px-3 py-2 font-mono text-xs text-text">
              {routine.prompt}
            </pre>
          </div>

          {(error ?? routine.lastError) && (
            <div
              className="rounded-[var(--radius)] border border-error/40 bg-error-soft px-3 py-2 text-sm text-error"
              data-testid="routine-run-error"
            >
              {error ?? `Last trigger failed: ${routine.lastError}`}
            </div>
          )}

          <div>
            <div className="mb-2 text-xs font-medium uppercase tracking-wide text-faint">
              Runs
            </div>
            {runs && runs.length === 0 && (
              <EmptyState icon={<CalendarClock size={24} />} title="No runs yet">
                Runs appear here, not in the sidebar. Each one works from the
                prompt alone — it has no way to ask you a question.
              </EmptyState>
            )}
            <div className="space-y-2">
              {(runs ?? []).map((s) => (
                <Link
                  key={s.id}
                  to={`/sessions/${s.id}`}
                  className="flex items-center gap-2.5 rounded-[var(--radius)] border px-4 py-3 hover:bg-surface-2"
                  data-testid="routine-run-row"
                  data-session-id={s.id}
                >
                  <StatusDot status={s.status} />
                  <div className="min-w-0 flex-1">
                    <div className="truncate text-sm font-medium text-text">
                      {sessionTitle(s.name)}
                    </div>
                    <div
                      className="truncate text-xs text-faint"
                      title={absoluteTime(s.createdAt)}
                    >
                      {relativeTime(s.createdAt)}
                      {s.lastError ? ` · ${s.lastError}` : ""}
                    </div>
                  </div>
                </Link>
              ))}
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
