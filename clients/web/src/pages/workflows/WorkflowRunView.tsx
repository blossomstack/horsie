import {
  MessageCircleQuestion,
  PauseCircle,
  RotateCcw,
  Square,
  Trash2,
} from "lucide-react";
import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { SessionStatusKind } from "../../api/types";
import type { StepRunView, WorkflowRunGraph } from "../../api/types";
import { WorkflowGraph, type NodeState } from "../../components/WorkflowGraph";
import { askConfirm } from "../../lib/confirm";
import { relativeTime } from "../../lib/format";
import { TONE_TEXT, statusMeta } from "../../lib/status";
import { useSession } from "../../hooks/useSessions";
import { useRetryStep, useWorkflowRun } from "../../hooks/useWorkflows";
import { Trans, useTranslation } from "react-i18next";

/**
 * A run's page.
 *
 * The graph *is* the transcript here: a run has no single session, so the
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
  status: SessionStatusKind,
): { step: string; agentId: string } | undefined {
  if (status !== SessionStatusKind.AwaitingInput || graph.current === undefined) {
    return undefined;
  }
  for (const node of graph.nodes) {
    const run = node.runs.find((r) => r.index === graph.current);
    if (run) return { step: node.step, agentId: run.agentId };
  }
  return undefined;
}

/**
 * Where a run stopped part-way, so the page can offer to resume it.
 *
 * A run stops part-way when a step was interrupted — by Interrupt, or by the
 * server restarting under it — and it is deliberately not resumed on its own,
 * because how far that step got is unknowable. A retry is the only thing that
 * moves it, so the page has to say so: an interrupted run with no explanation
 * is a dead end.
 *
 * Read off the log rather than a status word. `Suspended` was a second status
 * vocabulary for a fact the log already carries, and the log carries it more
 * precisely: a retry appends rather than truncating, so a later execution over
 * a cancelled one is what says the run moved on.
 */
export function resumePoint(
  graph: WorkflowRunGraph,
): { step: string; index: number } | undefined {
  let newest: { step: string; index: number; cancelled: boolean } | undefined;
  for (const node of graph.nodes) {
    for (const run of node.runs) {
      if (newest === undefined || run.index > newest.index) {
        newest = {
          step: node.step,
          index: run.index,
          cancelled: run.status.type === "Cancelled",
        };
      }
    }
  }
  return newest?.cancelled
    ? { step: newest.step, index: newest.index }
    : undefined;
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

interface Props {
  sessionId: string;
  onStop: () => void;
  onDelete: () => void;
}

export function WorkflowRunView({ sessionId, onStop, onDelete }: Props) {
  // A run's status is its session's — one vocabulary for every session, run or
  // session — so the graph says where it got to and the session says what
  // state it is in.
  const { data: detail } = useSession(sessionId);
  const status = detail?.status;
  const { data: graph, isLoading } = useWorkflowRun(sessionId, status);
  const retry = useRetryStep(sessionId);
  const navigate = useNavigate();
  const [selected, setSelected] = useState<string | undefined>();
  const { t } = useTranslation();

  const retryStep = async (index: number, step: string) => {
    if (
      !(await askConfirm(
        t("run.confirmRetry", { step }),
        t("run.retry"),
      ))
    ) {
      return;
    }
    retry.mutate(index);
  };

  if (isLoading || !graph || status === undefined) {
    return <p className="p-6 text-sm text-faint">{t("run.loading")}</p>;
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
  const meta = statusMeta(status);
  const live = !settled(status);
  const parked = parkedStep(graph, status);
  const resume = resumePoint(graph);

  return (
    <div className="flex h-full flex-col" data-testid="workflow-run-view">
      <header className="bar-scroll flex items-center gap-4 px-6 py-3">
        <div className="min-w-0">
          <h1 className="page-title truncate">{graph.workflow}</h1>
          <span
            className={`text-xs ${TONE_TEXT[meta.tone]}`}
            data-testid="run-status"
            data-status={status}
          >
            {meta.label}
          </span>
        </div>
        <span className="ml-auto text-xs text-faint" data-testid="run-usage">
          {t("run.tokens", {
            value: (graph.inputTokens + graph.outputTokens).toLocaleString(),
          })}
        </span>
        <button
          className="key key-stop key-sm"
          onClick={onStop}
          disabled={!live}
          data-testid="run-stop"
        >
          <Square size={13} />
          {t("run.interrupt")}
        </button>
        <button
          className="key key-danger key-sm"
          onClick={onDelete}
          data-testid="run-delete"
        >
          <Trash2 size={13} />
          {t("common.delete")}
        </button>
      </header>

      {graph.error && (
        <p className="border-b border-red bg-red-quiet px-6 py-2 text-sm text-red-ink">
          {graph.error}
        </p>
      )}

      {parked && (
        <div
          className="flex items-center gap-3 border-b border-accent bg-accent-quiet px-6 py-2 text-sm text-accent-ink"
          data-testid="run-awaiting"
        >
          <MessageCircleQuestion size={15} className="shrink-0" />
          <span>
            <Trans
              i18nKey="run.waitingOnQuestion"
              values={{ step: parked.step }}
              components={{ step: <strong className="font-medium" /> }}
            />
          </span>
          {/* The primary action on the page while a run is blocked: nothing else
              here moves it, and the question itself lives in the step's own
              transcript, where its choices and answer box are. */}
          <button
            className="key key-go ml-auto key-sm"
            onClick={() =>
              navigate(`/sessions/${sessionId}/agents/${parked.agentId}`)
            }
            data-testid="open-parked-step"
          >
            {t("run.answerIt")}
          </button>
        </div>
      )}

      {resume && (
        <div
          className="flex items-center gap-3 border-b border-accent bg-accent-quiet px-6 py-2 text-sm text-accent-ink"
          data-testid="run-suspended"
        >
          <PauseCircle size={15} className="shrink-0" />
          <span>
            <Trans
              i18nKey="run.wasInterrupted"
              values={{ step: resume.step }}
              components={{ step: <strong className="font-medium" /> }}
            />
          </span>
          <button
            className="key key-go ml-auto key-sm"
            onClick={() => void retryStep(resume.index, resume.step)}
            disabled={retryUnavailable(status, retry.isPending)}
            data-testid="resume-run"
          >
            <RotateCcw size={12} />
            {t("run.retryStep", { step: resume.step })}
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
            <div className="section" data-testid="run-output">
              <h2 className="legend">{t("agentPanel.result")}</h2>
              <pre className="mt-2 overflow-x-auto whitespace-pre-wrap break-words text-xs text-dim">
                {formatOutput(graph.output)}
              </pre>
            </div>
          )}

          {!selectedNode ? (
            <div className="section">
              <h2 className="legend">{t("run.steps")}</h2>
              <p className="mt-2 text-xs text-faint">
{t("run.stepsHint")}
              </p>
            </div>
          ) : (
            <div className="section" data-testid="step-detail">
              <h2 className="legend">{selectedNode.step}</h2>
              {selectedNode.runs.length === 0 ? (
                <p className="mt-2 text-xs text-faint">{t("run.neverReached")}</p>
              ) : (
                <div className="mt-3 space-y-3">
                  {selectedNode.runs
                    .slice()
                    .reverse()
                    .map((r) => (
                      <div
                        key={r.index}
                        className="rounded-[var(--radius-control)] p-2"
                        data-testid="step-attempt"
                      >
                        <div className="flex items-center gap-2">
                          <span className="text-xs text-dim">
                            {t("run.attempt", { n: r.attempt })}
                          </span>
                          <span
                            className={`ml-auto text-xs ${
                              r.status.type === "Failed"
                                ? "text-red-ink"
                                : r.status.type === "Running"
                                  ? "text-live-ink"
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
                            className="key key-sm"
                            onClick={() => navigate(`/sessions/${sessionId}/agents/${r.agentId}`)}
                            data-testid="open-step"
                          >
                            {t("common.open")}
                          </button>
                          <button
                            className="key key-sm"
                            onClick={() => void retryStep(r.index, selectedNode.step)}
                            disabled={retryUnavailable(status, retry.isPending, r)}
                            data-testid="retry-step"
                            title={
                              retryUnavailable(status, retry.isPending, r)
                                ? t("run.stepRunning")
                                : t("run.retryHint")
                            }
                          >
                            <RotateCcw size={12} />
                            {t("run.retry")}
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

/** Whether retrying would race a step that is already writing the workspace.
 *
 * The server can make a retry safe by cancelling the active step first, but
 * that is not what a Retry button should silently do. Keep it unavailable
 * until the run settles, and cover a stale session document with the attempt's
 * own live status. */
export function retryUnavailable(
  status: SessionStatusKind,
  retryPending: boolean,
  step?: StepRunView,
): boolean {
  return (
    retryPending ||
    status === SessionStatusKind.Running ||
    step?.status.type === "Running"
  );
}

/** Whether nothing can change without someone asking for it.
 *
 * `Finished` and `Failed` are a run's two resting places; `Unrecoverable` is
 * every session's. None of them is terminal — a retry moves all three — but
 * none of them moves on its own, which is what Interrupt and the poll care
 * about. */
function settled(status: SessionStatusKind): boolean {
  return (
    status === SessionStatusKind.Finished ||
    status === SessionStatusKind.Failed ||
    status === SessionStatusKind.Unrecoverable
  );
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
