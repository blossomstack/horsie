import {
  MessageCircleQuestion,
  PauseCircle,
  RotateCcw,
  Square,
  Trash2,
} from "lucide-react";
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

/**
 * The execution a parked run is waiting on, if it is waiting on one.
 *
 * A step asks through `conclude`, and the question lives in that step's own
 * transcript — which is its page, not this one. So the run page's job is to say
 * *which* step is waiting and get you there; a second transcript-shaped surface
 * does not belong on a page deliberately built without one.
 *
 * Exported for its own test: `current` indexes the run log, not `nodes`, so
 * finding the execution means searching across nodes.
 */
export function parkedStep(
  graph: WorkflowRunGraph,
): { step: string; agentId: string } | undefined {
  if (graph.status.type !== "AwaitingInput" || graph.current === undefined) {
    return undefined;
  }
  for (const node of graph.nodes) {
    const run = node.runs.find((r) => r.index === graph.current);
    if (run) return { step: node.step, agentId: run.agentId };
  }
  return undefined;
}

/**
 * Where a suspended run stopped, so the page can offer to resume it.
 *
 * A run is suspended when a step was interrupted — by Interrupt, or by the server
 * restarting under it — and it is deliberately not resumed on its own, because
 * how far that step got is unknowable. A retry is the only thing that moves it,
 * so the page has to say so: this state became reachable at all only once
 * interruption stopped leaving runs wedged as `Running`, and "Suspended" with no
 * explanation is a dead end.
 *
 * The cancelled execution is the last one in the log, and the retry names it by
 * index.
 */
export function resumePoint(
  graph: WorkflowRunGraph,
): { step: string; index: number } | undefined {
  if (graph.status.type !== "Suspended") return undefined;
  let best: { step: string; index: number } | undefined;
  for (const node of graph.nodes) {
    for (const run of node.runs) {
      if (run.status.type !== "Cancelled") continue;
      if (best === undefined || run.index > best.index) {
        best = { step: node.step, index: run.index };
      }
    }
  }
  return best;
}

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
  const parked = parkedStep(graph);
  const resume = resumePoint(graph);

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

      {parked && (
        <div
          className="flex items-center gap-3 border-b border-orange bg-orange-quiet px-6 py-2 text-sm text-orange-ink"
          data-testid="run-awaiting"
        >
          <MessageCircleQuestion size={15} className="shrink-0" />
          <span>
            <strong className="font-medium">{parked.step}</strong> is waiting on
            a question.
          </span>
          {/* The primary action on the page while a run is blocked: nothing else
              here moves it, and the question itself lives in the step's own
              transcript, where its choices and answer box are. */}
          <button
            className="key key-go ml-auto !px-2 !py-1 text-xs"
            onClick={() =>
              navigate(`/sessions/${sessionId}/agents/${parked.agentId}`)
            }
            data-testid="open-parked-step"
          >
            Answer it
          </button>
        </div>
      )}

      {resume && (
        <div
          className="flex items-center gap-3 border-b border-orange bg-orange-quiet px-6 py-2 text-sm text-orange-ink"
          data-testid="run-suspended"
        >
          <PauseCircle size={15} className="shrink-0" />
          <span>
            <strong className="font-medium">{resume.step}</strong> was
            interrupted. Nothing runs until you retry it — the workspace is not
            rolled back, so it starts from whatever the last attempt left.
          </span>
          <button
            className="key key-go ml-auto !px-2 !py-1 text-xs"
            onClick={() => retry.mutate(resume.index)}
            disabled={retry.isPending}
            data-testid="resume-run"
          >
            <RotateCcw size={12} />
            Retry {resume.step}
          </button>
        </div>
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

        <aside className="w-80 shrink-0 space-y-4 overflow-y-auto">
          {/* The run's result. It was on the wire from the start and rendered
              nowhere, so the one thing a finished run produced was reachable
              only by opening its last step. Above the step panel because it is
              what the page is *for* once the run is over. */}
          {graph.output !== undefined && graph.output !== null && (
            <div className="panel p-4" data-testid="run-output">
              <h2 className="legend">Result</h2>
              <pre className="mt-2 overflow-x-auto whitespace-pre-wrap break-words text-xs text-dim">
                {formatOutput(graph.output)}
              </pre>
            </div>
          )}

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

/** A run's output as text: a string is its own answer, anything else is JSON.
 *
 * The same rule the server uses to hand one step's output to the next, so what
 * is read here is what a following step would have been given. */
export function formatOutput(output: unknown): string {
  return typeof output === "string" ? output : JSON.stringify(output, null, 2);
}

function lastDetail(runs: StepRunView[]): string | undefined {
  const last = runs[runs.length - 1];
  if (!last) return undefined;
  return last.status.type === "Running" ? "running…" : undefined;
}
