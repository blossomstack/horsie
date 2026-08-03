import { useEffect, useRef, useState } from "react";
import type { AgentDocument, Usage, UsageView } from "../api/types";
import { compactNumber } from "../lib/format";

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
        hint="Full prompt tokens: system prompt, tool definitions, and the conversation history so far. Cache reads/writes below are included in this total, not additional."
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

/** The token channel on the header strip: an engraved legend and its live
 * value, expanding into the full meter. The headline figure is cumulative
 * spend, not context fullness — the panel says so, because confusing the two
 * is the single easiest misread on this screen. */
export function ContextStatsPanel({
  agent,
  sessionTotal,
  totalTokens,
}: {
  /** The main agent's own document — context size is per-agent and is never
   * summed across agents. */
  agent: AgentDocument | undefined;
  /** The session's usage summed across every agent it hosts. */
  sessionTotal: UsageView | undefined;
  totalTokens: number;
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

  if (totalTokens <= 0) return null;

  const mainAgent = agent;
  const fillPct =
    mainAgent?.contextWindow != null && mainAgent.contextWindow > 0
      ? Math.min(
          100,
          Math.round((mainAgent.contextTokens / mainAgent.contextWindow) * 100),
        )
      : null;

  return (
    <div className="relative" ref={ref}>
      <button
        className="flex items-baseline gap-1.5 rounded-[var(--radius-chip)] px-1 py-0.5 transition-colors hover:bg-raised"
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
        title={
          mainAgent
            ? `${mainAgent.contextTokens} tokens in context · ${totalTokens} session total`
            : "Cumulative tokens spent by this session. Open for the context-window meter."
        }
        data-testid="context-stats-button"
      >
        <span className="legend">Tokens</span>
        <span className="readout animate-latch text-[13px]" key={totalTokens}>
          {compactNumber(totalTokens)}
        </span>
      </button>
      {open && sessionTotal && mainAgent && (
        <div
          className="panel absolute left-0 top-full z-10 mt-2 w-[19rem] p-3.5 shadow-[var(--panel-lift)]"
          data-testid="context-stats-panel"
        >
          <div title="Tokens currently loaded in the main agent's context, out of its context window. Cache status doesn't shrink this — it only affects price and speed.">
            <div className="flex items-baseline justify-between gap-3">
              <span className="legend">Context window</span>
              <span className="readout text-xs">
                {compactNumber(mainAgent.contextTokens)}
                {mainAgent.contextWindow != null &&
                  ` / ${compactNumber(mainAgent.contextWindow)}`}
                {fillPct != null && ` · ${fillPct}%`}
              </span>
            </div>
            {fillPct != null && (
              <div
                className="screen mt-1.5 h-1.5 w-full overflow-hidden !rounded-[2px]"
                role="meter"
                aria-valuenow={fillPct}
                aria-valuemin={0}
                aria-valuemax={100}
                aria-label="Context window used"
              >
                <div
                  className="h-full bg-amber transition-[width] duration-500"
                  style={{ width: `${fillPct}%` }}
                />
              </div>
            )}
          </div>

          {mainAgent.lastTurnUsage && (
            <div className="mt-3.5 border-t pt-2.5">
              <div className="legend mb-1 !text-dim">This turn</div>
              <UsageBreakdown usage={mainAgent.lastTurnUsage} />
            </div>
          )}

          <div className="mt-3.5 border-t pt-2.5">
            <div className="legend mb-1 !text-dim">Session total</div>
            <UsageBreakdown usage={sessionTotal} />
          </div>
        </div>
      )}
    </div>
  );
}
