import { useEffect, useRef, useState } from "react";
import type { AgentDocument, Usage, UsageView } from "../api/types";
import { compactNumber } from "../lib/format";
import { cn } from "../lib/cn";

function StatRow({
  label,
  value,
  hint,
}: {
  label: string;
  value: string;
  hint: string;
}) {
  return (
    <div
      className="flex items-baseline justify-between gap-3 py-[3px]"
      title={hint}
    >
      <span className="legend">{label}</span>
      <span className="readout text-xs">{value}</span>
    </div>
  );
}

function UsageBreakdown({ usage }: { usage: Usage | UsageView }) {
  return (
    <>
      <StatRow
        label="Input"
        value={compactNumber(usage.inputTokens)}
        hint="Full prompt tokens: system prompt, tool definitions, and the session history so far. Cache reads/writes below are included in this total, not additional."
      />
      <StatRow
        label="Output"
        value={compactNumber(usage.outputTokens)}
        hint="Tokens the model generated back."
      />
      {usage.cacheReadTokens != null && (
        <StatRow
          label="Cache read"
          value={compactNumber(usage.cacheReadTokens)}
          hint="Served from the provider's prompt cache at a steep discount, instead of being reprocessed at full price."
        />
      )}
      {usage.cacheCreationTokens != null && (
        <StatRow
          label="Cache write"
          value={compactNumber(usage.cacheCreationTokens)}
          hint="Written to the provider's prompt cache this turn at a premium — pays off as cache reads on later turns that reuse it."
        />
      )}
    </>
  );
}

/** How full the context is, as a lamp colour. Green while there is room, live
 * once the window is filling, red when a compaction or a new session is close.
 * The thresholds are the operator's decision points, not decoration. */
function band(pct: number): { color: string; word: string } {
  if (pct >= 90) return { color: "var(--red)", word: "Nearly full" };
  if (pct >= 70) return { color: "var(--live)", word: "Filling" };
  return { color: "var(--lamp-ok)", word: "Room to spare" };
}

const R = 7.5;
/** The share of the window at which the agent compacts.
 *
 * Mirrors the server's own constant. Duplicated rather than fetched: it is one
 * number that changes with a release, and a round-trip to draw a tick mark
 * would make the gauge wait on the network to render. */
const COMPACT_AT_PERCENT = 80;

const CIRC = 2 * Math.PI * R;

/** The context dial on the header strip.
 *
 * A gauge, not a number: what an operator needs at a glance is how close this
 * session is to its context limit, and a ring answers that in one saccade
 * where "128,431 / 200,000" does not. The exact figures — and the cumulative
 * spend, which is a different quantity entirely — live one click away. */
export function ContextGauge({
  agent,
  sessionTotal,
}: {
  /** The main agent's own document — context size is per-agent and is never
   * summed across agents. Every figure here comes from the server, so a
   * session reopened after the server offloaded it reads the same as one that
   * has been in front of you the whole time. */
  agent: AgentDocument | undefined;
  /** The session's usage summed across every agent it hosts. */
  sessionTotal: UsageView | undefined;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onPointerDown = (e: PointerEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("pointerdown", onPointerDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  // Gated on the document existing, not on a token count. The old gate was
  // `totalTokens <= 0` against a locally-accumulated total, so a session the
  // server had offloaded — which replays no events — came back from a reload
  // with the dial gone entirely, even though every figure behind it was sitting
  // on the agent document.
  if (!agent) return null;

  const totalTokens = agent.usage.inputTokens + agent.usage.outputTokens;
  const known =
    agent.contextWindow != null &&
    agent.contextWindow > 0 &&
    agent.contextTokens != null;
  const pct = known
    ? Math.min(100, Math.round((agent.contextTokens / agent.contextWindow!) * 100))
    : null;
  const tone = pct != null ? band(pct) : null;

  return (
    <div className="relative" ref={ref}>
      <button
        className="flex h-8 w-8 items-center justify-center rounded-[var(--radius-control)] transition-colors hover:bg-raised"
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
        aria-label={
          pct != null
            ? `Context ${pct}% full — ${tone!.word}. Open token usage.`
            : "Open token usage"
        }
        title={
          pct != null
            ? `Context ${pct}% full — ${tone!.word}. ${compactNumber(agent.contextTokens)} of ${compactNumber(agent.contextWindow!)}. Click for the token breakdown.`
            : `${compactNumber(totalTokens)} tokens spent. Context window unknown for this model. Click for the token breakdown.`
        }
        data-testid="context-stats-button"
        data-pct={pct ?? ""}
      >
        <svg width="20" height="20" viewBox="0 0 20 20" aria-hidden>
          {/* The dial face: a full track, so an empty context still reads as a
              gauge rather than as a missing element. */}
          <circle
            cx="10"
            cy="10"
            r={R}
            fill="none"
            stroke="var(--rule-strong)"
            strokeWidth="2.5"
          />
          {pct != null && (
            // A floor of 4% of the sweep: a session that has barely started is
            // still a *known* reading, and with a literal 0-length arc it was
            // pixel-identical to the "window unknown" state.
            <circle
              cx="10"
              cy="10"
              r={R}
              fill="none"
              stroke={tone!.color}
              strokeWidth="2.5"
              strokeLinecap="butt"
              strokeDasharray={`${Math.max(0.04, pct / 100) * CIRC} ${CIRC}`}
              transform="rotate(-90 10 10)"
              style={{ transition: "stroke-dasharray 500ms ease-out" }}
            />
          )}
          {pct == null && (
            <circle cx="10" cy="10" r="2" fill="var(--legend-faint)" />
          )}
        </svg>
      </button>

      {open && (
        <div
          className="panel absolute right-0 top-full z-20 mt-2 w-[19rem] p-3.5 shadow-[var(--panel-lift)]"
          data-testid="context-stats-panel"
        >
          <div title="Tokens currently loaded in the main agent's context, out of its context window. Cache status doesn't shrink this — it only affects price and speed.">
            <div className="flex items-baseline justify-between gap-3">
              <span className="legend">Context window</span>
              <span className="readout text-xs">
                {compactNumber(agent.contextTokens)}
                {agent.contextWindow != null &&
                  ` / ${compactNumber(agent.contextWindow)}`}
                {pct != null && ` · ${pct}%`}
              </span>
            </div>
            {pct != null && (
              <>
                <div
                  className="screen relative mt-1.5 h-1.5 w-full overflow-hidden !rounded-[2px]"
                  role="meter"
                  aria-valuenow={pct}
                  aria-valuemin={0}
                  aria-valuemax={100}
                  aria-label="Context window used"
                >
                  <div
                    className="h-full transition-[width] duration-500"
                    style={{ width: `${pct}%`, background: tone!.color }}
                  />
                  {/* Where compaction happens. On the bar rather than the dial:
                      a 20px dial has no room for a mark that reads as anything,
                      and this is the surface someone opens *because* they are
                      wondering. No number beside it — the position is the
                      statement. */}
                  <div
                    data-testid="compaction-threshold"
                    className="absolute inset-y-0 w-px bg-[var(--legend-faint)]"
                    style={{ left: `${COMPACT_AT_PERCENT}%` }}
                    title={`Compacts automatically around ${COMPACT_AT_PERCENT}% full`}
                    aria-hidden
                  />
                </div>
                <p
                  className={cn("legend mt-1.5")}
                  style={{ color: tone!.color }}
                >
                  {tone!.word}
                </p>
              </>
            )}
          </div>

          {agent.lastTurnUsage && (
            <div className="mt-3.5 pt-2.5">
              <div className="legend mb-1 !text-dim">This turn</div>
              <UsageBreakdown usage={agent.lastTurnUsage} />
            </div>
          )}

          {sessionTotal && (
            <div className="mt-3.5 pt-2.5">
              <div
                className="legend mb-1 !text-dim"
                title="Everything this session has spent, across every agent it hosts. This is cost, not context fullness — the dial above is context."
              >
                Session total
              </div>
              <UsageBreakdown usage={sessionTotal} />
            </div>
          )}
        </div>
      )}
    </div>
  );
}
