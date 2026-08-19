# Agent Capability Composition Design

**Date:** 2026-08-18
**Status:** Approved — extends `2026-08-15-session-runners-design.md`

## Context

The session-runners redesign moved every capability onto the agent actor, where the state each one needs actually lives. It worked, and it left the agent actor smaller in judgement but no smaller in surface: 26 commands, 22 events, 13 state fields, and two features — the task list and timers — that were capabilities in everything but name, hand-rolling what the trait had just generalised.

Converting those two proved the trait can absorb a pre-existing feature: 5 commands, 4 events, 2 state fields and 2 toolbox wrappers deleted, replaced by nothing rather than by a different bespoke arm.

This design finishes the job. It answers one question — **what is the agent actor, once everything that can be composed in has been** — and settles the mechanisms that make the answer hold.

## The design in five sentences

If a piece of the implementation cannot be placed here, it is wrong.

> The **agent actor** owns the transcript and decides when a turn starts. Nothing else.
>
> A **capability** is one thing an agent can do. It owns its own state, its own commands, its own events, and the toolbox layer that executes its tools.
>
> No capability reads another's state, and neither does the actor. Anything outside that needs a fact from inside asks a **named question** and gets a computed answer.
>
> The loop defines a **fixed set of hooks**. Capabilities define their own commands. The two are different things and must not be conflated.
>
> A capability need not own state at all — some carry only policy.

## What stays on the actor

Three buckets, decided by two tests: *can you delete it and still have an agent?* and *would any runner want it different?*

**Core — cannot be deleted.**

| field | why |
|---|---|
| `log`, `next_seq` | The transcript, and the identity of positions in it. |
| `inbox`, `parked`, `nudges`, `turn_in_flight` | Turn control. |

`next_seq` is the reason the log cannot be a capability, and the reason is structural rather than sentimental. Every journaled event may append an entry — that is what `coarse_appends_an_entry` decides — so a capability owning `seq` would sit upstream of every other capability's events rather than beside them. That inverts the composition. The read cursor makes it worse: `ReadOutcome` fuses durable entries with in-flight streaming deltas because they are "two halves of one position", and deltas live in the running turn, not the journal. A log capability would need a hook into live run state that no other capability has and none should.

The relationship that does work is the inverse — the log is core, and capabilities *contribute* entries to it through `Act::Record`. That verb already exists, because without it `ask_user`'s question vanished from the UI with every test green.

**Plain projections — deletable, but no runner wants them different.**

`usage_total`, `last_turn_usage`, `context_tokens`. Each is a fold of run events and a field on `AgentStateView`. Making them capabilities would cost a trait impl, a `CapEvent` arm, a `CapSlice` arm and a projection hook, and buy nothing. They stay fields.

**Capabilities — everything else.**

## Two kinds of capability

A capability owns **a durable slice**, or **stateless policy**, or both. The trait already supports all three because `apply` and `save` have defaults.

- *Slice-owning*: task list, timers, ask_user, sub_agent, fork, workflow, step_result, memory, mcp, runtime, control_plane, title.
- *Policy-only*: the token budget, hook records.

This distinction matters because "capability = state slice" would push a state field onto the two policy capabilities that neither needs. The token budget does not own `context_tokens` — that is a projection, and it is core. What it owns is the answer to *"should this turn compact, and to what target?"* Today that answer is two server constants, `COMPACT_AT_PERCENT` and `COMPACT_RETAIN_PERCENT`, with a comment explaining they are properties of the model rather than the session. That correctly rules out a session setting. It does not rule out a **runner** choosing: a workflow step with a fixed brief and a structured result has nothing budget-wise in common with a long interactive session. There is precedent — `compaction_window(auto_compact, card_window)` already lets a session switch compaction off entirely.

## Mechanisms

### 1. A capability wires a toolbox layer; there is no `tools()` method

`fn tools(&self, facts: &AgentFacts) -> Vec<ToolSpec>` is replaced by

```rust
fn layer(&self, inner: Arc<dyn Toolbox>, facts: &AgentFacts) -> Arc<dyn Toolbox>;
```

applied in list order at run start, after `provide()` has produced the facts. `facts` remains a parameter because it is per-run data, not capability state — and because it is the reason `tools()` had to grow one: the agent catalogue `spawn_agent` advertises does not exist until the runtime's workspace scan, which is what the old compose-time toolbox layer was for. That regression is recorded as a known gap and this closes it.

**This deletes an ordering.** Today one list satisfies two orderings that read opposite ways — first in offer order, outermost in the toolbox — which is why `push_front` exists and why a capability appended after the open-namespace one is silently swallowed. With layering, wrapping order *is* precedence. One rule instead of two.

### 2. Capabilities own their commands: `CapCommand`

`Msg::Tool` and `Msg::Command` are removed. In their place, one closed enum with one opaque arm per capability:

```rust
enum AgentCommand { /* … */ Capability(CapCommand) }
```

This is the third application of a pattern already proven twice on this branch — `CapEvent` for the journal, `CapSlice` for persistence. **Dispatch open through `dyn Capability`; the enum closed so nothing can be forgotten and routing is by construction rather than by matching a tool name.** `CapCommand` carries `ReplyTo` channels and is never journaled, so it has no serde constraint.

**Replies go out after persistence, always.** This is a blanket rule, not a per-command judgement. A command that answers before its events are durable can report success for work a crash will lose, and a test for it cannot fail. `Decision` carries the reply; the actor owns when it is sent.

### 3. Hooks are a fixed set the loop defines

`Msg` is lifecycle and nothing else:

`Turn(TurnEvent)` · `TurnProposed` · `Loaded` · `Answer` · `Child` · `Reply` · `Woke` · `Concluded`

`TurnProposed` is new: fired before a turn is built, it is where the token budget capability says "compact first". `Concluded` was added during the timers conversion for a concrete reason worth preserving here — submitting a result cancels armed timers, and that is *not* a turn boundary; reusing `TurnEvent::Ended` made `sub_agent` and `step_result` mis-fire their nudge and hold bookkeeping.

A capability that wants a lifecycle point the loop does not define must add one here, deliberately. It may not smuggle one in as a command.

### 4. State is private, and the way in is a named question

No capability reads another's state and neither does the actor. Two needs cross the boundary, and each is a **named projection** — the method name is the question, the return value is a computed answer, and the caller never learns the shape behind it.

| need | question | answer |
|---|---|---|
| compaction | `carried_state()` | `Option<String>` — prose the model reads |
| the agent document | `view()` | `Option<CapView>` — a typed arm the client renders |

`CapView` is deliberately **not** `CapSlice`. The journal shape is a durability contract; the view shape is an API contract; tying them together makes an API change force a journal migration.

This replaces `Capabilities::slices()`, which today hands every capability's whole persisted state to any caller inside `agent_loop` — a general bypass of the invariant, currently used by `AgentState::tasks()` and one test helper. After this, `slices()` is serialization-only.

**There are only two such needs, and that is a consequence of keeping the log, the inbox and the parks core.** Everything that used to be a cross-slice read is now a read of core state. If a third appears, prefer widening `CapView` over adding a trait method — a trait that grows a method per concern is the failure mode this design is avoiding.

## Consequences

- The agent actor's state drops from 13 fields to roughly 7: the transcript, turn control, and three projections.
- Compaction policy becomes per-runner rather than a server constant.
- A capability list is the complete description of what an agent can do. Nothing is implicit.
- A fork starts with no capability state, by design — see `scrub_for_fork`.

## Costs accepted

**Journal legibility.** Nesting every event inside `Capability(CapEvent)` puts one more level between a reader and the fact they want. Two production outages on this project were persisted-shape breaks diagnosed by reading raw events. This is accepted knowingly, not overlooked.

**No compatibility.** Journals break. There are no `#[serde(default)]` bridges and no migration paths; the correct end state is reached directly.

## Migration order

Each step green on its own, one commit each.

1. `layer()` replaces `tools()`.
2. `CapView` replaces the `slices()` bypass.
3. `CapCommand`; `Msg::Tool` and `Msg::Command` removed.
4. `TurnProposed` and the token budget capability.
5. The hook records capability.
6. `context_tokens` joins usage as a plain projection.

Step 3 is the largest and touches every capability. Steps 1 and 2 are independent of it and are worth landing first so the trait surface is settled before the command surface moves.
