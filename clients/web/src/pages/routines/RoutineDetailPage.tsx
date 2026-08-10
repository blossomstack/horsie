import { Pencil, Play } from "lucide-react";
import { Link, useParams } from "react-router-dom";
import { StatusDot } from "../../components/StatusBadge";
import { ApiRequestError } from "../../api/client";
import { useState } from "react";
import { absoluteTime, relativeTime, sessionTitle } from "../../lib/format";
import { RailToggle } from "../../components/rail";
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
      <p className="px-6 py-4 text-sm text-red-ink">No such routine: {name}.</p>
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
        <RailToggle />
        <h1 className="page-title">
          {routine.name}
        </h1>
        {!routine.enabled && (
          <span className="rounded-full border px-2 py-0.5 text-[0.6875rem] text-faint">
            paused
          </span>
        )}
        <Link
          to={`/routines/${encodeURIComponent(routine.name)}/edit`}
          className="key ml-auto !px-2.5 !py-1.5 text-xs"
          data-testid="edit-routine-link"
        >
          <Pencil size={15} />
          Edit
        </Link>
        <button
          className="key key-go !px-2.5 !py-1.5 text-xs"
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
            <p className="text-sm text-dim">{routine.description}</p>
          )}

          <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1.5 text-sm">
            <dt className="text-faint">Agent</dt>
            <dd className="text-legend">
              <Link
                className="hover:underline"
                to={`/agents/${encodeURIComponent(routine.agent)}/edit`}
              >
                {routine.agent}
              </Link>
            </dd>
            <dt className="text-faint">Environment</dt>
            <dd className="text-legend">
              {routine.environment.type === "Named" ? (
                <Link
                  className="hover:underline"
                  to={`/environments/${encodeURIComponent(routine.environment.value.name)}/edit`}
                >
                  {routine.environment.value.name}
                </Link>
              ) : (
                <>
                  <span className="font-mono">{routine.environment.value.vendor}</span>
                  {(routine.environment.value.repos?.length ?? 0) > 0 && (
                    <span className="text-faint">
                      {" · "}
                      {routine.environment.value.repos?.length} repo
                      {routine.environment.value.repos?.length === 1 ? "" : "s"}
                    </span>
                  )}
                </>
              )}
            </dd>
            <dt className="text-faint">Runs</dt>
            <dd className="text-legend">{describeSchedule(routine.schedule)}</dd>
            <dt className="text-faint">Next</dt>
            <dd className="text-legend">
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
            <div className="mb-1 text-xs font-medium text-dim">Prompt</div>
            <pre className="whitespace-pre-wrap rounded-[var(--radius-control)] border bg-raised px-3 py-2 font-mono text-xs text-legend">
              {routine.prompt}
            </pre>
          </div>

          {(error ?? routine.lastError) && (
            <div
              className="rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2 text-sm text-red-ink"
              data-testid="routine-run-error"
            >
              {error ?? `Last trigger failed: ${routine.lastError}`}
            </div>
          )}

          <div>
            <div className="legend mb-2">
              Runs
            </div>
            {runs && runs.length === 0 && (
              <p className="screen px-3 py-5 text-center text-sm leading-relaxed text-faint">
                No runs yet. Runs appear here rather than in the rail, and each
                works from the prompt alone — it has no way to ask you a
                question.
              </p>
            )}
            <div className="space-y-2">
              {(runs ?? []).map((s) => (
                <Link
                  key={s.id}
                  to={`/sessions/${s.id}`}
                  className="flex items-center gap-2.5 rounded-[var(--radius-control)] border px-4 py-3 hover:bg-raised"
                  data-testid="routine-run-row"
                  data-session-id={s.id}
                >
                  <StatusDot status={s.status} />
                  <div className="min-w-0 flex-1">
                    <div className="truncate text-sm font-medium text-legend">
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
