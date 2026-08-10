import { Plus, Trash2 } from "lucide-react";
import { Link, useNavigate } from "react-router-dom";
import { relativeTime } from "../../lib/format";
import { askConfirm } from "../../lib/confirm";
import { RailToggle } from "../../components/rail";
import { useDeleteWorkflow, useWorkflows } from "../../hooks/useWorkflows";

export function WorkflowsPage() {
  const { data: workflows, isLoading, isError } = useWorkflows();
  const del = useDeleteWorkflow();
  const navigate = useNavigate();

  return (
    <div className="flex h-full flex-col" data-testid="workflows-page">
      <div className="flex items-center gap-3 border-b px-6 py-4">
        <RailToggle />
        <h1 className="page-title">Workflows</h1>
        <button
          className="key key-go ml-auto !px-2.5 !py-1.5 text-xs"
          onClick={() => navigate("/workflows/new")}
          data-testid="new-workflow-button"
        >
          <Plus size={15} />
          New workflow
        </button>
      </div>
      <div className="flex-1 overflow-y-auto px-6 py-4">
        {isLoading && <p className="text-sm text-faint">Loading…</p>}
        {isError && <p className="text-sm text-red-ink">Can’t reach the server.</p>}
        {workflows && workflows.length === 0 && (
          <section className="panel p-4" data-testid="workflows-empty">
            <h2 className="legend">Workflow roster</h2>
            <p className="mt-3 max-w-prose text-sm leading-relaxed text-dim">
              A workflow runs several agents in order, each one deciding where
              the next goes. Every step shares one workspace, so what one writes
              the next one reads. Runs appear in the rail alongside your
              sessions. Press{" "}
              <span className="text-legend">New workflow</span> to define one.
            </p>
          </section>
        )}
        <div className="space-y-2">
          {(workflows ?? []).map((w) => (
            <div
              key={w.name}
              className="flex items-center gap-3 rounded-[var(--radius-control)] border px-4 py-3"
              data-testid="workflow-row"
              data-workflow-name={w.name}
            >
              <Link
                to={`/workflows/${encodeURIComponent(w.name)}`}
                className="min-w-0 flex-1"
              >
                <span className="item-title">{w.name}</span>
                <span className="mt-0.5 block truncate text-xs text-faint">
                  {w.steps.length} step{w.steps.length === 1 ? "" : "s"} · starts at{" "}
                  <span className="text-dim">{w.start}</span>
                  {w.description ? ` · ${w.description}` : ""}
                </span>
              </Link>
              <span className="shrink-0 text-xs text-faint">
                {relativeTime(Number(w.updatedAt) * 1000)}
              </span>
              <button
                className="key key-danger !px-2 !py-1"
                title={`Delete ${w.name}`}
                aria-label={`Delete ${w.name}`}
                data-testid="delete-workflow"
                onClick={async () => {
                  // Runs are sessions in their own right and survive this, each
                  // carrying the graph it started with.
                  if (
                    await askConfirm(
                      `Delete workflow "${w.name}"? Its runs stay in the session rail.`,
                    )
                  ) {
                    del.mutate(w.name);
                  }
                }}
              >
                <Trash2 size={14} />
              </button>
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
