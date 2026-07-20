import { ChevronRight, Loader2 } from "lucide-react";
import { useState } from "react";
import type { WorkItem } from "../lib/transcriptSegments";
import { cn } from "../lib/cn";
import { ThinkingBlock } from "./ThinkingBlock";
import { ToolCallCard } from "./ToolCallCard";

function getItemKey(item: WorkItem, originalIndex: number): string {
  if (item.kind === "tool") {
    return item.call.id;
  }
  return `thinking-${originalIndex}`;
}

function renderItem(item: WorkItem, key: string) {
  return item.kind === "thinking" ? (
    <ThinkingBlock key={key} text={item.text} />
  ) : (
    <ToolCallCard key={key} call={item.call} />
  );
}

function summary(items: WorkItem[]): string {
  const thinkingCount = items.filter((i) => i.kind === "thinking").length;
  const toolCount = items.filter((i) => i.kind === "tool").length;
  if (thinkingCount > 0 && toolCount > 0) {
    return `Thought and ran ${toolCount} tool${toolCount === 1 ? "" : "s"}`;
  }
  if (thinkingCount > 0) return "Thought for a moment";
  return `Ran ${toolCount} tool${toolCount === 1 ? "" : "s"}`;
}

/** Renders a `work` segment: a run of thinking blocks + regular tool calls.
 * A single visible item renders bare (no extra chrome); two or more collapse
 * into one summary row that expands into the ordered list. `showThinking`
 * filters out thinking items entirely (not just their content). */
export function WorkGroup({
  items,
  live,
  showThinking,
}: {
  items: WorkItem[];
  live: boolean;
  showThinking: boolean;
}) {
  const [open, setOpen] = useState(false);
  const visibleWithIndices = items
    .map((item, index) => ({ item, index }))
    .filter(({ item }) => item.kind === "tool" || showThinking);

  if (visibleWithIndices.length === 0) {
    if (!live) return null;
    return (
      <div
        className="flex items-center gap-1.5 px-1 py-0.5 text-xs text-faint"
        data-testid="work-group-pulse"
      >
        <Loader2 size={12} className="animate-spin text-accent" />
        <span>Working…</span>
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
  const label = live
    ? runningTool
      ? `Running ${runningTool.call.name}…`
      : "Working…"
    : summary(visible);

  return (
    <div data-testid="work-group" data-live={live}>
      <button
        className="-ml-1 flex items-center gap-1.5 rounded px-1 py-0.5 text-xs text-faint transition-colors hover:bg-surface-2 hover:text-muted"
        onClick={() => setOpen((o) => !o)}
        data-testid="work-group-toggle"
      >
        <ChevronRight
          size={11}
          className={cn("transition-transform", open && "rotate-90")}
        />
        {live && <Loader2 size={12} className="animate-spin text-accent" />}
        <span data-testid="work-group-summary">{label}</span>
      </button>
      {open && (
        <div className="mt-1 ml-3 space-y-2 border-l pl-3">
          {visibleWithIndices.map(({ item, index }) =>
            renderItem(item, getItemKey(item, index))
          )}
        </div>
      )}
    </div>
  );
}
