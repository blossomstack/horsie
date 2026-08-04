import { ChevronRight } from "lucide-react";
import { useState } from "react";
import type { WorkItem } from "../lib/transcriptSegments";
import { cn } from "../lib/cn";
import { formatDuration } from "../lib/time";
import { SubAgentCard } from "./SubAgentCard";
import { ThinkingBlock } from "./ThinkingBlock";
import { ToolCallCard } from "./ToolCallCard";

function getItemKey(item: WorkItem, originalIndex: number): string {
  if (item.kind === "tool") return item.call.id;
  if (item.kind === "subagent") return `subagent-${item.result.subagentId}`;
  return `thinking-${originalIndex}`;
}

function renderItem(item: WorkItem, key: string) {
  switch (item.kind) {
    case "thinking":
      return <ThinkingBlock key={key} text={item.text} />;
    case "tool":
      return <ToolCallCard key={key} call={item.call} />;
    case "subagent":
      return <SubAgentCard key={key} result={item.result} />;
  }
}

/** Reads back what the collapsed row is hiding. Every kind of work is counted:
 * a group that summarised only its tools would tell the reader "ran 1 tool"
 * while quietly holding three finished subagents. */
function summary(items: WorkItem[]): string {
  const thinkingCount = items.filter((i) => i.kind === "thinking").length;
  const toolCount = items.filter((i) => i.kind === "tool").length;
  const subCount = items.filter((i) => i.kind === "subagent").length;
  const clauses: string[] = [];
  if (toolCount > 0) {
    clauses.push(`ran ${toolCount} tool${toolCount === 1 ? "" : "s"}`);
  }
  if (subCount > 0) {
    clauses.push(`${subCount} subagent${subCount === 1 ? "" : "s"} finished`);
  }
  if (clauses.length === 0) return "Thought for a moment";
  const body = clauses.join(" and ");
  const phrase = thinkingCount > 0 ? `Thought and ${body}` : body;
  return phrase.charAt(0).toUpperCase() + phrase.slice(1);
}

/** Renders a `work` segment: a run of thinking blocks + regular tool calls.
 * A single visible item renders bare (no extra chrome); two or more collapse
 * into one summary row that expands into the ordered list. `showThinking`
 * filters out thinking items entirely (not just their content).
 *
 * A finished group reports how long it took, from the server's stamps — the
 * one figure that says whether a collapsed row hides three seconds of work or
 * three minutes of it. */
export function WorkGroup({
  items,
  live,
  showThinking,
  startedAtMs,
  endedAtMs,
}: {
  items: WorkItem[];
  live: boolean;
  showThinking: boolean;
  startedAtMs?: number;
  endedAtMs?: number;
}) {
  const [open, setOpen] = useState(false);
  const visibleWithIndices = items
    .map((item, index) => ({ item, index }))
    .filter(({ item }) => item.kind !== "thinking" || showThinking);

  if (visibleWithIndices.length === 0) {
    if (!live) return null;
    return (
      <div
        className="flex items-center gap-2 py-0.5"
        data-testid="work-group-pulse"
      >
        <span className="lamp lamp-live text-amber-ink" aria-hidden />
        <span className="legend">Working</span>
      </div>
    );
  }

  if (visibleWithIndices.length === 1) {
    const { item, index } = visibleWithIndices[0];
    return renderItem(item, getItemKey(item, index));
  }

  const visible = visibleWithIndices.map(({ item }) => item);
  const runningTool = live
    ? [...visible]
        .reverse()
        .find(
          (i): i is Extract<WorkItem, { kind: "tool" }> =>
            i.kind === "tool" && i.call.running,
        )
    : undefined;
  const duration =
    !live && startedAtMs !== undefined && endedAtMs !== undefined
      ? formatDuration(endedAtMs - startedAtMs)
      : null;
  const label = live
    ? runningTool
      ? `Running ${runningTool.call.name}`
      : "Working"
    : summary(visible);

  return (
    <div data-testid="work-group" data-live={live}>
      <button
        className="-mx-1.5 flex items-center gap-2 rounded-[var(--radius-chip)] px-1.5 py-1 transition-colors hover:bg-raised"
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
        data-testid="work-group-toggle"
      >
        <ChevronRight
          size={11}
          className={cn(
            "shrink-0 text-faint transition-transform",
            open && "rotate-90",
          )}
          aria-hidden
        />
        {live && <span className="lamp lamp-live text-amber-ink" aria-hidden />}
        <span className="legend" data-testid="work-group-summary">
          {label}
        </span>
        {duration && (
          <span className="legend" data-testid="work-group-duration">
            · {duration}
          </span>
        )}
      </button>
      {open && (
        <div className="mt-1.5 ml-1.5 space-y-1.5 border-l pl-3.5">
          {visibleWithIndices.map(({ item, index }) =>
            renderItem(item, getItemKey(item, index)),
          )}
        </div>
      )}
    </div>
  );
}
