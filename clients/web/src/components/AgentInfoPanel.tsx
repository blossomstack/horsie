import { MessageSquareText, Trash2 } from "lucide-react";
import type { AgentStats, SubAgentView, SubSessionView, UsageView } from "../api/types";
import {
  KIND_LABEL,
  isLive,
  isRunNode,
  runGroups,
  runStatus,
  type AgentKind,
} from "../lib/agentTree";
import { absoluteTime, clockTime, compactNumber, humanDuration } from "../lib/format";
import { cn } from "../lib/cn";
import { Prose } from "./Prose";
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
  /**
   * When it began, and when it reached its result. Zero when the session's
   * journal never recorded one — an agent from before these were kept, and the
   * main agent, which nothing spawned.
   *
   * Both pictures draw with these and neither could say them: a bar's length
   * *is* a duration, and the one question you cannot answer by looking at a
   * bar is how long it was.
   */
  startedAtMs: number;
  endedAtMs: number;
  /**
   * Whether this has a page to open. False for a run an agent invoked: it is
   * not a session, nothing renders it on its own, and the key went to the
   * inviting session's transcript — which is not the run.
   */
  opens: boolean;
}

/**
 * A workflow run, as the panel reads it.
 *
 * A run is not on either roster — it has no transcript, no context and no id
 * of its own — so everything here is folded from its steps: when the first one
 * began, when the last one stopped, and what they spent between them. Summed
 * over each step's *subtree*, so the work a step delegated is counted once and
 * counted here.
 */
function selectRun(
  nodeId: string,
  agents: SubAgentView[],
  runTitle: string | undefined,
): SelectedAgent | null {
  const group = runGroups(agents, runTitle).find((g) => g.nodeId === nodeId);
  const steps = group?.steps ?? [];
  if (!group || steps.length === 0) return null;
  const usage = steps.reduce(
    (total, s) => ({
      inputTokens: total.inputTokens + s.stats.subtreeUsage.inputTokens,
      outputTokens: total.outputTokens + s.stats.subtreeUsage.outputTokens,
    }),
    { inputTokens: 0, outputTokens: 0 } as UsageView,
  );
  return {
    id: nodeId,
    title: group.label,
    kind: "run",
    status: runStatus(steps),
    startedAtMs: steps[0].spawnedAtMs,
    endedAtMs: steps.reduce((last, s) => Math.max(last, s.endedAtMs), 0),
    // The session's own run is its page; one an agent invoked has none.
    opens: group.root,
    // No context figure: a run has no single window to fill, which is why its
    // own page carries no gauge either.
    stats: { usage, subtreeUsage: usage, contextTokens: 0 },
  };
}

/** The roster rows, in the one shape this panel reads. */
export function selectAgent(
  id: string,
  agents: SubAgentView[],
  subSessions: SubSessionView[],
  sessionName: string | undefined,
  /** What this run is called, when the session is one. */
  runTitle?: string,
): SelectedAgent | null {
  if (isRunNode(id)) return selectRun(id, agents, runTitle);
  const sub = subSessions.find((s) => s.id === id);
  if (sub) {
    return {
      id,
      title: sub.title,
      kind: "sub_session",
      status: sub.status,
      input: sub.input,
      stats: sub.stats,
      startedAtMs: sub.createdAtMs,
      // Not an end — nothing closes a session — which is why the panel names
      // it for what it is rather than filing it under "ended".
      endedAtMs: sub.lastActivityMs,
      opens: true,
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
    startedAtMs: agent.spawnedAtMs,
    endedAtMs: agent.endedAtMs,
    opens: true,
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

/** One heading, shared by both panels beside the two pictures, so a section in
 *  one does not out-shout a section in the other. */
export const SECTION_TITLE = "legend !text-legend font-semibold";

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
      {/* A heading, not a caption. It was `legend !text-faint`: smaller than
          the body under it *and* quieter, so the one line whose job is to say
          what the block is came out as the least visible thing in it. */}
      {title && <h3 className={SECTION_TITLE}>{title}</h3>}
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

/**
 * When this agent ran, and for how long.
 *
 * Drawn from the roster's own stamps, so it costs nothing and never disagrees
 * with the lengths the timeline drew from the same two numbers. Absent
 * entirely when neither was recorded, rather than shown as a row of dashes:
 * an empty section teaches that the panel has nothing to say about time.
 */
function TimingSection({ agent }: { agent: SelectedAgent }) {
  const { startedAtMs: from, endedAtMs: to } = agent;
  if (from <= 0 && to <= 0) return null;
  const live = isLive(agent.status);
  // A live agent is measured against now, which is what the timeline does with
  // a bar that has not finished. It moves whenever the panel re-renders, which
  // a live session does constantly.
  const until = live ? Date.now() : to;
  const elapsed = from > 0 && until > from ? until - from : null;
  return (
    <Section title="Timing">
      {from > 0 && (
        <Row
          label={
            agent.kind === "sub_session" ? "Branched" : agent.kind === "main" ? "Opened" : "Spawned"
          }
          value={clockTime(from)}
          hint={absoluteTime(from)}
        />
      )}
      {to > from && (
        <Row
          // A session has no end, so the same stamp is named for what it
          // actually is on one and for what it actually is on the other.
          label={agent.kind === "sub_session" || live ? "Last activity" : "Ended"}
          value={clockTime(to)}
          hint={absoluteTime(to)}
        />
      )}
      {elapsed != null && (
        <Row
          label={live ? "Running for" : "Took"}
          value={humanDuration(elapsed)}
          hint={
            live
              ? "Measured against now: this agent has not stopped."
              : "From when it began to when it reached this result."
          }
        />
      )}
    </Section>
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
      legend={agent.kind === "run" ? "Run" : "Agent"}
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
            // `item-title`, the system's recipe for the name of one row — the
            // rail and every list already use it, and this is the same thing:
            // the name of what the panel is about.
            className="item-title break-words"
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

        <TimingSection agent={agent} />

        {/* A run has no context of its own to draw: it is a sequence of
            agents, each with a window of its own, and the run's page says as
            much by carrying no gauge either. */}
        {stats && agent.kind !== "run" && (
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

        {/* Both of these are markdown: a brief is written by an agent (or by
            the person briefing one) and a result is written by a model, so
            both arrive with headings, lists, fences and links in them. Held as
            pre-wrapped plain text they rendered as their own source — `##` and
            `- ` and bare fence markers down a 18rem column — which is the one
            rendering nobody wanted, least of all for the two blocks in this
            panel anyone actually reads at length. */}
        {agent.input && (
          <Section title={agent.kind === "sub_session" ? "Brief" : "Task"}>
            <div data-testid="agent-panel-input">
              <Prose text={agent.input} compact />
            </div>
          </Section>
        )}

        {agent.output && (
          <Section title="Result">
            <div data-testid="agent-panel-output">
              <Prose text={agent.output} compact />
            </div>
          </Section>
        )}
      </div>

      <div className="flex shrink-0 items-center gap-2 border-t px-3 py-2">
        {agent.opens && (
        <button
          className="key key-flat !px-2 !py-1 text-xs"
          onClick={() => onOpenTranscript(agent.id)}
          data-testid="agent-panel-open"
        >
          {/* The same glyph the jump key on a node carries. One action, one
              icon: the panel's key and the node's key go to the same place. */}
          <MessageSquareText size={13} aria-hidden />
          {/* A run's page is its graph; every other kind has a transcript. */}
          {agent.kind === "run" ? "Run" : "Transcript"}
        </button>
        )}
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
