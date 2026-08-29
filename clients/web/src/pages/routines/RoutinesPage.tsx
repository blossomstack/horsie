import { useScrolledUnder } from "../../hooks/useScrolledUnder";
import { Plus } from "lucide-react";
import { Trans, useTranslation } from "react-i18next";
import { RosterRow } from "../../components/RosterRow";
import { useNavigate } from "react-router-dom";
import type { RoutineView } from "../../api/types";
import { relativeTime } from "../../lib/format";
import { askConfirm } from "../../lib/confirm";
import { RailToggle } from "../../components/rail";
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
  const { t } = useTranslation();
  const { onScroll, barProps } = useScrolledUnder();
  const { data: routines, isLoading, isError } = useRoutines();
  const del = useDeleteRoutine();
  const navigate = useNavigate();

  return (
    <div className="flex h-full flex-col" data-testid="routines-page">
      <div {...barProps}
        className="flex h-[var(--header-h)] shrink-0 items-center bar-scroll gap-2 bg-panel px-4 sm:gap-3 sm:px-6">
        <RailToggle />
        <h1 className="page-title">{t("nav.routines")}</h1>
        <button
          className="key key-go ml-auto key-sm"
          onClick={() => navigate("/routines/new")}
          data-testid="new-routine-button"
        >
          <Plus size={15} />
          {t("routines.new")}
        </button>
      </div>
      <div className="flex-1 overflow-y-auto px-6 py-4" onScroll={onScroll}>
        {isLoading && <p className="text-sm text-faint">{t("common.loading")}</p>}
        {isError && (
          <p className="text-sm text-red-ink">{t("common.unreachableShort")}</p>
        )}
        {routines && routines.length === 0 && (
          <section className="section" data-testid="routines-empty">
            <h2 className="legend">{t("routines.rosterTitle")}</h2>
            <p className="mt-3 max-w-prose text-sm leading-relaxed text-dim">
              <Trans
                i18nKey="routines.rosterBlurb"
                components={{ key: <span className="text-legend" /> }}
              />
            </p>
          </section>
        )}
        <div className="list-divided">
          {(routines ?? []).map((r) => (
            <RosterRow
              key={r.name}
              to={`/routines/${encodeURIComponent(r.name)}`}
              name={r.name}
              meta={`${r.agent} · ${scheduleLine(r)}`}
              description={r.description}
              facts={
                <>
                  {r.lastRunAtMs !== undefined && (
                    <span className="legend">ran {relativeTime(r.lastRunAtMs)}</span>
                  )}
                  {r.lastError && (
                    <span className="legend !text-red-ink">{r.lastError}</span>
                  )}
                </>
              }
              testId="routine-row"
              nameAttr={{ "data-routine-name": r.name }}
              deleteLabel={`Delete ${r.name}`}
              deleteTestId={`delete-routine-${r.name}`}
              onDelete={async () => {
                if (
                  await askConfirm(
                    t("routines.confirmDelete", { name: r.name }),
                  )
                )
                  del.mutate(r.name);
              }}
            />
          ))}
        </div>
      </div>
    </div>
  );
}
