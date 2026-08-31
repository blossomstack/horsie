import { Plus } from "lucide-react";
import { Trans, useTranslation } from "react-i18next";
import { useNavigate, useParams } from "react-router-dom";
import { ListDetail, NothingSelected } from "../../components/ListDetail";
import { RosterRow } from "../../components/RosterRow";
import { askConfirm } from "../../lib/confirm";
import { useDeleteEnvironment, useEnvironments } from "../../hooks/useEnvironments";
import { EnvironmentDetail } from "./EnvironmentDetail";
import { EnvironmentEditPage } from "./EnvironmentEditPage";

/**
 * The roster, and beside it whichever environment is selected — read, or being
 * edited. `editing` is set by the `new` and `:name/edit` routes: the form is
 * the same width as the readout it replaces, so choosing another environment is
 * still one click away while you fill it in.
 */
export function EnvironmentsPage({ editing }: { editing?: boolean }) {
  const { t } = useTranslation();
  const { name } = useParams<{ name: string }>();
  const { data: environments, isLoading, isError } = useEnvironments();
  const del = useDeleteEnvironment();
  const navigate = useNavigate();

  const selected = environments?.find((e) => e.name === name);

  const remove = async (env: string) => {
    if (!(await askConfirm(t("environments.confirmDelete", { name: env })))) return;
    del.mutate(env);
    if (env === name) navigate("/environments");
  };

  return (
    <ListDetail
      testId="environments-page"
      title={t("nav.environments")}
      action={
        <button
          className="key key-go key-sm shrink-0"
          onClick={() => navigate("/environments/new")}
          data-testid="new-environment-button"
        >
          <Plus size={13} aria-hidden />
          {t("environments.new")}
        </button>
      }
      detail={
        editing ? (
          <EnvironmentEditPage />
        ) : selected ? (
          <EnvironmentDetail
            environment={selected}
            onDelete={() => void remove(selected.name)}
          />
        ) : (
          <NothingSelected>{t("environments.pickOne")}</NothingSelected>
        )
      }
    >
      {isLoading && (
        <div className="flex items-center gap-2 px-2.5 py-6">
          <span className="lamp lamp-live text-live-ink" aria-hidden />
          <span className="legend">{t("environments.loading")}</span>
        </div>
      )}
      {isError && (
        <p className="px-2.5 py-6 text-sm text-red-ink">{t("rail.unreachable")}</p>
      )}
      {environments && environments.length === 0 && (
        <section className="section m-1" data-testid="environments-empty">
          <h2 className="legend">{t("environments.rosterTitle")}</h2>
          <p className="mt-3 text-sm leading-relaxed text-dim">
            <Trans
              i18nKey="environments.rosterBlurb"
              components={{ key: <span className="text-legend" /> }}
            />
          </p>
        </section>
      )}
      <div className="list-divided">
        {(environments ?? []).map((e) => (
          <RosterRow
            key={e.name}
            to={`/environments/${encodeURIComponent(e.name)}`}
            name={e.name}
            description={e.description}
            selected={e.name === name}
            testId="environment-row"
            nameAttr={{ "data-environment-name": e.name }}
            deleteLabel={t("common.deleteNamed", { name: e.name })}
            deleteTestId={`delete-environment-${e.name}`}
            onDelete={() => void remove(e.name)}
          />
        ))}
      </div>
    </ListDetail>
  );
}
