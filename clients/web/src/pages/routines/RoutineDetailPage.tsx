import { ArrowLeft, Pencil, Play } from "lucide-react";
import { targetOf } from "../../lib/routineTarget";
import { Link, useParams } from "react-router-dom";
import { StatusDot } from "../../components/StatusBadge";
import { ApiRequestError } from "../../api/client";
import { useState } from "react";
import { absoluteTime, relativeTime, sessionTitle } from "../../lib/format";
import { RailToggle } from "../../components/rail";
import { ReadError } from "../../components/ReadError";
import { describeSchedule } from "../../lib/schedule";
import {
  useRoutine,
  useRoutineSessions,
  useRunRoutine,
} from "../../hooks/useRoutines";
import { useTranslation } from "react-i18next";

export function RoutineDetailPage() {
  const { name } = useParams<{ name: string }>();
  const { data: routine, isLoading, isError } = useRoutine(name);
  const {
    data: runs,
    isError: runsFailed,
    error: runsError,
  } = useRoutineSessions(name);
  const run = useRunRoutine();
  const [error, setError] = useState<string | null>(null);
  const { t } = useTranslation();

  if (isLoading) {
    return <p className="px-6 py-4 text-sm text-faint">{t("common.loading")}</p>;
  }
  if (isError || !routine) {
    return (
      <p className="px-6 py-4 text-sm text-red-ink">
        {t("routines.noSuch", { name })}
      </p>
    );
  }

  const handleRun = async () => {
    setError(null);
    try {
      await run.mutateAsync(routine.name);
    } catch (e) {
      setError(e instanceof ApiRequestError ? e.message : t("routines.runFailed"));
    }
  };

  const target = targetOf(routine.target);
  return (
    <div className="flex h-full flex-col" data-testid="routine-detail-page">
      <div className="flex h-[var(--header-h)] shrink-0 items-center bar-scroll gap-2 bg-panel px-4 sm:gap-3 sm:px-6">
        <RailToggle />
        <Link
          to="/routines"
          className="key key-sm"
          data-testid="return-to-routines"
        >
          <ArrowLeft size={15} />
          {t("common.return")}
        </Link>
        <h1 className="page-title min-w-0 flex-1 truncate">{routine.name}</h1>
        {!routine.enabled && (
          <span className="rounded-full px-2 py-0.5 text-[0.6875rem] text-faint">
            {t("routines.paused")}
          </span>
        )}
        <Link
          to={`/routines/${encodeURIComponent(routine.name)}/edit`}
          className="key ml-auto key-sm"
          data-testid="edit-routine-link"
        >
          <Pencil size={15} />
          {t("common.edit")}
        </Link>
        <button
          className="key key-go key-sm"
          onClick={handleRun}
          disabled={run.isPending}
          data-testid="run-routine-button"
        >
          <Play size={15} />
          {run.isPending ? t("routines.starting") : t("routines.runNow")}
        </button>
      </div>

      <div className="flex-1 overflow-y-auto px-6 py-4">
        <div className="mx-auto w-full max-w-3xl space-y-5">
          {routine.description && (
            <p className="text-sm text-dim">{routine.description}</p>
          )}

          <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1.5 text-sm">
            <dt className="text-faint">
              {t(
                target.kind === "agent" ? "routines.agent" : "channel.workflow",
              )}
            </dt>
            <dd className="text-legend">
              <Link className="hover:underline" to={target.to}>
                {target.name}
              </Link>
            </dd>
            <dt className="text-faint">{t("channel.environment")}</dt>
            <dd className="text-legend">
              {routine.environment.type === "Named" ? (
                <Link
                  className="hover:underline"
                  to={`/environments/${encodeURIComponent(routine.environment.value.name)}/edit`}
                >
                  {routine.environment.value.name}
                </Link>
              ) : routine.environment.type === "None" ? (
                <span className="text-faint">{t("routines.noRuntime")}</span>
              ) : (
                <>
                  <span className="font-mono">{routine.environment.value.vendor}</span>
                  {(routine.environment.value.repos?.length ?? 0) > 0 && (
                    <span className="text-faint">
                      {" · "}
                      {t("environment.repoCount", {
                        count: routine.environment.value.repos?.length ?? 0,
                      })}
                    </span>
                  )}
                </>
              )}
            </dd>
            <dt className="text-faint">{t("routines.runs")}</dt>
            <dd className="text-legend">{describeSchedule(routine.schedule)}</dd>
            <dt className="text-faint">{t("routines.next")}</dt>
            <dd className="text-legend">
              {routine.nextRunAtMs === undefined ? (
                <span className="text-faint">{t("routines.notScheduled")}</span>
              ) : (
                <span title={absoluteTime(routine.nextRunAtMs)}>
                  {relativeTime(routine.nextRunAtMs)}
                </span>
              )}
            </dd>
          </dl>

          <div>
            <div className="mb-1 text-xs font-medium text-dim">{t("routines.prompt")}</div>
            <pre className="whitespace-pre-wrap rounded-[var(--radius-control)] bg-raised px-3 py-2 font-mono text-xs text-legend">
              {routine.prompt}
            </pre>
          </div>

          {(error ?? routine.lastError) && (
            <div
              className="rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2 text-sm text-red-ink"
              data-testid="routine-run-error"
            >
              {error ?? t("routines.lastTriggerFailed", { error: routine.lastError })}
            </div>
          )}

          <div>
            <div className="legend mb-2">{t("routines.runs")}</div>
            {runsFailed && (
              <ReadError
                what={t("routines.runsRead")}
                error={runsError}
                testId="routine-runs-error"
              />
            )}
            {runs && runs.length === 0 && (
              <p className="screen px-3 py-5 text-center text-sm leading-relaxed text-faint">
{t("routines.noRuns")}
              </p>
            )}
            <div className="space-y-px">
              {(runs ?? []).map((s) => (
                <Link
                  key={s.id}
                  to={`/sessions/${s.id}`}
                  className="flex items-center gap-2.5 row px-2.5 py-2"
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
