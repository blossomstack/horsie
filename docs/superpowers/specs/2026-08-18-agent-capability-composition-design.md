# Agent Capability Composition Design

**Date:** 2026-08-18
**Status:** Revised 2026-08-18 — a capability is a closed enum, not a trait
**Extends:** `2026-08-15-session-runners-design.md`

## Context

The session-runners redesign moved every capability onto the agent actor, where the state each one needs actually lives. That was right and it stands. What followed it — making `Capability` a trait, with each implementation owning a private state slice, its own events, its own commands and its own toolbox layer — was a step too far, and this revision reverses it.

The reason is one fact. **Every `impl Capability` is in `crates/server/src/agent_loop/capabilities/`.** All thirteen are first-party and compiled into the same crate; nothing outside provides one. Plugins contribute hooks, MCP servers, agents and skills, never a capability. So the trait bought open dispatch over a set that was closed the whole time, and charged for it: `capabilities/mod.rs` reached 1743 lines, 1047 of them infrastructure — `Capability`, `Capabilities`, `Msg`, `Routing`, `Decision`, `Act`, `CapEvent`, `CapSlice`, `CapCommand`, `CapView`, `Mailbox`, `Answering` — whose entire job was to let a closed set pretend to be open.

Three further costs, each concrete:

- **The journal got three levels deeper.** `Capability(CapEvent::Timers(Event::Armed { .. }))` where a flat arm would do. Two production outages on this project were persisted-shape breaks, and both were diagnosed by reading raw events.
- **It contradicts what this codebase says it believes.** `CLAUDE.md`: *"Prefer exhaustive `match` over runtime guards — the compiler should enforce completeness, not tests."* A flat event enum with `wildcard_enum_match_arm` denied is more compile-time-checked than dynamic dispatch.
- **A tool call walks a stack.** An ordinary `bash` call traverses up to thirteen nested `ClaimedTools` layers, each scanning its own claims, before reaching the sandbox.

What was genuinely wrong with the original monolith was not the monolith. It was a 7577-line file. That has already been fixed by splitting `agent_actor.rs` into `state.rs`, `retries.rs`, `repair.rs` and `toolbox.rs`, and this design finishes the job the same way: **one actor, organised into one file per feature.**

## The design in five sentences

If a piece of the implementation cannot be placed here, it is wrong.

> The **agent actor** owns one state, one command enum and one event enum. There is no second dispatch mechanism inside it.
>
> A **capability** is an entry in a closed enum: one thing an agent is allowed to do, chosen per agent by its runner.
>
> Each capability's code — its tools, its command arms, its fold arms, its state type — lives in **one file**, and its state type keeps its fields private so nothing else can reach in.
>
> One function composes the whole toolbox from the enabled list, wrapping the sandbox **once**.
>
> A command's reply goes out only after its events are durable.

## What varies per agent, and what does not

This is the requirement the trait was protecting, and it survives without it. `sessions/runners/mod.rs`'s `assemble()` decides what each agent may do, and the answer is not the same for everyone:

| kind | also gets |
|---|---|
| conversation | `ask_user`, `set_title`, `fork` |
| subagent worker | none of those three |
| workflow step | none of those three; `submit_result` and its `ask_user` are declared per step |
| runtime | nothing at all |

Plus settings-driven entries — control plane, memory spaces, MCP servers — and per-agent variants: `ask_user` has an unattended form, `set_title` has a fork form that renames the fork rather than the session, and `sub_agent` carries the depth its gate is answered from.

**This is not cosmetic.** Give a worker `ask_user` and it can ask a question nobody is watching for; the turn parks and never resumes. Give it `set_title` and it renames the session out from under the person using it.

None of that requires per-capability state ownership. What varies is **which tools are wired and which behaviours are live** — never who owns the state. An agent that cannot ask simply never accumulates a pending question.

So:

```rust
pub enum Capability {
    AskUser(Attention),
    Title(TitleScope),
    Fork,
    SubAgent { depth: u32 },
    Workflow,
    StepResult(StepContract),
    TaskList,
    Timers,
    TokenBudget(BudgetPolicy),
    Hooks,
    ControlPlane,
    Memory(Vec<MemorySpace>),
    Mcp(Vec<McpServer>),
    Runtime(AgentType),
}
```

`assemble()` returns `Vec<Capability>` with exactly the logic it has today. The list is persisted with the agent, so a reload is equipped identically.

## What stays on the actor

Unchanged from the first draft, and the reasoning still holds.

**Core.** `log` and `next_seq` — the transcript and the identity of positions in it. `inbox`, `parked`, `nudges`, `turn_in_flight` — turn control.

`next_seq` is why the log could never have been a capability even under the trait: every journaled event may append an entry, so whoever owns `seq` sits upstream of everything rather than beside it. The read cursor compounds it — `ReadOutcome` fuses durable entries with in-flight streaming deltas because they are "two halves of one position", and deltas live in the running turn, not the journal.

**Plain projections.** `usage_total`, `last_turn_usage`, `context_tokens` — folds of run events, and fields on `AgentStateView`.

**Feature state.** One field per feature, each a type whose fields are **private to its own module**:

```rust
// agent_loop/timers.rs
pub struct TimerState { records: Vec<TimerRecord> }   // fields private
impl TimerState { pub(crate) fn armed(&self) -> &[TimerRecord] { … } }
```

Module privacy does what `CapSlice` was doing, for free. Nothing outside `timers.rs` can reach a `TimerRecord` except through an accessor that file chose to write. This is the encapsulation the trait was justified by, and it does not need the trait.

## Mechanisms

### One command enum, one event enum

`CapCommand` folds back into `AgentCommand`; `CapEvent` folds back into `AgentDomainEvent`. Both flat, both exhaustively matched, both readable in a raw journal without unwrapping two layers.

`Msg`, `Routing`, `Decision` and `Act` are deleted. A command arm returns a `CommandEffect` directly, which is what every other command in this actor already does. What `Act` encoded — answer, refuse, park, resume, conclude, hold, enqueue, record, ask, wake — becomes ordinary code in the arm that decided it. The verbs were only ever needed because a capability could not act, and now the code that decides is the code that acts.

**`Act::Hold` disappears entirely**, and that is a good sign. It existed because a broadcast *merged* decisions, so a capability claiming a turn boundary with an empty `Decision` was invisible by construction. With no broadcast there is nothing to be invisible to.

### One toolbox, composed once

```rust
fn compose(caps: &[Capability], facts: &AgentFacts, sandbox: Arc<dyn Toolbox>, …) -> Arc<dyn Toolbox>
```

matches over the enabled list, collects every claimed name paired with the command a call to it becomes, and wraps the sandbox once. Three improvements over per-capability layering:

- **A duplicate name is a construction-time error**, not a silent precedence win discovered by reading list order.
- **A call stops walking a stack** — one lookup instead of up to thirteen nested scans.
- **The ordering rule stops existing.** Not reduced from two orderings to one: gone. All that remains is agent-owned names taking precedence over the sandbox's open namespace, which is a single unambiguous fallthrough.

`ClaimedTool`'s idea survives — a `ToolSpec` paired with the command a call to it becomes, so there is no "name I claimed but cannot map" case. Only the per-capability layering goes.

### Replies go out after persistence, always

This is a blanket rule, and it is the one thing from the trait work that must not be lost. It was not a tidy-up: a test written against the pre-change code failed with `left: ["answered"], right: ["persisted", "answered"]`, proving every capability tool call was answered *before* its events were durable. A crash in that window told the model a task was added or a timer armed, and lost it.

A journal failure must reach the model as an execution failure, never as a success the log does not contain.

## What is deleted

`Capability` the trait, `Capabilities`, `Msg`, `TurnEvent` routing, `Routing`, `Decision`, `Act`, `CapEvent`, `CapSlice`, `CapCommand`, `CapView`, `Mailbox`, `Answering`, `layer()`, and the per-capability boilerplate each of the thirteen carried to satisfy them. Approximately 1000 lines of infrastructure plus its share of each capability file.

`FakeCapability` goes with it: tests exercise the real capabilities instead of an injected stub. Accepted deliberately — a fake that satisfies a trait proves the trait works, not that the feature does.

## Costs accepted

**One large file per concern, not one per implementation.** `AgentCommand` and `AgentDomainEvent` grow arms as features are added, and every arm is visible in one place. That is the point — it is also what makes them exhaustively checkable — but it does mean the enums are long. The mitigation is that each arm's *logic* lives in its feature's file, not beside the enum.

**No compatibility.** Journals break. No `#[serde(default)]` bridges, no shims.

## Migration order

Each step green on its own, one commit each. The branch currently sits at the end of the trait work, so this is a rework rather than a fresh build.

1. **`Capability` becomes a closed enum** holding the existing concrete structs; dispatch by `match` instead of vtable. Mechanical, and it proves the closed-set claim before anything else moves.
2. **`CapEvent` flattens into `AgentDomainEvent`; `CapSlice` state becomes `AgentState` fields** with module-private types.
3. **`CapCommand` flattens into `AgentCommand`**; `Msg`, `Routing`, `Decision` and `Act` are deleted and their logic moves into the command arms.
4. **`compose()` replaces `layer()`**; `Mailbox` is deleted.

Steps 1 and 2 are independent of 3. Step 4 depends on 3, because what a claimed name maps to changes there.
