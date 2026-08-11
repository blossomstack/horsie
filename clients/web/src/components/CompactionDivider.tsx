import { ChevronDown, ChevronRight, FoldVertical } from "lucide-react";
import { useState } from "react";
import type { RenderedCompaction } from "../hooks/useSessionStream";
import { compactNumber } from "../lib/format";
import { Prose } from "./Prose";

/** Where one conversation ended and the next began.
 *
 * A rule with a label, not a bubble: nobody said this. What it marks is the
 * point past which the model stopped being shown the messages above — which
 * are still right there, because compaction appends a boundary and never
 * removes anything.
 *
 * Collapsed by default. The summary is long by construction and the interesting
 * fact at a glance is *that* it happened and what it bought; the text matters
 * only when you are working out why the agent said something afterwards. */
export function CompactionDivider({ value }: { value: RenderedCompaction }) {
  const [open, setOpen] = useState(false);
  const saved = value.tokensBefore - value.tokensAfter;
  return (
    <div data-testid="compaction-divider" data-seq={value.seq} className="py-1">
      <div className="flex items-center gap-3">
        <span className="h-px flex-1 bg-[var(--rule)]" aria-hidden />
        <button
          onClick={() => setOpen((v) => !v)}
          data-testid="compaction-toggle"
          aria-expanded={open}
          className="flex shrink-0 items-center gap-1.5 rounded-full border border-[var(--rule)] px-2.5 py-1 text-[0.6875rem] uppercase tracking-wide text-dim transition-colors hover:border-amber hover:text-legend"
          title={
            open
              ? "Hide what was carried across"
              : "Show the summary and the state carried across"
          }
        >
          <FoldVertical size={11} aria-hidden />
          <span>
            Compacted
            {value.manual ? " by hand" : ""}
            {/* Entries, not messages: a tool result is an entry too, and
                counting only what the reader would call a message would
                undercount what the boundary actually covers. Omitted entirely
                when the conversation before this one has not been paged in —
                no count beats a wrong one. */}
            {value.covered !== null && <> · {value.covered} entries</>}
            {saved > 0 && <> · {compactNumber(saved)} tokens freed</>}
          </span>
          {open ? (
            <ChevronDown size={11} aria-hidden />
          ) : (
            <ChevronRight size={11} aria-hidden />
          )}
        </button>
        <span className="h-px flex-1 bg-[var(--rule)]" aria-hidden />
      </div>

      {open && (
        <div
          data-testid="compaction-detail"
          className="panel mx-auto mt-3 max-w-[46rem] p-4 text-sm"
        >
          <h4 className="mb-1.5 text-[0.6875rem] uppercase tracking-wide text-faint">
            Summary of earlier work
          </h4>
          <Prose text={value.summary} />
          {value.carriedState && (
            <>
              {/* Kept visually apart from the summary because they differ in
                  kind, not just in content: one is the model's prose and may be
                  wrong at the edges, the other is exact and was never shown to
                  the summariser. */}
              <h4 className="mb-1.5 mt-4 text-[0.6875rem] uppercase tracking-wide text-faint">
                Carried across exactly
              </h4>
              <pre className="overflow-x-auto whitespace-pre-wrap font-mono text-[0.8125rem] leading-relaxed text-dim">
                {value.carriedState}
              </pre>
            </>
          )}
        </div>
      )}
    </div>
  );
}
