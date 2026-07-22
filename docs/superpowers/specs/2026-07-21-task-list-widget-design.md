# Task list side widget for the web UI

## Context

`task_list` (see `2026-07-20-task-list-tool-design.md`) is durable agent state,
journaled as `AgentDomainEvent::TaskListChanged { snapshot }`. It's already
functional as a tool the model can call, but nothing surfaces the current list
to the person watching the session: `server/src/sessions/events.rs`'s
`wire_event` mapped `TaskListChanged` to `None`, so the state was invisible
outside of the tool's own text output (buried in a collapsed tool-call row
like any other tool). Requested: show the live list in the web UI, as a
collapsible side widget, with the wire type defined via fluorite per this
repo's protocol convention.

## Decision

**A new `SessionEvent::TaskListChanged(TaskListEvent)` wire variant, defined in
`models/fluorite/session.fl`.** `TaskListEvent { tasks: Vec<TaskItem> }` and
`TaskItem { id, content, status }` mirror the shape of `workflow`'s
hand-written `TaskRecord`/`TaskStatus`, but are a separate, fluorite-generated
type — per `CLAUDE.md`'s "protocol types are not persisted state" rule, the
durable `TaskListState` (journaled, replayed on actor recovery) and the wire
event (replayed to SSE clients) are intentionally not the same type, even
though today they hold the same fields. `TaskListState::tasks()` is a new
read-only accessor; `server/src/sessions/events.rs::wire_event` converts one
to the other on every replay.

**No new live-notification plumbing was needed.** `task_list` executes as a
normal tool call from the agent loop's perspective (`TaskListToolbox` wraps
the agent's toolbox, same as timers) — so it already gets a generic
`AgentEvent::ToolCallStart`/`ToolComplete` pair like any other tool, and
`ToolComplete` already triggers `SessionFrame::Journaled`, the wakeup that
tells a connected SSE client to re-read the journal past its last-seen
sequence id. `TaskListChanged` is persisted *before* the tool call's
`ToolComplete` (the command handler persists it synchronously while replying
to the tool's `ask`), so by the time a client re-fetches on that wakeup, both
events are already in the journal in the right order. Making `wire_event`
return `Some(...)` for `TaskListChanged` was therefore sufficient for both
the initial replay (session reload / reconnect) and live updates — no
separate broadcast path, unlike `TimerArmed`/`TimerFired` which still map to
`None` and remain invisible (out of scope here).

**The widget renders nothing until the agent has used the tool at least
once**, per "if any" in the request — an idle session with no plan shows no
extra chrome. Once `tasks.length > 0` it stays visible (even if a later
`create` clears back to a list that's merely all-`pending`); there's no
"hide again" path other than collapsing it, since the tool has no `delete`
action to signal "no plan anymore" (see the tool design doc — `remove` was
explicitly out of scope there too).

**Collapse state is local `useState`, not persisted.** A session-scoped toggle
resetting to expanded on reload matches how `WorkGroup`'s per-item collapse
already behaves, and avoids adding a new `useUiSettings` key for a widget that
only exists conditionally. Collapsed state renders a slim badge (`done/total`)
rather than disappearing entirely, so progress stays visible without the full
list.

## Testing

- `server/src/sessions/events.rs`: `task_list_changed_maps_to_wire_event`
  exercises the `wire_event` conversion (status mapping, ordering) via
  `replay_session_events` against an in-memory journal.
- `clients/web/e2e/h-task-list.spec.ts` (group H): no widget before any
  `task_list` use (H1), a `create` populates the panel (H2), an
  `update_status` mutation updates counts/strikethrough live (H3), and the
  collapse/expand toggle (H4) — all driven through the real server + mock LLM,
  same harness as the other groups.
