# Session transcript: collapsible thinking + grouped progress

**Status:** approved, not yet implemented
**Branch:** `feat/collapse-thinking-progress` (worktree `october/horsie-session-progress-ux`)

## Problem

After a user message, a turn can involve several LLM iterations of thinking +
tool calls before the final answer. Today (post `feat/concise-session-ui`,
PR #19) each thinking block and tool call already renders as its own
collapsed-by-default single-line row, but:

1. Thinking rows are shown by default, cluttering the transcript with content
   most users don't want to read.
2. A run of several thinking/tool-call steps (spanning multiple LLM
   iterations) still renders as several separate rows instead of one
   compact "the agent is working" indicator.
3. There's no way to reveal thinking content — the row itself is always
   there, just collapsed.

## Goals

1. Hide thinking blocks by default; add a settings control (top right of the
   session header) to reveal them. Build the control as a small extensible
   list so future display options can be added without new UI plumbing.
2. Collapse consecutive thinking + tool-call steps (no interleaved assistant
   text) into a single row by default, showing a live progress status while
   the agent is working.
3. Let the user expand that row to see the full ordered list of thinking
   blocks (if enabled) and tool calls, each still individually expandable
   for its own detail (unchanged from today).

## Non-goals

- No change to how thinking/tool-call *content* is fetched or stored
  (`useSessionStream` stays as-is).
- No per-session settings persistence, no server-side settings.
- No new UI toolkit dependency — settings menu and grouping use the same
  hand-rolled patterns already used by `ThemeToggle`/`ThinkingBlock`/
  `ToolCallCard`.
- Also fixes a small pre-existing issue found while touching this code:
  `ThinkingBlock`'s "Thought for a moment" label renders in italic — this is
  the only italic UI chrome on the session page (the other two `italic`
  rules in `index.css` are for code-comment/markdown-emphasis syntax
  highlighting and are unrelated) — switch it to regular weight.

## Design

### 1. Settings menu (extensible list)

New `hooks/useUiSettings.ts`, mirroring the existing `useTheme.ts`
localStorage pattern (key `horsie-theme`):

- A static list of setting definitions:
  ```ts
  interface SettingDef { key: string; label: string; description?: string; default: boolean }
  const SETTINGS: SettingDef[] = [
    { key: "showThinking", label: "Show thinking", description: "Reveal the model's reasoning steps in the transcript.", default: false },
  ];
  ```
- One localStorage blob, key `horsie-ui-settings`, holding
  `Record<string, boolean>` overrides layered on top of the defaults.
- Hook returns `{ values: Record<string, boolean>; toggle(key: string): void }`.
- Adding a future toggle (e.g. "compact tool output") is one more entry in
  `SETTINGS` — no new component code required.

New `components/SettingsMenu.tsx`:

- A gear icon `btn-icon` button in the session header, positioned with the
  existing Stop/Delete controls (top right).
- Click opens a small dropdown panel (hand-rolled, click-outside-to-close;
  no new dependency — matches the codebase's existing lack of a
  popover/menu library) listing one checkbox row per `SETTINGS` entry.

`SessionView.tsx` calls `useUiSettings()` once and passes
`values.showThinking` down through `Transcript` → `WorkGroup`.

### 2. Turn-wide segment grouping

Today `AssistantStep` renders one LLM iteration (message) at a time:
thinking block(s), then text, then tool call(s) — so a turn with several
tool-call iterations and no interleaved text still produces one row per
item across several message boundaries.

`Transcript.tsx` changes to build a flat sequence of **segments** per
`AssistantTurn`, walking all of the turn's messages *and* its live tail
(streaming text + orphan tools) in order:

- `{ kind: "text"; text: string; streaming?: boolean }` — rendered via
  `Prose`, exactly as today. Never grouped.
- `{ kind: "work"; items: WorkItem[]; live: boolean }` — a maximal run of
  consecutive thinking blocks and *regular* tool calls, spanning message
  boundaries, with no text or `ask_user` call in between. Rendered by the
  new `WorkGroup` component.
- `{ kind: "ask"; call: RenderedToolCall }` — an `ask_user` tool call.
  `ToolCallCard` already special-cases this name to render the
  always-visible `AskUserCard` (question + choices) instead of a
  collapsible row; a pending question must never be hidden inside a
  collapsed group, so it breaks work grouping exactly like text does and
  renders as its own standalone segment via the existing `ToolCallCard`.
- `{ kind: "pulse" }` — nothing has arrived yet for the live tail (replaces
  today's inline cursor-dot branch in `LiveTail`).

Build algorithm (linear scan, single pass):

```
segments = []
work = []
flushWork(live) { if (work.length) segments.push({kind:"work", items: work, live}); work = [] }

for each message m in turn.msgs:
  work.push(...m.thinking as thinking-items)
  if m.text: flushWork(false); segments.push({kind:"text", text: m.text})
  for each toolCall in m.toolCalls:
    if toolCall.name === "ask_user": flushWork(false); segments.push({kind:"ask", call: toolCall})
    else: work.push(toolCall as tool-item)

if turn has a live tail:
  for each toolCall in orphanTools:
    if toolCall.name === "ask_user": flushWork(false); segments.push({kind:"ask", call: toolCall})
    else: work.push(toolCall as tool-item)
  if live.text: flushWork(false); segments.push({kind:"text", text: live.text, streaming:true})
  if work.length: flushWork(true)      // trailing group is the live one
  else if !live.text: segments.push({kind:"pulse"})
else:
  flushWork(false)
```

A `work` segment is `live` only when it's the trailing segment of a turn
that is still actively streaming (has a live tail with no finalized closing
text yet) — i.e. at most one live work segment per turn, always the last
one.

**Fixed alongside:** today's `Transcript.tsx` only ever constructs a live
tail (`hasLive`) when `streaming.length > 0 || orphanTools.length > 0` —
so during the gap between the session entering `Running` and the first
token/tool actually arriving, nothing live-tail-shaped renders at all (the
`pulse`/cursor-dot branch that exists for exactly this case is therefore
dead code today). Gating `hasLive` on `showLive` (the session's `Running`
status) alone, instead of on non-empty content, makes that branch reachable
and gives requirement 2's "status indicating the agent is progressing" its
missing first frame. Low-risk, same code being rewritten regardless; not
independently e2e-tested since the window is sub-second and not worth an
artificial delay to pin down — covered by code review and manual smoke
instead.

### 3. `WorkGroup` component (new)

Wraps the existing `ThinkingBlock`/`ToolCallCard` unchanged — just adds an
outer collapse and a `showThinking`-gated filter.

- `visibleItems = items.filter(i => i.kind === "tool" || showThinking)`
- **Empty (`showThinking` off and the group is thinking-only):**
  - not live → render nothing.
  - live → render a bare spinner + `Working…` row, no chevron (nothing to
    expand into) — keeps the "agent is progressing" signal visible without
    leaking thinking content.
- **Exactly one visible item:** render that single `ThinkingBlock` or
  `ToolCallCard` directly, with no extra wrapper. A single tool call (the
  common case — one iteration, one tool, then the final answer) already had
  a perfectly adequate one-click-to-detail row before this change; forcing
  it through an additional group-level collapse would be a regression, not
  an improvement, and the goal was always to compress *runs* of steps, not
  add friction to a single one. (This also means a lone thinking block, once
  revealed via the setting, renders exactly as it does today.)
- **Two or more visible items:** single-line summary row (chevron + icon +
  label), collapsed by default; expands in place into the ordered
  `visibleItems` list (each `ThinkingBlock`/`ToolCallCard` still
  independently expandable, unchanged from today).
  - **Live:** spinner + `Running <tool name>…` if the last visible item is
    a tool call with `running === true`, else spinner + `Working…`.
  - **Finished:** static summary built from visible counts —
    `Thought and ran {n} tools` (both kinds present),
    `Thought for a moment` (thinking only — reuses today's copy),
    `Ran {n} tool{s}` (tools only). Thinking is excluded from counts and
    wording entirely when `showThinking` is off.

### 4. Fixed alongside

`ThinkingBlock.tsx`: drop the `italic` class from the toggle button (see
Non-goals).

## Testing

- Update the web e2e suite (`e2e/`) for: thinking hidden by default,
  `SettingsMenu` toggle revealing it, grouped work-row collapse/expand,
  live vs. finished summary text. Existing assertions keyed on
  `data-testid="tool-call-card"` / thinking visibility need review since
  defaults change.
- `bun run typecheck` / `build` clean, `make check` green (existing repo
  gates).
