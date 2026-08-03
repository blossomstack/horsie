import { Plus, Trash2 } from "lucide-react";
import { Link, useNavigate } from "react-router-dom";
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
        <h1 className="page-title">Routines</h1>
        <button
          className="key key-go ml-auto !px-2.5 !py-1.5 text-xs"
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
          <p className="text-sm text-red-ink">Can’t reach the server.</p>
        )}
        {routines && routines.length === 0 && (
          <section className="panel p-4" data-testid="routines-empty">
            <h2 className="legend">Routine roster</h2>
            <p className="mt-3 max-w-prose text-sm leading-relaxed text-dim">
              A routine runs an agent against a fixed prompt — on a timer, from
              the API, or whenever you press run. Its sessions live on its own
              page rather than in the rail. Press{" "}
              <span className="text-legend">New routine</span> to define one.
            </p>
          </section>
        )}
        <div className="space-y-2">
          {(routines ?? []).map((r) => (
            <div
              key={r.name}
              className="flex items-center gap-3 rounded-[var(--radius-control)] border px-4 py-3"
              data-testid="routine-row"
              data-routine-name={r.name}
            >
              <Link
                to={`/routines/${encodeURIComponent(r.name)}`}
                className="min-w-0 flex-1"
              >
                <div className="flex items-baseline gap-2">
                  <span className="font-mono text-sm font-medium text-legend">
                    {r.name}
                  </span>
                  <span className="text-xs text-faint">
                    {r.agent} · {scheduleLine(r)}
                  </span>
                </div>
                {r.description && (
                  <div className="truncate text-sm text-dim">
                    {r.description}
                  </div>
                )}
                <div className="mt-1 flex gap-2 text-[11px] text-faint">
                  {r.lastRunAtMs !== undefined && (
                    <span>ran {relativeTime(r.lastRunAtMs)}</span>
                  )}
                  {r.lastError && (
                    <span className="text-red-ink">{r.lastError}</span>
                  )}
                </div>
              </Link>
              <button
                className="rounded-[var(--radius-chip)] p-1.5 text-faint hover:bg-raised hover:text-red-ink"
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
