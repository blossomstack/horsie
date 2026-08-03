import { CalendarClock, Plus, Trash2 } from "lucide-react";
import { Link, useNavigate } from "react-router-dom";
import { EmptyState } from "../../components/EmptyState";
import type { RoutineView } from "../../api/types";
import { relativeTime } from "../../lib/format";
import { describeSchedule } from "../../lib/schedule";
import { useDeleteRoutine, useRoutines } from "../../hooks/useRoutines";

/** What the routine's timer is doing, in one phrase. */
function scheduleLine(r: RoutineView): string {
  const shape = describeSchedule(r.schedule);
  if (!r.enabled) return `${shape} · paused`;
  if (r.nextRunAtMs === undefined) return shape;
  return `${shape} · next ${relativeTime(r.nextRunAtMs)}`;
}

export function RoutinesPage() {
  const { data: routines, isLoading, isError } = useRoutines();
  const del = useDeleteRoutine();
  const navigate = useNavigate();

  return (
    <div className="flex h-full flex-col" data-testid="routines-page">
      <div className="flex items-center gap-3 border-b px-6 py-4">
        <h1 className="text-[15px] font-semibold text-text">Routines</h1>
        <button
          className="btn-primary ml-auto !px-2.5 !py-1.5 text-xs"
          onClick={() => navigate("/routines/new")}
          data-testid="new-routine-button"
        >
          <Plus size={15} />
          New routine
        </button>
      </div>
      <div className="flex-1 overflow-y-auto px-6 py-4">
        {isLoading && <p className="text-sm text-faint">Loading…</p>}
        {isError && (
          <p className="text-sm text-error">Can’t reach the server.</p>
        )}
        {routines && routines.length === 0 && (
          <EmptyState icon={<CalendarClock size={24} />} title="No routines yet">
            A routine runs an agent against a fixed prompt — on a timer, from
            the API, or whenever you press run. Its sessions live on its own
            page rather than in the sidebar.
          </EmptyState>
        )}
        <div className="space-y-2">
          {(routines ?? []).map((r) => (
            <div
              key={r.name}
              className="flex items-center gap-3 rounded-[var(--radius)] border px-4 py-3"
              data-testid="routine-row"
              data-routine-name={r.name}
            >
              <Link
                to={`/routines/${encodeURIComponent(r.name)}`}
                className="min-w-0 flex-1"
              >
                <div className="flex items-baseline gap-2">
                  <span className="font-mono text-sm font-medium text-text">
                    {r.name}
                  </span>
                  <span className="text-xs text-faint">
                    {r.agent} · {scheduleLine(r)}
                  </span>
                </div>
                {r.description && (
                  <div className="truncate text-sm text-muted">
                    {r.description}
                  </div>
                )}
                <div className="mt-1 flex gap-2 text-[11px] text-faint">
                  {r.lastRunAtMs !== undefined && (
                    <span>ran {relativeTime(r.lastRunAtMs)}</span>
                  )}
                  {r.lastError && (
                    <span className="text-error">{r.lastError}</span>
                  )}
                </div>
              </Link>
              <button
                className="rounded-[var(--radius-sm)] p-1.5 text-faint hover:bg-surface-2 hover:text-error"
                title={`Delete ${r.name}`}
                data-testid={`delete-routine-${r.name}`}
                onClick={() => {
                  if (
                    window.confirm(
                      `Delete routine '${r.name}' and every session it created?`,
                    )
                  )
                    del.mutate(r.name);
                }}
              >
                <Trash2 size={15} />
              </button>
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
