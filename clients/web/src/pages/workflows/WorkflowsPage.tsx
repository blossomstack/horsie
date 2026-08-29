import { useScrolledUnder } from "../../hooks/useScrolledUnder";
import { Plus } from "lucide-react";
import { Trans, useTranslation } from "react-i18next";
import { RosterRow } from "../../components/RosterRow";
import { useNavigate } from "react-router-dom";
import { relativeTime } from "../../lib/format";
import { askConfirm } from "../../lib/confirm";
import { RailToggle } from "../../components/rail";
import { useDeleteWorkflow, useWorkflows } from "../../hooks/useWorkflows";

export function WorkflowsPage() {
  const { t } = useTranslation();
  const { onScroll, barProps } = useScrolledUnder();
  const { data: workflows, isLoading, isError } = useWorkflows();
  const del = useDeleteWorkflow();
  const navigate = useNavigate();

  return (
    <div className="flex h-full flex-col" data-testid="workflows-page">
      <div {...barProps}
        className="flex h-[var(--header-h)] shrink-0 items-center bar-scroll gap-2 bg-panel px-4 sm:gap-3 sm:px-6">
        <RailToggle />
        <h1 className="page-title">{t("nav.workflows")}</h1>
        <button
          className="key key-go ml-auto key-sm"
          onClick={() => navigate("/workflows/new")}
          data-testid="new-workflow-button"
        >
          <Plus size={15} />
          {t("workflows.new")}
        </button>
      </div>
      <div className="flex-1 overflow-y-auto px-6 py-4" onScroll={onScroll}>
        {isLoading && <p className="text-sm text-faint">{t("common.loading")}</p>}
        {isError && (
          <p className="text-sm text-red-ink">{t("common.unreachableShort")}</p>
        )}
        {workflows && workflows.length === 0 && (
          <section className="section" data-testid="workflows-empty">
            <h2 className="legend">{t("workflows.rosterTitle")}</h2>
            <p className="mt-3 max-w-prose text-sm leading-relaxed text-dim">
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
              meta={t("workflows.rowMeta", {
                count: w.steps.length,
                start: w.start,
              })}
              description={w.description}
              aside={relativeTime(Number(w.updatedAt) * 1000)}
              testId="workflow-row"
              nameAttr={{ "data-workflow-name": w.name }}
              deleteLabel={t("common.deleteNamed", { name: w.name })}
              deleteTestId="delete-workflow"
              onDelete={async () => {
                // Runs are sessions in their own right and survive this, each
                // carrying the graph it started with.
                if (
                  await askConfirm(
                    t("workflows.confirmDelete", { name: w.name }),
                  )
                ) {
                  del.mutate(w.name);
                }
              }}
            />
          ))}
        </div>
      </div>
    </div>
  );
}
