# Session Progress UX Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Hide thinking blocks by default behind a new extensible settings menu, and collapse consecutive thinking/tool-call steps into one expandable "work group" row with a live progress status, in the horsie web session transcript.

**Architecture:** A pure `buildSegments()` function flattens each assistant turn's messages (+ live tail) into `text` / `work` (grouped) / `ask` (standalone) / `pulse` segments. `Transcript.tsx` renders each segment kind, delegating grouped work to a new `WorkGroup` component that filters items by a `showThinking` setting and only collapses runs of 2+ visible items (a single item renders bare, exactly as today). Settings are a small extensible `SETTINGS` list backed by one localStorage-persisted hook (`useUiSettings`), surfaced via a new gear-icon dropdown (`SettingsMenu`) in the session header.

**Tech Stack:** React 19 + TypeScript, Tailwind v4, `lucide-react` icons, Playwright e2e.

**Spec:** `docs/superpowers/specs/2026-07-20-session-progress-ux-design.md`

## Global Constraints

- No new npm dependencies — no popover/dropdown library; hand-roll, matching `ThemeToggle`'s and `ThinkingBlock`'s existing hand-rolled style.
- This project has **no unit-test runner** (no vitest / `@testing-library` in `clients/web/package.json`). Playwright e2e (`clients/web/e2e/`, driven against a real built server) is the only test type that exists. Frontend-only tasks below verify with `bun run typecheck` (fast, no server needed); full behavioral verification happens once, in the final task, via the e2e suite.
- Settings persist browser-wide via `localStorage`, one JSON blob, following the existing `useTheme.ts` convention (key `horsie-theme` → new key `horsie-ui-settings`).
- No italics in session-page UI chrome. (The two `italic` rules remaining in `index.css` are for code-comment/markdown-emphasis syntax highlighting — unrelated, leave them.)
- `ask_user` tool calls never collapse into a work group — they always render standalone via the existing `AskUserCard` branch in `ToolCallCard.tsx`, so a pending question is never hidden behind a click.
- A work segment with exactly one visible item renders that item directly, no extra wrapper — grouping/collapsing only kicks in for runs of 2+ visible items.
- Run all commands from `clients/web/` unless noted otherwise.

---

### Task 1: `useUiSettings` hook — extensible settings list + localStorage persistence

**Files:**
- Create: `clients/web/src/hooks/useUiSettings.ts`

**Interfaces:**
- Produces:
  - `interface SettingDef { key: string; label: string; description: string; default: boolean }`
  - `SETTINGS: SettingDef[]` — currently one entry, `{ key: "showThinking", ... }`
  - `useUiSettings(): { values: Record<string, boolean>; toggle: (key: string) => void }`

- [ ] **Step 1: Write the hook**

```ts
import { useEffect, useState } from "react";

export interface SettingDef {
  key: string;
  label: string;
  description: string;
  default: boolean;
}

/** Extensible list of boolean display settings shown in `SettingsMenu` —
 * add an entry here to add a new toggle, no new component code needed. */
export const SETTINGS: SettingDef[] = [
  {
    key: "showThinking",
    label: "Show thinking",
    description: "Reveal the model's reasoning steps in the transcript.",
    default: false,
  },
];

const STORAGE_KEY = "horsie-ui-settings";

function loadOverrides(): Record<string, boolean> {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    return raw ? (JSON.parse(raw) as Record<string, boolean>) : {};
  } catch {
    return {};
  }
}

function initialValues(): Record<string, boolean> {
  const overrides = loadOverrides();
  const values: Record<string, boolean> = {};
  for (const def of SETTINGS) values[def.key] = overrides[def.key] ?? def.default;
  return values;
}

export function useUiSettings(): {
  values: Record<string, boolean>;
  toggle: (key: string) => void;
} {
  const [values, setValues] = useState<Record<string, boolean>>(initialValues);

  useEffect(() => {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(values));
  }, [values]);

  const toggle = (key: string) => setValues((v) => ({ ...v, [key]: !v[key] }));

  return { values, toggle };
}
```

- [ ] **Step 2: Typecheck**

Run: `cd clients/web && bun run typecheck`
Expected: no errors (the file isn't imported anywhere yet, but must still compile standalone).

- [ ] **Step 3: Commit**

```bash
git add clients/web/src/hooks/useUiSettings.ts
git commit -m "web: add useUiSettings hook (extensible settings list + localStorage)"
```

---

### Task 2: `SettingsMenu` component — gear-icon dropdown

**Files:**
- Create: `clients/web/src/components/SettingsMenu.tsx`

**Interfaces:**
- Consumes: `SETTINGS`, `useUiSettings()` from Task 1 (`../hooks/useUiSettings`); `cn` from `../lib/cn`.
- Produces: `export function SettingsMenu(): JSX.Element` — no props, self-contained (reads/writes its own settings state).

- [ ] **Step 1: Write the component**

```tsx
import { Check, Settings } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { SETTINGS, useUiSettings } from "../hooks/useUiSettings";
import { cn } from "../lib/cn";

export function SettingsMenu() {
  const { values, toggle } = useUiSettings();
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

  return (
    <div className="relative" ref={ref}>
      <button
        className="btn-icon"
        onClick={() => setOpen((o) => !o)}
        title="Display settings"
        aria-label="Display settings"
        data-testid="settings-menu-button"
      >
        <Settings size={17} />
      </button>
      {open && (
        <div
          className="card absolute right-0 top-full z-10 mt-1.5 w-64 p-1.5 shadow-lg"
          data-testid="settings-menu"
        >
          {SETTINGS.map((def) => (
            <button
              key={def.key}
              className="flex w-full items-start gap-2 rounded-[var(--radius-sm)] px-2 py-1.5 text-left hover:bg-surface-2"
              onClick={() => toggle(def.key)}
              data-testid="setting-toggle"
              data-key={def.key}
              data-checked={values[def.key]}
            >
              <span
                className={cn(
                  "mt-0.5 flex h-4 w-4 shrink-0 items-center justify-center rounded border",
                  values[def.key] && "border-transparent",
                )}
                style={values[def.key] ? { background: "var(--accent)" } : undefined}
              >
                {values[def.key] && <Check size={12} className="text-accent-fg" />}
              </span>
              <span className="min-w-0">
                <span className="block text-sm text-text">{def.label}</span>
                <span className="block text-xs text-faint">{def.description}</span>
              </span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
```

- [ ] **Step 2: Typecheck**

Run: `cd clients/web && bun run typecheck`
Expected: no errors.

- [ ] **Step 3: Commit**

```bash
git add clients/web/src/components/SettingsMenu.tsx
git commit -m "web: add SettingsMenu dropdown"
```

---

### Task 3: `transcriptSegments.ts` — pure segment-building logic

**Files:**
- Create: `clients/web/src/lib/transcriptSegments.ts`

**Interfaces:**
- Consumes: `RenderedMessage`, `RenderedToolCall` from `../hooks/useSessionStream` (existing, unchanged).
- Produces:
  - `type WorkItem = { kind: "thinking"; text: string } | { kind: "tool"; call: RenderedToolCall }`
  - `type Segment = { kind: "text"; key: string; text: string; streaming?: boolean } | { kind: "work"; key: string; items: WorkItem[]; live: boolean } | { kind: "ask"; key: string; call: RenderedToolCall } | { kind: "pulse"; key: string }`
  - `buildSegments(msgs: RenderedMessage[], live?: { text: string; orphanTools: RenderedToolCall[] }): Segment[]`

- [ ] **Step 1: Write the module**

```ts
import type { RenderedMessage, RenderedToolCall } from "../hooks/useSessionStream";

/** The server's dedicated "ask the user" tool for sessions — kept in sync
 * with the same constant in ToolCallCard.tsx. */
const ASK_USER_TOOL = "ask_user";

export type WorkItem =
  | { kind: "thinking"; text: string }
  | { kind: "tool"; call: RenderedToolCall };

export type Segment =
  | { kind: "text"; key: string; text: string; streaming?: boolean }
  | { kind: "work"; key: string; items: WorkItem[]; live: boolean }
  | { kind: "ask"; key: string; call: RenderedToolCall }
  | { kind: "pulse"; key: string };

/**
 * Flattens a turn's messages (+ optional live tail) into a linear sequence
 * of text / grouped-work / standalone-question / pulse segments.
 *
 * Consecutive thinking blocks and regular tool calls — across message
 * (LLM-iteration) boundaries, as long as no text or `ask_user` call
 * interrupts them — collapse into one `work` segment. `ask_user` always
 * breaks the run and renders standalone: a pending question must never be
 * hidden inside a collapsed group.
 */
export function buildSegments(
  msgs: RenderedMessage[],
  live?: { text: string; orphanTools: RenderedToolCall[] },
): Segment[] {
  const segments: Segment[] = [];
  let work: WorkItem[] = [];
  let seq = 0;

  const flushWork = (isLive: boolean) => {
    if (work.length > 0) {
      segments.push({ kind: "work", key: `work${seq++}`, items: work, live: isLive });
      work = [];
    }
  };

  const pushToolCall = (call: RenderedToolCall) => {
    if (call.name === ASK_USER_TOOL) {
      flushWork(false);
      segments.push({ kind: "ask", key: `ask${seq++}`, call });
    } else {
      work.push({ kind: "tool", call });
    }
  };

  for (const m of msgs) {
    for (const t of m.thinking) work.push({ kind: "thinking", text: t });
    if (m.text) {
      flushWork(false);
      segments.push({ kind: "text", key: `text${seq++}`, text: m.text });
    }
    for (const tc of m.toolCalls) pushToolCall(tc);
  }

  if (live) {
    for (const tc of live.orphanTools) pushToolCall(tc);
    if (live.text) {
      flushWork(false);
      segments.push({ kind: "text", key: `text${seq++}`, text: live.text, streaming: true });
    }
    if (work.length > 0) flushWork(true);
    else if (!live.text) segments.push({ kind: "pulse", key: `pulse${seq++}` });
  } else {
    flushWork(false);
  }

  return segments;
}
```

- [ ] **Step 2: Typecheck**

Run: `cd clients/web && bun run typecheck`
Expected: no errors.

- [ ] **Step 3: Commit**

```bash
git add clients/web/src/lib/transcriptSegments.ts
git commit -m "web: add buildSegments — group thinking/tool-call runs, carve out ask_user"
```

---

### Task 4: `WorkGroup` component + `ThinkingBlock` polish

**Files:**
- Create: `clients/web/src/components/WorkGroup.tsx`
- Modify: `clients/web/src/components/ThinkingBlock.tsx`

**Interfaces:**
- Consumes: `WorkItem` from Task 3 (`../lib/transcriptSegments`); `ThinkingBlock`, `ToolCallCard` (existing); `cn` from `../lib/cn`.
- Produces: `export function WorkGroup({ items, live, showThinking }: { items: WorkItem[]; live: boolean; showThinking: boolean }): JSX.Element | null`

- [ ] **Step 1: Update `ThinkingBlock.tsx`** — drop the italic label style (the only italic UI chrome on the session page) and add test ids so `WorkGroup`'s expanded content and the settings-toggle e2e coverage can target it:

```tsx
import { ChevronRight } from "lucide-react";
import { useState } from "react";
import { cn } from "../lib/cn";

export function ThinkingBlock({ text }: { text: string }) {
  const [open, setOpen] = useState(false);
  return (
    <div data-testid="thinking-block">
      <button
        className="-ml-1 flex items-center gap-1 rounded px-1 py-0.5 text-xs text-faint transition-colors hover:bg-surface-2 hover:text-muted"
        onClick={() => setOpen((o) => !o)}
        data-testid="thinking-toggle"
      >
        <ChevronRight
          size={11}
          className={cn("transition-transform", open && "rotate-90")}
        />
        <span>Thought for a moment</span>
      </button>
      {open && (
        <div
          className="mt-1 ml-3 border-l pl-3 text-xs leading-relaxed whitespace-pre-wrap text-faint"
          data-testid="thinking-content"
        >
          {text}
        </div>
      )}
    </div>
  );
}
```

- [ ] **Step 2: Write `WorkGroup.tsx`**

```tsx
import { ChevronRight, Loader2 } from "lucide-react";
import { useState } from "react";
import type { WorkItem } from "../lib/transcriptSegments";
import { cn } from "../lib/cn";
import { ThinkingBlock } from "./ThinkingBlock";
import { ToolCallCard } from "./ToolCallCard";

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
  const visible = items.filter((i) => i.kind === "tool" || showThinking);

  if (visible.length === 0) {
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

  if (visible.length === 1) return renderItem(visible[0], "solo");

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
          {visible.map((item, i) => renderItem(item, `item${i}`))}
        </div>
      )}
    </div>
  );
}
```

- [ ] **Step 3: Typecheck**

Run: `cd clients/web && bun run typecheck`
Expected: no errors.

- [ ] **Step 4: Commit**

```bash
git add clients/web/src/components/WorkGroup.tsx clients/web/src/components/ThinkingBlock.tsx
git commit -m "web: add WorkGroup; drop italic + add test ids on ThinkingBlock"
```

---

### Task 5: `Transcript.tsx` — render turns as segments

**Files:**
- Modify: `clients/web/src/components/Transcript.tsx` (full rewrite of the turn-rendering logic; `groupTurns()` and `UserTurn`/`UserBubble`/`AssistantAvatar` are unchanged)

**Interfaces:**
- Consumes: `buildSegments`, `Segment` from Task 3 (`../lib/transcriptSegments`); `WorkGroup` from Task 4 (`./WorkGroup`); `ToolCallCard`, `Prose` (existing).
- Produces: `Transcript` now takes an additional required prop `showThinking: boolean`.

- [ ] **Step 1: Replace the file contents**

```tsx
import { cn } from "../lib/cn";
import type { RenderedMessage, RenderedToolCall } from "../hooks/useSessionStream";
import { buildSegments, type Segment } from "../lib/transcriptSegments";
import { Prose } from "./Prose";
import { ToolCallCard } from "./ToolCallCard";
import { WorkGroup } from "./WorkGroup";

function AssistantAvatar() {
  return (
    <div
      className="flex h-7 w-7 shrink-0 select-none items-center justify-center rounded-lg text-sm font-bold text-accent-fg"
      style={{ background: "var(--accent)" }}
      aria-hidden
    >
      h
    </div>
  );
}

function UserBubble({ text }: { text: string }) {
  return (
    <div className="flex justify-end">
      <div
        className="max-w-[85%] rounded-[var(--radius-lg)] rounded-br-sm px-3.5 py-2.5 text-[0.9375rem] leading-relaxed whitespace-pre-wrap text-text"
        style={{ background: "var(--surface-3)" }}
      >
        {text}
      </div>
    </div>
  );
}

function SegmentView({
  segment,
  showThinking,
}: {
  segment: Segment;
  showThinking: boolean;
}) {
  switch (segment.kind) {
    case "text":
      return (
        <div data-testid={segment.streaming ? "assistant-streaming" : "assistant-text"}>
          <Prose text={segment.text} />
        </div>
      );
    case "work":
      return (
        <WorkGroup items={segment.items} live={segment.live} showThinking={showThinking} />
      );
    case "ask":
      return <ToolCallCard call={segment.call} />;
    case "pulse":
      return (
        <div className="flex items-center gap-1.5 pt-1 text-sm text-faint">
          <span className="cursor-dot" />
        </div>
      );
  }
}

/** A run of consecutive assistant messages (no interleaved user turn) shares
 * one avatar — an agent's multi-step tool-call trajectory is one continuous
 * thread of work, not a series of separate replies. `live` merges a
 * still-streaming tail into the same avatar when it continues this turn;
 * with empty `msgs` it renders a turn that is entirely live. */
function AssistantTurn({
  msgs,
  live,
  showThinking,
}: {
  msgs: RenderedMessage[];
  live?: { text: string; orphanTools: RenderedToolCall[] };
  showThinking: boolean;
}) {
  const segments = buildSegments(msgs, live);
  return (
    <div data-testid="message" data-role="Assistant" className="flex gap-3 animate-rise">
      <AssistantAvatar />
      <div className="min-w-0 flex-1 space-y-2 pt-0.5">
        {segments.length === 0 ? (
          <span className="text-sm text-faint">…</span>
        ) : (
          segments.map((s) => (
            <SegmentView key={s.key} segment={s} showThinking={showThinking} />
          ))
        )}
      </div>
    </div>
  );
}

function UserTurn({ msg }: { msg: RenderedMessage }) {
  return (
    <div
      data-testid="message"
      data-role={msg.role}
      className={cn("animate-rise", msg.optimistic && "opacity-70")}
    >
      <UserBubble text={msg.text} />
    </div>
  );
}

/** Consecutive assistant messages collapse into one AssistantTurn; user
 * messages always start a fresh turn. */
type Turn =
  | { kind: "user"; msg: RenderedMessage }
  | { kind: "assistant"; id: string; msgs: RenderedMessage[] };

function groupTurns(messages: RenderedMessage[]): Turn[] {
  const turns: Turn[] = [];
  for (const m of messages) {
    if (m.role === "User") {
      turns.push({ kind: "user", msg: m });
      continue;
    }
    const last = turns[turns.length - 1];
    if (last?.kind === "assistant") last.msgs.push(m);
    else turns.push({ kind: "assistant", id: m.id, msgs: [m] });
  }
  return turns;
}

export function Transcript({
  messages,
  streaming,
  orphanTools,
  showLive,
  showThinking,
}: {
  messages: RenderedMessage[];
  streaming: string;
  orphanTools: RenderedToolCall[];
  showLive: boolean;
  showThinking: boolean;
}) {
  const turns = groupTurns(messages);
  // Gated on session status alone (not on whether content has arrived yet)
  // so the live tail — and its `pulse` progress indicator — is reachable
  // during the gap between "Running" and the first token/tool.
  const hasLive = showLive;
  const lastTurn = turns[turns.length - 1];
  // A live tail with no interleaved user message continues the last turn —
  // merge it into that turn's avatar instead of popping in a new one.
  const mergeLiveIntoLastTurn = hasLive && lastTurn?.kind === "assistant";

  return (
    <div className="mx-auto flex w-full max-w-3xl flex-col gap-5 px-4 py-6">
      {turns.map((t, i) =>
        t.kind === "user" ? (
          <UserTurn key={t.msg.id} msg={t.msg} />
        ) : (
          <AssistantTurn
            key={t.id}
            msgs={t.msgs}
            showThinking={showThinking}
            live={
              mergeLiveIntoLastTurn && i === turns.length - 1
                ? { text: streaming, orphanTools }
                : undefined
            }
          />
        ),
      )}
      {hasLive && !mergeLiveIntoLastTurn && (
        <AssistantTurn
          key="streaming"
          msgs={[]}
          showThinking={showThinking}
          live={{ text: streaming, orphanTools }}
        />
      )}
    </div>
  );
}
```

Note: the `optimistic` dimming (`opacity-70`) that the old `AssistantTurn` carried is dropped — `RenderedMessage.optimistic` is only ever set on optimistic *user* echoes (see `useSessionStream.ts`'s `addOptimisticUser`), so it was always `undefined`/falsy on assistant turns; `UserTurn` (which does receive real optimistic messages) keeps it.

- [ ] **Step 2: Typecheck**

Run: `cd clients/web && bun run typecheck`
Expected: an error at this point — `SessionView.tsx` calls `<Transcript ... />` without the new required `showThinking` prop. Confirms the prop is genuinely required before Task 6 wires it up.

- [ ] **Step 3: Commit**

```bash
git add clients/web/src/components/Transcript.tsx
git commit -m "web: render assistant turns as text/work/ask/pulse segments"
```

---

### Task 6: Wire `SettingsMenu` + `showThinking` into `SessionView`

**Files:**
- Modify: `clients/web/src/pages/SessionView.tsx`

**Interfaces:**
- Consumes: `useUiSettings` from Task 1 (`../hooks/useUiSettings`); `SettingsMenu` from Task 2 (`../components/SettingsMenu`); `Transcript`'s new `showThinking` prop from Task 5.

- [ ] **Step 1: Add the imports**

In `clients/web/src/pages/SessionView.tsx`, add alongside the existing imports:

```tsx
import { SettingsMenu } from "../components/SettingsMenu";
import { useUiSettings } from "../hooks/useUiSettings";
```

- [ ] **Step 2: Call the hook and pass `showThinking` to `Transcript`**

Inside `SessionView()`, near the other hooks (right after `const del = useDeleteSession();`):

```tsx
  const { values: uiSettings } = useUiSettings();
```

Then update the `<Transcript ... />` call to add the new prop:

```tsx
          <Transcript
            messages={stream.messages}
            streaming={stream.streaming}
            orphanTools={stream.orphanTools}
            showLive={status === SessionStatusKind.Running}
            showThinking={uiSettings.showThinking}
          />
```

- [ ] **Step 3: Add the `SettingsMenu` button to the header**

In the header's right-aligned button group (`<div className="ml-auto flex items-center gap-1">`), add `<SettingsMenu />` as the first child, before the conditional Stop button:

```tsx
        <div className="ml-auto flex items-center gap-1">
          <SettingsMenu />
          {stoppable && (
```

- [ ] **Step 4: Typecheck**

Run: `cd clients/web && bun run typecheck`
Expected: no errors (the Task 5 error from the missing prop is now resolved).

- [ ] **Step 5: Production build**

Run: `cd clients/web && bun run build`
Expected: clean build, no errors.

- [ ] **Step 6: Commit**

```bash
git add clients/web/src/pages/SessionView.tsx
git commit -m "web: surface SettingsMenu + showThinking in the session header"
```

---

### Task 7: e2e coverage + full suite green

**Files:**
- Create: `clients/web/e2e/e-progress-ux.spec.ts`

No other spec files need changes: `b-tool-call.spec.ts` (B1/B2) and `c-ask-user.spec.ts` (C1) each exercise exactly one tool call per turn, which — per Task 4's singleton rule — still renders bare, identical to today's DOM shape, so their existing assertions (`tool-call-card` visible without any group-expand click; `ask-user-card` visible immediately) continue to hold unmodified.

**Interfaces:**
- Consumes: `test`, `expect` from `./fixtures`; `createSession`, `sendMessage`, `expectStatus` from `./helpers` (all existing, unchanged).

- [ ] **Step 1: Write the new spec file**

```ts
// Group E — progress UX: thinking visibility + collapsed work-group rows.

import { test, expect } from "./fixtures";
import { createSession, sendMessage, expectStatus } from "./helpers";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

test("E1: a lone thinking step is hidden by default and revealed via Settings", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueThinking("Let me consider the options.");
  await mock.queueText("Here's my answer.");
  await createSession(page, appBase);

  await sendMessage(page, "think about it");

  await expect(page.getByTestId("assistant-text")).toContainText("Here's my answer.");
  await expect(page.getByTestId("thinking-block")).toHaveCount(0);

  await page.getByTestId("settings-menu-button").click();
  await page.locator('[data-testid="setting-toggle"][data-key="showThinking"]').click();

  const block = page.getByTestId("thinking-block");
  await expect(block).toBeVisible();
  await block.getByTestId("thinking-toggle").click();
  await expect(page.getByTestId("thinking-content")).toContainText(
    "Let me consider the options.",
  );
});

test("E2: several thinking + tool-call steps collapse into one work group", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueThinking("First I'll check the file.");
  await mock.queueToolCall("bash", { command: "echo one" });
  await mock.queueThinking("Now the second step.");
  await mock.queueToolCall("bash", { command: "echo two" });
  await mock.queueText("Both steps are done.");
  await createSession(page, appBase);

  await sendMessage(page, "do two steps");

  await expect(page.getByTestId("assistant-text")).toContainText("Both steps are done.");
  // Four LLM iterations (thinking, tool, thinking, tool) collapse into
  // exactly one work group, not four separate rows.
  await expect(page.getByTestId("work-group")).toHaveCount(1);
  await expect(page.getByTestId("work-group-summary")).toHaveText("Ran 2 tools");

  await page.getByTestId("work-group-toggle").click();
  await expect(page.locator('[data-testid="tool-call-card"]')).toHaveCount(2);
  // Thinking stays hidden — the setting was never touched in this test.
  await expect(page.getByTestId("thinking-block")).toHaveCount(0);
});

test("E3: a running tool shows a live status on a multi-item work-group row", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueThinking("Let me check first.");
  await mock.queueToolCall("bash", { command: "sleep 5" });
  await createSession(page, appBase);

  // Reveal thinking so this run has 2 visible items and actually collapses
  // into a group (a single visible item would render bare — see Task 4).
  await page.getByTestId("settings-menu-button").click();
  await page.locator('[data-testid="setting-toggle"][data-key="showThinking"]').click();

  await sendMessage(page, "run something slow");

  await expectStatus(page, "Running");
  await expect(page.getByTestId("work-group-summary")).toHaveText("Running bash…");

  await page.getByTestId("composer-stop").click();
  await expectStatus(page, "Stopped");
  // The single evolving row settles into a static summary once no longer live.
  await expect(page.getByTestId("work-group-summary")).toHaveText("Thought and ran 1 tool");
});

test("E4: ask_user always renders as a standalone question, never collapsed", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueThinking("I should ask which color.");
  await mock.queueToolCall("ask_user", {
    question: "Which color do you prefer?",
    choices: ["red", "blue"],
  });
  await createSession(page, appBase);

  await sendMessage(page, "pick a color for me");

  // The question is visible immediately, with no work-group click needed,
  // even though a thinking step preceded it.
  await expect(page.getByTestId("ask-user-card")).toContainText("Which color do you prefer?");
  await expect(page.getByTestId("thinking-block")).toHaveCount(0);
  await expectStatus(page, "AwaitingInput");
});
```

- [ ] **Step 2: Run the full e2e suite**

Run: `cd clients/web && bun install && bunx playwright install chromium && bun run test:e2e`
Expected: all tests pass — the existing 11 (A1–A3, B1–B2, C1, D1–D5) plus the new 4 (E1–E4), 15/15.

If any test fails, diagnose against this plan's design (most likely culprits: a stale build — rerun without `HORSIE_E2E_SKIP_BUILD` — or a timing assumption in E3) and fix before proceeding; do not weaken an assertion to make it pass without understanding why it failed.

- [ ] **Step 3: Typecheck + build one more time (belt and suspenders)**

Run: `cd clients/web && bun run typecheck && bun run build`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add clients/web/e2e/e-progress-ux.spec.ts
git commit -m "web/e2e: cover thinking visibility + work-group collapsing"
```

---

### Task 8: Open the PR

- [ ] **Step 1: Push the branch**

```bash
git push -u origin feat/collapse-thinking-progress
```

- [ ] **Step 2: Open the PR**

```bash
gh pr create --title "web: hide thinking by default, group work steps into a collapsed progress row" --body "$(cat <<'EOF'
## Summary
- Thinking blocks are hidden by default; a new gear-icon Settings menu (extensible list, top-right of the session header) reveals them.
- Consecutive thinking + tool-call steps (no interleaved text) collapse into one expandable work-group row with a live "Running <tool>…" / "Working…" status while active, settling into a static summary once done.
- `ask_user` questions always render standalone, never hidden inside a collapsed group.
- Fixed: the session-page "Thought for a moment" label was italic; now regular weight (the only italic UI chrome on the page — code-comment/markdown-emphasis syntax highlighting is unrelated and untouched).

## Test plan
- [x] `bun run typecheck` / `bun run build` clean
- [x] Full web e2e suite green (15/15: existing A–D groups + new group E)
EOF
)"
```

- [ ] **Step 3: Report the PR URL back**

No further action needed once CI is green — this plan's scope ends at a green, open PR.
