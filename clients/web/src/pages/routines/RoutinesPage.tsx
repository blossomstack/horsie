import { Plus } from "lucide-react";
import { Trans, useTranslation } from "react-i18next";
import { useNavigate, useParams } from "react-router-dom";
import type { RoutineView } from "../../api/types";
import { ListDetail, NothingSelected } from "../../components/ListDetail";
import { RosterRow } from "../../components/RosterRow";
import { relativeTime } from "../../lib/format";
import { askConfirm } from "../../lib/confirm";
import { describeSchedule } from "../../lib/schedule";
import { useDeleteRoutine, useRoutines } from "../../hooks/useRoutines";
import { RoutineDetail } from "./RoutineDetail";
import { RoutineEditPage } from "./RoutineEditPage";

/**
 * The roster, and beside it whichever routine is selected — read, or being
 * edited. `editing` is set by the `new` and `:name/edit` routes: the form is
 * the same width as the readout it replaces, so choosing another routine is
 * still one click away while you fill it in.
 */
export function RoutinesPage({ editing }: { editing?: boolean }) {
  const { t } = useTranslation();
  const { name } = useParams<{ name: string }>();
  const { data: routines, isLoading, isError } = useRoutines();
  const del = useDeleteRoutine();
  const navigate = useNavigate();

  /** What the routine's timer is doing, in one phrase. */
  const scheduleLine = (r: RoutineView): string => {
    const shape = describeSchedule(r.schedule);
    if (!r.enabled) return `${shape} · paused`;
    if (r.nextRunAtMs === undefined) return shape;
    return `${shape} · next ${relativeTime(r.nextRunAtMs)}`;
  };

  const remove = async (routine: string) => {
    if (!(await askConfirm(t("routines.confirmDelete", { name: routine })))) return;
    del.mutate(routine);
    if (routine === name) navigate("/routines");
  };

  return (
    <ListDetail
      testId="routines-page"
      title={t("nav.routines")}
      action={
        <button
          className="key key-go key-sm shrink-0"
          onClick={() => navigate("/routines/new")}
          data-testid="new-routine-button"
        >
          <Plus size={13} aria-hidden />
          {t("routines.new")}
        </button>
      }
      detail={
        editing ? (
          <RoutineEditPage />
        ) : name ? (
          <RoutineDetail name={name} onDelete={() => void remove(name)} />
        ) : (
          <NothingSelected>{t("routines.pickOne")}</NothingSelected>
        )
      }
    >
      {isLoading && (
        <p className="px-2.5 py-6 text-sm text-faint">{t("common.loading")}</p>
      )}
      {isError && (
        <p className="px-2.5 py-6 text-sm text-red-ink">
          {t("common.unreachableShort")}
        </p>
      )}
      {routines && routines.length === 0 && (
        <section className="section m-1" data-testid="routines-empty">
          <h2 className="legend">{t("routines.rosterTitle")}</h2>
          <p className="mt-3 text-sm leading-relaxed text-dim">
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
            description={r.description}
            selected={r.name === name}
            meta={scheduleLine(r)}
            testId="routine-row"
            nameAttr={{ "data-routine-name": r.name }}
            deleteLabel={t("common.deleteNamed", { name: r.name })}
            deleteTestId={`delete-routine-${r.name}`}
            onDelete={() => void remove(r.name)}
          />
        ))}
      </div>
    </ListDetail>
  );
}
