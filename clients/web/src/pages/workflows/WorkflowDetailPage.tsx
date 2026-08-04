import { Pencil, Play } from "lucide-react";
import { useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { StatusBadge } from "../../components/StatusBadge";
import { WorkflowGraph } from "../../components/WorkflowGraph";
import { relativeTime } from "../../lib/format";
import { useRunWorkflow, useWorkflow, useWorkflowRuns } from "../../hooks/useWorkflows";

/**
 * A workflow's page: the graph it will run, and the runs it has had.
 *
 * Starting one asks for the two things a run needs and a definition
 * deliberately does not hold — where it runs and what it runs against — plus
 * the input its first step is handed.
 */
export function WorkflowDetailPage() {
  const { name } = useParams<{ name: string }>();
  const { data: workflow, isLoading, isError } = useWorkflow(name);
  const { data: runs } = useWorkflowRuns(name);
  const run = useRunWorkflow();
  const navigate = useNavigate();

  const [input, setInput] = useState("");
  const [error, setError] = useState<string | null>(null);

  if (isLoading) return <p className="p-6 text-sm text-faint">Loading…</p>;
  if (isError || !workflow) {
    return <p className="p-6 text-sm text-red-ink">No such workflow.</p>;
  }

  const nodes = workflow.steps.map((s) => ({ step: s.name, detail: s.agent }));
  const edges = workflow.steps.flatMap((s) =>
    (s.transitions ?? []).map((t) => ({
      from: s.name,
      to: t.to,
      condition: t.condition,
    })),
  );

  const start = () => {
    setError(null);
    run.mutate(
      { name: workflow.name, body: { input } },
      {
        onSuccess: (r) => navigate(`/sessions/${r.session.id}`),
        onError: (e) => setError(e instanceof Error ? e.message : String(e)),
      },
    );
  };

  return (
    <div className="flex h-full flex-col" data-testid="workflow-detail-page">
      <div className="flex items-center gap-3 border-b px-6 py-4">
        <h1 className="page-title">{workflow.name}</h1>
        <Link
          to={`/workflows/${encodeURIComponent(workflow.name)}/edit`}
          className="key ml-auto !px-2.5 !py-1.5 text-xs"
          data-testid="edit-workflow"
        >
          <Pencil size={14} />
          Edit
        </Link>
      </div>

      <div className="flex-1 space-y-4 overflow-y-auto px-6 py-4">
        {workflow.description && (
          <p className="max-w-prose text-sm text-dim">{workflow.description}</p>
        )}

        <section className="panel p-4">
          <h2 className="legend">Run it</h2>
          <p className="mt-2 max-w-prose text-xs text-faint">
            Every step shares one runtime and one workspace. The run starts on
            the server’s default runtime; the input below is what{" "}
            <span className="text-dim">{workflow.start}</span> is handed.
          </p>
          <textarea
            className="field mt-3 min-h-20 w-full"
            value={input}
            placeholder="What this run is about."
            onChange={(e) => setInput(e.target.value)}
            data-testid="run-input"
          />
          {error && <p className="mt-2 text-sm text-red-ink">{error}</p>}
          <button
            className="key key-go mt-3 !px-2.5 !py-1.5 text-xs"
            disabled={!input.trim() || run.isPending}
            onClick={start}
            data-testid="start-run"
          >
            <Play size={14} />
            {run.isPending ? "Starting…" : "Start run"}
          </button>
        </section>

        <section className="panel p-4">
          <h2 className="legend">Graph</h2>
          <div className="mt-3 overflow-auto">
            <WorkflowGraph nodes={nodes} edges={edges} start={workflow.start} />
          </div>
        </section>

        <section className="panel p-4">
          <h2 className="legend">Runs</h2>
          {(runs ?? []).length === 0 ? (
            <p className="mt-2 text-sm text-faint">No runs yet.</p>
          ) : (
            <div className="mt-3 space-y-2">
              {(runs ?? []).map((s) => (
                <Link
                  key={s.id}
                  to={`/sessions/${s.id}`}
                  className="flex items-center gap-3 rounded-[var(--radius-control)] border px-3 py-2"
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
