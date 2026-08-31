import { Pencil, Play, Trash2 } from "lucide-react";
import { Link } from "react-router-dom";
import { StatusBadge } from "../../components/StatusBadge";
import { WorkflowGraph } from "../../components/WorkflowGraph";
import { relativeTime } from "../../lib/format";
import { renderFilter } from "./stepDraft";
import { ReadError } from "../../components/ReadError";
import { useWorkflow, useWorkflowRuns } from "../../hooks/useWorkflows";
import { useTranslation } from "react-i18next";

/**
 * A workflow, beside the roster: the graph it will run, and the runs it has had.
 *
 * Running one is not configured here. A run needs a runtime and a workspace,
 * which is exactly what the new-session page already asks for — so `Run` hands
 * the workflow to that page rather than growing a second launch form that
 * would have to learn the same channels.
 */
export function WorkflowDetail({
  name,
  onDelete,
}: {
  name: string;
  onDelete: () => void;
}) {
  const { data: workflow, isLoading, isError } = useWorkflow(name);
  const {
    data: runs,
    isError: runsFailed,
    error: runsError,
  } = useWorkflowRuns(name);
  const { t } = useTranslation();

  if (isLoading)
    return <p className="p-6 text-sm text-faint">{t("common.loading")}</p>;
  if (isError || !workflow) {
    return (
      <p className="p-6 text-sm text-red-ink">{t("workflows.noSuch")}</p>
    );
  }

  const nodes = workflow.steps.map((s) => ({ step: s.name, detail: s.agent }));
  const edges = workflow.steps.flatMap((s) =>
    (s.transitions ?? []).map((t) => ({
      from: s.name,
      to: t.to,
      condition: renderFilter(t.when),
    })),
  );

  return (
    <div className="flex h-full flex-col" data-testid="workflow-detail-page">
      <header className="flex h-[var(--header-h)] shrink-0 items-center gap-2 bar-scroll px-6">
        <h2 className="page-title min-w-0 flex-1 truncate">{workflow.name}</h2>
        <Link
          to={`/workflows/${encodeURIComponent(workflow.name)}/edit`}
          className="key key-sm"
          data-testid="edit-workflow"
        >
          <Pencil size={14} />
          {t("common.edit")}
        </Link>
        <Link
          to={`/?workflow=${encodeURIComponent(workflow.name)}`}
          className="key key-go key-sm"
          data-testid="run-workflow"
        >
          <Play size={14} />
          {t("common.run")}
        </Link>
        <button
          className="key-icon hover:!bg-red-quiet hover:!text-red-ink"
          onClick={onDelete}
          title={t("common.deleteNamed", { name: workflow.name })}
          aria-label={t("common.deleteNamed", { name: workflow.name })}
          data-testid="delete-workflow"
        >
          <Trash2 size={15} aria-hidden />
        </button>
      </header>

      <div className="flex-1 space-y-4 overflow-y-auto px-6 py-4">
        {workflow.description && (
          <p className="max-w-prose text-sm text-dim">{workflow.description}</p>
        )}

        <section className="section">
          <h2 className="legend">{t("workflows.graph")}</h2>
          <div className="mt-3 overflow-auto">
            <WorkflowGraph nodes={nodes} edges={edges} start={workflow.start} />
          </div>
        </section>

        <section className="section">
          <h2 className="legend">{t("routines.runs")}</h2>
          {runsFailed ? (
            <ReadError
              what={t("workflows.runsRead")}
              error={runsError}
              testId="workflow-runs-error"
              className="mt-2"
            />
          ) : (runs ?? []).length === 0 ? (
            <p className="mt-2 text-sm text-faint">{t("workflows.noRuns")}</p>
          ) : (
            <div className="mt-3 space-y-2">
              {(runs ?? []).map((s) => (
                <Link
                  key={s.id}
                  to={`/sessions/${s.id}`}
                  className="flex items-center gap-3 row px-2.5 py-2"
                  data-testid="workflow-run-row"
                >
                  <span className="min-w-0 flex-1 truncate text-sm text-legend">
                    {s.name ?? s.id}
                  </span>
                  <StatusBadge status={s.status} />
                  <span className="shrink-0 text-xs text-faint">
                    {relativeTime(s.createdAt)}
                  </span>
                </Link>
              ))}
            </div>
          )}
        </section>
      </div>
    </div>
  );
}
