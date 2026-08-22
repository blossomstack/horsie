import { ExternalLink, Trash2 } from "lucide-react";
import type { AgentStats, SubAgentView, SubSessionView, UsageView } from "../api/types";
import { KIND_LABEL, type AgentKind } from "../lib/agentTree";
import { compactNumber } from "../lib/format";
import { cn } from "../lib/cn";
import { SidePanel } from "./SidePanel";

/**
 * One agent, as the session's own record has it.
 *
 * Every figure here is banked — folded from an event the session journaled —
 * which is why this panel costs nothing to open: a graph of thirty agents is
 * one session read, not thirty agent recoveries. The consequence is honest and
 * worth stating in the UI: the context figure is as of the end of that agent's
 * last turn, not as of this instant.
 *
 * Shared with the timeline deliberately. Both views answer "what is this
 * agent?" and had no way to answer it except by navigating away from the
 * picture you were reading.
 */

/** What the graph and the timeline hand this panel: one selected agent. */
export interface SelectedAgent {
  id: string;
  title: string;
  kind: AgentKind;
  status: string;
  agentType?: string;
  /** What it was asked to do. Absent for the main agent, which is talked to
   * turn by turn rather than briefed once. */
  input?: string;
  /** What it produced. Only delegated work has one. */
  output?: string;
  stats?: AgentStats;
  error?: string;
}

/** The roster rows, in the one shape this panel reads. */
export function selectAgent(
  id: string,
  agents: SubAgentView[],
  subSessions: SubSessionView[],
  sessionName: string | undefined,
): SelectedAgent | null {
  const sub = subSessions.find((s) => s.id === id);
  if (sub) {
    return {
      id,
      title: sub.title,
      kind: "sub_session",
      status: sub.status,
      input: sub.input,
      stats: sub.stats,
    };
  }
  const agent = agents.find((a) => a.id === id);
  if (!agent) return null;
  const kind: AgentKind =
    agent.kind === "main" ? "main" : agent.kind === "step" ? "step" : "subagent";
  return {
    id,
    // The main agent's title is the session's name, because naming the session
    // *is* naming its main agent.
    title: (kind === "main" ? (agent.title ?? sessionName) : agent.title) ?? "untitled",
    kind,
    status: agent.status,
    agentType: agent.agentType,
    input: agent.input,
    output: agent.output,
    stats: agent.stats,
    error: agent.error,
  };
}

/** An agent's status, in the agent vocabulary.
 *
 * Not `StatusBadge`, which speaks the *session* vocabulary: a session is never
 * "completed" and a subagent is never "provisioning a runtime". Two words that
 * overlap are not one word, and rendering one through the other's map is how a
 * concluded subagent came to be badged with a session's phase.
 */
const STATUS_TONE: Record<string, string> = {
  running: "text-live-ink",
  provisioning: "text-live-ink",
  awaiting_input: "text-accent-ink",
  completed: "text-lamp-ok",
  failed: "text-red-ink",
  cancelled: "text-faint",
  idle: "text-dim",
};

function AgentStatusChip({ status }: { status: string }) {
  const live = status === "running" || status === "provisioning";
  return (
    <span
      className={cn("inline-flex items-center gap-1.5", STATUS_TONE[status] ?? "text-dim")}
      data-testid="agent-panel-status"
      data-status={status}
    >
      <span className={cn("lamp", live && "lamp-live")} aria-hidden />
      <span className="legend !text-[0.625rem] text-current">{status.replace(/_/g, " ")}</span>
    </span>
  );
}

function Row({ label, value, hint }: { label: string; value: string; hint?: string }) {
  return (
    <div className="flex items-baseline justify-between gap-3 py-[3px]" title={hint}>
      <span className="legend">{label}</span>
      <span className="readout text-xs">{value}</span>
    </div>
  );
}

/** `title` is optional: the identity block at the top needs no heading, because
 *  the panel's own legend two lines above it already says "Agent". */
function Section({ title, children }: { title?: string; children: React.ReactNode }) {
  return (
    <section className="border-t px-3 py-2.5 first:border-t-0">
      {title && <h3 className="legend !text-faint">{title}</h3>}
      <div className={title ? "mt-1.5" : undefined}>{children}</div>
    </section>
  );
}

function tokensOf(u: UsageView): number {
  return u.inputTokens + u.outputTokens;
}

/** How full the context is, as a bar. The same three bands the header dial
 * uses, so one agent's fullness reads the same wherever it is drawn. */
function ContextBar({ tokens, window }: { tokens: number; window: number }) {
  const pct = Math.min(100, Math.round((tokens / window) * 100));
  const tone = pct >= 90 ? "bg-red" : pct >= 70 ? "bg-live" : "bg-lamp-ok";
  return (
    <>
      <div className="flex items-baseline justify-between gap-3">
        <span className="readout text-xs">
          {compactNumber(tokens)} / {compactNumber(window)}
        </span>
        <span className="legend">{pct}%</span>
      </div>
      <div
        className="mt-1.5 h-1.5 overflow-hidden rounded-full bg-screen"
        role="meter"
        aria-valuenow={pct}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-label="Context used"
        data-testid="agent-context-bar"
      >
        <div className={cn("h-full rounded-full", tone)} style={{ width: `${pct}%` }} />
      </div>
    </>
  );
}

export function AgentInfoPanel({
  agent,
  onClose,
  onOpenTranscript,
  onDelete,
  deleting,
}: {
  agent: SelectedAgent;
  onClose: () => void;
  /** Go and read this agent's own transcript. */
  onOpenTranscript: (agentId: string) => void;
  /** Remove this agent's run. Absent for the two that cannot go: the main
   * agent *is* the session, and a workflow step belongs to its run's log. */
  onDelete?: (agentId: string) => void;
  deleting?: boolean;
}) {
  const stats = agent.stats;
  // Worth drawing only when it differs: on a leaf the two totals are the same
  // number, and a second identical figure invites the reader to look for a
  // difference that is not there.
  const subtreeDiffers =
    stats != null && tokensOf(stats.subtreeUsage) !== tokensOf(stats.usage);

  return (
    <SidePanel
      legend="Agent"
      readout={
        <span className="readout truncate text-[0.6875rem]" data-testid="agent-panel-readout">
          {KIND_LABEL[agent.kind]}
        </span>
      }
      onClose={onClose}
      closeLabel="Hide the agent panel"
      testId="agent-panel"
      closeTestId="agent-panel-collapse"
    >
      <div className="min-h-0 flex-1 overflow-y-auto" data-agent-kind={agent.kind}>
        <Section>
          <p
            className="text-[0.8125rem] leading-snug break-words text-legend"
            data-testid="agent-panel-title"
          >
            {agent.title}
          </p>
          <div className="mt-2 flex flex-wrap items-center gap-2">
            <AgentStatusChip status={agent.status} />
            {agent.agentType && (
              <span className="chip" data-testid="agent-panel-type">
                {agent.agentType}
              </span>
            )}
          </div>
          {agent.error && (
            <p className="mt-2 text-xs leading-relaxed text-red-ink" data-testid="agent-panel-error">
              {agent.error}
            </p>
          )}
        </Section>

        {stats && (
          <Section title="Context">
            {stats.contextWindow != null && stats.contextWindow > 0 ? (
              <ContextBar tokens={stats.contextTokens} window={stats.contextWindow} />
            ) : (
              <Row
                label="In context"
                value={compactNumber(stats.contextTokens)}
                hint="No window is configured for this agent's model, so there is no fraction to draw."
              />
            )}
            <p className="mt-1.5 text-[0.6875rem] leading-relaxed text-faint">
              As of the end of this agent's last turn.
            </p>
          </Section>
        )}

        {stats && (
          <Section title="Tokens">
            <Row
              label="Input"
              value={compactNumber(stats.usage.inputTokens)}
              hint="Full prompt tokens across this agent's turns. Cache reads and writes are included in this total, not additional."
            />
            <Row
              label="Output"
              value={compactNumber(stats.usage.outputTokens)}
              hint="Tokens this agent generated back."
            />
            {stats.usage.cacheReadTokens != null && (
              <Row
                label="Cache read"
                value={compactNumber(stats.usage.cacheReadTokens)}
                hint="Served from the provider's prompt cache at a discount."
              />
            )}
            {stats.usage.cacheCreationTokens != null && (
              <Row
                label="Cache write"
                value={compactNumber(stats.usage.cacheCreationTokens)}
                hint="Written to the provider's prompt cache at a premium."
              />
            )}
            {subtreeDiffers && (
              <div className="mt-2 border-t pt-2" data-testid="agent-panel-subtree">
                <Row
                  label="With subtree"
                  value={compactNumber(tokensOf(stats.subtreeUsage))}
                  hint="This agent plus everything below it: the subagents it spawned, the sub sessions branched from it, and the steps of any workflow it invoked."
                />
              </div>
            )}
          </Section>
        )}

        {agent.input && (
          <Section title={agent.kind === "sub_session" ? "Brief" : "Task"}>
            <p
              className="text-[0.8125rem] leading-snug break-words whitespace-pre-wrap text-dim"
              data-testid="agent-panel-input"
            >
              {agent.input}
            </p>
          </Section>
        )}

        {agent.output && (
          <Section title="Result">
            <p
              className="text-[0.8125rem] leading-snug break-words whitespace-pre-wrap text-dim"
              data-testid="agent-panel-output"
            >
              {agent.output}
            </p>
          </Section>
        )}
      </div>

      <div className="flex shrink-0 items-center gap-2 border-t px-3 py-2">
        <button
          className="key key-flat !px-2 !py-1 text-xs"
          onClick={() => onOpenTranscript(agent.id)}
          data-testid="agent-panel-open"
        >
          <ExternalLink size={13} aria-hidden />
          Transcript
        </button>
        {onDelete && (
          <button
            className="key-icon ml-auto hover:!bg-red-quiet hover:!text-red-ink"
            onClick={() => onDelete(agent.id)}
            disabled={deleting}
            title={
              agent.kind === "sub_session"
                ? "Delete this sub session"
                : "Delete this subagent run and everything below it"
            }
            aria-label="Delete this agent"
            data-testid="agent-panel-delete"
          >
            <Trash2 size={14} aria-hidden />
          </button>
        )}
      </div>
    </SidePanel>
  );
}
