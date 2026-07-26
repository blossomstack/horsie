import { Gauge } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { SessionUsageStats, Usage, UsageView } from "../api/types";
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
      className="flex items-baseline justify-between gap-3 py-0.5"
      title={hint}
    >
      <span className="text-xs text-muted">{label}</span>
      <span className="font-mono text-xs text-text">{value}</span>
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

export function ContextStatsPanel({
  stats,
  totalTokens,
}: {
  stats: SessionUsageStats | undefined;
  totalTokens: number;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onPointerDown = (e: PointerEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("pointerdown", onPointerDown);
    return () => document.removeEventListener("pointerdown", onPointerDown);
  }, [open]);

  if (totalTokens <= 0) return null;

  const mainAgent = stats?.mainAgent;
  const fillPct =
    mainAgent?.contextWindow != null && mainAgent.contextWindow > 0
      ? Math.min(100, Math.round((mainAgent.contextTokens / mainAgent.contextWindow) * 100))
      : null;

  return (
    <div className="relative" ref={ref}>
      <button
        className="chip hover:bg-surface-3"
        onClick={() => setOpen((o) => !o)}
        title={
          mainAgent
            ? `${mainAgent.contextTokens} tokens in context · ${totalTokens} session total`
            : undefined
        }
        data-testid="context-stats-button"
      >
        <Gauge size={12} />
        {compactNumber(totalTokens)} tok
      </button>
      {open && stats && mainAgent && (
        <div
          className="card absolute left-0 top-full z-10 mt-1.5 w-72 p-3 shadow-lg"
          data-testid="context-stats-panel"
        >
          <div className="mb-2">
            <div
              className="flex items-center justify-between text-xs text-muted"
              title="Tokens currently loaded in the main agent's context, out of its context window. Cache status doesn't shrink this — it only affects price and speed."
            >
              <span>Context window</span>
              <span className="font-mono">
                {compactNumber(mainAgent.contextTokens)}
                {mainAgent.contextWindow != null &&
                  ` / ${compactNumber(mainAgent.contextWindow)}`}
              </span>
            </div>
            {fillPct != null && (
              <div className="mt-1 h-1.5 w-full rounded-full bg-surface-2">
                <div
                  className="h-1.5 rounded-full bg-accent"
                  style={{ width: `${fillPct}%` }}
                />
              </div>
            )}
          </div>

          {mainAgent.lastTurnUsage && (
            <>
              <div className="mb-1 text-[11px] font-semibold uppercase text-faint">
                This turn
              </div>
              <UsageBreakdown usage={mainAgent.lastTurnUsage} />
            </>
          )}

          <div className="mt-2 mb-1 text-[11px] font-semibold uppercase text-faint">
            Session total
          </div>
          <UsageBreakdown usage={stats.sessionTotal} />
        </div>
      )}
    </div>
  );
}
