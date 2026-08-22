import { ArrowLeft, Pencil, Play } from "lucide-react";
import { Link, useParams } from "react-router-dom";
import { StatusBadge } from "../../components/StatusBadge";
import { WorkflowGraph } from "../../components/WorkflowGraph";
import { relativeTime } from "../../lib/format";
import { renderFilter } from "./stepDraft";
import { RailToggle } from "../../components/rail";
import { ReadError } from "../../components/ReadError";
import { useWorkflow, useWorkflowRuns } from "../../hooks/useWorkflows";

/**
 * A workflow's page: the graph it will run, and the runs it has had.
 *
 * Running one is not configured here. A run needs a runtime and a workspace,
 * which is exactly what the new-session page already asks for — so `Run` hands
 * the workflow to that page rather than growing a second launch form that
 * would have to learn the same channels.
 */
export function WorkflowDetailPage() {
  const { name } = useParams<{ name: string }>();
  const { data: workflow, isLoading, isError } = useWorkflow(name);
  const {
    data: runs,
    isError: runsFailed,
    error: runsError,
  } = useWorkflowRuns(name);

  if (isLoading) return <p className="p-6 text-sm text-faint">Loading…</p>;
  if (isError || !workflow) {
    return <p className="p-6 text-sm text-red-ink">No such workflow.</p>;
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
      <div className="flex h-[var(--header-h)] shrink-0 items-center bar-scroll gap-2 bg-panel px-4 sm:gap-3 sm:px-6">
        <RailToggle />
        <Link
          to="/workflows"
          className="key key-sm"
          data-testid="return-to-workflows"
        >
          <ArrowLeft size={14} />
          Return
        </Link>
        <h1 className="page-title min-w-0 flex-1 truncate">{workflow.name}</h1>
        <Link
          to={`/workflows/${encodeURIComponent(workflow.name)}/edit`}
          className="key ml-auto key-sm"
          data-testid="edit-workflow"
        >
          <Pencil size={14} />
          Edit
        </Link>
        <Link
          to={`/?workflow=${encodeURIComponent(workflow.name)}`}
          className="key key-go key-sm"
          data-testid="run-workflow"
        >
          <Play size={14} />
          Run
        </Link>
      </div>

      <div className="flex-1 space-y-4 overflow-y-auto px-6 py-4">
        {workflow.description && (
          <p className="max-w-prose text-sm text-dim">{workflow.description}</p>
        )}

        <section className="section">
          <h2 className="legend">Graph</h2>
          <p className="mt-1 text-xs text-faint">
            Every step shares one runtime and one workspace.{" "}
            <span className="text-dim">{workflow.start}</span> is handed the
            input the run starts with.
          </p>
          <div className="mt-3 overflow-auto">
            <WorkflowGraph nodes={nodes} edges={edges} start={workflow.start} />
          </div>
        </section>

        <section className="section">
          <h2 className="legend">Runs</h2>
          {runsFailed ? (
            <ReadError
              what="this workflow's runs"
              error={runsError}
              testId="workflow-runs-error"
              className="mt-2"
            />
          ) : (runs ?? []).length === 0 ? (
            <p className="mt-2 text-sm text-faint">No runs yet.</p>
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
