# Agent timers — design

**Date:** 2026-05-31
**Status:** Implemented

## Goal

Give an agent a durable, self-managed way to suspend itself and be woken later,
so it can watch external state (a PR's CI checks, an issue, anything pollable)
without holding an LLM/agent context while it waits. The mechanism is a **dumb
timer**: the agent arms timers, parks, and on each fire re-runs whatever checks
it wants and decides what to do next. PR-watching is the first application, not a
special case — the primitive is "wake me later," not "watch a PR."

## Decisions (locked)

- **Dumb timer, not a watcher.** The mechanism only does "wake me in X" /
  "wake me every X." All polling/decision logic lives in the agent, which
  re-checks on each wake. No predicates, no per-resource pollers, no server-side
  watching.
- **Timers are agent-control tools** (`set_timer`, `list_timers`, `cancel_timer`)
  plus a `park` conclude-outcome. They mutate AgentActor state and are
  **intercepted at the agent layer — never forwarded to the sandboxed runtime**
  (unlike `Bash`/`ReadFile`/…). This is a new seam in tool dispatch.
- **Timers live in AgentActor journaled state.** They are recoverable, queryable
  (`list_timers` is the reliable source of truth), and cancellable.
- **Park outcome.** `AgentResult` gains a third variant, `Parked`, alongside
  `Completed | Handoff`. Parking with zero active timers is a hard error.
- **Cheap suspension.** A parked agent runs no LLM/compute. The job sits in a
  dedicated idle status until a timer fires.
- **Delivery via actor mailbox, never mid-run injection.** A fire is just a
  command to the AgentActor. If a run is in flight the wake message is queued and
  consumed at the next `RunFinished` (coalescing multiple fires); if idle, it
  starts a new run.
- **Recovery re-arms from the journal**; past-due timers fire immediately
  (missed-fire-after-downtime).
- **Per-actor `tokio` sleeps** (Approach A). No global scheduler. A dedicated
  `SchedulerActor` is the future scale-out path and the home for cron — out of
  scope here.

## Assumptions (baked in)

- **Relative durations only** — one-shot + recurring interval. **No
  cron/absolute-time** scheduling in this iteration.
- **Wake message is a `user`-role input** message, e.g.
  *"Timer `<label>` fired (fire #N)."* The agent re-runs its own checks from
  there.

## Tool surface

Four agent-facing operations. Schemas/results are **protocol types (fluorite)**.

| Tool | Args | Returns |
|------|------|---------|
| `set_timer` | `{ kind: OneShot \| Recurring, after: Duration, label }` | `timer_id` |
| `list_timers` | — | `[{ id, label, kind, next_fire, fire_count }]` |
| `cancel_timer` | `{ id }` or `{ all: true }` | cancelled ids |
| `park` (conclude) | — | ends the turn as `Parked` |

`set_timer` arms a timer and returns its id immediately. `Recurring` auto-re-arms
on each fire. `park` is the explicit conclude-idle signal; it is only valid when
≥1 timer is active.

## Protocol vs storage layering

Per project rule (`CLAUDE.md`): protocol types via fluorite, persisted structures
owned by the storage layer.

- **Fluorite (protocol):** the four tool-call schemas + results, the wake input
  message, and (optionally) a streaming `AgentEvent` so observers see
  `TimerArmed` / `TimerFired` in the event stream.
- **Hand-written (storage, journaled):** the AgentActor **timer registry** and
  its domain events `TimerArmed` / `TimerCancelled` / `TimerFired`. These are
  persisted, evolve via migration, and must **not** be fluorite.

## State & registry

AgentActor journaled state gains a timer registry:

```
TimerRegistry: map<TimerId, TimerRecord>
TimerRecord { id, label, kind: OneShot | Recurring(interval), next_fire, fire_count }
```

Domain events (journaled, hand-written):

- `TimerArmed   { record }`     — on `set_timer`
- `TimerCancelled { id }`        — on `cancel_timer` (one event per cancelled id)
- `TimerFired  { id, fire_count, next_fire? }` — on fire; `next_fire` present for
  recurring (re-arm), absent for one-shot (removed)

## Firing & delivery

1. Each armed timer spawns a `tokio::time::sleep`; on elapse it sends
   `TimerFired(id)` to the AgentActor.
2. On `TimerFired`: journal the event; **one-shot** → remove from registry;
   **recurring** → bump `fire_count`, compute & journal `next_fire`, re-spawn the
   sleep.
3. Build the `user`-role wake message, then:
   - **agent idle/parked** → start a new run with it as input; job → `Running`.
   - **agent mid-run** → enqueue in a pending-input queue; consume at the next
     `RunFinished`. Multiple fires while running coalesce into the queue. No
     mid-run injection — the agentcore run loop is untouched.

## Park outcome & job status

- `AgentResult = Completed | Handoff | Parked`. `Parked` carries (or is validated
  against) the active-timer set; **zero active timers ⇒ hard error**.
- `WorkflowActor` on `Parked` moves the job to a **dedicated idle status**
  (e.g. `Idle` / `AwaitingTimer`), mirroring how `AwaitingUserInput` is its own
  variant rather than a flavor of `Suspended` — keeps "why is this parked"
  representable. Agent conversation/context is preserved in the journal.
- From a wake, the agent may: re-`park` (timers still active), `set_timer` more,
  `cancel_timer`, or conclude `Completed` to finish the job.

## Recovery

On daemon/job restart, AgentActor replays its journal → rebuilds the timer
registry → re-spawns sleeps with the **remaining** duration (`next_fire − now`).
Past-due timers fire immediately. Recurring timers resume their `fire_count` and
interval.

## End-to-end loop (PR-watch example)

```
run → open PR (Bash: gh pr create)
    → set_timer(Recurring, 5m, "pr-checks")
    → park                              # job → Idle, zero cost
… 5m …
    → TimerFired → wake "Timer pr-checks fired (fire #1)"
    → agent: gh pr checks  →  still pending
    → park                              # recurring already re-armed
… 5m …
    → TimerFired (fire #2) → gh pr checks → FAILED
    → agent: diagnose, push fix, leave timer armed, park
… checks pass / PR merged …
    → agent: cancel_timer(all) → Completed   # job finishes
```

## Out of scope (this iteration)

- Cron / absolute-time schedules (future `SchedulerActor`).
- A global scheduler / timing wheel; server-side predicates or typed
  per-resource watchers.
- Cross-agent or cross-job timers — a timer belongs to exactly one AgentActor.

## Open implementation questions (for the plan, not the spec)

- Exact interception point for agent-control tools in the toolbox/dispatch path
  (`agentcore` run loop vs `AgentActor` wrapper) and how a tool handler reaches
  back into journaled actor state.
- Whether `TimerArmed`/`TimerFired` should also surface as streaming
  `AgentEvent`s for observers, or stay purely internal.
- Duration encoding in the tool schema (seconds vs a structured duration).
