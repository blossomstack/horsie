import { RotateCcw, Square, Trash2 } from "lucide-react";
import { useState } from "react";
import { useNavigate } from "react-router-dom";
import type { StepRunView, WorkflowRunGraph } from "../../api/types";
import { WorkflowGraph, type NodeState } from "../../components/WorkflowGraph";
import { relativeTime } from "../../lib/format";
import { useRetryStep, useWorkflowRun } from "../../hooks/useWorkflows";

/**
 * A run's page.
 *
 * The graph *is* the transcript here: a run has no single conversation, so the
 * header carries what a run has (status, tokens, the controls) and the body
 * carries where it got to. Opening a node goes to that step's own page, which
 * is where its messages are.
 *
 * No context gauge: a run has no single context window to fill.
 */

/** The step's lamp: the latest execution decides how the node reads. */
function nodeState(runs: StepRunView[]): NodeState {
  const last = runs[runs.length - 1];
  if (!last) return "idle";
  switch (last.status.type) {
    case "Running":
      return "running";
    case "Concluded":
      return "done";
    case "Failed":
      return "failed";
    case "Cancelled":
      return "cancelled";
    default:
      return "idle";
  }
}

const STATUS_TEXT: Record<string, string> = {
  Pending: "Pending",
  Running: "Running",
  Suspended: "Suspended",
  AwaitingInput: "Awaiting input",
  Finished: "Finished",
  Failed: "Failed",
};

const STATUS_TONE: Record<string, string> = {
  Pending: "text-faint",
  Running: "text-amber-ink",
  Suspended: "text-orange-ink",
  AwaitingInput: "text-orange-ink",
  Finished: "text-lamp-ok",
  Failed: "text-red-ink",
};

interface Props {
  sessionId: string;
  onStop: () => void;
  onDelete: () => void;
}

export function WorkflowRunView({ sessionId, onStop, onDelete }: Props) {
  const { data: graph, isLoading } = useWorkflowRun(sessionId);
  const retry = useRetryStep(sessionId);
  const navigate = useNavigate();
  const [selected, setSelected] = useState<string | undefined>();

  if (isLoading || !graph) {
    return <p className="p-6 text-sm text-faint">Loading run…</p>;
  }

  const nodes = graph.nodes.map((n) => ({
    step: n.step,
    state: nodeState(n.runs),
    detail: n.runs.length > 0 ? lastDetail(n.runs) : undefined,
    attempts: n.runs.length,
  }));
  const edges = graph.edges.map((e) => ({
    from: e.from,
    to: e.to,
    condition: e.condition,
    taken: e.traversals.length > 0,
  }));

  const selectedNode = graph.nodes.find((n) => n.step === selected);
  const live = !isTerminal(graph);

  return (
    <div className="flex h-full flex-col" data-testid="workflow-run-view">
      <header className="flex items-center gap-4 border-b px-6 py-3">
        <div className="min-w-0">
          <h1 className="page-title truncate">{graph.workflow}</h1>
          <span
            className={`text-xs ${STATUS_TONE[graph.status.type] ?? "text-faint"}`}
            data-testid="run-status"
            data-status={graph.status.type}
          >
            {STATUS_TEXT[graph.status.type] ?? graph.status.type}
          </span>
        </div>
        <span className="ml-auto text-xs text-faint" data-testid="run-usage">
          {(graph.inputTokens + graph.outputTokens).toLocaleString()} tokens
        </span>
        <button
          className="key key-stop !px-2 !py-1 text-xs"
          onClick={onStop}
          disabled={!live}
          data-testid="run-stop"
        >
          <Square size={13} />
          Interrupt
        </button>
        <button
          className="key key-danger !px-2 !py-1 text-xs"
          onClick={onDelete}
          data-testid="run-delete"
        >
          <Trash2 size={13} />
          Delete
        </button>
      </header>

      {graph.error && (
        <p className="border-b border-red bg-red-quiet px-6 py-2 text-sm text-red-ink">
          {graph.error}
        </p>
      )}

      <div className="flex flex-1 gap-6 overflow-hidden px-6 py-4">
        <div className="flex-1 overflow-auto">
          <WorkflowGraph
            nodes={nodes}
            edges={edges}
            start={graph.start}
            selected={selected}
            onSelect={setSelected}
          />
        </div>

        <aside className="w-80 shrink-0 overflow-y-auto">
          {!selectedNode ? (
            <div className="panel p-4">
              <h2 className="legend">Steps</h2>
              <p className="mt-2 text-xs text-faint">
                Choose a step to see its attempts, or open one to read what it
                actually did.
              </p>
            </div>
          ) : (
            <div className="panel p-4" data-testid="step-detail">
              <h2 className="legend">{selectedNode.step}</h2>
              {selectedNode.runs.length === 0 ? (
                <p className="mt-2 text-xs text-faint">This run never reached it.</p>
              ) : (
                <div className="mt-3 space-y-3">
                  {selectedNode.runs
                    .slice()
                    .reverse()
                    .map((r) => (
                      <div
                        key={r.index}
                        className="rounded-[var(--radius-control)] border p-2"
                        data-testid="step-attempt"
                      >
                        <div className="flex items-center gap-2">
                          <span className="text-xs text-dim">
                            Attempt {r.attempt}
                          </span>
                          <span
                            className={`ml-auto text-xs ${
                              r.status.type === "Failed"
                                ? "text-red-ink"
                                : r.status.type === "Running"
                                  ? "text-amber-ink"
                                  : "text-faint"
                            }`}
                          >
                            {r.status.type}
                          </span>
                        </div>
                        <div className="mt-1 text-xs text-faint">
                          {relativeTime(r.startedAtMs)}
                        </div>
                        {r.error && (
                          <p className="mt-2 text-xs text-red-ink">{r.error}</p>
                        )}
                        <div className="mt-2 flex gap-2">
                          <button
                            className="key !px-2 !py-1 text-xs"
                            onClick={() => navigate(`/sessions/${sessionId}/agents/${r.agentId}`)}
                            data-testid="open-step"
                          >
                            Open
                          </button>
                          <button
                            className="key !px-2 !py-1 text-xs"
                            onClick={() => retry.mutate(r.index)}
                            disabled={retry.isPending}
                            data-testid="retry-step"
                            title="Re-run this step. The workspace is not rolled back."
                          >
                            <RotateCcw size={12} />
                            Retry
                          </button>
                        </div>
                      </div>
                    ))}
                </div>
              )}
            </div>
          )}
        </aside>
      </div>
    </div>
  );
}

function isTerminal(graph: WorkflowRunGraph): boolean {
  return graph.status.type === "Finished" || graph.status.type === "Failed";
}

function lastDetail(runs: StepRunView[]): string | undefined {
  const last = runs[runs.length - 1];
  if (!last) return undefined;
  return last.status.type === "Running" ? "running…" : undefined;
}
