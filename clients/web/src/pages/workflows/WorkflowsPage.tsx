import { Plus } from "lucide-react";
import { Trans, useTranslation } from "react-i18next";
import { useNavigate, useParams } from "react-router-dom";
import { ListDetail, NothingSelected } from "../../components/ListDetail";
import { RosterRow } from "../../components/RosterRow";
import { askConfirm } from "../../lib/confirm";
import { useDeleteWorkflow, useWorkflows } from "../../hooks/useWorkflows";
import { WorkflowDetail } from "./WorkflowDetail";

export function WorkflowsPage() {
  const { t } = useTranslation();
  const { name } = useParams<{ name: string }>();
  const { data: workflows, isLoading, isError } = useWorkflows();
  const del = useDeleteWorkflow();
  const navigate = useNavigate();

  const remove = async (workflow: string) => {
    if (!(await askConfirm(t("workflows.confirmDelete", { name: workflow })))) return;
    del.mutate(workflow);
    if (workflow === name) navigate("/workflows");
  };

  return (
    <ListDetail
      testId="workflows-page"
      title={t("nav.workflows")}
      action={
        <button
          className="key key-go key-sm shrink-0"
          onClick={() => navigate("/workflows/new")}
          data-testid="new-workflow-button"
        >
          <Plus size={13} aria-hidden />
          {t("workflows.new")}
        </button>
      }
      detail={
        name ? (
          <WorkflowDetail name={name} onDelete={() => void remove(name)} />
        ) : (
          <NothingSelected>{t("workflows.pickOne")}</NothingSelected>
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
      {workflows && workflows.length === 0 && (
        <section className="section m-1" data-testid="workflows-empty">
          <h2 className="legend">{t("workflows.rosterTitle")}</h2>
          <p className="mt-3 text-sm leading-relaxed text-dim">
            <Trans
              i18nKey="workflows.rosterBlurb"
              components={{ key: <span className="text-legend" /> }}
            />
          </p>
        </section>
      )}
      <div className="list-divided">
        {(workflows ?? []).map((w) => (
          <RosterRow
            key={w.name}
            to={`/workflows/${encodeURIComponent(w.name)}`}
            name={w.name}
            description={w.description}
            selected={w.name === name}
            testId="workflow-row"
            nameAttr={{ "data-workflow-name": w.name }}
            deleteLabel={t("common.deleteNamed", { name: w.name })}
            deleteTestId={`delete-workflow-${w.name}`}
            onDelete={() => void remove(w.name)}
          />
        ))}
      </div>
    </ListDetail>
  );
}
